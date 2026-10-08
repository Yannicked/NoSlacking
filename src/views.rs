//! The views at the top of the sidebar that take the place of the open
//! conversation, as in Slack: Activity (what mentions you or answers you)
//! All unreads (every conversation with something new, with the new
//! messages), Threads (the threads you follow, with their newest replies)
//! Later (messages saved for later, and your reminders) and Scheduled
//! (messages waiting to be sent, see [`schedule`]).
//!
//! Views push [`Action`]s (wrapped in [`crate::model::Action::Views`]);
//! [`apply`] turns them into [`Command`]s for the worker, whose answers come
//! back as [`Event`]s for [`handle`]. What the views hold lives in
//! [`State`], kept on [`App`].

pub mod remind;
pub mod schedule;

use std::collections::{HashMap, HashSet};

use crate::app::{App, LentDraft, WorkspaceState};
use crate::backend;
use crate::failure::Failure;
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
    /// Messages saved for later, and reminders.
    Later,
    /// Messages scheduled to be sent later.
    Scheduled,
}

impl View {
    /// Every view, in the sidebar's order.
    pub const ALL: [Self; 5] = [
        Self::Unreads,
        Self::Threads,
        Self::Activity,
        Self::Later,
        Self::Scheduled,
    ];

    /// Its name in the sidebar and its header.
    pub fn label(self) -> String {
        match self {
            Self::Activity => t("Activity").into_owned(),
            Self::Unreads => t("All unreads").into_owned(),
            Self::Threads => t("Threads").into_owned(),
            Self::Later => t("Later").into_owned(),
            Self::Scheduled => t("Scheduled").into_owned(),
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
    /// Saves a message for later, or with `save` false takes it off the
    /// list.
    Save { channel: String, ts: Ts, save: bool },
    /// Marks a reminder complete.
    CompleteReminder { id: String },
    /// Schedules the draft of the composer for `thread` (or the
    /// conversation's) to be sent `when`.
    SendLater {
        thread: Option<Ts>,
        when: schedule::When,
    },
    /// Asks when to send the draft of the composer for `thread`.
    AskSendLater { thread: Option<Ts> },
    /// Opens a scheduled message to change its text or time.
    EditScheduled { id: String },
    /// Carries out the "Send at" dialog.
    ConfirmSchedule,
    /// Closes the "Send at" dialog.
    CloseSchedule,
    /// Keeps a scheduled message from being sent.
    CancelScheduled { channel: String, id: String },
    /// Sets a reminder about message `ts` of `channel` (a reply in
    /// `thread`) for `when`.
    Remind {
        channel: String,
        ts: Ts,
        thread: Option<Ts>,
        when: remind::RemindIn,
    },
    /// Asks when to be reminded about message `ts` of `channel`.
    AskRemind {
        channel: String,
        ts: Ts,
        thread: Option<Ts>,
    },
    /// Follows the thread under `thread` in `channel`, or with `follow`
    /// false stops following it (browser sessions; see [`can_follow`]).
    Follow {
        channel: String,
        thread: Ts,
        follow: bool,
    },
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
    /// The messages saved for later, newest saved first.
    Saved,
    /// Your reminders that are not complete.
    Reminders,
    /// Saves a message for later or takes it off the list, already shown.
    Save { channel: String, ts: Ts, save: bool },
    /// Marks a reminder complete, already taken off the list.
    CompleteReminder { id: String },
    /// The messages waiting to be sent.
    Scheduled,
    /// `chat.scheduleMessage`, answered under `request`. `replace` is a
    /// scheduled message this one takes the place of, deleted once the new
    /// one is in.
    Schedule {
        request: u64,
        channel: String,
        text: String,
        thread: Option<Ts>,
        post_at: i64,
        replace: Option<String>,
    },
    /// `chat.deleteScheduledMessage`, already taken off the list.
    CancelScheduled { channel: String, id: String },
    /// `reminders.add` with this text, at `time` (seconds since the epoch).
    Remind { text: String, time: i64 },
    /// Follows a thread or stops following it, already shown (see
    /// [`follow_request`]).
    Follow {
        channel: String,
        thread: Ts,
        last_read: Ts,
        follow: bool,
    },
}

impl Command {
    /// The answer that says this command could not be carried out at all.
    pub fn failed(&self, error: Failure) -> Event {
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
            Self::Saved => Event::Saved {
                result: Err(error),
                starred: false,
            },
            Self::Reminders => Event::Reminders { result: Err(error) },
            Self::Save { channel, ts, save } => Event::Failed {
                what: Doing::Save {
                    channel: channel.clone(),
                    ts: ts.clone(),
                    save: *save,
                },
                error,
            },
            Self::CompleteReminder { id } => Event::Failed {
                what: Doing::CompleteReminder { id: id.clone() },
                error,
            },
            Self::Scheduled => Event::ScheduledList { result: Err(error) },
            Self::Schedule { request, .. } => Event::ScheduleDone {
                request: *request,
                result: Err(error),
            },
            Self::CancelScheduled { .. } => Event::Failed {
                what: Doing::CancelScheduled,
                error,
            },
            Self::Remind { time, .. } => Event::Reminded {
                time: *time,
                result: Err(error),
            },
            Self::Follow {
                channel,
                thread,
                follow,
                ..
            } => Event::Failed {
                what: Doing::Follow {
                    channel: channel.clone(),
                    thread: thread.clone(),
                    follow: *follow,
                },
                error,
            },
        }
    }
}

/// Which change failed, already shown as done, so the interface can take
/// it back and say so.
#[derive(Clone, Debug, PartialEq)]
pub enum Doing {
    /// Saving (`save`) a message or taking it off the list; the list
    /// shows it as it was.
    Save { channel: String, ts: Ts, save: bool },
    /// Completing a reminder; the reminders are read again.
    CompleteReminder { id: String },
    /// Cancelling a scheduled message; the list is read again.
    CancelScheduled,
    /// Following (`follow`) a thread, or stopping: it shows as it was.
    Follow {
        channel: String,
        thread: Ts,
        follow: bool,
    },
}

/// What the worker answers.
#[derive(Clone, Debug, PartialEq)]
pub enum Event {
    /// The activity list, or why there is none. `searched` says it came
    /// from a search for your name rather than Slack's activity feed, so
    /// it holds mentions of you only.
    Activity {
        result: Result<Vec<Activity>, Failure>,
        searched: bool,
    },
    /// A conversation's unread messages, oldest first. `more` says there
    /// are more than were read.
    Unread {
        channel: String,
        result: Result<(Vec<Message>, bool), Failure>,
    },
    /// The threads you follow, or why there are none. `searched` says they
    /// were found by searching for your replies rather than read from
    /// Slack's own list.
    Threads {
        result: Result<Vec<Followed>, Failure>,
        searched: bool,
    },
    /// The messages saved for later, or why there are none. `starred` says
    /// they are the older starred messages (OAuth sign-ins, which cannot
    /// read Later).
    Saved {
        result: Result<Vec<Saved>, Failure>,
        starred: bool,
    },
    Reminders {
        result: Result<Vec<Reminder>, Failure>,
    },
    /// A change already shown as done failed.
    Failed { what: Doing, error: Failure },
    /// The messages waiting to be sent, soonest first.
    ScheduledList {
        result: Result<Vec<schedule::Scheduled>, Failure>,
    },
    /// Slack answered schedule request `request`.
    ScheduleDone {
        request: u64,
        result: Result<schedule::Scheduled, Failure>,
    },
    /// Slack answered a reminder set for `time`.
    Reminded {
        time: i64,
        result: Result<(), Failure>,
    },
    /// You followed a thread or stopped, here or in another Slack client
    /// (`thread_subscribed` and `thread_unsubscribed`).
    Followed {
        channel: String,
        thread: Ts,
        follow: bool,
    },
    /// A command that needs no answer was carried out (or not, which
    /// changes nothing on screen).
    Nothing,
}

/// A message saved for later.
#[derive(Clone, Debug, PartialEq)]
pub struct Saved {
    pub channel: String,
    pub message: Message,
}

/// A reminder Slack will send you.
#[derive(Clone, Debug, PartialEq)]
pub struct Reminder {
    pub id: String,
    /// What to be reminded of, as typed.
    pub text: String,
    /// When, in seconds since the epoch; a recurring one names its next
    /// time.
    pub time: Option<i64>,
    pub recurring: bool,
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

/// Whether a sign-in can follow and unfollow threads. Slack has no public
/// method for it: its web client calls `subscriptions.thread.add` and
/// `.remove`, which only a browser session's token may.
pub fn can_follow(sign_in: crate::model::SignInKind) -> bool {
    sign_in == crate::model::SignInKind::Session
}

/// The web client's method and form to follow (`follow`) the thread under
/// `thread` in `channel`, or to stop following it, read up to
/// `last_read`. The fields are those emacs-slack sends to both methods.
pub fn follow_request(
    channel: &str,
    thread: &Ts,
    last_read: &Ts,
    follow: bool,
) -> (&'static str, Vec<(&'static str, String)>) {
    let method = if follow {
        "subscriptions.thread.add"
    } else {
        "subscriptions.thread.remove"
    };
    (
        method,
        vec![
            ("channel", channel.to_owned()),
            ("thread_ts", thread.as_str().to_owned()),
            ("last_read", last_read.as_str().to_owned()),
        ],
    )
}

/// Shows thread `thread` of `channel` as followed or not: on its parent,
/// wherever it is loaded, and in the threads list, which it leaves when
/// unfollowed and joins when followed (if its parent is loaded here).
pub fn show_followed(app: &mut App, team: &str, channel: &str, thread: &Ts, follow: bool) {
    let Some(workspace) = app.workspaces.iter_mut().find(|w| w.info.team_id == team) else {
        return;
    };
    for timeline in workspace.timelines_for_mut(channel) {
        if let Some(parent) = timeline.messages.iter_mut().find(|m| m.ts == *thread) {
            parent.subscribed = Some(follow);
        }
    }
    let parent = workspace.find_message(channel, thread).cloned();
    let replies: Vec<Message> = workspace
        .threads
        .get(&(channel.to_owned(), thread.clone()))
        .map(|t| {
            t.messages
                .iter()
                .filter(|m| m.ts != *thread && !m.ts.is_local())
                .cloned()
                .collect()
        })
        .unwrap_or_default();
    let Some(threads) = app.views.team_mut(team).threads.value.as_mut() else {
        return;
    };
    let listed = threads
        .iter()
        .position(|t| t.channel == channel && t.parent.ts == *thread);
    match (follow, listed) {
        (false, Some(at)) => {
            threads.remove(at);
        }
        (true, None) => {
            if let Some(parent) = parent {
                let mut replies = replies;
                let extra = replies.len().saturating_sub(THREAD_REPLIES);
                replies.drain(..extra);
                threads.push(Followed {
                    channel: channel.to_owned(),
                    parent,
                    replies,
                    unread: 0,
                });
                sort_threads(threads);
            }
        }
        _ => {}
    }
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
    pub error: Option<Failure>,
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
    pub fn arrived(&mut self, result: Result<T, Failure>) {
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
    pub saved: Fetch<Vec<Saved>>,
    /// Whether the saved messages are the older starred ones.
    pub starred: bool,
    pub reminders: Fetch<Vec<Reminder>>,
    /// The messages known to be saved, by conversation and timestamp, for
    /// the message toolbar's "Save for later" or "Remove from Later".
    pub saved_keys: HashSet<(String, Ts)>,
    pub scheduled: Fetch<Vec<schedule::Scheduled>>,
}

/// A schedule request on its way: what to put back if Slack refuses.
#[derive(Clone, Debug)]
pub struct Pending {
    /// The composer it came from, and its draft as it was.
    pub draft: Option<LentDraft>,
    /// The scheduled message it replaces.
    pub replace: Option<String>,
}

/// Everything the views hold.
#[derive(Clone, Debug, Default)]
pub struct State {
    /// The view shown in place of the conversation, if any.
    pub open: Option<View>,
    /// By team.
    pub teams: HashMap<String, TeamViews>,
    /// The "Send at" dialog, when it is open.
    pub dialog: Option<schedule::Dialog>,
    /// Schedule requests waiting for Slack, by request.
    pub pending: HashMap<u64, Pending>,
    next_request: u64,
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
        client_msg_id: None,
        subscribed: None,
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
        // Whether you follow the thread, as any loaded copy of its parent
        // says: one you stopped following stays off the list.
        let followed = workspace
            .timelines_for(channel)
            .find_map(|t| t.messages.iter().find(|m| m.ts == *parent)?.subscribed);
        match threads
            .iter_mut()
            .find(|t| t.channel == channel && t.parent.ts == *parent)
        {
            Some(thread) => thread.add_reply(message, me),
            // A reply to a thread of yours, or one you follow, that the
            // list does not hold yet.
            None if followed == Some(true)
                || (reason == Some(Reason::Reply) && followed.is_none()) =>
            {
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
        Action::Save { channel, ts, save } => {
            if ts.is_local() {
                return;
            }
            let message = app
                .workspaces
                .iter()
                .find(|w| w.info.team_id == team)
                .and_then(|w| w.find_message(&channel, &ts))
                .cloned();
            saved(app.views.team_mut(&team), &channel, &ts, save, message);
            app.toast(
                if save {
                    t("Saved for later")
                } else {
                    t("Removed from Later")
                },
                false,
            );
            send(app, &team, Command::Save { channel, ts, save });
        }
        Action::CompleteReminder { id } => {
            if let Some(reminders) = app.views.team_mut(&team).reminders.value.as_mut() {
                reminders.retain(|r| r.id != id);
            }
            send(app, &team, Command::CompleteReminder { id });
        }
        Action::SendLater { thread, when } => {
            let Some((key, channel)) = composer(app, &team, thread.as_ref()) else {
                return;
            };
            if let Some(post_at) = when.post_at(&jiff::Zoned::now()) {
                schedule_draft(app, &team, key, channel, thread, post_at);
            }
        }
        Action::AskSendLater { thread } => {
            let Some((key, channel)) = composer(app, &team, thread.as_ref()) else {
                return;
            };
            app.focus_overlay = true;
            app.views.dialog = Some(schedule::Dialog::new(
                schedule::Target::Draft {
                    key,
                    channel,
                    thread,
                },
                String::new(),
                None,
                &jiff::Zoned::now(),
            ));
        }
        Action::EditScheduled { id } => {
            let Some(message) = app
                .views
                .team(&team)
                .and_then(|v| v.scheduled.value.as_ref())
                .and_then(|list| list.iter().find(|s| s.id == id))
                .cloned()
            else {
                return;
            };
            let (text, mentions) = app
                .active_workspace()
                .map(|w| w.editable(&message.text))
                .unwrap_or_default();
            app.focus_overlay = true;
            let at = Some(message.post_at);
            let mut dialog = schedule::Dialog::new(
                schedule::Target::Edit(message),
                text,
                at,
                &jiff::Zoned::now(),
            );
            dialog.mentions = mentions;
            app.views.dialog = Some(dialog);
        }
        Action::ConfirmSchedule => confirm_schedule(app, &team),
        Action::CloseSchedule => app.views.dialog = None,
        Action::CancelScheduled { channel, id } => {
            if let Some(list) = app.views.team_mut(&team).scheduled.value.as_mut() {
                list.retain(|s| s.id != id);
            }
            send(app, &team, Command::CancelScheduled { channel, id });
        }
        Action::Remind {
            channel,
            ts,
            thread,
            when,
        } => {
            if let Some(time) = when.at(&jiff::Zoned::now()) {
                remind(app, &team, &channel, &ts, thread.as_ref(), time);
            }
        }
        Action::AskRemind {
            channel,
            ts,
            thread,
        } => {
            app.focus_overlay = true;
            app.views.dialog = Some(schedule::Dialog::new(
                schedule::Target::Remind {
                    channel,
                    ts,
                    thread,
                },
                String::new(),
                None,
                &jiff::Zoned::now(),
            ));
        }
        Action::Follow {
            channel,
            thread,
            follow,
        } => {
            let Some(workspace) = app.workspaces.iter().find(|w| w.info.team_id == team) else {
                return;
            };
            if !can_follow(workspace.info.sign_in) {
                return;
            }
            // Followed from here, read up to its newest reply known here.
            let last_read = workspace
                .threads
                .get(&(channel.clone(), thread.clone()))
                .and_then(|t| t.messages.iter().rev().find(|m| !m.ts.is_local()))
                .map_or_else(|| thread.clone(), |m| m.ts.clone());
            show_followed(app, &team, &channel, &thread, follow);
            send(
                app,
                &team,
                Command::Follow {
                    channel,
                    thread,
                    last_read,
                    follow,
                },
            );
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

/// The draft key and conversation of the composer for `thread`, or the
/// open conversation's.
fn composer(app: &App, team: &str, thread: Option<&Ts>) -> Option<(String, String)> {
    let channel = match thread {
        Some(_) => app.thread.as_ref().map(|(c, _)| c.clone()),
        None => app.active_workspace().and_then(|w| w.active.clone()),
    }?;
    Some((App::draft_key(team, &channel, thread), channel))
}

/// Takes a composer's draft and asks Slack to send it at `post_at`. A
/// refusal puts the draft back.
fn schedule_draft(
    app: &mut App,
    team: &str,
    key: String,
    channel: String,
    thread: Option<Ts>,
    post_at: i64,
) {
    let Some(draft) = app.drafts.get(&key).cloned() else {
        return;
    };
    if draft.text.trim().is_empty() {
        return;
    }
    if crate::slash::parse(&draft.text).is_some() {
        app.toast(t("A slash command cannot be scheduled."), true);
        return;
    }
    let wire = crate::app::to_wire(&draft.text, &draft.mentions);
    let wire = crate::emoji::tone_shortcodes(&wire, app.settings.skin_tone);
    app.drafts.remove(&key);
    let request = next_request(app);
    app.views.pending.insert(
        request,
        Pending {
            draft: Some(LentDraft { key, draft }),
            replace: None,
        },
    );
    send(
        app,
        team,
        Command::Schedule {
            request,
            channel,
            text: wire,
            thread,
            post_at,
            replace: None,
        },
    );
}

/// Asks Slack to remind you at `time` about message `ts` of `channel`,
/// quoting it and linking to it, since a reminder cannot point at a
/// message itself.
fn remind(app: &mut App, team: &str, channel: &str, ts: &Ts, thread: Option<&Ts>, time: i64) {
    let Some(workspace) = app.workspaces.iter().find(|w| w.info.team_id == team) else {
        return;
    };
    let Some(link) = crate::links::permalink(&workspace.info.domain, channel, ts, thread) else {
        app.toast(t("This message has no link yet"), true);
        return;
    };
    let plain = workspace
        .find_message(channel, ts)
        .map(|m| crate::ui::plain_text(workspace, m))
        .unwrap_or_default();
    let text = remind::reminder_text(&plain, &link);
    send(app, team, Command::Remind { text, time });
}

fn next_request(app: &mut App) -> u64 {
    app.views.next_request += 1;
    app.views.next_request
}

/// Carries out the "Send at" dialog: checks the time, then schedules the
/// draft, or sends the changed message in place of the old one.
fn confirm_schedule(app: &mut App, team: &str) {
    let Some(dialog) = app.views.dialog.as_mut() else {
        return;
    };
    if dialog.busy {
        return;
    }
    let now = jiff::Zoned::now();
    let post_at = match schedule::moment_within(
        &dialog.date,
        &dialog.time,
        now.time_zone(),
        now.timestamp().as_second(),
        dialog.target.max_ahead(),
    ) {
        Ok(post_at) => post_at,
        Err(problem) => {
            dialog.problem = Some(problem);
            return;
        }
    };
    match dialog.target.clone() {
        schedule::Target::Draft {
            key,
            channel,
            thread,
        } => {
            app.views.dialog = None;
            schedule_draft(app, team, key, channel, thread, post_at);
        }
        schedule::Target::Remind {
            channel,
            ts,
            thread,
        } => {
            app.views.dialog = None;
            remind(app, team, &channel, &ts, thread.as_ref(), post_at);
        }
        schedule::Target::Edit(old) => {
            if dialog.text.trim().is_empty() {
                dialog.problem = Some(schedule::Problem::Empty);
                return;
            }
            dialog.busy = true;
            dialog.problem = None;
            let wire = crate::app::to_wire(&dialog.text, &dialog.mentions);
            let wire = crate::emoji::tone_shortcodes(&wire, app.settings.skin_tone);
            let request = next_request(app);
            app.views.pending.insert(
                request,
                Pending {
                    draft: None,
                    replace: Some(old.id.clone()),
                },
            );
            send(
                app,
                team,
                Command::Schedule {
                    request,
                    channel: old.channel,
                    text: wire,
                    thread: old.thread,
                    post_at,
                    replace: Some(old.id),
                },
            );
        }
    }
}

/// Takes in Slack's answer to a schedule request.
fn scheduled(
    app: &mut App,
    team: &str,
    request: u64,
    result: Result<schedule::Scheduled, Failure>,
) {
    let pending = app.views.pending.remove(&request);
    let busy = app.views.dialog.as_ref().is_some_and(|d| d.busy);
    match result {
        Ok(message) => {
            if busy {
                app.views.dialog = None;
            }
            let when = crate::ui::moment_label(message.post_at);
            let list = app.views.team_mut(team).scheduled.value.as_mut();
            if let Some(list) = list {
                if let Some(old) = pending.as_ref().and_then(|p| p.replace.as_ref()) {
                    list.retain(|s| s.id != *old);
                }
                list.push(message);
                list.sort_by_key(|s| s.post_at);
            }
            app.toast(tf("Scheduled for {when}", &[("when", &when)]), false);
        }
        Err(error) => {
            if let Some(dialog) = app.views.dialog.as_mut() {
                dialog.busy = false;
            }
            // Back in its composer, unless something new was typed there.
            if let Some(LentDraft { key, draft }) = pending.and_then(|p| p.draft) {
                let current = app.drafts.edit(key);
                if current.text.trim().is_empty() {
                    *current = draft;
                }
            }
            app.toast(
                tf(
                    "Could not schedule the message: {error}",
                    &[("error", &error.message())],
                ),
                true,
            );
        }
    }
}

/// Shows a message as saved or not: in the toolbar's state, and in the
/// saved list when it is loaded (a message saved that is not loaded here
/// joins the list when it is next read).
pub fn saved(views: &mut TeamViews, channel: &str, ts: &Ts, save: bool, message: Option<Message>) {
    let key = (channel.to_owned(), ts.clone());
    if save {
        views.saved_keys.insert(key);
    } else {
        views.saved_keys.remove(&key);
    }
    let Some(list) = views.saved.value.as_mut() else {
        return;
    };
    list.retain(|s| !(s.channel == channel && s.message.ts == *ts));
    if save && let Some(message) = message {
        list.insert(
            0,
            Saved {
                channel: channel.to_owned(),
                message,
            },
        );
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

/// Reads the threads you follow when a browser session's workspace is
/// ready, so the sidebar counts the unread ones before Threads is opened:
/// Slack's own list is one call. Other sign-ins would search and read up
/// to [`crate::backend`]'s limit of threads one by one, so they wait for
/// the view to open.
pub fn warm_threads(app: &mut App, team: &str) {
    let Some(workspace) = app.workspaces.iter().find(|w| w.info.team_id == team) else {
        return;
    };
    if !can_follow(workspace.info.sign_in) {
        return;
    }
    let me = workspace.info.user_id.clone();
    let threads = &mut app.views.team_mut(team).threads;
    if threads.value.is_some() || threads.loading {
        return;
    }
    threads.start();
    send(app, team, Command::Threads { me });
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
        View::Later => {
            let views = app.views.team_mut(team);
            views.saved.start();
            views.reminders.start();
            send(app, team, Command::Saved);
            send(app, team, Command::Reminders);
        }
        View::Scheduled => {
            app.views.team_mut(team).scheduled.start();
            send(app, team, Command::Scheduled);
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
        Event::Saved { result, starred } => {
            if let Ok(list) = &result {
                let people: Vec<String> =
                    list.iter().filter_map(|s| s.message.user.clone()).collect();
                crate::convos::fetch_unknown(app, team, &people);
            }
            let views = app.views.team_mut(team);
            if let Ok(list) = &result {
                views.starred = starred;
                views.saved_keys = list
                    .iter()
                    .map(|s| (s.channel.clone(), s.message.ts.clone()))
                    .collect();
            }
            views.saved.arrived(result);
        }
        Event::Reminders { result } => app.views.team_mut(team).reminders.arrived(result),
        Event::Failed { what, error } => failed(app, team, what, &error),
        Event::ScheduledList { result } => app.views.team_mut(team).scheduled.arrived(result),
        Event::ScheduleDone { request, result } => scheduled(app, team, request, result),
        Event::Reminded { time, result } => match result {
            Ok(()) => {
                let when = crate::ui::moment_label(time);
                app.toast(tf("I will remind you {when}", &[("when", &when)]), false);
                // The Later view lists reminders: it shows the new one.
                let views = app.views.team_mut(team);
                if views.reminders.value.is_some() || views.reminders.loading {
                    views.reminders.start();
                    send(app, team, Command::Reminders);
                }
            }
            Err(error) => app.toast(remind_failure(&error), true),
        },
        Event::Followed {
            channel,
            thread,
            follow,
        } => show_followed(app, team, &channel, &thread, follow),
        Event::Nothing => {}
    }
}

/// Takes back a change that failed and says so.
fn failed(app: &mut App, team: &str, what: Doing, error: &Failure) {
    match what {
        Doing::Save { channel, ts, save } => {
            let message = app
                .workspaces
                .iter()
                .find(|w| w.info.team_id == team)
                .and_then(|w| w.find_message(&channel, &ts))
                .cloned();
            saved(app.views.team_mut(team), &channel, &ts, !save, message);
            let text = if save {
                tf(
                    "Could not save the message: {error}",
                    &[("error", &error.message())],
                )
            } else {
                tf(
                    "Could not remove the message from Later: {error}",
                    &[("error", &error.message())],
                )
            };
            app.toast(text, true);
        }
        Doing::CompleteReminder { id } => {
            log::debug!("reminder {id} was not completed");
            app.toast(
                tf(
                    "Could not complete the reminder: {error}",
                    &[("error", &error.message())],
                ),
                true,
            );
            app.views.team_mut(team).reminders.start();
            send(app, team, Command::Reminders);
        }
        Doing::CancelScheduled => {
            app.toast(
                tf(
                    "Could not cancel the scheduled message: {error}",
                    &[("error", &error.message())],
                ),
                true,
            );
            app.views.team_mut(team).scheduled.start();
            send(app, team, Command::Scheduled);
        }
        Doing::Follow {
            channel,
            thread,
            follow,
        } => {
            show_followed(app, team, &channel, &thread, !follow);
            let error = error.message();
            let text = if follow {
                tf("Could not follow the thread: {error}", &[("error", &error)])
            } else {
                tf(
                    "Could not stop following the thread: {error}",
                    &[("error", &error)],
                )
            };
            app.toast(text, true);
        }
    }
}

/// Why a reminder was not set, in words. A sign-in through your own Slack
/// app made from a manifest older than reminders lacks `reminders:write`:
/// say which permission, as for bookmarks.
fn remind_failure(error: &Failure) -> String {
    if *error == Failure::MissingPermission {
        return t("Setting a reminder needs the reminders:write permission, which your Slack app does not have.").into_owned();
    }
    tf(
        "Could not set the reminder: {error}",
        &[("error", &error.message())],
    )
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
    fn only_browser_sessions_follow_threads() {
        use crate::model::SignInKind;
        assert!(can_follow(SignInKind::Session));
        assert!(!can_follow(SignInKind::App));
    }

    #[test]
    fn following_sends_the_web_clients_fields() {
        let thread = Ts::new("1700000000.000100");
        let read = Ts::new("1700000000.000300");
        let fields = vec![
            ("channel", "C1".to_owned()),
            ("thread_ts", "1700000000.000100".to_owned()),
            ("last_read", "1700000000.000300".to_owned()),
        ];
        assert_eq!(
            follow_request("C1", &thread, &read, true),
            ("subscriptions.thread.add", fields.clone())
        );
        assert_eq!(
            follow_request("C1", &thread, &read, false),
            ("subscriptions.thread.remove", fields)
        );
    }

    #[test]
    fn a_reminder_refused_for_want_of_its_permission_names_it() {
        let missing = remind_failure(&Failure::MissingPermission);
        assert!(missing.contains("reminders:write"), "{missing}");
        let other = remind_failure(&Failure::ConversationGone);
        assert!(!other.contains("reminders:write"), "{other}");
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
            service: crate::model::Service::Slack,
            team_id: "T1".into(),
            name: "Acme".into(),
            domain: "acme".into(),
            icon: None,
            user_id: "U0".into(),
            sign_in: Default::default(),
            scopes: None,
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
            external: false,
            is_open: None,
            empty: false,
        }
    }

    #[test]
    fn unreads_list_what_is_new_newest_first() {
        let mut workspace = workspace();
        *workspace.conversations = vec![
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
    fn saving_shows_at_once_and_undoes_cleanly() {
        let mut views = TeamViews::default();
        let message = bare_message(Ts::new("1.0"), None, "keep".into(), None);
        saved(
            &mut views,
            "C1",
            &Ts::new("1.0"),
            true,
            Some(message.clone()),
        );
        assert!(views.saved_keys.contains(&("C1".into(), Ts::new("1.0"))));
        assert!(
            views.saved.value.is_none(),
            "a list never read stays unread"
        );
        views.saved.arrived(Ok(Vec::new()));
        saved(&mut views, "C1", &Ts::new("1.0"), true, Some(message));
        saved(&mut views, "C1", &Ts::new("1.0"), true, None);
        assert_eq!(views.saved.value.as_ref().map(Vec::len), Some(0));
        saved(
            &mut views,
            "C1",
            &Ts::new("2.0"),
            true,
            Some(bare_message(Ts::new("2.0"), None, String::new(), None)),
        );
        saved(&mut views, "C1", &Ts::new("2.0"), false, None);
        assert!(!views.saved_keys.contains(&("C1".into(), Ts::new("2.0"))));
        assert_eq!(views.saved.value.as_ref().map(Vec::len), Some(0));
    }

    #[test]
    fn a_failed_fetch_keeps_what_was_there() {
        let mut fetch = Fetch::default();
        fetch.start();
        assert!(fetch.waiting());
        fetch.arrived(Ok(vec![1]));
        fetch.start();
        assert!(!fetch.waiting(), "the old list stays on screen");
        fetch.arrived(Err(Failure::RateLimited));
        assert_eq!(fetch.value, Some(vec![1]));
        assert_eq!(fetch.error, Some(Failure::RateLimited));
    }
}
