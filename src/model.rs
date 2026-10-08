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

    /// `text` as a timestamp if it has a real one's shape, digits, a dot,
    /// digits, as in a link or a query someone could have mangled.
    pub fn parse(text: &str) -> Option<Ts> {
        let (secs, micros) = text.split_once('.')?;
        let digits = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
        (digits(secs) && digits(micros)).then(|| Ts::new(text))
    }

    /// Seconds since the epoch and the microseconds past them, for a real
    /// timestamp; `None` for a local or malformed one.
    pub fn parts(&self) -> Option<(u64, u64)> {
        match self.key() {
            Key::Real(secs, micros) => Some((secs, micros)),
            _ => None,
        }
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
        // Two of Slack's own, written alike (as nearly all are), order as
        // their text does, without taking either apart.
        if self.0.len() == other.0.len()
            && let Some(dot) = plain_dot(&self.0)
            && plain_dot(&other.0) == Some(dot)
        {
            return self.0.cmp(&other.0);
        }
        self.key()
            .cmp(&other.key())
            .then_with(|| self.0.cmp(&other.0))
    }
}

/// Where the dot of a plainly written real timestamp is (its length when
/// it has none): one to 19 digits, which always fit [`Ts::key`]'s seconds,
/// then perhaps a dot and more digits. Two of the same length with the dot
/// in the same place order by their text exactly as by [`Ts::key`] and
/// then their text: digit by digit is number by number at equal widths,
/// and the microseconds the key reads are the fraction's first digits.
fn plain_dot(ts: &str) -> Option<usize> {
    let bytes = ts.as_bytes();
    let dot = bytes
        .iter()
        .position(|&b| !b.is_ascii_digit())
        .unwrap_or(bytes.len());
    let fraction = match bytes.get(dot) {
        None => &[][..],
        Some(b'.') => &bytes[dot + 1..],
        Some(_) => return None,
    };
    ((1..=19).contains(&dot) && fraction.iter().all(u8::is_ascii_digit)).then_some(dot)
}

/// The chat service backing a workspace.
#[derive(
    Clone, Copy, Debug, Default, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize,
)]
pub enum Service {
    /// Slack, via Web API and RTM / Socket Mode.
    #[default]
    Slack,
    /// Microsoft Teams, via native Skype Spaces / Trouter APIs.
    Teams,
}

impl Service {
    /// The human-readable name of the service.
    pub fn name(self) -> &'static str {
        match self {
            Self::Slack => "Slack",
            Self::Teams => "Microsoft Teams",
        }
    }

    /// Whether the New message dialog asks the server for people as you
    /// type: a Teams workspace knows only the people met in its chats,
    /// where a Slack one has everyone already.
    pub fn searches_people(self) -> bool {
        self == Self::Teams
    }

    /// Whether workspaces of this service can do `ability` here. Slack
    /// does everything but calls, which are its huddles; Teams reads, sends, edits, deletes, reacts and
    /// marks read so far, so the interface leaves the rest out rather than
    /// offer what would only fail.
    pub fn offers(self, ability: Ability) -> bool {
        match self {
            Self::Slack => !matches!(ability, Ability::Calls | Ability::Meetings),
            Self::Teams => match ability {
                // Seen in recordings of the Teams web client and built.
                Ability::Reactions
                | Ability::Edit
                | Ability::NewMessage
                | Ability::Calls
                | Ability::Meetings => true,
                Ability::Huddles
                | Ability::CustomEmoji
                | Ability::Threads
                | Ability::Files
                | Ability::Pins
                | Ability::Later
                | Ability::Bookmarks
                | Ability::Channels
                | Ability::Describe
                | Ability::Sections
                | Ability::SlashCommands
                | Ability::Snooze
                | Ability::Status
                | Ability::Search
                | Ability::MarkUnread
                | Ability::Reminders
                | Ability::Scheduled
                | Ability::Views
                | Ability::Cards
                | Ability::Details
                | Ability::Links
                | Ability::Share => false,
            },
        }
    }
}

/// Something a workspace may or may not be able to do, by its service
/// (see [`Service::offers`]). Not to be confused with
/// [`crate::scopes::Feature`], which is what a Slack sign-in was granted.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Ability {
    /// Starting, joining and listening to huddles.
    Huddles,
    /// Calling someone from a one-to-one chat (Teams; Slack has huddles).
    Calls,
    /// Joining a meeting by its link or ID, and starting one now (Teams).
    Meetings,
    /// Adding custom emoji.
    CustomEmoji,
    /// Adding and removing reactions.
    Reactions,
    /// Editing your messages.
    Edit,
    /// Replying in threads.
    Threads,
    /// Uploading and deleting files.
    Files,
    /// Pinning messages and the pinned list.
    Pins,
    /// Saving messages for later.
    Later,
    /// A conversation's bookmarks.
    Bookmarks,
    /// Browsing, creating, joining and leaving channels.
    Channels,
    /// Starting a conversation with people (the New message dialog).
    NewMessage,
    /// Renaming a conversation and setting its topic.
    Describe,
    /// Editing sidebar sections.
    Sections,
    /// Slash commands.
    SlashCommands,
    /// Snoozing notifications (Do Not Disturb).
    Snooze,
    /// Setting your status and being away.
    Status,
    /// Searching messages.
    Search,
    /// Marking a message unread.
    MarkUnread,
    /// Reminders about messages.
    Reminders,
    /// Scheduling messages to send later.
    Scheduled,
    /// The Activity, Unreads, Threads, Later and Scheduled views.
    Views,
    /// Pressing buttons in Block Kit cards and opening them in Slack.
    Cards,
    /// A conversation's details panel: about, members and files.
    Details,
    /// Copying a link to a message.
    Links,
    /// Sharing a message to another conversation.
    Share,
}

/// A signed-in workspace.
#[derive(Clone, Debug, PartialEq)]
pub struct Workspace {
    /// Which service backs this workspace.
    pub service: Service,
    pub team_id: String,
    pub name: String,
    pub domain: String,
    pub icon: Option<String>,
    /// You, in this workspace.
    pub user_id: String,
    /// How you signed in to it.
    pub sign_in: SignInKind,
    /// The user scopes Slack granted an app sign-in; `None` for a session,
    /// or for an app sign-in from before they were recorded.
    pub scopes: Option<crate::scopes::Scopes>,
}

impl Workspace {
    /// Whether this workspace is backed by Microsoft Teams.
    pub fn is_teams(&self) -> bool {
        self.service == Service::Teams
    }

    /// Whether this workspace is backed by Slack.
    pub fn is_slack(&self) -> bool {
        self.service == Service::Slack
    }

    /// Whether this workspace's service can do `ability` here.
    pub fn offers(&self, ability: Ability) -> bool {
        self.service.offers(ability)
    }

    /// Whether this sign-in may use `feature`: always for a session, and
    /// for an app sign-in when Slack granted its scope (or when what it
    /// granted is not known yet, so the call is tried).
    pub fn can(&self, feature: crate::scopes::Feature) -> bool {
        if self.service == Service::Teams {
            return false;
        }
        crate::scopes::allows(
            self.scopes.as_ref(),
            self.sign_in == SignInKind::Session,
            feature.scope(),
        )
    }

    /// The features an app sign-in lacks the scopes for, which an app
    /// made from an older manifest does; empty when all are there or it
    /// is not known.
    pub fn lacking(&self) -> Vec<crate::scopes::Feature> {
        match (&self.scopes, self.sign_in) {
            (Some(scopes), SignInKind::App) => scopes.lacking(),
            _ => Vec::new(),
        }
    }
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
    /// Whether Slack has this direct message or group DM open in your
    /// sidebar; `None` when Slack did not say. A closed one stays out of
    /// the sidebar until something brings it back (see
    /// [`crate::sidebar::is_shut`]).
    #[serde(default)]
    pub is_open: Option<bool>,
    /// Slack said the conversation has no messages at all, which is not
    /// the same as not knowing its newest message (`latest` is `None` for
    /// both). Only meaningful while `latest` is `None`.
    #[serde(default)]
    pub empty: bool,
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

/// The id of a Microsoft Teams workspace's chat section: the catch-all
/// for its 1:1, group and meeting chats, a direct-message section titled
/// "Chat" as Teams calls it.
pub const TEAMS_CHAT_SECTION: &str = "teams:chat";

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
    /// A picture for its header: a Microsoft Teams team's.
    #[serde(default)]
    pub icon: Option<String>,
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
    /// Who uploaded it: only they may delete it here.
    pub user: Option<String>,
    /// Deleted: Slack keeps the message and says "This file was deleted"
    /// in its place, and only the id is left.
    pub deleted: bool,
    /// Slack's kind for it (`python`, `json`, `xlsx`), which names the
    /// language a preview is coloured as.
    pub filetype: String,
    /// The first lines of a snippet or text file, as Slack sends them, so
    /// it can be glanced at without fetching it.
    pub preview: Option<TextPreview>,
    /// A PDF Slack made of an Office document, for "Open as PDF".
    pub converted_pdf: Option<String>,
    /// A smaller copy of a video Slack made, quicker to fetch for playing.
    pub mp4_low: Option<String>,
    /// An MP4 copy of an old WebM voice clip, which more players open.
    pub aac: Option<String>,
    /// How long a video or sound lasts, in milliseconds.
    pub duration_ms: Option<u64>,
    /// A voice clip recorded in Slack, shown as a waveform rather than as a
    /// file.
    pub voice: bool,
    /// A voice clip's loudness over its length, from 0 to 100, as Slack
    /// measured it (a hundred of them).
    pub wave: Vec<u8>,
    /// The start of what Slack heard in a voice clip or video.
    pub transcript: Option<String>,
}

/// The first lines of a text file, as Slack previews it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TextPreview {
    /// Plain text: Slack's highlighted HTML is never used.
    pub text: String,
    /// How many lines of the file the preview leaves out, when Slack says.
    pub lines_more: Option<u32>,
    /// How many lines the whole file has, when Slack says.
    pub lines: Option<u32>,
    /// Whether Slack cut the preview short.
    pub truncated: bool,
}

/// What a preview leaves unshown, for the line under it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum More {
    /// The preview shows the whole file.
    Nothing,
    /// This many lines more.
    Lines(u32),
    /// More, but Slack did not say how much.
    Unknown,
}

impl TextPreview {
    /// The lines to show, at most `cap` of them, and what is left out:
    /// the preview's own lines past the cap plus the lines Slack left out
    /// of it.
    pub fn shown(&self, cap: usize) -> (Vec<&str>, More) {
        let all: Vec<&str> = self.text.trim_end().lines().collect();
        let shown: Vec<&str> = all.iter().copied().take(cap).collect();
        let cut = u32::try_from(all.len() - shown.len()).unwrap_or(u32::MAX);
        let count = u32::try_from(shown.len()).unwrap_or(u32::MAX);
        let more = match (self.lines_more, self.lines) {
            (Some(more), _) => Some(cut.saturating_add(more)),
            (None, Some(lines)) => Some(lines.saturating_sub(count)),
            (None, None) => None,
        };
        let more = match more {
            Some(0) if !self.truncated => More::Nothing,
            Some(0) => More::Unknown,
            Some(n) => More::Lines(n),
            None if self.truncated => More::Unknown,
            None if cut > 0 => More::Lines(cut),
            None => More::Nothing,
        };
        (shown, more)
    }
}

/// How long a video or sound lasts, as a player shows it: "0:07",
/// "4:39", "1:02:05". Rounded down to the second, so a clip never seems
/// longer than it is.
pub fn duration_text(ms: u64) -> String {
    let seconds = ms / 1000;
    let (hours, minutes, seconds) = (seconds / 3600, seconds / 60 % 60, seconds % 60);
    if hours > 0 {
        format!("{hours}:{minutes:02}:{seconds:02}")
    } else {
        format!("{minutes}:{seconds:02}")
    }
}

/// `name` with its extension replaced by `ext`, or `ext` added when it has
/// none: what a converted copy of a file is called.
fn with_extension(name: &str, ext: &str) -> String {
    let name = name.trim();
    let stem = match name.rsplit_once('.') {
        Some((stem, old)) if !stem.is_empty() && !old.is_empty() && !old.contains(' ') => stem,
        _ => name,
    };
    if stem.is_empty() {
        format!("file.{ext}")
    } else {
        format!("{stem}.{ext}")
    }
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

    /// What "Open as PDF" fetches and the name it is opened under: Slack's
    /// PDF of an Office document, called after the document ("Budget.xlsx"
    /// opens as "Budget.pdf").
    pub fn as_pdf(&self) -> Option<(String, String)> {
        let url = self.converted_pdf.clone()?;
        Some((url, with_extension(self.shown_name(), "pdf")))
    }

    /// What plays in the system's player and the name it is saved under:
    /// the smaller copy of a video and the MP4 of an old WebM voice clip
    /// when Slack made one (named for what they are, so the player knows
    /// them), else the file itself.
    pub fn player(&self) -> Option<(String, String)> {
        let copy = match self.media() {
            Some(Media::Video) => self.mp4_low.clone().map(|url| (url, "mp4")),
            Some(Media::Audio) => self.aac.clone().map(|url| (url, "m4a")),
            None => None,
        };
        if let Some((url, ext)) = copy {
            return Some((url, with_extension(self.shown_name(), ext)));
        }
        self.url_private
            .clone()
            .or_else(|| self.download_url.clone())
            .map(|url| (url, self.name.clone()))
    }

    /// The name to call it by: its file name, else its title.
    fn shown_name(&self) -> &str {
        if self.name.trim().is_empty() {
            &self.title
        } else {
            &self.name
        }
    }

    /// Whether `me` may delete it from here: its uploader, while it is
    /// still there. Admins may delete other people's files too, but this
    /// client does not offer that.
    pub fn deletable_by(&self, me: &str) -> bool {
        !self.deleted && !me.is_empty() && self.user.as_deref() == Some(me)
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
    /// Set when Slack unfurled a link to a Slack message: the card is then
    /// drawn as a quote of that message rather than as a link preview.
    pub quote: Option<Quote>,
}

/// A Slack message quoted under the one that links to it, the way Slack
/// shows a permalink: who wrote it, where and when, and how it starts.
///
/// Built from Slack's own unfurl of the link when it sent one, or else
/// from the message itself, loaded or fetched (see [`crate::quotes`]).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Quote {
    /// The link it quotes, which opens the message.
    pub url: String,
    /// The conversation it is in, when known.
    pub channel: Option<String>,
    /// What to call that conversation ("general"), when known.
    pub channel_name: Option<String>,
    /// When it was posted, when known.
    pub ts: Option<Ts>,
    /// Who wrote it, by user id, when known.
    pub user: Option<String>,
    /// The name to show for its author.
    pub author: Option<String>,
    /// The author's picture.
    pub author_icon: Option<String>,
    /// The quoted text, in mrkdwn.
    pub text: String,
    /// Set when the message is deleted or cannot be read: the card then
    /// says so instead of quoting it.
    pub unavailable: bool,
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

/// Why an interactive button or menu cannot be used here.
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

/// Pressing an app's interactive button, or choosing from its menu, on
/// message `ts` in `channel`.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Press {
    pub channel: String,
    pub ts: Ts,
    /// The bot that posted the message: Slack hands it the press.
    pub bot_id: String,
    pub block_id: String,
    pub action_id: String,
    /// The button's label, or the choice's, as Slack sent it, which the
    /// press repeats.
    pub text: String,
    /// The button's value, or the choice's.
    pub value: Option<String>,
    /// What was pressed, which shapes what Slack is sent.
    pub kind: PressKind,
}

impl Press {
    /// Whether `other` is on the same button or menu, whatever was chosen
    /// from it: a menu is busy while any choice of it is on its way.
    pub fn same_control(&self, other: &Press) -> bool {
        self.channel == other.channel
            && self.ts == other.ts
            && self.block_id == other.block_id
            && self.action_id == other.action_id
    }

    /// This menu's press with `choice` chosen.
    pub fn choosing(&self, choice: &MenuChoice) -> Press {
        Press {
            text: choice.text.clone(),
            value: Some(choice.value.clone()),
            ..self.clone()
        }
    }
}

/// The kind of element a [`Press`] comes from.
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash)]
pub enum PressKind {
    #[default]
    Button,
    /// A `static_select`, whose placeholder the press repeats.
    Select { placeholder: Option<String> },
    /// An `overflow` menu.
    Overflow,
    /// A set of `radio_buttons`.
    Radio,
}

/// What kind of menu an app's [`Menu`] is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MenuKind {
    /// A `static_select`: a drop-down that shows its choice.
    Select,
    /// An `overflow` menu: a "⋯" with a list of things to do.
    Overflow,
    /// `radio_buttons`: every choice in view, one of them picked.
    Radio,
}

/// One choice of an app's menu (Slack's option object).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct MenuChoice {
    /// The label as Slack sends it.
    pub text: String,
    /// What the app put in the choice for itself.
    pub value: String,
    /// A line under the label, in mrkdwn.
    pub description: Option<String>,
    /// A link an overflow choice opens as well.
    pub url: Option<String>,
}

/// Choices under a heading; a menu without headings has one group with
/// none.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ChoiceGroup {
    pub label: Option<String>,
    pub choices: Vec<MenuChoice>,
}

/// An app's Block Kit menu: a static select, an overflow menu or radio
/// buttons. Choosing from it works as a button press does (see
/// [`menu_use`]).
#[derive(Clone, Debug, PartialEq)]
pub struct Menu {
    pub kind: MenuKind,
    /// The app's own name for the menu, which the choice carries back.
    pub action_id: Option<String>,
    /// The block the menu sits in, which the choice names too.
    pub block_id: Option<String>,
    /// What an empty select says.
    pub placeholder: Option<String>,
    pub groups: Vec<ChoiceGroup>,
    /// The choice the app shows as made.
    pub initial: Option<MenuChoice>,
    /// The question to ask before a choice is sent, when the app wants one.
    pub confirm: Option<Confirm>,
}

impl Menu {
    /// Every choice, across its groups.
    pub fn choices(&self) -> impl Iterator<Item = &MenuChoice> {
        self.groups.iter().flat_map(|group| &group.choices)
    }

    /// The choice whose value is `value`.
    pub fn choice(&self, value: &str) -> Option<&MenuChoice> {
        self.choices().find(|choice| choice.value == value)
    }

    /// The id of this menu's drop-down on message `ts` in `channel` (in
    /// the thread panel or not), which a demo opens to show it.
    pub fn popup_id(&self, channel: &str, ts: &Ts, in_thread: bool) -> egui::Id {
        egui::Id::new((
            "kit-menu",
            channel,
            ts.as_str(),
            in_thread,
            self.block_id.as_deref(),
            self.action_id.as_deref(),
        ))
    }
}

/// An app's element only Slack itself can use here, such as a select
/// whose choices come from the app, a date picker or a text input: shown
/// by its label, never usable.
#[derive(Clone, Debug, PartialEq)]
pub struct Unusable {
    /// Slack's name for the element, such as `datepicker`, from which a
    /// label is worded when the app gave none.
    pub kind: String,
    /// What it says, as Slack sends it: its placeholder, or its text.
    pub label: Option<String>,
}

/// One element of an `actions` block.
#[derive(Clone, Debug, PartialEq)]
pub enum KitElement {
    Button(Button),
    Menu(Menu),
    Unusable(Unusable),
}

/// Where a choice from `menu` on `message` (in `channel`) goes in a
/// workspace signed in by `sign_in`: the press without a choice yet, to
/// [`Press::choosing`] one, or why there is none. As with [`button_use`],
/// only a browser session can send it.
pub fn menu_use(
    sign_in: SignInKind,
    channel: &str,
    message: &Message,
    menu: &Menu,
) -> Result<Press, NotHere> {
    let (bot_id, block_id, action_id) = press_target(
        sign_in,
        message,
        menu.block_id.as_ref(),
        menu.action_id.as_ref(),
    )?;
    let kind = match menu.kind {
        MenuKind::Select => PressKind::Select {
            placeholder: menu.placeholder.clone(),
        },
        MenuKind::Overflow => PressKind::Overflow,
        MenuKind::Radio => PressKind::Radio,
    };
    Ok(Press {
        channel: channel.to_owned(),
        ts: message.ts.clone(),
        bot_id,
        block_id,
        action_id,
        text: String::new(),
        value: None,
        kind,
    })
}

/// What pressing does next, by [`press_step`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PressStep<'a> {
    /// Ask the app's question; nothing opens or is sent until it is
    /// answered, and backing out does neither.
    Ask,
    /// Open `open`, if the choice has a link, and send the press.
    Go { open: Option<&'a str> },
}

/// Whether a press asks the app's `confirm` question first, or goes ahead
/// (`confirmed` once it was answered yes). An overflow choice's `link`
/// opens only when it goes ahead, so backing out opens nothing.
pub fn press_step<'a>(
    confirm: Option<&Confirm>,
    confirmed: bool,
    link: Option<&'a str>,
) -> PressStep<'a> {
    if confirm.is_some() && !confirmed {
        PressStep::Ask
    } else {
        PressStep::Go { open: link }
    }
}

/// The bot, block and action a press on `message` names, when `sign_in`
/// can send one at all.
fn press_target(
    sign_in: SignInKind,
    message: &Message,
    block_id: Option<&String>,
    action_id: Option<&String>,
) -> Result<(String, String, String), NotHere> {
    if sign_in != SignInKind::Session {
        return Err(NotHere::NeedsSession);
    }
    let (Some(bot_id), Some(block_id), Some(action_id)) =
        (message.bot_id.as_ref(), block_id, action_id)
    else {
        return Err(NotHere::NoApp);
    };
    if message.ts.is_local() {
        return Err(NotHere::NoApp);
    }
    Ok((bot_id.clone(), block_id.clone(), action_id.clone()))
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
    match press_target(
        sign_in,
        message,
        button.block_id.as_ref(),
        button.action_id.as_ref(),
    ) {
        Ok((bot_id, block_id, action_id)) => ButtonUse::Press(Press {
            channel: channel.to_owned(),
            ts: message.ts.clone(),
            bot_id,
            block_id,
            action_id,
            text: button.text.clone(),
            value: button.value.clone(),
            kind: PressKind::Button,
        }),
        Err(why) => ButtonUse::NotHere(why),
    }
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
    /// A select, an overflow menu or radio buttons.
    Menu(Box<Menu>),
    /// An element only Slack itself can use here.
    Unusable(Unusable),
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
        /// The picture's size, which Slack adds to the blocks it hands
        /// back, so it takes its place before it has loaded.
        size: Option<[f32; 2]>,
    },
    /// Buttons, menus and what only Slack can use, in a row.
    Actions(Vec<KitElement>),
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
    /// On a thread's parent, whether you follow the thread, when Slack
    /// said (browser sessions are told; other sign-ins are not).
    pub subscribed: Option<bool>,
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
    let hex = crate::text::hex(&bytes);
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

/// Puts a newer copy of a message in place of `existing`.
fn replace(existing: &mut Message, mut message: Message) {
    // A copy that gives no reply count may be trimmed, as some
    // edits and API answers are: keep the thread counters already
    // known. One that gives a count, even zero, carries Slack's
    // real counters, and zero then means the replies are gone.
    if !message.replies_known {
        message.reply_count = existing.reply_count;
        message.replies_known = existing.replies_known;
        message.reply_users = std::mem::take(&mut existing.reply_users);
        message.latest_reply = existing.latest_reply.take();
        if message.thread_ts.is_none() {
            message.thread_ts = existing.thread_ts.take();
        }
    }
    // Only some copies say whether you follow the thread.
    if message.subscribed.is_none() {
        message.subscribed = existing.subscribed;
    }
    *existing = message;
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
            replace(existing, message);
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
    /// ones go in front. The same as [`Self::upsert`] for each message in
    /// turn, but in one pass over the list rather than one per message.
    pub fn merge(&mut self, page: Vec<Message>) {
        let real = self.first_local();
        let ordered = self.messages[..real].windows(2).all(|w| w[0].ts < w[1].ts)
            && self.messages[real..].iter().all(|m| m.ts.is_local());
        if !ordered {
            // Not the shape `upsert` keeps (changed by hand): go its way.
            for message in page {
                self.upsert(message);
            }
            return;
        }
        let (mut fresh, local): (Vec<Message>, Vec<Message>) =
            page.into_iter().partition(|m| !m.ts.is_local());
        // Stable, so copies of one message keep the page's order.
        fresh.sort_by(|a, b| a.ts.cmp(&b.ts));
        let mut old = std::mem::take(&mut self.messages).into_iter().peekable();
        let mut merged = Vec::with_capacity(old.len() + fresh.len());
        for message in fresh {
            // What is here comes first, so a page's copy replaces it.
            while let Some(kept) = old.next_if(|m| !m.ts.is_local() && m.ts <= message.ts) {
                merged.push(kept);
            }
            match merged.last_mut() {
                Some(last) if last.ts == message.ts => replace(last, message),
                _ => merged.push(message),
            }
        }
        merged.extend(old);
        self.messages = merged;
        for message in local {
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
    /// Fetches message `ts` of `channel` in workspace `team` to quote
    /// under a link to it; `thread` is its parent for a reply. Asked once
    /// per message, however often its link is drawn.
    FetchQuote {
        team: String,
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
    /// Opens the "Add emoji" dialog (browser-session sign-ins).
    AddEmoji,
    /// Shows a file picker for the new emoji's picture.
    PickEmojiImage,
    /// Adds the emoji the dialog holds to the workspace.
    SendEmoji,
    /// Asks before deleting your file `file`, called `name`.
    AskDeleteFile {
        file: String,
        name: String,
    },
    /// Deletes your file `file` for everyone (`files.delete`): it goes
    /// from the screen at once, and comes back if Slack refuses.
    DeleteFile {
        file: String,
        name: String,
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
    /// Opens the meetings dialog for the workspace shown: "Meet now", or
    /// join by link or ID (Teams).
    OpenMeetings,
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
    /// Opens a spreadsheet, CSV file, archive or text file in the app's
    /// own viewer. `size` is the file's, as Slack gives it.
    ViewFile {
        url: String,
        name: String,
        filetype: String,
        kind: crate::viewer::Kind,
        size: u64,
    },
    /// Closes the file viewer.
    CloseViewer,
    OpenUrl(String),
    /// Plays, pauses, seeks or stops a sound in the app (see
    /// [`crate::audio`]).
    Audio(crate::audio::Request),
    /// Presses an app's interactive button, or sends a menu choice. When
    /// the app asked for a `confirm` dialog, this asks first, unless
    /// `confirmed`; `link` (an overflow choice's) opens only once it goes
    /// ahead (see [`press_step`]).
    PressButton {
        /// Boxed: a press is far larger than most actions.
        press: Box<Press>,
        confirm: Option<Confirm>,
        confirmed: bool,
        link: Option<String>,
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
    /// Changes the theme, as the settings' Theme choice does.
    SetAppearance(crate::settings::Appearance),
    /// Changes after how long quiet conversations are hidden, as the
    /// settings' choice does.
    HideInactive(crate::sidebar::HideInactive),
    AddWorkspace,
    SignOut(String),
    Reconnect,
    /// Uses the proxy now in the settings and restarts the connections.
    ApplyProxy,
    /// Loads the spelling dictionary now in the settings, or stops
    /// checking.
    ApplySpelling,
    DismissError,
    // Sign-in. These carry no secrets: the app reads the typed link, token
    // and credentials from its form, so they never sit in an action that
    // might be printed.
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
    /// Starts OAuth asking only for the scopes an app made from an older
    /// manifest has, and remembers that the app is one.
    SignInOlder,
    /// Starts OAuth asking for every scope again, after you updated your
    /// app from the current manifest.
    SignInUpdated,
    /// Starts Microsoft Teams Device Code sign-in with an optional tenant domain or ID.
    StartTeamsSignIn {
        tenant: Option<String>,
        personal: bool,
    },
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
    /// Answers a huddle invitation (see [`crate::huddles`]).
    Huddle(crate::huddles::Action),
    /// Lists or chooses a camera, microphone or speaker for huddles.
    Devices(crate::devices::Action),
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
            subscribed: None,
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
                kind: PressKind::Button,
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
    fn menu_choices_go_only_from_a_browser_session() {
        let posted = Message {
            bot_id: Some("B10".into()),
            ..message("1790171970.000100")
        };
        let beta = MenuChoice {
            text: "Beta".into(),
            value: "beta".into(),
            ..MenuChoice::default()
        };
        let select = Menu {
            kind: MenuKind::Select,
            action_id: Some("channel".into()),
            block_id: Some("rollout".into()),
            placeholder: Some("Pick a channel".into()),
            groups: vec![ChoiceGroup {
                label: None,
                choices: vec![beta.clone()],
            }],
            initial: None,
            confirm: None,
        };
        let press =
            menu_use(SignInKind::Session, "C05", &posted, &select).expect("a session can choose");
        assert_eq!(
            press.choosing(&beta),
            Press {
                channel: "C05".into(),
                ts: Ts::new("1790171970.000100"),
                bot_id: "B10".into(),
                block_id: "rollout".into(),
                action_id: "channel".into(),
                text: "Beta".into(),
                value: Some("beta".into()),
                kind: PressKind::Select {
                    placeholder: Some("Pick a channel".into()),
                },
            }
        );
        assert!(
            press.same_control(&press.choosing(&beta)),
            "busy whatever is chosen"
        );
        assert_eq!(
            menu_use(SignInKind::App, "C05", &posted, &select),
            Err(NotHere::NeedsSession)
        );
        let overflow = Menu {
            kind: MenuKind::Overflow,
            action_id: None,
            ..select.clone()
        };
        assert_eq!(
            menu_use(SignInKind::Session, "C05", &posted, &overflow),
            Err(NotHere::NoApp),
            "no action id, nothing for the app to tell apart"
        );
        let radio = Menu {
            kind: MenuKind::Radio,
            ..select.clone()
        };
        assert_eq!(
            menu_use(SignInKind::Session, "C05", &posted, &radio).map(|p| p.kind),
            Ok(PressKind::Radio)
        );
        let local = Message {
            ts: Ts::new("local-1"),
            ..posted.clone()
        };
        assert_eq!(
            menu_use(SignInKind::Session, "C05", &local, &select),
            Err(NotHere::NoApp),
            "a message not yet on Slack has nothing to answer"
        );
    }

    #[test]
    fn a_link_with_a_question_opens_only_once_answered_yes() {
        let confirm = Confirm::default();
        let link = Some("https://example.com/plan");
        assert_eq!(
            press_step(Some(&confirm), false, link),
            PressStep::Ask,
            "nothing opens before the answer; backing out ends here"
        );
        assert_eq!(
            press_step(Some(&confirm), true, link),
            PressStep::Go { open: link }
        );
        assert_eq!(
            press_step(None, false, link),
            PressStep::Go { open: link },
            "without a question, at once"
        );
        assert_eq!(
            press_step(Some(&confirm), true, None),
            PressStep::Go { open: None }
        );
    }

    /// How [`Timeline::merge`] used to work, to hold it to.
    fn merge_one_by_one(timeline: &mut Timeline, page: Vec<Message>) {
        for message in page {
            timeline.upsert(message);
        }
    }

    #[test]
    fn merging_a_page_is_upserting_each_message() {
        // A small fixed generator: the same cases every run.
        let mut seed = 0x2545_f491_4f6c_dd1d_u64;
        let mut next = |below: u64| {
            seed = seed
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            (seed >> 33) % below
        };
        let pool = [
            "1700000000.000100",
            "1700000000.000200",
            "1700000000.000300",
            "1700000001.000000",
            "1700000002.500000",
            "999999999.999999",
            "1700000000.5",
            "1700000000",
            "garbage",
            "1.x",
            "local-1",
            "local-2",
            "local-10",
        ];
        let mut copy = 0;
        let mut random = |next: &mut dyn FnMut(u64) -> u64| {
            copy += 1;
            let ts = pool[next(pool.len() as u64) as usize];
            Message {
                text: format!("{ts} copy {copy}"),
                replies_known: next(2) == 0,
                reply_count: next(4) as u32,
                reply_users: (0..next(3)).map(|i| format!("U{i}")).collect(),
                latest_reply: (next(2) == 0).then(|| Ts::new("1700000009.000000")),
                thread_ts: (next(2) == 0).then(|| Ts::new(ts)),
                subscribed: [None, Some(true), Some(false)][next(3) as usize],
                ..message(ts)
            }
        };
        for case in 0..3000 {
            let mut timeline = Timeline::default();
            for _ in 0..next(10) {
                let message = random(&mut next);
                if case % 10 == 0 {
                    // Some lists out of shape, changed by hand.
                    timeline.messages.push(message);
                } else {
                    timeline.upsert(message);
                }
            }
            let page: Vec<Message> = (0..next(14)).map(|_| random(&mut next)).collect();
            let mut expected = timeline.clone();
            merge_one_by_one(&mut expected, page.clone());
            timeline.merge(page);
            assert_eq!(timeline.messages, expected.messages, "case {case}");
        }
    }

    #[test]
    fn the_quick_order_of_timestamps_is_the_parsed_one() {
        let parsed = |a: &Ts, b: &Ts| a.key().cmp(&b.key()).then_with(|| a.0.cmp(&b.0));
        let tricky = [
            "1700000000.000100",
            "1700000000.000200",
            "1700000000.000099",
            "1700000001.000000",
            "0999999999.999999",
            "999999999.999999",
            "1700000000.1",
            "1700000000.5",
            "1700000000.10",
            "1700000000.100000",
            "1700000000.0000001",
            "1700000000.0000002",
            "1700000000.1234567",
            "1700000000.1234568",
            "1700000000.",
            "1700000000",
            "1800000000",
            "0001.000000",
            "0002.000000",
            "1000.000000",
            "1.5",
            "2.5",
            "01.5",
            "1.05",
            "0.000000",
            "9999999999999999999.000000",
            "1844674407370955161.5",
            "18446744073709551615.000000",
            "18446744073709551616.000000",
            "99999999999999999999.000000",
            "99999999999999999998.000000",
            ".500000",
            ".400000",
            "+170000000.000100",
            "-170000000.000100",
            "1700000000.00010x",
            "170000000x.000100",
            "1700000000.000.10",
            "1700000000..00010",
            "17000000 0.000100",
            "1700000000.00 100",
            "local-1",
            "local-9",
            "local-10",
            "local-x",
            "local-",
            "garbage",
            "",
            "١٧٠٠.٠٠٠١",
        ];
        for a in tricky {
            for b in tricky {
                let (a, b) = (Ts::new(a), Ts::new(b));
                assert_eq!(a.cmp(&b), parsed(&a, &b), "{a:?} against {b:?}");
            }
        }
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
    fn a_copy_that_does_not_say_keeps_whether_you_follow_the_thread() {
        let mut timeline = Timeline::default();
        timeline.upsert(Message {
            subscribed: Some(true),
            ..message("1.0")
        });
        timeline.upsert(message("1.0"));
        assert_eq!(timeline.messages[0].subscribed, Some(true));
        timeline.upsert(Message {
            subscribed: Some(false),
            ..message("1.0")
        });
        assert_eq!(timeline.messages[0].subscribed, Some(false));
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
            is_open: None,
            empty: false,
        };
        assert!(!c.has_unread());
        c.latest = Some(Ts::new("6.0"));
        assert!(c.has_unread());
    }

    fn preview(text: &str, lines_more: Option<u32>, lines: Option<u32>) -> TextPreview {
        TextPreview {
            text: text.into(),
            lines_more,
            lines,
            truncated: false,
        }
    }

    #[test]
    fn a_preview_shows_at_most_its_cap_and_counts_the_rest() {
        let ten = (1..=10)
            .map(|n| format!("line {n}"))
            .collect::<Vec<_>>()
            .join("\n");
        // Two lines past the cap, and the 30 Slack left out.
        let long = preview(&ten, Some(30), None);
        let (shown, more) = long.shown(8);
        assert_eq!(shown.len(), 8);
        assert_eq!(shown.last(), Some(&"line 8"));
        assert_eq!(more, More::Lines(32));
        // The whole file, within the cap.
        let whole = preview("a\nb\n\n", Some(0), Some(2));
        let (shown, more) = whole.shown(8);
        assert_eq!(shown, vec!["a", "b"], "trailing blank lines dropped");
        assert_eq!(more, More::Nothing);
        // Only the file's length known.
        assert_eq!(preview("a\nb", None, Some(40)).shown(8).1, More::Lines(38));
        // Nothing known but the cap.
        assert_eq!(preview(&ten, None, None).shown(8).1, More::Lines(2));
        assert_eq!(preview("a", None, None).shown(8).1, More::Nothing);
        // Cut short by Slack, without saying by how much.
        let cut = TextPreview {
            truncated: true,
            ..preview("a", None, None)
        };
        assert_eq!(cut.shown(8).1, More::Unknown);
    }

    #[test]
    fn only_real_timestamps_parse() {
        assert_eq!(
            Ts::parse("1700000000.000100"),
            Some(Ts::new("1700000000.000100"))
        );
        for bad in ["", "1700000000", "1700000000.", ".5", "17x.5", "local-3"] {
            assert_eq!(Ts::parse(bad), None, "{bad}");
        }
    }

    #[test]
    fn timestamps_split_into_seconds_and_micros() {
        assert_eq!(
            Ts::new("1700000000.000100").parts(),
            Some((1_700_000_000, 100))
        );
        assert_eq!(Ts::new("12.5").parts(), Some((12, 500_000)));
        assert_eq!(Ts::new("local-3").parts(), None);
        assert_eq!(Ts::new("12.x").parts(), None);
    }

    #[test]
    fn durations_read_like_a_players() {
        assert_eq!(duration_text(0), "0:00");
        assert_eq!(duration_text(999), "0:00", "rounded down");
        assert_eq!(duration_text(13_977), "0:13");
        assert_eq!(duration_text(279_145), "4:39");
        assert_eq!(duration_text(3_725_000), "1:02:05");
    }

    #[test]
    fn open_as_pdf_names_the_pdf_after_the_document() {
        let file = |name: &str| File {
            name: name.into(),
            title: "Quarterly numbers".into(),
            converted_pdf: Some(
                "https://files.slack.com/files-tmb/T-F-x/budget_converted.pdf".into(),
            ),
            ..File::default()
        };
        let name = |name: &str| file(name).as_pdf().map(|(_, name)| name);
        assert_eq!(name("Budget.xlsx").as_deref(), Some("Budget.pdf"));
        assert_eq!(name("Q3.final.pptx").as_deref(), Some("Q3.final.pdf"));
        assert_eq!(name("README").as_deref(), Some("README.pdf"));
        assert_eq!(
            name(".docx").as_deref(),
            Some(".docx.pdf"),
            "a name that is all extension"
        );
        assert_eq!(
            name("").as_deref(),
            Some("Quarterly numbers.pdf"),
            "the title"
        );
        assert_eq!(
            file("Budget.xlsx").as_pdf().map(|(url, _)| url).as_deref(),
            Some("https://files.slack.com/files-tmb/T-F-x/budget_converted.pdf")
        );
        let plain = File {
            converted_pdf: None,
            ..file("Budget.xlsx")
        };
        assert_eq!(plain.as_pdf(), None, "no PDF made");
    }

    #[test]
    fn the_player_gets_the_smaller_copy_when_there_is_one() {
        let video = File {
            name: "talk.mov".into(),
            mimetype: "video/quicktime".into(),
            url_private: Some("https://files.slack.com/files-pri/T-F/talk.mov".into()),
            ..File::default()
        };
        assert_eq!(
            video.player(),
            Some((
                "https://files.slack.com/files-pri/T-F/talk.mov".into(),
                "talk.mov".into()
            ))
        );
        let low = File {
            mp4_low: Some("https://files.slack.com/files-tmb/T-F-x/talk_trans.mp4".into()),
            ..video.clone()
        };
        assert_eq!(
            low.player(),
            Some((
                "https://files.slack.com/files-tmb/T-F-x/talk_trans.mp4".into(),
                "talk.mp4".into()
            ))
        );
        // A sound's MP4 copy is not a video's.
        let pdf = File {
            mimetype: "application/pdf".into(),
            aac: Some("https://files.slack.com/x.mp4".into()),
            ..video
        };
        assert_eq!(
            pdf.player().map(|(_, name)| name).as_deref(),
            Some("talk.mov")
        );
    }

    #[test]
    fn only_the_uploader_may_delete_a_file() {
        let file = File {
            id: "F1".into(),
            user: Some("U1".into()),
            ..File::default()
        };
        assert!(file.deletable_by("U1"));
        assert!(!file.deletable_by("U2"), "someone else's file");
        assert!(!file.deletable_by(""));
        let unknown = File {
            user: None,
            ..file.clone()
        };
        assert!(!unknown.deletable_by("U1"), "an uploader not known");
        let gone = File {
            deleted: true,
            ..file
        };
        assert!(!gone.deletable_by("U1"), "deleted already");
    }
}
