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

/// Translates Teams message content and message_type into display text,
/// an optional system subtype, and blocks; `None` for a message with
/// nothing to show (an activity Teams records that has no wording here).
pub fn translate_teams_content(
    msg: &types::Message,
) -> Option<(String, Option<String>, Vec<KitBlock>)> {
    let msg_type = msg.message_type.as_deref().unwrap_or("");
    let raw_content = msg.content.trim();

    // 1. Thread activity: who joined, renamed, changed what.
    if let Some(kind) = msg_type.strip_prefix("ThreadActivity/") {
        return thread_activity(kind, raw_content)
            .map(|(text, subtype)| (text, Some(subtype.to_owned()), Vec::new()));
    }
    // The same changes sometimes arrive without the type, as bare JSON.
    if raw_content.starts_with('{')
        && let Some(activity) = Activity::json(raw_content)
        && activity.value.is_some()
        && (raw_content.contains("\"newValue\"") || raw_content.contains("\"oldValue\""))
    {
        return Some((
            format!(
                "{} changed a setting to “{}”",
                activity.who(),
                mrkdwn_escape(&activity.value.unwrap_or_default())
            ),
            Some("channel_purpose".to_owned()),
            Vec::new(),
        ));
    }
    if raw_content.starts_with("{\"eventtime\":") || raw_content.starts_with("{\"eventTime\":") {
        return thread_activity("MemberJoined", raw_content)
            .map(|(text, subtype)| (text, Some(subtype.to_owned()), Vec::new()));
    }

    // 2. Check for Event/Call or meeting events
    if msg_type.starts_with("Event/Call")
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
        return Some((text, Some("channel_join".to_string()), Vec::new()));
    }

    // 3. Fallback: Check if content is a JSON blob starting with {"eventtime
    if raw_content.starts_with('{')
        && raw_content.ends_with('}')
        && let Ok(val) = serde_json::from_str::<serde_json::Value>(raw_content)
        && (val.get("eventtime").is_some() || val.get("eventTime").is_some())
    {
        return Some((
            "Meeting event.".to_string(),
            Some("channel_join".to_string()),
            Vec::new(),
        ));
    }

    // A file, picture or recording: its XML says what it is called, and
    // the name is what shows until such files can be opened here.
    if msg_type.starts_with("RichText/Media_") || msg_type == "RichText/UriObject" {
        let title = tag_texts(raw_content, "Title")
            .into_iter()
            .chain(attribute(raw_content, "OriginalName", "v"))
            .next()?;
        return Some((format!("📎 {}", mrkdwn_escape(&title)), None, Vec::new()));
    }

    // Anything else that is not a message (calls' signalling, typing,
    // cards Teams only shows itself) is left out rather than shown raw.
    if !(msg_type.is_empty() || msg_type.starts_with("Text") || msg_type.starts_with("RichText")) {
        log::debug!("leaving out a Teams message of type {msg_type}");
        return None;
    }

    // 4. Regular chat message
    let plain_text = strip_tags(&msg.content);
    let blocks = html_to_blocks(&msg.content);
    let mut kit_blocks = Vec::new();
    if !blocks.is_empty() {
        kit_blocks.push(KitBlock::RichText(Arc::from(blocks)));
    }

    Some((plain_text, None, kit_blocks))
}

/// The people and value a thread activity names, in either shape Teams
/// writes it: JSON (`{"user": …, "newValue": …, "members": […]}`) or XML
/// (`<initiator>…</initiator><value>…</value><target>…</target>`).
#[derive(Debug, Default, PartialEq)]
struct Activity {
    /// Who did it, as a user id.
    initiator: Option<String>,
    /// What a setting became.
    value: Option<String>,
    /// Whom it was done to, as user ids.
    targets: Vec<String>,
    /// The names Teams gave with them, for meeting guests who have no id
    /// to look up.
    names: Vec<String>,
}

impl Activity {
    fn parse(content: &str) -> Self {
        Self::json(content).unwrap_or_else(|| Self::xml(content))
    }

    fn json(content: &str) -> Option<Self> {
        let value: serde_json::Value = serde_json::from_str(content).ok()?;
        let text = |key: &str| match value.get(key)? {
            serde_json::Value::String(s) => Some(s.clone()),
            serde_json::Value::Null => None,
            other => Some(other.to_string()),
        };
        let members = value
            .get("members")
            .and_then(|m| m.as_array())
            .cloned()
            .unwrap_or_default();
        Some(Self {
            initiator: text("user")
                .or_else(|| text("initiator"))
                .and_then(|id| clean_teams_user_id(&id)),
            value: text("newValue").or_else(|| text("value")),
            targets: members
                .iter()
                .filter_map(|m| m.get("id").and_then(|id| id.as_str()))
                .filter_map(clean_teams_user_id)
                .collect(),
            names: members
                .iter()
                .filter_map(|m| {
                    m.get("friendlyname")
                        .or_else(|| m.get("friendlyName"))
                        .and_then(|f| f.as_str())
                })
                .map(str::trim)
                .filter(|n| !n.is_empty())
                .map(str::to_owned)
                .collect(),
        })
    }

    fn xml(content: &str) -> Self {
        Self {
            initiator: tag_texts(content, "initiator")
                .first()
                .and_then(|id| clean_teams_user_id(id)),
            value: tag_texts(content, "value").into_iter().next(),
            targets: tag_texts(content, "target")
                .iter()
                .chain(tag_texts(content, "id").iter())
                .filter_map(|id| clean_teams_user_id(id))
                .collect(),
            names: tag_texts(content, "friendlyname"),
        }
    }

    /// The one who did it, as a mention the interface names.
    fn who(&self) -> String {
        self.initiator
            .as_ref()
            .map_or_else(|| "Someone".to_owned(), |id| format!("<@{id}>"))
    }

    /// Whom it was done to: names where Teams gave them, else mentions.
    fn whom(&self) -> String {
        let people: Vec<String> = if self.names.is_empty() {
            self.targets.iter().map(|id| format!("<@{id}>")).collect()
        } else {
            self.names.iter().map(|n| mrkdwn_escape(n)).collect()
        };
        match people.as_slice() {
            [] => "someone".to_owned(),
            [one] => one.clone(),
            [first, second] => format!("{first} and {second}"),
            [first, second, rest @ ..] => format!("{first}, {second} and {} others", rest.len()),
        }
    }

    /// Whether the change was made to the one who made it: joining or
    /// leaving, rather than adding or removing someone.
    fn by_themselves(&self) -> bool {
        self.initiator.is_none()
            || self
                .targets
                .iter()
                .all(|t| Some(t) == self.initiator.as_ref())
    }
}

/// A thread activity (`ThreadActivity/{kind}`) as a system line and its
/// subtype, or `None` for one with nothing worth a line.
fn thread_activity(kind: &str, content: &str) -> Option<(String, &'static str)> {
    let activity = Activity::parse(content);
    let value = activity.value.as_deref().map(mrkdwn_escape);
    Some(match kind {
        "AddMember" | "MemberJoined" if activity.by_themselves() || !activity.names.is_empty() => {
            (format!("{} joined", activity.whom()), "channel_join")
        }
        "AddMember" | "MemberJoined" => (
            format!("{} added {}", activity.who(), activity.whom()),
            "channel_join",
        ),
        "DeleteMember" | "MemberLeft" if activity.by_themselves() => {
            (format!("{} left", activity.whom()), "channel_leave")
        }
        "DeleteMember" | "MemberLeft" => (
            format!("{} removed {}", activity.who(), activity.whom()),
            "channel_leave",
        ),
        "TopicUpdate" => match value.filter(|v| !v.trim().is_empty()) {
            Some(topic) => (
                format!("{} renamed the conversation to “{topic}”", activity.who()),
                "channel_name",
            ),
            None => (
                format!("{} removed the conversation's name", activity.who()),
                "channel_name",
            ),
        },
        "HistoryDisabled" => ("Chat history was turned off".to_owned(), "channel_purpose"),
        other => {
            // `PictureUpdate`, `DescriptionUpdate` and the like: "changed
            // the picture", with the new value when it is readable text.
            let setting = other.strip_suffix("Update")?;
            let words = camel_to_words(setting);
            match value.filter(|v| !v.trim().is_empty() && v.len() < 200) {
                Some(value) => (
                    format!("{} changed the {words} to “{value}”", activity.who()),
                    "channel_purpose",
                ),
                None => (
                    format!("{} changed the {words}", activity.who()),
                    "channel_purpose",
                ),
            }
        }
    })
}

/// The text inside every `<tag>…</tag>` of `xml`, unescaped.
fn tag_texts(xml: &str, tag: &str) -> Vec<String> {
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    let mut found = Vec::new();
    let mut rest = xml;
    while let Some(start) = rest.find(&open) {
        rest = &rest[start + open.len()..];
        let Some(end) = rest.find(&close) else {
            break;
        };
        let text = crate::teams::html::unescape_html(rest[..end].trim());
        if !text.is_empty() {
            found.push(text);
        }
        rest = &rest[end + close.len()..];
    }
    found
}

/// The `attr` of the first `<tag …>` in `xml`, as Teams writes a file's
/// name: `<OriginalName v="report.pdf"/>`.
fn attribute(xml: &str, tag: &str, attr: &str) -> Option<String> {
    let start = xml.find(&format!("<{tag} "))?;
    let rest = &xml[start..];
    let rest = &rest[..rest.find('>')?];
    let key = format!("{attr}=\"");
    let value = &rest[rest.find(&key)? + key.len()..];
    let value = crate::teams::html::unescape_html(&value[..value.find('"')?]);
    (!value.is_empty()).then_some(value)
}

/// `DescriptionUpdate`'s setting in words: `PictureUrl` → `picture url`.
fn camel_to_words(name: &str) -> String {
    let mut words = String::new();
    for c in name.chars() {
        if c.is_uppercase() && !words.is_empty() {
            words.push(' ');
        }
        words.extend(c.to_lowercase());
    }
    words
}

/// Text made safe to put in a message's mrkdwn, where `<` starts a
/// mention or link.
fn mrkdwn_escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// Translates a Teams [`types::Conversation`] into a [`Conversation`].
pub fn translate_conversation(conv: &types::Conversation) -> Conversation {
    let name = conv.display_name();
    let kind = if conv.is_channel() {
        ConversationKind::Channel
    } else if conv.is_meeting() {
        ConversationKind::Group
    } else if conv.id.contains("@unq.gbl.spaces") || conv.id.starts_with("19:uni01_") {
        // One-to-one chats come in both shapes in work tenants.
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
        last_read: conv.last_read_id().map(teams_id_to_ts),
        latest,
        unread: 0,
        mentions: 0,
        external: false,
        is_open: Some(true),
        empty: false,
    }
}

/// Translates a Teams [`types::Message`] into a [`Message`].
/// `None` for a message with nothing to show (see
/// [`translate_teams_content`]).
pub fn translate_message(msg: &types::Message) -> Option<Message> {
    let ts = teams_id_to_ts(&msg.id);
    let (plain_text, subtype, kit_blocks) = translate_teams_content(msg)?;

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

    Some(Message {
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
    })
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
            properties: Some(types::ConversationProperties {
                consumption_horizon: Some("1728287300000;1728287300001;0".into()),
            }),
        };

        let conv = translate_conversation(&teams_conv);
        assert_eq!(conv.name, "Project Alpha");
        assert_eq!(conv.kind, ConversationKind::Channel);
        assert_eq!(conv.last_read, Some(teams_id_to_ts("1728287300000")));
        assert!(conv.has_unread(), "read up to before the last message");
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

        let msg = translate_message(&teams_msg).expect("a message");
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

        let msg = translate_message(&add_member_msg).expect("a message");
        assert_eq!(msg.text, "Alexandra Ioan - IC joined");
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

        let msg2 = translate_message(&call_ended_msg).expect("a message");
        assert_eq!(msg2.text, "Meeting ended.");
        assert_eq!(msg2.subtype.as_deref(), Some("channel_join"));
        assert!(msg2.is_system());
    }

    fn activity(kind: &str, content: &str) -> Option<Message> {
        translate_message(&types::Message {
            id: "1732698120000".into(),
            message_type: Some(kind.into()),
            content: content.into(),
            from: Some("19:abc@thread.tacv2".into()),
            ..Default::default()
        })
    }

    #[test]
    fn a_setting_change_reads_as_a_line_not_json() {
        // As the Teams web client's own "All IO Collaborators" channel had it.
        let content = r#"{"oldValue":null,"newValue":"All Collaborators of the ITER Organization","user":"8:orgid:01f9b9bc-5d21-4b48-9df6-84ac2ceb0ba9"}"#;
        for kind in ["ThreadActivity/DescriptionUpdate", "RichText/Html"] {
            let msg = activity(kind, content).expect("a line");
            assert!(msg.is_system(), "{kind}");
            assert!(
                msg.text
                    .starts_with("<@01f9b9bc-5d21-4b48-9df6-84ac2ceb0ba9> changed"),
                "{kind}: {}",
                msg.text
            );
            assert!(
                msg.text
                    .contains("“All Collaborators of the ITER Organization”")
            );
            assert!(!msg.text.contains("oldValue"));
        }
    }

    #[test]
    fn renames_and_membership_name_who_did_what() {
        let renamed = activity(
            "ThreadActivity/TopicUpdate",
            "<topicupdate><eventtime>1</eventtime><initiator>8:orgid:a</initiator><value>Plans &amp; more</value></topicupdate>",
        )
        .expect("a line");
        assert_eq!(
            renamed.text,
            "<@a> renamed the conversation to “Plans &amp; more”"
        );
        assert_eq!(renamed.subtype.as_deref(), Some("channel_name"));

        let added = activity(
            "ThreadActivity/AddMember",
            "<addmember><eventtime>1</eventtime><initiator>8:orgid:a</initiator><target>8:orgid:b</target><target>8:orgid:c</target></addmember>",
        )
        .expect("a line");
        assert_eq!(added.text, "<@a> added <@b> and <@c>");

        let left = activity(
            "ThreadActivity/DeleteMember",
            "<deletemember><initiator>8:orgid:b</initiator><target>8:orgid:b</target></deletemember>",
        )
        .expect("a line");
        assert_eq!(left.text, "<@b> left");
        assert_eq!(left.subtype.as_deref(), Some("channel_leave"));
    }

    #[test]
    fn what_has_no_wording_is_left_out() {
        assert!(activity("ThreadActivity/SomethingNew", "<x/>").is_none());
        assert!(activity("Control/Typing", "").is_none());
        assert!(activity("RichText/Media_CallRecording", "<URIObject/>").is_none());
        let file = activity(
            "RichText/Media_GenericFile",
            r#"<URIObject type="File.1"><Title>Plan.docx</Title><OriginalName v="Plan.docx"/></URIObject>"#,
        )
        .expect("a file");
        assert_eq!(file.text, "📎 Plan.docx");
        assert!(!file.is_system());
        assert!(activity("Text", "hello").is_some());
        assert!(activity("RichText/Html", "<p>hello</p>").is_some());
    }

    #[test]
    fn both_shapes_of_one_to_one_chat_are_direct() {
        for id in ["19:a_b@unq.gbl.spaces", "19:uni01_abc123@thread.v2"] {
            let conv = types::Conversation {
                id: id.into(),
                ..Default::default()
            };
            assert_eq!(
                translate_conversation(&conv).kind,
                ConversationKind::Direct,
                "{id}"
            );
        }
        let group = types::Conversation {
            id: "19:0123abcd@thread.v2".into(),
            ..Default::default()
        };
        assert_eq!(
            translate_conversation(&group).kind,
            ConversationKind::Private
        );
    }

    #[test]
    fn a_conversation_without_a_read_marker_is_read_nowhere() {
        let conv = types::Conversation {
            id: "19:a@thread.v2".into(),
            properties: Some(types::ConversationProperties {
                consumption_horizon: Some("0;0;0".into()),
            }),
            ..Default::default()
        };
        assert_eq!(translate_conversation(&conv).last_read, None);
    }
}
