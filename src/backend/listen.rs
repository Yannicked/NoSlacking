//! The worker's side of listening to a huddle: one huddle at a time,
//! joined through Slack, played through [`crate::huddle_audio`], left
//! when asked, on sign-out, or when another starts. Its news goes to the
//! interface as [`crate::people::Event::Listening`]; failures as a
//! [`Failure`], their technical detail only in the log.
//!
//! Joined muted. The microphone opens only when the interface unmutes
//! and closes when it mutes or the huddle is left; what it is doing goes
//! back as [`crate::people::Event::Microphone`]. The session sends the
//! microphone's frames only once it is open (`effective` below), so a
//! microphone that would not open never unmutes the call.
//!
//! The speaker is checked every second: one that stopped playing (its
//! device thread gone, or no longer asking for sound) is opened again on
//! the same feed, up to three times; after that the huddle is left with
//! [`HuddleTrouble::SoundStopped`] rather than staying in it hearing
//! nothing.
//!
//! With `huddle-video`, the session also says who shares their screen
//! (`Listen::Shares`) and who has a camera on (`Listen::Cameras`), and
//! hands over the `Screen` the watched share's pictures arrive in
//! (`Listen::Screen`) and the `Gallery` the camera tiles' do
//! (`Listen::Gallery`); the interface says what its call window wants,
//! and only that is received, nothing while it is closed.
//!
//! With `huddle-camera`, joined with the camera off. The camera opens
//! only when the interface turns it on and closes when it turns it off,
//! when Chime takes no video from us, or when the huddle is left, as the
//! microphone does; what it is doing goes back as
//! `people::Event::Camera`. Its pictures are encoded on a
//! thread of their own for the session, and its self-preview arrives in
//! the `Preview` handed over as `Listen::Preview`.

use std::time::Duration;

use tokio::sync::{mpsc, oneshot, watch};

use super::{Event, Sink};
use crate::failure::{Failure, HuddleTrouble};
use crate::huddle_audio::cameras::Camera;
#[cfg(feature = "huddle-video")]
use crate::huddle_audio::cameras::Wish;
#[cfg(feature = "huddle-video")]
use crate::huddle_audio::gallery::Gallery;
use crate::huddle_audio::join::{self, JoinFailure};
use crate::huddle_audio::media::{self, Stage, Uplink};
use crate::huddle_audio::microphone::{Cpal, MicControl, Wiring};
use crate::huddle_audio::processing::RenderTap;
use crate::huddle_audio::roster::Roster;
#[cfg(feature = "huddle-video")]
use crate::huddle_audio::screen::Screen;
use crate::huddle_audio::speaker::{Feed, Speaker};
use crate::huddle_audio::video::Share;
use crate::huddle_mic::MicNews;
use crate::huddles::{Left, Listen};
use crate::people;
use crate::slack::Client;

/// The shortest time between two rosters sent to the interface. Chime
/// sends volumes several times a second; the window need not wake for
/// each, and a speaking mark held a moment longer reads as well.
const ROSTER_EVERY: Duration = Duration::from_millis(250);
/// How often the speaker is asked whether it still plays.
const SPEAKER_CHECK: Duration = Duration::from_secs(1);
/// How many times a session opens a stopped speaker again before it
/// gives up.
const SPEAKER_REOPENS: u32 = 3;

/// The huddle being listened to.
#[derive(Debug)]
struct Running {
    team: String,
    stop: watch::Sender<bool>,
    /// Whether the interface wants the microphone muted.
    muted: watch::Sender<bool>,
    /// What the call window wants.
    #[cfg(feature = "huddle-video")]
    wish: watch::Sender<Wish>,
    /// Whether the interface wants the camera on.
    #[cfg(feature = "huddle-camera")]
    camera: watch::Sender<bool>,
}

/// The one listening session there may be.
#[derive(Debug, Default)]
pub struct Listener {
    running: Option<Running>,
}

impl Listener {
    /// Listens to the huddle in `channel` of `team`, leaving the last one
    /// first.
    pub fn start(&mut self, client: Client, team: String, channel: String, sink: Sink) {
        self.stop();
        let (stop, stopped) = watch::channel(false);
        let (muted, wanted) = watch::channel(true);
        #[cfg(feature = "huddle-video")]
        let (wish, wishes) = watch::channel(Wish::closed());
        // Joined with the camera off.
        #[cfg(feature = "huddle-camera")]
        let (camera, camera_wanted) = watch::channel(false);
        let controls = Controls {
            stopped,
            wanted,
            #[cfg(feature = "huddle-video")]
            wishes,
            #[cfg(feature = "huddle-camera")]
            camera_wanted,
        };
        tokio::spawn(run(client, team.clone(), channel, controls, sink));
        self.running = Some(Running {
            team,
            stop,
            muted,
            #[cfg(feature = "huddle-video")]
            wish,
            #[cfg(feature = "huddle-camera")]
            camera,
        });
    }

    /// Turns the camera on or off in the huddle, if there is one.
    #[cfg(feature = "huddle-camera")]
    pub fn set_camera(&mut self, on: bool) {
        if let Some(running) = &self.running {
            let _ = running.camera.send(on);
        }
    }

    /// The call window wants `wish`: only that is received.
    #[cfg(feature = "huddle-video")]
    pub fn watch_call(&mut self, wish: Wish) {
        if let Some(running) = &self.running {
            let _ = running.wish.send(wish);
        }
    }

    /// Mutes or unmutes the microphone in the huddle, if there is one.
    pub fn set_muted(&mut self, muted: bool) {
        if let Some(running) = &self.running {
            let _ = running.muted.send(muted);
        }
    }

    /// Leaves the huddle; the session says when it has.
    pub fn stop(&mut self) {
        if let Some(running) = self.running.take() {
            let _ = running.stop.send(true);
        }
    }

    /// Leaves the huddle if it is in `team`, which signed out.
    pub fn signed_out(&mut self, team: &str) {
        if self.running.as_ref().is_some_and(|r| r.team == team) {
            self.stop();
        }
    }
}

/// The step a session's failure is worded by.
fn trouble(stage: Stage) -> HuddleTrouble {
    match stage {
        Stage::Signaling => HuddleTrouble::Signaling,
        Stage::Join => HuddleTrouble::Join,
        Stage::Relay => HuddleTrouble::Relay,
        Stage::Subscribe => HuddleTrouble::Offer,
        Stage::Connect => HuddleTrouble::Connect,
        Stage::Media => HuddleTrouble::Lost,
    }
}

/// What a failed `rooms.join` tells the interface.
fn join_failure(error: &JoinFailure) -> Failure {
    match error {
        JoinFailure::NotSession => Failure::NeedsSession,
        JoinFailure::Slack(error) => super::api::failure(error),
        JoinFailure::Answer(error) => Failure::Unexpected(error.to_string()),
    }
}

/// Opens and closes the microphone as `wanted` says, until `done`,
/// telling the session through `effective` and the interface through
/// `tell`; closes it at the end whatever happened.
pub(super) async fn microphone(
    mut wanted: watch::Receiver<bool>,
    effective: watch::Sender<bool>,
    wiring: Wiring,
    mut done: oneshot::Receiver<()>,
    tell: impl Fn(MicNews),
) {
    let mut control = Some(MicControl::new(Cpal::new(wiring)));
    loop {
        tokio::select! {
            _ = &mut done => break,
            changed = wanted.changed() => if changed.is_err() {
                break;
            },
        }
        let muted = *wanted.borrow_and_update();
        let Some(mut held) = control.take() else {
            break;
        };
        // Opening and closing wait on the device's thread: not on this one.
        let (held, result) = match tokio::task::spawn_blocking(move || {
            let result = held.set_muted(muted);
            (held, result)
        })
        .await
        {
            Ok(done) => done,
            Err(error) => {
                log::warn!("huddle microphone: the device thread failed: {error}");
                let _ = effective.send(true);
                tell(MicNews::Failed(Failure::Huddle(HuddleTrouble::Microphone)));
                return;
            }
        };
        control = Some(held);
        match result {
            Ok(()) => {
                let _ = effective.send(muted);
                tell(if muted { MicNews::Muted } else { MicNews::Live });
            }
            Err(error) => {
                log::warn!("huddle microphone: {error}");
                let _ = effective.send(true);
                tell(MicNews::Failed(Failure::Huddle(HuddleTrouble::Microphone)));
            }
        }
    }
    let _ = effective.send(true);
    if let Some(control) = control {
        let _ = tokio::task::spawn_blocking(move || drop(control)).await;
    }
}

/// What the camera's task needs: where the camera's pictures go, and
/// what the encoder thread sends from.
#[cfg(feature = "huddle-camera")]
struct CameraWiring {
    latest: crate::huddle_audio::camera::Latest,
    frames: mpsc::Sender<crate::huddle_audio::camera_send::VideoFrame>,
    control: crate::huddle_audio::camera_send::SendControl,
    preview: crate::huddle_camera::Preview,
}

/// What a camera that would not open tells the interface.
#[cfg(feature = "huddle-camera")]
fn camera_failure(error: &crate::huddle_audio::camera::CameraError) -> Failure {
    use crate::huddle_audio::camera::CameraError;
    Failure::Huddle(match error {
        CameraError::NoDevice => HuddleTrouble::NoCamera,
        CameraError::Denied => HuddleTrouble::CameraDenied,
        CameraError::Open(_) => HuddleTrouble::Camera,
    })
}

/// Opens and closes the camera as `wanted` says, until `done`, telling
/// the session through `on` and the interface through `tell`; closes it
/// when Chime takes no video from us (`refusals`), and at the end
/// whatever happened. The encoder's thread runs from the first time the
/// camera opens until the end.
#[cfg(feature = "huddle-camera")]
async fn camera(
    mut wanted: watch::Receiver<bool>,
    on: watch::Sender<bool>,
    wiring: CameraWiring,
    mut refusals: mpsc::Receiver<()>,
    mut done: oneshot::Receiver<()>,
    tell: impl Fn(crate::huddle_camera::CamNews),
) {
    use crate::huddle_audio::camera::{CameraControl, Nokhwa};
    use crate::huddle_audio::camera_send::Encoding;
    use crate::huddle_camera::CamNews;

    let CameraWiring {
        latest,
        frames,
        control: send_control,
        preview,
    } = wiring;
    let mut control = Some(CameraControl::new(Nokhwa::new(latest.clone())));
    let mut encoding: Option<Encoding> = None;
    let mut frames = Some(frames);
    loop {
        let (want, refused) = tokio::select! {
            _ = &mut done => break,
            changed = wanted.changed() => {
                if changed.is_err() {
                    break;
                }
                (*wanted.borrow_and_update(), false)
            }
            refused = refusals.recv() => match refused {
                Some(()) => (false, true),
                None => break,
            },
        };
        let Some(mut held) = control.take() else {
            break;
        };
        // Opening and closing wait on the camera's thread: not on this one.
        let (held, result) = match tokio::task::spawn_blocking(move || {
            let result = held.set_on(want);
            (held, result)
        })
        .await
        {
            Ok(done) => done,
            Err(error) => {
                log::warn!("huddle camera: the camera thread failed: {error}");
                let _ = on.send(false);
                tell(CamNews::Failed(Failure::Huddle(HuddleTrouble::Camera)));
                return;
            }
        };
        control = Some(held);
        match result {
            Ok(()) if refused => {
                let _ = on.send(false);
                preview.clear();
                tell(CamNews::Failed(Failure::Huddle(HuddleTrouble::ViewOnly)));
            }
            Ok(()) => {
                if want && encoding.is_none() {
                    match frames.take().map(|frames| {
                        Encoding::spawn(
                            latest.clone(),
                            frames,
                            send_control.clone(),
                            Some(preview.clone()),
                        )
                    }) {
                        Some(Ok(started)) => encoding = Some(started),
                        Some(Err(why)) => log::warn!("huddle camera: {why}"),
                        None => {}
                    }
                }
                if !want {
                    preview.clear();
                }
                let _ = on.send(want);
                tell(if want { CamNews::On } else { CamNews::Off });
            }
            Err(error) => {
                log::warn!("huddle camera: {error}");
                let _ = on.send(false);
                tell(CamNews::Failed(camera_failure(&error)));
            }
        }
    }
    let _ = on.send(false);
    // Closing joins the camera's and the encoder's threads: not on this
    // one.
    let _ = tokio::task::spawn_blocking(move || {
        drop(control);
        drop(encoding);
    })
    .await;
    preview.clear();
}

/// Lets go of a speaker that stopped and opens the device again for the
/// same feed, off this thread: both wait on the device's.
async fn reopen(old: Speaker, feed: Feed, tap: RenderTap) -> Result<Speaker, String> {
    let reopened = tokio::task::spawn_blocking(move || {
        drop(old);
        Speaker::reopen(&feed, Some(tap))
    })
    .await;
    match reopened {
        Ok(result) => result,
        Err(error) => Err(format!("the device thread failed: {error}")),
    }
}

/// How a session that ran ended, for the interface: left when asked,
/// the huddle over, or a failure in words; `sound_stopped` when the
/// speaker gave up, which is why it was left.
fn ending(
    meeting_ended: bool,
    sound_stopped: bool,
    result: Result<(), media::Failure>,
) -> Result<Left, Failure> {
    if sound_stopped {
        return Err(Failure::Huddle(HuddleTrouble::SoundStopped));
    }
    match result {
        Ok(()) if meeting_ended => Ok(Left::Ended),
        Ok(()) => Ok(Left::Asked),
        Err(failure) => {
            log::warn!("huddle audio: {failure}");
            Err(Failure::Huddle(trouble(failure.stage)))
        }
    }
}

/// What the interface tells a session as it goes.
struct Controls {
    /// Whether to leave.
    stopped: watch::Receiver<bool>,
    /// Whether the microphone should be muted.
    wanted: watch::Receiver<bool>,
    /// What the call window wants.
    #[cfg(feature = "huddle-video")]
    wishes: watch::Receiver<Wish>,
    /// Whether the camera should be on.
    #[cfg(feature = "huddle-camera")]
    camera_wanted: watch::Receiver<bool>,
}

/// The next list of who shares, or never while there is none.
async fn next_shares(
    shares: &mut Option<watch::Receiver<Vec<Share>>>,
) -> Result<Vec<Share>, watch::error::RecvError> {
    match shares {
        Some(shares) => {
            shares.changed().await?;
            Ok(shares.borrow_and_update().clone())
        }
        None => std::future::pending().await,
    }
}

/// The next list of cameras, or never while there is none.
async fn next_cameras(
    cameras: &mut Option<watch::Receiver<Vec<Camera>>>,
) -> Result<Vec<Camera>, watch::error::RecvError> {
    match cameras {
        Some(cameras) => {
            cameras.changed().await?;
            Ok(cameras.borrow_and_update().clone())
        }
        None => std::future::pending().await,
    }
}

/// One listening session, start to end.
async fn run(client: Client, team: String, channel: String, controls: Controls, sink: Sink) {
    let Controls {
        mut stopped,
        wanted,
        #[cfg(feature = "huddle-video")]
        wishes,
        #[cfg(feature = "huddle-camera")]
        camera_wanted,
    } = controls;
    let tell = |state: Listen| {
        sink.send(Event::People {
            team: team.clone(),
            event: people::Event::Listening {
                channel: channel.clone(),
                state,
            },
        });
    };
    tell(Listen::Joining);
    let tap = RenderTap::default();
    let speaker_tap = tap.clone();
    let reopen_tap = tap.clone();
    let (speaker, feed) =
        match tokio::task::spawn_blocking(move || Speaker::open(Some(speaker_tap))).await {
            Ok(Ok(opened)) => opened,
            Ok(Err(why)) => {
                log::warn!("huddle audio: {why}");
                tell(Listen::Ended(Err(Failure::Huddle(HuddleTrouble::NoSound))));
                return;
            }
            Err(error) => {
                log::warn!("huddle audio: the device thread failed: {error}");
                tell(Listen::Ended(Err(Failure::Huddle(HuddleTrouble::NoSound))));
                return;
            }
        };
    let region = crate::huddle_audio::region::for_join(None).await;
    let joined = match join::join(&client, &channel, &region).await {
        Ok(joined) => joined,
        Err(error) => {
            log::warn!("huddle audio: {error}");
            tell(Listen::Ended(Err(join_failure(&error))));
            return;
        }
    };
    let (frames, frames_in) = mpsc::channel(25);
    let (effective, muted) = watch::channel(true);
    let (close_mic, mic_done) = oneshot::channel();
    let mic_sink = sink.clone();
    let (mic_team, mic_channel) = (team.clone(), channel.clone());
    let mic = tokio::spawn(microphone(
        wanted,
        effective,
        Wiring {
            frames,
            render: tap,
        },
        mic_done,
        move |news| {
            mic_sink.send(Event::People {
                team: mic_team.clone(),
                event: people::Event::Microphone {
                    channel: mic_channel.clone(),
                    news,
                },
            });
        },
    ));
    let uplink = Uplink {
        frames: frames_in,
        muted,
    };
    let (live, connected) = oneshot::channel();
    let (roster, mut rosters) = watch::channel(Roster::default());
    // The session stops when the interface asks, or when the speaker
    // gives up.
    let (halt, halted) = watch::channel(false);
    let mut heard_stop = false;
    let mut speaker = Some(speaker);
    let mut reopened = 0;
    let mut sound_stopped = false;
    let mut checks = tokio::time::interval(SPEAKER_CHECK);
    checks.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    // Who shares and who has a camera on, for the call bar, and where the
    // watched share's and the tiles' pictures go, woken as the events are.
    #[cfg(feature = "huddle-video")]
    let (viewer, mut shares, mut cameras) = {
        let waker = sink.waker();
        let screen = Screen::new(move || waker.wake());
        tell(Listen::Screen(screen.clone()));
        let waker = sink.waker();
        let gallery = Gallery::new(move || waker.wake());
        tell(Listen::Gallery(gallery.clone()));
        let (shares, told) = watch::channel(Vec::new());
        let (cameras, cameras_told) = watch::channel(Vec::new());
        let viewer = crate::huddle_audio::watch::Viewer {
            shares,
            cameras,
            wish: wishes,
            screen,
            gallery,
        };
        (Some(viewer), Some(told), Some(cameras_told))
    };
    #[cfg(not(feature = "huddle-video"))]
    let (viewer, mut shares, mut cameras) = (None, None, None);
    // The camera: closed until turned on, its pictures encoded on a
    // thread of their own, its preview for the call bar.
    #[cfg(feature = "huddle-camera")]
    let (camera_uplink, camera_task, close_camera) = {
        let waker = sink.waker();
        let preview = crate::huddle_camera::Preview::new(move || waker.wake());
        tell(Listen::Preview(preview.clone()));
        let latest = crate::huddle_audio::camera::Latest::default();
        let (frames, frames_in) = mpsc::channel(crate::huddle_audio::camera_send::QUEUE);
        let control = crate::huddle_audio::camera_send::SendControl::default();
        let (refused, refusals) = mpsc::channel(1);
        let (on, on_rx) = watch::channel(false);
        let (close_camera, camera_done) = oneshot::channel();
        let camera_sink = sink.clone();
        let (camera_team, camera_channel) = (team.clone(), channel.clone());
        let task = tokio::spawn(camera(
            camera_wanted,
            on,
            CameraWiring {
                latest,
                frames,
                control: control.clone(),
                preview,
            },
            refusals,
            camera_done,
            move |news| {
                camera_sink.send(Event::People {
                    team: camera_team.clone(),
                    event: people::Event::Camera {
                        channel: camera_channel.clone(),
                        news,
                    },
                });
            },
        ));
        let uplink = crate::huddle_audio::camera_send::CameraUplink {
            frames: frames_in,
            on: on_rx,
            control,
            refused,
        };
        (Some(uplink), task, close_camera)
    };
    let listening = media::listen(
        &joined,
        Some(feed.clone()),
        Some(uplink),
        halted,
        Some(live),
        Some(roster),
        media::Video {
            options: crate::huddle_audio::video::for_app(),
            viewer,
            #[cfg(feature = "huddle-camera")]
            camera: camera_uplink,
        },
    );
    tokio::pin!(listening);
    let mut connected = Some(connected);
    // The next roster goes no sooner than this, and none once the
    // session has let go of its end.
    let mut next_roster = Some(tokio::time::Instant::now());
    let (report, result) = loop {
        tokio::select! {
            ended = &mut listening => break ended,
            changed = async {
                // A closed channel answers at once, over and over: once
                // heard, never again.
                if heard_stop {
                    std::future::pending().await
                } else {
                    stopped.changed().await
                }
            } => {
                if changed.is_err() || *stopped.borrow() {
                    heard_stop = true;
                    let _ = halt.send(true);
                }
            }
            _ = checks.tick(), if speaker.is_some() => {
                let now = std::time::Instant::now();
                if !speaker.as_mut().is_some_and(|s| s.stopped(now)) {
                    continue;
                }
                let Some(old) = speaker.take() else {
                    continue;
                };
                if reopened < SPEAKER_REOPENS {
                    reopened += 1;
                    log::warn!("huddle audio: the speaker stopped playing; opening it again ({reopened})");
                    match reopen(old, feed.clone(), reopen_tap.clone()).await {
                        Ok(fresh) => speaker = Some(fresh),
                        Err(why) => {
                            log::warn!("huddle audio: {why}");
                            sound_stopped = true;
                            let _ = halt.send(true);
                        }
                    }
                } else {
                    log::warn!("huddle audio: the speaker stopped playing again; leaving");
                    let _ = tokio::task::spawn_blocking(move || drop(old)).await;
                    sound_stopped = true;
                    let _ = halt.send(true);
                }
            }
            up = async {
                match connected.as_mut() {
                    Some(connected) => connected.await,
                    None => std::future::pending().await,
                }
            } => {
                connected = None;
                if up.is_ok() {
                    tell(Listen::Live);
                }
            }
            changed = async {
                match next_roster {
                    Some(at) => {
                        tokio::time::sleep_until(at).await;
                        rosters.changed().await
                    }
                    None => std::future::pending().await,
                }
            } => {
                if changed.is_ok() {
                    let roster = rosters.borrow_and_update().clone();
                    tell(Listen::Roster(roster));
                    next_roster = Some(tokio::time::Instant::now() + ROSTER_EVERY);
                } else {
                    next_roster = None;
                }
            }
            told = next_shares(&mut shares) => match told {
                #[cfg(feature = "huddle-video")]
                Ok(now) => tell(Listen::Shares(now)),
                #[cfg(not(feature = "huddle-video"))]
                Ok(_) => {}
                Err(_) => shares = None,
            },
            told = next_cameras(&mut cameras) => match told {
                #[cfg(feature = "huddle-video")]
                Ok(now) => tell(Listen::Cameras(now)),
                #[cfg(not(feature = "huddle-video"))]
                Ok(_) => {}
                Err(_) => cameras = None,
            },
        }
    };
    // Left: the microphone and the camera close before anything else.
    let _ = close_mic.send(());
    #[cfg(feature = "huddle-camera")]
    let _ = close_camera.send(());
    let _ = mic.await;
    #[cfg(feature = "huddle-camera")]
    let _ = camera_task.await;
    // Stopping the device waits for its thread; not on this one.
    let _ = tokio::task::spawn_blocking(move || drop(speaker)).await;
    log::info!(
        "huddle audio: {} frames in {} bytes; ended: {}",
        report.audio_frames,
        report.audio_bytes,
        report.ending.as_deref().unwrap_or("-")
    );
    if let Some(video) = &report.video {
        for line in crate::huddle_audio::probe::video_summary(video) {
            log::info!("huddle {line}");
        }
    }
    tell(Listen::Ended(ending(
        report.meeting_ended,
        sound_stopped,
        result,
    )));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_stage_has_its_words() {
        for stage in [
            Stage::Signaling,
            Stage::Join,
            Stage::Relay,
            Stage::Subscribe,
            Stage::Connect,
            Stage::Media,
        ] {
            assert!(!Failure::Huddle(trouble(stage)).message().is_empty());
        }
        assert_eq!(
            join_failure(&JoinFailure::NotSession),
            Failure::NeedsSession
        );
        assert_eq!(ending(true, false, Ok(())), Ok(Left::Ended));
        assert_eq!(ending(false, false, Ok(())), Ok(Left::Asked));
        assert_eq!(
            ending(false, true, Ok(())),
            Err(Failure::Huddle(HuddleTrouble::SoundStopped)),
            "left because the speaker gave up"
        );
        assert_eq!(
            ending(
                false,
                false,
                Err(media::Failure {
                    stage: Stage::Media,
                    why: "relay lost".into()
                })
            ),
            Err(Failure::Huddle(HuddleTrouble::Lost))
        );
    }

    #[cfg(feature = "huddle-camera")]
    #[test]
    fn each_camera_failure_has_its_words() {
        use crate::huddle_audio::camera::CameraError;
        assert_eq!(
            camera_failure(&CameraError::NoDevice),
            Failure::Huddle(HuddleTrouble::NoCamera)
        );
        assert_eq!(
            camera_failure(&CameraError::Denied),
            Failure::Huddle(HuddleTrouble::CameraDenied)
        );
        assert_eq!(
            camera_failure(&CameraError::Open("busy".into())),
            Failure::Huddle(HuddleTrouble::Camera)
        );
    }
}
