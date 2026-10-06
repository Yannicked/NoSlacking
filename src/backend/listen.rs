//! The worker's side of listening to a huddle (the `huddle-audio`
//! feature): one huddle at a time, joined through Slack, played through
//! [`crate::huddle_audio`], left when asked, on sign-out, or when another
//! starts. Its news goes to the interface as
//! [`crate::people::Event::Listening`]; failures as a
//! [`Failure`], their technical detail only in the log.

use std::time::Duration;

use tokio::sync::{oneshot, watch};

use super::{Event, Sink};
use crate::failure::{Failure, HuddleTrouble};
use crate::huddle_audio::join::{self, JoinFailure};
use crate::huddle_audio::media::{self, Stage};
use crate::huddle_audio::roster::Roster;
use crate::huddle_audio::speaker::Speaker;
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
        tokio::spawn(run(client, team.clone(), channel, stopped, sink));
        self.running = Some(Running { team, stop });
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

/// One listening session, start to end.
async fn run(
    client: Client,
    team: String,
    channel: String,
    stopped: watch::Receiver<bool>,
    sink: Sink,
) {
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
    let (speaker, feed) = match tokio::task::spawn_blocking(Speaker::open).await {
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
    let (live, connected) = oneshot::channel();
    let (roster, mut rosters) = watch::channel(Roster::default());
    let listening = media::listen(&joined, Some(feed), stopped, Some(live), Some(roster));
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
        }
    };
    // Stopping the device waits for its thread; not on this one.
    let _ = tokio::task::spawn_blocking(move || drop(speaker)).await;
    log::info!(
        "huddle audio: {} frames in {} bytes; ended: {}",
        report.audio_frames,
        report.audio_bytes,
        report.ending.as_deref().unwrap_or("-")
    );
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
