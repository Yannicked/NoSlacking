//! The worker's side of a Teams call: one at a time, placed through
//! [`crate::teams::calling::call::outgoing`] with the huddle's speaker
//! and microphone, ended when asked, on sign-out, or when another call or
//! huddle starts.
//!
//! It speaks to the interface as a huddle does, through
//! [`crate::people::Event::Listening`] and
//! [`crate::people::Event::Microphone`], so the call bar shows it as it
//! shows a huddle. Unlike a huddle, a call starts unmuted: you rang
//! someone to talk.

use std::sync::{Arc, Mutex};

use tokio::sync::{mpsc, oneshot, watch};

use super::listen::microphone;
use super::{Event, Sink};
use crate::failure::{Failure, HuddleTrouble};
use crate::huddle_audio::media::Uplink;
use crate::huddle_audio::microphone::Wiring;
use crate::huddle_audio::processing::RenderTap;
use crate::huddle_audio::roster::{Person, Roster};
use crate::huddle_audio::speaker::Speaker;
use crate::huddles::{Left, Listen};
use crate::people;
use crate::teams::calling::call::{CallEvent, Control, outgoing};
use crate::teams::calling::media::Audio;
use crate::teams::client::TeamsClient;

/// The call going on.
#[derive(Debug)]
struct Running {
    team: String,
    control: mpsc::UnboundedSender<Control>,
    /// Whether the interface wants the microphone muted.
    muted: watch::Sender<bool>,
}

/// The one Teams call there may be.
#[derive(Debug, Default)]
pub struct Caller {
    running: Option<Running>,
}

impl Caller {
    /// Calls `callee` (an MRI) from `channel` of `team`, ending the last
    /// call first.
    pub fn start(
        &mut self,
        client: TeamsClient,
        team: String,
        channel: String,
        callee: String,
        sink: Sink,
    ) {
        self.stop();
        let (control, controls) = mpsc::unbounded_channel();
        let (muted, wanted) = watch::channel(true);
        // Unmuted from the start: the microphone task opens it on this.
        let _ = muted.send(false);
        tokio::spawn(run(
            client,
            Place {
                team: team.clone(),
                channel,
                callee,
            },
            controls,
            wanted,
            sink,
        ));
        self.running = Some(Running {
            team,
            control,
            muted,
        });
    }

    /// Mutes or unmutes the microphone in the call, if there is one.
    pub fn set_muted(&mut self, muted: bool) {
        if let Some(running) = &self.running {
            let _ = running.muted.send(muted);
            let _ = running.control.send(Control::Mute(muted));
        }
    }

    /// Hangs up; the call says when it has ended.
    pub fn stop(&mut self) {
        if let Some(running) = self.running.take() {
            let _ = running.control.send(Control::HangUp);
        }
    }

    /// Hangs up if the call is in `team`, which signed out.
    pub fn signed_out(&mut self, team: &str) {
        if self.running.as_ref().is_some_and(|r| r.team == team) {
            self.stop();
        }
    }
}

/// Where a call is, and to whom.
struct Place {
    team: String,
    channel: String,
    callee: String,
}

/// Who is in a call with `callee`: you and them. The far end's mute and
/// speaking are not known; the bar shows them as there.
fn roster(callee: &str) -> Roster {
    Roster {
        people: vec![
            Person {
                user: Some(callee.to_owned()),
                me: false,
                muted: false,
                speaking: false,
            },
            Person {
                user: None,
                me: true,
                muted: false,
                speaking: false,
            },
        ],
        count: Some(2),
    }
}

/// What the interface hears of the call's end.
fn ending(result: Result<(), Failure>) -> Result<Left, Failure> {
    // Hung up here, the interface has already let the call go and drops
    // this; so an ending that reaches it was the far end's.
    result.map(|()| Left::Ended)
}

/// One call, from ringing to its end.
async fn run(
    client: TeamsClient,
    place: Place,
    controls: mpsc::UnboundedReceiver<Control>,
    wanted: watch::Receiver<bool>,
    sink: Sink,
) {
    let Place {
        team,
        channel,
        callee,
    } = place;
    let tell = {
        let (sink, team, channel) = (sink.clone(), team.clone(), channel.clone());
        move |state: Listen| {
            sink.send(Event::People {
                team: team.clone(),
                event: people::Event::Listening {
                    channel: channel.clone(),
                    state,
                },
            });
        }
    };
    tell(Listen::Joining);
    tell(Listen::Roster(roster(&callee)));
    let tap = RenderTap::default();
    let speaker_tap = tap.clone();
    let (speaker, feed) =
        match tokio::task::spawn_blocking(move || Speaker::open(Some(speaker_tap))).await {
            Ok(Ok(opened)) => opened,
            Ok(Err(why)) => {
                log::warn!("Teams call: {why}");
                tell(Listen::Ended(Err(Failure::Huddle(HuddleTrouble::NoSound))));
                return;
            }
            Err(error) => {
                log::warn!("Teams call: the device thread failed: {error}");
                tell(Listen::Ended(Err(Failure::Huddle(HuddleTrouble::NoSound))));
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
    let audio = Audio {
        feed: Some(feed),
        uplink: Some(Uplink {
            frames: frames_in,
            muted,
        }),
    };
    // The end is told only once the devices are closed, so nothing of
    // this call follows it.
    let ended: Arc<Mutex<Option<Result<(), Failure>>>> = Arc::default();
    let slot = ended.clone();
    let told = tell.clone();
    outgoing(client, callee, audio, controls, move |event| match event {
        CallEvent::Ringing => told(Listen::Ringing),
        CallEvent::Live => told(Listen::Live),
        CallEvent::AudioFlowing => {}
        CallEvent::Ended { result, .. } => {
            if let Ok(mut slot) = slot.lock() {
                *slot = Some(result);
            }
        }
    })
    .await;
    let _ = close_mic.send(());
    let _ = mic.await;
    // Stopping the device waits for its thread; not on this one.
    let _ = tokio::task::spawn_blocking(move || drop(speaker)).await;
    let result = ended
        .lock()
        .ok()
        .and_then(|mut slot| slot.take())
        .unwrap_or(Ok(()));
    tell(Listen::Ended(ending(result)));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_call_is_you_and_the_one_you_rang() {
        let roster = roster("8:live:ana");
        assert_eq!(roster.people.len(), 2);
        assert!(!roster.alone());
        assert_eq!(roster.people[0].user.as_deref(), Some("8:live:ana"));
        assert!(roster.people[1].me);
    }

    #[test]
    fn an_ending_that_reaches_the_interface_was_the_far_ends() {
        assert_eq!(ending(Ok(())), Ok(Left::Ended));
        assert_eq!(
            ending(Err(Failure::CallDeclined)),
            Err(Failure::CallDeclined)
        );
    }
}
