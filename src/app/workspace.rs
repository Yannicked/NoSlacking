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
    User, UserGroup, Workspace,
};
use crate::revision::Revised;
use crate::settings::Settings;

/// One signed-in workspace and everything loaded for it.
pub struct WorkspaceState {
    pub info: Workspace,
    /// Revised, like `users`, `sections` and `desktop`, so the sidebar and
    /// the pickers rebuild only when what they show may have changed
    /// (see [`WorkspaceState::revision`]).
    pub conversations: Revised<Vec<Conversation>>,
    pub users: Revised<HashMap<String, User>>,
    /// Apps and integrations, by `bot_id`.
    pub bots: HashMap<String, Bot>,
    /// Your Slack sidebar sections, when Slack shares them (sessions).
    pub sections: Revised<Option<Vec<SidebarSection>>>,
    pub emoji: EmojiSet,
    /// Whether this sign-in can add custom emoji (browser sessions).
    pub can_add_emoji: bool,
    /// Custom emoji added here, kept until Slack's own list has them: a
    /// list fetched right after may not yet.
    added_emoji: HashMap<String, String>,
    /// User groups you can mention. Empty when the sign-in may
    /// not list them, which leaves group mentions out of the suggestions.
    pub groups: Vec<UserGroup>,
    pub timelines: HashMap<String, Timeline>,
    pub threads: HashMap<(String, Ts), Timeline>,
    pub active: Option<String>,
    /// Why this workspace needs signing in again, if it does.
    pub signed_out: Option<Failure>,
    pub loaded: bool,
    pub(super) requested_users: HashSet<String>,
    pub(super) requested_bots: HashSet<String>,
    requested_conversations: HashSet<String>,
    /// Notification choices and the like for this workspace.
    pub desktop: Revised<crate::desktop::TeamState>,
    /// Who is around, and the like (see [`crate::people`]).
    pub people: crate::people::TeamPeople,
    /// The newest messages already treated as new (notified, hooks run),
    /// oldest first. A message can reach us both live and by a poll, in
    /// either order, and must be announced only once.
    seen: VecDeque<(String, Ts)>,
    /// Conversations you marked unread. Their read marker stays where you
    /// put it while you keep looking (looking would otherwise read them
    /// again at once), until you open them anew, mark them read, write
    /// in them or read further on another device.
    held_unread: HashSet<String>,
    /// The replies already counted on their parents here, oldest first: a
    /// reply comes back as Slack's answer and as its echo, and must count
    /// once whichever order they come in.
    counted_replies: VecDeque<(String, Ts)>,
    /// The messages already counted as unread mentions, oldest first: a
    /// poll can count one before its live copy comes (or Slack delivers it
    /// again), and it must count once.
    counted_mentions: VecDeque<(String, Ts)>,
    /// The optimistic copies of messages Slack has not answered for yet, by
    /// their local id. An echo can take a copy's place before its own
    /// answer comes; the answer still settles it from here.
    sending: HashMap<Ts, Message>,
    /// Sends whose echo came, matched by client id, before Slack's answer:
    /// their local id and the real ts. The echo proves the message went,
    /// whatever the answer says.
    echoed: HashMap<Ts, Ts>,
    /// Messages you deleted while they were still sending, by local id:
    /// the send cannot be called back, so the message is deleted once
    /// Slack answers.
    cancelled: HashSet<Ts>,
    /// Messages so deleted, by conversation and real ts, whose echo is not
    /// to bring them back.
    suppressed: HashSet<(String, Ts)>,
    /// Messages fetched to quote under links to them, which are not
    /// loaded in any list here.
    pub quotes: crate::quotes::Cache,
    /// Button presses and menu choices sent and not yet answered, which
    /// show as busy and cannot be used again meanwhile.
    pub pressing: HashSet<crate::model::Press>,
    /// The choices Slack took from selects and radio buttons, by the press
    /// that made them (see [`WorkspaceState::chosen`]).
    chosen: Vec<crate::model::Press>,
    /// Files you deleted that Slack has not answered for yet: hidden
    /// everywhere, and shown again if Slack refuses.
    deleting_files: HashSet<String>,
    /// Files Slack says are deleted, by you here or anyone anywhere. A
    /// copy of a message fetched before the deletion still carries the
    /// file, and must not bring it back.
    gone_files: HashSet<String>,
}

/// What became of a send Slack answered (see [`WorkspaceState::sent`]).
#[derive(Debug, PartialEq)]
pub(super) enum SendOutcome {
    /// It shows as sent, or as failed.
    Settled,
    /// You deleted it while it was sending; `delete` is the message to
    /// delete in Slack, if it was posted.
    Cancelled { delete: Option<Ts> },
    /// Slack answered with an error, but the message's echo had already
    /// shown that it was posted (a timed-out answer, say): it shows as
    /// sent, and there is nothing to report.
    Posted,
}

/// How many announced messages [`WorkspaceState::seen`] remembers. A copy
/// arriving twice comes close together, so a few hundred is plenty.
const SEEN_LIMIT: usize = 512;

/// How many live messages a conversation never opened keeps: about a
/// page of history.
const UNOPENED_LIMIT: usize = 50;

impl WorkspaceState {
    /// A workspace with nothing loaded yet.
    pub(crate) fn new(info: Workspace) -> Self {
        Self {
            info,
            conversations: Revised::default(),
            users: Revised::default(),
            bots: HashMap::new(),
            sections: Revised::default(),
            emoji: EmojiSet::default(),
            can_add_emoji: false,
            added_emoji: HashMap::new(),
            groups: Vec::new(),
            timelines: HashMap::new(),
            threads: HashMap::new(),
            active: None,
            signed_out: None,
            loaded: false,
            requested_users: HashSet::new(),
            requested_bots: HashSet::new(),
            requested_conversations: HashSet::new(),
            desktop: Revised::default(),
            people: crate::people::TeamPeople::default(),
            seen: VecDeque::new(),
            held_unread: HashSet::new(),
            counted_replies: VecDeque::new(),
            counted_mentions: VecDeque::new(),
            sending: HashMap::new(),
            echoed: HashMap::new(),
            cancelled: HashSet::new(),
            suppressed: HashSet::new(),
            quotes: crate::quotes::Cache::default(),
            pressing: HashSet::new(),
            chosen: Vec::new(),
            deleting_files: HashSet::new(),
            gone_files: HashSet::new(),
        }
    }

    /// Whether a press of the same button or menu as `press` is on its
    /// way.
    pub fn is_pressing(&self, press: &crate::model::Press) -> bool {
        self.pressing.iter().any(|p| p.same_control(press))
    }

    /// The value last chosen here from the select or radio buttons that
    /// `press` is on, once Slack took it. The app usually answers by
    /// changing the message, but one that does not still shows the choice,
    /// as Slack's own client does.
    pub fn chosen(&self, press: &crate::model::Press) -> Option<&str> {
        self.chosen
            .iter()
            .find(|p| p.same_control(press))
            .and_then(|p| p.value.as_deref())
    }

    /// Notes the choice `press` made, which Slack took.
    pub fn choose(&mut self, press: crate::model::Press) {
        self.chosen.retain(|p| !p.same_control(&press));
        self.chosen.push(press);
    }

    /// Whether file `id` still shows: not deleted, here or in Slack. A
    /// deleted file stands as "This file was deleted", as Slack's own
    /// copy of the message will say once it comes.
    pub fn shows_file(&self, id: &str) -> bool {
        !self.deleting_files.contains(id) && !self.gone_files.contains(id)
    }

    /// Whether you marked `channel` unread and it is to stay so while it
    /// shows.
    pub fn holds_unread(&self, channel: &str) -> bool {
        self.held_unread.contains(channel)
    }

    /// Lets `channel` be read by looking at it again.
    pub(super) fn release_unread(&mut self, channel: &str) {
        self.held_unread.remove(channel);
    }

    /// Changes whenever `users` may have, so lookups built from it know
    /// when to rebuild.
    pub fn users_version(&self) -> u64 {
        self.users.revision()
    }

    /// Changes whenever anything the sidebar is laid out from may have:
    /// the conversations, the people their titles name, the sections and
    /// the mutes that rank them.
    pub fn revision(&self) -> u64 {
        self.conversations
            .revision()
            .max(self.users.revision())
            .max(self.sections.revision())
            .max(self.desktop.revision())
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

    /// The user group with this id, if Slack listed it.
    pub fn group(&self, id: &str) -> Option<&UserGroup> {
        self.groups.iter().find(|g| g.id == id)
    }

    /// How a user group mention reads: the handle Slack lists now, else the
    /// label the message was sent with, else the bare id, so a mention is
    /// never lost from the text.
    pub fn group_label(&self, id: &str, label: Option<&str>) -> String {
        self.group(id)
            .map(|g| format!("@{}", g.handle))
            .or_else(|| label.map(str::to_owned))
            .unwrap_or_else(|| format!("@{id}"))
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

    /// Whether a message mentions you (or everyone). Read as the activity
    /// view reads it, so a longer id that starts with yours is not you.
    pub fn mentions_me(&self, message: &Message) -> bool {
        crate::views::mention_reason(&message.text, &self.info.user_id).is_some()
    }

    /// A message's text ready to edit, with people and channels named as
    /// you would type them. See [`to_editable`].
    pub fn editable(&self, wire: &str) -> (String, Vec<(String, String)>) {
        to_editable(wire, |sigil, id| match sigil {
            '@' => self.users.get(id).map(|u| u.label().to_owned()),
            '^' => self.group(id).map(|g| g.handle.clone()),
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
                let held = self.holds_unread(&kept.id);
                merge_conversation(&mut kept, conversation, held);
                conversation = kept;
            }
            merged.push(conversation);
        }
        for existing in &self.conversations {
            // A cached list: keep anything already known that it lacks.
            // The full list leaves out what Slack counts as closed (DMs,
            // say), so one fetched by itself for a message stays too: it
            // is never fetched again.
            let keep = !complete || self.requested_conversations.contains(&existing.id);
            if keep && !merged.iter().any(|c| c.id == existing.id) {
                merged.push(existing.clone());
            }
        }
        *self.conversations = merged;
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
        let held = self.holds_unread(&conversation.id);
        match self.conversation_mut(&conversation.id) {
            Some(existing) => merge_conversation(existing, conversation, held),
            None => self.conversations.push(conversation),
        }
        fetch
    }

    /// Slack says a direct message or group DM was opened or closed in
    /// your sidebar, here or in another client. Channels have no such
    /// state, so word about one changes nothing. Answers whether one just
    /// opened is unknown here and should be fetched, once: the full list
    /// may have been asked for before it opened.
    pub(super) fn opened(&mut self, channel: &str, open: bool) -> bool {
        match self.conversation_mut(channel) {
            Some(conversation) => {
                if conversation.kind.is_dm() {
                    conversation.is_open = Some(open);
                }
                false
            }
            None => open && self.loaded && self.requested_conversations.insert(channel.to_owned()),
        }
    }

    /// You left a conversation, or it was archived or deleted.
    pub(super) fn conversation_gone(&mut self, channel: &str) {
        self.conversations.retain(|c| c.id != channel);
        self.held_unread.remove(channel);
        self.timelines.remove(channel);
        self.threads.retain(|(c, _), _| c != channel);
        if self.active.as_deref() == Some(channel) {
            self.active = None;
        }
    }

    pub(super) fn users_arrived(&mut self, users: Vec<User>) {
        for user in users {
            self.requested_users.remove(&user.id);
            self.users.insert(user.id.clone(), user);
        }
    }

    /// A message fetched to quote, or why it could not be. Returns the
    /// people and apps to fetch, so its author shows by name.
    pub(super) fn quote_arrived(
        &mut self,
        channel: &str,
        ts: &Ts,
        result: Result<Option<Message>, Failure>,
    ) -> Arrived {
        let arrived = match &result {
            Ok(Some(message)) => Arrived {
                users: self.unknown_users(message.user.as_deref().into_iter()),
                bots: self.unknown_bots(std::iter::once(message)),
            },
            _ => Arrived::default(),
        };
        self.quotes.arrived(channel, ts, result);
        arrived
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
        let newest = messages.iter().map(|m| &m.ts).max().cloned();
        let timeline = self.timelines.entry(channel.to_owned()).or_default();
        if !older && timeline.cached {
            // Slack's own newest page replaces the cached copy whole: the
            // copy may hold messages deleted since, or end before a gap.
            // What is newer than both the page and the copy came live
            // while the page was on its way, and stays.
            let line = max_ts(newest.clone(), timeline.cached_newest.take());
            timeline.cached = false;
            timeline.loaded = false;
            timeline
                .messages
                .retain(|m| m.ts.is_local() || line.as_ref().is_none_or(|line| m.ts > *line));
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
        if counts {
            self.count_mention(channel, &message.ts);
        }
    }

    /// Adds an unread mention to `channel` for message `ts`, unless it was
    /// counted already.
    fn count_mention(&mut self, channel: &str, ts: &Ts) {
        if self
            .counted_mentions
            .iter()
            .any(|(c, t)| c == channel && t == ts)
        {
            return;
        }
        let Some(conversation) = self.conversation_mut(channel) else {
            return;
        };
        conversation.mentions += 1;
        if self.counted_mentions.len() >= SEEN_LIMIT {
            self.counted_mentions.pop_front();
        }
        self.counted_mentions
            .push_back((channel.to_owned(), ts.clone()));
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
        let newest = messages.iter().map(|m| &m.ts).max().cloned();
        let (arrived, _) = self.history_arrived(channel, messages, has_more, cursor, false);
        if let Some(timeline) = self.timelines.get_mut(channel) {
            timeline.cached = true;
            timeline.cached_newest = newest;
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
        let newest = messages.iter().map(|m| &m.ts).max().cloned();
        let timeline = self.timelines.entry(channel.to_owned()).or_default();
        // Messages still being sent stay; they go after everything real.
        // So do ones that came live past the page when it reaches the
        // present: they were sent while it was on its way.
        let line = if timeline.cached {
            max_ts(newest, timeline.cached_newest.take())
        } else {
            newest
        };
        let local: Vec<Message> = timeline
            .messages
            .iter()
            .filter(|m| {
                m.ts.is_local() || (!has_newer && line.as_ref().is_some_and(|line| m.ts > *line))
            })
            .cloned()
            .collect();
        timeline.cached = false;
        timeline.messages = messages;
        for message in local {
            timeline.upsert(message);
        }
        (timeline.has_more, timeline.cursor) = older;
        timeline.has_newer = has_newer;
        if !has_newer {
            timeline.release_held();
        }
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
        let newest = messages.iter().map(|m| &m.ts).max().cloned();
        let timeline = self.timelines.entry(channel.to_owned()).or_default();
        timeline.merge(messages);
        timeline.has_newer = has_newer;
        if !has_newer {
            // Messages that came live on the way here; the page asked for
            // may not have had them yet.
            timeline.release_held();
        }
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

    /// A whole thread, parent first. Replies loaded here that are newer
    /// than all of it came live while it was on its way, and stay, as do
    /// replies still being sent.
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
        let key = (channel.to_owned(), ts.clone());
        let newest = messages.iter().map(|m| &m.ts).max().cloned();
        let kept: Vec<Message> = self
            .threads
            .get(&key)
            .into_iter()
            .flat_map(|t| t.messages.iter())
            .filter(|m| m.ts.is_local() || newest.as_ref().is_some_and(|n| m.ts > *n))
            .cloned()
            .collect();
        let live: Vec<&Message> = kept
            .iter()
            .filter(|m| !m.ts.is_local() && m.is_reply())
            .collect();
        let extra = u32::try_from(live.len()).unwrap_or(u32::MAX);
        // The thread's own copy of its parent carries Slack's count, which
        // corrects whatever was counted here; failing that, the replies
        // that came are the count. Either way, live replies past the page
        // add to it.
        let replies = messages.iter().filter(|m| m.ts != ts).count() as u32;
        let mut messages = messages;
        let fresh = messages
            .iter_mut()
            .find(|m| m.ts == ts && m.replies_known)
            .map(|fresh| {
                add_replies(fresh, &live, extra);
                fresh.clone()
            });
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
                None => parent.reply_count = replies.saturating_add(extra),
            }
        }
        let timeline = self.threads.entry(key).or_default();
        timeline.loading = false;
        timeline.loaded = true;
        timeline.messages = messages;
        for message in kept {
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
        if self.is_suppressed(channel, &message.ts) {
            return (Arrived::default(), false);
        }
        if self.echo_arrived(channel, &message) {
            return (Arrived::default(), false);
        }
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
            if new && timeline.has_newer {
                timeline.hold(message);
            } else {
                timeline.upsert(message);
            }
            if new && from_me {
                // Writing in a conversation reads it, marked unread or not.
                self.held_unread.remove(channel);
            }
            let mut mention = false;
            if new && let Some(conversation) = self.conversation_mut(channel) {
                if conversation.latest.as_ref().is_none_or(|l| *l < ts) {
                    conversation.latest = Some(ts.clone());
                }
                // Something new opens a closed direct message again, as
                // in Slack; this does not wait for Slack to say so.
                if conversation.is_open == Some(false) {
                    conversation.is_open = Some(true);
                }
                if from_me {
                    conversation.last_read = Some(ts.clone());
                    conversation.mentions = 0;
                } else {
                    mention =
                        !viewing && (mentions_me || (conversation.kind.is_dm() && counts_all));
                }
            }
            if mention {
                self.count_mention(channel, &ts);
            }
            self.trim_unopened(channel);
        }
        let fetch_conversation = !known && self.requested_conversations.insert(channel.to_owned());
        if !known {
            // The conversation's details name its people; wait for them.
            arrived.users.clear();
        }
        (arrived, fetch_conversation)
    }

    /// Notes an echo of a message sent from here, known by its client id:
    /// its send is settled by it (see [`Self::sent`]). True when that send
    /// was one you deleted meanwhile, whose echo is not to show.
    fn echo_arrived(&mut self, channel: &str, message: &Message) -> bool {
        let Some(id) = &message.client_msg_id else {
            return false;
        };
        if message.user.as_deref() != Some(self.info.user_id.as_str()) {
            return false;
        }
        let Some(local) = self.sent_from_here(id) else {
            return false;
        };
        self.sending.remove(&local);
        self.echoed.insert(local.clone(), message.ts.clone());
        if self.cancelled.contains(&local) {
            self.suppressed
                .insert((channel.to_owned(), message.ts.clone()));
            return true;
        }
        false
    }

    /// The local id of the send still waiting for Slack whose client id
    /// is `id`.
    fn sent_from_here(&self, id: &str) -> Option<Ts> {
        self.sending
            .iter()
            .find(|(_, copy)| copy.client_msg_id.as_deref() == Some(id))
            .map(|(local, _)| local.clone())
    }

    /// Whether a message is the echo of one you deleted while it was
    /// sending, known by its client id before Slack has answered.
    pub(super) fn is_cancelled_echo(&self, message: &Message) -> bool {
        message
            .client_msg_id
            .as_deref()
            .and_then(|id| self.sent_from_here(id))
            .is_some_and(|local| self.cancelled.contains(&local))
    }

    /// Keeps only the newest messages that came live in a conversation
    /// never opened (see [`Self::unopened`]): nobody reads them there, and
    /// opening it loads its history anyway. Its counts live on the
    /// conversation, and stay.
    fn trim_unopened(&mut self, channel: &str) {
        if !self.unopened(channel) {
            return;
        }
        if let Some(timeline) = self.timelines.get_mut(channel) {
            let over = timeline.messages.len().saturating_sub(UNOPENED_LIMIT);
            timeline.messages.drain(..over);
        }
    }

    /// Counts a new reply on every loaded copy of its parent, once. A reply
    /// counted here before is not counted again. Nor is one no newer than
    /// a parent's latest reply when Slack set that, as Slack's own update
    /// of the parent can come before the reply does and counts it already;
    /// a latest reply counted here says nothing of the older ones, as your
    /// own reply can be answered by Slack after someone else's later one.
    fn count_reply(&mut self, channel: &str, reply: &Message) {
        let Some(parent) = reply.thread_ts.clone() else {
            return;
        };
        if self.reply_counted(channel, &reply.ts) {
            return;
        }
        let slacks_line = |copy: &Message, counted: &VecDeque<(String, Ts)>| {
            copy.latest_reply
                .as_ref()
                .filter(|latest| !counted.iter().any(|(c, t)| c == channel && t == *latest))
                .is_some_and(|latest| reply.ts <= *latest)
        };
        let counted = &self.counted_replies;
        let thread = self.threads.get_mut(&(channel.to_owned(), parent.clone()));
        let copies = self
            .timelines
            .get_mut(channel)
            .and_then(|t| t.find_mut(&parent))
            .into_iter()
            .chain(thread.and_then(|t| t.find_mut(&parent)));
        for copy in copies {
            if slacks_line(copy, counted) {
                continue;
            }
            copy.reply_count += 1;
            copy.latest_reply = max_ts(copy.latest_reply.take(), Some(reply.ts.clone()));
            if let Some(user) = &reply.user
                && !copy.reply_users.contains(user)
            {
                copy.reply_users.push(user.clone());
            }
        }
        if self.counted_replies.len() >= SEEN_LIMIT {
            self.counted_replies.pop_front();
        }
        self.counted_replies
            .push_back((channel.to_owned(), reply.ts.clone()));
    }

    /// Whether reply `ts` was counted on its parent here already.
    fn reply_counted(&self, channel: &str, ts: &Ts) -> bool {
        self.counted_replies
            .iter()
            .any(|(c, t)| c == channel && t == ts)
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
            if let Some(held) = timeline.held.iter_mut().find(|m| m.ts == message.ts) {
                held.clone_from(&message);
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
    /// real message, or is marked as failed. A copy an echo took away
    /// already is shown again from what was sent: the real message if it is
    /// not there, or a failed row to retry.
    /// One you deleted while it was sending is deleted now instead.
    pub(super) fn sent(
        &mut self,
        channel: &str,
        local: &Ts,
        result: &Result<Message, Failure>,
    ) -> SendOutcome {
        let echoed = self.echoed.remove(local);
        if self.cancelled.remove(local) {
            self.sending.remove(local);
            let delete = result
                .as_ref()
                .ok()
                .map(|message| message.ts.clone())
                .or(echoed);
            if let Some(ts) = &delete {
                // Its echo may be here already.
                self.suppressed.insert((channel.to_owned(), ts.clone()));
                self.remove_message(channel, ts);
            }
            return SendOutcome::Cancelled { delete };
        }
        if echoed.is_some() && result.is_err() {
            // Its echo stands in for the answer, and is shown already.
            for timeline in self.timelines_for_mut(channel) {
                timeline.messages.retain(|m| &m.ts != local);
            }
            return SendOutcome::Posted;
        }
        let mut found = false;
        for timeline in self.timelines_for_mut(channel) {
            let Some(position) = timeline.messages.iter().position(|m| &m.ts == local) else {
                continue;
            };
            found = true;
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
        match result {
            Ok(message) => {
                self.sending.remove(local);
                if !found && self.find_message(channel, &message.ts).is_none() {
                    self.place(channel, message.clone());
                }
            }
            Err(error) => {
                let copy = self.sending.get(local).cloned();
                if !found && let Some(mut copy) = copy {
                    copy.delivery = Delivery::Failed(error.clone());
                    self.add_local(channel, copy);
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
        {
            self.held_unread.remove(channel);
        }
        if let Ok(message) = result
            && message.in_channel()
            && let Some(conversation) = self.conversation_mut(channel)
        {
            conversation.latest = max_ts(conversation.latest.take(), Some(message.ts.clone()));
            conversation.last_read =
                max_ts(conversation.last_read.take(), Some(message.ts.clone()));
        }
        SendOutcome::Settled
    }

    /// You deleted a message of yours not sent yet. One still sending is
    /// remembered, to delete once Slack answers (see [`Self::sent`]); a
    /// failed one only goes. Call it before taking the copy away.
    pub(super) fn cancel_local(&mut self, channel: &str, local: &Ts) {
        let sending = self
            .find_message(channel, local)
            .is_some_and(|m| m.delivery == Delivery::Sending);
        if sending {
            // Its entry stays until Slack answers, so its echo is known.
            self.cancelled.insert(local.clone());
        } else {
            self.sending.remove(local);
        }
    }

    /// Whether a message is one you deleted while it was sending, whose
    /// echo is not to show.
    pub(super) fn is_suppressed(&self, channel: &str, ts: &Ts) -> bool {
        self.suppressed.contains(&(channel.to_owned(), ts.clone()))
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
    /// The echo of your own "Mark unread" comes this way too, with the
    /// marker already here, and so changes nothing.
    pub(super) fn read_elsewhere(&mut self, channel: &str, ts: Ts) {
        let Some(conversation) = self.conversation_mut(channel) else {
            return;
        };
        let before = conversation.last_read.clone();
        read_up_to(conversation, ts);
        // Read further on another device: no longer to keep unread here.
        if conversation.last_read != before {
            self.held_unread.remove(channel);
        }
    }

    /// Moves `channel`'s read marker back to just before message `ts` (see
    /// [`unread_marker`]) and counts what is unread from there, as the
    /// sidebar shows it. The conversation stays unread while it shows (see
    /// [`Self::holds_unread`]). Returns the marker to tell Slack, or `None`
    /// for a message not sent yet.
    pub(super) fn mark_unread(&mut self, channel: &str, ts: &Ts) -> Option<Ts> {
        if ts.is_local() {
            return None;
        }
        let listed: Vec<&Message> = self
            .timelines
            .get(channel)
            .into_iter()
            .flat_map(|t| t.messages.iter())
            .filter(|m| m.in_channel() && !m.ts.is_local())
            .collect();
        let marker = unread_marker(&listed, ts)?;
        let dm = self.conversation(channel).is_some_and(|c| c.kind.is_dm());
        // A muted direct message counts only what mentions you, as live.
        let counts_all = dm && !self.desktop.is_muted(channel);
        let me = self.info.user_id.as_str();
        // Your own messages are never unread to you, as with the "New" line.
        let theirs: Vec<&&Message> = listed
            .iter()
            .filter(|m| m.ts > marker && m.user.as_deref() != Some(me))
            .collect();
        let unread = u32::try_from(theirs.len()).unwrap_or(u32::MAX);
        let mentions = theirs
            .iter()
            .filter(|m| counts_all || self.mentions_me(m))
            .count();
        let mentions = u32::try_from(mentions).unwrap_or(u32::MAX);
        let newest = listed.last().map(|m| m.ts.clone());
        let conversation = self.conversation_mut(channel)?;
        conversation.last_read = Some(marker.clone());
        // The chosen message may be newer than the latest Slack last told.
        conversation.latest = max_ts(conversation.latest.take(), newest.or(Some(ts.clone())));
        conversation.unread = unread;
        conversation.mentions = mentions;
        self.held_unread.insert(channel.to_owned());
        Some(marker)
    }

    /// Puts a real message into the loaded lists it belongs in: its
    /// thread's and the conversation's own.
    fn place(&mut self, channel: &str, message: Message) {
        if message.is_reply()
            && let Some(parent) = message.thread_ts.clone()
            && let Some(thread) = self.threads.get_mut(&(channel.to_owned(), parent))
        {
            thread.upsert(message.clone());
        }
        if message.in_channel()
            && let Some(timeline) = self.timelines.get_mut(channel)
        {
            timeline.upsert(message);
        }
    }

    /// Shows a message you are sending before Slack has it.
    pub(super) fn add_local(&mut self, channel: &str, message: Message) {
        self.sending.insert(message.ts.clone(), message.clone());
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
                // Slack's layout is of the old text; the new one is the
                // block the edit is sent with, as Slack will show it.
                message
                    .blocks
                    .retain(|block| !matches!(block, KitBlock::RichText(_)));
                message.blocks.extend(sent_layout(wire));
                message.edited = true;
            }
        }
        before
    }

    /// The workspace's custom emoji arrived; those added here that the
    /// list does not have yet stay.
    pub(super) fn emoji_arrived(&mut self, mut emoji: HashMap<String, String>, can_add: bool) {
        self.added_emoji.retain(|name, _| !emoji.contains_key(name));
        for (name, url) in &self.added_emoji {
            emoji.insert(name.clone(), url.clone());
        }
        self.emoji = EmojiSet::new(emoji);
        self.can_add_emoji = can_add;
    }

    /// You added custom emoji `name`: it shows at once, from `url` (the
    /// picture you picked) until Slack's list brings its own.
    pub(super) fn emoji_added(&mut self, name: String, url: String) {
        self.emoji.insert(name.clone(), url.clone());
        self.added_emoji.insert(name, url);
    }

    /// A custom emoji was added, removed or renamed, here or elsewhere.
    /// One added here and waiting for Slack's list follows the change too,
    /// so a later list does not bring back what is gone.
    pub(super) fn emoji_changed(&mut self, change: &crate::emoji::EmojiChange) {
        use crate::emoji::EmojiChange;
        self.emoji.apply(change);
        match change {
            // Slack has it now.
            EmojiChange::Added { name, .. } => {
                self.added_emoji.remove(name);
            }
            EmojiChange::Removed(names) => {
                for name in names {
                    self.added_emoji.remove(name);
                }
            }
            EmojiChange::Renamed { old, .. } => {
                self.added_emoji.remove(old);
            }
        }
    }

    /// You deleted file `id`: it is hidden at once, wherever it shows,
    /// until Slack answers ([`Self::file_delete_settled`]).
    pub(super) fn hide_file(&mut self, id: &str) {
        if !self.gone_files.contains(id) {
            self.deleting_files.insert(id.to_owned());
        }
    }

    /// Slack answered your deletion of file `id`. Taken, it stays hidden
    /// for good; refused, it shows again, unless Slack itself said
    /// meanwhile that it is gone. Returns whether it shows again.
    pub(super) fn file_delete_settled(&mut self, id: &str, deleted: bool) -> bool {
        let was_hidden = self.deleting_files.remove(id);
        if deleted {
            self.gone_files.insert(id.to_owned());
            return false;
        }
        was_hidden && self.shows_file(id)
    }

    /// Slack says file `id` was deleted (`file_deleted`), here or
    /// anywhere: it stays hidden, and nothing brings it back.
    pub(super) fn file_gone(&mut self, id: &str) {
        self.deleting_files.remove(id);
        self.gone_files.insert(id.to_owned());
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

    /// Marks a failed message as sending again. Returns it, to send once
    /// more under the same client id.
    pub(super) fn retry_local(&mut self, channel: &str, local: &Ts) -> Option<Message> {
        let mut found = None;
        for timeline in self.timelines_for_mut(channel) {
            if let Some(message) = timeline.find_mut(local) {
                message.delivery = Delivery::Sending;
                found = Some(message.clone());
            }
        }
        if let Some(message) = &found {
            self.sending.insert(local.clone(), message.clone());
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
/// newer of each marker. A conversation you marked unread (`held`) keeps
/// its read marker and counts: a list asked for before the mark reached
/// Slack still has the old marker and would read it again.
fn merge_conversation(existing: &mut Conversation, fresh: Conversation, held: bool) {
    if held {
        let kept = (
            existing.last_read.clone(),
            existing.unread,
            existing.mentions,
        );
        merge_conversation(existing, fresh, false);
        (existing.last_read, existing.unread, existing.mentions) = kept;
        return;
    }
    let latest = max_ts(existing.latest.take(), fresh.latest.clone());
    let last_read = max_ts(existing.last_read.take(), fresh.last_read.clone());
    let mentions = existing.mentions;
    let unread = if fresh.unread > 0 {
        fresh.unread
    } else {
        existing.unread
    };
    // A copy that does not say (most calls leave `is_open` out) keeps
    // what was known; one that does is the newer word.
    let is_open = fresh.is_open.or(existing.is_open);
    // Known empty stays known until a message is known.
    let empty = latest.is_none() && (fresh.empty || existing.empty);
    *existing = Conversation {
        latest,
        last_read,
        mentions,
        unread,
        is_open,
        empty,
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

/// Where the read marker goes to make message `ts` and all after it
/// unread: on the message before it in `listed` (the conversation's own
/// list, oldest first), which is what Slack's own apps send; when the
/// message is the first loaded, so the one before is not known, one
/// microsecond before it. Slack takes any timestamp for
/// `conversations.mark` and compares messages with it, and no two
/// messages of a conversation share a microsecond, so that counts
/// exactly the same messages as read.
fn unread_marker(listed: &[&Message], ts: &Ts) -> Option<Ts> {
    let before = listed
        .iter()
        .take_while(|m| m.ts < *ts)
        .last()
        .map(|m| m.ts.clone());
    before.or_else(|| ts.just_before())
}

fn max_ts(a: Option<Ts>, b: Option<Ts>) -> Option<Ts> {
    match (a, b) {
        (Some(a), Some(b)) => Some(a.max(b)),
        (a, b) => a.or(b),
    }
}

/// The layout of a message being sent or edited: the rich text it goes
/// with, as Slack's copy will bring it back, so the message looks the same
/// before and after Slack answers. None when it goes as text alone.
fn sent_layout(wire: &str) -> Vec<KitBlock> {
    crate::slack::rich_out::layout(wire)
        .map(|blocks| vec![KitBlock::RichText(blocks.into())])
        .unwrap_or_default()
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
        blocks: sent_layout(wire),
        edited: false,
        subtype: None,
        delivery: Delivery::Sending,
        broadcast,
        pinned: false,
        client_msg_id: Some(crate::model::new_client_msg_id()),
        subscribed: None,
    }
}

/// Counts `live` replies, `extra` of them, on a copy of their parent
/// that Slack's count does not cover yet.
fn add_replies(parent: &mut Message, live: &[&Message], extra: u32) {
    parent.reply_count = parent.reply_count.saturating_add(extra);
    for reply in live {
        parent.latest_reply = max_ts(parent.latest_reply.take(), Some(reply.ts.clone()));
        if let Some(user) = &reply.user
            && !parent.reply_users.contains(user)
        {
            parent.reply_users.push(user.clone());
        }
    }
}

/// A sent message's own echo replaces its optimistic copy. An echo with
/// a client id takes only the copy with that id: one that failed, too, as
/// the echo shows it went after all. One with an id no copy has (sent
/// from another device) takes nothing. Without an id it is matched by
/// text, as long as it is not of a message already here (its answer came
/// first), which would take the place of another copy of the same text.
fn remove_echoed_local(timeline: &mut Timeline, message: &Message, from_me: bool) {
    if !from_me || timeline.messages.iter().any(|m| m.ts == message.ts) {
        return;
    }
    let position = match &message.client_msg_id {
        Some(id) => timeline
            .messages
            .iter()
            .position(|m| m.ts.is_local() && m.client_msg_id.as_ref() == Some(id)),
        None => timeline.messages.iter().position(|m| {
            m.ts.is_local() && m.delivery == Delivery::Sending && m.text == message.text
        }),
    };
    if let Some(position) = position {
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
            is_open: None,
            empty: false,
        };
        let fresh = Conversation {
            name: "renamed".into(),
            last_read: Some(Ts::new("7.0")),
            latest: None,
            mentions: 0,
            ..existing.clone()
        };
        merge_conversation(&mut existing, fresh, false);
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
            is_open: None,
            empty: false,
        }
    }

    #[test]
    fn counts_clear_when_read_elsewhere() {
        let mut existing = conversation("5.0", "9.0", 4, 1);
        merge_conversation(&mut existing, conversation("9.0", "9.0", 4, 0), false);
        assert_eq!(existing.unread, 0);
        assert_eq!(existing.mentions, 0);
        assert!(!existing.has_unread());
        // Still behind: Slack's count stands.
        let mut existing = conversation("5.0", "9.0", 0, 1);
        merge_conversation(&mut existing, conversation("6.0", "9.0", 3, 0), false);
        assert_eq!(existing.unread, 3);
        assert_eq!(existing.mentions, 1);
    }

    #[test]
    fn a_copy_that_does_not_say_keeps_open_and_empty() {
        let mut existing = Conversation {
            kind: ConversationKind::Group,
            latest: None,
            last_read: None,
            is_open: Some(false),
            empty: true,
            ..conversation("1.0", "1.0", 0, 0)
        };
        // `conversations.info` without `is_open`, history not asked.
        let partial = Conversation {
            is_open: None,
            empty: false,
            ..existing.clone()
        };
        merge_conversation(&mut existing, partial, false);
        assert_eq!(existing.is_open, Some(false));
        assert!(existing.empty);
        // A copy that says is the newer word.
        let opened = Conversation {
            is_open: Some(true),
            ..existing.clone()
        };
        merge_conversation(&mut existing, opened, false);
        assert_eq!(existing.is_open, Some(true));
        // A message known: no longer empty.
        let written = Conversation {
            latest: Some(Ts::new("3.0")),
            ..existing.clone()
        };
        merge_conversation(&mut existing, written, false);
        assert!(!existing.empty);
    }

    #[test]
    fn a_new_message_opens_a_closed_direct_message() {
        let mut w = workspace();
        w.conversations.push(Conversation {
            id: "D1".into(),
            kind: ConversationKind::Direct,
            latest: None,
            last_read: None,
            is_open: Some(false),
            empty: true,
            ..conversation("1.0", "1.0", 0, 0)
        });
        w.message_arrived("D1", message("5.0", None), false);
        let d1 = w.conversation("D1").expect("kept");
        assert_eq!(d1.is_open, Some(true));
        assert_eq!(d1.latest, Some(Ts::new("5.0")));
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
            client_msg_id: None,
            subscribed: None,
        }
    }

    fn workspace() -> WorkspaceState {
        WorkspaceState::new(Workspace {
            service: crate::model::Service::Slack,
            team_id: "T1".into(),
            name: "Acme".into(),
            domain: "acme".into(),
            icon: None,
            user_id: "U1".into(),
            sign_in: Default::default(),
            scopes: None,
        })
    }

    /// A message of yours in C1 at `ts` sharing file F1.
    fn with_file(ts: &str) -> Message {
        Message {
            files: vec![crate::model::File {
                id: "F1".into(),
                name: "sidebar-v2.png".into(),
                user: Some("U1".into()),
                ..crate::model::File::default()
            }],
            ..message(ts, None)
        }
    }

    #[test]
    fn a_deleted_file_hides_at_once_and_comes_back_if_refused() {
        let mut w = workspace();
        w.timelines
            .entry("C1".into())
            .or_default()
            .upsert(with_file("1.0"));
        assert!(w.shows_file("F1"));
        w.hide_file("F1");
        assert!(!w.shows_file("F1"), "hidden before Slack answers");
        // A copy of the message from before the deletion (a reaction, say)
        // must not bring it back.
        w.message_changed("C1", with_file("1.0"));
        assert!(!w.shows_file("F1"));
        assert!(
            w.file_delete_settled("F1", false),
            "refused: it shows again"
        );
        assert!(w.shows_file("F1"));
        assert_eq!(w.timelines["C1"].messages[0].files.len(), 1);
        assert!(!w.timelines["C1"].messages[0].files[0].deleted);
        // Taken: it stays hidden, whatever copy comes later.
        w.hide_file("F1");
        assert!(!w.file_delete_settled("F1", true));
        w.message_changed("C1", with_file("1.0"));
        assert!(!w.shows_file("F1"));
    }

    #[test]
    fn file_deleted_and_the_tombstone_agree_with_your_deletion() {
        let mut w = workspace();
        w.timelines
            .entry("C1".into())
            .or_default()
            .upsert(with_file("1.0"));
        w.hide_file("F1");
        // Slack's events come before its answer: the file is gone for
        // good, and the message stays once, with the file in its place.
        w.file_gone("F1");
        let mut tombstone = with_file("1.0");
        tombstone.files = vec![crate::model::File {
            id: "F1".into(),
            deleted: true,
            ..crate::model::File::default()
        }];
        w.message_changed("C1", tombstone);
        let messages = &w.timelines["C1"].messages;
        assert_eq!(messages.len(), 1, "no duplicate");
        assert!(messages[0].files[0].deleted);
        // A late refusal (a retry that found nothing to delete) does not
        // bring back what Slack itself says is gone.
        assert!(!w.file_delete_settled("F1", false));
        assert!(!w.shows_file("F1"));
        // Someone else's deletion, seen only through the event.
        w.file_gone("F9");
        assert!(!w.shows_file("F9"));
        assert!(!w.file_delete_settled("F9", false), "never hidden here");
    }

    #[test]
    fn a_new_emoji_shows_at_once_and_survives_a_list_without_it() {
        let mut w = workspace();
        w.emoji_arrived(
            HashMap::from([("a".to_owned(), "https://x/a.png".to_owned())]),
            true,
        );
        assert!(w.can_add_emoji);
        w.emoji_added("shipit".into(), "bytes://new/shipit.png".into());
        assert!(w.emoji.contains("shipit"));
        // Slack's list from just after may not have it yet.
        w.emoji_arrived(
            HashMap::from([("a".to_owned(), "https://x/a.png".to_owned())]),
            true,
        );
        assert_eq!(
            w.emoji.resolve("shipit"),
            crate::emoji::Resolved::Image("bytes://new/shipit.png".into())
        );
        // Once it does, Slack's address wins.
        w.emoji_arrived(
            HashMap::from([("shipit".to_owned(), "https://x/shipit.png".to_owned())]),
            true,
        );
        assert_eq!(
            w.emoji.resolve("shipit"),
            crate::emoji::Resolved::Image("https://x/shipit.png".into())
        );
        w.emoji_arrived(HashMap::new(), false);
        assert!(
            !w.emoji.contains("shipit"),
            "Slack's word is final after that"
        );
        assert!(!w.can_add_emoji);
    }

    #[test]
    fn an_emoji_removed_elsewhere_does_not_come_back_from_the_wait_for_slack() {
        use crate::emoji::EmojiChange;
        let mut w = workspace();
        w.emoji_added("shipit".into(), "bytes://new/shipit.png".into());
        w.emoji_changed(&EmojiChange::Removed(vec!["shipit".into()]));
        assert!(!w.emoji.contains("shipit"));
        // A list from before the removal reached Slack's list.
        w.emoji_arrived(HashMap::new(), true);
        assert!(!w.emoji.contains("shipit"), "not kept as waiting any more");
        w.emoji_changed(&EmojiChange::Added {
            name: "parrot".into(),
            value: "https://x/parrot.gif".into(),
        });
        assert_eq!(
            w.emoji.resolve("parrot"),
            crate::emoji::Resolved::Image("https://x/parrot.gif".into())
        );
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
    fn your_reply_answered_after_a_later_one_still_counts() {
        let mut w = workspace_with_thread();
        sending(&mut w, "local-1", "mine", Some("1.0"));
        let later = Message {
            user: Some("U2".into()),
            ..message("5.0", Some("1.0"))
        };
        w.message_arrived("C1", later, true);
        assert_eq!(counts(&w), (3, 3));
        let real = Message {
            text: "mine".into(),
            ..message("4.0", Some("1.0"))
        };
        w.sent("C1", &Ts::new("local-1"), &Ok(real.clone()));
        w.message_arrived("C1", real, true);
        assert_eq!(counts(&w), (4, 4));
        assert_eq!(
            w.timelines["C1"].messages[0].latest_reply,
            Some(Ts::new("5.0"))
        );
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
    fn an_echo_of_a_message_answered_already_takes_no_other_copy() {
        let mut w = workspace_in_general();
        sending(&mut w, "local-1", "ok", None);
        sending(&mut w, "local-2", "ok", None);
        w.sent("C1", &Ts::new("local-1"), &Ok(mine("5.0", "ok")));
        w.message_arrived("C1", mine("5.0", "ok"), true);
        assert_eq!(
            texts(&w.timelines["C1"]),
            [("5.0", "ok"), ("local-2", "ok")]
        );
    }

    #[test]
    fn a_copy_an_echo_took_still_settles() {
        // The second "ok" goes through first; its echo takes the first's
        // copy, which then fails.
        let mut w = workspace_in_general();
        sending(&mut w, "local-1", "ok", None);
        sending(&mut w, "local-2", "ok", None);
        w.message_arrived("C1", mine("5.0", "ok"), true);
        w.sent("C1", &Ts::new("local-2"), &Ok(mine("5.0", "ok")));
        w.sent("C1", &Ts::new("local-1"), &Err(Failure::RateLimited));
        let timeline = &w.timelines["C1"];
        assert_eq!(texts(timeline), [("5.0", "ok"), ("local-1", "ok")]);
        assert_eq!(
            timeline.messages[1].delivery,
            Delivery::Failed(Failure::RateLimited)
        );
        assert!(w.retry_local("C1", &Ts::new("local-1")).is_some());
        // The other way: it goes through, and shows from Slack's answer.
        let mut w = workspace_in_general();
        sending(&mut w, "local-1", "ok", None);
        sending(&mut w, "local-2", "ok", None);
        w.message_arrived("C1", mine("6.0", "ok"), true);
        w.sent("C1", &Ts::new("local-2"), &Ok(mine("6.0", "ok")));
        w.sent("C1", &Ts::new("local-1"), &Ok(mine("5.0", "ok")));
        assert_eq!(texts(&w.timelines["C1"]), [("5.0", "ok"), ("6.0", "ok")]);
    }

    #[test]
    fn a_message_deleted_while_sending_is_deleted_once_sent() {
        for echo_first in [true, false] {
            let mut w = workspace_in_general();
            sending(&mut w, "local-1", "oops", None);
            w.cancel_local("C1", &Ts::new("local-1"));
            w.remove_message("C1", &Ts::new("local-1"));
            if echo_first {
                w.message_arrived("C1", mine("5.0", "oops"), true);
            }
            let outcome = w.sent("C1", &Ts::new("local-1"), &Ok(mine("5.0", "oops")));
            assert_eq!(
                outcome,
                SendOutcome::Cancelled {
                    delete: Some(Ts::new("5.0"))
                }
            );
            w.message_arrived("C1", mine("5.0", "oops"), true);
            assert!(
                w.timelines["C1"].messages.is_empty(),
                "echo first: {echo_first}"
            );
        }
        // One that fails instead is simply gone.
        let mut w = workspace_in_general();
        sending(&mut w, "local-1", "oops", None);
        w.cancel_local("C1", &Ts::new("local-1"));
        w.remove_message("C1", &Ts::new("local-1"));
        let outcome = w.sent("C1", &Ts::new("local-1"), &Err(Failure::RateLimited));
        assert_eq!(outcome, SendOutcome::Cancelled { delete: None });
        assert!(w.timelines["C1"].messages.is_empty());
        // A failed one deleted is not sending: nothing to call back.
        let mut w = workspace_in_general();
        sending(&mut w, "local-1", "oops", None);
        w.sent("C1", &Ts::new("local-1"), &Err(Failure::RateLimited));
        w.cancel_local("C1", &Ts::new("local-1"));
        assert!(w.cancelled.is_empty());
    }

    #[test]
    fn a_failed_send_can_be_retried() {
        let mut w = workspace_in_general();
        sending(&mut w, "local-1", "hi", None);
        w.sent("C1", &Ts::new("local-1"), &Err(Failure::RateLimited));
        let failed = &w.timelines["C1"].messages[0];
        assert_eq!(failed.delivery, Delivery::Failed(Failure::RateLimited));
        let failed_id = failed.client_msg_id.clone();
        let retried = w
            .retry_local("C1", &Ts::new("local-1"))
            .expect("the failed copy");
        assert_eq!(retried.text, "hi");
        assert_eq!(
            retried.client_msg_id, failed_id,
            "a retry keeps its client id"
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
    fn a_longer_id_that_starts_with_yours_is_not_a_mention() {
        let w = workspace_in_general();
        let said = |text: &str| Message {
            text: text.into(),
            ..theirs("5.0")
        };
        assert!(!w.mentions_me(&said("hey <@U12>")));
        assert!(!w.mentions_me(&said("hey <@U12|bob>")));
        assert!(w.mentions_me(&said("hey <@U1>")));
        assert!(w.mentions_me(&said("hey <@U1|me>")));
        assert!(w.mentions_me(&said("<!here> look")));
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
    fn a_conversation_fetched_by_itself_outlives_the_full_list() {
        let mut w = workspace_in_general();
        let (_, fetch) = w.message_arrived("D9", theirs("5.0"), false);
        assert!(fetch);
        let mut dm = conversation("1.0", "5.0", 0, 0);
        dm.id = "D9".into();
        dm.kind = ConversationKind::Direct;
        w.conversation_arrived(dm);
        w.active = Some("D9".into());
        // The full list, which leaves the closed DM out.
        w.conversations_arrived(vec![conversation("1.0", "1.0", 0, 0)], true);
        assert!(w.conversation("D9").is_some());
        assert_eq!(w.active.as_deref(), Some("D9"));
        // Anything else the list lacks goes.
        let mut gone = conversation("1.0", "1.0", 0, 0);
        gone.id = "C7".into();
        w.conversation_arrived(gone);
        w.conversations_arrived(vec![conversation("1.0", "1.0", 0, 0)], true);
        assert!(w.conversation("C7").is_none());
        assert!(w.conversation("D9").is_some());
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

    /// General with messages from someone else at 2.0 to 5.0 (4.0 mentions
    /// you), all read.
    fn general_read_through() -> WorkspaceState {
        let mut w = workspace_in_general();
        w.conversations[0] = conversation("5.0", "5.0", 0, 0);
        let timeline = w.timelines.entry("C1".into()).or_default();
        for ts in ["2.0", "3.0", "4.0", "5.0"] {
            let mut theirs = message(ts, None);
            theirs.user = Some("U2".into());
            if ts == "4.0" {
                theirs.text = "hey <@U1>".into();
            }
            timeline.upsert(theirs);
        }
        w
    }

    #[test]
    fn marking_unread_moves_the_marker_back() {
        let mut w = general_read_through();
        // On the message before: the one marked and all after are unread.
        assert_eq!(w.mark_unread("C1", &Ts::new("4.0")), Some(Ts::new("3.0")));
        let c = w.conversation("C1").expect("C1");
        assert_eq!(c.last_read, Some(Ts::new("3.0")));
        assert_eq!((c.unread, c.mentions), (2, 1));
        assert!(w.is_unread(c));
        assert!(w.holds_unread("C1"));
        // The "New" line goes above the message marked.
        let read = c.last_read.clone().expect("marker");
        assert_eq!(
            first_unread(&w.timelines["C1"], &read, "U1"),
            Some(Ts::new("4.0"))
        );
        // A message not sent yet cannot be marked.
        assert_eq!(w.mark_unread("C1", &Ts::new("local-1")), None);
    }

    #[test]
    fn marking_the_first_loaded_message_unread_goes_just_before_it() {
        let mut w = general_read_through();
        let marker = w.mark_unread("C1", &Ts::new("2.0"));
        assert_eq!(marker.as_ref().map(Ts::as_str), Some("1.999999"));
        let c = w.conversation("C1").expect("C1");
        assert_eq!((c.unread, c.mentions), (4, 1));
        assert_eq!(
            first_unread(&w.timelines["C1"], &Ts::new("1.999999"), "U1"),
            Some(Ts::new("2.0"))
        );
    }

    #[test]
    fn a_conversation_marked_unread_stays_unread_while_it_shows() {
        let mut w = general_read_through();
        w.mark_unread("C1", &Ts::new("4.0"));
        // Slack echoes the mark, and a list asked for before it still has
        // the old marker: neither reads it again.
        w.read_elsewhere("C1", Ts::new("3.0"));
        w.conversations_arrived(vec![conversation("5.0", "5.0", 0, 0)], true);
        w.conversation_arrived(conversation("5.0", "5.0", 0, 0));
        let c = w.conversation("C1").expect("C1");
        assert_eq!(c.last_read, Some(Ts::new("3.0")));
        assert_eq!(c.unread, 2);
        // So looking at it (`App::mark_seen`) leaves it be.
        assert!(w.holds_unread("C1"));
        // Opening it anew or marking it read lets it go.
        w.release_unread("C1");
        assert!(!w.holds_unread("C1"));
    }

    #[test]
    fn writing_or_reading_elsewhere_ends_mark_unread() {
        let mut w = general_read_through();
        w.mark_unread("C1", &Ts::new("4.0"));
        w.message_arrived("C1", mine("6.0", "back"), true);
        assert!(!w.holds_unread("C1"));
        assert_eq!(
            w.conversation("C1").and_then(|c| c.last_read.clone()),
            Some(Ts::new("6.0"))
        );
        w.mark_unread("C1", &Ts::new("5.0"));
        assert!(w.holds_unread("C1"));
        w.read_elsewhere("C1", Ts::new("6.0"));
        assert!(!w.holds_unread("C1"));
        assert!(!w.conversation("C1").is_some_and(|c| c.has_unread()));
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
    fn an_edit_draws_the_layout_it_is_sent_with() {
        let mut w = workspace_with_thread();
        let ts = Ts::new("1.0");
        let laid_out: std::sync::Arc<[crate::mrkdwn::Block]> =
            crate::mrkdwn::parse("old layout").into();
        for timeline in w.timelines_for_mut("C1") {
            if let Some(message) = timeline.find_mut(&ts) {
                message.blocks = vec![KitBlock::RichText(laid_out.clone())];
            }
        }
        let before = w.edit_locally("C1", &ts, "*new*").map(Box::new);
        let sent = crate::slack::rich_out::layout("*new*").expect("a layout");
        assert_eq!(
            w.find_message("C1", &ts)
                .and_then(Message::rich_text)
                .map(|blocks| blocks.to_vec()),
            Some(sent),
            "the old layout would show the old words"
        );
        w.undo(
            "C1",
            Change::Edit {
                ts: ts.clone(),
                text: "*new*".into(),
                before,
            },
        );
        assert_eq!(
            w.find_message("C1", &ts).and_then(Message::rich_text),
            Some(&laid_out)
        );
        // Text that goes alone is drawn from its mrkdwn.
        w.edit_locally("C1", &ts, "<!date^1^{date}|then>");
        assert!(
            w.find_message("C1", &ts)
                .is_some_and(|m| m.rich_text().is_none())
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
    fn a_sent_message_looks_the_same_before_and_after_its_echo() {
        let wire = "*Plan* for <@U2>:\n1. ship `it`\n2. :tada:\n&gt; quoted\n```\ncode\n```";
        let local = local_message("U1", &Ts::new("local-1"), wire, &None, false);
        // Slack's copy carries the block the message was sent with.
        let blocks = crate::slack::rich_out::blocks_param(wire).expect("blocks");
        let echo = serde_json::json!({
            "type": "message",
            "ts": "5.0",
            "user": "U1",
            "text": wire,
            "blocks": serde_json::from_str::<serde_json::Value>(&blocks).expect("json"),
        });
        let echo = serde_json::from_value::<crate::slack::types::Message>(echo)
            .ok()
            .and_then(crate::slack::types::Message::into_model)
            .expect("a message");
        assert!(local.rich_text().is_some());
        assert_eq!(local.rich_text(), echo.rich_text());
        assert_eq!(local.blocks, echo.blocks);
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
        // 10.0 came live on the way, and joins once the list gets there.
        assert_eq!(order(&w), ["1.0", "2.0", "3.0", "4.0", "10.0", "local-1"]);
        assert!(!w.timelines["C1"].has_newer);
        w.message_arrived("C1", message("11.0", None), false);
        assert_eq!(w.timelines["C1"].messages.len(), 7);
    }

    #[test]
    fn a_jump_that_reaches_the_present_keeps_what_came_live() {
        let mut w = workspace_in_general();
        w.message_arrived("C1", theirs("8.0"), true);
        w.message_arrived("C1", theirs("9.0"), true);
        w.around_arrived(
            "C1",
            vec![message("7.0", None), message("8.0", None)],
            (true, None),
            false,
        );
        assert_eq!(stamps(&w.timelines["C1"].messages), ["7.0", "8.0", "9.0"]);
        // One that does not reach it stands apart.
        w.around_arrived("C1", vec![message("2.0", None)], (true, None), true);
        assert_eq!(stamps(&w.timelines["C1"].messages), ["2.0"]);
    }

    #[test]
    fn a_message_held_while_behind_follows_its_edits_and_deletes() {
        let mut w = workspace();
        w.conversations.push(conversation("1.0", "9.0", 0, 0));
        w.around_arrived("C1", vec![message("2.0", None)], (false, None), true);
        w.message_arrived("C1", message("10.0", None), false);
        w.message_arrived("C1", message("11.0", None), false);
        w.message_changed("C1", mine("10.0", "edited"));
        w.remove_message("C1", &Ts::new("11.0"));
        w.newer_arrived("C1", vec![message("3.0", None)], false);
        assert_eq!(
            texts(&w.timelines["C1"]),
            [("2.0", "text 2.0"), ("3.0", "text 3.0"), ("10.0", "edited")]
        );
    }

    #[test]
    fn slacks_page_keeps_what_came_live_over_the_cached_copy() {
        let mut w = workspace();
        w.conversations.push(conversation("1.0", "9.0", 0, 0));
        w.cached_history_arrived(
            "C1",
            vec![message("5.0", None), message("6.0", None)],
            false,
            None,
        );
        // 7.0 comes live while Slack's page is on its way; 6.0 was deleted.
        w.message_arrived("C1", theirs("7.0"), false);
        w.history_arrived("C1", vec![message("5.0", None)], false, None, false);
        let timeline = &w.timelines["C1"];
        assert_eq!(stamps(&timeline.messages), ["5.0", "7.0"]);
        assert!(!timeline.cached && timeline.cached_newest.is_none());
    }

    #[test]
    fn a_reply_that_came_live_outlives_the_thread_page() {
        let mut w = workspace_with_thread();
        // Slack's page was asked for before 4.0 was sent.
        w.message_arrived("C1", theirs_in_thread("4.0"), true);
        let mut parent = message("1.0", Some("1.0"));
        parent.reply_count = 2;
        parent.replies_known = true;
        parent.latest_reply = Some(Ts::new("3.0"));
        let page = vec![
            parent,
            message("2.0", Some("1.0")),
            message("3.0", Some("1.0")),
        ];
        w.thread_arrived("C1", Ts::new("1.0"), page);
        let thread = &w.threads[&("C1".to_owned(), Ts::new("1.0"))];
        assert_eq!(stamps(&thread.messages), ["1.0", "2.0", "3.0", "4.0"]);
        assert_eq!(counts(&w), (3, 3));
        let parent = &w.timelines["C1"].messages[0];
        assert_eq!(parent.latest_reply, Some(Ts::new("4.0")));
        assert!(parent.reply_users.iter().any(|u| u == "U2"));
    }

    /// A reply in C1's thread 1.0 from someone else.
    fn theirs_in_thread(ts: &str) -> Message {
        Message {
            user: Some("U2".into()),
            ..message(ts, Some("1.0"))
        }
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
    fn a_polled_mention_counts_once_when_its_live_copy_follows() {
        let mut w = unopened("2.0");
        let ping = Message {
            text: "hey <@U1>".into(),
            ..theirs("3.0")
        };
        w.count_polled("C1", &ping, false);
        w.polled_unopened("C1", std::slice::from_ref(&ping));
        // Its live copy, then Socket Mode delivering it again.
        w.message_arrived("C1", ping.clone(), false);
        w.message_arrived("C1", ping, false);
        assert_eq!(w.conversation("C1").map(|c| c.mentions), Some(1));
    }

    #[test]
    fn an_unopened_conversation_keeps_only_its_newest_live_messages() {
        let mut w = unopened("2.0");
        for i in 0..(UNOPENED_LIMIT + 10) {
            let ping = Message {
                text: "hey <@U1>".into(),
                ..theirs(&format!("{}.0", i + 3))
            };
            w.message_arrived("C1", ping, false);
        }
        let timeline = &w.timelines["C1"];
        assert_eq!(timeline.messages.len(), UNOPENED_LIMIT);
        let newest = format!("{}.0", UNOPENED_LIMIT + 12);
        assert_eq!(timeline.newest().map(Ts::as_str), Some(newest.as_str()));
        let c = w.conversation("C1").expect("C1");
        assert_eq!(c.mentions as usize, UNOPENED_LIMIT + 10);
        assert_eq!(c.latest, Some(Ts::new(newest)));
        // An open one keeps everything.
        let mut w = workspace_in_general();
        for i in 0..(UNOPENED_LIMIT + 10) {
            w.message_arrived("C1", theirs(&format!("{}.0", i + 3)), true);
        }
        assert_eq!(w.timelines["C1"].messages.len(), UNOPENED_LIMIT + 10);
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

    /// Slack's copy of local message `local`, as `ts`: same text, same
    /// client id.
    fn echo_of(w: &WorkspaceState, local: &str, ts: &str) -> Message {
        let copy = &w.sending[&Ts::new(local)];
        Message {
            client_msg_id: copy.client_msg_id.clone(),
            subscribed: None,
            ..mine(ts, &copy.text)
        }
    }

    #[test]
    fn client_ids_are_version_4_uuids() {
        let id = crate::model::new_client_msg_id();
        let parts: Vec<&str> = id.split('-').collect();
        assert_eq!(
            parts.iter().map(|p| p.len()).collect::<Vec<_>>(),
            [8, 4, 4, 4, 12]
        );
        assert!(
            id.chars()
                .all(|c| c == '-' || c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
        );
        assert!(parts[2].starts_with('4'));
        assert!(matches!(parts[3].as_bytes()[0], b'8' | b'9' | b'a' | b'b'));
        assert_ne!(id, crate::model::new_client_msg_id());
    }

    #[test]
    fn an_echo_takes_the_copy_with_its_client_id_not_its_text() {
        // Two "ok"s; the second's echo comes first and takes only its own
        // copy, so the first still settles normally when it fails.
        let mut w = workspace_in_general();
        sending(&mut w, "local-1", "ok", None);
        sending(&mut w, "local-2", "ok", None);
        let echo = echo_of(&w, "local-2", "5.0");
        w.message_arrived("C1", echo, true);
        assert_eq!(
            texts(&w.timelines["C1"]),
            [("5.0", "ok"), ("local-1", "ok")]
        );
        w.sent("C1", &Ts::new("local-2"), &Ok(mine("5.0", "ok")));
        w.sent("C1", &Ts::new("local-1"), &Err(Failure::RateLimited));
        let timeline = &w.timelines["C1"];
        assert_eq!(texts(timeline), [("5.0", "ok"), ("local-1", "ok")]);
        assert_eq!(
            timeline.messages[1].delivery,
            Delivery::Failed(Failure::RateLimited)
        );
    }

    #[test]
    fn an_echo_with_a_client_id_from_elsewhere_takes_no_copy() {
        // "ok" typed on your phone while an "ok" is sending here.
        let mut w = workspace_in_general();
        sending(&mut w, "local-1", "ok", None);
        let elsewhere = Message {
            client_msg_id: Some(crate::model::new_client_msg_id()),
            subscribed: None,
            ..mine("5.0", "ok")
        };
        w.message_arrived("C1", elsewhere, true);
        assert_eq!(
            texts(&w.timelines["C1"]),
            [("5.0", "ok"), ("local-1", "ok")]
        );
        assert_eq!(w.timelines["C1"].messages[1].delivery, Delivery::Sending);
    }

    #[test]
    fn an_echo_shows_a_send_went_even_when_its_answer_failed() {
        // Slack's answer timed out, but the message was posted.
        let mut w = workspace_in_general();
        sending(&mut w, "local-1", "hi", None);
        let echo = echo_of(&w, "local-1", "5.0");
        w.message_arrived("C1", echo, true);
        let outcome = w.sent("C1", &Ts::new("local-1"), &Err(Failure::RateLimited));
        assert_eq!(outcome, SendOutcome::Posted);
        assert_eq!(texts(&w.timelines["C1"]), [("5.0", "hi")]);
        assert_eq!(w.timelines["C1"].messages[0].delivery, Delivery::Sent);
        // Answered the other way round: the failed copy gives way to the
        // late echo.
        let mut w = workspace_in_general();
        sending(&mut w, "local-1", "hi", None);
        let echo = echo_of(&w, "local-1", "5.0");
        w.sent("C1", &Ts::new("local-1"), &Err(Failure::RateLimited));
        w.message_arrived("C1", echo, true);
        assert_eq!(texts(&w.timelines["C1"]), [("5.0", "hi")]);
    }

    #[test]
    fn the_echo_of_a_send_you_deleted_never_shows() {
        let mut w = workspace_in_general();
        sending(&mut w, "local-1", "oops", None);
        let echo = echo_of(&w, "local-1", "5.0");
        w.cancel_local("C1", &Ts::new("local-1"));
        w.remove_message("C1", &Ts::new("local-1"));
        assert!(w.is_cancelled_echo(&echo));
        w.message_arrived("C1", echo, true);
        assert!(texts(&w.timelines["C1"]).is_empty());
        // Even when Slack's answer then fails, the posted message goes.
        let outcome = w.sent("C1", &Ts::new("local-1"), &Err(Failure::RateLimited));
        assert_eq!(
            outcome,
            SendOutcome::Cancelled {
                delete: Some(Ts::new("5.0"))
            }
        );
    }

    #[test]
    fn the_revision_moves_with_what_the_sidebar_reads_and_only_then() {
        let mut w = workspace();
        let mut seen = vec![w.revision()];
        let mut moved = |w: &WorkspaceState, what: &str| {
            assert!(!seen.contains(&w.revision()), "{what} kept the revision");
            seen.push(w.revision());
        };
        w.conversations_arrived(vec![conversation("1.0", "1.0", 0, 0)], true);
        moved(&w, "the list");
        // Reading, as a frame does, changes nothing.
        let revision = w.revision();
        let c1 = w.conversation("C1").cloned().expect("C1");
        let _ = (w.title(&c1), w.rank(&c1), w.is_unread(&c1));
        let _ = (w.users_version(), w.conversations.len(), w.users.get("U1"));
        let _ = (w.sections.as_deref(), w.desktop.is_muted("C1"));
        for c in &w.conversations {
            let _ = &c.id;
        }
        assert_eq!(w.revision(), revision);
        w.conversation_arrived(conversation("1.0", "2.0", 1, 0));
        moved(&w, "fresh details");
        if let Some(c) = w.conversation_mut("C1") {
            c.mentions = 3;
        }
        moved(&w, "a change in place");
        w.users_arrived(vec![User {
            id: "U2".into(),
            ..User::default()
        }]);
        moved(&w, "people");
        *w.sections = Some(Vec::new());
        moved(&w, "sections");
        w.desktop.local_muted.insert("C1".into());
        moved(&w, "a mute");
        w.conversation_gone("C1");
        moved(&w, "leaving");
    }
}
