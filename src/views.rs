//! The views at the top of the sidebar that take the place of the open
//! conversation, as in Slack: Activity (what mentions you or answers you)
//! All unreads (every conversation with something new, with the new
//! messages) and Threads (the threads you follow, with their newest
//! replies).
//!
//! Views push [`Action`]s (wrapped in [`crate::model::Action::Views`]);
//! [`apply`] turns them into [`Command`]s for the worker, whose answers come
//! back as [`Event`]s for [`handle`]. What the views hold lives in
//! [`State`], kept on [`App`].

use std::collections::{HashMap, HashSet};

use crate::app::{App, WorkspaceState};
use crate::backend;
use crate::i18n::{t, tf};
use crate::model::{Delivery, Message, Ts};

/// The most live mentions kept per workspace: enough for a day of chatter,
/// few enough that the list stays quick.
const LIVE_LIMIT: usize = 200;
/// The most unread conversations whose messages are loaded at once: the
/// rest load as they are scrolled to.
const UNREAD_LIMIT: usize = 30;
/// How many of a thread's newest replies the threads list shows.
pub const THREAD_REPLIES: usize = 3;

/// One of the views at the top of the sidebar.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum View {
    /// Mentions of you, of everyone, and replies to your threads.
    Activity,
    /// Every conversation with unread messages, and those messages.
    Unreads,
    /// Threads you follow, with their newest replies.
    Threads,
}

impl View {
    /// Every view, in the sidebar's order.
    pub const ALL: [Self; 3] = [Self::Unreads, Self::Threads, Self::Activity];

    /// Its name in the sidebar and its header.
    pub fn label(self) -> String {
        match self {
            Self::Activity => t("Activity").into_owned(),
            Self::Unreads => t("All unreads").into_owned(),
            Self::Threads => t("Threads").into_owned(),
        }
    }
}

/// What the views ask for.
#[derive(Clone, Debug, PartialEq)]
pub enum Action {
    /// Shows a view in place of the conversation and loads what it lists.
    Open(View),
    /// Goes back to the conversation.
    Close,
    /// Loads the open view's list again.
    Refresh,
    /// Loads the unread messages of a conversation the unreads list shows.
    LoadUnread { channel: String },
    /// Marks a conversation read up to its newest message.
    MarkRead { channel: String },
    /// Marks every unread conversation read.
    MarkAllRead,
    /// Opens a thread of the threads list beside it, and marks it read.
    OpenThread { channel: String, ts: Ts },
}

/// What the interface asks the worker to do for one workspace.
#[derive(Clone, Debug, PartialEq)]
pub enum Command {
    /// What mentions you or answers you, newest first. `me` is your user
    /// id, for searching when Slack's own feed cannot be read.
    Activity { me: String },
    /// The messages of `channel` after `after` (the last one you read),
    /// oldest first; with no read marker, the newest few.
    Unread { channel: String, after: Option<Ts> },
    /// The threads you follow, newest reply first. `me` is your user id,
    /// for finding the threads you replied in when Slack's own list cannot
    /// be read.
    Threads { me: String },
    /// Tells Slack you read thread `thread` up to `ts` (browser sessions;
    /// others keep no thread read state to tell).
    ReadThread { channel: String, thread: Ts, ts: Ts },
}

impl Command {
    /// The answer that says this command could not be carried out at all.
    pub fn failed(&self, error: String) -> Event {
        match self {
            Self::Activity { .. } => Event::Activity {
                result: Err(error),
                searched: false,
            },
            Self::Unread { channel, .. } => Event::Unread {
                channel: channel.clone(),
                result: Err(error),
            },
            Self::Threads { .. } => Event::Threads {
                result: Err(error),
                searched: false,
            },
            Self::ReadThread { .. } => Event::Nothing,
        }
    }
}

/// What the worker answers.
#[derive(Clone, Debug, PartialEq)]
pub enum Event {
    /// The activity list, or why there is none. `searched` says it came
    /// from a search for your name rather than Slack's activity feed, so
    /// it holds mentions of you only.
    Activity {
        result: Result<Vec<Activity>, String>,
        searched: bool,
    },
    /// A conversation's unread messages, oldest first. `more` says there
    /// are more than were read.
    Unread {
        channel: String,
        result: Result<(Vec<Message>, bool), String>,
    },
    /// The threads you follow, or why there are none. `searched` says they
    /// were found by searching for your replies rather than read from
    /// Slack's own list.
    Threads {
        result: Result<Vec<Followed>, String>,
        searched: bool,
    },
    /// A command that needs no answer was carried out (or not, which
    /// changes nothing on screen).
    Nothing,
}

/// A thread you follow: you started it, replied in it, or asked to.
#[derive(Clone, Debug, PartialEq)]
pub struct Followed {
    pub channel: String,
    pub parent: Message,
    /// Its newest replies, oldest first: at most [`THREAD_REPLIES`].
    pub replies: Vec<Message>,
    /// How many replies you have not read.
    pub unread: u32,
}

impl Followed {
    /// When it last moved: its newest reply, or the parent.
    pub fn latest(&self) -> &Ts {
        self.replies.last().map_or(&self.parent.ts, |m| &m.ts)
    }

    /// Takes in a reply seen live: it joins the newest replies and, from
    /// someone else, counts as unread.
    pub fn add_reply(&mut self, reply: &Message, me: &str) {
        if self.replies.iter().any(|m| m.ts == reply.ts) {
            return;
        }
        self.replies.push(reply.clone());
        self.replies.sort_by(|a, b| a.ts.cmp(&b.ts));
        let extra = self.replies.len().saturating_sub(THREAD_REPLIES);
        self.replies.drain(..extra);
        self.parent.reply_count += 1;
        if reply.user.as_deref() != Some(me) {
            self.unread += 1;
        }
    }
}

/// Orders followed threads as the list shows them: the one with the newest
/// reply first.
pub fn sort_threads(threads: &mut [Followed]) {
    threads.sort_by(|a, b| b.latest().cmp(a.latest()));
}

/// Why a message is in your activity.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Reason {
    /// It names you (or a group you are in).
    Mention,
    /// It says @here, @channel or @everyone.
    Everyone,
    /// It answers a thread you started or replied in.
    Reply,
}

impl Reason {
    /// What the item's line says about it, in `place` (a conversation's
    /// name).
    pub fn label(self, place: &str) -> String {
        let place = [("place", place)];
        match self {
            Self::Mention => tf("Mentioned you in {place}", &place),
            Self::Everyone => tf("Mentioned everyone in {place}", &place),
            Self::Reply => tf("Replied to your thread in {place}", &place),
        }
    }
}

/// One message in your activity.
#[derive(Clone, Debug, PartialEq)]
pub struct Activity {
    pub reason: Reason,
    pub channel: String,
    pub message: Message,
    /// Whether Slack (or the read marker) says you have not seen it.
    pub unread: bool,
}

impl Activity {
    /// Tells items apart: the same message is listed once.
    pub fn key(&self) -> (&str, &Ts) {
        (&self.channel, &self.message.ts)
    }
}

/// A list the worker fetches: what arrived last, kept on screen while it is
/// fetched again.
#[derive(Clone, Debug, PartialEq)]
pub struct Fetch<T> {
    pub value: Option<T>,
    pub loading: bool,
    pub error: Option<String>,
}

impl<T> Default for Fetch<T> {
    fn default() -> Self {
        Self {
            value: None,
            loading: false,
            error: None,
        }
    }
}

impl<T> Fetch<T> {
    /// Notes that it is being asked for.
    pub fn start(&mut self) {
        self.loading = true;
        self.error = None;
    }

    /// Takes in the answer; a failure keeps what was there.
    pub fn arrived(&mut self, result: Result<T, String>) {
        self.loading = false;
        match result {
            Ok(value) => {
                self.value = Some(value);
                self.error = None;
            }
            Err(error) => self.error = Some(error),
        }
    }

    /// Whether nothing has arrived yet and something is on its way.
    pub fn waiting(&self) -> bool {
        self.loading && self.value.is_none()
    }
}

/// What the views hold for one workspace.
#[derive(Clone, Debug, Default)]
pub struct TeamViews {
    pub activity: Fetch<Vec<Activity>>,
    /// Whether the activity came from a search rather than Slack's feed.
    pub searched: bool,
    /// Mentions and replies seen live since the list was fetched, newest
    /// last.
    pub live: Vec<Activity>,
    /// The unread messages of conversations, by id, and whether there are
    /// more than were read.
    pub unread: HashMap<String, Fetch<(Vec<Message>, bool)>>,
    pub threads: Fetch<Vec<Followed>>,
    /// Whether the threads came from a search rather than Slack's list.
    pub threads_searched: bool,
}

/// Everything the views hold.
#[derive(Clone, Debug, Default)]
pub struct State {
    /// The view shown in place of the conversation, if any.
    pub open: Option<View>,
    /// By team.
    pub teams: HashMap<String, TeamViews>,
}

impl State {
    /// What is held for `team`, made empty when nothing is.
    pub fn team_mut(&mut self, team: &str) -> &mut TeamViews {
        self.teams.entry(team.to_owned()).or_default()
    }

    /// What is held for `team`, if anything.
    pub fn team(&self, team: &str) -> Option<&TeamViews> {
        self.teams.get(team)
    }
}

impl TeamViews {
    /// The activity to list: what was fetched and what came in live, each
    /// message once, newest first.
    pub fn activity(&self) -> Vec<&Activity> {
        merge_activity(
            self.activity.value.as_deref().unwrap_or_default(),
            &self.live,
        )
    }

    /// How many items of the activity are unread, for the sidebar.
    pub fn unread_activity(&self) -> usize {
        self.activity().iter().filter(|a| a.unread).count()
    }

    /// How many followed threads have replies you have not read.
    pub fn unread_threads(&self) -> usize {
        self.threads
            .value
            .as_deref()
            .unwrap_or_default()
            .iter()
            .filter(|t| t.unread > 0)
            .count()
    }
}

/// `fetched` and `live` together, each message once (the fetched copy
/// wins, being Slack's word), newest first.
pub fn merge_activity<'a>(fetched: &'a [Activity], live: &'a [Activity]) -> Vec<&'a Activity> {
    let mut seen = HashSet::new();
    let mut out: Vec<&Activity> = fetched
        .iter()
        .chain(live.iter().rev())
        .filter(|a| seen.insert((a.channel.clone(), a.message.ts.clone())))
        .collect();
    out.sort_by(|a, b| b.message.ts.cmp(&a.message.ts));
    out
}

/// Why a message's text puts it in your activity, if it does: naming you
/// beats naming everyone.
pub fn mention_reason(text: &str, me: &str) -> Option<Reason> {
    // `<@U1>` or `<@U1|name>`, never the longer id `<@U12>`.
    if !me.is_empty() && (text.contains(&format!("<@{me}>")) || text.contains(&format!("<@{me}|")))
    {
        return Some(Reason::Mention);
    }
    ["<!here", "<!channel", "<!everyone"]
        .iter()
        .any(|word| text.contains(word))
        .then_some(Reason::Everyone)
}

/// Why a new message is in your activity, as far as what is loaded can
/// tell: it mentions you or everyone, or answers a thread you started or
/// replied in.
pub fn live_reason(workspace: &WorkspaceState, channel: &str, message: &Message) -> Option<Reason> {
    let me = workspace.info.user_id.as_str();
    if message.user.as_deref() == Some(me) {
        return None;
    }
    if let Some(reason) = mention_reason(&message.text, me) {
        return Some(reason);
    }
    let parent = message.thread_ts.as_ref().filter(|_| message.is_reply())?;
    let mine = |m: &Message| m.user.as_deref() == Some(me);
    let started = workspace.find_message(channel, parent).is_some_and(&mine);
    let replied = workspace
        .threads
        .get(&(channel.to_owned(), parent.clone()))
        .is_some_and(|t| t.messages.iter().any(mine));
    (started || replied).then_some(Reason::Reply)
}

/// A message with only what a list of references says: its time, author
/// and text.
pub fn bare_message(ts: Ts, user: Option<String>, text: String, thread: Option<Ts>) -> Message {
    Message {
        ts,
        user,
        username: None,
        bot_icon: None,
        bot_id: None,
        text,
        thread_ts: thread,
        reply_count: 0,
        replies_known: false,
        reply_users: Vec::new(),
        latest_reply: None,
        reactions: Vec::new(),
        files: Vec::new(),
        attachments: Vec::new(),
        blocks: Vec::new(),
        edited: false,
        subtype: None,
        delivery: Delivery::Sent,
        broadcast: false,
        pinned: false,
    }
}

/// The unread conversations, newest first, as the unreads list shows them.
pub fn unread_conversations(workspace: &WorkspaceState) -> Vec<&crate::model::Conversation> {
    let mut list: Vec<_> = workspace
        .conversations
        .iter()
        .filter(|c| !c.archived && workspace.is_unread(c))
        .collect();
    list.sort_by(|a, b| b.latest.cmp(&a.latest).then_with(|| a.id.cmp(&b.id)));
    list
}

/// Takes in a new message seen live: one that mentions you or answers your
/// thread joins the activity at once, and one in a conversation the
/// unreads list has loaded joins its messages.
pub fn arrived(app: &mut App, team: &str, channel: &str, message: &Message) {
    let Some(workspace) = app.workspaces.iter().find(|w| w.info.team_id == team) else {
        return;
    };
    let reason = live_reason(workspace, channel, message);
    let views = app.views.team_mut(team);
    if message.in_channel()
        && let Some((messages, _)) = views.unread.get_mut(channel).and_then(|f| f.value.as_mut())
        && !messages.iter().any(|m| m.ts == message.ts)
    {
        messages.push(message.clone());
    }
    // A list of threads already read stays fresh; one never read is read
    // whole when it is opened.
    if let Some(parent) = message.thread_ts.as_ref().filter(|_| message.is_reply())
        && let Some(threads) = views.threads.value.as_mut()
    {
        let me = workspace.info.user_id.as_str();
        let known = workspace.find_message(channel, parent).cloned();
        match threads
            .iter_mut()
            .find(|t| t.channel == channel && t.parent.ts == *parent)
        {
            Some(thread) => thread.add_reply(message, me),
            // A reply to a thread of yours that the list does not hold yet.
            None if reason == Some(Reason::Reply) => {
                if let Some(parent) = known {
                    let mut thread = Followed {
                        channel: channel.to_owned(),
                        parent,
                        replies: Vec::new(),
                        unread: 0,
                    };
                    thread.add_reply(message, me);
                    threads.push(thread);
                }
            }
            None => {}
        }
        sort_threads(threads);
    }
    let Some(reason) = reason else {
        return;
    };
    if views.live.iter().any(|a| a.key() == (channel, &message.ts)) {
        return;
    }
    views.live.push(Activity {
        reason,
        channel: channel.to_owned(),
        message: message.clone(),
        unread: true,
    });
    if views.live.len() > LIVE_LIMIT {
        views.live.remove(0);
    }
}

/// Carries out a view's request.
pub fn apply(app: &mut App, action: Action) {
    let Some(team) = app.active_team() else {
        return;
    };
    match action {
        Action::Open(view) => {
            app.views.open = Some(view);
            app.page = crate::app::Page::Main;
            app.editing = None;
            app.thread = None;
            app.convos.details = None;
            load(app, &team, view);
        }
        Action::Close => {
            app.views.open = None;
            // Back in the conversation, which is read now it shows.
            if let Some(channel) = app.active_workspace().and_then(|w| w.active.clone()) {
                app.open_conversation(&channel);
            }
        }
        Action::Refresh => {
            if let Some(view) = app.views.open {
                load(app, &team, view);
            }
        }
        Action::LoadUnread { channel } => load_unread(app, &team, &channel),
        Action::MarkRead { channel } => app.mark_read(&team, &channel),
        Action::MarkAllRead => {
            let channels: Vec<String> = app
                .active_workspace()
                .map(|w| {
                    unread_conversations(w)
                        .iter()
                        .map(|c| c.id.clone())
                        .collect()
                })
                .unwrap_or_default();
            for channel in channels {
                app.mark_read(&team, &channel);
            }
        }
        Action::OpenThread { channel, ts } => {
            let newest = app
                .views
                .team_mut(&team)
                .threads
                .value
                .as_mut()
                .and_then(|threads| {
                    let thread = threads
                        .iter_mut()
                        .find(|t| t.channel == channel && t.parent.ts == ts)?;
                    let unread = std::mem::take(&mut thread.unread);
                    (unread > 0).then(|| thread.latest().clone())
                });
            if let Some(newest) = newest {
                send(
                    app,
                    &team,
                    Command::ReadThread {
                        channel: channel.clone(),
                        thread: ts.clone(),
                        ts: newest,
                    },
                );
            }
            app.actions
                .push(crate::model::Action::OpenThread { channel, ts });
        }
    }
}

/// Asks for a conversation's unread messages, unless they are on their way.
fn load_unread(app: &mut App, team: &str, channel: &str) {
    let Some(after) = app
        .workspaces
        .iter()
        .find(|w| w.info.team_id == team)
        .and_then(|w| w.conversation(channel))
        .map(|c| c.last_read.clone())
    else {
        return;
    };
    let fetch = app
        .views
        .team_mut(team)
        .unread
        .entry(channel.to_owned())
        .or_default();
    if fetch.loading {
        return;
    }
    fetch.start();
    send(
        app,
        team,
        Command::Unread {
            channel: channel.to_owned(),
            after,
        },
    );
}

/// Asks for what `view` lists in `team`.
fn load(app: &mut App, team: &str, view: View) {
    let me = app
        .workspaces
        .iter()
        .find(|w| w.info.team_id == team)
        .map(|w| w.info.user_id.clone())
        .unwrap_or_default();
    match view {
        View::Activity => {
            app.views.team_mut(team).activity.start();
            send(app, team, Command::Activity { me });
        }
        View::Unreads => {
            // Read afresh: what was loaded may be read by now.
            app.views.team_mut(team).unread.clear();
            let channels: Vec<String> = app
                .workspaces
                .iter()
                .find(|w| w.info.team_id == team)
                .map(|w| {
                    unread_conversations(w)
                        .iter()
                        .take(UNREAD_LIMIT)
                        .map(|c| c.id.clone())
                        .collect()
                })
                .unwrap_or_default();
            for channel in channels {
                load_unread(app, team, &channel);
            }
        }
        View::Threads => {
            app.views.team_mut(team).threads.start();
            send(app, team, Command::Threads { me });
        }
    }
}

/// Sends a command for `team` to the worker.
fn send(app: &App, team: &str, command: Command) {
    app.backend.send(backend::Command::Views {
        team: team.to_owned(),
        command,
    });
}

/// Takes in one of the worker's answers for `team`.
pub fn handle(app: &mut App, team: &str, event: Event) {
    match event {
        Event::Activity { result, searched } => {
            if let Ok(items) = &result {
                let people: Vec<String> = items
                    .iter()
                    .filter_map(|a| a.message.user.clone())
                    .collect();
                crate::convos::fetch_unknown(app, team, &people);
            }
            let views = app.views.team_mut(team);
            if result.is_ok() {
                views.searched = searched;
                // What came in live is in the fresh list, or was before it.
                views.live.clear();
            }
            views.activity.arrived(result);
        }
        Event::Unread { channel, result } => {
            if let Ok((messages, _)) = &result {
                let people: Vec<String> = messages.iter().filter_map(|m| m.user.clone()).collect();
                crate::convos::fetch_unknown(app, team, &people);
            }
            app.views
                .team_mut(team)
                .unread
                .entry(channel)
                .or_default()
                .arrived(result);
        }
        Event::Threads { result, searched } => {
            if let Ok(threads) = &result {
                let people: Vec<String> = threads
                    .iter()
                    .flat_map(|t| std::iter::once(&t.parent).chain(&t.replies))
                    .filter_map(|m| m.user.clone())
                    .collect();
                crate::convos::fetch_unknown(app, team, &people);
            }
            let views = app.views.team_mut(team);
            if result.is_ok() {
                views.threads_searched = searched;
            }
            views.threads.arrived(result.map(|mut threads| {
                sort_threads(&mut threads);
                threads
            }));
        }
        Event::Nothing => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Workspace;

    fn item(channel: &str, ts: &str, text: &str) -> Activity {
        Activity {
            reason: Reason::Mention,
            channel: channel.into(),
            message: bare_message(Ts::new(ts), Some("U1".into()), text.into(), None),
            unread: false,
        }
    }

    #[test]
    fn activity_lists_each_message_once_newest_first() {
        let fetched = [item("C1", "1.0", "slack"), item("C1", "3.0", "slack")];
        let live = [item("C1", "3.0", "live"), item("C2", "2.0", "live")];
        let merged = merge_activity(&fetched, &live);
        let order: Vec<(&str, &str)> = merged
            .iter()
            .map(|a| (a.channel.as_str(), a.message.ts.as_str()))
            .collect();
        assert_eq!(order, [("C1", "3.0"), ("C2", "2.0"), ("C1", "1.0")]);
        assert_eq!(merged[0].message.text, "slack", "Slack's copy wins");
    }

    #[test]
    fn mentions_of_you_beat_mentions_of_everyone() {
        assert_eq!(
            mention_reason("<!here> <@U0|me> look", "U0"),
            Some(Reason::Mention)
        );
        assert_eq!(
            mention_reason("<!channel> look", "U0"),
            Some(Reason::Everyone)
        );
        assert_eq!(mention_reason("<@U01> look", "U0"), None, "a longer id");
        assert_eq!(mention_reason("plain", "U0"), None);
        assert_eq!(mention_reason("<@>", ""), None);
    }

    fn workspace() -> WorkspaceState {
        WorkspaceState::new(Workspace {
            team_id: "T1".into(),
            name: "Acme".into(),
            domain: "acme".into(),
            icon: None,
            user_id: "U0".into(),
        })
    }

    #[test]
    fn replies_to_your_threads_count_as_activity() {
        let mut workspace = workspace();
        let mut parent = bare_message(Ts::new("1.0"), Some("U0".into()), "plan".into(), None);
        parent.thread_ts = Some(Ts::new("1.0"));
        workspace
            .timelines
            .entry("C1".into())
            .or_default()
            .upsert(parent);
        let reply = bare_message(
            Ts::new("2.0"),
            Some("U1".into()),
            "ok".into(),
            Some(Ts::new("1.0")),
        );
        assert_eq!(live_reason(&workspace, "C1", &reply), Some(Reason::Reply));
        // A thread someone else started, where you have not replied.
        let other = bare_message(
            Ts::new("3.0"),
            Some("U1".into()),
            "ok".into(),
            Some(Ts::new("0.5")),
        );
        assert_eq!(live_reason(&workspace, "C1", &other), None);
        // One you replied in.
        let mut thread = crate::model::Timeline::default();
        thread.upsert(bare_message(
            Ts::new("0.7"),
            Some("U0".into()),
            "me too".into(),
            Some(Ts::new("0.5")),
        ));
        workspace
            .threads
            .insert(("C1".into(), Ts::new("0.5")), thread);
        assert_eq!(live_reason(&workspace, "C1", &other), Some(Reason::Reply));
        // Your own messages are never your activity.
        let mine = bare_message(Ts::new("4.0"), Some("U0".into()), "<@U0>".into(), None);
        assert_eq!(live_reason(&workspace, "C1", &mine), None);
    }

    fn conversation(id: &str, latest: &str, read: &str) -> crate::model::Conversation {
        crate::model::Conversation {
            id: id.into(),
            name: id.to_lowercase(),
            kind: crate::model::ConversationKind::Channel,
            user: None,
            topic: String::new(),
            purpose: String::new(),
            members: None,
            archived: false,
            last_read: Some(Ts::new(read)),
            latest: Some(Ts::new(latest)),
            unread: 0,
            mentions: 0,
        }
    }

    #[test]
    fn unreads_list_what_is_new_newest_first() {
        let mut workspace = workspace();
        workspace.conversations = vec![
            conversation("C1", "5.0", "4.0"),
            conversation("C2", "3.0", "3.0"),
            conversation("C3", "9.0", "1.0"),
            crate::model::Conversation {
                archived: true,
                ..conversation("C4", "9.0", "1.0")
            },
            conversation("C5", "8.0", "1.0"),
        ];
        workspace.desktop.local_muted.insert("C5".into());
        let ids: Vec<&str> = unread_conversations(&workspace)
            .iter()
            .map(|c| c.id.as_str())
            .collect();
        assert_eq!(
            ids,
            ["C3", "C1"],
            "read, archived and muted ones are left out"
        );
    }

    #[test]
    fn live_replies_keep_a_thread_fresh() {
        let parent = bare_message(Ts::new("1.0"), Some("U0".into()), "plan".into(), None);
        let mut thread = Followed {
            channel: "C1".into(),
            parent,
            replies: Vec::new(),
            unread: 0,
        };
        for (ts, user) in [("2.0", "U1"), ("3.0", "U0"), ("4.0", "U1"), ("5.0", "U2")] {
            let reply = bare_message(Ts::new(ts), Some(user.into()), String::new(), None);
            thread.add_reply(&reply, "U0");
            thread.add_reply(&reply, "U0");
        }
        let kept: Vec<&str> = thread.replies.iter().map(|m| m.ts.as_str()).collect();
        assert_eq!(kept, ["3.0", "4.0", "5.0"], "the newest few, oldest first");
        assert_eq!(
            thread.unread, 3,
            "your own reply is read, a repeat counts once"
        );
        assert_eq!(thread.parent.reply_count, 4);
        assert_eq!(thread.latest(), &Ts::new("5.0"));
        let quiet = Followed {
            channel: "C2".into(),
            parent: bare_message(Ts::new("4.5"), None, String::new(), None),
            replies: Vec::new(),
            unread: 0,
        };
        let mut threads = vec![quiet, thread];
        sort_threads(&mut threads);
        assert_eq!(threads[0].channel, "C1");
    }

    #[test]
    fn a_failed_fetch_keeps_what_was_there() {
        let mut fetch = Fetch::default();
        fetch.start();
        assert!(fetch.waiting());
        fetch.arrived(Ok(vec![1]));
        fetch.start();
        assert!(!fetch.waiting(), "the old list stays on screen");
        fetch.arrived(Err("down".into()));
        assert_eq!(fetch.value, Some(vec![1]));
        assert_eq!(fetch.error.as_deref(), Some("down"));
    }
}
