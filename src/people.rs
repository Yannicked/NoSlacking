//! People around you: who is active or away.
//!
//! The app works out which people are on screen (your direct messages,
//! the profile card, a channel's member list) and asks the worker to
//! watch them with [`Command::Watch`]. The worker answers with
//! [`Event`]s, which [`handle`] keeps in each workspace's [`TeamPeople`].
//! Views only read that.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use crate::app::{App, WorkspaceState};
use crate::backend;
use crate::model::ConversationKind;

/// How many direct messages, newest first, have their people watched. The
/// sidebar shows about this many before "Show more".
const WATCHED_DMS: usize = 40;
/// How many people of an open member list are watched.
const WATCHED_MEMBERS: usize = 100;
/// How often the app works out who is on screen. People come and go from
/// the screen slowly; once a second is plenty.
const WATCH_EVERY: Duration = Duration::from_secs(1);

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

/// What the interface asks the worker to do for one workspace.
#[derive(Clone, Debug, PartialEq)]
pub enum Command {
    /// Keep these people's presence up to date, and stop for anyone else.
    /// Sent whenever the set on screen changes; empty stops watching.
    Watch { users: Vec<String> },
}

/// What the worker answers.
#[derive(Clone, Debug, PartialEq)]
pub enum Event {
    /// These people are now active or away.
    Presence { users: Vec<(String, Presence)> },
}

/// What one workspace knows about its people beyond their profiles.
#[derive(Clone, Debug, Default)]
pub struct TeamPeople {
    /// Who is active or away, for the people watched.
    pub presence: HashMap<String, Presence>,
}

impl TeamPeople {
    /// Someone's presence, when known.
    pub fn presence(&self, user: &str) -> Option<Presence> {
        self.presence.get(user).copied()
    }
}

/// The app's side of watching people.
#[derive(Debug, Default)]
pub struct State {
    /// The people last asked to be watched, by workspace.
    watched: HashMap<String, Vec<String>>,
    /// When the people on screen were last worked out.
    checked: Option<Instant>,
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

/// Takes in one of the worker's answers for `team`.
pub fn handle(app: &mut App, team: &str, event: Event) {
    let Some(workspace) = app.workspaces.iter_mut().find(|w| w.info.team_id == team) else {
        return;
    };
    match event {
        Event::Presence { users } => {
            workspace.people.presence.extend(users);
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
    fn slacks_words_for_presence() {
        assert_eq!(Presence::parse("active"), Some(Presence::Active));
        assert_eq!(Presence::parse("away"), Some(Presence::Away));
        assert_eq!(Presence::parse("lurking"), None);
    }
}
