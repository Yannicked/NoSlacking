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

/// The Slack calls an edit needs, worked out from the sections before it.
pub fn plan(sections: &[SidebarSection], edit: &SidebarEdit) -> Vec<SidebarCall> {
    let kind = |id: &str| sections.iter().find(|s| s.id == id).map(|s| s.kind);
    // The custom section that names a conversation, if any.
    let custom_home = |channel: &str| {
        sections
            .iter()
            .find(|s| s.kind == SectionKind::Custom && s.channel_ids.iter().any(|id| id == channel))
            .map(|s| s.id.clone())
    };
    match edit {
        SidebarEdit::Create { name, channel } => vec![SidebarCall::Create {
            name: name.clone(),
            channel: channel.clone(),
            remove_from: channel.as_deref().and_then(custom_home),
        }],
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
            // Each call puts a section right before another; moving down is
            // moving the next one up.
            let (moved, before) = if *up {
                match at.checked_sub(1) {
                    Some(previous) => (at, previous),
                    None => return Vec::new(),
                }
            } else if at + 1 < sections.len() {
                (at + 1, at)
            } else {
                return Vec::new();
            };
            vec![SidebarCall::Set {
                section: sections[moved].id.clone(),
                name: None,
                next: Some(sections[before].id.clone()),
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
            let insert = (to_kind == Some(SectionKind::Custom)).then(|| to.clone());
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
                id: format!("local-{}", sections.len()),
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
            if let Some(at) = sections.iter().position(|s| s.id == *section) {
                let to = if *up { at.checked_sub(1) } else { Some(at + 1) };
                if let Some(to) = to.filter(|to| *to < sections.len()) {
                    sections.swap(at, to);
                }
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
                if let Some(stars) = sections.iter_mut().find(|s| s.kind == SectionKind::Starred)
                    && !stars.channel_ids.contains(channel)
                {
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
