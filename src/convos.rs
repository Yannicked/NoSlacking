//! Starting and finding conversations: a new direct message with one or
//! more people.
//!
//! Views push [`Action`]s (wrapped in [`crate::model::Action::Convos`]);
//! [`apply`] turns them into [`Command`]s for the worker, whose answers come
//! back as [`Event`]s for [`handle`]. What the dialogs hold lives in
//! [`State`], kept on [`App`].

use crate::app::{App, WorkspaceState};
use crate::backend;
use crate::i18n::tf;
use crate::model::{ConversationKind, User};

/// The most people a group message can have besides you: Slack's limit.
pub const MAX_PEOPLE: usize = 8;
/// How many people the "New message" dialog suggests at once.
const SUGGESTIONS: usize = 8;

/// What the conversation views ask for.
#[derive(Clone, Debug, PartialEq)]
pub enum Action {
    /// Shows the "New message" dialog.
    NewMessage,
    /// Opens (or starts) the direct message with these people.
    Open { users: Vec<String> },
}

/// What the interface asks the worker to do for one workspace.
#[derive(Clone, Debug, PartialEq)]
pub enum Command {
    /// `conversations.open` with these people, then opens the result.
    Open { users: Vec<String> },
}

impl Command {
    /// What failed, should this command fail.
    pub fn failure(&self) -> Failure {
        match self {
            Self::Open { .. } => Failure::Open,
        }
    }
}

/// What the worker answers.
#[derive(Clone, Debug, PartialEq)]
pub enum Event {
    /// A conversation you started or joined: its details arrived first as
    /// [`backend::Event::Conversation`], so it can be opened now.
    Opened { channel: String },
    /// Slack refused, or could not be reached.
    Failed { what: Failure, error: String },
}

/// Which request failed, so the interface can say so in its own words.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Failure {
    Open,
}

/// The "New message" dialog.
#[derive(Clone, Debug, Default)]
pub struct NewMessage {
    /// What is typed to find someone.
    pub query: String,
    /// The people picked so far, in order.
    pub picked: Vec<String>,
    /// The highlighted suggestion.
    pub selected: usize,
    /// Waiting for Slack to open the conversation.
    pub busy: bool,
    /// The last query's matches, with the people's version they were found
    /// in: a workspace can have tens of thousands of people, too many to
    /// search on every frame.
    found: Option<(String, (usize, u64), Vec<String>)>,
}

impl NewMessage {
    /// The suggestions for the current query, worked out again only when
    /// the query, the picks or the people change.
    pub fn suggestions(&mut self, workspace: &WorkspaceState) -> Vec<String> {
        let version = (workspace.users.len(), workspace.users_version());
        let key = format!("{}\u{0}{}", self.query, self.picked.join(","));
        if let Some((query, seen, found)) = &self.found
            && *query == key
            && *seen == version
        {
            return found.clone();
        }
        let found: Vec<String> = people_matching(
            workspace.users.values(),
            &self.query,
            &self.picked,
            &workspace.info.user_id,
        )
        .into_iter()
        .take(SUGGESTIONS)
        .map(|u| u.id.clone())
        .collect();
        self.found = Some((key, version, found.clone()));
        found
    }

    /// Adds someone, unless they are already there or the group is full.
    pub fn pick(&mut self, user: String) {
        if !self.picked.contains(&user) && self.picked.len() < MAX_PEOPLE {
            self.picked.push(user);
        }
        self.query.clear();
        self.selected = 0;
    }
}

/// Everything the conversation dialogs hold.
#[derive(Clone, Debug, Default)]
pub struct State {
    pub new_message: Option<NewMessage>,
}

impl State {
    /// Whether one of these dialogs covers the window, so its keys come
    /// first.
    pub fn overlay_open(&self) -> bool {
        self.new_message.is_some()
    }
}

/// The people whose names match `query`, best first: whoever starts with
/// it before whoever only contains it, people before apps, then by name.
/// Deactivated accounts, the people already `picked` and, while a group is
/// being put together, you, are left out.
pub fn people_matching<'a>(
    users: impl Iterator<Item = &'a User>,
    query: &str,
    picked: &[String],
    me: &str,
) -> Vec<&'a User> {
    let query = query.trim().trim_start_matches('@').to_lowercase();
    let mut found: Vec<(bool, bool, String, &User)> = users
        .filter(|u| !u.deleted && !picked.contains(&u.id))
        // A DM with yourself is your notes; it makes no sense in a group.
        .filter(|u| picked.is_empty() || u.id != me)
        .filter_map(|u| {
            let names = [
                u.display_name.to_lowercase(),
                u.real_name.to_lowercase(),
                u.name.to_lowercase(),
            ];
            let starts = names.iter().any(|n| {
                n.starts_with(&query) || n.split_whitespace().any(|w| w.starts_with(&query))
            });
            let contains = starts || names.iter().any(|n| n.contains(&query));
            contains.then(|| (!starts, u.is_bot, u.label().to_lowercase(), u))
        })
        .collect();
    found.sort_by(|a, b| (a.0, a.1, &a.2).cmp(&(b.0, b.1, &b.2)));
    found.into_iter().map(|(.., u)| u).collect()
}

/// The direct message you already have with exactly this one person.
pub fn existing_dm(workspace: &WorkspaceState, users: &[String]) -> Option<String> {
    let [user] = users else {
        return None;
    };
    workspace
        .conversations
        .iter()
        .find(|c| c.kind == ConversationKind::Direct && c.user.as_deref() == Some(user.as_str()))
        .map(|c| c.id.clone())
}

/// Carries out a view's request.
pub fn apply(app: &mut App, action: Action) {
    match action {
        Action::NewMessage => {
            app.focus_overlay = true;
            app.convos.new_message = Some(NewMessage::default());
        }
        Action::Open { users } => open(app, users),
    }
}

/// Opens the DM with `users`: at once when it is already in the sidebar,
/// else once Slack has opened it.
fn open(app: &mut App, users: Vec<String>) {
    if users.is_empty() {
        return;
    }
    let Some(workspace) = app.active_workspace() else {
        return;
    };
    let team = workspace.info.team_id.clone();
    if let Some(channel) = existing_dm(workspace, &users) {
        app.convos.new_message = None;
        app.open_conversation(&channel);
        return;
    }
    if let Some(dialog) = &mut app.convos.new_message {
        dialog.busy = true;
    }
    send(app, team, Command::Open { users });
}

/// Sends a command for `team` to the worker.
fn send(app: &App, team: String, command: Command) {
    app.backend.send(backend::Command::Convos { team, command });
}

/// Takes in one of the worker's answers for `team`.
pub fn handle(app: &mut App, team: &str, event: Event) {
    let active = app.active_team().as_deref() == Some(team);
    match event {
        Event::Opened { channel } => {
            app.convos.new_message = None;
            if active {
                app.open_conversation(&channel);
            }
        }
        Event::Failed { what, error } => {
            if let Some(dialog) = &mut app.convos.new_message {
                dialog.busy = false;
            }
            let text = match what {
                Failure::Open => tf(
                    "Could not open the conversation: {error}",
                    &[("error", &error)],
                ),
            };
            app.toast(text, true);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Conversation, Workspace};

    fn person(id: &str, handle: &str, real: &str) -> User {
        User {
            id: id.into(),
            name: handle.into(),
            real_name: real.into(),
            ..User::default()
        }
    }

    fn people() -> Vec<User> {
        vec![
            person("U0", "me", "Me Myself"),
            person("U1", "ana", "Ana Lima"),
            person("U2", "bob", "Bob Martens"),
            person("U3", "dana", "Dana Anders"),
            User {
                is_bot: true,
                ..person("U4", "anabot", "Ana Bot")
            },
            User {
                deleted: true,
                ..person("U5", "anakin", "Anakin")
            },
        ]
    }

    fn ids(found: &[&User]) -> Vec<String> {
        found.iter().map(|u| u.id.clone()).collect()
    }

    #[test]
    fn people_who_start_with_the_query_come_first() {
        let all = people();
        let found = people_matching(all.iter(), "an", &[], "U0");
        // Ana starts with it, Dana Anders has a word that does, the bot
        // comes after the people, and Anakin is deactivated.
        assert_eq!(ids(&found), ["U1", "U3", "U4"]);
        let found = people_matching(all.iter(), "@MART", &[], "U0");
        assert_eq!(ids(&found), ["U2"]);
        assert_eq!(people_matching(all.iter(), "", &[], "U0").len(), 5);
    }

    #[test]
    fn picked_people_and_you_leave_the_suggestions_of_a_group() {
        let all = people();
        let alone = people_matching(all.iter(), "me", &[], "U0");
        assert_eq!(ids(&alone), ["U0"], "a DM with yourself is allowed");
        let group = people_matching(all.iter(), "", &["U1".into()], "U0");
        assert!(!ids(&group).contains(&"U0".to_owned()));
        assert!(!ids(&group).contains(&"U1".to_owned()));
    }

    #[test]
    fn a_group_stops_at_slacks_limit() {
        let mut dialog = NewMessage {
            query: "a".into(),
            ..NewMessage::default()
        };
        for n in 0..12 {
            dialog.pick(format!("U{n}"));
        }
        assert_eq!(dialog.picked.len(), MAX_PEOPLE);
        dialog.pick("U1".into());
        assert_eq!(dialog.picked.len(), MAX_PEOPLE);
        assert!(dialog.query.is_empty());
    }

    #[test]
    fn suggestions_follow_the_query_and_new_people() {
        let mut workspace = WorkspaceState::new(Workspace {
            team_id: "T1".into(),
            name: "Acme".into(),
            domain: "acme".into(),
            icon: None,
            user_id: "U0".into(),
        });
        for user in people() {
            workspace.users.insert(user.id.clone(), user);
        }
        let mut dialog = NewMessage {
            query: "bo".into(),
            ..NewMessage::default()
        };
        assert_eq!(dialog.suggestions(&workspace), ["U2", "U4"]);
        workspace
            .users
            .insert("U6".into(), person("U6", "bodil", "Bodil"));
        assert_eq!(dialog.suggestions(&workspace), ["U2", "U6", "U4"]);
        dialog.pick("U2".into());
        dialog.query = "bo".into();
        assert_eq!(dialog.suggestions(&workspace), ["U6", "U4"]);
    }

    #[test]
    fn one_person_reuses_the_dm_you_have() {
        let mut workspace = WorkspaceState::new(Workspace {
            team_id: "T1".into(),
            name: "Acme".into(),
            domain: "acme".into(),
            icon: None,
            user_id: "U0".into(),
        });
        workspace.conversations.push(Conversation {
            id: "D1".into(),
            name: "U1".into(),
            kind: ConversationKind::Direct,
            user: Some("U1".into()),
            topic: String::new(),
            purpose: String::new(),
            members: None,
            archived: false,
            last_read: None,
            latest: None,
            unread: 0,
            mentions: 0,
        });
        assert_eq!(
            existing_dm(&workspace, &["U1".into()]).as_deref(),
            Some("D1")
        );
        assert_eq!(existing_dm(&workspace, &["U2".into()]), None);
        assert_eq!(existing_dm(&workspace, &["U1".into(), "U2".into()]), None);
    }
}
