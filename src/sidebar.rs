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
    pub conversations: Vec<&'a Conversation>,
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
        SectionKind::DirectMessages => crate::i18n::t("Direct messages").into_owned(),
        SectionKind::Apps => crate::i18n::t("Apps").into_owned(),
    }
}

/// The sections to draw, each with its conversations in order.
pub fn layout<'a>(
    sections: Option<&[SidebarSection]>,
    conversations: &'a [Conversation],
    users: &HashMap<String, User>,
    titled: impl Fn(&Conversation) -> String,
    sort: Sort,
) -> Vec<Shown<'a>> {
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
        order(&mut channels, sort, &titled);
        order(&mut direct, Sort::Recent, &titled);
        return vec![
            Shown {
                id: None,
                kind: SectionKind::Channels,
                title: crate::i18n::t("Channels").into_owned(),
                conversations: channels,
            },
            Shown {
                id: None,
                kind: SectionKind::DirectMessages,
                title: crate::i18n::t("Direct messages").into_owned(),
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
            order(&mut rows, sort, &titled);
            Shown {
                id: Some(section.id.clone()),
                kind: section.kind,
                title: title(section),
                conversations: rows,
            }
        })
        .collect();
    if !stray.is_empty() {
        // Sections came without a catch-all: keep everything reachable.
        order(&mut stray, sort, &titled);
        shown.push(Shown {
            id: None,
            kind: SectionKind::Channels,
            title: crate::i18n::t("Other").into_owned(),
            conversations: stray,
        });
    }
    shown
}

fn order(rows: &mut [&Conversation], sort: Sort, titled: &impl Fn(&Conversation) -> String) {
    match sort {
        Sort::Name => rows.sort_by_cached_key(|c| titled(c).to_lowercase()),
        Sort::Recent => rows.sort_by(|a, b| b.latest.cmp(&a.latest)),
    }
}

/// Everything [`layout`] reads, folded into one number, so a frame can tell
/// cheaply whether the sidebar's shape changed. That is a walk over the
/// conversations without sorting or allocating, where [`layout`] sorts by
/// lower-cased titles and builds maps. It covers what `titled` may read: a
/// conversation's name and, for a direct message, the person's label.
pub fn fingerprint(
    sections: Option<&[SidebarSection]>,
    conversations: &[Conversation],
    users: &HashMap<String, User>,
    sort: Sort,
) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    std::mem::discriminant(&sort).hash(&mut hasher);
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
        conversation.user.hash(&mut hasher);
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
    rows: Vec<usize>,
}

/// [`layout`], remembered until its [`fingerprint`] changes: the sidebar
/// is drawn every frame, but its shape changes only when a conversation,
/// a section or a name does.
#[derive(Clone, Debug, Default)]
pub struct Memo {
    key: Option<u64>,
    placed: std::sync::Arc<[Placed]>,
}

impl Memo {
    /// The same as [`layout`] with these arguments; `titled` must read only
    /// what [`fingerprint`] covers.
    pub fn layout<'a>(
        &mut self,
        sections: Option<&[SidebarSection]>,
        conversations: &'a [Conversation],
        users: &HashMap<String, User>,
        titled: impl Fn(&Conversation) -> String,
        sort: Sort,
    ) -> Vec<Shown<'a>> {
        let key = fingerprint(sections, conversations, users, sort);
        if self.key != Some(key) {
            let index: HashMap<&str, usize> = conversations
                .iter()
                .enumerate()
                .map(|(i, c)| (c.id.as_str(), i))
                .collect();
            self.placed = layout(sections, conversations, users, titled, sort)
                .into_iter()
                .map(|shown| Placed {
                    id: shown.id,
                    kind: shown.kind,
                    title: shown.title,
                    rows: shown
                        .conversations
                        .iter()
                        .filter_map(|c| index.get(c.id.as_str()).copied())
                        .collect(),
                })
                .collect();
            self.key = Some(key);
        }
        self.placed
            .iter()
            .map(|placed| Shown {
                id: placed.id.clone(),
                kind: placed.kind,
                title: placed.title.clone(),
                conversations: placed
                    .rows
                    .iter()
                    .filter_map(|&i| conversations.get(i))
                    .collect(),
            })
            .collect()
    }
}

/// The section a conversation is shown in, for "Move to".
pub fn section_of(shown: &[Shown<'_>], channel: &str) -> Option<String> {
    shown
        .iter()
        .find(|s| s.conversations.iter().any(|c| c.id == channel))
        .and_then(|s| s.id.clone())
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
        }
    }

    fn section(id: &str, kind: SectionKind, name: &str, ids: &[&str]) -> SidebarSection {
        SidebarSection {
            id: id.into(),
            kind,
            name: name.into(),
            emoji: String::new(),
            channel_ids: ids.iter().map(|s| (*s).to_owned()).collect(),
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

    #[test]
    fn the_memo_matches_a_fresh_layout_and_follows_changes() {
        let (sections, mut conversations, users) = sample();
        let mut memo = Memo::default();
        let titled = |c: &Conversation| c.name.clone();
        let fresh = |conversations: &[Conversation]| {
            let shown = layout(Some(&sections), conversations, &users, titled, Sort::Name);
            shown
                .iter()
                .map(|s| s.conversations.iter().map(|c| c.id.clone()).collect())
                .collect::<Vec<Vec<String>>>()
        };
        let remembered = memo.layout(Some(&sections), &conversations, &users, titled, Sort::Name);
        assert_eq!(all_ids(&remembered), fresh(&conversations));
        let key = memo.key;
        // A redraw with nothing changed keeps the remembered layout.
        memo.layout(Some(&sections), &conversations, &users, titled, Sort::Name);
        assert_eq!(memo.key, key);
        // A rename reorders; a new message reorders direct messages.
        conversations[0].name = "aardvark".into();
        conversations[4].latest = Some(Ts::new("99.0"));
        let remembered = memo.layout(Some(&sections), &conversations, &users, titled, Sort::Name);
        assert_ne!(memo.key, key);
        assert_eq!(all_ids(&remembered), fresh(&conversations));
        assert_eq!(ids(&remembered[2]), ["C1", "C4"]);
    }

    #[test]
    fn the_fingerprint_sees_what_layout_reads() {
        let (mut sections, conversations, mut users) = sample();
        let key = |sections: &[SidebarSection], users: &HashMap<String, User>, sort| {
            fingerprint(Some(sections), &conversations, users, sort)
        };
        let before = key(&sections, &users, Sort::Name);
        assert_eq!(before, key(&sections, &users, Sort::Name));
        assert_ne!(before, key(&sections, &users, Sort::Recent));
        assert_ne!(
            before,
            fingerprint(None, &conversations, &users, Sort::Name)
        );
        sections[1].channel_ids.push("C1".into());
        let moved = key(&sections, &users, Sort::Name);
        assert_ne!(before, moved);
        if let Some(bot) = users.get_mut("UD3") {
            bot.display_name = "Deployer".into();
        }
        assert_ne!(moved, key(&sections, &users, Sort::Name));
    }

    #[test]
    fn conversations_land_where_slack_puts_them() {
        let (sections, conversations, users) = sample();
        let shown = layout(
            Some(&sections),
            &conversations,
            &users,
            |c| c.name.clone(),
            Sort::Name,
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
            Sort::Recent,
        );
        assert_eq!(ids(&shown[2]), ["C4", "C1"]);
        let plain = layout(
            None,
            &conversations,
            &users,
            |c| c.name.clone(),
            Sort::Recent,
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
            Sort::Name,
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
}
