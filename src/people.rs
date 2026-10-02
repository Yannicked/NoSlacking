//! People around you: who is active or away, and who is typing.
//!
//! The app works out which people are on screen (your direct messages,
//! the profile card, a channel's member list) and asks the worker to
//! watch them with [`Command::Watch`]. The worker answers with
//! [`Event`]s, which [`handle`] keeps in each workspace's [`TeamPeople`].
//! Views only read that, and push [`Action`]s (wrapped in
//! [`crate::model::Action::People`]) for [`apply`].

use std::collections::HashMap;
use std::time::{Duration, Instant};

use crate::app::{App, WorkspaceState};
use crate::backend;
use crate::i18n::{t, tf};
use crate::model::{ConversationKind, Ts};

/// How many direct messages, newest first, have their people watched. The
/// sidebar shows about this many before "Show more".
const WATCHED_DMS: usize = 40;
/// How many people of an open member list are watched.
const WATCHED_MEMBERS: usize = 100;
/// How often the app works out who is on screen. People come and go from
/// the screen slowly; once a second is plenty.
const WATCH_EVERY: Duration = Duration::from_secs(1);
/// How long "is typing" shows after Slack last said so. Slack repeats it
/// every few seconds while someone types.
pub const TYPING_FOR: Duration = Duration::from_secs(5);
/// The shortest gap between two of your own "typing" notices for one
/// conversation, as Slack's own client spaces them.
const TYPING_EVERY: Duration = Duration::from_secs(3);

/// Whether someone is around, as Slack says.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Presence {
    Active,
    Away,
}

impl Presence {
    /// Slack's word for it: `active` or `away`. Anything else is unknown.
    pub fn parse(word: &str) -> Option<Self> {
        match word {
            "active" => Some(Self::Active),
            "away" => Some(Self::Away),
            _ => None,
        }
    }
}

/// What the views ask for.
#[derive(Clone, Debug, PartialEq)]
pub enum Action {
    /// You typed in the composer of a conversation in the open workspace,
    /// or of one of its threads.
    Typing { channel: String, thread: Option<Ts> },
}

/// What the interface asks the worker to do for one workspace.
#[derive(Clone, Debug, PartialEq)]
pub enum Command {
    /// Keep these people's presence up to date, and stop for anyone else.
    /// Sent whenever the set on screen changes; empty stops watching.
    Watch { users: Vec<String> },
    /// Tell the others you are typing. Only a browser session's RTM socket
    /// can; otherwise nothing happens.
    Typing { channel: String, thread: Option<Ts> },
}

/// What the worker answers.
#[derive(Clone, Debug, PartialEq)]
pub enum Event {
    /// These people are now active or away.
    Presence { users: Vec<(String, Presence)> },
    /// Someone is typing in a conversation, or in one of its threads.
    Typing {
        channel: String,
        thread: Option<Ts>,
        user: String,
    },
}

/// Someone typing, until a moment.
#[derive(Clone, Debug, PartialEq)]
pub struct Typing {
    pub user: String,
    pub channel: String,
    pub thread: Option<Ts>,
    pub until: Instant,
}

/// What one workspace knows about its people beyond their profiles.
#[derive(Clone, Debug, Default)]
pub struct TeamPeople {
    /// Who is active or away, for the people watched.
    pub presence: HashMap<String, Presence>,
    /// Who is typing where; old entries are dropped as new ones come.
    pub typing: Vec<Typing>,
}

impl TeamPeople {
    /// Someone's presence, when known.
    pub fn presence(&self, user: &str) -> Option<Presence> {
        self.presence.get(user).copied()
    }

    /// Notes that `user` is typing, for [`TYPING_FOR`] from `now`.
    pub fn typing(&mut self, user: String, channel: String, thread: Option<Ts>, now: Instant) {
        self.typing.retain(|t| {
            t.until > now && !(t.user == user && t.channel == channel && t.thread == thread)
        });
        self.typing.push(Typing {
            user,
            channel,
            thread,
            until: now + TYPING_FOR,
        });
    }

    /// `user` sent a message in `channel`: they are done typing there.
    pub fn stopped_typing(&mut self, channel: &str, user: &str) {
        self.typing
            .retain(|t| !(t.channel == channel && t.user == user));
    }

    /// Who is typing in `channel` (outside threads with `None`, in one
    /// thread with its parent) at `now`, in the order they started, and
    /// when the first of them stops showing.
    pub fn typing_in(
        &self,
        channel: &str,
        thread: Option<&Ts>,
        now: Instant,
    ) -> (Vec<&str>, Option<Instant>) {
        let here: Vec<&Typing> = self
            .typing
            .iter()
            .filter(|t| t.channel == channel && t.thread.as_ref() == thread && t.until > now)
            .collect();
        let until = here.iter().map(|t| t.until).min();
        (here.into_iter().map(|t| t.user.as_str()).collect(), until)
    }
}

/// The line under the composer: who is typing, by name.
pub fn typing_line(names: &[String]) -> Option<String> {
    match names {
        [] => None,
        [one] => Some(tf("{name} is typing…", &[("name", one)])),
        [one, two] => Some(tf(
            "{first} and {second} are typing…",
            &[("first", one), ("second", two)],
        )),
        _ => Some(t("Several people are typing…").into_owned()),
    }
}

/// The app's side of watching people.
#[derive(Debug, Default)]
pub struct State {
    /// The people last asked to be watched, by workspace.
    watched: HashMap<String, Vec<String>>,
    /// When the people on screen were last worked out.
    checked: Option<Instant>,
    /// When you were last said to be typing, by workspace, conversation
    /// and thread.
    typed: HashMap<(String, String, Option<Ts>), Instant>,
}

impl State {
    /// Whether a "typing" notice for this place may go out at `now`, and
    /// if so notes that it did. Slack only needs one every few seconds.
    fn may_say_typing(&mut self, place: (String, String, Option<Ts>), now: Instant) -> bool {
        self.typed
            .retain(|_, at| now.duration_since(*at) < TYPING_EVERY);
        if self.typed.contains_key(&place) {
            return false;
        }
        self.typed.insert(place, now);
        true
    }
}

/// The people whose presence a workspace shows: the newest direct
/// messages, you, and whoever else is on screen. Sorted, so two answers
/// compare equal when they hold the same people.
pub fn wanted(workspace: &WorkspaceState, others: &[&str]) -> Vec<String> {
    // A deactivated account is never around.
    let present = |id: &&str| !id.is_empty() && workspace.user(id).is_none_or(|u| !u.deleted);
    let mut dms: Vec<_> = workspace
        .conversations
        .iter()
        .filter(|c| c.kind == ConversationKind::Direct && !c.archived)
        .filter_map(|c| Some((c.latest.as_ref(), c.user.as_deref()?)))
        .filter(|(_, user)| present(user))
        .collect();
    dms.sort_by(|a, b| b.0.cmp(&a.0));
    let mut users: Vec<String> = dms
        .into_iter()
        .take(WATCHED_DMS)
        .map(|(_, user)| user)
        .chain(std::iter::once(workspace.info.user_id.as_str()))
        .chain(others.iter().copied())
        .filter(present)
        .map(str::to_owned)
        .collect();
    users.sort();
    users.dedup();
    users
}

/// Tells the worker who is on screen in each workspace, when that changed.
/// Only the workspace you look at is watched; the others stop.
pub fn frame(app: &mut App, now: Instant) {
    if app
        .people
        .checked
        .is_some_and(|at| now.duration_since(at) < WATCH_EVERY)
    {
        return;
    }
    app.people.checked = Some(now);
    let active = app.active_team();
    let mut others: Vec<String> = Vec::new();
    if let Some(user) = &app.profile {
        others.push(user.clone());
    }
    if let (Some(team), Some(details)) = (&active, &app.convos.details)
        && let Some(data) = app.convos.data(team, &details.channel)
        && let crate::convos::Loaded::Ready(members) = &data.members
    {
        others.extend(members.iter().take(WATCHED_MEMBERS).cloned());
    }
    let others: Vec<&str> = others.iter().map(String::as_str).collect();
    let mut changes = Vec::new();
    for workspace in &app.workspaces {
        let team = &workspace.info.team_id;
        let users = if active.as_deref() == Some(team.as_str()) && workspace.signed_out.is_none() {
            wanted(workspace, &others)
        } else {
            Vec::new()
        };
        let before = app.people.watched.get(team);
        if before.map_or(!users.is_empty(), |before| *before != users) {
            changes.push((team.clone(), users));
        }
    }
    for (team, users) in changes {
        app.people.watched.insert(team.clone(), users.clone());
        app.backend.send(backend::Command::People {
            team,
            command: Command::Watch { users },
        });
    }
}

/// Applies a view's request.
pub fn apply(app: &mut App, action: Action) {
    let Some(team) = app.active_team() else {
        return;
    };
    match action {
        Action::Typing { channel, thread } => {
            let place = (team.clone(), channel.clone(), thread.clone());
            if app.people.may_say_typing(place, Instant::now()) {
                app.backend.send(backend::Command::People {
                    team,
                    command: Command::Typing { channel, thread },
                });
            }
        }
    }
}

/// Whether a change to a draft says you are typing: there is text, and it
/// is not a slash command, which only you see.
pub fn is_typing(before: &str, after: &str) -> bool {
    before != after && !after.trim().is_empty() && !after.trim_start().starts_with('/')
}

/// Takes in one of the worker's answers for `team`.
pub fn handle(app: &mut App, team: &str, event: Event) {
    let Some(workspace) = app.workspaces.iter_mut().find(|w| w.info.team_id == team) else {
        return;
    };
    match event {
        Event::Presence { users } => {
            workspace.people.presence.extend(users);
        }
        Event::Typing {
            channel,
            thread,
            user,
        } => {
            if user != workspace.info.user_id {
                workspace
                    .people
                    .typing(user, channel, thread, Instant::now());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Conversation, Ts, User, Workspace};

    fn dm(id: &str, user: &str, latest: &str) -> Conversation {
        Conversation {
            id: id.into(),
            name: user.into(),
            kind: ConversationKind::Direct,
            user: Some(user.into()),
            topic: String::new(),
            purpose: String::new(),
            members: None,
            archived: false,
            last_read: None,
            latest: Some(Ts::new(latest)),
            unread: 0,
            mentions: 0,
        }
    }

    fn workspace() -> WorkspaceState {
        let mut workspace = WorkspaceState::new(Workspace {
            team_id: "T1".into(),
            name: "Acme".into(),
            domain: "acme".into(),
            icon: None,
            user_id: "U0".into(),
        });
        workspace.users.insert(
            "U9".into(),
            User {
                id: "U9".into(),
                deleted: true,
                ..User::default()
            },
        );
        workspace
    }

    #[test]
    fn the_newest_direct_messages_you_and_the_screen_are_watched() {
        let mut workspace = workspace();
        for n in 0..(WATCHED_DMS + 5) {
            workspace.conversations.push(dm(
                &format!("D{n}"),
                &format!("U{n:03}"),
                &format!("{n}.0"),
            ));
        }
        workspace.conversations.push(dm("DX", "U9", "9999.0"));
        let users = wanted(&workspace, &["U500", "U0"]);
        assert!(users.contains(&"U0".to_owned()), "you");
        assert!(users.contains(&"U500".to_owned()), "the profile card");
        assert!(!users.contains(&"U9".to_owned()), "a deactivated account");
        assert!(!users.contains(&"U000".to_owned()), "an old DM");
        assert!(users.contains(&format!("U{:03}", WATCHED_DMS + 4)));
        assert_eq!(users.len(), WATCHED_DMS + 2);
        let mut sorted = users.clone();
        sorted.sort();
        assert_eq!(users, sorted);
    }

    #[test]
    fn typing_shows_for_a_while_and_ends_with_the_message() {
        let start = Instant::now();
        let mut people = TeamPeople::default();
        people.typing("U1".into(), "C1".into(), None, start);
        people.typing("U2".into(), "C1".into(), Some(Ts::new("1.0")), start);
        let soon = start + Duration::from_secs(1);
        people.typing("U3".into(), "C1".into(), None, soon);
        let (here, until) = people.typing_in("C1", None, soon);
        assert_eq!(here, ["U1", "U3"]);
        assert_eq!(until, Some(start + TYPING_FOR));
        assert_eq!(
            people.typing_in("C1", Some(&Ts::new("1.0")), soon).0,
            ["U2"]
        );
        assert!(people.typing_in("C2", None, soon).0.is_empty());
        // Typing again keeps one entry, showing for longer.
        people.typing("U1".into(), "C1".into(), None, soon);
        assert_eq!(people.typing_in("C1", None, soon).0, ["U3", "U1"]);
        let later = start + TYPING_FOR + Duration::from_millis(500);
        assert_eq!(people.typing_in("C1", None, later).0, ["U3", "U1"]);
        people.stopped_typing("C1", "U3");
        assert_eq!(people.typing_in("C1", None, later).0, ["U1"]);
        let much_later = later + TYPING_FOR;
        assert!(people.typing_in("C1", None, much_later).0.is_empty());
    }

    #[test]
    fn your_typing_goes_out_now_and_then() {
        let start = Instant::now();
        let mut state = State::default();
        let here = || ("T1".to_owned(), "C1".to_owned(), None);
        assert!(state.may_say_typing(here(), start));
        assert!(!state.may_say_typing(here(), start + Duration::from_secs(1)));
        let thread = ("T1".to_owned(), "C1".to_owned(), Some(Ts::new("1.0")));
        assert!(state.may_say_typing(thread, start + Duration::from_secs(1)));
        assert!(state.may_say_typing(here(), start + TYPING_EVERY));
        assert!(is_typing("", "h"));
        assert!(!is_typing("h", "h"));
        assert!(!is_typing("h", " "));
        assert!(!is_typing("", "/stat"));
    }

    #[test]
    fn the_typing_line_names_one_or_two() {
        let names = |list: &[&str]| list.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>();
        assert_eq!(typing_line(&[]), None);
        assert_eq!(
            typing_line(&names(&["Ana"])).as_deref(),
            Some("Ana is typing…")
        );
        assert_eq!(
            typing_line(&names(&["Ana", "Bob"])).as_deref(),
            Some("Ana and Bob are typing…")
        );
        assert_eq!(
            typing_line(&names(&["Ana", "Bob", "Carla"])).as_deref(),
            Some("Several people are typing…")
        );
    }

    #[test]
    fn slacks_words_for_presence() {
        assert_eq!(Presence::parse("active"), Some(Presence::Active));
        assert_eq!(Presence::parse("away"), Some(Presence::Away));
        assert_eq!(Presence::parse("lurking"), None);
    }
}
