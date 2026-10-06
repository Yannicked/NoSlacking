//! Listening to a huddle, the app's side (the `huddle-audio` feature):
//! one huddle at a time, shown in the call bar from joining until it is
//! left, with who is in it and who speaks. Left when asked (the bar's
//! Leave, the conversation header's, Ctrl+Shift+H), when everyone else
//! has been gone a minute, on sign-out and on quit; when the huddle ends
//! Chime closes it and the bar says so.

use std::time::{Duration, Instant};

use crate::app::App;
use crate::backend;
use crate::failure::Failure;
use crate::i18n::{t, tf};
use crate::people;

pub use crate::huddle_audio::roster::{Person, Roster};

/// How long you stay once everyone else has left before leaving too:
/// long enough for someone dropping out and back in.
pub const ALONE_FOR: Duration = Duration::from_secs(60);
/// How long a failure shows in the call bar, unless closed sooner.
pub const FAILED_FOR: Duration = Duration::from_secs(30);

/// How listening ended without failing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Left {
    /// You left: asked here, or by signing out or quitting.
    Asked,
    /// The huddle itself ended.
    Ended,
}

/// Where listening to a huddle got to, as the worker tells it.
#[derive(Clone, Debug, PartialEq)]
pub enum Listen {
    /// Joining: Slack, then Chime, then the audio connection.
    Joining,
    /// The audio is connected and playing.
    Live,
    /// Who is in the huddle and speaking now. Sent when it changes, at
    /// most a few times a second.
    Roster(Roster),
    /// Left, the huddle over, or failed.
    Ended(Result<Left, Failure>),
}

/// Where the huddle being listened to is.
#[derive(Clone, Debug, PartialEq)]
pub enum Phase {
    /// Joining it.
    Joining,
    /// Listening since `since`.
    Live { since: Instant },
    /// It failed at `at`; the bar shows why for a while.
    Failed { error: Failure, at: Instant },
}

/// The huddle being listened to: one at a time.
#[derive(Clone, Debug, PartialEq)]
pub struct Listening {
    /// The workspace.
    pub team: String,
    /// The conversation the huddle is in.
    pub channel: String,
    pub phase: Phase,
    /// Who is in it, as last told.
    pub roster: Roster,
    /// Since when no one else has been in it, while that lasts.
    pub alone_since: Option<Instant>,
    /// The microphone: muted on joining.
    pub mic: crate::huddle_mic::Mic,
}

impl Listening {
    /// Joining the huddle in `channel` of `team`.
    pub fn new(team: &str, channel: &str) -> Self {
        Self {
            team: team.to_owned(),
            channel: channel.to_owned(),
            phase: Phase::Joining,
            roster: Roster::default(),
            alone_since: None,
            mic: crate::huddle_mic::Mic::Muted,
        }
    }

    /// Whether you are in the huddle, joining or listening, rather than
    /// looking at why it failed.
    pub fn in_huddle(&self) -> bool {
        !matches!(self.phase, Phase::Failed { .. })
    }

    /// Whether this is the huddle in `channel` of `team`, and you are in
    /// it.
    pub fn is(&self, team: &str, channel: &str) -> bool {
        self.in_huddle() && self.team == team && self.channel == channel
    }
}

/// Since when you have been alone in the huddle, after `roster` came at
/// `now`: still `was` while you stay alone, from now once you are,
/// `None` while anyone else is there. Only counts once the audio plays.
pub fn alone_since(
    was: Option<Instant>,
    roster: &Roster,
    live: bool,
    now: Instant,
) -> Option<Instant> {
    (live && roster.alone()).then(|| was.unwrap_or(now))
}

/// Whether to leave at `now`, alone since `since`.
pub fn leave_alone(since: Option<Instant>, now: Instant) -> bool {
    since.is_some_and(|since| now.saturating_duration_since(since) >= ALONE_FOR)
}

/// A call's length as a clock: `0:05`, `12:34`, `1:02:03`.
pub fn clock(length: Duration) -> String {
    let seconds = length.as_secs();
    let (hours, minutes, seconds) = (seconds / 3600, seconds / 60 % 60, seconds % 60);
    if hours > 0 {
        format!("{hours}:{minutes:02}:{seconds:02}")
    } else {
        format!("{minutes}:{seconds:02}")
    }
}

/// Where the huddle is, for the bar's title.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Place<'a> {
    /// A channel, by name.
    Channel(&'a str),
    /// A direct or group message, by who is in it.
    Direct(&'a str),
}

/// The call bar's title: "Listening in #design", "Listening with Ana".
pub fn title_text(place: Place<'_>) -> String {
    match place {
        Place::Channel(name) => tf("Listening in #{channel}", &[("channel", name)]),
        Place::Direct(name) => tf("Listening with {name}", &[("name", name)]),
    }
}

/// What the call bar says of where listening is at `now`.
pub fn status_text(phase: &Phase, now: Instant) -> String {
    match phase {
        Phase::Joining => t("Joining…").into_owned(),
        Phase::Live { since } => tf(
            "Live · {time}",
            &[("time", &clock(now.saturating_duration_since(*since)))],
        ),
        Phase::Failed { error, .. } => {
            tf("Could not listen: {error}", &[("error", &error.message())])
        }
    }
}

/// The faces the call bar shows: one per person, however many devices
/// they are in on (you in Slack and here, say), the others first in the
/// order they came, you last.
pub fn faces(roster: &Roster) -> Vec<Person> {
    let mut faces: Vec<Person> = Vec::new();
    for person in &roster.people {
        let same = faces
            .iter_mut()
            .find(|f| f.user.is_some() && f.user == person.user);
        match same {
            Some(face) => {
                face.me |= person.me;
                face.muted &= person.muted;
                face.speaking |= person.speaking;
            }
            None => faces.push(person.clone()),
        }
    }
    faces.sort_by_key(|f| f.me);
    faces
}

/// Listens to the huddle in `channel` of `team` here, leaving any other.
pub fn listen(app: &mut App, team: String, channel: String) {
    if !super::is_session(app, &team) {
        return;
    }
    // The worker listens to one huddle at a time and leaves the last
    // itself; a different workspace's is told to stop.
    if let Some(last) = app.huddles.listening.take()
        && last.team != team
        && last.in_huddle()
    {
        app.backend.send(backend::Command::People {
            team: last.team,
            command: people::Command::LeaveHuddle,
        });
    }
    app.huddles.listening = Some(Listening::new(&team, &channel));
    app.backend.send(backend::Command::People {
        team,
        command: people::Command::ListenHuddle { channel },
    });
}

/// Leaves the huddle being listened to, or closes the failure shown.
pub fn leave(app: &mut App) {
    if let Some(last) = app.huddles.listening.take()
        && last.in_huddle()
    {
        app.backend.send(backend::Command::People {
            team: last.team,
            command: people::Command::LeaveHuddle,
        });
    }
}

/// Takes in where listening to the huddle in `channel` of `team` got to.
/// News of a session already left or replaced is dropped.
pub fn heard(app: &mut App, team: &str, channel: &str, state: Listen) {
    let now = Instant::now();
    let Some(listening) = app
        .huddles
        .listening
        .as_mut()
        .filter(|l| l.is(team, channel))
    else {
        return;
    };
    match state {
        Listen::Joining => {}
        Listen::Live => {
            if !matches!(listening.phase, Phase::Live { .. }) {
                listening.phase = Phase::Live { since: now };
            }
        }
        Listen::Roster(roster) => {
            let live = matches!(listening.phase, Phase::Live { .. });
            listening.alone_since = alone_since(listening.alone_since, &roster, live, now);
            listening.roster = roster;
        }
        Listen::Ended(Ok(Left::Asked)) => app.huddles.listening = None,
        Listen::Ended(Ok(Left::Ended)) => {
            app.huddles.listening = None;
            app.toast(t("The huddle ended"), false);
        }
        Listen::Ended(Err(error)) => {
            listening.phase = Phase::Failed { error, at: now };
            listening.alone_since = None;
        }
    }
}

/// Runs every frame: forgets the huddle of a workspace signed out, leaves
/// one everyone else left a while ago, and lets a failure go after a
/// while.
pub fn frame(app: &mut App, now: Instant) {
    let Some(listening) = &app.huddles.listening else {
        return;
    };
    // Signed out while listening: the worker stopped it, and its news
    // went with the workspace.
    if !super::is_session(app, &listening.team) {
        app.huddles.listening = None;
        return;
    }
    if let Phase::Failed { at, .. } = listening.phase {
        let until = at + FAILED_FOR;
        if until <= now {
            app.huddles.listening = None;
        } else {
            app.waker.wake_after(until - now);
        }
        return;
    }
    if leave_alone(listening.alone_since, now) {
        leave(app);
        app.toast(t("Everyone else left the huddle"), false);
    } else if let Some(since) = listening.alone_since {
        app.waker
            .wake_after((since + ALONE_FOR).saturating_duration_since(now));
    }
}

/// Leaves the huddle being listened to before the app quits, waiting a
/// little for Chime to hear it: the worker's thread goes with the app.
pub fn quit(app: &mut App) {
    let Some(last) = app.huddles.listening.take().filter(Listening::in_huddle) else {
        return;
    };
    app.backend.send(backend::Command::People {
        team: last.team,
        command: people::Command::LeaveHuddle,
    });
    let until = Instant::now() + Duration::from_secs(4);
    while Instant::now() < until {
        match app.backend.try_recv() {
            Some(backend::Event::People {
                event:
                    people::Event::Listening {
                        state: Listen::Ended(_),
                        ..
                    },
                ..
            }) => return,
            Some(_) => {}
            None => std::thread::sleep(Duration::from_millis(20)),
        }
    }
    log::warn!("left the huddle without hearing back");
}

#[cfg(test)]
mod tests {
    use super::*;

    fn person(user: &str, me: bool, muted: bool, speaking: bool) -> Person {
        Person {
            user: Some(user.into()),
            me,
            muted,
            speaking,
        }
    }

    #[test]
    fn everyone_else_gone_a_minute_means_leaving() {
        let now = Instant::now();
        let alone = Roster {
            people: vec![person("U0", true, true, false)],
            count: Some(1),
        };
        let company = Roster {
            people: vec![
                person("U0", true, true, false),
                person("U2", false, false, false),
            ],
            count: Some(2),
        };
        // Not while joining: the roster may not have come in full.
        assert_eq!(alone_since(None, &alone, false, now), None);
        let since = alone_since(None, &alone, true, now);
        assert_eq!(since, Some(now));
        // Staying alone keeps the first moment.
        let later = now + Duration::from_secs(30);
        assert_eq!(alone_since(since, &alone, true, later), Some(now));
        assert!(!leave_alone(since, later));
        assert!(leave_alone(since, now + ALONE_FOR));
        // Someone back in time: the minute starts over.
        assert_eq!(alone_since(since, &company, true, later), None);
        assert!(!leave_alone(None, now + ALONE_FOR * 10));
    }

    #[test]
    fn the_bar_says_where_and_how_long() {
        assert_eq!(clock(Duration::from_secs(5)), "0:05");
        assert_eq!(clock(Duration::from_secs(12 * 60 + 34)), "12:34");
        assert_eq!(clock(Duration::from_secs(3723)), "1:02:03");
        assert_eq!(title_text(Place::Channel("design")), "Listening in #design");
        assert_eq!(title_text(Place::Direct("Ana")), "Listening with Ana");
        let now = Instant::now();
        assert_eq!(status_text(&Phase::Joining, now), "Joining…");
        assert_eq!(
            status_text(
                &Phase::Live {
                    since: now - Duration::from_secs(134)
                },
                now
            ),
            "Live · 2:14"
        );
        let failed = Phase::Failed {
            error: Failure::Huddle(crate::failure::HuddleTrouble::Lost),
            at: now,
        };
        assert!(status_text(&failed, now).starts_with("Could not listen: "));
    }

    #[test]
    fn faces_are_one_per_person_you_last() {
        let roster = Roster {
            people: vec![
                person("U0", true, true, false),
                person("U2", false, false, true),
                // You again, from Slack's own app, unmuted and talking.
                person("U0", false, false, true),
                person("U3", false, true, false),
                Person {
                    user: None,
                    me: false,
                    muted: false,
                    speaking: false,
                },
            ],
            count: Some(5),
        };
        let faces = faces(&roster);
        let seen: Vec<(Option<&str>, bool, bool, bool)> = faces
            .iter()
            .map(|f| (f.user.as_deref(), f.me, f.muted, f.speaking))
            .collect();
        assert_eq!(
            seen,
            [
                (Some("U2"), false, false, true),
                (Some("U3"), false, true, false),
                (None, false, false, false),
                (Some("U0"), true, false, true),
            ]
        );
    }

    #[test]
    fn news_of_another_huddle_is_dropped() {
        let mut listening = Listening::new("T1", "C1");
        assert!(listening.is("T1", "C1"));
        assert!(!listening.is("T1", "C2") && !listening.is("T2", "C1"));
        listening.phase = Phase::Failed {
            error: Failure::Huddle(crate::failure::HuddleTrouble::Lost),
            at: Instant::now(),
        };
        assert!(!listening.in_huddle() && !listening.is("T1", "C1"));
    }
}
