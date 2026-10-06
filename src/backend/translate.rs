//! What a real-time event from Slack means for the interface.

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
}

pub(super) fn str_of<'a>(value: &'a Value, key: &str) -> Option<&'a str> {
    value.get(key).and_then(Value::as_str)
}

fn message_of(value: &Value) -> Option<Message> {
    serde_json::from_value::<types::Message>(value.clone())
        .ok()?
        .into_model()
}

/// Turns an Events API event into interface events.
pub(super) fn translate(team: &str, me: &str, event: &Value) -> Vec<Translated> {
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
        "reaction_added" | "reaction_removed" => {
            let item = event.get("item");
            let channel = item.and_then(|i| str_of(i, "channel"));
            let ts = item.and_then(|i| str_of(i, "ts"));
            let name = str_of(event, "reaction");
            let user = str_of(event, "user");
            if let (Some(channel), Some(ts), Some(name), Some(user)) = (channel, ts, name, user) {
                out.push(Translated::Event(Event::Reaction {
                    team,
                    channel: channel.to_owned(),
                    ts: Ts::new(ts),
                    name: name.to_owned(),
                    user: user.to_owned(),
                    added: kind == "reaction_added",
                }));
            }
        }
        // Slack also sends the message with the file as a tombstone
        // (`message_changed`); this says so for every copy at once.
        "file_deleted" => {
            if let Some(file) = str_of(event, "file_id").filter(|f| !f.is_empty()) {
                out.push(Translated::Event(Event::FileGone {
                    team,
                    file: file.to_owned(),
                }));
            }
        }
        "member_joined_channel" | "member_left_channel" => {
            let user = str_of(event, "user");
            if user == Some(me)
                && let Some(channel) = channel
            {
                if kind == "member_joined_channel" {
                    out.push(Translated::Refresh(channel));
                } else {
                    out.push(Translated::Event(Event::ConversationGone { team, channel }));
                }
            }
        }
        "channel_left" | "group_left" | "channel_deleted" | "group_deleted" | "channel_archive"
        | "group_archive" => {
            if let Some(channel) = channel {
                out.push(Translated::Event(Event::ConversationGone { team, channel }));
            }
        }
        "channel_rename" | "group_rename" | "channel_created" | "channel_unarchive"
        | "im_created" => {
            let id = channel.or_else(|| {
                event
                    .get("channel")
                    .and_then(|c| str_of(c, "id"))
                    .map(str::to_owned)
            });
            // Only conversations you are in belong in the sidebar; a fresh
            // channel someone else made is not one of them.
            if let Some(id) = id
                && kind != "channel_created"
            {
                out.push(Translated::Refresh(id));
            }
        }
        "user_change" | "team_join" => {
            if let Some(user) = event
                .get("user")
                .and_then(|u| serde_json::from_value::<types::User>(u.clone()).ok())
            {
                out.push(Translated::Event(Event::Users {
                    team,
                    users: vec![user.into_model()],
                }));
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
        // Mutes, notification levels or keywords changed in another client.
        "pref_change" => {
            if super::desktop::is_notification_pref(str_of(event, "name").unwrap_or("")) {
                out.push(Translated::RefreshPrefs);
            }
        }
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
        // Read on another device (or in another window): the read marker
        // moves, so unread counts here follow. The interface only ever moves
        // a marker forward, so an older mark arriving late changes nothing.
        "channel_marked" | "group_marked" | "im_marked" | "mpim_marked" => {
            if let (Some(channel), Some(ts)) = (channel, str_of(event, "ts")) {
                out.push(Translated::Event(Event::Read {
                    team,
                    channel,
                    ts: Ts::new(ts),
                }));
            }
        }
        _ => match super::people::translate(event) {
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
