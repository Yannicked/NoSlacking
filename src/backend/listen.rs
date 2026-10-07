//! The worker's side of listening to a huddle (the `huddle-audio`
//! feature): one huddle at a time, joined through Slack, played through
//! [`crate::huddle_audio`], left when asked, on sign-out, or when another
//! starts. Its news goes to the interface as
//! [`crate::people::Event::Listening`]; failures as a
//! [`Failure`], their technical detail only in the log.
//!
//! Joined muted. The microphone opens only when the interface unmutes
//! and closes when it mutes or the huddle is left; what it is doing goes
//! back as [`crate::people::Event::Microphone`]. The session sends the
//! microphone's frames only once it is open (`effective` below), so a
//! microphone that would not open never unmutes the call.
//!
//! With `huddle-video`, the session also says who shares their screen
//! (`Listen::Shares`) and hands over the `Screen` the watched share's
//! pictures arrive in (`Listen::Screen`); the interface says which
//! share its call window shows, and only that one is received.

use std::time::Duration;

use tokio::sync::{mpsc, oneshot, watch};

use super::{Event, Sink};
use crate::failure::{Failure, HuddleTrouble};
use crate::huddle_audio::join::{self, JoinFailure};
use crate::huddle_audio::media::{self, Stage, Uplink};
use crate::huddle_audio::microphone::{Cpal, MicControl, Wiring};
use crate::huddle_audio::processing::RenderTap;
use crate::huddle_audio::roster::Roster;
#[cfg(feature = "huddle-video")]
use crate::huddle_audio::screen::Screen;
use crate::huddle_audio::speaker::Speaker;
use crate::huddle_audio::video::Share;
use crate::huddle_mic::MicNews;
use crate::huddles::{Left, Listen};
use crate::people;
use crate::slack::Client;

/// The shortest time between two rosters sent to the interface. Chime
/// sends volumes several times a second; the window need not wake for
/// each, and a speaking mark held a moment longer reads as well.
const ROSTER_EVERY: Duration = Duration::from_millis(250);

/// The huddle being listened to.
#[derive(Debug)]
struct Running {
    team: String,
    stop: watch::Sender<bool>,
    /// Whether the interface wants the microphone muted.
    muted: watch::Sender<bool>,
    /// Which share the call window shows.
    #[cfg(feature = "huddle-video")]
    watched: watch::Sender<Option<String>>,
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
        let (watched, watching) = watch::channel(None);
        let controls = Controls {
            stopped,
            wanted,
            #[cfg(feature = "huddle-video")]
            watching,
        };
        tokio::spawn(run(client, team.clone(), channel, controls, sink));
        self.running = Some(Running {
            team,
            stop,
            muted,
            #[cfg(feature = "huddle-video")]
            watched,
        });
    }

    /// Shows the share `key` in the call window, or none: only that one
    /// is received.
    #[cfg(feature = "huddle-video")]
    pub fn watch_share(&mut self, key: Option<String>) {
        if let Some(running) = &self.running {
            let _ = running.watched.send(key);
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
async fn microphone(
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

/// How a session that ran ended, for the interface: left when asked,
/// the huddle over, or a failure in words.
fn ending(meeting_ended: bool, result: Result<(), media::Failure>) -> Result<Left, Failure> {
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
    /// Which share the call window shows.
    #[cfg(feature = "huddle-video")]
    watching: watch::Receiver<Option<String>>,
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

/// One listening session, start to end.
async fn run(client: Client, team: String, channel: String, controls: Controls, sink: Sink) {
    let Controls {
        stopped,
        wanted,
        #[cfg(feature = "huddle-video")]
        watching,
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
    // Who shares, for the call bar, and the screen the watched share's
    // pictures go to, woken as the events are.
    #[cfg(feature = "huddle-video")]
    let (viewer, mut shares) = {
        let waker = sink.waker();
        let screen = Screen::new(move || waker.wake());
        tell(Listen::Screen(screen.clone()));
        let (shares, told) = watch::channel(Vec::new());
        let viewer = crate::huddle_audio::watch::Viewer {
            shares,
            watched: watching,
            screen,
        };
        (Some(viewer), Some(told))
    };
    #[cfg(not(feature = "huddle-video"))]
    let (viewer, mut shares) = (None, None);
    let listening = media::listen(
        &joined,
        Some(feed),
        Some(uplink),
        stopped,
        Some(live),
        Some(roster),
        media::Video {
            options: crate::huddle_audio::video::for_app(),
            viewer,
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
        }
    };
    // Left: the microphone closes before anything else.
    let _ = close_mic.send(());
    let _ = mic.await;
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
    tell(Listen::Ended(ending(report.meeting_ended, result)));
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
        assert_eq!(ending(true, Ok(())), Ok(Left::Ended));
        assert_eq!(ending(false, Ok(())), Ok(Left::Asked));
        assert_eq!(
            ending(
                false,
                Err(media::Failure {
                    stage: Stage::Media,
                    why: "relay lost".into()
                })
            ),
            Err(Failure::Huddle(HuddleTrouble::Lost))
        );
    }
}
