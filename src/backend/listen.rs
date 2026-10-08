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
//! The devices are the ones chosen ([`crate::devices`], told by
//! [`Listener::use_devices`] at start and on each change), the system's
//! default for any not chosen or not connected. A change while in the
//! huddle takes effect at once: an open microphone is closed and opened
//! on the new one (one that will not open leaves you muted, saying so);
//! the speaker is opened anew on the same feed and the same echo
//! canceller's tap (one that will not open gives way to the default,
//! saying so); a camera that is on is started again on the new one,
//! beginning with a keyframe.
//!
//! With `huddle-camera`, joined with the camera off. The camera opens
//! only when the interface turns it on and closes when it turns it off,
//! when Chime takes no video from us, or when the huddle is left, as the
//! microphone does; what it is doing goes back as
//! `people::Event::Camera`. The video helper opens, captures and encodes
//! it; a thread of its own here asks for each frame and hands it to the
//! session, and its self-preview arrives in the `Preview` handed over as
//! `Listen::Preview`. No helper, no camera: turning it on says so. A
//! helper that fails while the camera is on is started again with the
//! camera in it (a camera has no dialog to show again), a few times at
//! most; after that the camera goes off, saying why.

use std::time::Duration;

use tokio::sync::{mpsc, oneshot, watch};

use super::{Event, Sink};
use crate::devices::{Chosen, Kind};
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

#[cfg(feature = "huddle-share")]
mod share;

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
    /// The devices chosen.
    devices: watch::Sender<Chosen>,
    /// What the call window wants.
    #[cfg(feature = "huddle-video")]
    wish: watch::Sender<Wish>,
    /// Whether the interface wants the camera on.
    #[cfg(feature = "huddle-camera")]
    camera: watch::Sender<bool>,
    /// What the interface asks of the screen share.
    #[cfg(feature = "huddle-share")]
    share: mpsc::Sender<crate::huddle_share::ShareRequest>,
}

/// The one listening session there may be.
#[derive(Debug, Default)]
pub struct Listener {
    running: Option<Running>,
    /// The devices chosen, for this huddle and the next.
    chosen: Chosen,
}

impl Listener {
    /// Listens to the huddle in `channel` of `team`, leaving the last one
    /// first.
    pub fn start(&mut self, client: Client, team: String, channel: String, sink: Sink) {
        self.stop();
        let (stop, stopped) = watch::channel(false);
        let (muted, wanted) = watch::channel(true);
        let (devices, chosen) = watch::channel(self.chosen.clone());
        #[cfg(feature = "huddle-video")]
        let (wish, wishes) = watch::channel(Wish::closed());
        // Joined with the camera off.
        #[cfg(feature = "huddle-camera")]
        let (camera, camera_wanted) = watch::channel(false);
        // Joined not sharing.
        #[cfg(feature = "huddle-share")]
        let (share, share_requests) = mpsc::channel(8);
        let controls = Controls {
            stopped,
            wanted,
            chosen,
            #[cfg(feature = "huddle-video")]
            wishes,
            #[cfg(feature = "huddle-camera")]
            camera_wanted,
            #[cfg(feature = "huddle-share")]
            share_requests,
        };
        tokio::spawn(run(client, team.clone(), channel, controls, sink));
        self.running = Some(Running {
            team,
            stop,
            muted,
            devices,
            #[cfg(feature = "huddle-video")]
            wish,
            #[cfg(feature = "huddle-camera")]
            camera,
            #[cfg(feature = "huddle-share")]
            share,
        });
    }

    /// Starts, picks or stops sharing your screen in the huddle, if there
    /// is one.
    #[cfg(feature = "huddle-share")]
    pub fn share(&mut self, request: crate::huddle_share::ShareRequest) {
        if let Some(running) = &self.running
            && let Err(error) = running.share.try_send(request)
        {
            log::warn!("huddle share: a request was dropped: {error}");
        }
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

    /// Uses `chosen` from now on: in the huddle going on, at once.
    pub fn use_devices(&mut self, chosen: Chosen) {
        if let Some(running) = &self.running {
            let _ = running.devices.send(chosen.clone());
        }
        self.chosen = chosen;
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

/// The next change of the device chosen for `kind` from `current`, or
/// never once the interface has let go (`chosen` then none).
async fn next_choice(
    chosen: &mut Option<watch::Receiver<Chosen>>,
    kind: Kind,
    current: &Option<crate::devices::Choice>,
) -> Option<crate::devices::Choice> {
    loop {
        let Some(receiver) = chosen.as_mut() else {
            return std::future::pending().await;
        };
        if receiver.changed().await.is_err() {
            *chosen = None;
            continue;
        }
        let now = receiver.borrow_and_update().get(kind).cloned();
        if now != *current {
            return now;
        }
    }
}

/// Opens and closes the microphone as `wanted` says, on the device
/// `chosen` says, until `done`, telling the session through `effective`
/// and the interface through `tell`; closes it at the end whatever
/// happened.
async fn microphone(
    mut wanted: watch::Receiver<bool>,
    chosen: watch::Receiver<Chosen>,
    effective: watch::Sender<bool>,
    wiring: Wiring,
    mut done: oneshot::Receiver<()>,
    tell: impl Fn(MicNews),
) {
    /// What the task was told.
    enum Told {
        Muted(bool),
        Device(Option<crate::devices::Choice>),
    }
    let mut device = chosen.borrow().microphone.clone();
    let mut chosen = Some(chosen);
    let mut control = Some(MicControl::new(Cpal::new(wiring, device.clone())));
    loop {
        let told = tokio::select! {
            _ = &mut done => break,
            changed = wanted.changed() => {
                if changed.is_err() {
                    break;
                }
                Told::Muted(*wanted.borrow_and_update())
            }
            choice = next_choice(&mut chosen, Kind::Microphone, &device) => Told::Device(choice),
        };
        let Some(mut held) = control.take() else {
            break;
        };
        let muted = match told {
            Told::Muted(muted) => muted,
            Told::Device(choice) => {
                device.clone_from(&choice);
                // Closing and opening wait on the device's thread.
                let switched = tokio::task::spawn_blocking(move || {
                    let result = held.switch(|mic| mic.choose(choice));
                    (held, result)
                })
                .await;
                let (held, result) = match switched {
                    Ok(done) => done,
                    Err(error) => {
                        log::warn!("huddle microphone: the device thread failed: {error}");
                        let _ = effective.send(true);
                        tell(MicNews::SwitchFailed(Failure::Huddle(
                            HuddleTrouble::Microphone,
                        )));
                        return;
                    }
                };
                let open = held.is_open();
                control = Some(held);
                match result {
                    Err(error) => {
                        log::warn!("huddle microphone: the new device: {error}");
                        let _ = effective.send(true);
                        tell(MicNews::SwitchFailed(Failure::Huddle(
                            HuddleTrouble::Microphone,
                        )));
                    }
                    Ok(()) if open => log::info!("huddle microphone: switched while live"),
                    Ok(()) => {}
                }
                continue;
            }
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

/// What the camera's task needs: where the encoded frames go, what the
/// session tells the sender, and the call bar's self-preview.
#[cfg(feature = "huddle-camera")]
struct CameraWiring {
    frames: mpsc::Sender<crate::huddle_audio::camera_send::VideoFrame>,
    control: crate::huddle_audio::camera_send::SendControl,
    preview: crate::huddle_camera::Preview,
}

/// The camera while it is on: its sending thread (dropping it closes the
/// camera in the helper), and what it says if it stops by itself.
#[cfg(feature = "huddle-camera")]
struct CameraOn {
    _encoding: crate::huddle_audio::camera_send::Encoding,
    ended: watch::Receiver<Option<crate::huddle_audio::camera_send::Ending>>,
}

/// What starts our camera in `helper`: `choice` (the chosen camera as
/// found now, or the system's first), encoded on the GPU if the setting
/// allows, at the rate the session aims at now, with a self-view for the
/// call bar. Also what starts it again in a fresh helper after one
/// fails: a camera has no dialog, so it can come back by itself.
#[cfg(feature = "huddle-camera")]
fn camera_starter(
    helper: &crate::huddle_audio::helper::Helper,
    control: &crate::huddle_audio::camera_send::SendControl,
    choice: noslacking_video_ipc::CameraChoice,
) -> crate::huddle_audio::camera_send::Restart {
    use crate::huddle_audio::camera_send::{PREVIEW_WIDTH, step};
    let (helper, control) = (helper.clone(), control.clone());
    Box::new(move || {
        helper.start_camera(
            choice.clone(),
            crate::huddle_audio::helper::gpu(),
            control.limits().bitrate(step(control.bitrate())),
            PREVIEW_WIDTH,
        )
    })
}

/// Opens the camera `chosen` names (the first while it is not connected,
/// or none is chosen) in the video helper and starts sending it, or why
/// not. Blocks: the helper may have to start, and macOS may ask the
/// user.
#[cfg(feature = "huddle-camera")]
fn camera_on(
    wiring: &CameraWiring,
    chosen: Option<&crate::devices::Choice>,
) -> Result<CameraOn, Failure> {
    use crate::huddle_audio::camera_send::{
        Encoding, Options, Pace, camera_choice, camera_failure,
    };
    use crate::huddle_audio::helper::{self, Lane};
    let Some(helper) = helper::shared(Lane::Camera).filter(|h| !h.given_up()) else {
        log::warn!("huddle camera: no video helper: no camera");
        return Err(Failure::Huddle(HuddleTrouble::CameraNeedsHelper));
    };
    // The chosen camera is looked for among those there are now: its
    // id is only where it was plugged in (see `crate::devices`).
    let choice = match chosen {
        None => noslacking_video_ipc::CameraChoice::First,
        Some(chosen) => match helper.cameras() {
            Ok(cameras) => camera_choice(chosen, &cameras),
            Err(trouble) => {
                log::warn!("huddle camera: no list of cameras ({trouble:?}); the first");
                noslacking_video_ipc::CameraChoice::First
            }
        },
    };
    log::info!("huddle camera: starting {choice:?}");
    // A new capture starts with a keyframe; ask for one all the same, as
    // the receivers lose the old camera's pictures.
    wiring.control.want_keyframe();
    let mut start = camera_starter(&helper, &wiring.control, choice);
    let camera = start().map_err(|trouble| {
        log::warn!("huddle camera: it did not start: {trouble:?}");
        camera_failure(&trouble, helper.given_up())
    })?;
    let (ending, ended) = watch::channel(None);
    let options = Options {
        what: "camera",
        pace: Pace::CAMERA,
        preview: Some(wiring.preview.clone()),
        restart: Some(start),
        ending: Box::new(move |trouble| {
            crate::huddle_audio::camera_send::Ending::Failed(camera_failure(
                trouble,
                helper.given_up(),
            ))
        }),
    };
    let encoding = Encoding::spawn(
        camera,
        Some(wiring.frames.clone()),
        wiring.control.clone(),
        ending,
        options,
    )
    .map_err(|why| {
        log::warn!("huddle camera: {why}");
        Failure::Huddle(HuddleTrouble::Camera)
    })?;
    Ok(CameraOn {
        _encoding: encoding,
        ended,
    })
}

/// How the camera that is on stopped by itself, or never while none is.
#[cfg(feature = "huddle-camera")]
async fn camera_ended(live: &mut Option<CameraOn>) -> Failure {
    use crate::huddle_audio::camera_send::Ending;
    let Some(CameraOn { ended, .. }) = live else {
        return std::future::pending().await;
    };
    loop {
        match ended.borrow_and_update().clone() {
            Some(Ending::Failed(failure)) => return failure,
            Some(Ending::Ended) => return Failure::Huddle(HuddleTrouble::CameraGone),
            None => {}
        }
        if ended.changed().await.is_err() {
            return std::future::pending().await;
        }
    }
}

/// Closes the camera, off this thread: its sending thread is joined and
/// the camera closed in the helper.
#[cfg(feature = "huddle-camera")]
async fn camera_off(live: Option<CameraOn>) {
    if live.is_some() {
        let _ = tokio::task::spawn_blocking(move || drop(live)).await;
    }
}

/// Opens and closes the camera (in the video helper) as `wanted` says,
/// until `done`, telling the session through `on` and the interface
/// through `tell`; closes it when Chime takes no video from us
/// (`refusals`), when it stops by itself, and at the end whatever
/// happened.
#[cfg(feature = "huddle-camera")]
async fn camera(
    mut wanted: watch::Receiver<bool>,
    chosen: watch::Receiver<Chosen>,
    on: watch::Sender<bool>,
    wiring: CameraWiring,
    mut refusals: mpsc::Receiver<()>,
    mut done: oneshot::Receiver<()>,
    tell: impl Fn(crate::huddle_camera::CamNews),
) {
    use crate::huddle_camera::CamNews;

    /// What the camera's task was told.
    enum Told {
        Want(bool),
        Device(Option<crate::devices::Choice>),
        Refused,
        Stopped(Failure),
    }

    let wiring = std::sync::Arc::new(wiring);
    let mut live: Option<CameraOn> = None;
    let mut device = chosen.borrow().camera.clone();
    let mut chosen = Some(chosen);
    loop {
        let told = tokio::select! {
            _ = &mut done => break,
            choice = next_choice(&mut chosen, Kind::Camera, &device) => Told::Device(choice),
            changed = wanted.changed() => {
                if changed.is_err() {
                    break;
                }
                Told::Want(*wanted.borrow_and_update())
            }
            refused = refusals.recv() => match refused {
                Some(()) => Told::Refused,
                None => break,
            },
            failure = camera_ended(&mut live) => Told::Stopped(failure),
        };
        let news = match told {
            Told::Want(true) if live.is_some() => CamNews::On,
            // Another camera while it is off: the next one turned on.
            Told::Device(choice) if live.is_none() => {
                device = choice;
                continue;
            }
            Told::Want(true) | Told::Device(_) => {
                if let Told::Device(choice) = told {
                    // On: the old camera closes, the new one opens.
                    log::info!("huddle camera: another camera chosen; switching");
                    device = choice;
                    camera_off(live.take()).await;
                }
                let wiring = std::sync::Arc::clone(&wiring);
                let device = device.clone();
                // Opening waits on the helper and maybe the user: not on
                // this thread.
                match tokio::task::spawn_blocking(move || camera_on(&wiring, device.as_ref())).await
                {
                    Ok(Ok(started)) => {
                        live = Some(started);
                        CamNews::On
                    }
                    Ok(Err(failure)) => CamNews::Failed(failure),
                    Err(error) => {
                        log::warn!("huddle camera: its start failed: {error}");
                        CamNews::Failed(Failure::Huddle(HuddleTrouble::Camera))
                    }
                }
            }
            Told::Want(false) => {
                camera_off(live.take()).await;
                CamNews::Off
            }
            Told::Refused => {
                camera_off(live.take()).await;
                CamNews::Failed(Failure::Huddle(HuddleTrouble::ViewOnly))
            }
            Told::Stopped(failure) => {
                log::warn!("huddle camera: it stopped: {failure:?}");
                camera_off(live.take()).await;
                CamNews::Stopped(failure)
            }
        };
        let is_on = news == CamNews::On;
        if !is_on {
            wiring.preview.clear();
        }
        let _ = on.send(is_on);
        tell(news);
    }
    let _ = on.send(false);
    camera_off(live.take()).await;
    wiring.preview.clear();
}

/// Lets go of a speaker (one that stopped, or another chosen) and opens
/// the device `choice` names for the same feed and tap, off this thread:
/// both wait on the device's. The speaker, and why the chosen device
/// gave way to the default if it did.
async fn reopen(
    old: Speaker,
    feed: Feed,
    tap: RenderTap,
    choice: Option<crate::devices::Choice>,
) -> Result<crate::huddle_audio::speaker::Opened, String> {
    let reopened = tokio::task::spawn_blocking(move || {
        drop(old);
        Speaker::reopen(&feed, Some(tap), choice)
    })
    .await;
    match reopened {
        Ok(result) => result,
        Err(error) => Err(format!("the device thread failed: {error}")),
    }
}

/// Tells the interface that the chosen speaker gave way to the default,
/// if it did (`why`, for the log).
fn fell_back(sink: &Sink, why: Option<String>) {
    if let Some(why) = why {
        log::warn!("huddle audio: {why}");
        sink.send(Event::Devices(crate::devices::Event::FellBack {
            kind: Kind::Speaker,
            failure: Failure::Huddle(HuddleTrouble::NoSound),
        }));
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
    /// The devices chosen.
    chosen: watch::Receiver<Chosen>,
    /// What the call window wants.
    #[cfg(feature = "huddle-video")]
    wishes: watch::Receiver<Wish>,
    /// Whether the camera should be on.
    #[cfg(feature = "huddle-camera")]
    camera_wanted: watch::Receiver<bool>,
    /// What is asked of the screen share.
    #[cfg(feature = "huddle-share")]
    share_requests: mpsc::Receiver<crate::huddle_share::ShareRequest>,
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
        chosen,
        #[cfg(feature = "huddle-video")]
        wishes,
        #[cfg(feature = "huddle-camera")]
        camera_wanted,
        #[cfg(feature = "huddle-share")]
        share_requests,
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
    let mut speaker_choice = chosen.borrow().speaker.clone();
    let open_choice = speaker_choice.clone();
    let ((speaker, why), feed) =
        match tokio::task::spawn_blocking(move || Speaker::open(Some(speaker_tap), open_choice))
            .await
        {
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
    fell_back(&sink, why);
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
        chosen.clone(),
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
    let mut speaker_chosen = Some(chosen.clone());
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
    // The camera: closed until turned on, then opened and encoded in
    // the video helper, its frames fetched on a thread of their own, its
    // preview for the call bar.
    #[cfg(feature = "huddle-camera")]
    let (camera_uplink, camera_task, close_camera) = {
        let waker = sink.waker();
        let preview = crate::huddle_camera::Preview::new(move || waker.wake());
        tell(Listen::Preview(preview.clone()));
        let (frames, frames_in) = mpsc::channel(crate::huddle_audio::camera_send::QUEUE);
        let control = crate::huddle_audio::camera_send::SendControl::default();
        let (refused, refusals) = mpsc::channel(1);
        let (on, on_rx) = watch::channel(false);
        let (close_camera, camera_done) = oneshot::channel();
        let camera_sink = sink.clone();
        let (camera_team, camera_channel) = (team.clone(), channel.clone());
        let task = tokio::spawn(camera(
            camera_wanted,
            chosen.clone(),
            on,
            CameraWiring {
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
            descriptor: crate::huddle_audio::camera_send::DESCRIPTOR,
        };
        (Some(uplink), task, close_camera)
    };
    // The screen share: nothing captured until asked; its own session
    // (the `#content` attendee) from then until it stops or this one
    // ends.
    #[cfg(feature = "huddle-share")]
    let (share_task, close_share) = {
        let (close_share, share_done) = oneshot::channel();
        let share_sink = sink.clone();
        let (share_team, share_channel) = (team.clone(), channel.clone());
        let task = tokio::spawn(share::run(
            joined.content(),
            share_requests,
            shares.clone(),
            share_done,
            move |news| {
                share_sink.send(Event::People {
                    team: share_team.clone(),
                    event: people::Event::Share {
                        channel: share_channel.clone(),
                        news,
                    },
                });
            },
        ));
        (task, close_share)
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
                    match reopen(old, feed.clone(), reopen_tap.clone(), speaker_choice.clone()).await {
                        Ok((fresh, why)) => {
                            fell_back(&sink, why);
                            speaker = Some(fresh);
                        }
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
            // Another speaker chosen: the same feed and tap, so the echo
            // canceller hears on, on the new device.
            choice = next_choice(&mut speaker_chosen, Kind::Speaker, &speaker_choice), if speaker.is_some() => {
                speaker_choice = choice;
                let Some(old) = speaker.take() else {
                    continue;
                };
                log::info!("huddle audio: another speaker chosen; switching");
                match reopen(old, feed.clone(), reopen_tap.clone(), speaker_choice.clone()).await {
                    Ok((fresh, why)) => {
                        fell_back(&sink, why);
                        speaker = Some(fresh);
                    }
                    Err(why) => {
                        log::warn!("huddle audio: {why}");
                        sound_stopped = true;
                        let _ = halt.send(true);
                    }
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
    // Left: the microphone, the camera and the share close before
    // anything else.
    let _ = close_mic.send(());
    #[cfg(feature = "huddle-camera")]
    let _ = close_camera.send(());
    #[cfg(feature = "huddle-share")]
    let _ = close_share.send(());
    let _ = mic.await;
    #[cfg(feature = "huddle-camera")]
    let _ = camera_task.await;
    #[cfg(feature = "huddle-share")]
    let _ = share_task.await;
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

    #[tokio::test]
    async fn each_task_hears_only_of_its_own_device() {
        use crate::devices::Choice;
        let headset = Choice {
            id: "alsa:sysdefault:CARD=Headset".into(),
            name: "Headset".into(),
        };
        let (devices, chosen) = watch::channel(Chosen::default());
        let mut chosen = Some(chosen);
        let mut current = None;
        // The speaker changes: the microphone's task goes on waiting.
        devices.send_modify(|c| c.speaker = Some(headset.clone()));
        let waited = tokio::time::timeout(
            Duration::from_millis(50),
            next_choice(&mut chosen, Kind::Microphone, &current),
        )
        .await;
        assert!(waited.is_err(), "not the microphone");
        // The microphone changes: told at once, with the new one.
        devices.send_modify(|c| c.microphone = Some(headset.clone()));
        let told = next_choice(&mut chosen, Kind::Microphone, &current).await;
        assert_eq!(told.as_ref(), Some(&headset));
        current = told;
        // The same again tells nothing; back to the default does.
        devices.send_modify(|c| c.microphone = Some(headset.clone()));
        devices.send_modify(|c| c.microphone = None);
        assert_eq!(
            next_choice(&mut chosen, Kind::Microphone, &current).await,
            None
        );
        // Once the interface lets go, never again.
        drop(devices);
        let waited = tokio::time::timeout(
            Duration::from_millis(50),
            next_choice(&mut chosen, Kind::Microphone, &None),
        )
        .await;
        assert!(waited.is_err());
        assert!(chosen.is_none());
    }

    #[test]
    fn the_session_hears_of_devices_chosen_before_and_during_it() {
        use crate::devices::Choice;
        let brio = Choice {
            id: "v4l2:/dev/video2".into(),
            name: "Logitech BRIO".into(),
        };
        let mut listener = Listener::default();
        let mut chosen = Chosen {
            camera: Some(brio),
            ..Chosen::default()
        };
        // Before any huddle: kept for the next.
        listener.use_devices(chosen.clone());
        assert_eq!(listener.chosen, chosen);
        // In one: passed on at once.
        let (stop, _stopped) = watch::channel(false);
        let (muted, _wanted) = watch::channel(true);
        let (devices, mut heard) = watch::channel(listener.chosen.clone());
        #[cfg(feature = "huddle-video")]
        let (wish, _wishes) = watch::channel(Wish::closed());
        #[cfg(feature = "huddle-camera")]
        let (camera, _camera_wanted) = watch::channel(false);
        #[cfg(feature = "huddle-share")]
        let (share, _share_requests) = mpsc::channel(1);
        listener.running = Some(Running {
            team: "T1".into(),
            stop,
            muted,
            devices,
            #[cfg(feature = "huddle-video")]
            wish,
            #[cfg(feature = "huddle-camera")]
            camera,
            #[cfg(feature = "huddle-share")]
            share,
        });
        chosen.camera = None;
        listener.use_devices(chosen.clone());
        assert!(heard.has_changed().expect("open"));
        assert_eq!(*heard.borrow_and_update(), chosen);
    }

    /// The camera's task switching cameras while on, against the
    /// helper's own code on a thread: the old one closes, the chosen one
    /// opens and its first frame is a keyframe; off, a new choice waits
    /// for the camera to be turned on.
    #[cfg(feature = "huddle-camera")]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_camera_task_switches_cameras_while_on() {
        use crate::devices::Choice;
        use crate::huddle_audio::camera_send::{QUEUE, SendControl};
        use crate::huddle_camera::{CamNews, Preview};
        use std::sync::Mutex;
        let (frames, mut frames_in) = mpsc::channel(QUEUE);
        let (want, wanted) = watch::channel(false);
        let (devices, chosen) = watch::channel(Chosen::default());
        let (on, mut on_rx) = watch::channel(false);
        let (_refused, refusals) = mpsc::channel(1);
        let (close, done) = oneshot::channel();
        let news = std::sync::Arc::new(Mutex::new(Vec::new()));
        let heard = std::sync::Arc::clone(&news);
        let task = tokio::spawn(camera(
            wanted,
            chosen,
            on,
            CameraWiring {
                frames,
                control: SendControl::default(),
                preview: Preview::new(|| {}),
            },
            refusals,
            done,
            move |news| heard.lock().expect("a lock").push(news),
        ));
        // Off: choosing opens nothing and says nothing.
        let pretend = Choice {
            id: "pretend:camera".into(),
            name: "A pretend camera".into(),
        };
        devices.send_modify(|c| c.camera = Some(pretend.clone()));
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(news.lock().expect("a lock").is_empty());
        want.send(true).expect("sent");
        on_rx.changed().await.expect("told");
        let first = tokio::time::timeout(Duration::from_secs(10), frames_in.recv())
            .await
            .expect("in time")
            .expect("a frame");
        assert!(first.keyframe);
        // On: another camera (not connected: the first) starts at once.
        devices.send_modify(|c| {
            c.camera = Some(Choice {
                id: "pretend:9".into(),
                name: "Gone".into(),
            });
        });
        on_rx.changed().await.expect("told");
        // What the old one queued, then the new one's first: a keyframe.
        let mut keyframe = false;
        for _ in 0..QUEUE + 10 {
            let frame = tokio::time::timeout(Duration::from_secs(10), frames_in.recv())
                .await
                .expect("in time")
                .expect("a frame");
            if frame.keyframe {
                keyframe = true;
                break;
            }
        }
        assert!(keyframe, "the new camera starts with a keyframe");
        let _ = close.send(());
        task.await.expect("the task ends");
        assert_eq!(*news.lock().expect("a lock"), [CamNews::On, CamNews::On]);
    }

    /// The camera's task against the helper's own code on a thread (its
    /// pretend camera is the test camera): off until turned on, then
    /// frames for the session and pictures for the bar; off again when
    /// turned off, and when Chime takes no video, saying so.
    #[cfg(feature = "huddle-camera")]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_camera_task_opens_the_helpers_camera_only_while_on() {
        use crate::huddle_audio::camera_send::{QUEUE, SendControl};
        use crate::huddle_camera::{CamNews, Preview};
        use std::sync::Mutex;
        let (frames, mut frames_in) = mpsc::channel(QUEUE);
        let preview = Preview::new(|| {});
        let (want, wanted) = watch::channel(false);
        let (on, mut on_rx) = watch::channel(false);
        let (refused, refusals) = mpsc::channel(1);
        let (close, done) = oneshot::channel();
        let news = std::sync::Arc::new(Mutex::new(Vec::new()));
        let heard = std::sync::Arc::clone(&news);
        let (_devices, chosen) = watch::channel(Chosen::default());
        let task = tokio::spawn(camera(
            wanted,
            chosen,
            on,
            CameraWiring {
                frames,
                control: SendControl::default(),
                preview: preview.clone(),
            },
            refusals,
            done,
            move |news| heard.lock().expect("a lock").push(news),
        ));
        // Joined: nothing comes until it is turned on.
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(frames_in.try_recv().is_err());
        assert_eq!(preview.pictures(), 0);
        want.send(true).expect("sent");
        on_rx.changed().await.expect("told");
        assert!(*on_rx.borrow());
        let mut got = Vec::new();
        while got.len() < 5 {
            let frame = tokio::time::timeout(Duration::from_secs(10), frames_in.recv())
                .await
                .expect("in time")
                .expect("a frame");
            got.push(frame);
        }
        assert!(got[0].keyframe);
        assert!(preview.pictures() >= 4, "{}", preview.pictures());
        // Off: the frames stop.
        want.send(false).expect("sent");
        on_rx.changed().await.expect("told");
        assert!(!*on_rx.borrow());
        tokio::time::sleep(Duration::from_millis(100)).await;
        while frames_in.try_recv().is_ok() {}
        tokio::time::sleep(Duration::from_millis(150)).await;
        assert!(frames_in.try_recv().is_err(), "nothing once off");
        // On again, then refused by Chime: off, saying why.
        want.send(true).expect("sent");
        on_rx.changed().await.expect("told");
        refused.send(()).await.expect("sent");
        on_rx.changed().await.expect("told");
        assert!(!*on_rx.borrow());
        let _ = close.send(());
        task.await.expect("the task ends");
        assert_eq!(
            *news.lock().expect("a lock"),
            [
                CamNews::On,
                CamNews::Off,
                CamNews::On,
                CamNews::Failed(Failure::Huddle(HuddleTrouble::ViewOnly))
            ]
        );
    }
}
