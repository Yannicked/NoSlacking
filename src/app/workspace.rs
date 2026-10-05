//! One workspace's loaded state, and how the worker's events and your
//! own actions change it.
//!
//! Kept apart from `app.rs` and free of side effects, so it can be tested
//! without a backend.

use std::collections::{HashMap, HashSet, VecDeque};

use super::to_editable;
use crate::backend::Change;
use crate::emoji::EmojiSet;
use crate::failure::Failure;
use crate::i18n::t;
use crate::model::{
    Bot, Conversation, ConversationKind, Delivery, KitBlock, Message, SidebarSection, Timeline, Ts,
    User, Workspace,
};
use crate::settings::Settings;

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
    pub signed_out: Option<Failure>,
    pub loaded: bool,
    pub(super) requested_users: HashSet<String>,
    pub(super) requested_bots: HashSet<String>,
    requested_conversations: HashSet<String>,
    /// Raised whenever people arrive, so lookups built from `users` know
    /// when to rebuild.
    users_version: u64,
    /// Notification choices and the like for this workspace.
    pub desktop: crate::desktop::TeamState,
    /// Who is around, and the like (see [`crate::people`]).
    pub people: crate::people::TeamPeople,
    /// The newest messages already treated as new (notified, hooks run),
    /// oldest first. A message can reach us both live and by a poll, in
    /// either order, and must be announced only once.
    seen: VecDeque<(String, Ts)>,
}

/// How many announced messages [`WorkspaceState::seen`] remembers. A copy
/// arriving twice comes close together, so a few hundred is plenty.
const SEEN_LIMIT: usize = 512;

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
            users_version: 0,
            desktop: crate::desktop::TeamState::default(),
            people: crate::people::TeamPeople::default(),
            seen: VecDeque::new(),
        }
    }

    /// Changes whenever `users` gains or updates someone through the
    /// worker; pair it with `users.len()` for edits made directly.
    pub fn users_version(&self) -> u64 {
        self.users_version
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

    /// Whether a conversation shows as unread: a muted one only for its
    /// mentions, as in Slack.
    pub fn is_unread(&self, conversation: &Conversation) -> bool {
        conversation.has_unread()
            && (conversation.mentions > 0 || !self.desktop.is_muted(&conversation.id))
    }

    /// How much a conversation asks for you, for the sidebar's
    /// unread-first order; a muted one counts only for its mentions.
    pub fn rank(&self, conversation: &Conversation) -> crate::sidebar::Rank {
        crate::sidebar::rank(conversation, self.is_unread(conversation))
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
    pub(super) fn remove_message(&mut self, channel: &str, ts: &Ts) {
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

/// What a change from the worker leaves for [`App`](super::App) to do: the people and
/// apps it named that are not known yet.
#[derive(Debug, Default, PartialEq)]
pub(super) struct Arrived {
    pub(super) users: Vec<String>,
    pub(super) bots: Vec<String>,
}

// What the worker's events and your own actions change in one workspace.
// Kept apart from `App` and free of side effects (no commands, toasts or
// scrolling), so they can be tested without a backend.
impl WorkspaceState {
    /// The full (or a cached) list of conversations. Returns the people
    /// to fetch for DMs.
    pub(super) fn conversations_arrived(
        &mut self,
        list: Vec<Conversation>,
        complete: bool,
    ) -> Vec<String> {
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
    pub(super) fn conversation_arrived(&mut self, conversation: Conversation) -> Vec<String> {
        let fetch = self.unknown_users(conversation.user.as_deref().into_iter());
        match self.conversation_mut(&conversation.id) {
            Some(existing) => merge_conversation(existing, conversation),
            None => self.conversations.push(conversation),
        }
        fetch
    }

    /// You left a conversation, or it was archived or deleted.
    pub(super) fn conversation_gone(&mut self, channel: &str) {
        self.conversations.retain(|c| c.id != channel);
        self.timelines.remove(channel);
        self.threads.retain(|(c, _), _| c != channel);
        if self.active.as_deref() == Some(channel) {
            self.active = None;
        }
    }

    pub(super) fn users_arrived(&mut self, users: Vec<User>) {
        self.users_version += 1;
        for user in users {
            self.requested_users.remove(&user.id);
            self.users.insert(user.id.clone(), user);
        }
    }

    pub(super) fn bots_arrived(&mut self, bots: Vec<Bot>) {
        for bot in bots {
            self.requested_bots.remove(&bot.id);
            self.bots.insert(bot.id.clone(), bot);
        }
    }

    /// A page of a conversation's history. Returns whom to fetch, and
    /// whether this was the first page.
    pub(super) fn history_arrived(
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
        if !older && timeline.cached {
            // Slack's own newest page replaces the cached copy whole: the
            // copy may hold messages deleted since, or end before a gap.
            timeline.cached = false;
            timeline.loaded = false;
            timeline.messages.retain(|m| m.ts.is_local());
        }
        let first = !timeline.loaded;
        // The newest page does not join a stretch of older history opened
        // around a message: it would leave a gap between them unseen.
        let joins = older || !timeline.has_newer;
        timeline.merge(if joins { messages } else { Vec::new() });
        // Messages around one jumped to are still coming, and will replace
        // these: nothing else may be asked for until then.
        timeline.loading = timeline.around.is_some();
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

    /// The messages of a history page that are new since the conversation
    /// was last seen, to be announced as if they had come live.
    ///
    /// Only a poll (`polled`) of the newest page (not `older`) can bring
    /// such messages. For a conversation whose messages are loaded, new
    /// means newer than the newest one loaded. A conversation never opened
    /// (see [`Self::unopened`]) has no messages to measure from, so new
    /// means newer than its newest message known before this page: a
    /// conversation with none known yet announces nothing, as its first
    /// data is not news. A first load in progress, the offline cache's
    /// copy, or a stretch of history opened around an older message gives
    /// no line at all, and so nothing. Either way, your own messages and
    /// ones announced already are left out. Call it before the page is
    /// merged, since merging moves the line.
    pub(super) fn polled_new(
        &self,
        channel: &str,
        messages: &[Message],
        older: bool,
        polled: bool,
    ) -> Vec<Message> {
        if !polled || older {
            return Vec::new();
        }
        let line = if self.unopened(channel) {
            // Live messages can sit in a list never opened: past news too.
            let known = self.conversation(channel).and_then(|c| c.latest.as_ref());
            let live = self.timelines.get(channel).and_then(Timeline::newest);
            match known.max(live) {
                Some(line) => Some(line),
                None => return Vec::new(),
            }
        } else {
            let Some(timeline) = self.timelines.get(channel) else {
                return Vec::new();
            };
            if !timeline.loaded
                || timeline.cached
                || timeline.has_newer
                || timeline.around.is_some()
            {
                return Vec::new();
            }
            // Anything loaded, live ones included, is past news.
            timeline.newest()
        };
        let me = self.info.user_id.as_str();
        messages
            .iter()
            .filter(|m| !m.ts.is_local() && line.is_none_or(|n| m.ts > *n))
            .filter(|m| m.user.as_deref() != Some(me))
            .filter(|m| !self.was_seen(channel, &m.ts))
            .cloned()
            .collect()
    }

    /// Whether a conversation was never opened here: nothing loaded,
    /// nothing loading, no offline copy shown. A poll of such a
    /// conversation only tells what is new in it; its messages wait until
    /// it is opened and loaded properly, offline copy and all.
    pub(super) fn unopened(&self, channel: &str) -> bool {
        self.timelines
            .get(channel)
            .is_none_or(|t| !t.loaded && !t.loading && t.around.is_none())
    }

    /// A polled page of a conversation never opened (see
    /// [`Self::unopened`]): its newest message becomes the conversation's
    /// latest, so the sidebar shows it unread, and the page is not kept.
    pub(super) fn polled_unopened(&mut self, channel: &str, messages: &[Message]) {
        let newest = messages
            .iter()
            .map(|m| &m.ts)
            .filter(|ts| !ts.is_local())
            .max()
            .cloned();
        if let Some(conversation) = self.conversation_mut(channel) {
            conversation.latest = max_ts(conversation.latest.take(), newest);
        }
    }

    /// Counts an unread mention for a message a poll announced, as the
    /// live path does: one that mentions you, or any in a direct message
    /// that is not muted, unless you are looking at it.
    pub(super) fn count_polled(&mut self, channel: &str, message: &Message, viewing: bool) {
        let counts = !viewing
            && (self.mentions_me(message)
                || (self.conversation(channel).is_some_and(|c| c.kind.is_dm())
                    && !self.desktop.is_muted(channel)));
        if counts && let Some(conversation) = self.conversation_mut(channel) {
            conversation.mentions += 1;
        }
    }

    /// What a poll learnt about a conversation (see
    /// [`crate::backend::Event::Activity`]). Markers only move forward;
    /// Slack's mention count is taken when it is higher than what was
    /// counted here, and cleared when the marker is past everything.
    /// Answers whether the conversation is unknown here and should be
    /// fetched, once: only when it has something unread and the full list
    /// is in, as Slack counts conversations the list leaves out (closed
    /// ones), and a poll at start-up can beat the list.
    pub(super) fn activity(
        &mut self,
        channel: &str,
        latest: Option<Ts>,
        last_read: Option<Ts>,
        mentions: Option<u32>,
    ) -> bool {
        let Some(conversation) = self.conversation_mut(channel) else {
            let unread = latest
                .as_ref()
                .is_some_and(|latest| last_read.as_ref().is_none_or(|read| read < latest));
            return self.loaded
                && unread
                && self.requested_conversations.insert(channel.to_owned());
        };
        conversation.latest = max_ts(conversation.latest.take(), latest);
        if let Some(mentions) = mentions {
            conversation.mentions = conversation.mentions.max(mentions);
        }
        match last_read {
            Some(ts) => read_up_to(conversation, ts),
            None if read_through(conversation) => {
                conversation.unread = 0;
                conversation.mentions = 0;
            }
            None => {}
        }
        false
    }

    /// Whether a message was announced already.
    fn was_seen(&self, channel: &str, ts: &Ts) -> bool {
        self.seen.iter().any(|(c, t)| c == channel && t == ts)
    }

    /// Records a message as announced. Answers whether this is the first
    /// time, so a copy coming again (live after a poll, or the other way
    /// round) stays quiet.
    pub(super) fn first_sight(&mut self, channel: &str, ts: &Ts) -> bool {
        if self.was_seen(channel, ts) {
            return false;
        }
        if self.seen.len() >= SEEN_LIMIT {
            self.seen.pop_front();
        }
        self.seen.push_back((channel.to_owned(), ts.clone()));
        true
    }

    /// The offline cache's copy of the newest page, shown only while nothing
    /// else is, until Slack's page replaces it. Returns whom to fetch, or
    /// `None` when the copy came too late to be of use.
    pub(super) fn cached_history_arrived(
        &mut self,
        channel: &str,
        messages: Vec<Message>,
        has_more: bool,
        cursor: Option<String>,
    ) -> Option<Arrived> {
        if self
            .timelines
            .get(channel)
            .is_some_and(|t| t.loaded || t.around.is_some())
        {
            return None;
        }
        let (arrived, _) = self.history_arrived(channel, messages, has_more, cursor, false);
        if let Some(timeline) = self.timelines.get_mut(channel) {
            timeline.cached = true;
            // Older pages wait for Slack's newest one, whose cursor counts.
            timeline.loading = true;
        }
        Some(arrived)
    }

    /// The messages around one jumped to, which replace the list: the
    /// stretch it held may lie far from them. `older` is whether there is
    /// history before them, and its cursor. Returns whom to fetch.
    pub(super) fn around_arrived(
        &mut self,
        channel: &str,
        messages: Vec<Message>,
        older: (bool, Option<String>),
        has_newer: bool,
    ) -> Arrived {
        let arrived = self.arrived_in(&messages);
        let timeline = self.timelines.entry(channel.to_owned()).or_default();
        // Messages still being sent stay; they go after everything real.
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
        (timeline.has_more, timeline.cursor) = older;
        timeline.has_newer = has_newer;
        timeline.loaded = true;
        timeline.loading = false;
        timeline.around = None;
        arrived
    }

    /// The page after the newest message of a list of older history.
    /// Returns whom to fetch.
    pub(super) fn newer_arrived(
        &mut self,
        channel: &str,
        messages: Vec<Message>,
        has_newer: bool,
    ) -> Arrived {
        let arrived = self.arrived_in(&messages);
        let newest = messages.iter().map(|m| m.ts.clone()).max();
        let timeline = self.timelines.entry(channel.to_owned()).or_default();
        timeline.merge(messages);
        timeline.has_newer = has_newer;
        timeline.loading = false;
        if let (Some(newest), Some(conversation)) = (newest, self.conversation_mut(channel))
            && conversation.latest.as_ref().is_none_or(|l| *l < newest)
        {
            conversation.latest = Some(newest);
        }
        arrived
    }

    /// The authors, repliers and apps of `messages` not known yet.
    fn arrived_in(&self, messages: &[Message]) -> Arrived {
        Arrived {
            users: self.unknown_users(messages.iter().flat_map(|m| {
                m.user
                    .as_deref()
                    .into_iter()
                    .chain(m.reply_users.iter().map(String::as_str))
            })),
            bots: self.unknown_bots(messages.iter()),
        }
    }

    pub(super) fn history_failed(&mut self, channel: &str) {
        if let Some(timeline) = self.timelines.get_mut(channel) {
            timeline.loading = false;
            timeline.around = None;
        }
    }

    /// A whole thread, parent first.
    pub(super) fn thread_arrived(
        &mut self,
        channel: &str,
        ts: Ts,
        messages: Vec<Message>,
    ) -> Arrived {
        let arrived = Arrived {
            users: self.unknown_users(messages.iter().filter_map(|m| m.user.as_deref())),
            bots: self.unknown_bots(messages.iter()),
        };
        // The thread's own copy of its parent carries Slack's count, which
        // corrects whatever was counted here; failing that, the replies
        // that came are the count.
        let replies = messages.iter().filter(|m| m.ts != ts).count() as u32;
        let fresh = messages.iter().find(|m| m.ts == ts && m.replies_known);
        if let Some(parent) = self
            .timelines
            .get_mut(channel)
            .and_then(|t| t.find_mut(&ts))
        {
            match fresh {
                Some(fresh) => {
                    parent.reply_count = fresh.reply_count;
                    parent.replies_known = true;
                    parent.reply_users.clone_from(&fresh.reply_users);
                    parent.latest_reply.clone_from(&fresh.latest_reply);
                }
                None => parent.reply_count = replies,
            }
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
    pub(super) fn message_arrived(
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
        // A muted direct message counts only what mentions you.
        let counts_all = !self.desktop.is_muted(channel);
        if message.is_reply() {
            self.count_reply(channel, &message);
            let parent_ts = message.thread_ts.clone().unwrap_or_default();
            let key = (channel.to_owned(), parent_ts);
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
            // A list of older history shows new messages once it is read up
            // to them, not after a gap.
            if !(new && timeline.has_newer) {
                timeline.upsert(message);
            }
            if new && let Some(conversation) = self.conversation_mut(channel) {
                if conversation.latest.as_ref().is_none_or(|l| *l < ts) {
                    conversation.latest = Some(ts.clone());
                }
                if from_me {
                    conversation.last_read = Some(ts);
                    conversation.mentions = 0;
                } else if !viewing && (mentions_me || (conversation.kind.is_dm() && counts_all)) {
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

    /// Counts a new reply on every loaded copy of its parent, once. A reply
    /// no newer than the parent's latest is counted already: by an earlier
    /// copy of this reply, or by Slack, whose own update of the parent can
    /// come before the reply does.
    fn count_reply(&mut self, channel: &str, reply: &Message) {
        let Some(parent) = reply.thread_ts.clone() else {
            return;
        };
        let thread = self.threads.get_mut(&(channel.to_owned(), parent.clone()));
        let copies = self
            .timelines
            .get_mut(channel)
            .and_then(|t| t.find_mut(&parent))
            .into_iter()
            .chain(thread.and_then(|t| t.find_mut(&parent)));
        for copy in copies {
            if copy
                .latest_reply
                .as_ref()
                .is_some_and(|latest| reply.ts <= *latest)
            {
                continue;
            }
            copy.reply_count += 1;
            copy.latest_reply = Some(reply.ts.clone());
            if let Some(user) = &reply.user
                && !copy.reply_users.contains(user)
            {
                copy.reply_users.push(user.clone());
            }
        }
    }

    /// A new copy of a message already sent: an edit, or a thread
    /// parent's new reply details. It replaces the copies that are loaded
    /// and nothing else: no counts change, and a message outside the
    /// loaded history is not pulled in. Returns whom to fetch.
    pub(super) fn message_changed(&mut self, channel: &str, message: Message) -> Arrived {
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
    pub(super) fn sent(&mut self, channel: &str, local: &Ts, result: &Result<Message, Failure>) {
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
        // Your reply counts on whichever comes first, this answer or its
        // echo; the other finds it counted.
        if let Ok(message) = result
            && message.is_reply()
        {
            self.count_reply(channel, message);
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
    pub(super) fn reaction_changed(
        &mut self,
        channel: &str,
        ts: &Ts,
        name: &str,
        user: &str,
        added: bool,
    ) {
        for timeline in self.timelines_for_mut(channel) {
            if let Some(message) = timeline.find_mut(ts) {
                message.toggle_reaction(name, user, added);
            }
        }
    }

    /// You read up to `ts`, maybe on another device.
    pub(super) fn read_elsewhere(&mut self, channel: &str, ts: Ts) {
        if let Some(conversation) = self.conversation_mut(channel) {
            read_up_to(conversation, ts);
        }
    }

    /// Shows a message you are sending before Slack has it.
    pub(super) fn add_local(&mut self, channel: &str, message: Message) {
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
    pub(super) fn edit_locally(&mut self, channel: &str, ts: &Ts, wire: &str) -> Option<Message> {
        let mut before = None;
        for timeline in self.timelines_for_mut(channel) {
            if let Some(message) = timeline.find_mut(ts) {
                before.get_or_insert_with(|| message.clone());
                message.text = wire.to_owned();
                // Slack's layout is of the old text; until its copy of the
                // edit comes, the new text is drawn from its mrkdwn.
                message
                    .blocks
                    .retain(|block| !matches!(block, KitBlock::RichText(_)));
                message.edited = true;
            }
        }
        before
    }

    /// Takes back a change Slack refused: the text before your edit, the
    /// message you deleted, or your reaction toggle.
    pub(super) fn undo(&mut self, channel: &str, change: Change) {
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
                        message.blocks.clone_from(&before.blocks);
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
    pub(super) fn toggle_my_reaction(
        &mut self,
        channel: &str,
        ts: &Ts,
        name: &str,
    ) -> Option<bool> {
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
    pub(super) fn retry_local(
        &mut self,
        channel: &str,
        local: &Ts,
    ) -> Option<(String, Option<Ts>, bool)> {
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
    pub(super) fn last_editable(&self) -> Option<(String, Ts)> {
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
/// Views that borrow [`App`](super::App)'s fields apart call this instead of
/// [`App::active_workspace`](super::App::active_workspace).
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
pub(super) fn local_message(
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
        pinned: false,
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

/// The message the "New" line goes above: the first in the
/// conversation's own list after `read` that is not yours, as the list
/// draws it.
pub(super) fn first_unread(timeline: &Timeline, read: &Ts, me: &str) -> Option<Ts> {
    timeline
        .messages
        .iter()
        .filter(|m| m.in_channel() && !m.ts.is_local())
        .find(|m| m.ts > *read && m.user.as_deref() != Some(me))
        .map(|m| m.ts.clone())
}

#[cfg(test)]
mod tests {
    use super::*;

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
            external: false,
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
            external: false,
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
            pinned: false,
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

    fn counts(w: &WorkspaceState) -> (u32, u32) {
        let parent = Ts::new("1.0");
        let thread = &w.threads[&("C1".to_owned(), parent.clone())];
        (
            w.timelines["C1"].messages[0].reply_count,
            thread
                .messages
                .iter()
                .find(|m| m.ts == parent)
                .map_or(0, |m| m.reply_count),
        )
    }

    #[test]
    fn slacks_parent_update_before_the_reply_counts_it_once() {
        let mut w = workspace_with_thread();
        // Slack's copy of the parent, already counting reply 4.0.
        let mut parent = message("1.0", Some("1.0"));
        parent.reply_count = 3;
        parent.replies_known = true;
        parent.latest_reply = Some(Ts::new("4.0"));
        w.message_changed("C1", parent);
        w.message_arrived("C1", message("4.0", Some("1.0")), true);
        assert_eq!(counts(&w), (3, 3));
    }

    #[test]
    fn a_reply_delivered_twice_counts_once_with_its_thread_closed() {
        let mut w = workspace_with_thread();
        w.threads.clear();
        w.message_arrived("C1", message("4.0", Some("1.0")), false);
        w.message_arrived("C1", message("4.0", Some("1.0")), false);
        assert_eq!(w.timelines["C1"].messages[0].reply_count, 3);
    }

    #[test]
    fn your_reply_counts_once_whichever_comes_first() {
        for answer_first in [true, false] {
            let mut w = workspace_with_thread();
            sending(&mut w, "local-1", "mine", Some("1.0"));
            let real = Message {
                text: "mine".into(),
                ..message("4.0", Some("1.0"))
            };
            if answer_first {
                w.sent("C1", &Ts::new("local-1"), &Ok(real.clone()));
                w.message_arrived("C1", real, true);
            } else {
                w.message_arrived("C1", real.clone(), true);
                w.sent("C1", &Ts::new("local-1"), &Ok(real));
            }
            assert_eq!(counts(&w), (3, 3), "answer first: {answer_first}");
        }
    }

    #[test]
    fn opening_a_thread_takes_slacks_count() {
        let mut w = workspace_with_thread();
        if let Some(parent) = w
            .timelines
            .get_mut("C1")
            .and_then(|t| t.find_mut(&Ts::new("1.0")))
        {
            parent.reply_count = 9;
        }
        let mut parent = message("1.0", Some("1.0"));
        parent.reply_count = 2;
        parent.replies_known = true;
        let thread = vec![
            parent,
            message("2.0", Some("1.0")),
            message("3.0", Some("1.0")),
        ];
        w.thread_arrived("C1", Ts::new("1.0"), thread);
        assert_eq!(counts(&w), (2, 2), "a count that ran high comes down");
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
        w.sent("C1", &Ts::new("local-1"), &Err(Failure::RateLimited));
        let failed = &w.timelines["C1"].messages[0];
        assert_eq!(failed.delivery, Delivery::Failed(Failure::RateLimited));
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
    fn an_edit_draws_its_new_text_until_slack_lays_it_out() {
        let mut w = workspace_with_thread();
        let ts = Ts::new("1.0");
        let laid_out: std::sync::Arc<[crate::mrkdwn::Block]> =
            crate::mrkdwn::parse("old layout").into();
        for timeline in w.timelines_for_mut("C1") {
            if let Some(message) = timeline.find_mut(&ts) {
                message.blocks = vec![KitBlock::RichText(laid_out.clone())];
            }
        }
        let before = w.edit_locally("C1", &ts, "new").map(Box::new);
        assert!(
            w.find_message("C1", &ts)
                .is_some_and(|m| m.rich_text().is_none()),
            "the old layout would show the old words"
        );
        w.undo(
            "C1",
            Change::Edit {
                ts: ts.clone(),
                text: "new".into(),
                before,
            },
        );
        assert_eq!(
            w.find_message("C1", &ts).and_then(Message::rich_text),
            Some(&laid_out)
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

    #[test]
    fn slacks_newest_page_replaces_the_cached_copy() {
        let mut w = workspace();
        w.conversations.push(conversation("1.0", "9.0", 0, 0));
        let order = |w: &WorkspaceState| -> Vec<String> {
            w.timelines["C1"]
                .messages
                .iter()
                .map(|m| m.ts.0.clone())
                .collect()
        };
        assert!(
            w.cached_history_arrived(
                "C1",
                vec![message("5.0", None), message("6.0", None)],
                true,
                Some("stale".into()),
            )
            .is_some()
        );
        assert_eq!(order(&w), ["5.0", "6.0"]);
        assert!(w.timelines["C1"].cached && w.timelines["C1"].loading);
        w.add_local(
            "C1",
            local_message("U1", &Ts::new("local-1"), "hi", &None, false),
        );
        // 6.0 was deleted meanwhile: Slack's page drops it, and its cursor
        // is the one that counts.
        w.history_arrived(
            "C1",
            vec![message("5.0", None), message("8.0", None)],
            true,
            Some("fresh".into()),
            false,
        );
        assert_eq!(order(&w), ["5.0", "8.0", "local-1"]);
        let timeline = &w.timelines["C1"];
        assert!(!timeline.cached && !timeline.loading);
        assert_eq!(timeline.cursor.as_deref(), Some("fresh"));
        // A cached copy that comes after Slack's page is not used.
        assert!(
            w.cached_history_arrived("C1", vec![message("1.0", None)], false, None)
                .is_none()
        );
        assert_eq!(order(&w), ["5.0", "8.0", "local-1"]);
    }

    #[test]
    fn a_stretch_of_older_history_stands_apart_from_the_newest() {
        let mut w = workspace();
        w.conversations.push(conversation("1.0", "9.0", 0, 0));
        w.history_arrived(
            "C1",
            vec![message("8.0", None), message("9.0", None)],
            true,
            Some("c".into()),
            false,
        );
        w.add_local(
            "C1",
            local_message("U1", &Ts::new("local-1"), "hi", &None, false),
        );
        // Jumping to an old message replaces the list, keeping what is
        // still being sent.
        w.around_arrived(
            "C1",
            vec![message("2.0", None), message("3.0", None)],
            (true, Some("older".into())),
            true,
        );
        let order = |w: &WorkspaceState| -> Vec<String> {
            w.timelines["C1"]
                .messages
                .iter()
                .map(|m| m.ts.0.clone())
                .collect()
        };
        assert_eq!(order(&w), ["2.0", "3.0", "local-1"]);
        let timeline = &w.timelines["C1"];
        assert!(timeline.has_newer && timeline.has_more);
        assert_eq!(timeline.cursor.as_deref(), Some("older"));
        // Neither the newest page (a poll) nor a new message joins it.
        w.history_arrived("C1", vec![message("9.0", None)], false, None, false);
        w.message_arrived("C1", message("10.0", None), false);
        assert_eq!(order(&w), ["2.0", "3.0", "local-1"]);
        assert_eq!(
            w.conversation("C1").and_then(|c| c.latest.clone()),
            Some(Ts::new("10.0"))
        );
        // An older page still does, and so do newer pages, up to the end.
        w.history_arrived("C1", vec![message("1.0", None)], false, None, true);
        w.newer_arrived("C1", vec![message("4.0", None)], false);
        assert_eq!(order(&w), ["1.0", "2.0", "3.0", "4.0", "local-1"]);
        assert!(!w.timelines["C1"].has_newer);
        w.message_arrived("C1", message("11.0", None), false);
        assert_eq!(w.timelines["C1"].messages.len(), 6);
    }

    /// A message in C1 from someone else.
    fn theirs(ts: &str) -> Message {
        Message {
            user: Some("U2".into()),
            ..message(ts, None)
        }
    }

    fn stamps(messages: &[Message]) -> Vec<&str> {
        messages.iter().map(|m| m.ts.as_str()).collect()
    }

    #[test]
    fn a_first_load_announces_nothing() {
        let w = workspace();
        let page = [theirs("1.0"), theirs("2.0")];
        assert!(w.polled_new("C1", &page, false, true).is_empty());
        // Nor does the offline cache's copy count as a line to measure from.
        let mut w = workspace();
        w.cached_history_arrived("C1", vec![theirs("1.0")], false, None);
        assert!(w.polled_new("C1", &page, false, true).is_empty());
    }

    #[test]
    fn a_poll_announces_what_is_past_the_newest_message() {
        let mut w = workspace();
        w.history_arrived("C1", vec![theirs("1.0"), theirs("2.0")], false, None, false);
        let page = [theirs("1.0"), theirs("2.0"), theirs("3.0"), theirs("4.0")];
        assert_eq!(
            stamps(&w.polled_new("C1", &page, false, true)),
            ["3.0", "4.0"]
        );
        // The same page as a reload, not a poll, is not news.
        assert!(w.polled_new("C1", &page, false, false).is_empty());
        // Once merged, the next poll finds nothing more.
        w.history_arrived("C1", page.to_vec(), false, None, false);
        assert!(w.polled_new("C1", &page, false, true).is_empty());
    }

    #[test]
    fn a_message_seen_live_is_not_announced_again() {
        let mut w = workspace();
        w.history_arrived("C1", vec![theirs("1.0")], false, None, false);
        // Live first: it is loaded, and so past news for the poll.
        assert!(w.first_sight("C1", &Ts::new("2.0")));
        w.message_arrived("C1", theirs("2.0"), false);
        let page = [theirs("1.0"), theirs("2.0")];
        assert!(w.polled_new("C1", &page, false, true).is_empty());
        // Announced live but not loaded: still quiet.
        assert!(w.first_sight("C1", &Ts::new("3.0")));
        assert!(w.polled_new("C1", &[theirs("3.0")], false, true).is_empty());
        // Polled first: the live copy that follows is not a first sight.
        let found = w.polled_new("C1", &[theirs("4.0")], false, true);
        assert_eq!(stamps(&found), ["4.0"]);
        assert!(w.first_sight("C1", &found[0].ts));
        assert!(!w.first_sight("C1", &Ts::new("4.0")));
    }

    #[test]
    fn your_own_messages_are_not_announced() {
        let mut w = workspace();
        w.history_arrived("C1", vec![theirs("1.0")], false, None, false);
        let page = [theirs("1.0"), message("2.0", None)];
        assert!(w.polled_new("C1", &page, false, true).is_empty());
    }

    #[test]
    fn an_older_page_announces_nothing() {
        let mut w = workspace();
        w.history_arrived("C1", vec![theirs("5.0")], true, Some("c".into()), false);
        let page = [theirs("1.0"), theirs("2.0")];
        assert!(w.polled_new("C1", &page, true, true).is_empty());
        assert!(w.polled_new("C1", &page, true, false).is_empty());
        // Nor does the newest page while a stretch of older history is open.
        w.around_arrived("C1", vec![theirs("1.0")], (false, None), true);
        assert!(w.polled_new("C1", &[theirs("9.0")], false, true).is_empty());
    }

    /// A workspace that knows C1 up to `latest` (read up to 1.0), but
    /// never opened it.
    fn unopened(latest: &str) -> WorkspaceState {
        let mut w = workspace();
        w.conversation_arrived(conversation("1.0", latest, 0, 0));
        w
    }

    #[test]
    fn an_unopened_conversation_announces_only_what_is_past_its_latest() {
        let mut w = unopened("2.0");
        assert!(w.unopened("C1"));
        let page = [theirs("1.0"), theirs("2.0"), theirs("3.0"), theirs("4.0")];
        assert_eq!(
            stamps(&w.polled_new("C1", &page, false, true)),
            ["3.0", "4.0"]
        );
        // Not a poll, or an older page: no news.
        assert!(w.polled_new("C1", &page, false, false).is_empty());
        assert!(w.polled_new("C1", &page, true, true).is_empty());
        // The page moves the latest on, and is not kept as a timeline.
        w.polled_unopened("C1", &page);
        assert_eq!(
            w.conversation("C1").and_then(|c| c.latest.clone()),
            Some(Ts::new("4.0"))
        );
        assert!(w.unopened("C1"));
        assert!(!w.timelines.contains_key("C1"));
        assert!(w.conversation("C1").is_some_and(Conversation::has_unread));
        // So the same page again is no news.
        assert!(w.polled_new("C1", &page, false, true).is_empty());
    }

    #[test]
    fn an_unopened_conversation_announces_nothing_twice() {
        let mut w = unopened("2.0");
        let page = [theirs("2.0"), theirs("3.0")];
        let found = w.polled_new("C1", &page, false, true);
        assert_eq!(stamps(&found), ["3.0"]);
        assert!(w.first_sight("C1", &found[0].ts));
        // Asked again before the latest moved (a page that came twice).
        assert!(w.polled_new("C1", &page, false, true).is_empty());
        // Seen live first: the poll that follows stays quiet.
        let mut w = unopened("2.0");
        assert!(w.first_sight("C1", &Ts::new("3.0")));
        w.message_arrived("C1", theirs("3.0"), false);
        assert!(w.unopened("C1"));
        assert!(w.polled_new("C1", &page, false, true).is_empty());
    }

    #[test]
    fn an_unopened_conversation_with_nothing_known_announces_nothing() {
        // Known, but with no newest message yet: its first data is not news.
        let mut w = workspace();
        let mut c = conversation("1.0", "1.0", 0, 0);
        c.latest = None;
        w.conversation_arrived(c);
        assert!(w.polled_new("C1", &[theirs("3.0")], false, true).is_empty());
        // Not known at all.
        let w = workspace();
        assert!(w.polled_new("C9", &[theirs("3.0")], false, true).is_empty());
    }

    #[test]
    fn an_unopened_conversation_leaves_out_your_own_messages() {
        let w = unopened("2.0");
        let page = [theirs("2.0"), message("3.0", None), theirs("4.0")];
        assert_eq!(stamps(&w.polled_new("C1", &page, false, true)), ["4.0"]);
    }

    #[test]
    fn a_first_load_under_way_is_not_unopened() {
        let mut w = unopened("2.0");
        w.timelines.entry("C1".into()).or_default().loading = true;
        assert!(!w.unopened("C1"));
        assert!(w.polled_new("C1", &[theirs("3.0")], false, true).is_empty());
    }

    #[test]
    fn polled_activity_moves_markers_only_forward() {
        let mut w = unopened("5.0");
        // Newer: taken, with Slack's mention count.
        assert!(!w.activity("C1", Some(Ts::new("7.0")), Some(Ts::new("2.0")), Some(2)));
        let c = w.conversation("C1").cloned().expect("known");
        assert_eq!(c.latest, Some(Ts::new("7.0")));
        assert_eq!(c.last_read, Some(Ts::new("2.0")));
        assert_eq!(c.mentions, 2);
        // Older, and a lower count: nothing goes back.
        w.activity("C1", Some(Ts::new("6.0")), Some(Ts::new("1.0")), Some(0));
        let c = w.conversation("C1").cloned().expect("known");
        assert_eq!(c.latest, Some(Ts::new("7.0")));
        assert_eq!(c.last_read, Some(Ts::new("2.0")));
        assert_eq!(c.mentions, 2);
        // Read up to the newest elsewhere: all clear.
        w.activity("C1", Some(Ts::new("7.0")), Some(Ts::new("7.0")), Some(0));
        let c = w.conversation("C1").cloned().expect("known");
        assert!(!c.has_unread());
        assert_eq!(c.mentions, 0);
    }

    #[test]
    fn polled_activity_fetches_an_unknown_conversation_once_when_unread() {
        let mut w = workspace();
        let new = || (Some(Ts::new("3.0")), Some(Ts::new("1.0")));
        // Before the list is in, nothing is fetched.
        let (latest, read) = new();
        assert!(!w.activity("D9", latest, read, None));
        w.conversations_arrived(Vec::new(), true);
        // Nothing unread: left alone.
        assert!(!w.activity("D9", Some(Ts::new("3.0")), Some(Ts::new("3.0")), None));
        let (latest, read) = new();
        assert!(w.activity("D9", latest, read, None));
        let (latest, read) = new();
        assert!(!w.activity("D9", latest, read, None));
    }

    #[test]
    fn polled_messages_count_mentions_as_live_ones_do() {
        let mut w = workspace();
        let mut dm = conversation("1.0", "1.0", 0, 0);
        dm.id = "D1".into();
        dm.kind = ConversationKind::Direct;
        w.conversation_arrived(dm);
        w.conversation_arrived(conversation("1.0", "1.0", 0, 0));
        w.count_polled("D1", &theirs("2.0"), false);
        // Looking at it: no unread mention.
        w.count_polled("D1", &theirs("3.0"), true);
        // A channel message that does not mention you.
        w.count_polled("C1", &theirs("2.0"), false);
        assert_eq!(w.conversation("D1").map(|c| c.mentions), Some(1));
        assert_eq!(w.conversation("C1").map(|c| c.mentions), Some(0));
    }

    #[test]
    fn the_new_line_goes_above_the_first_message_from_someone_else() {
        let mut timeline = Timeline::default();
        let mut mine = message("2.0", None);
        mine.user = Some("U1".into());
        let mut reply = message("3.0", Some("1.0"));
        reply.user = Some("U2".into());
        let mut theirs = message("4.0", None);
        theirs.user = Some("U2".into());
        for m in [message("1.0", None), mine, reply, theirs] {
            timeline.upsert(m);
        }
        assert_eq!(
            first_unread(&timeline, &Ts::new("1.0"), "U1"),
            Some(Ts::new("4.0"))
        );
        assert_eq!(first_unread(&timeline, &Ts::new("4.0"), "U1"), None);
    }
}
