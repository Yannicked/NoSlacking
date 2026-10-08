//! The sidebar's shape: which conversation goes in which section, in what
//! order, and how an edit changes it.
//!
//! Slack keeps your sections (Starred, your own, Channels, Direct messages,
//! Apps) as a list in your order. Starred and your own sections name their
//! conversations; Channels and Direct messages take everything else, and
//! Apps takes direct messages with bots. Without sections (a Slack-app
//! sign-in cannot read them) the sidebar is just Channels and Direct
//! messages.
//!
//! Edits apply here at once, go to Slack, and are replaced by Slack's own
//! answer when the sections are fetched again.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};

use crate::model::{Conversation, SectionKind, SidebarSection, User};

/// How channels are ordered within their sections. Direct messages are
/// always newest first, as in Slack.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Sort {
    #[default]
    Name,
    Recent,
}

impl Sort {
    /// Every order, as the settings list them.
    pub const ALL: [Self; 2] = [Self::Name, Self::Recent];
}

/// When the sidebar tucks away a conversation that has gone quiet, as
/// Slack's own client tidies its sidebar.
#[derive(
    Clone, Copy, Debug, Default, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize,
)]
#[serde(rename_all = "snake_case")]
pub enum HideInactive {
    /// Every conversation shows.
    Off,
    /// After a week without a new message.
    Week,
    /// After a month without a new message.
    #[default]
    Month,
    /// After three months without a new message.
    ThreeMonths,
}

impl HideInactive {
    /// Every choice, shortest wait last but Off first, as the settings
    /// list them.
    pub const ALL: [Self; 4] = [Self::Off, Self::Week, Self::Month, Self::ThreeMonths];

    /// What the settings call the choice.
    pub fn label(self) -> String {
        use crate::i18n::t;
        match self {
            HideInactive::Off => t("Off"),
            HideInactive::Week => t("After 1 week"),
            HideInactive::Month => t("After 1 month"),
            HideInactive::ThreeMonths => t("After 3 months"),
        }
        .into_owned()
    }

    /// How long a conversation must have been quiet to be hidden, in
    /// seconds; `None` when nothing is hidden.
    pub fn age(self) -> Option<i64> {
        const DAY: i64 = 24 * 60 * 60;
        match self {
            HideInactive::Off => None,
            HideInactive::Week => Some(7 * DAY),
            HideInactive::Month => Some(30 * DAY),
            HideInactive::ThreeMonths => Some(90 * DAY),
        }
    }
}

/// How finely the time is taken for hiding quiet conversations, in
/// seconds. Hiding reads the clock, and the remembered layout is made
/// again whenever what it reads changes: by the hour it is made again
/// once an hour, not every frame, and nobody notices a conversation
/// going quiet an hour late.
pub const TIME_STEP: i64 = 60 * 60;

/// What hiding quiet conversations reads besides the conversations.
#[derive(Clone, Copy, Debug)]
pub struct Tidy<'a> {
    /// After how long a quiet conversation is hidden.
    pub after: HideInactive,
    /// The time now in Unix seconds. Only the [`TIME_STEP`] it falls in
    /// counts.
    pub now: i64,
    /// The conversations with a draft, which always show so what you
    /// started writing is not lost from sight.
    pub drafts: Option<&'a HashSet<String>>,
}

impl Tidy<'_> {
    /// The newest-message time before which a conversation counts as
    /// inactive, or `None` when nothing is hidden. Taken from the start
    /// of the [`TIME_STEP`], so it moves only when the step does.
    pub fn cutoff(&self) -> Option<i64> {
        let now = self.now.div_euclid(TIME_STEP) * TIME_STEP;
        Some(now.saturating_sub(self.after.age()?))
    }
}

/// Whether the sidebar hides `conversation`, in a section of `kind`, for
/// having gone quiet: its newest message is older than the cutoff, or
/// Slack said it has none at all (`Conversation::empty`), as a group DM
/// someone made and nobody wrote in.
///
/// Never hidden, whatever their age: one whose newest message is not
/// known (an OAuth sign-in may not know it), one that shows as unread
/// (`rank`, which follows the mute rules) or mentions you, one held open
/// (`hold`, which is the open conversation), one with a draft, and
/// anything in Starred, which you chose to keep in sight, or in Apps,
/// where apps are tools you go to rather than talks that run dry.
pub fn is_inactive(
    conversation: &Conversation,
    kind: SectionKind,
    rank: Rank,
    hold: Option<&Hold>,
    tidy: &Tidy<'_>,
) -> bool {
    let Some(cutoff) = tidy.cutoff() else {
        return false;
    };
    let kept = matches!(kind, SectionKind::Starred | SectionKind::Apps)
        || rank != Rank::Read
        || conversation.mentions > 0
        || hold.is_some_and(|held| held.id == conversation.id)
        || tidy
            .drafts
            .is_some_and(|drafts| drafts.contains(&conversation.id));
    !kept
        && match conversation.latest.as_ref() {
            Some(latest) => latest.seconds().is_some_and(|latest| latest < cutoff),
            None => conversation.empty,
        }
}

/// How much a conversation asks for you, for putting unread ones first.
/// The order of the variants is the order in a section.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Rank {
    /// A mention of you, or an unread direct message: someone is waiting.
    Urgent,
    /// Something new you have not read.
    Unread,
    /// Nothing new, or nothing you asked to hear about (a muted channel).
    #[default]
    Read,
}

/// A conversation's [`Rank`], given whether it shows as unread (see
/// `WorkspaceState::is_unread`, which already leaves muted channels out
/// unless they mention you).
pub fn rank(conversation: &Conversation, unread: bool) -> Rank {
    if !unread {
        Rank::Read
    } else if conversation.mentions > 0 || conversation.kind.is_dm() {
        Rank::Urgent
    } else {
        Rank::Unread
    }
}

/// The open conversation and the rank it keeps while it stays open.
///
/// Opening a conversation reads it, and without this it would leave the
/// unread rows at once and jump away from under the pointer. It goes to
/// its new place when another conversation opens.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Hold {
    /// The open conversation.
    pub id: String,
    /// The rank it is placed by while open.
    pub rank: Rank,
}

/// How [`layout`] orders the conversations within each section.
#[derive(Clone, Copy, Debug, Default)]
pub struct Arrange<'a> {
    /// Channels by name or by activity; direct messages are always by
    /// activity.
    pub sort: Sort,
    /// Mentions and unread direct messages first, then other unread
    /// conversations, then the rest, each group in the `sort` order.
    pub unread_first: bool,
    /// The open conversation's held rank, used in place of its own. The
    /// held conversation is never hidden as inactive either.
    pub hold: Option<&'a Hold>,
    /// Which conversations to mark as gone quiet (see [`is_inactive`]);
    /// `None` marks none.
    pub tidy: Option<Tidy<'a>>,
    /// A number that changes whenever the sections, the conversations,
    /// the people or the ranks [`Memo::layout`] is given may have (the
    /// workspace's [`revision`](crate::app::WorkspaceState::revision)).
    /// While it and the rest of this stay the same, the memo answers
    /// without walking the conversations; `None` walks them every time.
    pub revision: Option<u64>,
}

impl Arrange<'_> {
    /// Ordered by `sort` alone, with unread conversations in among the rest.
    pub fn plain(sort: Sort) -> Self {
        Arrange {
            sort,
            unread_first: false,
            hold: None,
            tidy: None,
            revision: None,
        }
    }
}

/// What to hold once `active` is the open conversation. The same one open
/// as before keeps what it held; a newly opened one holds the rank it had
/// when it was last shown (`seen`), before opening it read it.
pub fn hold(
    previous: Option<&Hold>,
    active: Option<&str>,
    seen: impl Fn(&str) -> Rank,
) -> Option<Hold> {
    let active = active?;
    match previous {
        Some(held) if held.id == active => Some(held.clone()),
        _ => Some(Hold {
            id: active.to_owned(),
            rank: seen(active),
        }),
    }
}

/// A change to the sidebar, made here and sent to Slack.
#[derive(Clone, Debug, PartialEq)]
pub enum SidebarEdit {
    /// A new section of your own, optionally taking a conversation along.
    Create {
        name: String,
        channel: Option<String>,
    },
    Rename {
        section: String,
        name: String,
    },
    Delete {
        section: String,
    },
    /// Moves a section up or down one place.
    Shift {
        section: String,
        up: bool,
    },
    /// Moves a conversation to another section (or back to its catch-all).
    Move {
        channel: String,
        from: Option<String>,
        to: String,
    },
    Star {
        channel: String,
        starred: bool,
    },
}

/// One Slack call that carries out (part of) an edit.
#[derive(Clone, Debug, PartialEq)]
pub enum SidebarCall {
    /// `users.channelSections.create`, then optionally moving a
    /// conversation into the new section (and out of its old one).
    Create {
        name: String,
        channel: Option<String>,
        remove_from: Option<String>,
    },
    /// `users.channelSections.set`: a new name, or a place before `next`.
    Set {
        section: String,
        name: Option<String>,
        next: Option<String>,
    },
    /// `users.channelSections.delete`.
    Delete { section: String },
    /// `users.channelSections.channels.bulkUpdate`: out of one section,
    /// into another, in one call.
    Channels {
        channel: String,
        insert: Option<String>,
        remove: Option<String>,
    },
    /// `stars.add` / `stars.remove`.
    Star { channel: String, starred: bool },
}

/// What the ids of sections made here start with, until Slack's own id
/// arrives with the next fetch.
const LOCAL_PREFIX: &str = "local-";

/// Whether a section was made here and Slack does not know its id yet.
pub fn is_local(section: &str) -> bool {
    section.starts_with(LOCAL_PREFIX)
}

/// A new local section id. A counter rather than the number of sections,
/// which repeats once a section is deleted and another made.
fn local_id() -> String {
    static NEXT: AtomicU64 = AtomicU64::new(1);
    format!("{LOCAL_PREFIX}{}", NEXT.fetch_add(1, Ordering::Relaxed))
}

/// Whether the sidebar hides a section, so moving past it changes nothing
/// on screen. Only an empty Starred section is known to be hidden from the
/// sections alone; whether Apps shows depends on the conversations, so it
/// counts as shown.
fn hidden(section: &SidebarSection) -> bool {
    section.kind == SectionKind::Starred && section.channel_ids.is_empty()
}

/// For moving the section at `at` one place, the section that changes
/// place and the one it goes right before. Slack's call puts one section
/// before another, so moving down is moving the next one up. Hidden
/// sections are stepped over.
fn shift(sections: &[SidebarSection], at: usize, up: bool) -> Option<(usize, usize)> {
    if up {
        let previous = (0..at).rev().find(|&i| !hidden(&sections[i]))?;
        Some((at, previous))
    } else {
        let next = (at + 1..sections.len()).find(|&i| !hidden(&sections[i]))?;
        Some((next, at))
    }
}

/// Whether the section `id` can move one place up (or down): whether a
/// shown section sits on that side of it. The same rule as [`plan`] and
/// [`apply`] follow, so the menu never offers a move that does nothing.
pub fn can_shift(sections: &[SidebarSection], id: &str, up: bool) -> bool {
    sections
        .iter()
        .position(|s| s.id == id)
        .is_some_and(|at| shift(sections, at, up).is_some())
}

/// The Slack calls an edit needs, worked out from the sections before it.
///
/// Sections made here have no Slack id until the next fetch, so calls that
/// would name one are left out; the fetch that follows the creation then
/// shows what Slack has.
pub fn plan(sections: &[SidebarSection], edit: &SidebarEdit) -> Vec<SidebarCall> {
    let kind = |id: &str| sections.iter().find(|s| s.id == id).map(|s| s.kind);
    // The custom section that names a conversation, if Slack knows it.
    let custom_home = |channel: &str| {
        sections
            .iter()
            .find(|s| s.kind == SectionKind::Custom && s.channel_ids.iter().any(|id| id == channel))
            .map(|s| s.id.clone())
            .filter(|id| !is_local(id))
    };
    match edit {
        SidebarEdit::Create { name, channel } => vec![SidebarCall::Create {
            name: name.clone(),
            channel: channel.clone(),
            remove_from: channel.as_deref().and_then(custom_home),
        }],
        SidebarEdit::Rename { section, .. } | SidebarEdit::Delete { section }
            if is_local(section) =>
        {
            Vec::new()
        }
        SidebarEdit::Rename { section, name } => vec![SidebarCall::Set {
            section: section.clone(),
            name: Some(name.clone()),
            next: None,
        }],
        SidebarEdit::Delete { section } => vec![SidebarCall::Delete {
            section: section.clone(),
        }],
        SidebarEdit::Shift { section, up } => {
            let Some(at) = sections.iter().position(|s| s.id == *section) else {
                return Vec::new();
            };
            let Some((moved, before)) = shift(sections, at, *up) else {
                return Vec::new();
            };
            let (moved, before) = (&sections[moved].id, &sections[before].id);
            if is_local(moved) || is_local(before) {
                return Vec::new();
            }
            vec![SidebarCall::Set {
                section: moved.clone(),
                name: None,
                next: Some(before.clone()),
            }]
        }
        SidebarEdit::Move { channel, from, to } => {
            let mut calls = Vec::new();
            let from_kind = from.as_deref().and_then(kind);
            let to_kind = kind(to);
            if from_kind == Some(SectionKind::Starred) && to_kind != Some(SectionKind::Starred) {
                calls.push(SidebarCall::Star {
                    channel: channel.clone(),
                    starred: false,
                });
            }
            let remove = custom_home(channel).filter(|home| home != to);
            let insert =
                (to_kind == Some(SectionKind::Custom) && !is_local(to)).then(|| to.clone());
            if insert.is_some() || remove.is_some() {
                calls.push(SidebarCall::Channels {
                    channel: channel.clone(),
                    insert,
                    remove,
                });
            }
            if to_kind == Some(SectionKind::Starred) {
                calls.push(SidebarCall::Star {
                    channel: channel.clone(),
                    starred: true,
                });
            }
            calls
        }
        SidebarEdit::Star { channel, starred } => vec![SidebarCall::Star {
            channel: channel.clone(),
            starred: *starred,
        }],
    }
}

/// One section as the sidebar draws it.
#[derive(Debug, PartialEq)]
pub struct Shown<'a> {
    /// Slack's id; `None` for the sidebar without sections.
    pub id: Option<String>,
    pub kind: SectionKind,
    pub title: String,
    /// A picture for its header, if the section has one.
    pub icon: Option<String>,
    pub conversations: Vec<&'a Conversation>,
    /// For each of `conversations`, in the same order, whether it has gone
    /// quiet and is hidden until the section is expanded.
    pub inactive: Vec<bool>,
}

/// The title Slack shows for a section.
pub fn title(section: &SidebarSection) -> String {
    match section.kind {
        SectionKind::Custom => {
            let emoji = (!section.emoji.is_empty())
                .then(|| crate::emoji::unicode(&section.emoji, None))
                .flatten();
            match emoji {
                Some(emoji) => format!("{emoji} {}", section.name),
                None => section.name.clone(),
            }
        }
        SectionKind::Starred => crate::i18n::t("Starred").into_owned(),
        SectionKind::Channels => crate::i18n::t("Channels").into_owned(),
        SectionKind::DirectMessages if section.id == crate::model::TEAMS_CHAT_SECTION => {
            crate::i18n::t("Chat").into_owned()
        }
        SectionKind::DirectMessages => crate::i18n::t("Direct messages").into_owned(),
        SectionKind::Apps => crate::i18n::t("Apps").into_owned(),
    }
}

/// Whether a conversation was closed (see `Settings::closed`) and has had
/// nothing new since.
pub fn is_closed(
    closed: Option<&std::collections::BTreeMap<String, String>>,
    conversation: &Conversation,
) -> bool {
    closed
        .and_then(|closed| closed.get(&conversation.id))
        .is_some_and(|at| {
            conversation
                .latest
                .as_ref()
                .is_none_or(|latest| *latest <= crate::model::Ts::new(at.clone()))
        })
}

/// Whether Slack says a direct message or group DM is closed (see
/// `Conversation::is_open`) and nothing brings it back: it is not unread
/// (`unread`), does not mention you, and has no draft (`draft`). Slack's
/// own client leaves such conversations out of the sidebar. One whose
/// state Slack did not give is never shut, and neither is a channel.
/// The sidebar also shows a shut one while it is the open conversation.
pub fn is_shut(conversation: &Conversation, unread: bool, draft: bool) -> bool {
    conversation.kind.is_dm()
        && conversation.is_open == Some(false)
        && !unread
        && conversation.mentions == 0
        && !draft
}

/// The sections to draw, each with its conversations in order. `rank`
/// says how much each conversation asks for you; it matters only when
/// `arrange` puts unread ones first.
pub fn layout<'a>(
    sections: Option<&[SidebarSection]>,
    conversations: &'a [Conversation],
    users: &HashMap<String, User>,
    titled: impl Fn(&Conversation) -> String,
    rank: impl Fn(&Conversation) -> Rank,
    arrange: &Arrange<'_>,
) -> Vec<Shown<'a>> {
    let sort = arrange.sort;
    let order = |rows: &mut [&Conversation], sort: Sort| order(rows, sort, &titled, &rank, arrange);
    let inactive = |rows: &[&Conversation], kind: SectionKind| -> Vec<bool> {
        rows.iter()
            .map(|c| {
                arrange
                    .tidy
                    .is_some_and(|tidy| is_inactive(c, kind, rank(c), arrange.hold, &tidy))
            })
            .collect()
    };
    let is_bot_dm = |c: &Conversation| {
        c.user
            .as_deref()
            .and_then(|id| users.get(id))
            .is_some_and(|u| u.is_bot)
    };
    let Some(sections) = sections.filter(|s| !s.is_empty()) else {
        let mut channels: Vec<&Conversation> =
            conversations.iter().filter(|c| !c.kind.is_dm()).collect();
        let mut direct: Vec<&Conversation> =
            conversations.iter().filter(|c| c.kind.is_dm()).collect();
        order(&mut channels, sort);
        order(&mut direct, Sort::Recent);
        return vec![
            Shown {
                id: None,
                kind: SectionKind::Channels,
                title: crate::i18n::t("Channels").into_owned(),
                icon: None,
                inactive: inactive(&channels, SectionKind::Channels),
                conversations: channels,
            },
            Shown {
                id: None,
                kind: SectionKind::DirectMessages,
                title: crate::i18n::t("Direct messages").into_owned(),
                icon: None,
                inactive: inactive(&direct, SectionKind::DirectMessages),
                conversations: direct,
            },
        ];
    };
    let by_id: HashMap<&str, &Conversation> =
        conversations.iter().map(|c| (c.id.as_str(), c)).collect();
    let mut placed: HashMap<&str, usize> = HashMap::new();
    // Starred wins over your own sections, as in Slack.
    let explicit_first = sections
        .iter()
        .enumerate()
        .filter(|(_, s)| s.kind == SectionKind::Starred)
        .chain(
            sections
                .iter()
                .enumerate()
                .filter(|(_, s)| s.kind != SectionKind::Starred),
        );
    for (index, section) in explicit_first {
        for id in &section.channel_ids {
            if let Some(conversation) = by_id.get(id.as_str()) {
                placed.entry(conversation.id.as_str()).or_insert(index);
            }
        }
    }
    let find = |kind: SectionKind| sections.iter().position(|s| s.kind == kind);
    let channels_at = find(SectionKind::Channels);
    let direct_at = find(SectionKind::DirectMessages).or(channels_at);
    let apps_at = find(SectionKind::Apps).or(direct_at);
    let mut rows: Vec<Vec<&Conversation>> = vec![Vec::new(); sections.len()];
    let mut stray: Vec<&Conversation> = Vec::new();
    for conversation in conversations {
        let index = placed.get(conversation.id.as_str()).copied().or_else(|| {
            if is_bot_dm(conversation) {
                apps_at
            } else if conversation.kind.is_dm() {
                direct_at
            } else {
                channels_at
            }
        });
        match index {
            Some(index) => rows[index].push(conversation),
            None => stray.push(conversation),
        }
    }
    let mut shown: Vec<Shown<'a>> = sections
        .iter()
        .zip(rows)
        .filter(|(section, rows)| match section.kind {
            // Empty Starred and Apps sections are hidden, as in Slack;
            // your own stay so you can drop things into them.
            SectionKind::Starred | SectionKind::Apps => !rows.is_empty(),
            _ => true,
        })
        .map(|(section, mut rows)| {
            let sort = match section.kind {
                SectionKind::DirectMessages | SectionKind::Apps => Sort::Recent,
                _ => sort,
            };
            order(&mut rows, sort);
            Shown {
                id: Some(section.id.clone()),
                kind: section.kind,
                title: title(section),
                icon: section.icon.clone(),
                inactive: inactive(&rows, section.kind),
                conversations: rows,
            }
        })
        .collect();
    if !stray.is_empty() {
        // Sections came without a catch-all: keep everything reachable.
        order(&mut stray, sort);
        shown.push(Shown {
            id: None,
            kind: SectionKind::Channels,
            title: crate::i18n::t("Other").into_owned(),
            icon: None,
            inactive: inactive(&stray, SectionKind::Channels),
            conversations: stray,
        });
    }
    shown
}

/// Orders one section's rows: by `sort`, then, when unread come first, by
/// rank. Both sorts are stable, so each rank keeps the `sort` order.
fn order(
    rows: &mut [&Conversation],
    sort: Sort,
    titled: &impl Fn(&Conversation) -> String,
    rank: &impl Fn(&Conversation) -> Rank,
    arrange: &Arrange<'_>,
) {
    match sort {
        Sort::Name => rows.sort_by_cached_key(|c| titled(c).to_lowercase()),
        Sort::Recent => rows.sort_by(|a, b| b.latest.cmp(&a.latest)),
    }
    if arrange.unread_first {
        rows.sort_by_cached_key(|c| placed_rank(c, rank, arrange.hold));
    }
}

/// The rank a conversation is placed by: the held one for the open
/// conversation, its own for the rest.
fn placed_rank(
    conversation: &Conversation,
    rank: &impl Fn(&Conversation) -> Rank,
    hold: Option<&Hold>,
) -> Rank {
    match hold {
        Some(held) if held.id == conversation.id => held.rank,
        _ => rank(conversation),
    }
}

/// Everything [`layout`] reads, folded into one number, so a frame can tell
/// cheaply whether the sidebar's shape changed. That is a walk over the
/// conversations without sorting or allocating, where [`layout`] sorts by
/// lower-cased titles and builds maps. It covers what `titled` may read: a
/// conversation's name and, for a direct message, the person's label; and
/// what `rank` answers, which is cheap to ask; and for hiding quiet
/// conversations, the setting, the time step and who has a draft.
pub fn fingerprint(
    sections: Option<&[SidebarSection]>,
    conversations: &[Conversation],
    users: &HashMap<String, User>,
    rank: impl Fn(&Conversation) -> Rank,
    arrange: &Arrange<'_>,
) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    std::mem::discriminant(&arrange.sort).hash(&mut hasher);
    arrange.unread_first.hash(&mut hasher);
    arrange.hold.map(|h| (&h.id, h.rank)).hash(&mut hasher);
    // The cutoff, not the time: it moves once a step, and not at all
    // with hiding off.
    let cutoff = arrange.tidy.and_then(|tidy| tidy.cutoff());
    cutoff.hash(&mut hasher);
    let drafts = arrange.tidy.and_then(|tidy| tidy.drafts);
    // Section titles are translated.
    std::mem::discriminant(&crate::i18n::locale()).hash(&mut hasher);
    match sections {
        Some(sections) => {
            sections.len().hash(&mut hasher);
            for section in sections {
                section.id.hash(&mut hasher);
                std::mem::discriminant(&section.kind).hash(&mut hasher);
                section.name.hash(&mut hasher);
                section.emoji.hash(&mut hasher);
                section.channel_ids.hash(&mut hasher);
            }
        }
        None => usize::MAX.hash(&mut hasher),
    }
    conversations.len().hash(&mut hasher);
    for conversation in conversations {
        conversation.id.hash(&mut hasher);
        conversation.name.hash(&mut hasher);
        conversation.kind.hash(&mut hasher);
        conversation.latest.hash(&mut hasher);
        conversation.empty.hash(&mut hasher);
        conversation.is_open.hash(&mut hasher);
        conversation.user.hash(&mut hasher);
        // Hashed even with unread-first off, so the ranks the memo keeps
        // for holding are never stale when it is turned on.
        rank(conversation).hash(&mut hasher);
        conversation.mentions.hash(&mut hasher);
        if cutoff.is_some() {
            drafts
                .is_some_and(|drafts| drafts.contains(&conversation.id))
                .hash(&mut hasher);
        }
        if let Some(user) = conversation.user.as_deref().and_then(|id| users.get(id)) {
            user.label().hash(&mut hasher);
            user.is_bot.hash(&mut hasher);
        }
    }
    hasher.finish()
}

/// One section of a remembered layout, its rows as indices into the
/// conversations it was made from.
#[derive(Clone, Debug)]
struct Placed {
    id: Option<String>,
    kind: SectionKind,
    title: String,
    icon: Option<String>,
    rows: Vec<usize>,
    inactive: Vec<bool>,
}

/// What a layout was made for, apart from the data the revision stands
/// for: all cheap to compare, so an unchanged frame costs no walk over
/// the conversations.
#[derive(Clone, Debug, PartialEq)]
struct Stamp {
    revision: u64,
    sort: Sort,
    unread_first: bool,
    hold: Option<Hold>,
    cutoff: Option<i64>,
    locale: crate::i18n::Locale,
    /// Who has a draft, which counts only while quiet ones are hidden.
    drafts: Option<HashSet<String>>,
}

impl Stamp {
    /// The drafts `arrange` hides by, when it hides any.
    fn drafts<'a>(arrange: &Arrange<'a>, cutoff: Option<i64>) -> Option<&'a HashSet<String>> {
        cutoff.and(arrange.tidy.and_then(|tidy| tidy.drafts))
    }

    fn of(revision: u64, arrange: &Arrange<'_>) -> Self {
        let cutoff = arrange.tidy.and_then(|tidy| tidy.cutoff());
        Self {
            revision,
            sort: arrange.sort,
            unread_first: arrange.unread_first,
            hold: arrange.hold.cloned(),
            cutoff,
            locale: crate::i18n::locale(),
            drafts: Self::drafts(arrange, cutoff).cloned(),
        }
    }

    /// Whether this is the stamp of `arrange` at `revision`, without
    /// building one.
    fn is(&self, revision: u64, arrange: &Arrange<'_>) -> bool {
        let cutoff = arrange.tidy.and_then(|tidy| tidy.cutoff());
        self.revision == revision
            && self.sort == arrange.sort
            && self.unread_first == arrange.unread_first
            && self.hold.as_ref() == arrange.hold
            && self.cutoff == cutoff
            && self.locale == crate::i18n::locale()
            && self.drafts.as_ref() == Self::drafts(arrange, cutoff)
    }
}

/// [`layout`], remembered until its [`fingerprint`] changes: the sidebar
/// is drawn every frame, but its shape changes only when a conversation,
/// a section or a name does. Given a [`Arrange::revision`], the
/// fingerprint itself is taken only once that or the arrangement moves.
/// It also keeps the open conversation's [`Hold`].
#[derive(Clone, Debug, Default)]
pub struct Memo {
    key: Option<u64>,
    /// What the fingerprint was last taken for, when given a revision.
    stamp: Option<Stamp>,
    placed: std::sync::Arc<[Placed]>,
    /// What the open conversation holds (see [`Memo::hold`]).
    held: Option<Hold>,
    /// Each conversation's rank when the layout was last made, leaving
    /// out the read ones: what a conversation holds once it opens.
    seen: std::sync::Arc<HashMap<String, Rank>>,
}

impl Memo {
    /// Updates and returns the open conversation's [`Hold`], given which
    /// one is open now. Call it before [`Memo::layout`] each frame, and
    /// pass what it returns in the [`Arrange`]: a conversation that just
    /// opened holds the rank the last layout gave it, from before opening
    /// it marked it read.
    pub fn hold(
        &mut self,
        active: Option<&str>,
        conversations: &[Conversation],
        rank: impl Fn(&Conversation) -> Rank,
    ) -> Option<Hold> {
        let laid_out = self.key.is_some();
        let remembered = &self.seen;
        let seen = |id: &str| {
            if laid_out {
                remembered.get(id).copied().unwrap_or_default()
            } else {
                // Nothing shown yet (just started): it is as it is.
                conversations
                    .iter()
                    .find(|c| c.id == id)
                    .map(&rank)
                    .unwrap_or_default()
            }
        };
        let held = hold(self.held.as_ref(), active, seen);
        self.held.clone_from(&held);
        held
    }

    /// The open conversation's [`Hold`] as the sidebar last kept it, for
    /// stepping through the sidebar in the order it shows.
    pub fn held(&self) -> Option<&Hold> {
        self.held.as_ref()
    }

    /// The same as [`layout`] with these arguments; `titled` and `rank`
    /// must read only what [`fingerprint`] covers.
    pub fn layout<'a>(
        &mut self,
        sections: Option<&[SidebarSection]>,
        conversations: &'a [Conversation],
        users: &HashMap<String, User>,
        titled: impl Fn(&Conversation) -> String,
        rank: impl Fn(&Conversation) -> Rank,
        arrange: &Arrange<'_>,
    ) -> Vec<Shown<'a>> {
        let unchanged = arrange.revision.is_some_and(|revision| {
            self.stamp
                .as_ref()
                .is_some_and(|stamp| stamp.is(revision, arrange))
        });
        if !unchanged {
            self.relayout(sections, conversations, users, titled, rank, arrange);
            self.stamp = arrange
                .revision
                .map(|revision| Stamp::of(revision, arrange));
        }
        self.placed
            .iter()
            .map(|placed| {
                let (conversations, inactive) = placed
                    .rows
                    .iter()
                    .zip(&placed.inactive)
                    .filter_map(|(&i, &quiet)| conversations.get(i).map(|c| (c, quiet)))
                    .unzip();
                Shown {
                    id: placed.id.clone(),
                    kind: placed.kind,
                    title: placed.title.clone(),
                    icon: placed.icon.clone(),
                    conversations,
                    inactive,
                }
            })
            .collect()
    }

    /// Lays the sidebar out again if its [`fingerprint`] moved.
    fn relayout(
        &mut self,
        sections: Option<&[SidebarSection]>,
        conversations: &[Conversation],
        users: &HashMap<String, User>,
        titled: impl Fn(&Conversation) -> String,
        rank: impl Fn(&Conversation) -> Rank,
        arrange: &Arrange<'_>,
    ) {
        let key = fingerprint(sections, conversations, users, &rank, arrange);
        if self.key != Some(key) {
            self.seen = std::sync::Arc::new(
                conversations
                    .iter()
                    .map(|c| (c, rank(c)))
                    .filter(|(_, rank)| *rank != Rank::Read)
                    .map(|(c, rank)| (c.id.clone(), rank))
                    .collect(),
            );
            let index: HashMap<&str, usize> = conversations
                .iter()
                .enumerate()
                .map(|(i, c)| (c.id.as_str(), i))
                .collect();
            self.placed = layout(sections, conversations, users, titled, &rank, arrange)
                .into_iter()
                .map(|shown| {
                    let (rows, inactive) = shown
                        .conversations
                        .iter()
                        .zip(shown.inactive)
                        .filter_map(|(c, quiet)| index.get(c.id.as_str()).map(|&i| (i, quiet)))
                        .unzip();
                    Placed {
                        id: shown.id,
                        kind: shown.kind,
                        title: shown.title,
                        icon: shown.icon,
                        rows,
                        inactive,
                    }
                })
                .collect();
            self.key = Some(key);
        }
    }
}

/// Applies an edit to the local copy, before Slack confirms it.
pub fn apply(sections: &mut Vec<SidebarSection>, edit: &SidebarEdit) {
    match edit {
        SidebarEdit::Create { name, channel } => {
            // Slack puts new sections first; the real id arrives with the
            // next fetch.
            let mut section = SidebarSection {
                id: local_id(),
                kind: SectionKind::Custom,
                name: name.clone(),
                emoji: String::new(),
                channel_ids: Vec::new(),
                icon: None,
            };
            if let Some(channel) = channel {
                remove_everywhere(sections, channel, false);
                section.channel_ids.push(channel.clone());
            }
            let at = sections
                .iter()
                .position(|s| s.kind != SectionKind::Starred)
                .unwrap_or(sections.len());
            sections.insert(at, section);
        }
        SidebarEdit::Rename { section, name } => {
            if let Some(s) = sections.iter_mut().find(|s| s.id == *section) {
                s.name = name.clone();
            }
        }
        SidebarEdit::Delete { section } => sections.retain(|s| s.id != *section),
        SidebarEdit::Shift { section, up } => {
            // The same move as the Slack call `plan` makes, so the local
            // order matches what the next fetch brings.
            if let Some(at) = sections.iter().position(|s| s.id == *section)
                && let Some((moved, before)) = shift(sections, at, *up)
            {
                let section = sections.remove(moved);
                let before = if moved < before { before - 1 } else { before };
                sections.insert(before, section);
            }
        }
        SidebarEdit::Move { channel, to, .. } => {
            let Some(target) = sections.iter().position(|s| s.id == *to) else {
                return;
            };
            let catch_all = matches!(
                sections[target].kind,
                SectionKind::Channels | SectionKind::DirectMessages
            );
            remove_everywhere(sections, channel, false);
            if !catch_all {
                sections[target].channel_ids.push(channel.clone());
            }
        }
        SidebarEdit::Star { channel, starred } => {
            if *starred {
                // Slack makes the Starred section with the first star;
                // until the next fetch brings it, show a local one so the
                // star appears at once, as it will in Slack.
                let stars = match sections.iter().position(|s| s.kind == SectionKind::Starred) {
                    Some(at) => at,
                    None => {
                        sections.insert(
                            0,
                            SidebarSection {
                                id: local_id(),
                                kind: SectionKind::Starred,
                                name: String::new(),
                                emoji: String::new(),
                                channel_ids: Vec::new(),
                                icon: None,
                            },
                        );
                        0
                    }
                };
                let stars = &mut sections[stars];
                if !stars.channel_ids.contains(channel) {
                    stars.channel_ids.push(channel.clone());
                }
            } else {
                remove_everywhere(sections, channel, true);
            }
        }
    }
}

/// Takes a conversation out of the sections that name it: every one, or
/// only Starred.
fn remove_everywhere(sections: &mut [SidebarSection], channel: &str, starred_only: bool) {
    for section in sections {
        if !starred_only || section.kind == SectionKind::Starred {
            section.channel_ids.retain(|id| id != channel);
        }
    }
}

/// Whether a conversation is starred.
pub fn is_starred(sections: Option<&[SidebarSection]>, channel: &str) -> bool {
    sections.is_some_and(|sections| {
        sections
            .iter()
            .any(|s| s.kind == SectionKind::Starred && s.channel_ids.iter().any(|id| id == channel))
    })
}

/// The sections a conversation can be moved into by hand.
pub fn targets(sections: &[SidebarSection], from: Option<&str>) -> Vec<(String, String)> {
    let mut seen = HashSet::new();
    sections
        .iter()
        .filter(|s| {
            matches!(
                s.kind,
                SectionKind::Custom | SectionKind::Channels | SectionKind::DirectMessages
            )
        })
        .filter(|s| Some(s.id.as_str()) != from)
        .filter(|s| seen.insert(s.id.clone()))
        .map(|s| (s.id.clone(), title(s)))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_teams_chat_section_is_called_chat() {
        let section = |id: &str| SidebarSection {
            id: id.to_owned(),
            kind: SectionKind::DirectMessages,
            name: String::new(),
            emoji: String::new(),
            channel_ids: Vec::new(),
            icon: None,
        };
        assert_eq!(title(&section(crate::model::TEAMS_CHAT_SECTION)), "Chat");
        assert_eq!(title(&section("L04")), "Direct messages");
    }
    use crate::model::{ConversationKind, Ts};

    fn conversation(id: &str, name: &str, kind: ConversationKind, latest: &str) -> Conversation {
        Conversation {
            id: id.into(),
            name: name.into(),
            kind,
            user: (kind == ConversationKind::Direct).then(|| format!("U{id}")),
            topic: String::new(),
            purpose: String::new(),
            members: None,
            archived: false,
            last_read: None,
            latest: Some(Ts::new(latest)),
            unread: 0,
            mentions: 0,
            external: false,
            is_open: None,
            empty: false,
        }
    }

    #[test]
    fn closed_conversations_come_back_with_something_new() {
        let closed: std::collections::BTreeMap<String, String> =
            [("D1".to_owned(), "20.0".to_owned())].into();
        let dm = |latest: &str| conversation("D1", "ana", ConversationKind::Direct, latest);
        assert!(is_closed(Some(&closed), &dm("20.0")));
        assert!(is_closed(Some(&closed), &dm("9.0")));
        assert!(!is_closed(Some(&closed), &dm("21.0")));
        assert!(!is_closed(None, &dm("9.0")));
        let other = conversation("D2", "bob", ConversationKind::Direct, "1.0");
        assert!(!is_closed(Some(&closed), &other));
    }

    fn section(id: &str, kind: SectionKind, name: &str, ids: &[&str]) -> SidebarSection {
        SidebarSection {
            id: id.into(),
            kind,
            name: name.into(),
            emoji: String::new(),
            channel_ids: ids.iter().map(|s| (*s).to_owned()).collect(),
            icon: None,
        }
    }

    fn sample() -> (
        Vec<SidebarSection>,
        Vec<Conversation>,
        HashMap<String, User>,
    ) {
        let sections = vec![
            section("S1", SectionKind::Starred, "", &["C3"]),
            section("S2", SectionKind::Custom, "Team", &["C2", "C3", "D2"]),
            section("S3", SectionKind::Channels, "", &[]),
            section("S4", SectionKind::DirectMessages, "", &[]),
            section("S5", SectionKind::Apps, "", &[]),
        ];
        let conversations = vec![
            conversation("C1", "zeta", ConversationKind::Channel, "1.0"),
            conversation("C2", "beta", ConversationKind::Channel, "5.0"),
            conversation("C3", "alpha", ConversationKind::Channel, "2.0"),
            conversation("C4", "gamma", ConversationKind::Private, "9.0"),
            conversation("D1", "ann", ConversationKind::Direct, "3.0"),
            conversation("D2", "zoe", ConversationKind::Direct, "4.0"),
            conversation("D3", "deploy bot", ConversationKind::Direct, "8.0"),
        ];
        let users = HashMap::from([(
            "UD3".to_owned(),
            User {
                id: "UD3".into(),
                is_bot: true,
                ..User::default()
            },
        )]);
        (sections, conversations, users)
    }

    fn ids<'a>(shown: &Shown<'a>) -> Vec<&'a str> {
        shown.conversations.iter().map(|c| c.id.as_str()).collect()
    }

    fn all_ids<'a>(shown: &[Shown<'a>]) -> Vec<Vec<&'a str>> {
        shown.iter().map(ids).collect()
    }

    /// The rank with nothing muted.
    fn live(c: &Conversation) -> Rank {
        rank(c, c.has_unread())
    }

    /// The rank with C4 and C5 muted, as `WorkspaceState::is_unread` has
    /// it: a muted one counts only for its mentions.
    fn muted(c: &Conversation) -> Rank {
        let muted = c.id == "C4" || c.id == "C5";
        rank(c, c.has_unread() && (c.mentions > 0 || !muted))
    }

    /// A conversation read up to `read`, with `mentions` of you.
    fn read_to(
        id: &str,
        name: &str,
        kind: ConversationKind,
        latest: &str,
        read: &str,
        mentions: u32,
    ) -> Conversation {
        Conversation {
            last_read: Some(Ts::new(read)),
            mentions,
            ..conversation(id, name, kind, latest)
        }
    }

    /// Read, unread, mentioned and muted channels, with direct messages,
    /// in a Starred section, one of your own and the catch-alls.
    fn unread_sample() -> (Vec<SidebarSection>, Vec<Conversation>) {
        use ConversationKind::{Channel, Direct};
        let sections = vec![
            section("S1", SectionKind::Starred, "", &["C7", "C8"]),
            section("S2", SectionKind::Custom, "Team", &["C9", "C10"]),
            section("S3", SectionKind::Channels, "", &[]),
            section("S4", SectionKind::DirectMessages, "", &[]),
        ];
        let conversations = vec![
            read_to("C1", "alpha", Channel, "1.0", "1.0", 0),
            read_to("C2", "beta", Channel, "5.0", "1.0", 0),
            read_to("C3", "gamma", Channel, "2.0", "1.0", 1),
            // Muted and unread: stays with the read ones.
            read_to("C4", "delta", Channel, "9.0", "1.0", 0),
            // Muted, but it mentions you: that still counts.
            read_to("C5", "epsilon", Channel, "4.0", "1.0", 1),
            read_to("C6", "zeta", Channel, "3.0", "1.0", 0),
            read_to("C7", "ant", Channel, "1.0", "1.0", 0),
            read_to("C8", "bee", Channel, "2.0", "1.0", 0),
            read_to("C9", "cat", Channel, "1.0", "1.0", 0),
            read_to("C10", "dog", Channel, "2.0", "1.0", 1),
            read_to("D1", "ann", Direct, "9.0", "9.0", 0),
            read_to("D2", "bob", Direct, "3.0", "1.0", 0),
            read_to("D3", "cy", Direct, "5.0", "1.0", 0),
        ];
        (sections, conversations)
    }

    fn arranged(
        sections: &[SidebarSection],
        conversations: &[Conversation],
        arrange: &Arrange<'_>,
    ) -> Vec<Vec<String>> {
        let shown = layout(
            Some(sections),
            conversations,
            &HashMap::new(),
            |c| c.name.clone(),
            muted,
            arrange,
        );
        shown
            .iter()
            .map(|s| s.conversations.iter().map(|c| c.id.clone()).collect())
            .collect()
    }

    fn first(sort: Sort) -> Arrange<'static> {
        Arrange {
            sort,
            unread_first: true,
            hold: None,
            tidy: None,
            revision: None,
        }
    }

    #[test]
    fn ranks_follow_mentions_direct_messages_and_mutes() {
        let (_, conversations) = unread_sample();
        let ranks: Vec<(&str, Rank)> = conversations
            .iter()
            .map(|c| (c.id.as_str(), muted(c)))
            .collect();
        let of = |id: &str| ranks.iter().find(|(c, _)| *c == id).map(|(_, r)| *r);
        assert_eq!(of("C1"), Some(Rank::Read));
        assert_eq!(of("C2"), Some(Rank::Unread));
        assert_eq!(of("C3"), Some(Rank::Urgent), "a mention");
        assert_eq!(of("C4"), Some(Rank::Read), "muted");
        assert_eq!(of("C5"), Some(Rank::Urgent), "muted, but a mention");
        assert_eq!(of("D1"), Some(Rank::Read));
        assert_eq!(of("D2"), Some(Rank::Urgent), "an unread direct message");
    }

    #[test]
    fn unread_come_first_in_every_section_by_name() {
        let (sections, conversations) = unread_sample();
        let shown = arranged(&sections, &conversations, &first(Sort::Name));
        assert_eq!(shown[0], ["C8", "C7"], "Starred too");
        assert_eq!(shown[1], ["C10", "C9"], "your own sections too");
        assert_eq!(
            shown[2],
            ["C5", "C3", "C2", "C6", "C1", "C4"],
            "mentions, then unread, then the rest, each by name"
        );
        assert_eq!(shown[3], ["D3", "D2", "D1"], "direct messages by activity");
    }

    #[test]
    fn unread_come_first_by_recent_activity() {
        let (sections, conversations) = unread_sample();
        let shown = arranged(&sections, &conversations, &first(Sort::Recent));
        assert_eq!(shown[0], ["C8", "C7"]);
        assert_eq!(shown[2], ["C5", "C3", "C2", "C6", "C4", "C1"]);
        assert_eq!(shown[3], ["D3", "D2", "D1"]);
    }

    #[test]
    fn unread_direct_messages_come_before_newer_read_ones() {
        let (sections, mut conversations) = unread_sample();
        // Ann's is the newest, but read; Bob's is older and unread.
        if let Some(d3) = conversations.iter_mut().find(|c| c.id == "D3") {
            d3.last_read = d3.latest.clone();
        }
        let shown = arranged(&sections, &conversations, &first(Sort::Recent));
        assert_eq!(shown[3], ["D2", "D1", "D3"]);
        let plain = arranged(&sections, &conversations, &Arrange::plain(Sort::Recent));
        assert_eq!(plain[3], ["D1", "D3", "D2"]);
    }

    #[test]
    fn with_the_setting_off_the_order_is_as_before() {
        let (sections, conversations) = unread_sample();
        let held = Hold {
            id: "C1".into(),
            rank: Rank::Urgent,
        };
        for sort in [Sort::Name, Sort::Recent] {
            let off = Arrange {
                sort,
                unread_first: false,
                hold: Some(&held),
                tidy: None,
                revision: None,
            };
            assert_eq!(
                arranged(&sections, &conversations, &off),
                arranged(&sections, &conversations, &Arrange::plain(sort)),
            );
        }
        let by_name = arranged(&sections, &conversations, &Arrange::plain(Sort::Name));
        assert_eq!(by_name[2], ["C1", "C2", "C4", "C5", "C3", "C6"]);
        let by_activity = arranged(&sections, &conversations, &Arrange::plain(Sort::Recent));
        assert_eq!(by_activity[2], ["C4", "C2", "C5", "C6", "C3", "C1"]);
    }

    #[test]
    fn the_open_one_keeps_what_it_held_until_another_opens() {
        let seen = |id: &str| if id == "C2" { Rank::Unread } else { Rank::Read };
        let opened = hold(None, Some("C2"), seen);
        assert_eq!(
            opened,
            Some(Hold {
                id: "C2".into(),
                rank: Rank::Unread
            })
        );
        // Read now, but still open: it keeps its rank.
        let still = hold(opened.as_ref(), Some("C2"), |_| Rank::Read);
        assert_eq!(still, opened);
        let other = hold(still.as_ref(), Some("C1"), seen);
        assert_eq!(
            other.map(|h| (h.id, h.rank)),
            Some(("C1".into(), Rank::Read))
        );
        assert_eq!(hold(opened.as_ref(), None, seen), None);
    }

    #[test]
    fn the_open_conversation_stays_put_until_you_switch() {
        let (sections, mut conversations) = unread_sample();
        let users = HashMap::new();
        let mut memo = Memo::default();
        let frame = |memo: &mut Memo, conversations: &[Conversation], active: Option<&str>| {
            let held = memo.hold(active, conversations, muted);
            let shown = memo.layout(
                Some(&sections),
                conversations,
                &users,
                |c| c.name.clone(),
                muted,
                &Arrange {
                    sort: Sort::Name,
                    unread_first: true,
                    hold: held.as_ref(),
                    tidy: None,
                    revision: None,
                },
            );
            shown
                .iter()
                .map(|s| s.conversations.iter().map(|c| c.id.clone()).collect())
                .collect::<Vec<Vec<String>>>()
        };
        let before = frame(&mut memo, &conversations, None);
        assert_eq!(before[2], ["C5", "C3", "C2", "C6", "C1", "C4"]);
        // Opening beta reads it at once, before the sidebar draws again.
        if let Some(beta) = conversations.iter_mut().find(|c| c.id == "C2") {
            beta.last_read = beta.latest.clone();
        }
        let open = frame(&mut memo, &conversations, Some("C2"));
        assert_eq!(open[2], before[2], "beta stays where it was");
        // Something new elsewhere does not shake it loose.
        if let Some(alpha) = conversations.iter_mut().find(|c| c.id == "C1") {
            alpha.latest = Some(Ts::new("8.0"));
        }
        let open = frame(&mut memo, &conversations, Some("C2"));
        assert_eq!(open[2], ["C5", "C3", "C1", "C2", "C6", "C4"]);
        // Opening alpha reads it; beta goes down with the read ones and
        // alpha, opened while unread, keeps its place.
        if let Some(alpha) = conversations.iter_mut().find(|c| c.id == "C1") {
            alpha.last_read = alpha.latest.clone();
        }
        let switched = frame(&mut memo, &conversations, Some("C1"));
        assert_eq!(switched[2], ["C5", "C3", "C1", "C6", "C2", "C4"]);
        // Closing it lets it go too.
        let closed = frame(&mut memo, &conversations, None);
        assert_eq!(closed[2], ["C5", "C3", "C6", "C1", "C2", "C4"]);
    }

    #[test]
    fn just_started_the_open_one_holds_its_own_rank() {
        let (_, conversations) = unread_sample();
        let mut memo = Memo::default();
        let held = memo.hold(Some("C3"), &conversations, muted);
        assert_eq!(held.map(|h| h.rank), Some(Rank::Urgent));
        assert_eq!(memo.held().map(|h| h.id.as_str()), Some("C3"));
    }

    #[test]
    fn the_memo_matches_a_fresh_layout_and_follows_changes() {
        let (sections, mut conversations, users) = sample();
        let mut memo = Memo::default();
        let titled = |c: &Conversation| c.name.clone();
        let fresh = |conversations: &[Conversation]| {
            let shown = layout(
                Some(&sections),
                conversations,
                &users,
                titled,
                live,
                &Arrange::plain(Sort::Name),
            );
            shown
                .iter()
                .map(|s| s.conversations.iter().map(|c| c.id.clone()).collect())
                .collect::<Vec<Vec<String>>>()
        };
        let remembered = memo.layout(
            Some(&sections),
            &conversations,
            &users,
            titled,
            live,
            &Arrange::plain(Sort::Name),
        );
        assert_eq!(all_ids(&remembered), fresh(&conversations));
        let key = memo.key;
        // A redraw with nothing changed keeps the remembered layout.
        memo.layout(
            Some(&sections),
            &conversations,
            &users,
            titled,
            live,
            &Arrange::plain(Sort::Name),
        );
        assert_eq!(memo.key, key);
        // A rename reorders; a new message reorders direct messages.
        conversations[0].name = "aardvark".into();
        conversations[4].latest = Some(Ts::new("99.0"));
        let remembered = memo.layout(
            Some(&sections),
            &conversations,
            &users,
            titled,
            live,
            &Arrange::plain(Sort::Name),
        );
        assert_ne!(memo.key, key);
        assert_eq!(all_ids(&remembered), fresh(&conversations));
        assert_eq!(ids(&remembered[2]), ["C1", "C4"]);
    }

    #[test]
    fn a_revision_spares_the_walk_until_it_or_the_arrangement_moves() {
        let (sections, conversations, users) = sample();
        let mut conversations = crate::revision::Revised::new(conversations);
        let mut memo = Memo::default();
        let walked = std::cell::Cell::new(0);
        let rank = |c: &Conversation| {
            walked.set(walked.get() + 1);
            live(c)
        };
        let titled = |c: &Conversation| c.name.clone();
        let arrange = |revision, sort| Arrange {
            revision: Some(revision),
            ..Arrange::plain(sort)
        };
        let mut frame = |conversations: &[Conversation], arrange: &Arrange<'_>| {
            walked.set(0);
            let shown = memo.layout(
                Some(&sections),
                conversations,
                &users,
                titled,
                rank,
                arrange,
            );
            let owned: Vec<Vec<String>> = all_ids(&shown)
                .into_iter()
                .map(|ids| ids.into_iter().map(str::to_owned).collect())
                .collect();
            (owned, walked.get())
        };
        let (first, walks) = frame(
            &conversations,
            &arrange(conversations.revision(), Sort::Name),
        );
        assert!(walks > 0);
        for _ in 0..3 {
            let (idle, walks) = frame(
                &conversations,
                &arrange(conversations.revision(), Sort::Name),
            );
            assert_eq!((&idle, walks), (&first, 0), "an idle frame walks nothing");
        }
        // Another arrangement walks again, at the same revision.
        let (_, walks) = frame(
            &conversations,
            &arrange(conversations.revision(), Sort::Recent),
        );
        assert!(walks > 0);
        // A change moves the revision, and the layout follows it.
        conversations[0].name = "aardvark".into();
        let (renamed, walks) = frame(
            &conversations,
            &arrange(conversations.revision(), Sort::Name),
        );
        assert!(walks > 0);
        assert_ne!(renamed, first);
        let fresh = layout(
            Some(&sections),
            &conversations,
            &users,
            titled,
            live,
            &Arrange::plain(Sort::Name),
        );
        assert_eq!(renamed, all_ids(&fresh));
        // While quiet ones are hidden, who has a draft counts too.
        let none = HashSet::new();
        let one = HashSet::from(["C1".to_owned()]);
        let revision = conversations.revision();
        let tidy = |drafts| Arrange {
            tidy: Some(Tidy {
                after: HideInactive::Month,
                now: 1_000_000_000,
                drafts: Some(drafts),
            }),
            ..arrange(revision, Sort::Name)
        };
        frame(&conversations, &tidy(&none));
        let (_, idle) = frame(&conversations, &tidy(&none));
        assert_eq!(idle, 0);
        let (_, walks) = frame(&conversations, &tidy(&one));
        assert!(walks > 0);
        // Without a revision, every frame walks, as before.
        let (_, walks) = frame(&conversations, &Arrange::plain(Sort::Name));
        let (_, again) = frame(&conversations, &Arrange::plain(Sort::Name));
        assert!(walks > 0 && again > 0);
    }

    #[test]
    fn the_fingerprint_sees_what_layout_reads() {
        let (mut sections, conversations, mut users) = sample();
        let key = |sections: &[SidebarSection], users: &HashMap<String, User>, sort| {
            fingerprint(
                Some(sections),
                &conversations,
                users,
                live,
                &Arrange::plain(sort),
            )
        };
        let before = key(&sections, &users, Sort::Name);
        assert_eq!(before, key(&sections, &users, Sort::Name));
        assert_ne!(before, key(&sections, &users, Sort::Recent));
        assert_ne!(
            before,
            fingerprint(
                None,
                &conversations,
                &users,
                live,
                &Arrange::plain(Sort::Name)
            )
        );
        sections[1].channel_ids.push("C1".into());
        let moved = key(&sections, &users, Sort::Name);
        assert_ne!(before, moved);
        if let Some(bot) = users.get_mut("UD3") {
            bot.display_name = "Deployer".into();
        }
        assert_ne!(moved, key(&sections, &users, Sort::Name));
        // What the unread-first order reads: the setting, the hold and
        // each conversation's rank.
        let arranged = |arrange: &Arrange<'_>| {
            fingerprint(Some(&sections), &conversations, &users, live, arrange)
        };
        let plain = arranged(&Arrange::plain(Sort::Name));
        let first = Arrange {
            sort: Sort::Name,
            unread_first: true,
            hold: None,
            tidy: None,
            revision: None,
        };
        assert_ne!(plain, arranged(&first));
        let held = Hold {
            id: "C1".into(),
            rank: Rank::Unread,
        };
        assert_ne!(
            arranged(&first),
            arranged(&Arrange {
                hold: Some(&held),
                ..first
            })
        );
        let mut read = conversations.clone();
        read[4].last_read = read[4].latest.clone();
        assert_ne!(
            plain,
            fingerprint(
                Some(&sections),
                &read,
                &users,
                live,
                &Arrange::plain(Sort::Name)
            )
        );
    }

    #[test]
    fn conversations_land_where_slack_puts_them() {
        let (sections, conversations, users) = sample();
        let shown = layout(
            Some(&sections),
            &conversations,
            &users,
            |c| c.name.clone(),
            live,
            &Arrange::plain(Sort::Name),
        );
        let kinds: Vec<SectionKind> = shown.iter().map(|s| s.kind).collect();
        assert_eq!(
            kinds,
            [
                SectionKind::Starred,
                SectionKind::Custom,
                SectionKind::Channels,
                SectionKind::DirectMessages,
                SectionKind::Apps
            ]
        );
        assert_eq!(
            ids(&shown[0]),
            ["C3"],
            "starred wins over the custom section"
        );
        assert_eq!(
            ids(&shown[1]),
            ["C2", "D2"],
            "a DM can live in your own section"
        );
        assert_eq!(
            ids(&shown[2]),
            ["C4", "C1"],
            "the rest of the channels, by name"
        );
        assert_eq!(ids(&shown[3]), ["D1"]);
        assert_eq!(ids(&shown[4]), ["D3"], "bot DMs go to Apps");
    }

    #[test]
    fn channels_can_sort_by_activity_and_dms_always_do() {
        let (sections, conversations, users) = sample();
        let shown = layout(
            Some(&sections),
            &conversations,
            &users,
            |c| c.name.clone(),
            live,
            &Arrange::plain(Sort::Recent),
        );
        assert_eq!(ids(&shown[2]), ["C4", "C1"]);
        let plain = layout(
            None,
            &conversations,
            &users,
            |c| c.name.clone(),
            live,
            &Arrange::plain(Sort::Recent),
        );
        assert_eq!(ids(&plain[0]), ["C4", "C2", "C3", "C1"]);
        assert_eq!(ids(&plain[1]), ["D3", "D2", "D1"]);
    }

    #[test]
    fn empty_starred_and_apps_are_hidden() {
        let sections = vec![
            section("S1", SectionKind::Starred, "", &[]),
            section("S2", SectionKind::Custom, "Empty", &[]),
            section("S3", SectionKind::Channels, "", &[]),
        ];
        let conversations = vec![conversation("C1", "a", ConversationKind::Channel, "1.0")];
        let shown = layout(
            Some(&sections),
            &conversations,
            &HashMap::new(),
            |c| c.name.clone(),
            live,
            &Arrange::plain(Sort::Name),
        );
        let titles: Vec<&str> = shown.iter().map(|s| s.title.as_str()).collect();
        assert_eq!(titles, ["Empty", "Channels"]);
    }

    #[test]
    fn moves_stars_and_section_edits_apply_locally() {
        let (mut sections, ..) = sample();
        apply(
            &mut sections,
            &SidebarEdit::Move {
                channel: "C2".into(),
                from: Some("S2".into()),
                to: "S3".into(),
            },
        );
        assert_eq!(
            sections[1].channel_ids,
            ["C3", "D2"],
            "back to the catch-all"
        );
        apply(
            &mut sections,
            &SidebarEdit::Move {
                channel: "C1".into(),
                from: Some("S3".into()),
                to: "S2".into(),
            },
        );
        assert!(sections[1].channel_ids.contains(&"C1".to_owned()));
        apply(
            &mut sections,
            &SidebarEdit::Star {
                channel: "C1".into(),
                starred: true,
            },
        );
        assert!(is_starred(Some(&sections), "C1"));
        apply(
            &mut sections,
            &SidebarEdit::Star {
                channel: "C1".into(),
                starred: false,
            },
        );
        assert!(!is_starred(Some(&sections), "C1"));
        assert!(
            sections[1].channel_ids.contains(&"C1".to_owned()),
            "unstarring keeps its section"
        );
        apply(
            &mut sections,
            &SidebarEdit::Create {
                name: "New".into(),
                channel: Some("C4".into()),
            },
        );
        assert_eq!(
            sections[1].name, "New",
            "new sections go first, after Starred"
        );
        assert_eq!(sections[1].channel_ids, ["C4"]);
        let new = sections[1].id.clone();
        apply(
            &mut sections,
            &SidebarEdit::Rename {
                section: new.clone(),
                name: "Renamed".into(),
            },
        );
        apply(
            &mut sections,
            &SidebarEdit::Shift {
                section: new.clone(),
                up: false,
            },
        );
        assert_eq!(sections[2].name, "Renamed");
        apply(&mut sections, &SidebarEdit::Delete { section: new });
        assert!(sections.iter().all(|s| s.name != "Renamed"));
    }

    #[test]
    fn edits_become_the_slack_calls_they_need() {
        let (sections, ..) = sample();
        // From your section back to Channels: just out of the section.
        assert_eq!(
            plan(
                &sections,
                &SidebarEdit::Move {
                    channel: "C2".into(),
                    from: Some("S2".into()),
                    to: "S3".into()
                }
            ),
            [SidebarCall::Channels {
                channel: "C2".into(),
                insert: None,
                remove: Some("S2".into())
            }]
        );
        // From Starred (C3 is also in Team) to Channels: unstar, leave Team.
        assert_eq!(
            plan(
                &sections,
                &SidebarEdit::Move {
                    channel: "C3".into(),
                    from: Some("S1".into()),
                    to: "S3".into()
                }
            ),
            [
                SidebarCall::Star {
                    channel: "C3".into(),
                    starred: false
                },
                SidebarCall::Channels {
                    channel: "C3".into(),
                    insert: None,
                    remove: Some("S2".into())
                },
            ]
        );
        // From Channels into your section: one insert.
        assert_eq!(
            plan(
                &sections,
                &SidebarEdit::Move {
                    channel: "C1".into(),
                    from: Some("S3".into()),
                    to: "S2".into()
                }
            ),
            [SidebarCall::Channels {
                channel: "C1".into(),
                insert: Some("S2".into()),
                remove: None
            }]
        );
        // Moving down is the next section moving up before it.
        assert_eq!(
            plan(
                &sections,
                &SidebarEdit::Shift {
                    section: "S2".into(),
                    up: false
                }
            ),
            [SidebarCall::Set {
                section: "S3".into(),
                name: None,
                next: Some("S2".into())
            }]
        );
        assert_eq!(
            plan(
                &sections,
                &SidebarEdit::Shift {
                    section: "S2".into(),
                    up: true
                }
            ),
            [SidebarCall::Set {
                section: "S2".into(),
                name: None,
                next: Some("S1".into())
            }]
        );
        assert!(
            plan(
                &sections,
                &SidebarEdit::Shift {
                    section: "S1".into(),
                    up: true
                }
            )
            .is_empty()
        );
        assert!(
            plan(
                &sections,
                &SidebarEdit::Shift {
                    section: "S5".into(),
                    up: false
                }
            )
            .is_empty()
        );
        // A new section taking D2 along leaves Team.
        assert_eq!(
            plan(
                &sections,
                &SidebarEdit::Create {
                    name: "X".into(),
                    channel: Some("D2".into())
                }
            ),
            [SidebarCall::Create {
                name: "X".into(),
                channel: Some("D2".into()),
                remove_from: Some("S2".into())
            }]
        );
    }

    fn create(sections: &mut Vec<SidebarSection>, name: &str) -> String {
        apply(
            sections,
            &SidebarEdit::Create {
                name: name.into(),
                channel: None,
            },
        );
        let made = sections.iter().find(|s| s.name == name).expect("created");
        made.id.clone()
    }

    #[test]
    fn local_sections_never_share_an_id() {
        let (mut sections, ..) = sample();
        let first = create(&mut sections, "A");
        apply(
            &mut sections,
            &SidebarEdit::Delete {
                section: first.clone(),
            },
        );
        let second = create(&mut sections, "B");
        let third = create(&mut sections, "C");
        assert!(is_local(&first) && is_local(&second) && is_local(&third));
        assert_ne!(first, second, "an id is not reused after a delete");
        assert_ne!(second, third);
    }

    #[test]
    fn local_section_ids_never_reach_slack() {
        let (mut sections, ..) = sample();
        apply(
            &mut sections,
            &SidebarEdit::Create {
                name: "New".into(),
                channel: Some("C1".into()),
            },
        );
        let local = sections[1].id.clone();
        for edit in [
            SidebarEdit::Rename {
                section: local.clone(),
                name: "Renamed".into(),
            },
            SidebarEdit::Delete {
                section: local.clone(),
            },
            // Up past Starred, and down past Team: either way one side of
            // the call is the local section.
            SidebarEdit::Shift {
                section: local.clone(),
                up: true,
            },
            SidebarEdit::Shift {
                section: "S2".into(),
                up: true,
            },
        ] {
            assert_eq!(plan(&sections, &edit), [], "{edit:?}");
        }
        // Into the local section: nothing to insert into on Slack's side.
        assert_eq!(
            plan(
                &sections,
                &SidebarEdit::Move {
                    channel: "C4".into(),
                    from: Some("S3".into()),
                    to: local.clone(),
                }
            ),
            []
        );
        // Out of it, back to Channels: Slack never had it there.
        assert_eq!(
            plan(
                &sections,
                &SidebarEdit::Move {
                    channel: "C1".into(),
                    from: Some(local.clone()),
                    to: "S3".into(),
                }
            ),
            []
        );
        // Out of it, into Team: only the insert.
        assert_eq!(
            plan(
                &sections,
                &SidebarEdit::Move {
                    channel: "C1".into(),
                    from: Some(local.clone()),
                    to: "S2".into(),
                }
            ),
            [SidebarCall::Channels {
                channel: "C1".into(),
                insert: Some("S2".into()),
                remove: None
            }]
        );
        // A new section taking C1 along has nothing to take it out of.
        assert_eq!(
            plan(
                &sections,
                &SidebarEdit::Create {
                    name: "Other".into(),
                    channel: Some("C1".into()),
                }
            ),
            [SidebarCall::Create {
                name: "Other".into(),
                channel: Some("C1".into()),
                remove_from: None
            }]
        );
    }

    #[test]
    fn shifting_steps_over_a_hidden_starred_section() {
        let mut sections = vec![
            section("S1", SectionKind::Custom, "One", &[]),
            section("S2", SectionKind::Starred, "", &[]),
            section("S3", SectionKind::Custom, "Three", &[]),
            section("S4", SectionKind::Channels, "", &[]),
        ];
        let up = SidebarEdit::Shift {
            section: "S3".into(),
            up: true,
        };
        assert_eq!(
            plan(&sections, &up),
            [SidebarCall::Set {
                section: "S3".into(),
                name: None,
                next: Some("S1".into())
            }]
        );
        apply(&mut sections, &up);
        let order: Vec<&str> = sections.iter().map(|s| s.id.as_str()).collect();
        assert_eq!(order, ["S3", "S1", "S2", "S4"], "as Slack will have it");

        let down = SidebarEdit::Shift {
            section: "S1".into(),
            up: false,
        };
        assert_eq!(
            plan(&sections, &down),
            [SidebarCall::Set {
                section: "S4".into(),
                name: None,
                next: Some("S1".into())
            }]
        );
        apply(&mut sections, &down);
        let order: Vec<&str> = sections.iter().map(|s| s.id.as_str()).collect();
        assert_eq!(order, ["S3", "S4", "S1", "S2"]);

        // Nothing shown above: no call and no change.
        let mut top = vec![
            section("S1", SectionKind::Starred, "", &[]),
            section("S2", SectionKind::Custom, "Two", &[]),
        ];
        let edit = SidebarEdit::Shift {
            section: "S2".into(),
            up: true,
        };
        assert_eq!(plan(&top, &edit), []);
        assert!(!can_shift(&top, "S2", true));
        assert!(!can_shift(&top, "S2", false));
        apply(&mut top, &edit);
        assert_eq!(top[1].id, "S2");
        assert!(can_shift(&sections, "S1", true));
        assert!(!can_shift(&sections, "S1", false), "only hidden S2 below");
        assert!(!can_shift(&sections, "S9", true));
    }

    #[test]
    fn starring_without_a_starred_section_shows_the_star_at_once() {
        let mut sections = vec![
            section("S1", SectionKind::Custom, "Team", &[]),
            section("S2", SectionKind::Channels, "", &[]),
        ];
        let star = SidebarEdit::Star {
            channel: "C1".into(),
            starred: true,
        };
        assert_eq!(
            plan(&sections, &star),
            [SidebarCall::Star {
                channel: "C1".into(),
                starred: true
            }]
        );
        apply(&mut sections, &star);
        assert!(is_starred(Some(&sections), "C1"));
        assert_eq!(sections[0].kind, SectionKind::Starred);
        assert!(is_local(&sections[0].id));
        // A second star goes into the same section.
        apply(
            &mut sections,
            &SidebarEdit::Star {
                channel: "C2".into(),
                starred: true,
            },
        );
        assert_eq!(sections.len(), 3);
        assert_eq!(sections[0].channel_ids, ["C1", "C2"]);
    }

    #[test]
    fn move_targets_skip_the_current_and_special_sections() {
        let (sections, ..) = sample();
        let targets: Vec<String> = targets(&sections, Some("S2"))
            .into_iter()
            .map(|(id, _)| id)
            .collect();
        assert_eq!(targets, ["S3", "S4"]);
    }

    /// A fixed "now" for hiding, on the hour: 2001-09-09 01:00 UTC.
    const NOW: i64 = 1_000_000_800;
    const DAY: i64 = 24 * 60 * 60;

    /// A read channel whose newest message is `days` old.
    fn aged(id: &str, days: i64) -> Conversation {
        let at = format!("{}.000100", NOW - days * DAY);
        read_to(id, id, ConversationKind::Channel, &at, &at, 0)
    }

    fn tidy(after: HideInactive) -> Tidy<'static> {
        Tidy {
            after,
            now: NOW,
            drafts: None,
        }
    }

    #[test]
    fn a_conversation_hides_once_quiet_past_the_cutoff() {
        let month = tidy(HideInactive::Month);
        let quiet = |c: &Conversation| is_inactive(c, SectionKind::Channels, live(c), None, &month);
        assert!(quiet(&aged("C1", 31)));
        assert!(!quiet(&aged("C2", 29)));
        let week = tidy(HideInactive::Week);
        assert!(is_inactive(
            &aged("C2", 8),
            SectionKind::Channels,
            Rank::Read,
            None,
            &week
        ));
        let quarter = tidy(HideInactive::ThreeMonths);
        assert!(!is_inactive(
            &aged("C1", 31),
            SectionKind::Channels,
            Rank::Read,
            None,
            &quarter
        ));
        assert!(is_inactive(
            &aged("C1", 91),
            SectionKind::DirectMessages,
            Rank::Read,
            None,
            &quarter
        ));
    }

    #[test]
    fn with_hiding_off_nothing_hides() {
        let off = tidy(HideInactive::Off);
        assert_eq!(off.cutoff(), None);
        let ancient = aged("C1", 10_000);
        assert!(!is_inactive(
            &ancient,
            SectionKind::Channels,
            Rank::Read,
            None,
            &off
        ));
        let (sections, _) = unread_sample();
        let conversations = [aged("C1", 400), aged("C9", 400)];
        let shown = layout(
            Some(&sections),
            &conversations,
            &HashMap::new(),
            |c| c.name.clone(),
            live,
            &Arrange {
                tidy: Some(off),
                ..Arrange::plain(Sort::Name)
            },
        );
        assert!(shown.iter().all(|s| s.inactive.iter().all(|quiet| !quiet)));
    }

    #[test]
    fn an_unknown_newest_message_is_never_hidden() {
        let month = tidy(HideInactive::Month);
        let mut unknown = aged("C1", 400);
        unknown.latest = None;
        assert!(!is_inactive(
            &unknown,
            SectionKind::Channels,
            Rank::Read,
            None,
            &month
        ));
        unknown.latest = Some(Ts::new("not a time"));
        assert!(!is_inactive(
            &unknown,
            SectionKind::Channels,
            Rank::Read,
            None,
            &month
        ));
    }

    #[test]
    fn unread_mentioned_open_and_drafted_ones_never_hide() {
        let drafts = HashSet::from(["C5".to_owned()]);
        let month = Tidy {
            drafts: Some(&drafts),
            ..tidy(HideInactive::Month)
        };
        let old = aged("C1", 400);
        let quiet = |c: &Conversation, kind, rank, hold| is_inactive(c, kind, rank, hold, &month);
        assert!(quiet(&old, SectionKind::Channels, Rank::Read, None));
        // Unread, as `WorkspaceState::is_unread` says (mutes included).
        assert!(!quiet(&old, SectionKind::Channels, Rank::Unread, None));
        assert!(!quiet(&old, SectionKind::Channels, Rank::Urgent, None));
        // A mention of you, even counted as read.
        let mentioned = Conversation {
            mentions: 1,
            ..old.clone()
        };
        assert!(!quiet(&mentioned, SectionKind::Channels, Rank::Read, None));
        // The open one, held in its place.
        let held = Hold {
            id: "C1".into(),
            rank: Rank::Read,
        };
        assert!(!quiet(&old, SectionKind::Channels, Rank::Read, Some(&held)));
        let other = Hold {
            id: "C2".into(),
            rank: Rank::Read,
        };
        assert!(quiet(&old, SectionKind::Channels, Rank::Read, Some(&other)));
        // One with a draft.
        assert!(!quiet(
            &aged("C5", 400),
            SectionKind::Channels,
            Rank::Read,
            None
        ));
        // Starred, and apps in their own section.
        assert!(!quiet(&old, SectionKind::Starred, Rank::Read, None));
        assert!(!quiet(&old, SectionKind::Apps, Rank::Read, None));
        // Every other section hides, your own included.
        for kind in [
            SectionKind::Custom,
            SectionKind::Channels,
            SectionKind::DirectMessages,
        ] {
            assert!(quiet(&old, kind, Rank::Read, None), "{kind:?}");
        }
    }

    #[test]
    fn known_empty_conversations_hide_like_quiet_ones() {
        let drafts = HashSet::from(["G5".to_owned()]);
        let month = Tidy {
            drafts: Some(&drafts),
            ..tidy(HideInactive::Month)
        };
        let blank = |id: &str, kind| Conversation {
            latest: None,
            last_read: None,
            empty: true,
            ..conversation(id, id, kind, "1.0")
        };
        let quiet = |c: &Conversation, kind, rank, hold| is_inactive(c, kind, rank, hold, &month);
        let group = blank("G1", ConversationKind::Group);
        // Group DMs, direct messages and channels alike.
        assert!(quiet(&group, SectionKind::DirectMessages, Rank::Read, None));
        assert!(quiet(
            &blank("D1", ConversationKind::Direct),
            SectionKind::DirectMessages,
            Rank::Read,
            None
        ));
        assert!(quiet(
            &blank("C1", ConversationKind::Channel),
            SectionKind::Channels,
            Rank::Read,
            None
        ));
        assert!(quiet(&group, SectionKind::Custom, Rank::Read, None));
        // Not known to be empty, only not known: never hidden.
        let unknown = Conversation {
            empty: false,
            ..group.clone()
        };
        assert!(!quiet(
            &unknown,
            SectionKind::DirectMessages,
            Rank::Read,
            None
        ));
        // The same exceptions as for quiet ones.
        assert!(!quiet(
            &group,
            SectionKind::DirectMessages,
            Rank::Unread,
            None
        ));
        assert!(!quiet(
            &group,
            SectionKind::DirectMessages,
            Rank::Urgent,
            None
        ));
        let mentioned = Conversation {
            mentions: 1,
            ..group.clone()
        };
        assert!(!quiet(
            &mentioned,
            SectionKind::DirectMessages,
            Rank::Read,
            None
        ));
        let held = Hold {
            id: "G1".into(),
            rank: Rank::Read,
        };
        assert!(!quiet(
            &group,
            SectionKind::DirectMessages,
            Rank::Read,
            Some(&held)
        ));
        assert!(!quiet(
            &blank("G5", ConversationKind::Group),
            SectionKind::DirectMessages,
            Rank::Read,
            None
        ));
        assert!(!quiet(&group, SectionKind::Starred, Rank::Read, None));
        assert!(!quiet(&group, SectionKind::Apps, Rank::Read, None));
        // With hiding off, nothing hides.
        let off = tidy(HideInactive::Off);
        assert!(!is_inactive(
            &group,
            SectionKind::DirectMessages,
            Rank::Read,
            None,
            &off
        ));
    }

    #[test]
    fn direct_messages_slack_closed_stay_out_until_something_brings_them_back() {
        let closed = |kind, is_open| Conversation {
            is_open,
            ..conversation("G1", "ana, bob", kind, "1.0")
        };
        let group = closed(ConversationKind::Group, Some(false));
        assert!(is_shut(&group, false, false));
        assert!(is_shut(
            &closed(ConversationKind::Direct, Some(false)),
            false,
            false
        ));
        // Open, or not said: shown.
        assert!(!is_shut(
            &closed(ConversationKind::Group, Some(true)),
            false,
            false
        ));
        assert!(!is_shut(
            &closed(ConversationKind::Group, None),
            false,
            false
        ));
        // Channels have no such state.
        assert!(!is_shut(
            &closed(ConversationKind::Channel, Some(false)),
            false,
            false
        ));
        // Something unread, a mention or a draft brings it back.
        assert!(!is_shut(&group, true, false));
        assert!(!is_shut(&group, false, true));
        let mentioned = Conversation {
            mentions: 1,
            ..group.clone()
        };
        assert!(!is_shut(&mentioned, false, false));
    }

    #[test]
    fn the_fingerprint_sees_open_and_empty() {
        let (sections, mut conversations, users) = sample();
        let key = |conversations: &[Conversation]| {
            fingerprint(
                Some(&sections),
                conversations,
                &users,
                live,
                &Arrange::plain(Sort::Name),
            )
        };
        let before = key(&conversations);
        conversations[0].empty = true;
        let emptied = key(&conversations);
        assert_ne!(before, emptied);
        conversations[0].is_open = Some(false);
        assert_ne!(emptied, key(&conversations));
    }

    #[test]
    fn layout_marks_quiet_ones_in_every_section_but_starred_and_apps() {
        let (sections, mut conversations, users) = sample();
        // Everything a year quiet and read.
        for c in &mut conversations {
            let at = Ts::new(format!("{}.000100", NOW - 365 * DAY));
            c.latest = Some(at.clone());
            c.last_read = Some(at);
        }
        // But one channel spoke yesterday.
        conversations[3].latest = Some(Ts::new(format!("{}.000100", NOW - DAY)));
        conversations[3].last_read = conversations[3].latest.clone();
        let shown = layout(
            Some(&sections),
            &conversations,
            &users,
            |c| c.name.clone(),
            live,
            &Arrange {
                tidy: Some(tidy(HideInactive::Month)),
                ..Arrange::plain(Sort::Name)
            },
        );
        let marks: Vec<(SectionKind, Vec<(&str, bool)>)> = shown
            .iter()
            .map(|s| {
                let rows = ids(s).into_iter().zip(s.inactive.iter().copied()).collect();
                (s.kind, rows)
            })
            .collect();
        assert_eq!(
            marks,
            [
                (SectionKind::Starred, vec![("C3", false)]),
                (SectionKind::Custom, vec![("C2", true), ("D2", true)]),
                (SectionKind::Channels, vec![("C4", false), ("C1", true)]),
                (SectionKind::DirectMessages, vec![("D1", true)]),
                (SectionKind::Apps, vec![("D3", false)]),
            ]
        );
    }

    #[test]
    fn the_cutoff_moves_by_the_hour() {
        let at = |now| {
            Tidy {
                now,
                ..tidy(HideInactive::Week)
            }
            .cutoff()
        };
        assert_eq!(at(NOW), Some(NOW - 7 * DAY));
        assert_eq!(at(NOW + TIME_STEP - 1), at(NOW), "within the hour");
        assert_eq!(at(NOW + TIME_STEP), Some(NOW + TIME_STEP - 7 * DAY));
    }

    #[test]
    fn the_fingerprint_sees_what_hiding_reads() {
        let (sections, conversations, users) = sample();
        let drafts = HashSet::from(["C1".to_owned()]);
        let key = |tidy: Option<Tidy<'_>>| {
            fingerprint(
                Some(&sections),
                &conversations,
                &users,
                live,
                &Arrange {
                    tidy,
                    ..Arrange::plain(Sort::Name)
                },
            )
        };
        let month = tidy(HideInactive::Month);
        let base = key(Some(month));
        assert_eq!(base, key(Some(month)));
        // The setting.
        assert_ne!(base, key(Some(tidy(HideInactive::Week))));
        assert_ne!(base, key(Some(tidy(HideInactive::Off))));
        // The time, but only by the hour.
        let later = |now| Some(Tidy { now, ..month });
        assert_eq!(base, key(later(NOW + 60)), "a minute later");
        assert_ne!(base, key(later(NOW + TIME_STEP)), "the next hour");
        // With hiding off, the clock does not matter.
        let off = tidy(HideInactive::Off);
        assert_eq!(
            key(Some(off)),
            key(Some(Tidy {
                now: NOW + 99 * DAY,
                ..off
            }))
        );
        // Who has a draft.
        let drafted = Tidy {
            drafts: Some(&drafts),
            ..month
        };
        assert_ne!(base, key(Some(drafted)));
    }

    #[test]
    fn the_memo_hides_again_after_the_hour_turns() {
        let (sections, _) = unread_sample();
        let users = HashMap::new();
        // Quiet for 30 days less half an hour: hidden from the next hour.
        let at = format!("{}.000100", NOW - 30 * DAY + 1800);
        let conversations = [read_to(
            "C1",
            "alpha",
            ConversationKind::Channel,
            &at,
            &at,
            0,
        )];
        let mut memo = Memo::default();
        let mut quiet = |now| {
            let shown = memo.layout(
                Some(&sections),
                &conversations,
                &users,
                |c| c.name.clone(),
                live,
                &Arrange {
                    tidy: Some(Tidy {
                        now,
                        ..tidy(HideInactive::Month)
                    }),
                    ..Arrange::plain(Sort::Name)
                },
            );
            shown[1].inactive.clone()
        };
        assert_eq!(quiet(NOW), [false]);
        assert_eq!(quiet(NOW + TIME_STEP), [true]);
    }
}
