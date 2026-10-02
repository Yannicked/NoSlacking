//! Starting and finding conversations: a new direct message with one or
//! more people, the channel browser, joining, leaving and creating
//! channels.
//!
//! Views push [`Action`]s (wrapped in [`crate::model::Action::Convos`]);
//! [`apply`] turns them into [`Command`]s for the worker, whose answers come
//! back as [`Event`]s for [`handle`]. What the dialogs hold lives in
//! [`State`], kept on [`App`].

use std::collections::HashSet;

use crate::app::{App, WorkspaceState};
use crate::backend;
use crate::i18n::tf;
use crate::model::{ConversationKind, User};

/// The most people a group message can have besides you: Slack's limit.
pub const MAX_PEOPLE: usize = 8;
/// How many people the "New message" dialog suggests at once.
const SUGGESTIONS: usize = 8;
/// The longest channel name Slack takes, in characters.
pub const MAX_NAME: usize = 80;

/// What the conversation views ask for.
#[derive(Clone, Debug, PartialEq)]
pub enum Action {
    /// Shows the "New message" dialog.
    NewMessage,
    /// Opens (or starts) the direct message with these people.
    Open { users: Vec<String> },
    /// Shows the channel browser and lists the public channels.
    Browse,
    /// Joins a public channel and opens it.
    Join { channel: String },
    /// Asks whether to leave a channel.
    AskLeave { channel: String },
    /// Leaves a channel.
    Leave { channel: String },
    /// Shows the "Create a channel" dialog.
    NewChannel,
    /// Creates a channel with this (already checked) name and opens it.
    Create { name: String, private: bool },
}

/// What the interface asks the worker to do for one workspace.
#[derive(Clone, Debug, PartialEq)]
pub enum Command {
    /// `conversations.open` with these people, then opens the result.
    Open { users: Vec<String> },
    /// Lists the public channels you are not in.
    Browse,
    /// `conversations.join`, then opens the channel.
    Join { channel: String },
    /// `conversations.leave`; the channel then leaves the sidebar.
    Leave { channel: String },
    /// `conversations.create`, then opens the new channel.
    Create { name: String, private: bool },
}

impl Command {
    /// What failed, should this command fail.
    pub fn failure(&self) -> Failure {
        match self {
            Self::Open { .. } => Failure::Open,
            Self::Browse => Failure::Browse,
            Self::Join { .. } => Failure::Join,
            Self::Leave { .. } => Failure::Leave,
            Self::Create { .. } => Failure::Create,
        }
    }
}

/// What the worker answers.
#[derive(Clone, Debug, PartialEq)]
pub enum Event {
    /// A conversation you started or joined: its details arrived first as
    /// [`backend::Event::Conversation`], so it can be opened now.
    Opened { channel: String },
    /// A page of public channels you could join; `done` on the last.
    Browsed { channels: Vec<Listed>, done: bool },
    /// Slack refused, or could not be reached.
    Failed { what: Failure, error: String },
}

/// Which request failed, so the interface can say so in its own words.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Failure {
    Open,
    Browse,
    Join,
    Leave,
    Create,
}

/// A public channel the browser lists.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Listed {
    pub id: String,
    pub name: String,
    pub topic: String,
    pub purpose: String,
    pub members: u32,
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

/// The channel browser.
#[derive(Clone, Debug, Default)]
pub struct Browse {
    /// The workspace listed: answers for another one are dropped.
    pub team: String,
    pub query: String,
    /// What has arrived so far.
    pub channels: Vec<Listed>,
    /// Whether the whole list is in.
    pub done: bool,
    /// Why the list stopped, if it did.
    pub error: Option<String>,
    /// Channels being joined, waiting for Slack.
    pub joining: HashSet<String>,
    /// The last query's matches, by index into `channels`, with the query
    /// and how many channels there were: thousands of channels are too
    /// many to filter on every frame.
    found: Option<(String, usize, Vec<usize>)>,
}

impl Browse {
    /// The channels matching the query, best first.
    pub fn matches(&mut self) -> Vec<usize> {
        if let Some((query, count, found)) = &self.found
            && *query == self.query
            && *count == self.channels.len()
        {
            return found.clone();
        }
        let found = channels_matching(&self.channels, &self.query);
        self.found = Some((self.query.clone(), self.channels.len(), found.clone()));
        found
    }
}

/// The "Create a channel" dialog.
#[derive(Clone, Debug, Default)]
pub struct NewChannel {
    pub name: String,
    pub private: bool,
    /// Waiting for Slack.
    pub busy: bool,
}

/// Everything the conversation dialogs hold.
#[derive(Clone, Debug, Default)]
pub struct State {
    pub new_message: Option<NewMessage>,
    pub browse: Option<Browse>,
    pub new_channel: Option<NewChannel>,
    /// A channel waiting for "Leave?" to be answered.
    pub leave: Option<String>,
}

impl State {
    /// Whether one of these dialogs covers the window, so its keys come
    /// first.
    pub fn overlay_open(&self) -> bool {
        self.new_message.is_some()
            || self.browse.is_some()
            || self.new_channel.is_some()
            || self.leave.is_some()
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

/// The channels whose name, topic or purpose holds `query`, as indexes
/// into `channels`: names starting with it first, then other name matches,
/// then the rest; the busiest first within each.
pub fn channels_matching(channels: &[Listed], query: &str) -> Vec<usize> {
    let query = query.trim().trim_start_matches('#').to_lowercase();
    let mut found: Vec<(u8, std::cmp::Reverse<u32>, usize)> = channels
        .iter()
        .enumerate()
        .filter_map(|(index, c)| {
            let name = c.name.to_lowercase();
            let rank = if name.starts_with(&query) {
                0
            } else if name.contains(&query) {
                1
            } else if c.topic.to_lowercase().contains(&query)
                || c.purpose.to_lowercase().contains(&query)
            {
                2
            } else {
                return None;
            };
            Some((rank, std::cmp::Reverse(c.members), index))
        })
        .collect();
    found.sort_by(|a, b| {
        (a.0, a.1)
            .cmp(&(b.0, b.1))
            .then_with(|| channels[a.2].name.cmp(&channels[b.2].name))
    });
    found.into_iter().map(|(.., index)| index).collect()
}

/// Why a channel name cannot be used.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NameProblem {
    Empty,
    TooLong,
    /// Only letters, numbers, hyphens and underscores are allowed.
    Character(char),
    /// You already have a conversation by that name.
    Taken,
}

/// The name Slack will get for what was typed, as Slack's own dialog
/// makes it: lower case, with spaces as hyphens. Anything else that is not
/// a letter, a number, a hyphen or an underscore is refused, as is a name
/// longer than [`MAX_NAME`].
pub fn channel_name(typed: &str) -> Result<String, NameProblem> {
    let name: String = typed
        .trim()
        .trim_start_matches('#')
        .chars()
        .map(|c| if c.is_whitespace() { '-' } else { c })
        .flat_map(char::to_lowercase)
        .collect();
    if name.is_empty() {
        return Err(NameProblem::Empty);
    }
    if let Some(bad) = name
        .chars()
        .find(|c| !(c.is_alphanumeric() || *c == '-' || *c == '_'))
    {
        return Err(NameProblem::Character(bad));
    }
    if name.chars().count() > MAX_NAME {
        return Err(NameProblem::TooLong);
    }
    Ok(name)
}

/// [`channel_name`], also refusing a name one of your channels already has.
pub fn new_channel_name(workspace: &WorkspaceState, typed: &str) -> Result<String, NameProblem> {
    let name = channel_name(typed)?;
    let taken = workspace
        .conversations
        .iter()
        .any(|c| !c.kind.is_dm() && c.name == name);
    if taken {
        return Err(NameProblem::Taken);
    }
    Ok(name)
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
    let Some(team) = app.active_team() else {
        return;
    };
    match action {
        Action::NewMessage => {
            app.focus_overlay = true;
            app.convos.new_message = Some(NewMessage::default());
        }
        Action::Open { users } => open(app, users),
        Action::Browse => {
            app.focus_overlay = true;
            app.convos.browse = Some(Browse {
                team: team.clone(),
                ..Browse::default()
            });
            send(app, team, Command::Browse);
        }
        Action::Join { channel } => {
            // Already in it: nothing to ask Slack.
            if app
                .active_workspace()
                .is_some_and(|w| w.conversation(&channel).is_some())
            {
                app.convos.browse = None;
                app.open_conversation(&channel);
                return;
            }
            if let Some(browse) = &mut app.convos.browse {
                browse.joining.insert(channel.clone());
            }
            send(app, team, Command::Join { channel });
        }
        Action::AskLeave { channel } => app.convos.leave = Some(channel),
        Action::Leave { channel } => {
            app.convos.leave = None;
            send(app, team, Command::Leave { channel });
        }
        Action::NewChannel => {
            app.focus_overlay = true;
            app.convos.browse = None;
            app.convos.new_channel = Some(NewChannel::default());
        }
        Action::Create { name, private } => {
            if let Some(dialog) = &mut app.convos.new_channel {
                dialog.busy = true;
            }
            send(app, team, Command::Create { name, private });
        }
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
            app.convos.new_channel = None;
            app.convos.browse = None;
            if active {
                app.open_conversation(&channel);
            }
        }
        Event::Browsed { channels, done } => {
            if let Some(browse) = app.convos.browse.as_mut().filter(|b| b.team == team) {
                browse.channels.extend(channels);
                browse.done = done;
            }
        }
        Event::Failed { what, error } => {
            let text = match what {
                Failure::Open => {
                    if let Some(dialog) = &mut app.convos.new_message {
                        dialog.busy = false;
                    }
                    tf(
                        "Could not open the conversation: {error}",
                        &[("error", &error)],
                    )
                }
                Failure::Browse => {
                    if let Some(browse) = &mut app.convos.browse {
                        browse.error = Some(error.clone());
                        browse.done = true;
                    }
                    tf("Could not list the channels: {error}", &[("error", &error)])
                }
                Failure::Join => {
                    if let Some(browse) = &mut app.convos.browse {
                        browse.joining.clear();
                    }
                    tf("Could not join the channel: {error}", &[("error", &error)])
                }
                Failure::Leave => tf("Could not leave the channel: {error}", &[("error", &error)]),
                Failure::Create => {
                    if let Some(dialog) = &mut app.convos.new_channel {
                        dialog.busy = false;
                    }
                    tf(
                        "Could not create the channel: {error}",
                        &[("error", &error)],
                    )
                }
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

    #[test]
    fn channel_names_follow_slacks_rules() {
        assert_eq!(
            channel_name("  Release Notes ").as_deref(),
            Ok("release-notes")
        );
        assert_eq!(channel_name("#team_ops-2").as_deref(), Ok("team_ops-2"));
        assert_eq!(channel_name("Ünïcode").as_deref(), Ok("ünïcode"));
        assert_eq!(channel_name("   "), Err(NameProblem::Empty));
        assert_eq!(channel_name("v1.2"), Err(NameProblem::Character('.')));
        assert_eq!(channel_name("a!"), Err(NameProblem::Character('!')));
        assert!(channel_name(&"x".repeat(MAX_NAME)).is_ok());
        assert_eq!(
            channel_name(&"x".repeat(MAX_NAME + 1)),
            Err(NameProblem::TooLong)
        );
    }

    #[test]
    fn a_name_you_already_have_is_taken() {
        let mut workspace = WorkspaceState::new(Workspace {
            team_id: "T1".into(),
            name: "Acme".into(),
            domain: "acme".into(),
            icon: None,
            user_id: "U0".into(),
        });
        workspace.conversations.push(Conversation {
            id: "C1".into(),
            name: "general".into(),
            kind: ConversationKind::Channel,
            user: None,
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
            new_channel_name(&workspace, "General"),
            Err(NameProblem::Taken)
        );
        assert_eq!(new_channel_name(&workspace, "gen").as_deref(), Ok("gen"));
    }

    #[test]
    fn channels_starting_with_the_query_come_first() {
        let listed = |name: &str, topic: &str, members: u32| Listed {
            id: name.into(),
            name: name.into(),
            topic: topic.into(),
            members,
            ..Listed::default()
        };
        let channels = [
            listed("team-design", "", 40),
            listed("design", "", 5),
            listed("random", "design reviews", 90),
            listed("design-system", "", 30),
            listed("ops", "", 3),
        ];
        let found: Vec<&str> = channels_matching(&channels, "#Design")
            .into_iter()
            .map(|i| channels[i].name.as_str())
            .collect();
        assert_eq!(found, ["design-system", "design", "team-design", "random"]);
        assert_eq!(channels_matching(&channels, "").len(), 5);
        assert_eq!(channels_matching(&channels, "")[0], 2, "busiest first");
    }
}
