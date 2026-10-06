//! What the interface shows, independent of Slack's JSON.
//!
//! The worker turns API responses and Socket Mode events into these types;
//! views only ever read them, and ask for changes with [`Action`]s.

use std::cmp::Ordering;
use std::path::PathBuf;

/// A Slack message timestamp: `"1700000000.123456"`. Unique per conversation
/// and ordered by time. Optimistic messages carry `local-<n>` until Slack
/// answers, and sort after every real one.
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(transparent)]
pub struct Ts(pub String);

impl Ts {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn is_local(&self) -> bool {
        self.0.starts_with("local-")
    }

    /// What orders timestamps: real ones by time, then local ones by their
    /// counter (so `local-10` comes after `local-9`), then anything
    /// malformed.
    fn key(&self) -> Key {
        if let Some(counter) = self.0.strip_prefix("local-") {
            return match counter.parse() {
                Ok(counter) => Key::Local(counter),
                Err(_) => Key::Malformed,
            };
        }
        let (secs, fraction) = self.0.split_once('.').unwrap_or((&self.0, ""));
        let Ok(secs) = secs.parse() else {
            return Key::Malformed;
        };
        if !fraction.bytes().all(|b| b.is_ascii_digit()) {
            return Key::Malformed;
        }
        // Slack always writes six digits, but read "1.5" as half a second
        // rather than five microseconds.
        let micros = fraction
            .bytes()
            .chain(std::iter::repeat(b'0'))
            .take(6)
            .fold(0, |micros, digit| micros * 10 + u64::from(digit - b'0'));
        Key::Real(secs, micros)
    }

    /// Seconds since the epoch.
    pub fn seconds(&self) -> Option<i64> {
        match self.key() {
            Key::Real(secs, _) => Some(i64::try_from(secs).unwrap_or(i64::MAX)),
            _ => None,
        }
    }

    /// The timestamp one microsecond earlier, in Slack's own form
    /// (`1700000000.000099`): a read marker set there counts this message
    /// as unread and anything older as read. `None` for a local or
    /// malformed one, or the very first instant.
    pub fn just_before(&self) -> Option<Ts> {
        let Key::Real(secs, micros) = self.key() else {
            return None;
        };
        let (secs, micros) = match micros.checked_sub(1) {
            Some(micros) => (secs, micros),
            None => (secs.checked_sub(1)?, 999_999),
        };
        Some(Ts(format!("{secs}.{micros:06}")))
    }

    pub fn zoned(&self) -> Option<jiff::Zoned> {
        let ts = jiff::Timestamp::from_second(self.seconds()?).ok()?;
        Some(ts.to_zoned(jiff::tz::TimeZone::system()))
    }
}

/// A [`Ts`] taken apart for ordering; the variants sort in this order.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Key {
    /// Seconds and microseconds.
    Real(u64, u64),
    /// The counter of an optimistic message.
    Local(u64),
    Malformed,
}

impl PartialOrd for Ts {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Ts {
    fn cmp(&self, other: &Self) -> Ordering {
        self.key()
            .cmp(&other.key())
            .then_with(|| self.0.cmp(&other.0))
    }
}

/// A signed-in workspace.
#[derive(Clone, Debug, PartialEq)]
pub struct Workspace {
    pub team_id: String,
    pub name: String,
    pub domain: String,
    pub icon: Option<String>,
    /// You, in this workspace.
    pub user_id: String,
    /// How you signed in to it.
    pub sign_in: SignInKind,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub enum ConversationKind {
    Channel,
    Private,
    Direct,
    Group,
}

impl ConversationKind {
    pub fn is_dm(self) -> bool {
        matches!(self, Self::Direct | Self::Group)
    }
}

/// A channel, private channel, direct message or group direct message.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Conversation {
    pub id: String,
    /// The channel name, or for a DM the other person's user id until
    /// [`crate::app`] resolves it.
    pub name: String,
    pub kind: ConversationKind,
    /// The other person, for a direct message.
    pub user: Option<String>,
    pub topic: String,
    pub purpose: String,
    pub members: Option<u32>,
    pub archived: bool,
    /// The newest message you have read.
    #[serde(default)]
    pub last_read: Option<Ts>,
    /// The newest message known.
    #[serde(default)]
    pub latest: Option<Ts>,
    /// Unread messages Slack counted, when it says.
    #[serde(default)]
    pub unread: u32,
    /// Unread mentions of you, counted from messages seen live.
    #[serde(default)]
    pub mentions: u32,
    /// Shared with another organization through Slack Connect (or about
    /// to be).
    #[serde(default)]
    pub external: bool,
}

impl Conversation {
    pub fn has_unread(&self) -> bool {
        if self.unread > 0 {
            return true;
        }
        match (&self.latest, &self.last_read) {
            (Some(latest), Some(read)) => latest > read,
            (Some(_), None) => self.kind.is_dm(),
            _ => false,
        }
    }
}

/// Someone in a workspace.
#[derive(Clone, Debug, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct User {
    pub id: String,
    /// The handle (`jane.doe`).
    pub name: String,
    pub real_name: String,
    pub display_name: String,
    pub avatar: Option<String>,
    pub is_bot: bool,
    pub deleted: bool,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub status_text: String,
    #[serde(default)]
    pub status_emoji: String,
    #[serde(default)]
    pub tz: Option<String>,
    /// The workspace the person belongs to; another one's for someone
    /// reached through Slack Connect.
    #[serde(default)]
    pub team: String,
    /// The Enterprise Grid organization of that workspace, if any.
    #[serde(default)]
    pub enterprise: String,
    /// Slack says the person is outside your organization and has no
    /// profile here beyond the basics.
    #[serde(default)]
    pub stranger: bool,
}

impl User {
    /// The name Slack shows: display name, else real name, else handle.
    pub fn label(&self) -> &str {
        if !self.display_name.is_empty() {
            &self.display_name
        } else if !self.real_name.is_empty() {
            &self.real_name
        } else {
            &self.name
        }
    }
}

/// A user group (`@design`), which a message can mention to reach all of
/// its members at once.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct UserGroup {
    /// Slack's id (`S123`), which mentions carry.
    pub id: String,
    /// What you type after `@` to mention it (`design`).
    pub handle: String,
    /// Its full name (`Design team`).
    pub name: String,
    /// How many people are in it, when Slack says.
    pub members: Option<usize>,
}

/// What a sidebar section holds, as Slack types them.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum SectionKind {
    /// A section you made yourself.
    Custom,
    Starred,
    /// Every channel not placed in another section.
    Channels,
    /// Every DM not placed in another section.
    DirectMessages,
    /// DMs with apps and bots.
    Apps,
}

/// One section of your Slack sidebar, in your order.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct SidebarSection {
    pub id: String,
    pub kind: SectionKind,
    /// Your name for a custom section; empty for Slack's own.
    pub name: String,
    /// Its emoji shortcode, without colons, if any.
    pub emoji: String,
    /// The conversations placed in it explicitly. Slack's catch-all
    /// sections leave this empty.
    pub channel_ids: Vec<String>,
}

/// An app or integration that posts messages.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Bot {
    pub id: String,
    pub name: String,
    pub icon: Option<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Reaction {
    /// The shortcode without colons (`thumbsup`, `+1::skin-tone-2`).
    pub name: String,
    pub count: u32,
    pub users: Vec<String>,
}

/// A file shared in a message.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct File {
    pub id: String,
    pub name: String,
    pub title: String,
    pub mimetype: String,
    pub size: u64,
    /// The full file, which needs your token.
    pub url_private: Option<String>,
    pub download_url: Option<String>,
    /// The largest thumbnail Slack made, for images.
    pub thumb: Option<String>,
    pub thumb_size: Option<[f32; 2]>,
    pub permalink: Option<String>,
    /// The picture's own size in pixels, for the image viewer.
    pub original_size: Option<[f32; 2]>,
    /// A still Slack made of a file that is not a picture: a video's
    /// first frame, a PDF's first page.
    pub poster: Option<String>,
    pub poster_size: Option<[f32; 2]>,
}

/// A file to play rather than look at.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Media {
    Video,
    Audio,
}

impl File {
    pub fn is_image(&self) -> bool {
        self.mimetype.starts_with("image/") && self.thumb.is_some()
    }

    /// Whether this is a video or a sound, which open in the system's
    /// player: Slack's own player needs its web page.
    pub fn media(&self) -> Option<Media> {
        let mimetype = self.mimetype.to_ascii_lowercase();
        if mimetype.starts_with("video/") {
            Some(Media::Video)
        } else if mimetype.starts_with("audio/") {
            Some(Media::Audio)
        } else {
            None
        }
    }

    pub fn is_pdf(&self) -> bool {
        self.mimetype.eq_ignore_ascii_case("application/pdf")
    }
}

/// A labelled value in a legacy attachment (GlitchTip's "Project",
/// "Environment"). Short fields sit two to a row.
#[derive(Clone, Debug, PartialEq)]
pub struct Field {
    /// mrkdwn.
    pub title: String,
    /// mrkdwn.
    pub value: String,
    pub short: bool,
}

/// An attachment card: a link unfurl or a bot's legacy attachment.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Attachment {
    pub color: Option<egui::Color32>,
    /// The site the link is on ("YouTube"), shown above the title.
    pub service: Option<String>,
    /// The site's little icon, beside its name.
    pub service_icon: Option<String>,
    /// Who wrote what the link shows (a channel, an account).
    pub author: Option<String>,
    pub author_icon: Option<String>,
    pub author_link: Option<String>,
    /// The size Slack gives for `image` and `thumb`, so they take their
    /// place before they have loaded.
    pub image_size: Option<[f32; 2]>,
    pub thumb_size: Option<[f32; 2]>,
    /// For a video or other player (YouTube, Vimeo, a tweet's clip): the
    /// page to open to play it. The thumbnail is then shown large, with a
    /// play button over it.
    pub video: Option<String>,
    /// mrkdwn shown above the card, outside its colour bar.
    pub pretext: Option<String>,
    pub title: Option<String>,
    pub title_link: Option<String>,
    /// mrkdwn.
    pub text: String,
    pub fields: Vec<Field>,
    pub image: Option<String>,
    /// A small picture beside the text.
    pub thumb: Option<String>,
    pub footer: Option<String>,
    /// Block Kit layout some apps put inside an attachment.
    pub blocks: Vec<KitBlock>,
}

/// A Block Kit button: a link to follow, or an interactive button whose
/// press Slack hands to the app that posted it (see [`button_use`]).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Button {
    /// The label as Slack sends it.
    pub text: String,
    pub url: Option<String>,
    /// `primary` or `danger`, for colour.
    pub style: Option<String>,
    /// The app's own name for the button, which the press carries back.
    pub action_id: Option<String>,
    /// The block the button sits in, which the press names too.
    pub block_id: Option<String>,
    /// What the app put in the button for itself.
    pub value: Option<String>,
    /// The question to ask before pressing, when the app wants one.
    pub confirm: Option<Confirm>,
}

/// The dialog an app asks for before its button is pressed. A part the
/// app left out is `None`, and is worded here instead.
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash)]
pub struct Confirm {
    pub title: Option<String>,
    /// mrkdwn.
    pub text: Option<String>,
    /// The label of the button that goes ahead.
    pub confirm: Option<String>,
    /// The label of the button that backs out.
    pub deny: Option<String>,
    /// `danger` when going ahead is destructive.
    pub style: Option<String>,
}

/// How a workspace was signed in, which decides what Slack lets this
/// client do beyond the public Web API.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum SignInKind {
    /// A browser session (an `xoxc` token with the `d` cookie): the calls
    /// Slack's own web client makes work too.
    Session,
    /// The user token of your own Slack app, from OAuth: only the public
    /// Web API.
    #[default]
    App,
}

impl SignInKind {
    /// The kind of a sign-in that is, or is not, a browser session.
    pub fn of(session: bool) -> Self {
        if session { Self::Session } else { Self::App }
    }
}

/// Why an interactive button cannot be pressed here.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NotHere {
    /// Only Slack's own clients press an app's buttons. A browser session
    /// can stand in for one; an app's user token cannot, as the public API
    /// has no method for it.
    NeedsSession,
    /// The message does not say which app posted it, or the button lacks
    /// the ids a press needs.
    NoApp,
}

/// What pressing a Block Kit button does here.
#[derive(Clone, Debug, PartialEq)]
pub enum ButtonUse<'a> {
    /// Opens its link.
    Link(&'a str),
    /// Sends the press to the app, as Slack's web client does.
    Press(Press),
    /// Nothing: the button is shown, but only works in Slack itself.
    NotHere(NotHere),
}

/// Pressing an app's interactive button on message `ts` in `channel`.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Press {
    pub channel: String,
    pub ts: Ts,
    /// The bot that posted the message: Slack hands it the press.
    pub bot_id: String,
    pub block_id: String,
    pub action_id: String,
    /// The label as Slack sent it, which the press repeats.
    pub text: String,
    pub value: Option<String>,
}

/// What `button` on `message` (in `channel`) does in a workspace signed in
/// by `sign_in`. A link always opens. An interactive button can be pressed
/// only from a browser session, through the call Slack's web client makes
/// (`blocks.actions`); an OAuth sign-in has no such call, so there the
/// button is shown but not pressable, and the message opens in Slack.
pub fn button_use<'a>(
    sign_in: SignInKind,
    channel: &str,
    message: &Message,
    button: &'a Button,
) -> ButtonUse<'a> {
    if let Some(url) = &button.url {
        return ButtonUse::Link(url);
    }
    if sign_in != SignInKind::Session {
        return ButtonUse::NotHere(NotHere::NeedsSession);
    }
    let (Some(bot_id), Some(block_id), Some(action_id)) = (
        message.bot_id.as_ref(),
        button.block_id.as_ref(),
        button.action_id.as_ref(),
    ) else {
        return ButtonUse::NotHere(NotHere::NoApp);
    };
    if message.ts.is_local() {
        return ButtonUse::NotHere(NotHere::NoApp);
    }
    ButtonUse::Press(Press {
        channel: channel.to_owned(),
        ts: message.ts.clone(),
        bot_id: bot_id.clone(),
        block_id: block_id.clone(),
        action_id: action_id.clone(),
        text: button.text.clone(),
        value: button.value.clone(),
    })
}

/// What a Block Kit section shows on its right.
#[derive(Clone, Debug, PartialEq)]
pub enum Accessory {
    Image {
        url: String,
        alt: String,
    },
    /// Boxed: a button is far larger than a picture.
    Button(Box<Button>),
}

/// One piece of a Block Kit context line.
#[derive(Clone, Debug, PartialEq)]
pub enum ContextItem {
    /// mrkdwn.
    Text(String),
    Image {
        url: String,
        alt: String,
    },
}

/// A Block Kit block, as apps lay out their messages. Every text here is
/// mrkdwn (plain text is escaped into it).
#[derive(Clone, Debug, PartialEq)]
pub enum KitBlock {
    Header(String),
    Section {
        text: Option<String>,
        fields: Vec<String>,
        accessory: Option<Accessory>,
    },
    Context(Vec<ContextItem>),
    Divider,
    Image {
        url: String,
        alt: String,
        title: Option<String>,
    },
    Actions(Vec<Button>),
    /// What people type, as Slack laid it out: the message's `text` says
    /// the same in mrkdwn, but these say for certain what is an emoji, a
    /// mention or a style.
    RichText(std::sync::Arc<[crate::mrkdwn::Block]>),
}

impl KitBlock {
    /// Whether the block says something the message's `text` may not.
    pub fn is_layout(&self) -> bool {
        !matches!(self, Self::RichText(_))
    }
}

/// Where a message stands on the way to Slack.
#[derive(Clone, Debug, PartialEq)]
pub enum Delivery {
    Sent,
    Sending,
    Failed(crate::failure::Failure),
}

#[derive(Clone, Debug, PartialEq)]
pub struct Message {
    pub ts: Ts,
    /// Who wrote it, for people.
    pub user: Option<String>,
    /// The name a bot or integration posted under.
    pub username: Option<String>,
    pub bot_icon: Option<String>,
    /// The app or integration that posted it. Webhook messages often carry
    /// only this, and the name comes from `bots.info`.
    pub bot_id: Option<String>,
    /// Slack mrkdwn as Slack sends it, still escaped: [`crate::mrkdwn`] parses it.
    pub text: String,
    /// Set on replies and on a thread's parent.
    pub thread_ts: Option<Ts>,
    pub reply_count: u32,
    /// Whether Slack said how many replies there are (even none). A copy
    /// that does not, such as some edits, keeps the counters already known.
    pub replies_known: bool,
    pub reply_users: Vec<String>,
    pub latest_reply: Option<Ts>,
    pub reactions: Vec<Reaction>,
    pub files: Vec<File>,
    pub attachments: Vec<Attachment>,
    /// Block Kit layout. When it holds more than rich text, it is drawn
    /// instead of `text`, which apps send only as the notification fallback.
    pub blocks: Vec<KitBlock>,
    pub edited: bool,
    /// Slack's subtype (`channel_join`, `bot_message`, ...), if any.
    pub subtype: Option<String>,
    pub delivery: Delivery,
    /// A reply also sent to the channel.
    pub broadcast: bool,
    /// Pinned to its conversation.
    pub pinned: bool,
    /// The id the sending client gave the message (see
    /// [`new_client_msg_id`]). Slack keeps it, so a message you send here
    /// comes back from Slack carrying the id of its optimistic copy.
    pub client_msg_id: Option<String>,
}

/// A fresh id for a message about to be sent, in the form Slack's own
/// clients use: a random (version 4) UUID, in lower case.
pub fn new_client_msg_id() -> String {
    use rand::Rng as _;
    let mut bytes = [0u8; 16];
    rand::rng().fill_bytes(&mut bytes);
    // The version and variant bits that make it a version 4 UUID.
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    let hex: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    format!(
        "{}-{}-{}-{}-{}",
        &hex[..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..]
    )
}

impl Message {
    /// Whether the Block Kit layout replaces `text` on screen.
    pub fn uses_blocks(&self) -> bool {
        self.blocks.iter().any(KitBlock::is_layout)
    }

    /// Slack's own layout of `text`, when the message carries one: drawn
    /// instead of parsing `text`, which can only guess at it.
    pub fn rich_text(&self) -> Option<&std::sync::Arc<[crate::mrkdwn::Block]>> {
        self.blocks.iter().find_map(|block| match block {
            KitBlock::RichText(blocks) => Some(blocks),
            _ => None,
        })
    }

    /// Whether this is a reply inside a thread (not the parent).
    pub fn is_reply(&self) -> bool {
        self.thread_ts
            .as_ref()
            .is_some_and(|parent| *parent != self.ts)
    }

    /// Whether it belongs in the channel's own list: parents, ordinary
    /// messages and replies also sent to the channel.
    pub fn in_channel(&self) -> bool {
        !self.is_reply() || self.broadcast
    }

    /// A join, leave, topic change or the like, drawn as one quiet line.
    pub fn is_system(&self) -> bool {
        matches!(
            self.subtype.as_deref(),
            Some(
                "channel_join"
                    | "channel_leave"
                    | "channel_topic"
                    | "channel_purpose"
                    | "channel_name"
                    | "channel_archive"
                    | "channel_unarchive"
                    | "group_join"
                    | "group_leave"
                    | "group_topic"
                    | "group_purpose"
                    | "group_name"
                    | "pinned_item"
                    | "unpinned_item"
            )
        )
    }

    /// Adds or removes one person's reaction.
    pub fn toggle_reaction(&mut self, name: &str, user: &str, added: bool) {
        match self.reactions.iter_mut().position(|r| r.name == name) {
            Some(index) => {
                let reaction = &mut self.reactions[index];
                let has = reaction.users.iter().any(|u| u == user);
                if added && !has {
                    reaction.users.push(user.to_owned());
                    reaction.count += 1;
                } else if !added && has {
                    reaction.users.retain(|u| u != user);
                    reaction.count = reaction.count.saturating_sub(1);
                }
                if reaction.count == 0 {
                    self.reactions.remove(index);
                }
            }
            None if added => self.reactions.push(Reaction {
                name: name.to_owned(),
                count: 1,
                users: vec![user.to_owned()],
            }),
            None => {}
        }
    }
}

/// How many messages [`Timeline::held`] keeps: about a page.
const HELD_LIMIT: usize = 100;

/// Messages of one conversation or thread, oldest first.
#[derive(Clone, Debug, Default)]
pub struct Timeline {
    pub messages: Vec<Message>,
    /// Whether older messages exist on the server.
    pub has_more: bool,
    /// The cursor for the next older page.
    pub cursor: Option<String>,
    pub loading: bool,
    /// Whether the first page has arrived.
    pub loaded: bool,
    /// Whether newer messages exist on the server than the newest one
    /// here: the list was opened around an older message, and does not
    /// reach the present until newer pages are read.
    pub has_newer: bool,
    /// The message whose surroundings are on their way, after a jump to
    /// it: they replace the list when they arrive.
    pub around: Option<Ts>,
    /// Whether the list is the offline cache's copy of the newest page,
    /// which the first page from Slack replaces.
    pub cached: bool,
    /// The newest message of the offline cache's copy, while it shows:
    /// anything newer came live, and stays when Slack's page replaces it.
    pub cached_newest: Option<Ts>,
    /// New messages that came live while the list does not reach the
    /// present (see [`Self::has_newer`]), oldest first. They join it once
    /// it does, in case the page that gets there was asked for before
    /// they were sent.
    pub held: Vec<Message>,
}

impl Timeline {
    /// Inserts or replaces a message, keeping the order.
    pub fn upsert(&mut self, message: Message) {
        if let Some(existing) = self.messages.iter_mut().find(|m| m.ts == message.ts) {
            // A copy that gives no reply count may be trimmed, as some
            // edits and API answers are: keep the thread counters already
            // known. One that gives a count, even zero, carries Slack's
            // real counters, and zero then means the replies are gone.
            let mut message = message;
            if !message.replies_known {
                message.reply_count = existing.reply_count;
                message.replies_known = existing.replies_known;
                message.reply_users = std::mem::take(&mut existing.reply_users);
                message.latest_reply = existing.latest_reply.take();
                if message.thread_ts.is_none() {
                    message.thread_ts = existing.thread_ts.take();
                }
            }
            *existing = message;
            return;
        }
        if message.ts.is_local() {
            self.messages.push(message);
            return;
        }
        let real = self.first_local();
        let at = self.messages[..real].partition_point(|m| m.ts < message.ts);
        self.messages.insert(at, message);
    }

    fn first_local(&self) -> usize {
        self.messages
            .iter()
            .position(|m| m.ts.is_local())
            .unwrap_or(self.messages.len())
    }

    /// Merges a page of history: newer pages replace what they cover, older
    /// ones go in front.
    pub fn merge(&mut self, page: Vec<Message>) {
        for message in page {
            self.upsert(message);
        }
    }

    pub fn find_mut(&mut self, ts: &Ts) -> Option<&mut Message> {
        self.messages.iter_mut().find(|m| &m.ts == ts)
    }

    pub fn remove(&mut self, ts: &Ts) {
        self.messages.retain(|m| &m.ts != ts);
        self.held.retain(|m| &m.ts != ts);
    }

    /// Keeps a new message for when the list reaches the present (see
    /// [`Self::held`]), a page's worth at most: a list that far behind
    /// reads the rest from Slack.
    pub fn hold(&mut self, message: Message) {
        match self.held.iter_mut().find(|m| m.ts == message.ts) {
            Some(existing) => *existing = message,
            None => self.held.push(message),
        }
        self.held.sort_by(|a, b| a.ts.cmp(&b.ts));
        let over = self.held.len().saturating_sub(HELD_LIMIT);
        self.held.drain(..over);
    }

    /// Puts the messages held while the list did not reach the present into
    /// it, without replacing the copies it has.
    pub fn release_held(&mut self) {
        for message in std::mem::take(&mut self.held) {
            if self.find_mut(&message.ts).is_none() {
                self.upsert(message);
            }
        }
    }

    pub fn newest(&self) -> Option<&Ts> {
        self.messages
            .iter()
            .rev()
            .map(|m| &m.ts)
            .find(|ts| !ts.is_local())
    }
}

/// A request from a view, applied by [`crate::app::App`] after the frame.
#[derive(Clone, Debug)]
pub enum Action {
    SelectWorkspace(String),
    OpenConversation(String),
    /// Opens a conversation of the active workspace in a window of its own.
    PopOut(String),
    /// Closes a direct message or group DM of the active workspace: it
    /// leaves the sidebar until something new arrives in it.
    CloseConversation(String),
    OpenThread {
        channel: String,
        ts: Ts,
    },
    CloseThread,
    LoadOlder,
    /// Brings the open conversation's "New" line into view.
    JumpToUnread,
    /// Brings the open conversation's newest messages into view.
    JumpToNewest,
    /// Reads the page after the newest message loaded, in a list opened
    /// around an older message.
    LoadNewer,
    /// Shows a message of the open workspace in its conversation, with the
    /// messages around it, and highlights it. `thread` is its thread's
    /// parent when it is a reply, which then opens beside it.
    JumpTo {
        channel: String,
        ts: Ts,
        thread: Option<Ts>,
    },
    Send {
        text: String,
        thread: Option<Ts>,
        broadcast: bool,
    },
    Retry {
        channel: String,
        local: Ts,
    },
    Edit {
        channel: String,
        ts: Ts,
        text: String,
    },
    Delete {
        channel: String,
        ts: Ts,
    },
    React {
        channel: String,
        ts: Ts,
        name: String,
    },
    /// Opens the emoji picker to react to a message.
    PickReaction {
        channel: String,
        ts: Ts,
    },
    /// Opens the emoji picker to insert into a draft.
    PickEmoji {
        draft: String,
    },
    StartEdit {
        channel: String,
        ts: Ts,
    },
    /// Like `StartEdit`, in the thread panel: a thread's parent shows in
    /// both panels, and only one of them gets the edit field.
    StartEditInThread {
        channel: String,
        ts: Ts,
    },
    CancelEdit,
    /// Edits your newest message in the open conversation.
    EditLast,
    /// Moves the conversation's read marker back to just before message
    /// `ts`, so it and everything after it is unread again, and keeps it
    /// there until you leave the conversation and come back.
    MarkUnread {
        channel: String,
        ts: Ts,
    },
    /// Asks before deleting a message.
    AskDelete {
        channel: String,
        ts: Ts,
    },
    /// Shows an image large.
    Preview {
        uri: String,
        name: String,
    },
    /// Opens the image viewer on file `file` of message `ts`, stepping
    /// through the images of the list it is in: the thread with parent
    /// `thread`, or else the conversation.
    ViewImage {
        channel: String,
        thread: Option<Ts>,
        ts: Ts,
        file: String,
    },
    OpenSwitcher,
    /// Opens the search window.
    OpenSearch,
    /// Searches for what is typed in the search window.
    RunSearch,
    /// Reads the next page of the search results.
    SearchMore,
    /// Changes the sidebar here and in Slack.
    Sidebar(crate::sidebar::SidebarEdit),
    /// Asks for a section name: a new section (taking `channel` along), or
    /// a new name for `rename`.
    NameSection {
        rename: Option<String>,
        channel: Option<String>,
    },
    Upload {
        thread: Option<Ts>,
        path: PathBuf,
        comment: String,
    },
    PickUpload {
        thread: Option<Ts>,
    },
    /// Uploads the image on the clipboard, if that is what it holds.
    PasteImage {
        thread: Option<Ts>,
    },
    /// A file was taken out of a composer before sending.
    Unstage(std::path::PathBuf),
    /// Puts a picture on the clipboard, from the first of these image
    /// loader URIs that loads (the full picture, then its thumbnail).
    CopyImage(Vec<String>),
    /// Stops an upload by its [`crate::app::Upload::id`].
    CancelUpload(u64),
    Download {
        url: String,
        name: String,
    },
    /// Opens a file (a video, a sound) in the system's app for it.
    OpenFile {
        url: String,
        name: String,
    },
    OpenUrl(String),
    /// Presses an app's interactive button. When the app asked for a
    /// `confirm` dialog, this asks first, unless `confirmed`.
    PressButton {
        press: Press,
        confirm: Option<Confirm>,
        confirmed: bool,
    },
    /// Opens a message in Slack itself (the browser or Slack's app), for
    /// what only works there; `thread` is its parent for a reply.
    OpenInSlack {
        channel: String,
        ts: Ts,
        thread: Option<Ts>,
    },
    /// Copies a message's permalink; `thread` is its parent for a reply.
    CopyLink {
        channel: String,
        ts: Ts,
        thread: Option<Ts>,
    },
    /// Opens the "Share message" dialog for a message; `thread` is its
    /// parent for a reply.
    Share {
        channel: String,
        ts: Ts,
        thread: Option<Ts>,
    },
    /// Posts a link to a message, after an optional `comment`, in
    /// conversation `to`, staying where you are.
    ShareTo {
        channel: String,
        ts: Ts,
        thread: Option<Ts>,
        to: String,
        comment: String,
    },
    OpenProfile(String),
    Copy(String),
    ShowSettings,
    HideSettings,
    /// Opens the sheet that lists the keyboard shortcuts.
    ShowShortcuts,
    AddWorkspace,
    SignOut(String),
    Reconnect,
    /// Uses the proxy now in the settings and restarts the connections.
    ApplyProxy,
    /// Loads the spelling dictionary now in the settings, or stops
    /// checking.
    ApplySpelling,
    DismissError,
    // Sign-in. These carry no secrets: the app reads the typed cookie,
    // token and credentials from its form, so they never sit in an action
    // that might be printed.
    /// Signs in with the session cookie and workspace from the form.
    SignInSession,
    /// Signs in with the pasted `slack://` link from the browser sign-in.
    SignInLink,
    /// Opens Slack's sign-in page in the browser.
    StartBrowserSignIn,
    /// Signs in with the user token from the form.
    PasteToken,
    /// Saves the Slack app's credentials from the form.
    SaveApp,
    /// Starts OAuth in the browser with the saved app.
    StartSignIn,
    CancelSignIn,
    /// Opens a folder in the system's file manager.
    OpenFolder(PathBuf),
    /// Sets how much of a conversation in the open workspace notifies;
    /// `None` goes back to Slack's choice or the default.
    NotifyLevel {
        channel: String,
        level: Option<crate::notify::Level>,
    },
    /// Snoozes notifications in the open workspace, or with `None` ends
    /// the snooze.
    Snooze(Option<crate::dnd::Snooze>),
    /// Mutes or unmutes a conversation in the open workspace.
    Mute {
        channel: String,
        muted: bool,
    },
    /// Starts, finds or looks after a conversation (see [`crate::convos`]).
    Convos(crate::convos::Action),
    /// Something about people: your typing, your status (see
    /// [`crate::people`]).
    People(crate::people::Action),
    /// Opens or works a view at the top of the sidebar (see
    /// [`crate::views`]).
    Views(crate::views::Action),
}

#[cfg(test)]
mod tests {
    use super::*;

    fn message(ts: &str) -> Message {
        Message {
            ts: Ts::new(ts),
            user: None,
            username: None,
            bot_icon: None,
            bot_id: None,
            text: ts.to_owned(),
            thread_ts: None,
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
        }
    }

    #[test]
    fn buttons_press_only_from_a_browser_session() {
        let posted = Message {
            bot_id: Some("B09".into()),
            ..message("1790171950.000100")
        };
        let approve = Button {
            text: "Approve".into(),
            action_id: Some("approve".into()),
            block_id: Some("deploy".into()),
            value: Some("1288".into()),
            ..Button::default()
        };
        assert_eq!(
            button_use(SignInKind::Session, "C05", &posted, &approve),
            ButtonUse::Press(Press {
                channel: "C05".into(),
                ts: Ts::new("1790171950.000100"),
                bot_id: "B09".into(),
                block_id: "deploy".into(),
                action_id: "approve".into(),
                text: "Approve".into(),
                value: Some("1288".into()),
            })
        );
        assert_eq!(
            button_use(SignInKind::App, "C05", &posted, &approve),
            ButtonUse::NotHere(NotHere::NeedsSession),
            "an OAuth token has no call to press with"
        );
        let link = Button {
            url: Some("https://example.com".into()),
            ..approve.clone()
        };
        assert_eq!(
            button_use(SignInKind::App, "C05", &posted, &link),
            ButtonUse::Link("https://example.com"),
            "links open whatever the sign-in"
        );
        let unknown_app = Message {
            bot_id: None,
            ..posted.clone()
        };
        assert_eq!(
            button_use(SignInKind::Session, "C05", &unknown_app, &approve),
            ButtonUse::NotHere(NotHere::NoApp)
        );
        let no_id = Button {
            action_id: None,
            ..approve.clone()
        };
        assert_eq!(
            button_use(SignInKind::Session, "C05", &posted, &no_id),
            ButtonUse::NotHere(NotHere::NoApp)
        );
        assert_eq!(SignInKind::of(true), SignInKind::Session);
        assert_eq!(SignInKind::of(false), SignInKind::App);
    }

    #[test]
    fn timestamps_order_numerically() {
        assert!(Ts::new("1700000000.000200") > Ts::new("1700000000.000100"));
        assert!(Ts::new("1700000001.000000") > Ts::new("999999999.999999"));
        assert!(Ts::new("local-1") > Ts::new("1700000000.000100"));
    }

    #[test]
    fn local_and_malformed_timestamps_sort_last() {
        assert!(Ts::new("local-10") > Ts::new("local-9"));
        assert!(Ts::new("local-9") > Ts::new("9999999999.999999"));
        assert!(Ts::new("garbage") > Ts::new("local-10"));
        assert!(Ts::new("1.x") > Ts::new("local-1"));
        assert!(Ts::new("local-x") > Ts::new("local-1"));
        assert!(
            Ts::new("1.5") > Ts::new("1.000009"),
            "a short fraction is tenths"
        );
        assert!(Ts::new("2") > Ts::new("1.999999"));
        assert_eq!(Ts::new("1700000000.000100").seconds(), Some(1_700_000_000));
        assert_eq!(Ts::new("local-3").seconds(), None);
        assert_eq!(Ts::new("").seconds(), None);
        assert_eq!(Ts::new("-1.0").seconds(), None);
        let mut sorted = [
            Ts::new("local-10"),
            Ts::new("bad"),
            Ts::new("2.0"),
            Ts::new("local-9"),
            Ts::new("1.0"),
        ];
        sorted.sort();
        let order: Vec<&str> = sorted.iter().map(Ts::as_str).collect();
        assert_eq!(order, ["1.0", "2.0", "local-9", "local-10", "bad"]);
    }

    #[test]
    fn upserts_keep_order_and_local_messages_last() {
        let mut timeline = Timeline::default();
        timeline.upsert(message("2.0"));
        timeline.upsert(message("local-1"));
        timeline.upsert(message("1.0"));
        timeline.upsert(message("3.0"));
        let order: Vec<_> = timeline.messages.iter().map(|m| m.ts.as_str()).collect();
        assert_eq!(order, ["1.0", "2.0", "3.0", "local-1"]);
        assert_eq!(timeline.newest(), Some(&Ts::new("3.0")));
    }

    #[test]
    fn edits_keep_thread_counters() {
        let mut timeline = Timeline::default();
        let mut parent = message("1.0");
        parent.reply_count = 3;
        timeline.upsert(parent);
        let mut edited = message("1.0");
        edited.text = "edited".into();
        timeline.upsert(edited);
        assert_eq!(timeline.messages[0].reply_count, 3);
        assert_eq!(timeline.messages[0].text, "edited");
    }

    #[test]
    fn a_parent_reporting_no_replies_clears_its_counters() {
        let mut timeline = Timeline::default();
        let mut parent = message("1.0");
        parent.thread_ts = Some(Ts::new("1.0"));
        parent.reply_count = 1;
        parent.reply_users = vec!["U1".into()];
        parent.latest_reply = Some(Ts::new("2.0"));
        timeline.upsert(parent);
        // Slack's copy after the last reply was deleted: still a thread
        // parent, with nothing in it.
        let mut emptied = message("1.0");
        emptied.thread_ts = Some(Ts::new("1.0"));
        emptied.replies_known = true;
        timeline.upsert(emptied);
        let parent = &timeline.messages[0];
        assert_eq!(parent.reply_count, 0);
        assert!(parent.reply_users.is_empty());
        assert_eq!(parent.latest_reply, None);
    }

    #[test]
    fn reactions_toggle_per_person() {
        let mut m = message("1.0");
        m.toggle_reaction("tada", "U1", true);
        m.toggle_reaction("tada", "U2", true);
        m.toggle_reaction("tada", "U1", true);
        assert_eq!(m.reactions[0].count, 2);
        m.toggle_reaction("tada", "U1", false);
        m.toggle_reaction("tada", "U2", false);
        assert!(m.reactions.is_empty());
    }

    #[test]
    fn just_before_is_one_microsecond_earlier() {
        let before = |ts: &str| Ts::new(ts).just_before().map(|t| t.0);
        assert_eq!(
            before("1700000000.000100").as_deref(),
            Some("1700000000.000099")
        );
        // Borrows from the seconds, and reads "5.0" as Slack's "5.000000".
        assert_eq!(before("5.0").as_deref(), Some("4.999999"));
        assert!(Ts::new("4.999999") < Ts::new("5.0"));
        assert_eq!(before("0.000000"), None);
        assert_eq!(before("local-3"), None);
    }

    #[test]
    fn unread_compares_latest_with_last_read() {
        let mut c = Conversation {
            id: "C1".into(),
            name: "general".into(),
            kind: ConversationKind::Channel,
            user: None,
            topic: String::new(),
            purpose: String::new(),
            members: None,
            archived: false,
            last_read: Some(Ts::new("5.0")),
            latest: Some(Ts::new("4.0")),
            unread: 0,
            mentions: 0,
            external: false,
        };
        assert!(!c.has_unread());
        c.latest = Some(Ts::new("6.0"));
        assert!(c.has_unread());
    }
}
