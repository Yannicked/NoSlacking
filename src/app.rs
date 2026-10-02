//! The application state: what is signed in, what is on screen, and how
//! events from the worker and actions from the views change it.
//!
//! Views read [`App`] and push [`Action`]s; [`App::frame_ui`] applies them
//! after drawing, so a frame never sees half an update.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use fastframe_shell::{Closed, Headless};

use crate::backend::{self, Backend, Change, Command, Event, SignIn, Socket, Source, Waker};
use crate::credentials::AppCredentials;
use crate::emoji::EmojiSet;
use crate::i18n::{self, t, tf};
use crate::model::{
    Action, Bot, Conversation, ConversationKind, Delivery, Message, SidebarSection, Timeline, Ts,
    User, Workspace,
};
use crate::mrkdwn;
use crate::paths::AppDirs;
use crate::settings::{Appearance, Settings, WorkspaceMeta};
use crate::theme::{self, Catalog, Palette};

/// How long a toast stays.
const TOAST_FOR: Duration = Duration::from_secs(5);
/// Read markers are sent at most this often per conversation.
const MARK_EVERY: Duration = Duration::from_secs(3);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Page {
    Main,
    /// Adding a workspace, or first run.
    SignIn,
    Settings,
}

pub struct Toast {
    pub text: String,
    pub error: bool,
    pub until: Instant,
}

/// What the emoji picker adds to.
#[derive(Clone, Debug, PartialEq)]
pub enum PickerTarget {
    Reaction { channel: String, ts: Ts },
    Draft(String),
}

/// A message being edited in place.
#[derive(Clone, Debug)]
pub struct Editing {
    pub channel: String,
    pub ts: Ts,
    pub text: String,
    /// The mentions and links in `text`, as for [`Draft::mentions`].
    pub mentions: Vec<(String, String)>,
    /// Whether the field is in the thread panel, which can show the same
    /// message (a thread's parent) as the conversation.
    pub in_thread: bool,
    /// Focus the field when it is next drawn. Only once, so you can click
    /// or tab away from it.
    pub focus: bool,
}

/// A message picked with the keyboard, whose actions its letter keys run.
#[derive(Clone, Debug, PartialEq)]
pub struct Selected {
    pub channel: String,
    pub ts: Ts,
    /// Whether it is picked in the thread panel rather than the
    /// conversation, which can both show a thread's parent.
    pub in_thread: bool,
    /// Bring it into view and give it focus when it is next drawn: the
    /// selection has just moved.
    pub reveal: bool,
}

/// An unsent message.
#[derive(Clone, Debug, Default)]
pub struct Draft {
    pub text: String,
    /// Mentions picked from the suggestions: the text inserted and the
    /// markup it stands for (`<@U123>`), as [`to_wire`] reads them.
    pub mentions: Vec<(String, String)>,
    pub broadcast: bool,
    pub selected: usize,
    /// The word (its start, in chars, and text) whose suggestions Esc
    /// closed. They stay closed until the word changes, so Enter sends.
    pub dismissed: Option<(usize, String)>,
    /// Whether suggestions were showing when last drawn, so Esc closes
    /// them and not the thread.
    pub suggesting: bool,
}

/// The "name this section" dialog.
#[derive(Clone, Debug, Default)]
pub struct SectionDialog {
    /// The section being renamed; `None` makes a new one.
    pub rename: Option<String>,
    /// A conversation to move into the new section.
    pub channel: Option<String>,
    pub name: String,
}

/// The fields of the sign-in page.
#[derive(Clone, Default)]
pub struct SetupForm {
    pub client_id: String,
    pub client_secret: String,
    pub app_token: String,
    pub user_token: String,
    pub show_manual: bool,
    /// Session sign-in: the workspace address and the `d` cookie.
    pub session_workspace: String,
    pub session_cookie: String,
    /// Browser sign-in: the `slack://` link Slack's page hands over.
    pub session_link: String,
    /// Whether the "use your own Slack app" section is expanded.
    pub show_app: bool,
}

/// The form holds secrets as they are typed; only the plain fields print.
impl std::fmt::Debug for SetupForm {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SetupForm")
            .field("client_id", &self.client_id)
            .field("session_workspace", &self.session_workspace)
            .field("show_manual", &self.show_manual)
            .field("show_app", &self.show_app)
            .finish_non_exhaustive()
    }
}

/// One signed-in workspace and everything loaded for it.
pub struct WorkspaceState {
    pub info: Workspace,
    pub conversations: Vec<Conversation>,
    pub users: HashMap<String, User>,
    /// Apps and integrations, by `bot_id`.
    pub bots: HashMap<String, Bot>,
    /// Your Slack sidebar sections, when Slack shares them (sessions).
    pub sections: Option<Vec<SidebarSection>>,
    pub emoji: EmojiSet,
    pub timelines: HashMap<String, Timeline>,
    pub threads: HashMap<(String, Ts), Timeline>,
    pub active: Option<String>,
    /// Why this workspace needs signing in again, if it does.
    pub signed_out: Option<String>,
    pub loaded: bool,
    requested_users: HashSet<String>,
    requested_bots: HashSet<String>,
    requested_conversations: HashSet<String>,
}

impl WorkspaceState {
    /// A workspace with nothing loaded yet.
    pub(crate) fn new(info: Workspace) -> Self {
        Self {
            info,
            conversations: Vec::new(),
            users: HashMap::new(),
            bots: HashMap::new(),
            sections: None,
            emoji: EmojiSet::default(),
            timelines: HashMap::new(),
            threads: HashMap::new(),
            active: None,
            signed_out: None,
            loaded: false,
            requested_users: HashSet::new(),
            requested_bots: HashSet::new(),
            requested_conversations: HashSet::new(),
        }
    }

    pub fn conversation(&self, id: &str) -> Option<&Conversation> {
        self.conversations.iter().find(|c| c.id == id)
    }

    pub fn conversation_mut(&mut self, id: &str) -> Option<&mut Conversation> {
        self.conversations.iter_mut().find(|c| c.id == id)
    }

    /// Every list a message of `channel` can be in: the conversation's own
    /// and its loaded threads, since a thread's parent and a reply also
    /// sent to the channel show in both.
    pub fn timelines_for<'s, 'c>(
        &'s self,
        channel: &'c str,
    ) -> impl Iterator<Item = &'s Timeline> + use<'s, 'c> {
        self.timelines.get(channel).into_iter().chain(
            self.threads
                .iter()
                .filter(move |((c, _), _)| c == channel)
                .map(|(_, t)| t),
        )
    }

    /// [`Self::timelines_for`], to change them.
    pub fn timelines_for_mut<'s, 'c>(
        &'s mut self,
        channel: &'c str,
    ) -> impl Iterator<Item = &'s mut Timeline> + use<'s, 'c> {
        self.timelines.get_mut(channel).into_iter().chain(
            self.threads
                .iter_mut()
                .filter(move |((c, _), _)| c == channel)
                .map(|(_, t)| t),
        )
    }

    /// A message of `channel`, wherever it is loaded.
    pub fn find_message(&self, channel: &str, ts: &Ts) -> Option<&Message> {
        self.timelines_for(channel)
            .find_map(|t| t.messages.iter().find(|m| m.ts == *ts))
    }

    pub fn user(&self, id: &str) -> Option<&User> {
        self.users.get(id)
    }

    /// A person's name, or their id until it is known.
    pub fn user_label(&self, id: &str) -> String {
        self.users
            .get(id)
            .map_or_else(|| id.to_owned(), |u| u.label().to_owned())
    }

    /// What the sidebar and header call a conversation.
    pub fn title(&self, conversation: &Conversation) -> String {
        match conversation.kind {
            ConversationKind::Direct => conversation
                .user
                .as_deref()
                .map_or_else(|| conversation.name.clone(), |id| self.user_label(id)),
            _ => conversation.name.clone(),
        }
    }

    /// The label for a message's author: the name a bot posted under, the
    /// person, or the app behind the `bot_id`.
    pub fn author(&self, message: &Message) -> String {
        if let Some(name) = message.username.as_ref().filter(|n| !n.is_empty()) {
            return name.clone();
        }
        if let Some(user) = message.user.as_deref().and_then(|id| self.users.get(id)) {
            return user.label().to_owned();
        }
        if let Some(bot) = message
            .bot_id
            .as_deref()
            .and_then(|id| self.bots.get(id))
            .filter(|b| !b.name.is_empty())
        {
            return bot.name.clone();
        }
        match (&message.user, &message.bot_id) {
            (Some(id), _) => id.clone(),
            (None, Some(_)) => t("App").into_owned(),
            (None, None) => t("Unknown").into_owned(),
        }
    }

    /// The picture for a message's author.
    pub fn author_icon<'a>(&'a self, message: &'a Message) -> Option<&'a str> {
        message
            .bot_icon
            .as_deref()
            .or_else(|| {
                message
                    .user
                    .as_deref()
                    .and_then(|id| self.users.get(id))
                    .and_then(|u| u.avatar.as_deref())
            })
            .or_else(|| {
                message
                    .bot_id
                    .as_deref()
                    .and_then(|id| self.bots.get(id))
                    .and_then(|b| b.icon.as_deref())
            })
    }

    fn unknown_bots<'a>(&self, messages: impl Iterator<Item = &'a Message>) -> Vec<String> {
        let mut out: Vec<String> = messages
            .filter_map(|m| m.bot_id.as_deref())
            .filter(|id| !id.is_empty() && !self.bots.contains_key(*id))
            .filter(|id| !self.requested_bots.contains(*id))
            .map(str::to_owned)
            .collect();
        out.sort();
        out.dedup();
        out
    }

    /// Whether a message mentions you (or everyone).
    pub fn mentions_me(&self, message: &Message) -> bool {
        let me = format!("<@{}", self.info.user_id);
        message.text.contains(&me)
            || message.text.contains("<!here")
            || message.text.contains("<!channel")
            || message.text.contains("<!everyone")
    }

    /// A message's text ready to edit, with people and channels named as
    /// you would type them. See [`to_editable`].
    pub fn editable(&self, wire: &str) -> (String, Vec<(String, String)>) {
        to_editable(wire, |sigil, id| match sigil {
            '@' => self.users.get(id).map(|u| u.label().to_owned()),
            _ => self.conversation(id).map(|c| self.title(c)),
        })
    }

    /// Takes a deleted message out of the conversation and its threads.
    /// A reply lowers its parent's count; a parent takes its thread along.
    fn remove_message(&mut self, channel: &str, ts: &Ts) {
        let parent = self
            .threads
            .iter()
            .find(|((c, parent), t)| {
                c == channel && parent != ts && t.messages.iter().any(|m| m.ts == *ts)
            })
            .map(|((_, parent), _)| parent.clone())
            .or_else(|| {
                let timeline = self.timelines.get(channel)?;
                let message = timeline.messages.iter().find(|m| m.ts == *ts)?;
                message.thread_ts.clone().filter(|_| message.is_reply())
            });
        self.threads.remove(&(channel.to_owned(), ts.clone()));
        for timeline in self.timelines_for_mut(channel) {
            timeline.remove(ts);
        }
        // Optimistic replies were never counted.
        let Some(parent) = parent.filter(|_| !ts.is_local()) else {
            return;
        };
        let thread = self.threads.get_mut(&(channel.to_owned(), parent.clone()));
        let copies = self
            .timelines
            .get_mut(channel)
            .and_then(|t| t.find_mut(&parent))
            .into_iter()
            .chain(thread.and_then(|t| t.find_mut(&parent)));
        for message in copies {
            message.reply_count = message.reply_count.saturating_sub(1);
        }
    }

    fn unknown_users<'a>(&self, ids: impl Iterator<Item = &'a str>) -> Vec<String> {
        let mut out: Vec<String> = ids
            .filter(|id| !id.is_empty() && !self.users.contains_key(*id))
            .filter(|id| !self.requested_users.contains(*id))
            .map(str::to_owned)
            .collect();
        out.sort();
        out.dedup();
        out
    }
}

/// Where a file goes: the team, the channel and the thread, if any.
type UploadTarget = (String, String, Option<Ts>);

/// What a change from the worker leaves for [`App`] to do: the people and
/// apps it named that are not known yet.
#[derive(Debug, Default, PartialEq)]
struct Arrived {
    users: Vec<String>,
    bots: Vec<String>,
}

// What the worker's events and your own actions change in one workspace.
// Kept apart from `App` and free of side effects (no commands, toasts or
// scrolling), so they can be tested without a backend.
impl WorkspaceState {
    /// The full (or a cached) list of conversations. Returns the people
    /// to fetch for DMs.
    fn conversations_arrived(&mut self, list: Vec<Conversation>, complete: bool) -> Vec<String> {
        let mut merged = Vec::with_capacity(list.len());
        for mut conversation in list {
            if let Some(existing) = self.conversation(&conversation.id) {
                let mut kept = existing.clone();
                merge_conversation(&mut kept, conversation);
                conversation = kept;
            }
            merged.push(conversation);
        }
        if !complete {
            // A cached list: keep anything already known that it lacks.
            for existing in &self.conversations {
                if !merged.iter().any(|c| c.id == existing.id) {
                    merged.push(existing.clone());
                }
            }
        }
        self.conversations = merged;
        self.loaded = self.loaded || complete;
        let users = self.unknown_users(self.conversations.iter().filter_map(|c| c.user.as_deref()));
        let needs_open = match &self.active {
            Some(id) => self.conversation(id).is_none() && complete,
            None => true,
        };
        if needs_open {
            self.active = self
                .conversations
                .iter()
                .filter(|c| !c.kind.is_dm())
                .min_by_key(|c| (c.name != "general", c.name.clone()))
                .or_else(|| self.conversations.first())
                .map(|c| c.id.clone());
        }
        users
    }

    /// Fresh details of one conversation. Returns the person to fetch, for
    /// a DM with someone not known yet.
    fn conversation_arrived(&mut self, conversation: Conversation) -> Vec<String> {
        let fetch = self.unknown_users(conversation.user.as_deref().into_iter());
        match self.conversation_mut(&conversation.id) {
            Some(existing) => merge_conversation(existing, conversation),
            None => self.conversations.push(conversation),
        }
        fetch
    }

    /// You left a conversation, or it was archived or deleted.
    fn conversation_gone(&mut self, channel: &str) {
        self.conversations.retain(|c| c.id != channel);
        self.timelines.remove(channel);
        self.threads.retain(|(c, _), _| c != channel);
        if self.active.as_deref() == Some(channel) {
            self.active = None;
        }
    }

    fn users_arrived(&mut self, users: Vec<User>) {
        for user in users {
            self.requested_users.remove(&user.id);
            self.users.insert(user.id.clone(), user);
        }
    }

    fn bots_arrived(&mut self, bots: Vec<Bot>) {
        for bot in bots {
            self.requested_bots.remove(&bot.id);
            self.bots.insert(bot.id.clone(), bot);
        }
    }

    /// A page of a conversation's history. Returns whom to fetch, and
    /// whether this was the first page.
    fn history_arrived(
        &mut self,
        channel: &str,
        messages: Vec<Message>,
        has_more: bool,
        cursor: Option<String>,
        older: bool,
    ) -> (Arrived, bool) {
        let arrived = Arrived {
            users: self.unknown_users(messages.iter().flat_map(|m| {
                m.user
                    .as_deref()
                    .into_iter()
                    .chain(m.reply_users.iter().map(String::as_str))
            })),
            bots: self.unknown_bots(messages.iter()),
        };
        let newest = messages.iter().map(|m| m.ts.clone()).max();
        let timeline = self.timelines.entry(channel.to_owned()).or_default();
        let first = !timeline.loaded;
        timeline.merge(messages);
        timeline.loading = false;
        if older || first {
            timeline.has_more = has_more;
            timeline.cursor = cursor;
        }
        timeline.loaded = true;
        if let (Some(newest), Some(conversation)) = (newest, self.conversation_mut(channel))
            && conversation.latest.as_ref().is_none_or(|l| *l < newest)
        {
            conversation.latest = Some(newest);
        }
        (arrived, first)
    }

    fn history_failed(&mut self, channel: &str) {
        if let Some(timeline) = self.timelines.get_mut(channel) {
            timeline.loading = false;
        }
    }

    /// A whole thread, parent first.
    fn thread_arrived(&mut self, channel: &str, ts: Ts, messages: Vec<Message>) -> Arrived {
        let arrived = Arrived {
            users: self.unknown_users(messages.iter().filter_map(|m| m.user.as_deref())),
            bots: self.unknown_bots(messages.iter()),
        };
        let replies = messages.iter().filter(|m| m.ts != ts).count() as u32;
        if let Some(parent) = self
            .timelines
            .get_mut(channel)
            .and_then(|t| t.find_mut(&ts))
        {
            parent.reply_count = parent.reply_count.max(replies);
        }
        let timeline = self.threads.entry((channel.to_owned(), ts)).or_default();
        timeline.loading = false;
        timeline.loaded = true;
        // Keep replies still being sent.
        let local: Vec<Message> = timeline
            .messages
            .iter()
            .filter(|m| m.ts.is_local())
            .cloned()
            .collect();
        timeline.messages = messages;
        for message in local {
            timeline.upsert(message);
        }
        arrived
    }

    /// A new or changed message, live. `viewing` says whether you are
    /// looking at its conversation, which then gains no unread mention.
    /// Returns whom to fetch, and whether the conversation is unknown and
    /// should be fetched first.
    fn message_arrived(
        &mut self,
        channel: &str,
        message: Message,
        viewing: bool,
    ) -> (Arrived, bool) {
        let mut arrived = Arrived {
            users: self.unknown_users(message.user.as_deref().into_iter()),
            bots: self.unknown_bots(std::iter::once(&message)),
        };
        let known = self.conversation(channel).is_some();
        let from_me = message.user.as_deref() == Some(self.info.user_id.as_str());
        let mentions_me = self.mentions_me(&message);
        if message.is_reply() {
            let parent_ts = message.thread_ts.clone().unwrap_or_default();
            let key = (channel.to_owned(), parent_ts);
            let already = self
                .threads
                .get(&key)
                .is_some_and(|t| t.messages.iter().any(|m| m.ts == message.ts));
            if !already
                && let Some(parent) = self
                    .timelines
                    .get_mut(channel)
                    .and_then(|t| t.find_mut(&key.1))
            {
                parent.reply_count += 1;
                parent.latest_reply = Some(message.ts.clone());
                if let Some(user) = &message.user
                    && !parent.reply_users.contains(user)
                {
                    parent.reply_users.push(user.clone());
                }
            }
            if let Some(thread) = self.threads.get_mut(&key) {
                remove_echoed_local(thread, &message, from_me);
                thread.upsert(message.clone());
            }
        }
        if message.in_channel() {
            let timeline = self.timelines.entry(channel.to_owned()).or_default();
            remove_echoed_local(timeline, &message, from_me);
            let new = timeline.find_mut(&message.ts).is_none();
            let ts = message.ts.clone();
            timeline.upsert(message);
            if new && let Some(conversation) = self.conversation_mut(channel) {
                if conversation.latest.as_ref().is_none_or(|l| *l < ts) {
                    conversation.latest = Some(ts.clone());
                }
                if from_me {
                    conversation.last_read = Some(ts);
                    conversation.mentions = 0;
                } else if !viewing && (mentions_me || conversation.kind.is_dm()) {
                    conversation.mentions += 1;
                }
            }
        }
        let fetch_conversation = !known && self.requested_conversations.insert(channel.to_owned());
        if !known {
            // The conversation's details name its people; wait for them.
            arrived.users.clear();
        }
        (arrived, fetch_conversation)
    }

    /// A new copy of a message already sent: an edit, or a thread
    /// parent's new reply details. It replaces the copies that are loaded
    /// and nothing else: no counts change, and a message outside the
    /// loaded history is not pulled in. Returns whom to fetch.
    fn message_changed(&mut self, channel: &str, message: Message) -> Arrived {
        let mut loaded = false;
        for timeline in self.timelines_for_mut(channel) {
            if timeline.find_mut(&message.ts).is_some() {
                timeline.upsert(message.clone());
                loaded = true;
            }
        }
        if !loaded {
            return Arrived::default();
        }
        Arrived {
            users: self.unknown_users(message.user.as_deref().into_iter()),
            bots: self.unknown_bots(std::iter::once(&message)),
        }
    }

    /// Slack answered a send: the optimistic copy `local` gives way to the
    /// real message, or is marked as failed.
    fn sent(&mut self, channel: &str, local: &Ts, result: &Result<Message, String>) {
        for timeline in self.timelines_for_mut(channel) {
            let Some(position) = timeline.messages.iter().position(|m| &m.ts == local) else {
                continue;
            };
            match result {
                Ok(message) => {
                    timeline.messages.remove(position);
                    // The echo from Socket Mode may already be there.
                    if timeline.find_mut(&message.ts).is_none() {
                        timeline.upsert(message.clone());
                    }
                }
                Err(error) => {
                    timeline.messages[position].delivery = Delivery::Failed(error.clone());
                }
            }
        }
        if let Ok(message) = result
            && message.in_channel()
            && let Some(conversation) = self.conversation_mut(channel)
        {
            conversation.latest = max_ts(conversation.latest.take(), Some(message.ts.clone()));
            conversation.last_read =
                max_ts(conversation.last_read.take(), Some(message.ts.clone()));
        }
    }

    /// Someone reacted, or took a reaction back.
    fn reaction_changed(&mut self, channel: &str, ts: &Ts, name: &str, user: &str, added: bool) {
        for timeline in self.timelines_for_mut(channel) {
            if let Some(message) = timeline.find_mut(ts) {
                message.toggle_reaction(name, user, added);
            }
        }
    }

    /// You read up to `ts`, maybe on another device.
    fn read_elsewhere(&mut self, channel: &str, ts: Ts) {
        if let Some(conversation) = self.conversation_mut(channel) {
            read_up_to(conversation, ts);
        }
    }

    /// Shows a message you are sending before Slack has it.
    fn add_local(&mut self, channel: &str, message: Message) {
        let timeline = match &message.thread_ts {
            Some(parent) => self
                .threads
                .entry((channel.to_owned(), parent.clone()))
                .or_default(),
            None => self.timelines.entry(channel.to_owned()).or_default(),
        };
        timeline.upsert(message);
    }

    /// Shows your edit at once. Returns the message as it was, to put
    /// back if Slack refuses the edit.
    fn edit_locally(&mut self, channel: &str, ts: &Ts, wire: &str) -> Option<Message> {
        let mut before = None;
        for timeline in self.timelines_for_mut(channel) {
            if let Some(message) = timeline.find_mut(ts) {
                before.get_or_insert_with(|| message.clone());
                message.text = wire.to_owned();
                message.edited = true;
            }
        }
        before
    }

    /// Takes back a change Slack refused: the text before your edit, the
    /// message you deleted, or your reaction toggle.
    fn undo(&mut self, channel: &str, change: Change) {
        match change {
            Change::Edit { ts, text, before } => {
                let Some(before) = before else { return };
                for timeline in self.timelines_for_mut(channel) {
                    // Only while it still shows this edit: a later edit
                    // or Slack's own copy wins.
                    if let Some(message) = timeline.find_mut(&ts)
                        && message.text == text
                    {
                        message.text.clone_from(&before.text);
                        message.edited = before.edited;
                    }
                }
            }
            Change::Delete { removed, .. } => {
                if let Some(message) = removed {
                    self.restore(channel, *message);
                }
            }
            Change::React { ts, name, added } => {
                let me = self.info.user_id.clone();
                self.reaction_changed(channel, &ts, &name, &me, !added);
            }
        }
    }

    /// Shows a deleted message again where it was, undoing
    /// [`Self::remove_message`]: a reply counts on its parent once more.
    fn restore(&mut self, channel: &str, message: Message) {
        // Slack may have sent it again already.
        if self.find_message(channel, &message.ts).is_some() {
            return;
        }
        if message.is_reply() {
            let parent = message.thread_ts.clone().unwrap_or_default();
            let key = (channel.to_owned(), parent.clone());
            if let Some(thread) = self.threads.get_mut(&key) {
                thread.upsert(message.clone());
            }
            let thread = self.threads.get_mut(&key);
            let copies = self
                .timelines
                .get_mut(channel)
                .and_then(|t| t.find_mut(&parent))
                .into_iter()
                .chain(thread.and_then(|t| t.find_mut(&parent)));
            for copy in copies {
                copy.reply_count += 1;
            }
        }
        if message.in_channel()
            && let Some(timeline) = self.timelines.get_mut(channel)
        {
            timeline.upsert(message);
        }
    }

    /// Adds your reaction, or takes it back if it is there. Returns
    /// whether it was added, or `None` if the message is not loaded.
    fn toggle_my_reaction(&mut self, channel: &str, ts: &Ts, name: &str) -> Option<bool> {
        let me = self.info.user_id.clone();
        let mut add = None;
        for timeline in self.timelines_for_mut(channel) {
            if let Some(message) = timeline.find_mut(ts) {
                let adding = *add.get_or_insert_with(|| {
                    !message
                        .reactions
                        .iter()
                        .any(|r| r.name == name && r.users.contains(&me))
                });
                message.toggle_reaction(name, &me, adding);
            }
        }
        add
    }

    /// Marks a failed message as sending again. Returns its text, thread
    /// and broadcast flag, to send once more.
    fn retry_local(&mut self, channel: &str, local: &Ts) -> Option<(String, Option<Ts>, bool)> {
        let mut found = None;
        for timeline in self.timelines_for_mut(channel) {
            if let Some(message) = timeline.find_mut(local) {
                message.delivery = Delivery::Sending;
                found = Some((
                    message.text.clone(),
                    message.thread_ts.clone(),
                    message.broadcast,
                ));
            }
        }
        found
    }

    /// Your newest message in the open conversation that can be edited.
    fn last_editable(&self) -> Option<(String, Ts)> {
        let channel = self.active.clone()?;
        let me = self.info.user_id.as_str();
        let ts = self
            .timelines
            .get(&channel)?
            .messages
            .iter()
            .rev()
            .find(|m| {
                m.user.as_deref() == Some(me) && m.delivery == Delivery::Sent && !m.is_system()
            })?
            .ts
            .clone();
        Some((channel, ts))
    }
}

/// The workspace on screen: the one chosen in `settings`, else the first.
/// Views that borrow [`App`]'s fields apart call this instead of
/// [`App::active_workspace`].
pub fn active_in<'a>(
    workspaces: &'a [WorkspaceState],
    settings: &Settings,
) -> Option<&'a WorkspaceState> {
    let id = settings.active_workspace.as_deref();
    workspaces
        .iter()
        .find(|w| Some(w.info.team_id.as_str()) == id)
        .or_else(|| workspaces.first())
}

/// A file chosen in the picker, and where it goes.
type PickedFile = (UploadTarget, PathBuf);

pub struct AppOptions {
    pub demo: bool,
}

pub struct App {
    pub dirs: AppDirs,
    pub settings: Settings,
    pub backend: Backend,
    pub waker: Waker,
    pub palette: Palette,
    applied: Option<(Palette, f32)>,
    pub catalog: Catalog,
    pub page: Page,
    pub app_credentials: Option<AppCredentials>,
    pub app_loaded: bool,
    pub keyring_error: Option<String>,
    pub workspaces: Vec<WorkspaceState>,
    pub sign_in: Option<SignIn>,
    pub setup: SetupForm,
    pub socket: Socket,
    pub thread: Option<(String, Ts)>,
    pub drafts: HashMap<String, Draft>,
    pub editing: Option<Editing>,
    pub selected: Option<Selected>,
    pub toasts: Vec<Toast>,
    pub actions: Vec<Action>,
    pub switcher: Option<(String, usize)>,
    pub profile: Option<String>,
    pub picker: Option<PickerTarget>,
    pub picker_query: String,
    pub preview: Option<(String, String)>,
    /// A message waiting for "Delete?" to be answered.
    pub confirm_delete: Option<(String, Ts)>,
    pub section_dialog: Option<SectionDialog>,
    /// Where the "New" line goes: the read marker when the open
    /// conversation was opened, by `team/channel`.
    pub read_line: Option<(String, Option<Ts>)>,
    pub sidebar_filter: String,
    pub demo: bool,
    /// Older history arrived: the list keeps its place by this much.
    pub prepended: Option<(String, f32)>,
    /// Lists to scroll to the bottom when next drawn, by
    /// [`App::draft_key`]: a reply sent in a thread must not move the
    /// conversation beside it.
    pub scroll_to_bottom: HashSet<String>,
    /// Focus the composer next frame.
    pub focus_composer: bool,
    /// Focus the field of the dialog or picker just opened, once: asking
    /// every frame would keep Tab from reaching its buttons.
    pub focus_overlay: bool,
    local_counter: u64,
    uploads: (mpsc::Sender<PickedFile>, mpsc::Receiver<PickedFile>),
    marks: HashMap<(String, String), (Ts, Instant)>,
    pending_marks: HashMap<(String, String), Ts>,
    window_focused: bool,
    settings_dirty: bool,
    quit: bool,
}

impl App {
    pub fn new(waker: &Waker, dirs: AppDirs, settings: Settings, options: AppOptions) -> Self {
        let locale = settings.language.unwrap_or_else(i18n::Locale::detect);
        i18n::set_locale(locale);
        let source = if options.demo {
            #[cfg(feature = "demo")]
            {
                Source::Demo
            }
            #[cfg(not(feature = "demo"))]
            {
                Source::Slack {
                    dirs: dirs.clone(),
                    workspaces: Vec::new(),
                    credentials_in_memory: true,
                }
            }
        } else {
            Source::Slack {
                dirs: dirs.clone(),
                workspaces: settings.workspaces.clone(),
                credentials_in_memory: false,
            }
        };
        let backend = backend::spawn(waker, source, dirs.images());
        let mut catalog = Catalog::default();
        if !options.demo {
            theme::enable_desktop_themes(&mut catalog);
        }
        let palette = match (&settings.appearance, &settings.cached_theme) {
            (Appearance::Custom(name), Some(cached)) if cached.filename == *name => cached.palette,
            (Appearance::Light, _) => Palette::light(),
            _ => Palette::dark(),
        };
        let mut workspaces = Vec::new();
        for meta in &settings.workspaces {
            let mut state = WorkspaceState::new(Workspace {
                team_id: meta.team_id.clone(),
                name: meta.name.clone(),
                domain: meta.domain.clone(),
                icon: meta.icon.clone(),
                user_id: meta.user_id.clone(),
            });
            state.active = settings.last_conversation.get(&meta.team_id).cloned();
            workspaces.push(state);
        }
        let page = if workspaces.is_empty() && !options.demo {
            Page::SignIn
        } else {
            Page::Main
        };
        let mut app = Self {
            dirs,
            settings,
            backend,
            waker: waker.clone(),
            palette,
            applied: None,
            catalog,
            page,
            app_credentials: None,
            app_loaded: false,
            keyring_error: None,
            workspaces: if options.demo { Vec::new() } else { workspaces },
            sign_in: None,
            setup: SetupForm::default(),
            socket: Socket::Off,
            thread: None,
            drafts: HashMap::new(),
            editing: None,
            selected: None,
            toasts: Vec::new(),
            actions: Vec::new(),
            switcher: None,
            profile: None,
            picker: None,
            picker_query: String::new(),
            preview: None,
            confirm_delete: None,
            section_dialog: None,
            read_line: None,
            sidebar_filter: String::new(),
            demo: options.demo,
            prepended: None,
            scroll_to_bottom: HashSet::new(),
            focus_composer: true,
            focus_overlay: false,
            local_counter: 0,
            uploads: mpsc::channel(),
            marks: HashMap::new(),
            pending_marks: HashMap::new(),
            window_focused: true,
            settings_dirty: false,
            quit: false,
        };
        app.start_theme_scan();
        app
    }

    fn start_theme_scan(&mut self) {
        if self.demo {
            return;
        }
        let selected = match &self.settings.appearance {
            Appearance::Custom(name) => Some(name.clone()),
            _ => None,
        };
        let waker = self.waker.clone();
        self.catalog.start(
            self.dirs.themes(),
            selected,
            &fastframe_theme::Waker::new(move || waker.wake()),
        );
    }

    /// Called once the window's egui context exists.
    pub fn attach(&mut self, ctx: &egui::Context) {
        self.waker.attach(ctx);
        theme::install(ctx);
        ctx.add_bytes_loader(std::sync::Arc::new(self.backend.images.clone()));
        #[cfg(feature = "demo")]
        if self.demo {
            ctx.include_bytes(crate::demo::PICTURE, crate::demo::PICTURE_BYTES);
            ctx.include_bytes(crate::demo::PARROT, crate::demo::PARROT_BYTES);
            ctx.add_bytes_loader(std::sync::Arc::new(crate::demo::SlowImages::default()));
        }
        self.applied = None;
    }

    pub fn active_workspace(&self) -> Option<&WorkspaceState> {
        active_in(&self.workspaces, &self.settings)
    }

    pub fn active_workspace_mut(&mut self) -> Option<&mut WorkspaceState> {
        let id = self.settings.active_workspace.clone();
        let index = self
            .workspaces
            .iter()
            .position(|w| Some(&w.info.team_id) == id.as_ref())
            .or(if self.workspaces.is_empty() {
                None
            } else {
                Some(0)
            })?;
        self.workspaces.get_mut(index)
    }

    fn workspace_mut(&mut self, team: &str) -> Option<&mut WorkspaceState> {
        self.workspaces.iter_mut().find(|w| w.info.team_id == team)
    }

    pub fn active_team(&self) -> Option<String> {
        self.active_workspace().map(|w| w.info.team_id.clone())
    }

    pub fn active_conversation(&self) -> Option<(&WorkspaceState, &Conversation)> {
        let workspace = self.active_workspace()?;
        let id = workspace.active.as_deref()?;
        Some((workspace, workspace.conversation(id)?))
    }

    /// The key a draft is stored under.
    pub fn draft_key(team: &str, channel: &str, thread: Option<&Ts>) -> String {
        match thread {
            Some(ts) => format!("{team}/{channel}/{}", ts.as_str()),
            None => format!("{team}/{channel}"),
        }
    }

    /// The drafts of the composers on screen: the conversation's and the
    /// open thread's.
    pub fn visible_drafts(&self) -> Vec<String> {
        let Some(workspace) = self.active_workspace() else {
            return Vec::new();
        };
        let team = &workspace.info.team_id;
        let mut keys: Vec<String> = workspace
            .active
            .iter()
            .map(|channel| Self::draft_key(team, channel, None))
            .collect();
        if let Some((channel, ts)) = &self.thread {
            keys.push(Self::draft_key(team, channel, Some(ts)));
        }
        keys
    }

    pub fn toast(&mut self, text: impl Into<String>, error: bool) {
        let text = text.into();
        if error {
            log::warn!("{text}");
        }
        self.toasts.retain(|t| t.text != text);
        self.toasts.push(Toast {
            text,
            error,
            until: Instant::now() + TOAST_FOR,
        });
        self.waker.wake_after(TOAST_FOR);
    }

    fn save_settings(&mut self) {
        self.settings_dirty = true;
    }

    pub fn settings_changed(&mut self) {
        self.save_settings();
    }

    // ---- per-frame work -------------------------------------------------

    /// Everything that must happen whether or not a window is open.
    pub fn background_frame(&mut self, ctx: &egui::Context) {
        while let Some(event) = self.backend.try_recv() {
            let signed_out = matches!(event, Event::SignedOut { reason: None, .. });
            self.handle(event);
            if signed_out {
                // egui holds decoded copies of the workspace's private files.
                ctx.forget_all_images();
            }
        }
        while let Ok(((team, channel, thread), path)) = self.uploads.1.try_recv() {
            self.backend.send(Command::Upload {
                team,
                channel,
                thread,
                path,
                comment: String::new(),
            });
        }
        if self.catalog.poll() {
            self.refresh_custom_theme();
        }
        if self.catalog.needs_reload() {
            self.start_theme_scan();
        }
        self.flush_marks();
        let now = Instant::now();
        self.toasts.retain(|t| t.until > now);
        if self.settings_dirty {
            self.settings_dirty = false;
            self.settings.save(&self.dirs.settings_file());
        }
    }

    pub fn frame_ui(&mut self, ui: &mut egui::Ui) {
        let ctx = ui.ctx().clone();
        self.apply_theme(&ctx);
        let focused = ctx.input(|i| i.viewport().focused.unwrap_or(true));
        if focused && !self.window_focused {
            self.mark_active_read();
        }
        self.window_focused = focused;
        crate::ui::show(self, ui);
        // Applying an action may queue another (editing the last message).
        for _ in 0..4 {
            let actions = std::mem::take(&mut self.actions);
            if actions.is_empty() {
                break;
            }
            for action in actions {
                self.apply(action, &ctx);
            }
        }
    }

    fn apply_theme(&mut self, ctx: &egui::Context) {
        let palette = match &self.settings.appearance {
            Appearance::System => match ctx.system_theme() {
                Some(egui::Theme::Light) => Palette::light(),
                _ => Palette::dark(),
            },
            Appearance::Dark => Palette::dark(),
            Appearance::Light => Palette::light(),
            Appearance::Custom(_) => self.palette,
        };
        self.palette = palette;
        let zoom = self.settings.zoom.clamp(0.6, 2.0);
        if self.applied != Some((palette, zoom)) {
            theme::apply(ctx, &palette);
            ctx.set_zoom_factor(zoom);
            self.applied = Some((palette, zoom));
        }
    }

    fn refresh_custom_theme(&mut self) {
        if let Appearance::Custom(name) = &self.settings.appearance
            && let Some(found) = self.catalog.find(name).cloned()
        {
            if self.settings.cached_theme.as_ref() != Some(&found) {
                self.settings.cached_theme = Some(found.clone());
                self.save_settings();
            }
            self.palette = found.palette;
        }
    }

    pub fn set_appearance(&mut self, appearance: Appearance) {
        self.settings.appearance = appearance;
        if let Appearance::Custom(name) = &self.settings.appearance {
            if let Some(found) = self.catalog.find(name).cloned() {
                self.palette = found.palette;
                self.settings.cached_theme = Some(found);
            }
        } else {
            self.settings.cached_theme = None;
        }
        self.save_settings();
    }

    pub fn set_language(&mut self, locale: i18n::Locale) {
        self.settings.language = Some(locale);
        i18n::set_locale(locale);
        self.save_settings();
    }

    // ---- events from the worker ----------------------------------------

    fn handle(&mut self, event: Event) {
        match event {
            // The account: the app, the keyring, sign-in and the socket.
            Event::AppLoaded(app) => self.app_loaded(app),
            Event::KeyringError(error) => {
                self.toast(tf("Keyring: {error}", &[("error", &error)]), true);
                self.keyring_error = Some(error);
            }
            Event::SignIn(state) => self.sign_in_changed(state),
            Event::WorkspaceReady(info) => self.workspace_ready(info),
            Event::SignedOut { team, reason } => self.signed_out(&team, reason),
            Event::Socket(socket) => self.socket_changed(socket),
            Event::Error(error) => self.toast(error, true),
            Event::Notice(text) => self.toast(text, false),
            // A workspace's conversations, people, apps and sidebar.
            Event::Conversations {
                team,
                list,
                complete,
            } => self.conversations(&team, list, complete),
            Event::Conversation { team, conversation } => {
                if let Some(workspace) = self.workspace_mut(&team) {
                    let users = workspace.conversation_arrived(conversation);
                    self.fetch_users(&team, users);
                }
            }
            Event::ConversationGone { team, channel } => self.conversation_gone(&team, &channel),
            Event::Users { team, users } => {
                if let Some(workspace) = self.workspace_mut(&team) {
                    workspace.users_arrived(users);
                }
            }
            Event::Bots { team, bots } => {
                if let Some(workspace) = self.workspace_mut(&team) {
                    workspace.bots_arrived(bots);
                }
            }
            Event::Sections { team, sections } => {
                if let Some(workspace) = self.workspace_mut(&team) {
                    workspace.sections = Some(sections);
                }
            }
            Event::Emoji { team, emoji } => {
                if let Some(workspace) = self.workspace_mut(&team) {
                    workspace.emoji = EmojiSet::new(emoji);
                }
            }
            // Messages and read state.
            Event::History {
                team,
                channel,
                messages,
                has_more,
                cursor,
                older,
            } => self.history(&team, &channel, messages, has_more, cursor, older),
            Event::HistoryFailed {
                team,
                channel,
                error,
            } => {
                if let Some(workspace) = self.workspace_mut(&team) {
                    workspace.history_failed(&channel);
                }
                self.toast(
                    tf("Could not load messages: {error}", &[("error", &error)]),
                    true,
                );
            }
            Event::Thread {
                team,
                channel,
                ts,
                messages,
            } => {
                if let Some(workspace) = self.workspace_mut(&team) {
                    let arrived = workspace.thread_arrived(&channel, ts, messages);
                    self.fetch_arrived(&team, arrived);
                }
            }
            Event::Message {
                team,
                channel,
                message,
                changed,
            } => self.message(&team, &channel, message, changed),
            Event::Deleted { team, channel, ts } => self.remove_message(&team, &channel, &ts),
            Event::Reaction {
                team,
                channel,
                ts,
                name,
                user,
                added,
            } => {
                if let Some(workspace) = self.workspace_mut(&team) {
                    workspace.reaction_changed(&channel, &ts, &name, &user, added);
                }
            }
            Event::Sent {
                team,
                channel,
                local,
                result,
            } => self.sent(&team, &channel, &local, result),
            Event::Read { team, channel, ts } => {
                if let Some(workspace) = self.workspace_mut(&team) {
                    workspace.read_elsewhere(&channel, ts);
                }
            }
            Event::Settled {
                team,
                channel,
                change,
                result,
            } => self.settled(&team, &channel, change, result),
        }
    }

    /// Slack answered an edit, delete or reaction; a refused one is
    /// undone on screen, and you are told.
    fn settled(&mut self, team: &str, channel: &str, change: Change, result: Result<(), String>) {
        let Err(error) = result else { return };
        let what = match &change {
            Change::Edit { .. } => t("Could not edit the message"),
            Change::Delete { .. } => t("Could not delete the message"),
            Change::React { .. } => t("Could not change the reaction"),
        };
        if let Some(workspace) = self.workspace_mut(team) {
            workspace.undo(channel, change);
        }
        self.toast(format!("{what}: {error}"), true);
    }

    fn app_loaded(&mut self, app: Option<AppCredentials>) {
        if let Some(app) = &app {
            self.setup.client_id = app.client_id.clone();
            self.setup.client_secret = app.client_secret.clone();
            self.setup.app_token = app.app_token.clone();
        }
        self.app_credentials = app;
        self.app_loaded = true;
    }

    fn sign_in_changed(&mut self, state: SignIn) {
        if let SignIn::Done(name) = &state {
            self.toast(tf("Signed in to {name}.", &[("name", name)]), false);
            self.page = Page::Main;
            self.setup.user_token.clear();
        }
        self.sign_in = Some(state);
    }

    fn workspace_ready(&mut self, info: Workspace) {
        let team = info.team_id.clone();
        self.settings.upsert_workspace(WorkspaceMeta {
            team_id: info.team_id.clone(),
            name: info.name.clone(),
            domain: info.domain.clone(),
            icon: info.icon.clone(),
            user_id: info.user_id.clone(),
        });
        match self.workspace_mut(&team) {
            Some(workspace) => {
                workspace.info = info;
                workspace.signed_out = None;
            }
            None => {
                let mut state = WorkspaceState::new(info);
                state.active = self.settings.last_conversation.get(&team).cloned();
                self.workspaces.push(state);
            }
        }
        if self.settings.active_workspace.is_none() {
            self.settings.active_workspace = Some(team);
        }
        self.save_settings();
    }

    /// A workspace needs signing in again (`reason`), or was signed out.
    fn signed_out(&mut self, team: &str, reason: Option<String>) {
        match reason {
            Some(reason) => {
                if let Some(workspace) = self.workspace_mut(team) {
                    workspace.signed_out = Some(reason);
                }
            }
            None => {
                self.workspaces.retain(|w| w.info.team_id != team);
                self.settings.remove_workspace(team);
                self.save_settings();
                if self.workspaces.is_empty() {
                    self.page = Page::SignIn;
                }
            }
        }
    }

    fn socket_changed(&mut self, socket: Socket) {
        if let Socket::Rejected(reason) = &socket {
            self.toast(
                tf(
                    "Slack refused the app-level token ({reason})",
                    &[("reason", reason)],
                ),
                true,
            );
        }
        self.socket = socket;
    }

    fn conversations(&mut self, team: &str, list: Vec<Conversation>, complete: bool) {
        let active_team = self.active_team();
        let Some(workspace) = self.workspace_mut(team) else {
            return;
        };
        let users = workspace.conversations_arrived(list, complete);
        let open = workspace.active.clone();
        self.fetch_users(team, users);
        if active_team.as_deref() == Some(team)
            && let Some(open) = open
        {
            self.ensure_loaded(team, &open);
        }
    }

    /// A conversation is gone, and with it any thread open from it.
    fn conversation_gone(&mut self, team: &str, channel: &str) {
        if let Some(workspace) = self.workspace_mut(team) {
            workspace.conversation_gone(channel);
        }
        if self.active_team().as_deref() == Some(team)
            && self.thread.as_ref().is_some_and(|(c, _)| c == channel)
        {
            self.thread = None;
        }
    }

    fn history(
        &mut self,
        team: &str,
        channel: &str,
        messages: Vec<Message>,
        has_more: bool,
        cursor: Option<String>,
        older: bool,
    ) {
        let Some(workspace) = self.workspace_mut(team) else {
            return;
        };
        let (arrived, first) =
            workspace.history_arrived(channel, messages, has_more, cursor, older);
        if older {
            self.prepended = Some((format!("{team}/{channel}"), 0.0));
        }
        if first {
            self.scroll_to_bottom
                .insert(Self::draft_key(team, channel, None));
        }
        self.fetch_arrived(team, arrived);
        if !older {
            self.mark_if_viewing(team, channel);
        }
    }

    fn message(&mut self, team: &str, channel: &str, message: Message, changed: bool) {
        let viewing = self.is_viewing(team, channel);
        let Some(workspace) = self.workspace_mut(team) else {
            return;
        };
        if changed {
            let arrived = workspace.message_changed(channel, message);
            self.fetch_arrived(team, arrived);
            if viewing {
                self.waker.wake();
            }
            return;
        }
        let from_me = message.user.as_deref() == Some(workspace.info.user_id.as_str());
        let (arrived, fetch_conversation) = workspace.message_arrived(channel, message, viewing);
        if fetch_conversation {
            self.backend.send(Command::FetchConversation {
                team: team.to_owned(),
                channel: channel.to_owned(),
            });
        }
        self.fetch_arrived(team, arrived);
        if viewing && !from_me {
            self.mark_if_viewing(team, channel);
        }
        if viewing {
            self.waker.wake();
        }
    }

    fn sent(&mut self, team: &str, channel: &str, local: &Ts, result: Result<Message, String>) {
        let Some(workspace) = self.workspace_mut(team) else {
            return;
        };
        workspace.sent(channel, local, &result);
        if let Err(error) = result {
            self.toast(tf("Message not sent: {error}", &[("error", &error)]), true);
        }
    }

    /// A message is gone; an open thread it started closes with it.
    fn remove_message(&mut self, team: &str, channel: &str, ts: &Ts) {
        if let Some(workspace) = self.workspace_mut(team) {
            workspace.remove_message(channel, ts);
        }
        if self.active_team().as_deref() == Some(team)
            && self
                .thread
                .as_ref()
                .is_some_and(|(c, parent)| c == channel && parent == ts)
        {
            self.thread = None;
        }
    }

    fn fetch_arrived(&mut self, team: &str, arrived: Arrived) {
        self.fetch_users(team, arrived.users);
        self.fetch_bots(team, arrived.bots);
    }

    fn fetch_users(&mut self, team: &str, ids: Vec<String>) {
        if ids.is_empty() || self.demo {
            return;
        }
        if let Some(workspace) = self.workspace_mut(team) {
            workspace.requested_users.extend(ids.iter().cloned());
        }
        self.backend.send(Command::FetchUsers {
            team: team.to_owned(),
            ids,
        });
    }

    fn fetch_bots(&mut self, team: &str, ids: Vec<String>) {
        if ids.is_empty() || self.demo {
            return;
        }
        if let Some(workspace) = self.workspace_mut(team) {
            workspace.requested_bots.extend(ids.iter().cloned());
        }
        self.backend.send(Command::FetchBots {
            team: team.to_owned(),
            ids,
        });
    }

    // ---- read state -----------------------------------------------------

    fn is_viewing(&self, team: &str, channel: &str) -> bool {
        self.page == Page::Main
            && self.window_focused
            && self.active_team().as_deref() == Some(team)
            && self.active_workspace().and_then(|w| w.active.as_deref()) == Some(channel)
    }

    fn mark_if_viewing(&mut self, team: &str, channel: &str) {
        if self.is_viewing(team, channel) {
            self.mark_read(team, channel);
        }
    }

    fn mark_active_read(&mut self) {
        if let Some(team) = self.active_team()
            && let Some(channel) = self.active_workspace().and_then(|w| w.active.clone())
        {
            self.mark_read(&team, &channel);
        }
    }

    /// Clears a conversation's unread state here and tells Slack, at most
    /// every few seconds.
    fn mark_read(&mut self, team: &str, channel: &str) {
        let Some(workspace) = self.workspace_mut(team) else {
            return;
        };
        let newest = workspace
            .timelines
            .get(channel)
            .and_then(|t| t.newest().cloned());
        let Some(conversation) = workspace.conversation_mut(channel) else {
            return;
        };
        let Some(latest) = newest.or_else(|| conversation.latest.clone()) else {
            return;
        };
        conversation.mentions = 0;
        conversation.unread = 0;
        if conversation
            .last_read
            .as_ref()
            .is_some_and(|read| *read >= latest)
        {
            return;
        }
        conversation.last_read = Some(latest.clone());
        self.pending_marks
            .insert((team.to_owned(), channel.to_owned()), latest);
    }

    fn flush_marks(&mut self) {
        if self.pending_marks.is_empty() || self.demo {
            self.pending_marks.clear();
            return;
        }
        let now = Instant::now();
        let ready: Vec<(String, String)> = self
            .pending_marks
            .keys()
            .filter(|key| {
                self.marks
                    .get(*key)
                    .is_none_or(|(_, at)| now.duration_since(*at) >= MARK_EVERY)
            })
            .cloned()
            .collect();
        for key in ready {
            if let Some(ts) = self.pending_marks.remove(&key) {
                self.backend.send(Command::Mark {
                    team: key.0.clone(),
                    channel: key.1.clone(),
                    ts: ts.clone(),
                });
                self.marks.insert(key, (ts, now));
            }
        }
        if !self.pending_marks.is_empty() {
            self.waker.wake_after(MARK_EVERY);
        }
    }

    // ---- actions from the views ----------------------------------------

    fn ensure_loaded(&mut self, team: &str, channel: &str) {
        let Some(workspace) = self.workspace_mut(team) else {
            return;
        };
        let timeline = workspace.timelines.entry(channel.to_owned()).or_default();
        if !timeline.loaded && !timeline.loading {
            timeline.loading = true;
            self.backend.send(Command::LoadHistory {
                team: team.to_owned(),
                channel: channel.to_owned(),
            });
        }
        self.backend.send(Command::Focus {
            team: team.to_owned(),
            channel: Some(channel.to_owned()),
        });
    }

    pub fn open_conversation(&mut self, channel: &str) {
        let Some(team) = self.active_team() else {
            return;
        };
        if let Some(workspace) = self.workspace_mut(&team) {
            workspace.active = Some(channel.to_owned());
        }
        self.settings
            .last_conversation
            .insert(team.clone(), channel.to_owned());
        self.save_settings();
        self.thread = None;
        self.editing = None;
        self.page = Page::Main;
        self.scroll_to_bottom
            .insert(Self::draft_key(&team, channel, None));
        // An anchor kept for another conversation's older page.
        self.prepended = None;
        self.focus_composer = true;
        self.remember_read_line(&team, channel);
        self.ensure_loaded(&team, channel);
        self.mark_read(&team, channel);
    }

    fn remember_read_line(&mut self, team: &str, channel: &str) {
        let read = self
            .workspaces
            .iter()
            .find(|w| w.info.team_id == team)
            .and_then(|w| w.conversation(channel))
            .filter(|c| c.has_unread())
            .and_then(|c| c.last_read.clone());
        self.read_line = Some((format!("{team}/{channel}"), read));
    }

    fn next_local(&mut self) -> Ts {
        self.local_counter += 1;
        Ts::new(format!("local-{}", self.local_counter))
    }

    fn send(&mut self, text: String, thread: Option<Ts>, broadcast: bool) {
        let Some(team) = self.active_team() else {
            return;
        };
        let channel = match &thread {
            Some(_) => self.thread.as_ref().map(|(c, _)| c.clone()),
            None => self.active_workspace().and_then(|w| w.active.clone()),
        };
        let Some(channel) = channel else {
            return;
        };
        let key = Self::draft_key(&team, &channel, thread.as_ref());
        let draft = self.drafts.remove(&key).unwrap_or_default();
        let wire = to_wire(&text, &draft.mentions);
        if wire.trim().is_empty() {
            return;
        }
        let local = self.next_local();
        let Some(workspace) = self.workspace_mut(&team) else {
            return;
        };
        let message = local_message(&workspace.info.user_id, &local, &wire, &thread, broadcast);
        workspace.add_local(&channel, message);
        self.scroll_to_bottom.insert(key);
        self.backend.send(Command::Send {
            team,
            channel,
            text: wire,
            thread,
            broadcast,
            local,
        });
    }

    fn retry(&mut self, channel: &str, local: &Ts) {
        let Some(team) = self.active_team() else {
            return;
        };
        let found = self
            .workspace_mut(&team)
            .and_then(|w| w.retry_local(channel, local));
        if let Some((text, thread, broadcast)) = found {
            self.backend.send(Command::Send {
                team,
                channel: channel.to_owned(),
                text,
                thread,
                broadcast,
                local: local.clone(),
            });
        }
    }

    fn react(&mut self, channel: &str, ts: &Ts, name: &str) {
        let Some(team) = self.active_team() else {
            return;
        };
        let add = self
            .workspace_mut(&team)
            .and_then(|w| w.toggle_my_reaction(channel, ts, name));
        if let Some(add) = add {
            self.backend.send(Command::React {
                team,
                channel: channel.to_owned(),
                ts: ts.clone(),
                name: name.to_owned(),
                add,
            });
        }
    }

    fn edit(&mut self, channel: String, ts: Ts, text: String) {
        let Some(team) = self.active_team() else {
            return;
        };
        let mentions = self
            .editing
            .take()
            .filter(|e| e.ts == ts && e.channel == channel)
            .map(|e| e.mentions)
            .unwrap_or_default();
        let wire = to_wire(&text, &mentions);
        let before = self
            .workspace_mut(&team)
            .and_then(|w| w.edit_locally(&channel, &ts, &wire));
        self.backend.send(Command::Edit {
            team,
            channel,
            ts,
            text: wire,
            before: before.map(Box::new),
        });
    }

    fn delete(&mut self, channel: String, ts: Ts) {
        let Some(team) = self.active_team() else {
            return;
        };
        let removed = self
            .active_workspace()
            .and_then(|w| w.find_message(&channel, &ts))
            .cloned();
        self.remove_message(&team, &channel, &ts);
        if !ts.is_local() {
            self.backend.send(Command::Delete {
                team,
                channel,
                ts,
                removed: removed.map(Box::new),
            });
        }
    }

    /// Where a file from the composer of `thread` (or the conversation)
    /// goes, as it is on screen now.
    fn upload_target(&self, thread: Option<Ts>) -> Option<UploadTarget> {
        let team = self.active_team()?;
        let channel = match &thread {
            Some(_) => self.thread.as_ref().map(|(c, _)| c.clone()),
            None => self.active_workspace().and_then(|w| w.active.clone()),
        }?;
        Some((team, channel, thread))
    }

    fn upload(&mut self, thread: Option<Ts>, path: PathBuf, comment: String) {
        if let Some((team, channel, thread)) = self.upload_target(thread) {
            self.backend.send(Command::Upload {
                team,
                channel,
                thread,
                path,
                comment,
            });
        }
    }

    fn pick_upload(&mut self, thread: Option<Ts>) {
        // Decided now: the dialog may stay open while you switch to
        // another conversation, and the file belongs to this one.
        let Some(target) = self.upload_target(thread) else {
            return;
        };
        let sender = self.uploads.0.clone();
        let waker = self.waker.clone();
        std::thread::spawn(move || {
            if let Some(path) = rfd::FileDialog::new().pick_file() {
                let _ = sender.send((target, path));
                waker.wake();
            }
        });
    }

    fn apply(&mut self, action: Action, ctx: &egui::Context) {
        match action {
            // Where you are.
            Action::SelectWorkspace(team) => self.select_workspace(team),
            Action::OpenConversation(channel) => self.open_conversation(&channel),
            Action::OpenThread { channel, ts } => self.open_thread(channel, ts),
            Action::CloseThread => self.thread = None,
            Action::LoadOlder => self.load_older(),
            Action::ShowSettings => self.page = Page::Settings,
            Action::HideSettings => {
                self.page = if self.workspaces.is_empty() {
                    Page::SignIn
                } else {
                    Page::Main
                };
            }
            // Messages.
            Action::Send {
                text,
                thread,
                broadcast,
            } => self.send(text, thread, broadcast),
            Action::Retry { channel, local } => self.retry(&channel, &local),
            Action::Edit { channel, ts, text } => self.edit(channel, ts, text),
            Action::Delete { channel, ts } => self.delete(channel, ts),
            Action::React { channel, ts, name } => self.react(&channel, &ts, &name),
            Action::StartEdit { channel, ts } => self.start_edit(channel, ts, false),
            Action::StartEditInThread { channel, ts } => self.start_edit(channel, ts, true),
            Action::CancelEdit => self.editing = None,
            Action::EditLast => {
                if let Some((channel, ts)) = self.active_workspace().and_then(|w| w.last_editable())
                {
                    self.actions.push(Action::StartEdit { channel, ts });
                }
            }
            Action::Upload {
                thread,
                path,
                comment,
            } => self.upload(thread, path, comment),
            Action::PickUpload { thread } => self.pick_upload(thread),
            Action::Download { url, name } => {
                if let Some(team) = self.active_team() {
                    self.backend.send(Command::Download { team, url, name });
                }
            }
            Action::Sidebar(edit) => self.edit_sidebar(edit),
            // What floats over the window.
            Action::PickReaction { channel, ts } => {
                self.open_picker(PickerTarget::Reaction { channel, ts });
            }
            Action::PickEmoji { draft } => self.open_picker(PickerTarget::Draft(draft)),
            Action::AskDelete { channel, ts } => self.confirm_delete = Some((channel, ts)),
            Action::NameSection { rename, channel } => self.name_section(rename, channel),
            Action::Preview { uri, name } => self.preview = Some((uri, name)),
            Action::OpenSwitcher => {
                self.focus_overlay = true;
                self.switcher = Some((String::new(), 0));
            }
            Action::OpenProfile(user) => self.profile = Some(user),
            Action::DismissError => self.toasts.clear(),
            // Leaving the app: links, folders and the clipboard.
            Action::OpenUrl(url) => self.open_url(&url),
            Action::OpenFolder(path) => {
                if let Err(error) = open::that_detached(&path) {
                    let error = error.to_string();
                    self.toast(
                        tf("Could not open the folder: {error}", &[("error", &error)]),
                        true,
                    );
                }
            }
            Action::Copy(text) => {
                ctx.copy_text(text);
                self.toast(t("Copied").into_owned(), false);
            }
            // Accounts and sign-in.
            Action::AddWorkspace => {
                self.sign_in = None;
                self.page = Page::SignIn;
            }
            Action::SignOut(team) => self.backend.send(Command::SignOut(team)),
            Action::Reconnect => self.backend.send(Command::Reconnect),
            Action::SignInSession => {
                self.sign_in = None;
                self.backend.send(Command::SignInSession {
                    cookie: self.setup.session_cookie.trim().to_owned(),
                    workspace_url: self.setup.session_workspace.trim().to_owned(),
                });
            }
            Action::StartBrowserSignIn => {
                self.sign_in = None;
                self.backend.send(Command::StartBrowserSignIn);
            }
            Action::SignInLink => {
                self.sign_in = None;
                // The link is a one-time secret: read it once, then forget it.
                let link = std::mem::take(&mut self.setup.session_link);
                self.backend
                    .send(Command::SignInLink(link.trim().to_owned()));
            }
            Action::PasteToken => {
                let token = self.setup.user_token.trim().to_owned();
                self.backend.send(Command::PasteToken(token));
            }
            Action::SaveApp => self.save_app(),
            Action::StartSignIn => {
                self.sign_in = None;
                self.backend.send(Command::StartSignIn {
                    redirect: self.settings.redirect,
                    port: self.settings.loopback_port,
                });
            }
            Action::CancelSignIn => {
                self.backend.send(Command::CancelSignIn);
                self.sign_in = None;
            }
        }
    }

    fn select_workspace(&mut self, team: String) {
        self.settings.active_workspace = Some(team.clone());
        self.save_settings();
        self.thread = None;
        self.page = Page::Main;
        self.prepended = None;
        if let Some(channel) = self.workspace_mut(&team).and_then(|w| w.active.clone()) {
            self.scroll_to_bottom
                .insert(Self::draft_key(&team, &channel, None));
            self.remember_read_line(&team, &channel);
            self.ensure_loaded(&team, &channel);
            self.mark_read(&team, &channel);
        } else {
            self.backend.send(Command::Focus {
                team,
                channel: None,
            });
        }
    }

    fn open_thread(&mut self, channel: String, ts: Ts) {
        let Some(team) = self.active_team() else {
            return;
        };
        self.thread = Some((channel.clone(), ts.clone()));
        if let Some(workspace) = self.workspace_mut(&team) {
            workspace
                .threads
                .entry((channel.clone(), ts.clone()))
                .or_default()
                .loading = true;
        }
        self.backend.send(Command::LoadThread { team, channel, ts });
    }

    fn load_older(&mut self) {
        let Some(team) = self.active_team() else {
            return;
        };
        let Some(workspace) = self.workspace_mut(&team) else {
            return;
        };
        let Some(channel) = workspace.active.clone() else {
            return;
        };
        if let Some(timeline) = workspace.timelines.get_mut(&channel)
            && timeline.has_more
            && !timeline.loading
            && let Some(cursor) = timeline.cursor.clone()
        {
            timeline.loading = true;
            self.backend.send(Command::LoadOlder {
                team,
                channel,
                cursor,
            });
        }
    }

    fn open_picker(&mut self, target: PickerTarget) {
        self.focus_overlay = true;
        self.picker_query.clear();
        self.picker = Some(target);
    }

    fn name_section(&mut self, rename: Option<String>, channel: Option<String>) {
        let name = rename
            .as_deref()
            .and_then(|id| {
                self.active_workspace()?
                    .sections
                    .as_ref()?
                    .iter()
                    .find(|s| s.id == id)
                    .map(|s| s.name.clone())
            })
            .unwrap_or_default();
        self.focus_overlay = true;
        self.section_dialog = Some(SectionDialog {
            rename,
            channel,
            name,
        });
    }

    fn open_url(&mut self, url: &str) {
        if let Some(channel) = slack_link_channel(url, self.active_workspace()) {
            self.open_conversation(&channel);
        } else if !mrkdwn::is_openable(url) {
            // Attachments and blocks carry URLs a bot chose.
            self.toast(t("Only web and mail links can be opened"), true);
        } else if let Err(error) = open::that_detached(url) {
            let error = error.to_string();
            self.toast(
                tf("Could not open the link: {error}", &[("error", &error)]),
                true,
            );
        }
    }

    fn save_app(&mut self) {
        let form = AppCredentials {
            client_id: self.setup.client_id.trim().to_owned(),
            client_secret: self.setup.client_secret.trim().to_owned(),
            app_token: self.setup.app_token.trim().to_owned(),
        };
        if form.can_sign_in() {
            self.backend.send(Command::SaveApp(form.clone()));
            self.app_credentials = Some(form);
        }
    }

    fn start_edit(&mut self, channel: String, ts: Ts, in_thread: bool) {
        let found = self
            .active_workspace()
            .and_then(|w| w.find_message(&channel, &ts).map(|m| w.editable(&m.text)));
        if let Some((text, mentions)) = found {
            self.editing = Some(Editing {
                channel,
                ts,
                text,
                mentions,
                in_thread,
                focus: true,
            });
        }
    }

    /// Whether a dialog or picker covers the window, so its keys come
    /// first.
    pub fn overlay_open(&self) -> bool {
        self.switcher.is_some()
            || self.picker.is_some()
            || self.profile.is_some()
            || self.preview.is_some()
            || self.confirm_delete.is_some()
            || self.section_dialog.is_some()
    }

    /// Changes the sidebar at once, and in Slack, which then sends back the
    /// sections as they really are.
    fn edit_sidebar(&mut self, edit: crate::sidebar::SidebarEdit) {
        let Some(team) = self.active_team() else {
            return;
        };
        let demo = self.demo;
        let Some(sections) = self.workspace_mut(&team).and_then(|w| w.sections.as_mut()) else {
            return;
        };
        let calls = crate::sidebar::plan(sections, &edit);
        crate::sidebar::apply(sections, &edit);
        if !calls.is_empty() && !demo {
            self.backend.send(Command::Sidebar { team, calls });
        }
    }

    /// Opens the DM with someone, if there is one.
    pub fn direct_message(&self, user: &str) -> Option<String> {
        self.active_workspace()?
            .conversations
            .iter()
            .find(|c| c.kind == ConversationKind::Direct && c.user.as_deref() == Some(user))
            .map(|c| c.id.clone())
    }

    pub fn save_state(&mut self) {
        self.settings.save(&self.dirs.settings_file());
    }

    pub fn request_quit(&mut self) {
        self.quit = true;
    }
}

/// Fills in what a fresher copy of a conversation lacks, and keeps the
/// newer of each marker.
fn merge_conversation(existing: &mut Conversation, fresh: Conversation) {
    let latest = max_ts(existing.latest.take(), fresh.latest.clone());
    let last_read = max_ts(existing.last_read.take(), fresh.last_read.clone());
    let mentions = existing.mentions;
    let unread = if fresh.unread > 0 {
        fresh.unread
    } else {
        existing.unread
    };
    *existing = Conversation {
        latest,
        last_read,
        mentions,
        unread,
        ..fresh
    };
    // Read on another device: Slack's count lags, the markers do not.
    if read_through(existing) {
        existing.unread = 0;
        existing.mentions = 0;
    }
}

/// Whether the read marker is at or past the newest message.
fn read_through(conversation: &Conversation) -> bool {
    matches!(
        (&conversation.last_read, &conversation.latest),
        (Some(read), Some(latest)) if read >= latest
    )
}

/// Moves the read marker to `ts`, never back: markers from other devices
/// and from polling can arrive out of order. The counts clear only once
/// nothing newer is left.
fn read_up_to(conversation: &mut Conversation, ts: Ts) {
    conversation.last_read = max_ts(conversation.last_read.take(), Some(ts));
    if conversation.latest.is_none() || read_through(conversation) {
        conversation.unread = 0;
        conversation.mentions = 0;
    }
}

fn max_ts(a: Option<Ts>, b: Option<Ts>) -> Option<Ts> {
    match (a, b) {
        (Some(a), Some(b)) => Some(a.max(b)),
        (a, b) => a.or(b),
    }
}

/// The copy of a message you are sending that shows until Slack answers.
fn local_message(
    me: &str,
    local: &Ts,
    wire: &str,
    thread: &Option<Ts>,
    broadcast: bool,
) -> Message {
    Message {
        ts: local.clone(),
        user: Some(me.to_owned()),
        username: None,
        bot_icon: None,
        bot_id: None,
        text: wire.to_owned(),
        thread_ts: thread.clone(),
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
        delivery: Delivery::Sending,
        broadcast,
    }
}

/// A sent message's own echo replaces its optimistic copy.
fn remove_echoed_local(timeline: &mut Timeline, message: &Message, from_me: bool) {
    if !from_me {
        return;
    }
    if let Some(position) = timeline
        .messages
        .iter()
        .position(|m| m.ts.is_local() && m.delivery == Delivery::Sending && m.text == message.text)
    {
        timeline.messages.remove(position);
    }
}

/// What `@here`, `@channel` and `@everyone` become for Slack.
const BROADCASTS: [(&str, &str); 3] = [
    ("@here", "<!here>"),
    ("@channel", "<!channel>"),
    ("@everyone", "<!everyone>"),
];

/// What Slack receives for what you typed: markup characters escaped, and
/// picked mentions and broadcasts turned into Slack's own forms.
///
/// `mentions` pairs the text as typed with the markup it stands for. A
/// label only counts where it stands alone, so "@Ann" leaves "@Annabel"
/// be, and the text is read once from the start, so markup already put in
/// is never matched again.
pub fn to_wire(text: &str, mentions: &[(String, String)]) -> String {
    let escaped = mrkdwn::escape(text.trim_end());
    let mut forms: Vec<(String, &str)> = mentions
        .iter()
        .map(|(label, wire)| (mrkdwn::escape(label), wire.as_str()))
        .chain(
            BROADCASTS
                .iter()
                .map(|(typed, wire)| ((*typed).to_owned(), *wire)),
        )
        .filter(|(label, _)| !label.is_empty())
        .collect();
    // Longest first, so "@Ann Lee" wins over "@Ann".
    forms.sort_by_key(|(label, _)| std::cmp::Reverse(label.len()));
    let mut out = String::with_capacity(escaped.len());
    let mut previous = None;
    let mut rest = escaped.as_str();
    while let Some(c) = rest.chars().next() {
        let found = forms.iter().find(|(label, _)| {
            rest.starts_with(label.as_str()) && is_word_edge(rest[label.len()..].chars().next())
        });
        if is_word_edge(previous)
            && let Some((label, wire)) = found
        {
            out.push_str(wire);
            previous = label.chars().next_back();
            rest = &rest[label.len()..];
        } else {
            out.push(c);
            previous = Some(c);
            rest = &rest[c.len_utf8()..];
        }
    }
    out
}

/// Whether a typed label may start or end next to `c`.
fn is_word_edge(c: Option<char>) -> bool {
    c.is_none_or(|c| !c.is_alphanumeric())
}

/// A piece of a message being made editable.
struct Piece {
    shown: String,
    /// The markup it came from, when typing `shown` alone would not bring
    /// it back.
    wire: Option<String>,
    /// What to show (and its markup) when `shown` is not unique in the
    /// text: [`to_wire`] would otherwise turn every copy into this link.
    fallback: Option<(String, String)>,
}

impl Piece {
    fn text(text: &str) -> Self {
        Self {
            shown: mrkdwn::unescape(text),
            wire: None,
            fallback: None,
        }
    }

    fn markup(shown: String, wire: String) -> Self {
        Self {
            shown,
            wire: Some(wire),
            fallback: None,
        }
    }
}

/// A sent message's text as you would type it, and the mentions that turn
/// it back into the same markup through [`to_wire`].
///
/// People and channels show as `@name` and `#name`. A link shows its label
/// when that is unique in the text, and its address otherwise. `name_of`
/// names a person (`'@'`) or a channel (`'#'`) by id.
pub fn to_editable(
    wire: &str,
    name_of: impl Fn(char, &str) -> Option<String>,
) -> (String, Vec<(String, String)>) {
    let mut pieces: Vec<Piece> = Vec::new();
    let mut rest = wire;
    while let Some(open) = rest.find('<') {
        pieces.push(Piece::text(&rest[..open]));
        let after = &rest[open + 1..];
        let inner = after
            .find('>')
            .map(|close| &after[..close])
            .filter(|inner| !inner.is_empty() && !inner.contains(['<', '\n']));
        let Some(inner) = inner else {
            // Not markup: Slack escapes a typed `<`, so keep it as it is.
            pieces.push(Piece::text("<"));
            rest = after;
            continue;
        };
        rest = &after[inner.len() + 1..];
        let raw = format!("<{inner}>");
        let (target, label) = match inner.split_once('|') {
            Some((target, label)) => (target, Some(mrkdwn::unescape(label))),
            None => (inner, None),
        };
        let label = label.filter(|l| !l.is_empty());
        let name = |sigil: char, id: &str| {
            name_of(sigil, id)
                .or_else(|| {
                    label
                        .as_deref()
                        .map(|l| l.trim_start_matches(sigil).to_owned())
                })
                .unwrap_or_else(|| id.to_owned())
        };
        let piece = if let Some(id) = target.strip_prefix('@') {
            Piece::markup(format!("@{}", name('@', id)), raw)
        } else if let Some(id) = target.strip_prefix('#') {
            Piece::markup(format!("#{}", name('#', id)), raw)
        } else if let Some(command) = target.strip_prefix('!') {
            let word = command.split('^').next().unwrap_or(command);
            match BROADCASTS.iter().find(|(typed, _)| typed[1..] == *word) {
                // Typing these brings them back.
                Some((typed, _)) => Piece::text(typed),
                // User groups and dates: their label stands for them.
                None => Piece::markup(label.clone().unwrap_or_else(|| format!("@{word}")), raw),
            }
        } else {
            let url = mrkdwn::unescape(target);
            let bare = format!("<{target}>");
            match label.clone().filter(|l| *l != url) {
                Some(label) => Piece {
                    shown: label,
                    wire: Some(raw),
                    fallback: Some((url, bare)),
                },
                None => Piece::markup(url, bare),
            }
        };
        pieces.push(piece);
    }
    pieces.push(Piece::text(rest));
    let text: String = pieces.iter().map(|p| p.shown.as_str()).collect();
    let mut mentions: Vec<(String, String)> = Vec::new();
    for piece in &mut pieces {
        if let Some((url, bare)) = piece.fallback.take()
            && text.matches(piece.shown.as_str()).count() > 1
        {
            piece.shown = url;
            piece.wire = Some(bare);
        }
        if let Some(wire) = &piece.wire
            && !mentions.iter().any(|(label, _)| *label == piece.shown)
        {
            mentions.push((piece.shown.clone(), wire.clone()));
        }
    }
    let text = pieces.iter().map(|p| p.shown.as_str()).collect();
    (text, mentions)
}

/// The conversation a link to this workspace's Slack points at
/// (`https://acme.slack.com/archives/C123/p…`).
fn slack_link_channel(url: &str, workspace: Option<&WorkspaceState>) -> Option<String> {
    let workspace = workspace?;
    if workspace.info.domain.is_empty() {
        return None;
    }
    let prefix = format!("https://{}.slack.com/archives/", workspace.info.domain);
    let rest = url.strip_prefix(&prefix)?;
    let channel = rest.split(['/', '?']).next()?;
    workspace.conversation(channel).map(|c| c.id.clone())
}

impl fastframe_shell::Resident for App {
    fn closed(&self) -> Closed {
        Closed::Quit
    }

    fn window_gone(&mut self) {
        self.waker.detach();
    }

    fn headless_frame(&mut self, ctx: &egui::Context) -> Headless {
        self.background_frame(ctx);
        if self.quit {
            Headless::Quit
        } else {
            Headless::Wait
        }
    }

    fn shutdown(&mut self) {
        self.save_state();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn typed_text_becomes_slack_markup() {
        let mentions = vec![
            ("@Ann".to_owned(), "<@U1>".to_owned()),
            ("@Ann Lee".to_owned(), "<@U2>".to_owned()),
        ];
        assert_eq!(
            to_wire("hi @Ann Lee & @Ann <3 @here, not @heresy", &mentions),
            "hi <@U2> &amp; <@U1> &lt;3 <!here>, not @heresy"
        );
    }

    #[test]
    fn mention_labels_match_only_whole_words() {
        let mentions = vec![
            ("@Ann".to_owned(), "<@U1>".to_owned()),
            ("@Annabel".to_owned(), "<@U2>".to_owned()),
            ("@Zoë".to_owned(), "<@U3>".to_owned()),
        ];
        assert_eq!(
            to_wire("@Annabel, @Ann and @Annie", &mentions),
            "<@U2>, <@U1> and @Annie"
        );
        assert_eq!(
            to_wire("über @Zoë! ünd @Zoëy mail@Ann", &mentions),
            "über <@U3>! ünd @Zoëy mail@Ann"
        );
        // The boundary is read from the whole text, not from where the last
        // match ended.
        assert_eq!(to_wire("é@here @here", &[]), "é@here <!here>");
        assert_eq!(to_wire("@channel—@everyone", &[]), "<!channel>—<!everyone>");
    }

    #[test]
    fn inserted_markup_is_not_matched_again() {
        // A person called "U2" must not reach into `<@U2>`.
        let mentions = vec![
            ("@Bo".to_owned(), "<@U2>".to_owned()),
            ("@U2".to_owned(), "<@U9>".to_owned()),
        ];
        assert_eq!(to_wire("@Bo", &mentions), "<@U2>");
    }

    fn names(sigil: char, id: &str) -> Option<String> {
        match (sigil, id) {
            ('@', "U1") => Some("Ann Lee".into()),
            ('#', "C1") => Some("general".into()),
            _ => None,
        }
    }

    #[test]
    fn edited_messages_keep_their_markup() {
        let wire = "hi <@U1> and <@U2|bob> in <#C1|general> &amp; <!here>: see \
                    <https://x.y/a?b=1&amp;c=2|the docs>, <https://x.y> or \
                    <mailto:a@x.y|a@x.y> &lt;3 <!subteam^S1|@design> ünï *bold*";
        let (text, mentions) = to_editable(wire, names);
        assert_eq!(
            text,
            "hi @Ann Lee and @bob in #general & @here: see the docs, https://x.y or \
             a@x.y <3 @design ünï *bold*"
        );
        assert_eq!(to_wire(&text, &mentions), wire);
    }

    #[test]
    fn edits_survive_changes_around_the_markup() {
        let (text, mentions) = to_editable("ping <@U1> about <#C9>", names);
        assert_eq!(text, "ping @Ann Lee about #C9");
        let changed = text.replace("ping", "hey") + " & <#C1>";
        assert_eq!(
            to_wire(&changed, &mentions),
            "hey <@U1> about <#C9> &amp; &lt;#C1&gt;"
        );
    }

    #[test]
    fn a_link_label_that_repeats_shows_the_address() {
        // "docs" appears as a word too; keeping the label would link both.
        let wire = "docs: <https://x.y|docs>";
        let (text, mentions) = to_editable(wire, names);
        assert_eq!(text, "docs: https://x.y");
        assert_eq!(to_wire(&text, &mentions), "docs: <https://x.y>");
    }

    #[test]
    fn stray_angle_brackets_stay_text() {
        let (text, mentions) = to_editable("a < b <> c", names);
        assert_eq!(text, "a < b <> c");
        assert!(mentions.is_empty());
    }

    #[test]
    fn fresher_conversations_keep_newer_markers() {
        let mut existing = Conversation {
            id: "C1".into(),
            name: "old".into(),
            kind: ConversationKind::Channel,
            user: None,
            topic: String::new(),
            purpose: String::new(),
            members: None,
            archived: false,
            last_read: Some(Ts::new("5.0")),
            latest: Some(Ts::new("9.0")),
            unread: 0,
            mentions: 2,
        };
        let fresh = Conversation {
            name: "renamed".into(),
            last_read: Some(Ts::new("7.0")),
            latest: None,
            mentions: 0,
            ..existing.clone()
        };
        merge_conversation(&mut existing, fresh);
        assert_eq!(existing.name, "renamed");
        assert_eq!(existing.latest, Some(Ts::new("9.0")));
        assert_eq!(existing.last_read, Some(Ts::new("7.0")));
        assert_eq!(existing.mentions, 2);
    }

    fn conversation(last_read: &str, latest: &str, unread: u32, mentions: u32) -> Conversation {
        Conversation {
            id: "C1".into(),
            name: "general".into(),
            kind: ConversationKind::Channel,
            user: None,
            topic: String::new(),
            purpose: String::new(),
            members: None,
            archived: false,
            last_read: Some(Ts::new(last_read)),
            latest: Some(Ts::new(latest)),
            unread,
            mentions,
        }
    }

    #[test]
    fn counts_clear_when_read_elsewhere() {
        let mut existing = conversation("5.0", "9.0", 4, 1);
        merge_conversation(&mut existing, conversation("9.0", "9.0", 4, 0));
        assert_eq!(existing.unread, 0);
        assert_eq!(existing.mentions, 0);
        assert!(!existing.has_unread());
        // Still behind: Slack's count stands.
        let mut existing = conversation("5.0", "9.0", 0, 1);
        merge_conversation(&mut existing, conversation("6.0", "9.0", 3, 0));
        assert_eq!(existing.unread, 3);
        assert_eq!(existing.mentions, 1);
    }

    fn message(ts: &str, thread: Option<&str>) -> Message {
        Message {
            ts: Ts::new(ts),
            user: Some("U1".into()),
            username: None,
            bot_icon: None,
            bot_id: None,
            text: format!("text {ts}"),
            thread_ts: thread.map(Ts::new),
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
        }
    }

    fn workspace() -> WorkspaceState {
        WorkspaceState::new(Workspace {
            team_id: "T1".into(),
            name: "Acme".into(),
            domain: "acme".into(),
            icon: None,
            user_id: "U1".into(),
        })
    }

    /// A workspace with a parent in C1 and two replies loaded in its thread.
    fn workspace_with_thread() -> WorkspaceState {
        let mut w = workspace();
        let mut parent = message("1.0", Some("1.0"));
        parent.reply_count = 2;
        let main = w.timelines.entry("C1".into()).or_default();
        main.upsert(parent.clone());
        main.upsert(message("5.0", None));
        let thread = w.threads.entry(("C1".into(), Ts::new("1.0"))).or_default();
        thread.upsert(parent);
        thread.upsert(message("2.0", Some("1.0")));
        thread.upsert(message("3.0", Some("1.0")));
        w
    }

    #[test]
    fn deleting_a_reply_lowers_the_count() {
        let mut w = workspace_with_thread();
        w.remove_message("C1", &Ts::new("2.0"));
        let count = |t: &Timeline| t.messages[0].reply_count;
        assert_eq!(count(&w.timelines["C1"]), 1);
        let thread = &w.threads[&("C1".to_owned(), Ts::new("1.0"))];
        assert_eq!(count(thread), 1);
        assert_eq!(thread.messages.len(), 2);
        // Deleted again (the echo of our own delete): nothing more changes.
        w.remove_message("C1", &Ts::new("2.0"));
        assert_eq!(count(&w.timelines["C1"]), 1);
        // A plain message has no parent to change.
        w.remove_message("C1", &Ts::new("5.0"));
        assert_eq!(count(&w.timelines["C1"]), 1);
    }

    #[test]
    fn a_channels_timelines_are_its_own_and_its_threads() {
        let mut w = workspace_with_thread();
        w.threads
            .entry(("C2".into(), Ts::new("2.0")))
            .or_default()
            .upsert(message("2.0", Some("2.0")));
        assert_eq!(w.timelines_for("C1").count(), 2);
        assert_eq!(w.timelines_for("C2").count(), 1);
        assert!(w.find_message("C1", &Ts::new("3.0")).is_some());
        assert!(w.find_message("C2", &Ts::new("3.0")).is_none());
    }

    #[test]
    fn deleting_a_parent_takes_its_thread() {
        let mut w = workspace_with_thread();
        w.remove_message("C1", &Ts::new("1.0"));
        assert!(w.threads.is_empty());
        assert_eq!(w.timelines["C1"].messages.len(), 1);
    }

    /// A workspace with #general (C1) open on an empty history.
    fn workspace_in_general() -> WorkspaceState {
        let mut w = workspace();
        w.conversations.push(conversation("1.0", "1.0", 0, 0));
        let timeline = w.timelines.entry("C1".into()).or_default();
        timeline.loaded = true;
        w
    }

    fn sending(w: &mut WorkspaceState, local: &str, text: &str, thread: Option<&str>) {
        let thread = thread.map(Ts::new);
        w.add_local(
            "C1",
            local_message("U1", &Ts::new(local), text, &thread, false),
        );
    }

    fn texts(timeline: &Timeline) -> Vec<(&str, &str)> {
        timeline
            .messages
            .iter()
            .map(|m| (m.ts.as_str(), m.text.as_str()))
            .collect()
    }

    fn mine(ts: &str, text: &str) -> Message {
        Message {
            text: text.into(),
            ..message(ts, None)
        }
    }

    #[test]
    fn a_sent_message_replaces_its_local_copy() {
        let mut w = workspace_in_general();
        sending(&mut w, "local-1", "hi", None);
        assert_eq!(w.timelines["C1"].messages[0].delivery, Delivery::Sending);
        w.sent("C1", &Ts::new("local-1"), &Ok(mine("5.0", "hi")));
        // Then Socket Mode echoes it: still one copy.
        w.message_arrived("C1", mine("5.0", "hi"), true);
        assert_eq!(texts(&w.timelines["C1"]), [("5.0", "hi")]);
        let c = w.conversation("C1").expect("C1");
        assert_eq!(c.latest, Some(Ts::new("5.0")));
        assert_eq!(c.last_read, Some(Ts::new("5.0")));
    }

    #[test]
    fn an_echo_before_the_answer_also_replaces_the_local_copy() {
        let mut w = workspace_in_general();
        sending(&mut w, "local-1", "one", None);
        sending(&mut w, "local-2", "two", None);
        w.message_arrived("C1", mine("5.0", "two"), true);
        assert_eq!(
            texts(&w.timelines["C1"]),
            [("5.0", "two"), ("local-1", "one")]
        );
        w.sent("C1", &Ts::new("local-2"), &Ok(mine("5.0", "two")));
        assert_eq!(
            texts(&w.timelines["C1"]),
            [("5.0", "two"), ("local-1", "one")]
        );
        // Someone else saying the same is not our echo.
        let mut theirs = mine("6.0", "one");
        theirs.user = Some("U2".into());
        w.message_arrived("C1", theirs, true);
        assert!(w.timelines["C1"].messages.iter().any(|m| m.ts.is_local()));
    }

    #[test]
    fn a_failed_send_can_be_retried() {
        let mut w = workspace_in_general();
        sending(&mut w, "local-1", "hi", None);
        w.sent("C1", &Ts::new("local-1"), &Err("ratelimited".into()));
        let failed = &w.timelines["C1"].messages[0];
        assert_eq!(failed.delivery, Delivery::Failed("ratelimited".into()));
        assert_eq!(
            w.retry_local("C1", &Ts::new("local-1")),
            Some(("hi".to_owned(), None, false))
        );
        assert_eq!(w.timelines["C1"].messages[0].delivery, Delivery::Sending);
        assert_eq!(w.retry_local("C1", &Ts::new("local-9")), None);
    }

    #[test]
    fn a_reply_counts_once_on_its_parent() {
        let mut w = workspace_with_thread();
        w.conversations.push(conversation("1.0", "5.0", 0, 0));
        let reply = || message("4.0", Some("1.0"));
        w.message_arrived("C1", reply(), true);
        w.message_arrived("C1", reply(), true);
        let parent = &w.timelines["C1"].messages[0];
        assert_eq!(parent.reply_count, 3);
        assert_eq!(parent.latest_reply, Some(Ts::new("4.0")));
        // Replies stay out of the channel's own list.
        assert_eq!(w.timelines["C1"].messages.len(), 2);
    }

    #[test]
    fn mentions_count_only_when_not_looking() {
        let mut w = workspace_in_general();
        let mut ping = message("5.0", None);
        ping.user = Some("U2".into());
        ping.text = "hey <@U1>".into();
        w.message_arrived("C1", ping.clone(), true);
        assert_eq!(w.conversation("C1").map(|c| c.mentions), Some(0));
        ping.ts = Ts::new("6.0");
        w.message_arrived("C1", ping, false);
        assert_eq!(w.conversation("C1").map(|c| c.mentions), Some(1));
        // Writing there yourself means you have read it.
        w.message_arrived("C1", mine("7.0", "on it"), false);
        let c = w.conversation("C1").expect("C1");
        assert_eq!((c.mentions, c.last_read.clone()), (0, Some(Ts::new("7.0"))));
    }

    #[test]
    fn messages_in_unknown_conversations_fetch_it_once() {
        let mut w = workspace_in_general();
        let (arrived, fetch) = w.message_arrived("C9", message("5.0", None), false);
        assert!(fetch);
        // Its details name the people; they are not fetched one by one.
        assert!(arrived.users.is_empty());
        let (_, fetch) = w.message_arrived("C9", message("6.0", None), false);
        assert!(!fetch);
    }

    #[test]
    fn read_events_arrive_in_any_order() {
        let mut w = workspace_in_general();
        w.conversations[0] = conversation("1.0", "9.0", 3, 1);
        w.read_elsewhere("C1", Ts::new("9.0"));
        w.read_elsewhere("C1", Ts::new("4.0"));
        let c = w.conversation("C1").expect("C1");
        assert_eq!(c.last_read, Some(Ts::new("9.0")));
        assert_eq!((c.unread, c.mentions), (0, 0));
    }

    #[test]
    fn my_reaction_toggles_everywhere_the_message_shows() {
        let mut w = workspace_with_thread();
        assert_eq!(
            w.toggle_my_reaction("C1", &Ts::new("1.0"), "tada"),
            Some(true)
        );
        let count = |w: &WorkspaceState| {
            w.timelines_for("C1")
                .filter_map(|t| t.messages.iter().find(|m| m.ts == Ts::new("1.0")))
                .map(|m| m.reactions.len())
                .collect::<Vec<_>>()
        };
        assert_eq!(count(&w), [1, 1]);
        assert_eq!(
            w.toggle_my_reaction("C1", &Ts::new("1.0"), "tada"),
            Some(false)
        );
        assert_eq!(count(&w), [0, 0]);
        assert_eq!(w.toggle_my_reaction("C1", &Ts::new("8.0"), "tada"), None);
    }

    #[test]
    fn read_markers_never_move_back() {
        let mut c = conversation("5.0", "9.0", 2, 1);
        read_up_to(&mut c, Ts::new("9.0"));
        assert_eq!(c.last_read, Some(Ts::new("9.0")));
        assert_eq!((c.unread, c.mentions), (0, 0));
        // A late, older marker changes nothing.
        read_up_to(&mut c, Ts::new("7.0"));
        assert_eq!(c.last_read, Some(Ts::new("9.0")));
        // Read only part of the way: what is left stays unread.
        let mut c = conversation("5.0", "9.0", 2, 1);
        read_up_to(&mut c, Ts::new("7.0"));
        assert_eq!(c.last_read, Some(Ts::new("7.0")));
        assert_eq!((c.unread, c.mentions), (2, 1));
        assert!(c.has_unread());
    }

    #[test]
    fn a_refused_edit_puts_the_old_text_back() {
        let mut w = workspace_with_thread();
        let ts = Ts::new("1.0");
        let before = w.edit_locally("C1", &ts, "new").map(Box::new);
        assert!(before.is_some());
        let edit = Change::Edit {
            ts: ts.clone(),
            text: "new".into(),
            before,
        };
        w.undo("C1", edit);
        for timeline in w.timelines_for("C1") {
            let parent = &timeline.messages[0];
            assert_eq!(parent.text, "text 1.0");
            assert!(!parent.edited);
        }
        // A later edit that did go through is not undone.
        let before = w.edit_locally("C1", &ts, "first").map(Box::new);
        w.edit_locally("C1", &ts, "second");
        w.undo(
            "C1",
            Change::Edit {
                ts: ts.clone(),
                text: "first".into(),
                before,
            },
        );
        assert_eq!(
            w.find_message("C1", &ts).map(|m| m.text.as_str()),
            Some("second")
        );
    }

    #[test]
    fn a_refused_delete_brings_the_message_back() {
        let mut w = workspace_with_thread();
        let reply = w.find_message("C1", &Ts::new("2.0")).cloned();
        w.remove_message("C1", &Ts::new("2.0"));
        w.undo(
            "C1",
            Change::Delete {
                ts: Ts::new("2.0"),
                removed: reply.map(Box::new),
            },
        );
        let thread = &w.threads[&("C1".to_owned(), Ts::new("1.0"))];
        let order: Vec<&str> = thread.messages.iter().map(|m| m.ts.as_str()).collect();
        assert_eq!(order, ["1.0", "2.0", "3.0"]);
        assert_eq!(thread.messages[0].reply_count, 2);
        assert_eq!(w.timelines["C1"].messages[0].reply_count, 2);
        assert_eq!(
            w.timelines["C1"].messages.len(),
            2,
            "a reply stays in its thread"
        );

        let plain = w.find_message("C1", &Ts::new("5.0")).cloned();
        w.remove_message("C1", &Ts::new("5.0"));
        let undo = Change::Delete {
            ts: Ts::new("5.0"),
            removed: plain.map(Box::new),
        };
        w.undo("C1", undo.clone());
        // Twice, as if Slack had sent it back meanwhile: still one copy.
        w.undo("C1", undo);
        let order: Vec<&str> = w.timelines["C1"]
            .messages
            .iter()
            .map(|m| m.ts.as_str())
            .collect();
        assert_eq!(order, ["1.0", "5.0"]);
    }

    #[test]
    fn a_refused_reaction_is_taken_back() {
        let mut w = workspace_with_thread();
        let ts = Ts::new("1.0");
        let added = w.toggle_my_reaction("C1", &ts, "tada");
        assert_eq!(added, Some(true));
        w.undo(
            "C1",
            Change::React {
                ts: ts.clone(),
                name: "tada".into(),
                added: true,
            },
        );
        assert!(
            w.timelines_for("C1")
                .all(|t| t.messages[0].reactions.is_empty())
        );
    }

    #[test]
    fn edits_change_no_counts() {
        let mut w = workspace_with_thread();
        w.conversations.push(conversation("1.0", "5.0", 0, 0));
        // An edited reply that now mentions you, while you look elsewhere.
        let mut reply = message("2.0", Some("1.0"));
        reply.user = Some("U2".into());
        reply.text = "now for <@U1>".into();
        reply.edited = true;
        w.message_changed("C1", reply);
        let thread = &w.threads[&("C1".to_owned(), Ts::new("1.0"))];
        assert_eq!(thread.messages[1].text, "now for <@U1>");
        assert_eq!(thread.messages[0].reply_count, 2);
        assert_eq!(w.timelines["C1"].messages[0].reply_count, 2);
        let c = w.conversation("C1").expect("C1");
        assert_eq!((c.mentions, c.latest.clone()), (0, Some(Ts::new("5.0"))));
        assert_eq!(w.timelines["C1"].messages.len(), 2);

        // A parent's new thread details replace the loaded copies.
        let mut parent = message("1.0", Some("1.0"));
        parent.reply_count = 3;
        parent.replies_known = true;
        w.message_changed("C1", parent);
        assert_eq!(w.timelines["C1"].messages[0].reply_count, 3);
    }

    #[test]
    fn edits_outside_the_loaded_history_stay_out() {
        let mut w = workspace_with_thread();
        w.conversations.push(conversation("1.0", "5.0", 0, 0));
        let mut old = message("0.5", None);
        old.edited = true;
        let arrived = w.message_changed("C1", old.clone());
        assert_eq!(arrived, Arrived::default());
        let order: Vec<&str> = w.timelines["C1"]
            .messages
            .iter()
            .map(|m| m.ts.as_str())
            .collect();
        assert_eq!(order, ["1.0", "5.0"]);
        // Nor in a conversation that is not loaded at all.
        w.message_changed("C2", old);
        assert!(!w.timelines.contains_key("C2"));
    }
}
