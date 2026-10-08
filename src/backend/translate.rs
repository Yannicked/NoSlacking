//! What a real-time event from Slack means for the interface.

use serde::Deserialize;
use serde_json::Value;

use super::Event;
use crate::model::{Message, Ts};
use crate::slack::types;

/// What one Socket Mode event means here. Short-lived, so its size does
/// not matter.
#[derive(Debug)]
#[allow(clippy::large_enum_variant)]
pub(super) enum Translated {
    Event(Event),
    /// Fetch this conversation's details again.
    Refresh(String),
    /// The sidebar's sections changed in another Slack client.
    RefreshSections,
    /// Your notification preferences changed in another Slack client.
    RefreshPrefs,
    /// The custom emoji changed in a way not told: fetch them all again.
    RefreshEmoji,
}

/// What an `emoji_changed` event says changed, by its `subtype`. None for
/// a subtype Slack has not described or that lacks its fields: then the
/// whole list is fetched again, as Slack's docs ask.
fn emoji_change(event: &Value) -> Option<crate::emoji::EmojiChange> {
    use crate::emoji::EmojiChange;
    let text = |key: &str| {
        str_of(event, key)
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
    };
    match str_of(event, "subtype")? {
        "add" => Some(EmojiChange::Added {
            name: text("name")?,
            value: text("value")?,
        }),
        "remove" => {
            let names: Vec<String> = event
                .get("names")?
                .as_array()?
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect();
            (!names.is_empty()).then_some(EmojiChange::Removed(names))
        }
        "rename" => Some(EmojiChange::Renamed {
            old: text("old_name")?,
            new: text("new_name")?,
            value: text("value"),
        }),
        _ => None,
    }
}

pub(super) fn str_of<'a>(value: &'a Value, key: &str) -> Option<&'a str> {
    value.get(key).and_then(Value::as_str)
}

fn message_of(value: &Value) -> Option<Message> {
    types::Message::deserialize(value).ok()?.into_model()
}

/// Turns a real-time event into interface events: the events with fields
/// of their own as [`types::RtEvent`], the rest from their JSON.
pub(super) fn translate(team: &str, me: &str, event: &Value) -> Vec<Translated> {
    let mut out = Vec::new();
    // A huddle's message, besides the message itself.
    if let Some(huddle) = super::people::huddle_in_message(event) {
        out.push(Translated::Event(Event::People {
            team: team.to_owned(),
            event: huddle,
        }));
    }
    match types::RtEvent::deserialize(event) {
        Ok(types::RtEvent::Other) => out.extend(from_json(team, event)),
        Ok(typed) => out.extend(from_rt(team.to_owned(), me, typed)),
        Err(error) => log::debug!(
            "unreadable {} event: {error}",
            str_of(event, "type").unwrap_or("")
        ),
    }
    out
}

/// What an event read by its fields means here.
fn from_rt(team: String, me: &str, event: types::RtEvent) -> Option<Translated> {
    use types::RtEvent as Rt;
    let event_of = |event| Some(Translated::Event(event));
    match event {
        Rt::ReactionAdded(reaction) => reacted(team, reaction, true),
        Rt::ReactionRemoved(reaction) => reacted(team, reaction, false),
        // Slack also sends the message with the file as a tombstone
        // (`message_changed`); this says so for every copy at once.
        Rt::FileDeleted(deleted) => event_of(Event::FileGone {
            team,
            file: deleted.file_id.filter(|f| !f.is_empty())?,
        }),
        Rt::MemberJoinedChannel(joined) if joined.user.as_deref() == Some(me) => {
            Some(Translated::Refresh(joined.id()?.to_owned()))
        }
        Rt::MemberLeftChannel(left) if left.user.as_deref() == Some(me) => {
            event_of(Event::ConversationGone {
                team,
                channel: left.id()?.to_owned(),
            })
        }
        Rt::ChannelLeft(gone)
        | Rt::GroupLeft(gone)
        | Rt::ChannelDeleted(gone)
        | Rt::GroupDeleted(gone)
        | Rt::ChannelArchive(gone)
        | Rt::GroupArchive(gone) => event_of(Event::ConversationGone {
            team,
            channel: gone.id()?.to_owned(),
        }),
        // Only conversations you are in belong in the sidebar; a fresh
        // channel someone else made is not one of them.
        Rt::ChannelCreated(_) => None,
        Rt::ChannelRename(changed)
        | Rt::GroupRename(changed)
        | Rt::ChannelUnarchive(changed)
        | Rt::ImCreated(changed) => Some(Translated::Refresh(changed.id()?.to_owned())),
        // A direct message or group DM opened or closed in your sidebar,
        // perhaps in another client. Slack documents `im_open`/`im_close`
        // and, for group DMs, `group_open`; `group_close` is documented
        // for private channels, which have no open state, so the app
        // ignores it for them. `mpim_open`/`mpim_close` are not
        // documented and are read the same way, by their `channel`.
        Rt::ImOpen(opened) | Rt::MpimOpen(opened) | Rt::GroupOpen(opened) => {
            event_of(Event::Opened {
                team,
                channel: opened.id()?.to_owned(),
                open: true,
            })
        }
        Rt::ImClose(closed) | Rt::MpimClose(closed) | Rt::GroupClose(closed) => {
            event_of(Event::Opened {
                team,
                channel: closed.id()?.to_owned(),
                open: false,
            })
        }
        // Read on another device (or in another window): the read marker
        // moves, so unread counts here follow. The interface only ever moves
        // a marker forward, so an older mark arriving late changes nothing.
        Rt::ChannelMarked(read)
        | Rt::GroupMarked(read)
        | Rt::ImMarked(read)
        | Rt::MpimMarked(read) => event_of(Event::Read {
            team,
            channel: read.id()?.to_owned(),
            ts: Ts::new(read.ts?),
        }),
        Rt::UserChange(changed) | Rt::TeamJoin(changed) => event_of(Event::Users {
            team,
            users: vec![changed.user()?.into_model()],
        }),
        // Mutes, notification levels or keywords changed in another client.
        Rt::PrefChange(pref) => {
            super::desktop::is_notification_pref(pref.name.as_deref().unwrap_or(""))
                .then_some(Translated::RefreshPrefs)
        }
        // A thread followed or no longer followed, here or in another
        // client. Only a browser session's socket says so; wee-slack reads
        // the same events (`slack_workspace.py`).
        Rt::ThreadSubscribed(changed) => followed(team, changed.subscription, true),
        Rt::ThreadUnsubscribed(changed) => followed(team, changed.subscription, false),
        Rt::PresenceChange(_) | Rt::ManualPresenceChange(_) | Rt::UserTyping(_) => {
            let event = super::people::from_rt(&event)?;
            event_of(Event::People { team, event })
        }
        _ => None,
    }
}

/// A reaction `added` or taken off.
fn reacted(team: String, reaction: types::ReactionEvent, added: bool) -> Option<Translated> {
    let item = reaction.item?;
    Some(Translated::Event(Event::Reaction {
        team,
        channel: item.channel?,
        ts: Ts::new(item.ts?),
        name: reaction.reaction?,
        user: reaction.user?,
        added,
    }))
}

/// A thread followed (`follow`) or no longer followed.
fn followed(team: String, thread: types::SubscribedThread, follow: bool) -> Option<Translated> {
    if thread.kind.as_deref().is_some_and(|kind| kind != "thread") {
        return None;
    }
    Some(Translated::Event(Event::Views {
        team,
        event: crate::views::Event::Followed {
            channel: thread.channel?,
            thread: Ts::new(thread.thread_ts?),
            follow,
        },
    }))
}

/// What an event read from its JSON means here: messages, by their
/// subtype, and the events whose shapes vary.
fn from_json(team: &str, event: &Value) -> Vec<Translated> {
    let team = team.to_owned();
    let kind = str_of(event, "type").unwrap_or("");
    let channel = str_of(event, "channel").map(str::to_owned);
    let mut out = Vec::new();
    match kind {
        "message" => {
            let Some(channel) = channel else {
                return out;
            };
            match str_of(event, "subtype") {
                Some("message_changed" | "message_replied") => {
                    if let Some(message) = event.get("message").and_then(message_of) {
                        out.push(Translated::Event(Event::Message {
                            team,
                            channel,
                            message,
                            changed: true,
                        }));
                    }
                }
                Some("message_deleted") => {
                    if let Some(ts) = str_of(event, "deleted_ts") {
                        out.push(Translated::Event(Event::Deleted {
                            team,
                            channel,
                            ts: Ts::new(ts),
                        }));
                    }
                }
                Some("channel_name" | "group_name" | "channel_topic" | "channel_purpose") => {
                    if let Some(message) = message_of(event) {
                        out.push(Translated::Event(Event::Message {
                            team,
                            channel: channel.clone(),
                            message,
                            changed: false,
                        }));
                    }
                    out.push(Translated::Refresh(channel));
                }
                _ => {
                    if let Some(message) = message_of(event) {
                        out.push(Translated::Event(Event::Message {
                            team,
                            channel,
                            message,
                            changed: false,
                        }));
                    }
                }
            }
        }
        // Sections made, renamed, moved, deleted, or channels moved between
        // them, and stars, in Slack's own client.
        "channel_section_upserted"
        | "channel_section_deleted"
        | "channel_sections_channels_upserted"
        | "channel_sections_channels_removed"
        | "star_added"
        | "star_removed" => out.push(Translated::RefreshSections),
        // A custom emoji added, removed or renamed anywhere, so it shows
        // here without a restart.
        "emoji_changed" => out.push(match emoji_change(event) {
            Some(change) => Translated::Event(Event::EmojiChanged { team, change }),
            None => Translated::RefreshEmoji,
        }),
        // Your own Do Not Disturb changed, here or in another client.
        "dnd_updated" => {
            if let Some(dnd) = super::desktop::dnd_event(event) {
                out.push(Translated::Event(Event::Dnd { team, dnd }));
            }
        }
        "pin_added" | "pin_removed" => {
            if let Some(event) = super::convos::pin_event(kind, event) {
                out.push(Translated::Event(Event::Convos { team, event }));
            }
        }
        // Someone (maybe you, elsewhere) changed a channel's bookmarks.
        "bookmark_added" | "bookmark_changed" | "bookmark_removed" => {
            if let Some(event) = super::convos::bookmark_event(kind, event) {
                out.push(Translated::Event(Event::Convos { team, event }));
            }
        }
        // A browser session's socket also carries what Slack's own client
        // keeps for its activity badge, counts and search box, none of
        // which shows here. `user_huddle_changed` is a person's "in a
        // huddle" mark on their profile; the model keeps no such mark,
        // as who is in a huddle comes from the `sh_room_*` events.
        "activity" | "badge_counts_updated" | "search_recents" | "user_huddle_changed" => {}
        _ => match super::people::huddle_event(event) {
            Some(event) => out.push(Translated::Event(Event::People { team, event })),
            None => log::debug!("unhandled event {kind}"),
        },
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn events(value: &str) -> Vec<Translated> {
        translate("T1", "U1", &serde_json::from_str(value).expect("json"))
    }

    #[test]
    fn a_huddle_message_tells_of_the_huddle_and_is_a_message() {
        let got = events(
            r#"{"type":"message","subtype":"huddle_thread","channel":"C1","ts":"1.0",
                "user":"U2","text":"","room":{"id":"R1","channels":["C1"],
                "participants":["U2"],"date_end":0}}"#,
        );
        assert!(
            matches!(
                &got[..],
                [
                    Translated::Event(Event::People {
                        event: crate::people::Event::Huddles { .. },
                        ..
                    }),
                    Translated::Event(Event::Message { .. }),
                ]
            ),
            "{got:?}"
        );
    }

    #[test]
    fn people_events_read_by_their_fields() {
        let got = events(r#"{"type":"user_typing","channel":"C1","user":"U2"}"#);
        assert!(
            matches!(&got[..], [Translated::Event(Event::People {
                event: crate::people::Event::Typing { channel, user, thread: None },
                ..
            })] if channel == "C1" && user == "U2"),
            "{got:?}"
        );
        let got = events(r#"{"type":"channel_rename","channel":{"id":"C3","name":"x"}}"#);
        assert!(
            matches!(&got[..], [Translated::Refresh(id)] if id == "C3"),
            "{got:?}"
        );
        // A channel someone else made is not yours.
        assert!(events(r#"{"type":"channel_created","channel":{"id":"C4"}}"#).is_empty());
    }

    #[test]
    fn reads_on_other_devices_move_the_read_marker() {
        for kind in ["channel_marked", "group_marked", "im_marked", "mpim_marked"] {
            let read = events(&format!(
                r#"{{"type":"{kind}","channel":"C1","ts":"1700000000.000200"}}"#
            ));
            assert!(
                matches!(&read[..], [Translated::Event(Event::Read { team, channel, ts })]
                    if team == "T1" && channel == "C1" && ts.0 == "1700000000.000200"),
                "{kind}"
            );
        }
        assert!(events(r#"{"type":"channel_marked","channel":"C1"}"#).is_empty());
    }

    #[test]
    fn direct_messages_open_and_close_elsewhere() {
        for (kind, opened) in [
            ("im_open", true),
            ("im_close", false),
            ("mpim_open", true),
            ("mpim_close", false),
            ("group_open", true),
            ("group_close", false),
        ] {
            let got = events(&format!(
                r#"{{"type":"{kind}","user":"U1","channel":"D024BE91L"}}"#
            ));
            assert!(
                matches!(&got[..], [Translated::Event(Event::Opened { team, channel, open })]
                    if team == "T1" && channel == "D024BE91L" && *open == opened),
                "{kind}: {got:?}"
            );
        }
        assert!(events(r#"{"type":"im_close","user":"U1"}"#).is_empty());
    }

    #[test]
    fn threads_followed_elsewhere_show_as_followed() {
        let followed = events(
            r#"{"type":"thread_subscribed","subscription":{"type":"thread",
                "channel":"C1","thread_ts":"1700000000.000100","active":true,
                "last_read":"1700000000.000300"},"event_ts":"1700000001.000000"}"#,
        );
        assert!(
            matches!(&followed[..], [Translated::Event(Event::Views {
                team,
                event: crate::views::Event::Followed { channel, thread, follow: true },
            })] if team == "T1" && channel == "C1" && thread.0 == "1700000000.000100"),
            "{followed:?}"
        );
        let dropped = events(
            r#"{"type":"thread_unsubscribed","subscription":{"type":"thread",
                "channel":"C1","thread_ts":"1700000000.000100"}}"#,
        );
        assert!(matches!(
            &dropped[..],
            [Translated::Event(Event::Views {
                event: crate::views::Event::Followed { follow: false, .. },
                ..
            })]
        ));
        assert!(events(r#"{"type":"thread_subscribed","subscription":{}}"#).is_empty());
    }

    #[test]
    fn bookmark_events_reach_the_details_panel() {
        let removed =
            events(r#"{"type":"bookmark_removed","channel_id":"C1","bookmark":{"id":"Bk1"}}"#);
        assert!(
            matches!(&removed[..], [Translated::Event(Event::Convos {
                team,
                event: crate::convos::Event::BookmarkRemoved { channel, id },
            })] if team == "T1" && channel == "C1" && id == "Bk1"),
            "{removed:?}"
        );
        let added = events(
            r#"{"type":"bookmark_added","channel_id":"C1",
                "bookmark":{"id":"Bk2","title":"Docs","link":"https://a.example"}}"#,
        );
        assert!(matches!(
            &added[..],
            [Translated::Event(Event::Convos {
                event: crate::convos::Event::BookmarkChanged { .. },
                ..
            })]
        ));
        assert!(events(r#"{"type":"bookmark_changed"}"#).is_empty());
    }

    #[test]
    fn new_edited_and_deleted_messages() {
        match &events(r#"{"type":"message","channel":"C1","user":"U2","text":"hi","ts":"1.0"}"#)[..]
        {
            [
                Translated::Event(Event::Message {
                    channel,
                    message,
                    changed: false,
                    ..
                }),
            ] => {
                assert_eq!(channel, "C1");
                assert_eq!(message.text, "hi");
            }
            other => panic!("{other:?}"),
        }
        match &events(
            r#"{"type":"message","subtype":"message_changed","channel":"C1","message":{"user":"U2","text":"edited","ts":"1.0","edited":{"user":"U2","ts":"2.0"}}}"#,
        )[..]
        {
            [
                Translated::Event(Event::Message {
                    message, changed, ..
                }),
            ] => assert!(message.edited && *changed),
            other => panic!("{other:?}"),
        }
        match &events(
            r#"{"type":"message","subtype":"message_deleted","channel":"C1","deleted_ts":"1.0"}"#,
        )[..]
        {
            [Translated::Event(Event::Deleted { ts, .. })] => assert_eq!(ts.as_str(), "1.0"),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_deleted_file_and_its_tombstone_agree() {
        match &events(r#"{"type":"file_deleted","file_id":"F1","event_ts":"3.0"}"#)[..] {
            [Translated::Event(Event::FileGone { file, .. })] => assert_eq!(file, "F1"),
            other => panic!("{other:?}"),
        }
        assert!(events(r#"{"type":"file_deleted"}"#).is_empty());
        // The message that shared it stays, with the file in its place.
        match &events(
            r#"{"type":"message","subtype":"message_changed","channel":"C1","message":{"user":"U2","text":"","ts":"1.0","files":[{"id":"F1","mode":"tombstone"}]}}"#,
        )[..]
        {
            [Translated::Event(Event::Message { message, .. })] => {
                assert_eq!(message.files.len(), 1);
                assert!(message.files[0].deleted);
                assert_eq!(message.files[0].id, "F1");
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn reactions_and_membership() {
        match &events(
            r#"{"type":"reaction_added","user":"U2","reaction":"tada","item":{"type":"message","channel":"C1","ts":"1.0"}}"#,
        )[..]
        {
            [
                Translated::Event(Event::Reaction {
                    added: true, name, ..
                }),
            ] => assert_eq!(name, "tada"),
            other => panic!("{other:?}"),
        }
        assert!(matches!(
            &events(r#"{"type":"member_joined_channel","user":"U1","channel":"C9"}"#)[..],
            [Translated::Refresh(c)] if c == "C9"
        ));
        assert!(
            events(r#"{"type":"member_joined_channel","user":"U2","channel":"C9"}"#).is_empty()
        );
        assert!(
            events(r#"{"type":"channel_created","channel":{"id":"C5","name":"x"}}"#).is_empty()
        );
    }

    #[test]
    fn emoji_added_elsewhere_arrive_with_their_picture_or_alias() {
        use crate::emoji::EmojiChange;
        let added = events(
            r#"{"type":"emoji_changed","subtype":"add","name":"picard_facepalm",
                "value":"https://my.slack.com/emoji/picard_facepalm/db8e287430eaa459.gif",
                "event_ts":"1361482916.000004"}"#,
        );
        assert!(matches!(
            &added[..],
            [Translated::Event(Event::EmojiChanged { team, change: EmojiChange::Added { name, value } })]
                if team == "T1" && name == "picard_facepalm"
                    && value == "https://my.slack.com/emoji/picard_facepalm/db8e287430eaa459.gif"
        ));
        let alias = events(
            r#"{"type":"emoji_changed","subtype":"add","name":"facepalm","value":"alias:picard_facepalm"}"#,
        );
        assert!(matches!(
            &alias[..],
            [Translated::Event(Event::EmojiChanged { change: EmojiChange::Added { value, .. }, .. })]
                if value == "alias:picard_facepalm"
        ));
        assert!(
            matches!(
                &events(r#"{"type":"emoji_changed","subtype":"add","name":"x"}"#)[..],
                [Translated::RefreshEmoji]
            ),
            "without a value, the list is fetched again"
        );
    }

    #[test]
    fn emoji_removed_elsewhere_go_by_name() {
        use crate::emoji::EmojiChange;
        let removed = events(
            r#"{"type":"emoji_changed","subtype":"remove","names":["picard_facepalm","shipit"],
                "event_ts":"1361482916.000004"}"#,
        );
        assert!(matches!(
            &removed[..],
            [Translated::Event(Event::EmojiChanged { change: EmojiChange::Removed(names), .. })]
                if names == &["picard_facepalm", "shipit"]
        ));
        assert!(matches!(
            &events(r#"{"type":"emoji_changed","subtype":"remove","names":[]}"#)[..],
            [Translated::RefreshEmoji]
        ));
    }

    #[test]
    fn emoji_renamed_elsewhere_keep_their_picture() {
        use crate::emoji::EmojiChange;
        let renamed = events(
            r#"{"type":"emoji_changed","subtype":"rename","old_name":"grin","new_name":"cheese-grin",
                "value":"https://my.slack.com/emoji/picard_facepalm/db8e287430eaa459.gif",
                "event_ts":"1361482916.000004"}"#,
        );
        assert!(matches!(
            &renamed[..],
            [Translated::Event(Event::EmojiChanged { change: EmojiChange::Renamed { old, new, value: Some(_) }, .. })]
                if old == "grin" && new == "cheese-grin"
        ));
        let bare = events(
            r#"{"type":"emoji_changed","subtype":"rename","old_name":"grin","new_name":"cheese-grin"}"#,
        );
        assert!(matches!(
            &bare[..],
            [Translated::Event(Event::EmojiChanged {
                change: EmojiChange::Renamed { value: None, .. },
                ..
            })]
        ));
    }

    #[test]
    fn other_emoji_changes_fetch_the_list_again() {
        for event in [
            r#"{"type":"emoji_changed","event_ts":"1361482916.000004"}"#,
            r#"{"type":"emoji_changed","subtype":"recolor","name":"x"}"#,
        ] {
            assert!(
                matches!(&events(event)[..], [Translated::RefreshEmoji]),
                "{event}"
            );
        }
    }

    #[test]
    fn preference_and_dnd_changes() {
        assert!(matches!(
            &events(r#"{"type":"pref_change","name":"muted_channels","value":"C1"}"#)[..],
            [Translated::RefreshPrefs]
        ));
        assert!(events(r#"{"type":"pref_change","name":"theme","value":"dark"}"#).is_empty());
        assert!(matches!(
            &events(
                r#"{"type":"dnd_updated","user":"U1","dnd_status":{"dnd_enabled":false,"snooze_enabled":true,"snooze_endtime":500}}"#
            )[..],
            [Translated::Event(Event::Dnd { team, .. })] if team == "T1"
        ));
    }
}
