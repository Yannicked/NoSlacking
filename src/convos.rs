//! Starting, finding and looking after conversations: a new direct message
//! with one or more people, the channel browser, joining, leaving and
//! creating channels, a channel's details (topic, purpose, members, files,
//! pinned messages and bookmarks), pinning messages, and adding, editing
//! and removing bookmarks.
//!
//! Views push [`Action`]s (wrapped in [`crate::model::Action::Convos`]);
//! [`apply`] turns them into [`Command`]s for the worker, whose answers come
//! back as [`Event`]s for [`handle`]. What the dialogs hold lives in
//! [`State`], kept on [`App`].

use std::collections::{HashMap, HashSet};

use crate::app::{App, WorkspaceState};
use crate::backend;
// Why a request failed; `Failure` here names which request it was.
use crate::failure::Failure as Why;
use crate::i18n::tf;
use crate::model::{ConversationKind, File, Message, Ts, User};

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
    Open {
        users: Vec<String>,
    },
    /// Shows the channel browser and lists the public channels.
    Browse,
    /// Joins a public channel and opens it.
    Join {
        channel: String,
    },
    /// Asks whether to leave a channel.
    AskLeave {
        channel: String,
    },
    /// Leaves a channel.
    Leave {
        channel: String,
    },
    /// Shows the "Create a channel" dialog.
    NewChannel,
    /// Creates a channel with this (already checked) name and opens it.
    Create {
        name: String,
        private: bool,
    },
    /// Shows a conversation's details beside it, on `tab`, loading what
    /// the tab needs.
    Details {
        channel: String,
        tab: Tab,
    },
    CloseDetails,
    /// Sets a conversation's topic or purpose.
    Describe {
        channel: String,
        field: Field,
        text: String,
    },
    /// Pins a message to its conversation, or unpins it.
    Pin {
        channel: String,
        ts: Ts,
        pin: bool,
    },
    /// Shows the bookmark dialog: empty to add one, or filled with
    /// `bookmark` to edit it.
    AskBookmark {
        channel: String,
        bookmark: Option<Bookmark>,
    },
    /// Adds a bookmark (no `id`) or saves the one with this `id`, from the
    /// dialog's already checked fields.
    SaveBookmark {
        channel: String,
        id: Option<String>,
        form: BookmarkForm,
    },
    /// Asks whether to remove a bookmark.
    AskRemoveBookmark {
        channel: String,
        bookmark: Bookmark,
    },
    /// Removes a bookmark.
    RemoveBookmark {
        channel: String,
        id: String,
    },
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
    /// When and by whom a conversation was made (`conversations.info`).
    About { channel: String },
    /// Who is in a conversation (`conversations.members`).
    Members { channel: String },
    /// The files shared in a conversation (`files.list`).
    Files { channel: String },
    /// `conversations.setTopic` or `conversations.setPurpose`.
    Describe {
        channel: String,
        field: Field,
        text: String,
    },
    /// The pinned messages (`pins.list`).
    Pins { channel: String },
    /// The bookmarks (`bookmarks.list`).
    Bookmarks { channel: String },
    /// `pins.add` or `pins.remove`.
    Pin { channel: String, ts: Ts, pin: bool },
    /// `bookmarks.add`, `bookmarks.edit` or `bookmarks.remove`, already
    /// shown as done.
    Bookmark {
        channel: String,
        change: BookmarkChange,
    },
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
            Self::About { channel }
            | Self::Members { channel }
            | Self::Files { channel }
            | Self::Pins { channel }
            | Self::Bookmarks { channel } => Failure::Load {
                channel: channel.clone(),
            },
            Self::Pin { channel, ts, pin } => Failure::Pin {
                channel: channel.clone(),
                ts: ts.clone(),
                pin: *pin,
            },
            Self::Describe { channel, field, .. } => Failure::Describe {
                channel: channel.clone(),
                field: *field,
            },
            Self::Bookmark { channel, change } => Failure::Bookmark {
                channel: channel.clone(),
                change: change.clone(),
            },
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
    About {
        channel: String,
        result: Result<About, Why>,
    },
    Members {
        channel: String,
        result: Result<Vec<String>, Why>,
    },
    Files {
        channel: String,
        result: Result<Vec<SharedFile>, Why>,
    },
    Pins {
        channel: String,
        result: Result<Vec<Pin>, Why>,
    },
    Bookmarks {
        channel: String,
        result: Result<Vec<Bookmark>, Why>,
    },
    /// A message was pinned or unpinned, maybe by someone else.
    Pinned {
        channel: String,
        ts: Ts,
        pinned: bool,
        by: Option<String>,
    },
    /// Slack saved a bookmark added here: it takes the place of the one
    /// shown under the `local` id until now.
    BookmarkSaved {
        channel: String,
        local: String,
        bookmark: Bookmark,
    },
    /// A bookmark was added or changed, maybe by someone else.
    BookmarkChanged { channel: String, bookmark: Bookmark },
    /// A bookmark was removed, maybe by someone else.
    BookmarkRemoved { channel: String, id: String },
    /// Slack refused, or could not be reached.
    Failed { what: Failure, error: Why },
}

/// Which request failed, so the interface can say so in its own words.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Failure {
    Open,
    Browse,
    Join,
    Leave,
    Create,
    /// Loading a part of the details. Slack's refusals come in the part's
    /// own event; this is for when the request could not be made at all.
    Load {
        channel: String,
    },
    /// Setting a topic or purpose; the one shown is fetched again.
    Describe {
        channel: String,
        field: Field,
    },
    /// Pinning (`pin`) or unpinning a message, already shown as done.
    Pin {
        channel: String,
        ts: Ts,
        pin: bool,
    },
    /// Adding, editing or removing a bookmark, already shown as done.
    Bookmark {
        channel: String,
        change: BookmarkChange,
    },
}

/// A conversation's text that its members can change.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Field {
    Topic,
    Purpose,
}

/// A tab of the details panel.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Tab {
    #[default]
    About,
    Members,
    Pins,
    Bookmarks,
    Files,
}

/// A pinned message.
#[derive(Clone, Debug, PartialEq)]
pub struct Pin {
    pub message: Message,
    /// Who pinned it.
    pub by: Option<String>,
}

/// A link saved at the top of a conversation.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Bookmark {
    pub id: String,
    pub title: String,
    pub link: String,
    /// Its emoji shortcode, without colons, if any.
    pub emoji: Option<String>,
}

/// What starts the id of a bookmark shown before Slack has saved it.
const LOCAL_BOOKMARK: &str = "local-";

impl Bookmark {
    /// Whether it is still waiting for Slack, and so has no id Slack knows.
    pub fn is_local(&self) -> bool {
        self.id.starts_with(LOCAL_BOOKMARK)
    }
}

/// A change to a conversation's bookmarks, with what it takes to undo it
/// should Slack refuse.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BookmarkChange {
    /// A new bookmark, shown under a local id until Slack names it.
    Add(Bookmark),
    /// A bookmark as it was, and as it is to be.
    Edit { before: Bookmark, after: Bookmark },
    /// A bookmark taken out of the list at `at`, to put back there.
    Remove { bookmark: Bookmark, at: usize },
}

impl BookmarkChange {
    /// Shows the change in a loaded list of bookmarks.
    pub fn apply(&self, list: &mut Vec<Bookmark>) {
        match self {
            Self::Add(bookmark) => upsert_bookmark(list, bookmark.clone()),
            Self::Edit { after, .. } => upsert_bookmark(list, after.clone()),
            Self::Remove { bookmark, .. } => list.retain(|b| b.id != bookmark.id),
        }
    }

    /// Takes the change back out of a loaded list, after Slack refused it.
    pub fn undo(&self, list: &mut Vec<Bookmark>) {
        match self {
            Self::Add(bookmark) => list.retain(|b| b.id != bookmark.id),
            Self::Edit { before, .. } => upsert_bookmark(list, before.clone()),
            Self::Remove { bookmark, at } => {
                if !list.iter().any(|b| b.id == bookmark.id) {
                    list.insert((*at).min(list.len()), bookmark.clone());
                }
            }
        }
    }
}

/// Puts a bookmark in place of the one with its id, or at the end, where
/// Slack adds new ones.
pub fn upsert_bookmark(list: &mut Vec<Bookmark>, bookmark: Bookmark) {
    match list.iter_mut().find(|b| b.id == bookmark.id) {
        Some(old) => *old = bookmark,
        None => list.push(bookmark),
    }
}

/// Puts the bookmark Slack saved in place of the one shown under the
/// `local` id. Slack's own event for it may have come first, so it is
/// never there twice.
pub fn saved_bookmark(list: &mut Vec<Bookmark>, local: &str, bookmark: Bookmark) {
    list.retain(|b| b.id != bookmark.id);
    match list.iter_mut().find(|b| b.id == local) {
        Some(shown) => *shown = bookmark,
        None => list.push(bookmark),
    }
}

/// What the bookmark dialog sends: a title, a web link, and an emoji
/// shortcode without colons.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct BookmarkForm {
    pub title: String,
    pub link: String,
    pub emoji: Option<String>,
}

/// Why the bookmark dialog's fields cannot be saved.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BookmarkProblem {
    NoLink,
    /// Only web links (http and https) can be bookmarked.
    NotALink,
    /// The emoji is not a shortcode such as `rocket`.
    NotAnEmoji,
}

/// The link Slack gets for what was typed: a web address, with `https://`
/// put in front of a bare domain such as `example.com`, as browsers do.
pub fn bookmark_link(typed: &str) -> Result<String, BookmarkProblem> {
    let typed = typed.trim();
    if typed.is_empty() {
        return Err(BookmarkProblem::NoLink);
    }
    if typed.chars().any(char::is_whitespace) {
        return Err(BookmarkProblem::NotALink);
    }
    let link = if typed.contains("://") {
        typed.to_owned()
    } else if typed.contains('.') && !typed.contains(':') && !typed.starts_with('.') {
        format!("https://{typed}")
    } else {
        return Err(BookmarkProblem::NotALink);
    };
    let web = link.split_once(':').is_some_and(|(scheme, _)| {
        scheme.eq_ignore_ascii_case("http") || scheme.eq_ignore_ascii_case("https")
    });
    if web && crate::mrkdwn::is_openable(&link) {
        Ok(link)
    } else {
        Err(BookmarkProblem::NotALink)
    }
}

/// The emoji shortcode for what was typed (`rocket` or `:rocket:`); none
/// when nothing was.
pub fn bookmark_emoji(typed: &str) -> Result<Option<String>, BookmarkProblem> {
    let name = typed.trim().trim_matches(':');
    if name.is_empty() {
        return Ok(None);
    }
    let shortcode = name
        .chars()
        .all(|c| c.is_alphanumeric() || matches!(c, '_' | '-' | '+' | '\''));
    if shortcode {
        Ok(Some(name.to_lowercase()))
    } else {
        Err(BookmarkProblem::NotAnEmoji)
    }
}

/// The dialog's fields, checked. An empty title becomes the link, as in
/// Slack's own dialog.
pub fn bookmark_form(
    title: &str,
    link: &str,
    emoji: &str,
) -> Result<BookmarkForm, BookmarkProblem> {
    let link = bookmark_link(link)?;
    let emoji = bookmark_emoji(emoji)?;
    let title = match title.trim() {
        "" => link.clone(),
        title => title.to_owned(),
    };
    Ok(BookmarkForm { title, link, emoji })
}

/// The dialog that adds a bookmark or edits one.
#[derive(Clone, Debug, Default)]
pub struct BookmarkDialog {
    pub channel: String,
    /// The bookmark being edited; none when adding one.
    pub editing: Option<Bookmark>,
    pub title: String,
    pub link: String,
    pub emoji: String,
}

impl BookmarkDialog {
    /// The dialog for `channel`, filled from `bookmark` when editing it.
    pub fn new(channel: String, bookmark: Option<Bookmark>) -> Self {
        let (title, link, emoji) = bookmark
            .as_ref()
            .map(|b| {
                (
                    b.title.clone(),
                    b.link.clone(),
                    b.emoji.clone().unwrap_or_default(),
                )
            })
            .unwrap_or_default();
        Self {
            channel,
            editing: bookmark,
            title,
            link,
            emoji,
        }
    }
}

/// Something the details panel fetches when it is first shown.
#[derive(Clone, Debug, Default, PartialEq)]
pub enum Loaded<T> {
    #[default]
    Idle,
    Loading,
    Ready(T),
    Failed(Why),
}

impl<T> Loaded<T> {
    /// Whether it should be asked for: never yet, or it failed.
    pub fn wanted(&self) -> bool {
        matches!(self, Self::Idle | Self::Failed(_))
    }
}

impl<T> From<Result<T, Why>> for Loaded<T> {
    fn from(result: Result<T, Why>) -> Self {
        match result {
            Ok(value) => Self::Ready(value),
            Err(error) => Self::Failed(error),
        }
    }
}

/// When and by whom a conversation was made.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct About {
    /// Seconds since the epoch.
    pub created: Option<i64>,
    pub creator: Option<String>,
}

/// A file shared in a conversation, as its Files tab lists it.
#[derive(Clone, Debug, PartialEq)]
pub struct SharedFile {
    pub file: File,
    /// Who shared it.
    pub user: Option<String>,
    /// Seconds since the epoch.
    pub created: Option<i64>,
}

/// What the details panel has loaded for one conversation.
#[derive(Clone, Debug, Default)]
pub struct ChannelData {
    pub about: Loaded<About>,
    pub members: Loaded<Vec<String>>,
    pub files: Loaded<Vec<SharedFile>>,
    pub pins: Loaded<Vec<Pin>>,
    pub bookmarks: Loaded<Vec<Bookmark>>,
}

/// The details panel beside a conversation.
#[derive(Clone, Debug, Default)]
pub struct Details {
    pub channel: String,
    pub tab: Tab,
    /// The topic or purpose being edited, and the text so far.
    pub editing: Option<(Field, String)>,
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
    pub error: Option<Why>,
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
    /// The bookmark dialog, when it is open.
    pub bookmark: Option<BookmarkDialog>,
    /// A bookmark waiting for "Remove?" to be answered, and its channel.
    pub remove_bookmark: Option<(String, Bookmark)>,
    /// How many bookmarks were added while the app ran, to give each a
    /// local id until Slack names it.
    added_bookmarks: u64,
    pub details: Option<Details>,
    /// What the details panel loaded, by team and conversation. Kept while
    /// the app runs, so going back to a channel shows it at once.
    pub data: HashMap<(String, String), ChannelData>,
}

impl State {
    /// Whether one of these dialogs covers the window, so its keys come
    /// first.
    pub fn overlay_open(&self) -> bool {
        self.new_message.is_some()
            || self.browse.is_some()
            || self.new_channel.is_some()
            || self.leave.is_some()
            || self.bookmark.is_some()
            || self.remove_bookmark.is_some()
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

/// A conversation you have, as the quick switcher and the "Share
/// message" picker list it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Candidate {
    pub id: String,
    /// The channel's name, or the person (or people) for a DM.
    pub title: String,
    pub kind: ConversationKind,
    pub unread: bool,
    pub archived: bool,
    /// When its newest message came, in seconds; 0 when not known.
    pub latest: i64,
}

/// Every conversation of `workspace`, as [`conversations_matching`]
/// takes them.
pub fn candidates(workspace: &WorkspaceState) -> Vec<Candidate> {
    workspace
        .conversations
        .iter()
        .map(|c| Candidate {
            id: c.id.clone(),
            title: workspace.title(c),
            kind: c.kind,
            unread: c.has_unread(),
            archived: c.archived,
            latest: c.latest.as_ref().and_then(Ts::seconds).unwrap_or(0),
        })
        .collect()
}

/// Changes whenever what [`candidates`] gives for `workspace` may have:
/// a conversation's name, unread state, newest message or archiving, or
/// the people DMs are titled by. Far cheaper than the candidates
/// themselves, so a picker can keep its matches until it changes.
pub fn candidates_fingerprint(workspace: &WorkspaceState) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    (workspace.users.len(), workspace.users_version()).hash(&mut hasher);
    for c in &workspace.conversations {
        (
            &c.id,
            &c.name,
            &c.user,
            c.archived,
            c.has_unread(),
            &c.latest,
        )
            .hash(&mut hasher);
    }
    hasher.finish()
}

/// The conversations whose title holds `query`, best first: titles that
/// start with it, then unread ones, then the busiest lately.
pub fn conversations_matching(candidates: Vec<Candidate>, query: &str) -> Vec<Candidate> {
    let needle = query.trim().trim_start_matches(['#', '@']).to_lowercase();
    // Each title lowered once, not again in every comparison of the sort.
    let mut found: Vec<(bool, Candidate)> = candidates
        .into_iter()
        .filter_map(|c| {
            let title = c.title.to_lowercase();
            title
                .contains(&needle)
                .then(|| (!title.starts_with(&needle), c))
        })
        .collect();
    found.sort_by_key(|(later, c)| (*later, !c.unread, std::cmp::Reverse(c.latest)));
    found.into_iter().map(|(_, c)| c).collect()
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

/// How far someone's clock is from yours.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ZoneDifference {
    /// Their clock shows a later time than yours.
    pub ahead: bool,
    pub hours: u32,
    pub minutes: u32,
}

/// The difference between their offset from UTC and yours, both in
/// seconds; `None` when the clocks agree.
pub fn zone_difference(theirs: i32, mine: i32) -> Option<ZoneDifference> {
    let gap = i64::from(theirs) - i64::from(mine);
    if gap == 0 {
        return None;
    }
    let minutes = u32::try_from(gap.unsigned_abs() / 60).unwrap_or(u32::MAX);
    Some(ZoneDifference {
        ahead: gap > 0,
        hours: minutes / 60,
        minutes: minutes % 60,
    })
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
        Action::Details { channel, tab } => details(app, team, channel, tab),
        Action::CloseDetails => app.convos.details = None,
        Action::Describe {
            channel,
            field,
            text,
        } => {
            if let Some(details) = &mut app.convos.details {
                details.editing = None;
            }
            // Shown at once; Slack's refusal fetches the real one back.
            if let Some(conversation) = app
                .active_workspace_mut()
                .and_then(|w| w.conversation_mut(&channel))
            {
                match field {
                    Field::Topic => conversation.topic.clone_from(&text),
                    Field::Purpose => conversation.purpose.clone_from(&text),
                }
            }
            send(
                app,
                team,
                Command::Describe {
                    channel,
                    field,
                    text,
                },
            );
        }
        Action::Pin { channel, ts, pin } => {
            if ts.is_local() {
                return;
            }
            let me = app.active_workspace().map(|w| w.info.user_id.clone());
            pinned(app, &team, &channel, &ts, pin, me);
            send(app, team, Command::Pin { channel, ts, pin });
        }
        Action::AskBookmark { channel, bookmark } => {
            app.focus_overlay = true;
            app.convos.bookmark = Some(BookmarkDialog::new(channel, bookmark));
        }
        Action::SaveBookmark { channel, id, form } => {
            app.convos.bookmark = None;
            // A copy: the count of added bookmarks lives beside the list.
            let list = app
                .convos
                .data(&team, &channel)
                .map(|d| d.bookmarks.clone());
            let Some(change) = app.convos.bookmark_change(list.as_ref(), id, form) else {
                return;
            };
            change_bookmarks(app, &team, &channel, &change, false);
            send(app, team, Command::Bookmark { channel, change });
        }
        Action::AskRemoveBookmark { channel, bookmark } => {
            app.convos.remove_bookmark = Some((channel, bookmark));
        }
        Action::RemoveBookmark { channel, id } => {
            app.convos.remove_bookmark = None;
            let Some(change) = app
                .convos
                .data(&team, &channel)
                .and_then(|d| removal(&d.bookmarks, &id))
            else {
                return;
            };
            change_bookmarks(app, &team, &channel, &change, false);
            send(app, team, Command::Bookmark { channel, change });
        }
    }
}

impl State {
    /// The change that adds a bookmark from `form`, or (with `id`) edits
    /// the one with that id in the loaded `list`; none when it is not
    /// loaded, or nothing changed.
    fn bookmark_change(
        &mut self,
        list: Option<&Loaded<Vec<Bookmark>>>,
        id: Option<String>,
        form: BookmarkForm,
    ) -> Option<BookmarkChange> {
        let BookmarkForm { title, link, emoji } = form;
        let Some(id) = id else {
            self.added_bookmarks += 1;
            return Some(BookmarkChange::Add(Bookmark {
                id: format!("{LOCAL_BOOKMARK}{}", self.added_bookmarks),
                title,
                link,
                emoji,
            }));
        };
        let Some(Loaded::Ready(list)) = list else {
            return None;
        };
        let before = list.iter().find(|b| b.id == id)?.clone();
        let after = Bookmark {
            id,
            title,
            link,
            emoji,
        };
        (after != before).then_some(BookmarkChange::Edit { before, after })
    }
}

/// The change that removes the bookmark `id` from a loaded list.
fn removal(list: &Loaded<Vec<Bookmark>>, id: &str) -> Option<BookmarkChange> {
    let Loaded::Ready(list) = list else {
        return None;
    };
    let at = list.iter().position(|b| b.id == id)?;
    Some(BookmarkChange::Remove {
        bookmark: list[at].clone(),
        at,
    })
}

/// Shows a bookmark change in the channel's loaded list, or takes it back
/// out (`undo`).
fn change_bookmarks(app: &mut App, team: &str, channel: &str, change: &BookmarkChange, undo: bool) {
    if let Loaded::Ready(list) = &mut app.convos.data_mut(team, channel).bookmarks {
        if undo {
            change.undo(list);
        } else {
            change.apply(list);
        }
    }
}

/// Why a bookmark change was taken back, in words.
fn bookmark_failure(change: &BookmarkChange, error: &Why) -> String {
    // An app made from the first manifest lacks `bookmarks:write`; the
    // interface hides the controls once it knows, and says which
    // permission is missing when it did not know yet.
    if *error == Why::MissingPermission {
        return crate::i18n::t("Changing bookmarks needs the bookmarks:write permission, which your Slack app does not have.").into_owned();
    }
    let error = error.message();
    match change {
        BookmarkChange::Add(_) => tf("Could not add the bookmark: {error}", &[("error", &error)]),
        BookmarkChange::Edit { .. } => {
            tf("Could not save the bookmark: {error}", &[("error", &error)])
        }
        BookmarkChange::Remove { .. } => tf(
            "Could not remove the bookmark: {error}",
            &[("error", &error)],
        ),
    }
}

/// Shows a message as pinned or not wherever it is loaded, and keeps a
/// loaded list of pins in step with it.
fn pinned(app: &mut App, team: &str, channel: &str, ts: &Ts, pin: bool, by: Option<String>) {
    let Some(workspace) = app.workspaces.iter_mut().find(|w| w.info.team_id == team) else {
        return;
    };
    let message = set_pinned(workspace, channel, ts, pin);
    let data = app.convos.data_mut(team, channel);
    let Loaded::Ready(pins) = &mut data.pins else {
        return;
    };
    pins.retain(|p| p.message.ts != *ts);
    if pin {
        match message {
            Some(message) => pins.insert(0, Pin { message, by }),
            // Not loaded here: fetch the list again when it is next shown.
            None => data.pins = Loaded::Idle,
        }
    }
}

/// Marks every loaded copy of a message as pinned or not, and returns it
/// as it now is, if it is loaded.
pub fn set_pinned(
    workspace: &mut WorkspaceState,
    channel: &str,
    ts: &Ts,
    pinned: bool,
) -> Option<Message> {
    let mut found = None;
    for timeline in workspace.timelines_for_mut(channel) {
        if let Some(message) = timeline.find_mut(ts) {
            message.pinned = pinned;
            found = Some(message.clone());
        }
    }
    found
}

/// Opens the details panel on `tab`, in place of a thread, and asks for
/// what the tab shows unless it is loaded already.
fn details(app: &mut App, team: String, channel: String, tab: Tab) {
    app.thread = None;
    let editing = app
        .convos
        .details
        .take()
        .filter(|d| d.channel == channel)
        .and_then(|d| d.editing);
    app.convos.details = Some(Details {
        channel: channel.clone(),
        tab,
        editing,
    });
    let data = app
        .convos
        .data
        .entry((team.clone(), channel.clone()))
        .or_default();
    let mut commands = Vec::new();
    // About shows the members' count too, and is cheap: always fresh.
    if data.about.wanted() || tab == Tab::About {
        data.about = Loaded::Loading;
        commands.push(Command::About {
            channel: channel.clone(),
        });
    }
    match tab {
        Tab::Members if data.members.wanted() => {
            data.members = Loaded::Loading;
            commands.push(Command::Members { channel });
        }
        Tab::Pins if data.pins.wanted() => {
            data.pins = Loaded::Loading;
            commands.push(Command::Pins { channel });
        }
        Tab::Bookmarks if data.bookmarks.wanted() => {
            data.bookmarks = Loaded::Loading;
            commands.push(Command::Bookmarks { channel });
        }
        Tab::Files if data.files.wanted() => {
            data.files = Loaded::Loading;
            commands.push(Command::Files { channel });
        }
        _ => {}
    }
    for command in commands {
        send(app, team.clone(), command);
    }
}

impl State {
    /// What is loaded for a conversation, made empty when nothing is.
    pub fn data_mut(&mut self, team: &str, channel: &str) -> &mut ChannelData {
        self.data
            .entry((team.to_owned(), channel.to_owned()))
            .or_default()
    }

    /// What is loaded for a conversation, if anything.
    pub fn data(&self, team: &str, channel: &str) -> Option<&ChannelData> {
        self.data.get(&(team.to_owned(), channel.to_owned()))
    }
}

/// Asks for the people among `ids` the workspace does not know yet, so
/// lists of them show names rather than ids.
pub(crate) fn fetch_unknown(app: &App, team: &str, ids: &[String]) {
    if app.demo {
        return;
    }
    let Some(workspace) = app.workspaces.iter().find(|w| w.info.team_id == team) else {
        return;
    };
    let mut unknown: Vec<String> = ids
        .iter()
        .filter(|id| !workspace.users.contains_key(*id))
        .cloned()
        .collect();
    unknown.sort();
    unknown.dedup();
    if !unknown.is_empty() {
        app.backend.send(backend::Command::FetchUsers {
            team: team.to_owned(),
            ids: unknown,
        });
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
        Event::About { channel, result } => {
            app.convos.data_mut(team, &channel).about = result.into();
        }
        Event::Members { channel, result } => {
            if let Ok(members) = &result {
                fetch_unknown(app, team, members);
            }
            app.convos.data_mut(team, &channel).members = result.into();
        }
        Event::Files { channel, result } => {
            if let Ok(files) = &result {
                let people: Vec<String> = files.iter().filter_map(|f| f.user.clone()).collect();
                fetch_unknown(app, team, &people);
            }
            app.convos.data_mut(team, &channel).files = result.into();
        }
        Event::Pins { channel, result } => {
            if let Ok(pins) = &result {
                let people: Vec<String> = pins
                    .iter()
                    .flat_map(|p| p.message.user.iter().chain(p.by.iter()).cloned())
                    .collect();
                fetch_unknown(app, team, &people);
            }
            app.convos.data_mut(team, &channel).pins = result.into();
        }
        Event::Bookmarks { channel, result } => {
            app.convos.data_mut(team, &channel).bookmarks = result.into();
        }
        Event::Pinned {
            channel,
            ts,
            pinned: pin,
            by,
        } => pinned(app, team, &channel, &ts, pin, by),
        Event::BookmarkSaved {
            channel,
            local,
            bookmark,
        } => {
            if let Loaded::Ready(list) = &mut app.convos.data_mut(team, &channel).bookmarks {
                saved_bookmark(list, &local, bookmark);
            }
        }
        Event::BookmarkChanged { channel, bookmark } => {
            if let Loaded::Ready(list) = &mut app.convos.data_mut(team, &channel).bookmarks {
                upsert_bookmark(list, bookmark);
            }
        }
        Event::BookmarkRemoved { channel, id } => {
            if let Loaded::Ready(list) = &mut app.convos.data_mut(team, &channel).bookmarks {
                list.retain(|b| b.id != id);
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
                        &[("error", &error.message())],
                    )
                }
                Failure::Browse => {
                    if let Some(browse) = &mut app.convos.browse {
                        browse.error = Some(error.clone());
                        browse.done = true;
                    }
                    tf(
                        "Could not list the channels: {error}",
                        &[("error", &error.message())],
                    )
                }
                Failure::Join => {
                    if let Some(browse) = &mut app.convos.browse {
                        browse.joining.clear();
                    }
                    tf(
                        "Could not join the channel: {error}",
                        &[("error", &error.message())],
                    )
                }
                Failure::Leave => tf(
                    "Could not leave the channel: {error}",
                    &[("error", &error.message())],
                ),
                Failure::Create => {
                    if let Some(dialog) = &mut app.convos.new_channel {
                        dialog.busy = false;
                    }
                    tf(
                        "Could not create the channel: {error}",
                        &[("error", &error.message())],
                    )
                }
                Failure::Load { channel } => {
                    // The panel shows it where the list would be.
                    let data = app.convos.data_mut(team, &channel);
                    if data.about == Loaded::Loading {
                        data.about = Loaded::Failed(error.clone());
                    }
                    if data.members == Loaded::Loading {
                        data.members = Loaded::Failed(error.clone());
                    }
                    if data.files == Loaded::Loading {
                        data.files = Loaded::Failed(error.clone());
                    }
                    if data.pins == Loaded::Loading {
                        data.pins = Loaded::Failed(error.clone());
                    }
                    if data.bookmarks == Loaded::Loading {
                        data.bookmarks = Loaded::Failed(error.clone());
                    }
                    return;
                }
                Failure::Pin { channel, ts, pin } => {
                    pinned(app, team, &channel, &ts, !pin, None);
                    if pin {
                        tf(
                            "Could not pin the message: {error}",
                            &[("error", &error.message())],
                        )
                    } else {
                        tf(
                            "Could not unpin the message: {error}",
                            &[("error", &error.message())],
                        )
                    }
                }
                Failure::Bookmark { channel, change } => {
                    change_bookmarks(app, team, &channel, &change, true);
                    bookmark_failure(&change, &error)
                }
                Failure::Describe { channel, field } => {
                    // Put back what Slack really has.
                    app.backend.send(backend::Command::FetchConversation {
                        team: team.to_owned(),
                        channel,
                    });
                    match field {
                        Field::Topic => tf(
                            "Could not set the topic: {error}",
                            &[("error", &error.message())],
                        ),
                        Field::Purpose => tf(
                            "Could not set the description: {error}",
                            &[("error", &error.message())],
                        ),
                    }
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
            sign_in: Default::default(),
            scopes: None,
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
            sign_in: Default::default(),
            scopes: None,
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
            external: false,
            is_open: None,
            empty: false,
        });
        assert_eq!(
            existing_dm(&workspace, &["U1".into()]).as_deref(),
            Some("D1")
        );
        assert_eq!(existing_dm(&workspace, &["U2".into()]), None);
        assert_eq!(existing_dm(&workspace, &["U1".into(), "U2".into()]), None);
    }

    #[test]
    fn pinning_marks_every_loaded_copy() {
        let mut workspace = WorkspaceState::new(Workspace {
            team_id: "T1".into(),
            name: "Acme".into(),
            domain: "acme".into(),
            icon: None,
            user_id: "U0".into(),
            sign_in: Default::default(),
            scopes: None,
        });
        let message = Message {
            ts: Ts::new("1.0"),
            user: Some("U1".into()),
            username: None,
            bot_icon: None,
            bot_id: None,
            text: "hi".into(),
            thread_ts: Some(Ts::new("1.0")),
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
            delivery: crate::model::Delivery::Sent,
            broadcast: false,
            pinned: false,
            client_msg_id: None,
            subscribed: None,
        };
        // A thread's parent is in the channel and in its thread.
        workspace
            .timelines
            .entry("C1".into())
            .or_default()
            .upsert(message.clone());
        workspace
            .threads
            .entry(("C1".into(), Ts::new("1.0")))
            .or_default()
            .upsert(message);
        let shown = set_pinned(&mut workspace, "C1", &Ts::new("1.0"), true);
        assert!(shown.is_some_and(|m| m.pinned));
        assert!(workspace.timelines_for("C1").all(|t| t.messages[0].pinned));
        assert!(set_pinned(&mut workspace, "C1", &Ts::new("2.0"), true).is_none());
        set_pinned(&mut workspace, "C1", &Ts::new("1.0"), false);
        assert!(workspace.timelines_for("C1").all(|t| !t.messages[0].pinned));
    }

    #[test]
    fn clocks_differ_in_hours_and_minutes() {
        assert_eq!(zone_difference(3600, 3600), None);
        assert_eq!(
            zone_difference(5 * 3600 + 1800, 3600),
            Some(ZoneDifference {
                ahead: true,
                hours: 4,
                minutes: 30
            })
        );
        assert_eq!(
            zone_difference(-7 * 3600, 2 * 3600),
            Some(ZoneDifference {
                ahead: false,
                hours: 9,
                minutes: 0
            })
        );
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
            sign_in: Default::default(),
            scopes: None,
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
            external: false,
            is_open: None,
            empty: false,
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

    #[test]
    fn conversations_starting_with_the_query_come_first() {
        let candidate = |id: &str, title: &str, unread: bool, latest: i64| Candidate {
            id: id.into(),
            title: title.into(),
            kind: ConversationKind::Channel,
            unread,
            archived: false,
            latest,
        };
        let all = vec![
            candidate("C1", "team-design", true, 50),
            candidate("C2", "design", false, 10),
            candidate("C3", "random", true, 90),
            candidate("C4", "design-system", true, 5),
        ];
        let ids = |found: Vec<Candidate>| found.into_iter().map(|c| c.id).collect::<Vec<_>>();
        assert_eq!(
            ids(conversations_matching(all.clone(), " #Design")),
            ["C4", "C2", "C1"],
            "starting with it, then unread first"
        );
        assert_eq!(
            ids(conversations_matching(all, "")),
            ["C3", "C1", "C4", "C2"],
            "unread and busiest first without a query"
        );
    }

    fn mark(id: &str, title: &str) -> Bookmark {
        Bookmark {
            id: id.into(),
            title: title.into(),
            link: format!("https://example.com/{id}"),
            emoji: None,
        }
    }

    fn ids_of(list: &[Bookmark]) -> Vec<&str> {
        list.iter().map(|b| b.id.as_str()).collect()
    }

    #[test]
    fn bookmark_links_must_be_on_the_web() {
        assert_eq!(
            bookmark_link("  https://example.com/a?b=1 ").as_deref(),
            Ok("https://example.com/a?b=1")
        );
        assert_eq!(
            bookmark_link("HTTP://Example.com").as_deref(),
            Ok("HTTP://Example.com")
        );
        assert_eq!(
            bookmark_link("docs.example.com/release").as_deref(),
            Ok("https://docs.example.com/release"),
            "a bare domain gets https"
        );
        assert_eq!(bookmark_link("   "), Err(BookmarkProblem::NoLink));
        for bad in [
            "mailto:a@example.com",
            "javascript:alert(1)",
            "file:///etc/passwd",
            "slack://channel?id=C1",
            "ftp://example.com",
            "https://",
            "example",
            ".com",
            "https://exa mple.com",
            "localhost:8080",
        ] {
            assert_eq!(bookmark_link(bad), Err(BookmarkProblem::NotALink), "{bad}");
        }
    }

    #[test]
    fn bookmark_emoji_are_shortcodes_and_titles_default_to_the_link() {
        assert_eq!(bookmark_emoji(" :Rocket: "), Ok(Some("rocket".into())));
        assert_eq!(bookmark_emoji("+1"), Ok(Some("+1".into())));
        assert_eq!(bookmark_emoji("::"), Ok(None));
        assert_eq!(
            bookmark_emoji("two words"),
            Err(BookmarkProblem::NotAnEmoji)
        );
        assert_eq!(bookmark_emoji("🚀"), Err(BookmarkProblem::NotAnEmoji));
        assert_eq!(
            bookmark_form(" ", "example.com", ""),
            Ok(BookmarkForm {
                title: "https://example.com".into(),
                link: "https://example.com".into(),
                emoji: None,
            })
        );
        assert_eq!(
            bookmark_form("Docs", "nope", "rocket"),
            Err(BookmarkProblem::NotALink)
        );
        assert_eq!(
            bookmark_form("Docs", "example.com", "a b"),
            Err(BookmarkProblem::NotAnEmoji)
        );
    }

    #[test]
    fn bookmark_changes_show_at_once_and_undo_cleanly() {
        let start = vec![mark("Bk1", "One"), mark("Bk2", "Two"), mark("Bk3", "Three")];

        let add = BookmarkChange::Add(mark("local-1", "New"));
        let mut list = start.clone();
        add.apply(&mut list);
        assert_eq!(ids_of(&list), ["Bk1", "Bk2", "Bk3", "local-1"]);
        assert!(list[3].is_local());
        add.undo(&mut list);
        assert_eq!(list, start);

        let edit = BookmarkChange::Edit {
            before: mark("Bk2", "Two"),
            after: mark("Bk2", "Deux"),
        };
        let mut list = start.clone();
        edit.apply(&mut list);
        assert_eq!(list[1].title, "Deux");
        assert_eq!(ids_of(&list), ["Bk1", "Bk2", "Bk3"], "edited in place");
        edit.undo(&mut list);
        assert_eq!(list, start);

        let remove = BookmarkChange::Remove {
            bookmark: mark("Bk2", "Two"),
            at: 1,
        };
        let mut list = start.clone();
        remove.apply(&mut list);
        assert_eq!(ids_of(&list), ["Bk1", "Bk3"]);
        remove.undo(&mut list);
        assert_eq!(list, start, "put back where it was");
        // Put back once only, even if it came back some other way.
        remove.undo(&mut list);
        assert_eq!(list, start);
        // A list that shrank meanwhile takes it at its end.
        let mut short = vec![mark("Bk1", "One")];
        BookmarkChange::Remove {
            bookmark: mark("Bk9", "Nine"),
            at: 5,
        }
        .undo(&mut short);
        assert_eq!(ids_of(&short), ["Bk1", "Bk9"]);
    }

    #[test]
    fn a_saved_bookmark_replaces_its_local_copy_once() {
        let mut list = vec![mark("Bk1", "One"), mark("local-1", "New")];
        saved_bookmark(&mut list, "local-1", mark("Bk7", "New"));
        assert_eq!(ids_of(&list), ["Bk1", "Bk7"]);
        // Slack's event for it came before its answer.
        let mut list = vec![
            mark("local-1", "New"),
            mark("Bk1", "One"),
            mark("Bk7", "New"),
        ];
        saved_bookmark(&mut list, "local-1", mark("Bk7", "New"));
        assert_eq!(ids_of(&list), ["Bk7", "Bk1"]);
        // Its local copy is gone (the list was fetched again): just added.
        let mut list = vec![mark("Bk1", "One")];
        saved_bookmark(&mut list, "local-1", mark("Bk7", "New"));
        assert_eq!(ids_of(&list), ["Bk1", "Bk7"]);
    }

    #[test]
    fn saving_a_bookmark_adds_or_edits_only_what_changed() {
        let mut state = State::default();
        let form = |title: &str| BookmarkForm {
            title: title.into(),
            link: "https://example.com/Bk1".into(),
            emoji: None,
        };
        let first = state.bookmark_change(None, None, form("A"));
        let second = state.bookmark_change(None, None, form("B"));
        let local = |change: Option<BookmarkChange>| match change {
            Some(BookmarkChange::Add(b)) => b.id,
            other => panic!("not an addition: {other:?}"),
        };
        assert_ne!(local(first), local(second), "each addition its own id");
        let list = Loaded::Ready(vec![mark("Bk1", "One")]);
        assert_eq!(
            state.bookmark_change(Some(&list), Some("Bk1".into()), form("One")),
            None,
            "nothing changed"
        );
        assert_eq!(
            state.bookmark_change(Some(&list), Some("Bk1".into()), form("Uno")),
            Some(BookmarkChange::Edit {
                before: mark("Bk1", "One"),
                after: mark("Bk1", "Uno"),
            })
        );
        assert_eq!(
            state.bookmark_change(Some(&list), Some("Bk2".into()), form("X")),
            None
        );
        assert_eq!(
            removal(&list, "Bk1"),
            Some(BookmarkChange::Remove {
                bookmark: mark("Bk1", "One"),
                at: 0,
            })
        );
        assert_eq!(removal(&Loaded::Loading, "Bk1"), None);
    }

    #[test]
    fn a_missing_bookmark_permission_is_named() {
        let change = BookmarkChange::Add(mark("local-1", "New"));
        let text = bookmark_failure(&change, &Why::MissingPermission);
        assert!(text.contains("bookmarks:write"), "{text}");
        let text = bookmark_failure(&change, &Why::Archived);
        assert!(text.contains(&Why::Archived.message()), "{text}");
        assert!(!text.contains("bookmarks:write"), "{text}");
    }

    #[test]
    fn the_fingerprint_follows_what_pickers_list() {
        let mut workspace = WorkspaceState::new(Workspace {
            team_id: "T1".into(),
            name: "Acme".into(),
            domain: "acme".into(),
            icon: None,
            user_id: "U0".into(),
            sign_in: Default::default(),
            scopes: None,
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
            last_read: Some(Ts::new("1.0")),
            latest: Some(Ts::new("1.0")),
            unread: 0,
            mentions: 0,
            external: false,
            is_open: None,
            empty: false,
        });
        let first = candidates_fingerprint(&workspace);
        assert_eq!(candidates_fingerprint(&workspace), first);
        // A topic is not listed: no reason to search again.
        workspace.conversations[0].topic = "news".into();
        assert_eq!(candidates_fingerprint(&workspace), first);
        // A new message makes it unread and moves it up.
        workspace.conversations[0].latest = Some(Ts::new("2.0"));
        let unread = candidates_fingerprint(&workspace);
        assert_ne!(unread, first);
        workspace.conversations[0].archived = true;
        assert_ne!(candidates_fingerprint(&workspace), unread);
    }
}
