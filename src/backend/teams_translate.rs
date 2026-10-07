//! Translates Microsoft Teams data structures into NoSlacking model types.
//!
//! Converts Teams message IDs to/from [`Ts`], HTML message content to rich text blocks,
//! and Teams conversations, users, and teams into [`Conversation`], [`User`], and [`SidebarSection`].

use std::sync::Arc;

use crate::model::{
    Conversation, ConversationKind, Delivery, KitBlock, Message, Reaction, SectionKind,
    SidebarSection, Ts, User,
};
use crate::teams::html::{html_to_blocks, strip_tags};
use crate::teams::types;

/// Converts a Teams message ID (epoch milliseconds or string) into a [`Ts`].
pub fn teams_id_to_ts(id: &str) -> Ts {
    if let Ok(millis) = id.parse::<u64>() {
        let secs = millis / 1000;
        let micros = (millis % 1000) * 1000;
        return Ts::new(format!("{secs}.{micros:06}"));
    }
    // Fall back to numeric extraction if possible
    let digits: String = id.chars().filter(|c| c.is_ascii_digit()).collect();
    if let Ok(millis) = digits.parse::<u64>() {
        let secs = millis / 1000;
        let micros = (millis % 1000) * 1000;
        return Ts::new(format!("{secs}.{micros:06}"));
    }
    // As last resort, wrap id in Ts
    Ts::new(id)
}

/// Converts a [`Ts`] back into a Teams millisecond epoch ID string.
pub fn ts_to_teams_id(ts: &Ts) -> String {
    if let Some((secs_str, frac_str)) = ts.as_str().split_once('.')
        && let Ok(secs) = secs_str.parse::<u64>()
    {
        let micros = frac_str
            .bytes()
            .chain(std::iter::repeat(b'0'))
            .take(6)
            .fold(0u64, |m, d| m * 10 + u64::from(d.saturating_sub(b'0')));
        let millis = secs * 1000 + micros / 1000;
        return millis.to_string();
    }
    ts.as_str().to_string()
}

/// Extracts a clean user ID from a Teams `from` field (which may be a contacts URL or MRI).
/// Returns `None` if the sender is a thread, channel, or system entity.
pub fn clean_teams_user_id(from: &str) -> Option<String> {
    if from.contains("@thread") || from.starts_with("19:") {
        return None;
    }
    let after_path = if let Some((_, last)) = from.rsplit_once("/contacts/") {
        last
    } else if let Some((_, last)) = from.rsplit_once('/') {
        last
    } else {
        from
    };

    if after_path.contains("@thread") || after_path.starts_with("19:") {
        return None;
    }

    let stripped = after_path
        .trim_start_matches("8:orgid:")
        .trim_start_matches("8:teamsvisitor:")
        .trim_start_matches("8:guest:")
        .trim_start_matches("8:");

    if stripped.is_empty() {
        None
    } else {
        Some(stripped.to_string())
    }
}

/// Translates Teams message content and message_type into display text, an optional system subtype, and blocks.
pub fn translate_teams_content(msg: &types::Message) -> (String, Option<String>, Vec<KitBlock>) {
    let msg_type = msg.message_type.as_deref().unwrap_or("");
    let raw_content = msg.content.trim();

    // 1. Check for ThreadActivity / Member updates (JSON or XML)
    if msg_type.contains("AddMember")
        || msg_type.contains("MemberJoined")
        || raw_content.starts_with("{\"eventtime\":")
        || raw_content.starts_with("{\"eventTime\":")
    {
        if let Ok(val) = serde_json::from_str::<serde_json::Value>(raw_content)
            && let Some(members) = val.get("members").and_then(|m| m.as_array())
        {
            let names: Vec<&str> = members
                .iter()
                .filter_map(|m| {
                    m.get("friendlyname")
                        .or_else(|| m.get("friendlyName"))
                        .and_then(|f| f.as_str())
                })
                .filter(|n| !n.trim().is_empty())
                .collect();
            if !names.is_empty() {
                let text = if names.len() == 1 {
                    format!("{} joined the meeting.", names[0])
                } else if names.len() == 2 {
                    format!("{} and {} joined the meeting.", names[0], names[1])
                } else {
                    format!(
                        "{}, {}, and {} others joined the meeting.",
                        names[0],
                        names[1],
                        names.len() - 2
                    )
                };
                return (text, Some("channel_join".to_string()), Vec::new());
            }
        }
        return (
            "Members joined the meeting.".to_string(),
            Some("channel_join".to_string()),
            Vec::new(),
        );
    }

    if msg_type.contains("DeleteMember") || msg_type.contains("MemberLeft") {
        if let Ok(val) = serde_json::from_str::<serde_json::Value>(raw_content)
            && let Some(members) = val.get("members").and_then(|m| m.as_array())
        {
            let names: Vec<&str> = members
                .iter()
                .filter_map(|m| {
                    m.get("friendlyname")
                        .or_else(|| m.get("friendlyName"))
                        .and_then(|f| f.as_str())
                })
                .filter(|n| !n.trim().is_empty())
                .collect();
            if !names.is_empty() {
                let text = format!("{} left the meeting.", names.join(", "));
                return (text, Some("channel_leave".to_string()), Vec::new());
            }
        }
        return (
            "A member left the meeting.".to_string(),
            Some("channel_leave".to_string()),
            Vec::new(),
        );
    }

    if msg_type.contains("TopicUpdate") {
        let topic = strip_tags(&msg.content);
        let text = if topic.trim().is_empty() {
            "Channel topic updated.".to_string()
        } else {
            format!("Topic updated to: {}", topic.trim())
        };
        return (text, Some("channel_topic".to_string()), Vec::new());
    }

    // 2. Check for Event/Call or meeting events
    if msg_type.contains("Call")
        || raw_content.contains("<systemMessage")
        || raw_content.contains("<partlist")
    {
        let text = if raw_content.contains("Ended") || raw_content.contains("ended") {
            "Meeting ended.".to_string()
        } else if raw_content.contains("Started") || raw_content.contains("started") {
            "Meeting started.".to_string()
        } else if raw_content.contains("Scheduled") || raw_content.contains("scheduled") {
            "Meeting scheduled.".to_string()
        } else {
            "Call event.".to_string()
        };
        return (text, Some("channel_join".to_string()), Vec::new());
    }

    // 3. Fallback: Check if content is a JSON blob starting with {"eventtime
    if raw_content.starts_with('{')
        && raw_content.ends_with('}')
        && let Ok(val) = serde_json::from_str::<serde_json::Value>(raw_content)
        && (val.get("eventtime").is_some() || val.get("eventTime").is_some())
    {
        return (
            "Meeting event.".to_string(),
            Some("channel_join".to_string()),
            Vec::new(),
        );
    }

    // 4. Regular chat message
    let plain_text = strip_tags(&msg.content);
    let blocks = html_to_blocks(&msg.content);
    let mut kit_blocks = Vec::new();
    if !blocks.is_empty() {
        kit_blocks.push(KitBlock::RichText(Arc::from(blocks)));
    }

    (plain_text, None, kit_blocks)
}

/// Translates a Teams [`types::Conversation`] into a [`Conversation`].
pub fn translate_conversation(conv: &types::Conversation) -> Conversation {
    let name = conv.display_name();
    let kind = if conv.is_channel() {
        ConversationKind::Channel
    } else if conv.is_meeting() {
        ConversationKind::Group
    } else if conv.id.contains("@unq.gbl.spaces") {
        ConversationKind::Direct
    } else {
        ConversationKind::Private
    };

    let latest = conv
        .last_message
        .as_ref()
        .and_then(|m| m.id.as_deref())
        .map(teams_id_to_ts);

    let raw_topic = conv
        .thread_properties
        .as_ref()
        .and_then(|p| p.topic.clone())
        .unwrap_or_default();

    // Do not duplicate topic if it matches the conversation display name
    let topic = if raw_topic == name {
        String::new()
    } else {
        raw_topic
    };

    Conversation {
        id: conv.id.clone(),
        name,
        kind,
        user: None,
        topic,
        purpose: String::new(),
        members: None,
        archived: false,
        last_read: None,
        latest,
        unread: 0,
        mentions: 0,
        external: false,
        is_open: Some(true),
        empty: false,
    }
}

/// Translates a Teams [`types::Message`] into a [`Message`].
pub fn translate_message(msg: &types::Message) -> Message {
    let ts = teams_id_to_ts(&msg.id);
    let (plain_text, subtype, kit_blocks) = translate_teams_content(msg);

    let (user, username) = if subtype.is_some() {
        (None, None)
    } else {
        let user = msg.from.as_deref().and_then(clean_teams_user_id);
        let username = msg.im_display_name.clone().filter(|n| !n.trim().is_empty());
        (user, username)
    };

    let mut reactions = Vec::new();
    if let Some(props) = &msg.properties
        && let Some(emotions) = &props.emotions
    {
        for emotion in emotions {
            let name = map_emotion_to_reaction_name(&emotion.key);
            let users: Vec<String> = emotion
                .users
                .iter()
                .filter_map(|u| clean_teams_user_id(&u.mri))
                .collect();
            let count = users.len() as u32;
            reactions.push(Reaction { name, count, users });
        }
    }

    let edited = msg.properties.as_ref().is_some_and(|p| p.is_edited());

    Message {
        ts,
        user,
        username,
        bot_icon: None,
        bot_id: None,
        text: plain_text,
        thread_ts: None,
        reply_count: 0,
        replies_known: true,
        reply_users: Vec::new(),
        latest_reply: None,
        reactions,
        files: Vec::new(),
        attachments: Vec::new(),
        blocks: kit_blocks,
        edited,
        subtype,
        delivery: Delivery::Sent,
        broadcast: false,
        pinned: false,
        client_msg_id: msg.client_message_id.clone(),
        subscribed: None,
    }
}

/// Translates a Teams [`types::UserDetails`] into a [`User`].
pub fn translate_user(user: &types::UserDetails) -> User {
    let display_name = user.display_name.clone().unwrap_or_else(|| user.id.clone());
    let name = user
        .user_principal_name
        .clone()
        .unwrap_or_else(|| display_name.clone());

    User {
        id: user.id.clone(),
        name,
        real_name: display_name.clone(),
        display_name,
        avatar: None,
        is_bot: false,
        deleted: false,
        title: String::new(),
        status_text: String::new(),
        status_emoji: String::new(),
        tz: None,
        team: String::new(),
        enterprise: String::new(),
        stranger: false,
    }
}

/// Translates a Teams [`types::Team`] into a [`SidebarSection`] and child [`Conversation`]s.
pub fn translate_team(team: &types::Team) -> (SidebarSection, Vec<Conversation>) {
    let channel_ids: Vec<String> = team.channels.iter().map(|c| c.id.clone()).collect();
    let section = SidebarSection {
        id: team.id.clone(),
        kind: SectionKind::Custom,
        name: team.display_name.clone(),
        emoji: String::new(),
        channel_ids: channel_ids.clone(),
    };

    let conversations = team
        .channels
        .iter()
        .map(|c| Conversation {
            id: c.id.clone(),
            name: c.display_name.clone(),
            kind: ConversationKind::Channel,
            user: None,
            topic: c.description.clone().unwrap_or_default(),
            purpose: String::new(),
            members: None,
            archived: false,
            last_read: None,
            latest: None,
            unread: 0,
            mentions: 0,
            external: false,
            is_open: Some(true),
            empty: false,
        })
        .collect();

    (section, conversations)
}

fn map_emotion_to_reaction_name(key: &str) -> String {
    match key {
        "like" => "thumbsup".into(),
        "heart" => "heart".into(),
        "laugh" => "joy".into(),
        "surprised" => "open_mouth".into(),
        "sad" => "cry".into(),
        "angry" => "rage".into(),
        other => other.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timestamp_lossless_round_trip() {
        let teams_id = "1728287364123";
        let ts = teams_id_to_ts(teams_id);
        assert_eq!(ts.as_str(), "1728287364.123000");
        let back = ts_to_teams_id(&ts);
        assert_eq!(back, teams_id);
    }

    #[test]
    fn translates_conversation_and_message() {
        let teams_conv = types::Conversation {
            id: "19:ch123@thread.tacv2".into(),
            conversation_type: Some("Thread".into()),
            thread_properties: Some(types::ThreadProperties {
                topic: Some("Project Alpha".into()),
                ..Default::default()
            }),
            last_message: Some(types::MessagePreview {
                id: Some("1728287364000".into()),
                im_display_name: Some("Alice".into()),
                content: Some("<p>Hello</p>".into()),
                ..Default::default()
            }),
        };

        let conv = translate_conversation(&teams_conv);
        assert_eq!(conv.name, "Project Alpha");
        assert_eq!(conv.kind, ConversationKind::Channel);
        assert_eq!(
            conv.latest.as_ref().map(|t| t.as_str()),
            Some("1728287364.000000")
        );

        let teams_msg = types::Message {
            id: "1728287364000".into(),
            from: Some("8:orgid:alice-id".into()),
            im_display_name: Some("Alice".into()),
            content: "<p>Hello <b>team</b></p>".into(),
            properties: Some(types::MessageProperties {
                emotions: Some(vec![types::Emotion {
                    key: "like".into(),
                    users: vec![types::EmotionUser {
                        mri: "8:orgid:bob-id".into(),
                        time: Some(1728287370000),
                    }],
                }]),
                ..Default::default()
            }),
            ..Default::default()
        };

        let msg = translate_message(&teams_msg);
        assert_eq!(msg.user.as_deref(), Some("alice-id"));
        assert_eq!(msg.username.as_deref(), Some("Alice"));
        assert_eq!(msg.text, "Hello team");
        assert!(msg.rich_text().is_some());
        assert_eq!(msg.reactions.len(), 1);
        assert_eq!(msg.reactions[0].name, "thumbsup");
        assert_eq!(msg.reactions[0].users, ["bob-id"]);
    }

    #[test]
    fn cleans_teams_user_id_from_urls_and_mris() {
        assert_eq!(
            clean_teams_user_id(
                "https://fr.ng.msg.teams.microsoft.com/v1/users/ME/contacts/8:orgid:75e42c8f-3a75-4e3d-890e-890082762a57"
            ),
            Some("75e42c8f-3a75-4e3d-890e-890082762a57".into())
        );
        assert_eq!(
            clean_teams_user_id("8:orgid:alice-uuid"),
            Some("alice-uuid".into())
        );
        assert_eq!(
            clean_teams_user_id("8:teamsvisitor:visitor-uuid"),
            Some("visitor-uuid".into())
        );
        assert_eq!(
            clean_teams_user_id(
                "https://fr.ng.msg.teams.microsoft.com/v1/users/ME/contacts/19:meeting_12345@thread.v2"
            ),
            None
        );
        assert_eq!(clean_teams_user_id("19:meeting_12345@thread.v2"), None);
    }

    #[test]
    fn translates_system_activity_messages() {
        let add_member_msg = types::Message {
            id: "1702370363757".into(),
            message_type: Some("ThreadActivity/AddMember".into()),
            content: r#"{"eventtime":1702370363757,"initiator":"8:orgid:host-id","members":[{"id":"8:teamsvisitor:visitor-id","friendlyname":"Alexandra Ioan - IC"}]}"#.into(),
            from: Some(
                "https://fr.ng.msg.teams.microsoft.com/v1/users/ME/contacts/19:meeting_123@thread.v2"
                    .into(),
            ),
            ..Default::default()
        };

        let msg = translate_message(&add_member_msg);
        assert_eq!(msg.text, "Alexandra Ioan - IC joined the meeting.");
        assert_eq!(msg.subtype.as_deref(), Some("channel_join"));
        assert!(msg.is_system());
        assert!(msg.user.is_none());
        assert!(msg.username.is_none());

        let call_ended_msg = types::Message {
            id: "1702370363758".into(),
            message_type: Some("Event/Call".into()),
            content: "Started 09:55:28 Ended 10:00:00".into(),
            from: Some("8:orgid:some-user".into()),
            ..Default::default()
        };

        let msg2 = translate_message(&call_ended_msg);
        assert_eq!(msg2.text, "Meeting ended.");
        assert_eq!(msg2.subtype.as_deref(), Some("channel_join"));
        assert!(msg2.is_system());
    }
}
