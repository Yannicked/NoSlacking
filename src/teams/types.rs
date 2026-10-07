//! Types representing the Microsoft Teams wire format.
//!
//! Covers ChatSvc, CSA, and Trouter JSON structures. All parsing
//! handles missing and optional fields gracefully.

use serde::{Deserialize, Deserializer, Serialize};

/// Strips surrounding quotes from string values if present.
fn de_trimmed_string<'de, D>(deserializer: D) -> Result<Option<String>, D::Error>
where
    D: Deserializer<'de>,
{
    let opt: Option<String> = Option::deserialize(deserializer)?;
    Ok(opt.map(|s| s.trim_matches('"').to_string()))
}

/// A user identity in Microsoft Teams.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct UserDetails {
    pub id: String,
    #[serde(default, rename = "userPrincipalName")]
    pub user_principal_name: Option<String>,
    #[serde(default, rename = "displayName")]
    pub display_name: Option<String>,
    /// The address; Graph calls it `mail`.
    #[serde(default, alias = "mail")]
    pub email: Option<String>,
}

/// User presence information from Teams presence service.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PresenceInfo {
    #[serde(default)]
    pub availability: Option<String>,
    #[serde(default)]
    pub activity: Option<String>,
    #[serde(default, rename = "deviceType")]
    pub device_type: Option<String>,
}

/// A team in Microsoft Teams (CSA endpoint).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Team {
    pub id: String,
    #[serde(default, rename = "displayName")]
    pub display_name: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub channels: Vec<Channel>,
    #[serde(
        default,
        rename = "pictureETag",
        deserialize_with = "de_trimmed_string"
    )]
    pub picture_etag: Option<String>,
}

/// A channel within a team.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Channel {
    pub id: String,
    #[serde(default, rename = "displayName")]
    pub display_name: String,
    #[serde(default)]
    pub description: Option<String>,
}

/// A conversation (1:1 chat, group chat, meeting chat, or channel thread).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Conversation {
    pub id: String,
    #[serde(default, rename = "type")]
    pub conversation_type: Option<String>,
    #[serde(default, rename = "threadProperties")]
    pub thread_properties: Option<ThreadProperties>,
    #[serde(default, rename = "lastMessage")]
    pub last_message: Option<MessagePreview>,
    #[serde(default)]
    pub properties: Option<ConversationProperties>,
}

/// A chat's own record (`/v1/threads/{id}`), for who is in it.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Thread {
    #[serde(default)]
    pub members: Vec<ThreadMember>,
}

/// Someone in a chat.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ThreadMember {
    /// Their MRI.
    pub id: String,
    #[serde(default)]
    pub role: Option<String>,
}

/// Your own state in a conversation.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConversationProperties {
    /// How far you have read: `{message id};{when, ms};{client message
    /// id}`, as the read marker sets it.
    #[serde(default, rename = "consumptionhorizon")]
    pub consumption_horizon: Option<String>,
}

impl Conversation {
    /// The id of the last message you have read here, if Teams knows.
    pub fn last_read_id(&self) -> Option<&str> {
        let horizon = self.properties.as_ref()?.consumption_horizon.as_deref()?;
        horizon
            .split(';')
            .next()
            .filter(|id| !id.is_empty() && *id != "0")
    }

    /// The best display name for this conversation.
    pub fn display_name(&self) -> String {
        if let Some(props) = &self.thread_properties {
            if let Some(topic) = &props.topic
                && !topic.is_empty()
            {
                return topic.clone();
            }
            if let Some(space_topic) = &props.space_thread_topic
                && !space_topic.is_empty()
            {
                return space_topic.clone();
            }
        }
        if let Some(msg) = &self.last_message
            && let Some(name) = &msg.im_display_name
            && !name.is_empty()
        {
            return name.clone();
        }
        self.id.clone()
    }

    /// Whether this is a channel thread rather than a direct chat.
    pub fn is_channel(&self) -> bool {
        self.id.contains("@thread.tacv2")
    }

    /// Whether this is a meeting chat.
    pub fn is_meeting(&self) -> bool {
        self.id.contains("meeting_")
    }
}

/// Properties of a conversation thread.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ThreadProperties {
    #[serde(default)]
    pub topic: Option<String>,
    #[serde(default, rename = "spaceThreadTopic")]
    pub space_thread_topic: Option<String>,
    #[serde(default, rename = "lastjoinat")]
    pub last_join_at: Option<String>,
    #[serde(default)]
    pub members: Option<String>,
}

/// Preview of the last message in a conversation.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MessagePreview {
    #[serde(default)]
    pub id: Option<String>,
    #[serde(default, rename = "composetime")]
    pub compose_time: Option<String>,
    #[serde(default, rename = "originalarrivaltime")]
    pub original_arrival_time: Option<String>,
    #[serde(default)]
    pub from: Option<String>,
    #[serde(default, rename = "imdisplayname")]
    pub im_display_name: Option<String>,
    #[serde(default)]
    pub content: Option<String>,
    #[serde(default, rename = "messagetype")]
    pub message_type: Option<String>,
}

/// A message from Teams ChatSvc.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Message {
    pub id: String,
    #[serde(default, rename = "sequenceId")]
    pub sequence_id: Option<u64>,
    #[serde(default, rename = "clientarrivaltime")]
    pub client_arrival_time: Option<String>,
    #[serde(default, rename = "composetime")]
    pub compose_time: Option<String>,
    #[serde(default, rename = "originalarrivaltime")]
    pub original_arrival_time: Option<String>,
    #[serde(default)]
    pub from: Option<String>,
    #[serde(default, rename = "imdisplayname")]
    pub im_display_name: Option<String>,
    #[serde(default)]
    pub content: String,
    #[serde(default, rename = "messagetype")]
    pub message_type: Option<String>,
    #[serde(default)]
    pub properties: Option<MessageProperties>,
    #[serde(default, rename = "conversationId")]
    pub conversation_id: Option<String>,
    /// The sender's own id for the message, which the echo of a message
    /// sent from here carries back.
    #[serde(default, rename = "clientmessageid")]
    pub client_message_id: Option<String>,
}

/// Message properties carrying reactions, edits, and deletions.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MessageProperties {
    #[serde(default)]
    pub emotions: Option<Vec<Emotion>>,
    #[serde(default, rename = "deletetime")]
    pub delete_time: Option<serde_json::Value>,
    #[serde(default, rename = "edittime")]
    pub edit_time: Option<serde_json::Value>,
    #[serde(default, rename = "isdelete")]
    pub is_deleted: Option<serde_json::Value>,
}

impl MessageProperties {
    /// Whether this message has been marked as deleted.
    pub fn is_deleted(&self) -> bool {
        if let Some(ref d) = self.is_deleted {
            match d {
                serde_json::Value::Bool(b) => *b,
                serde_json::Value::String(s) => s.eq_ignore_ascii_case("true"),
                _ => false,
            }
        } else {
            self.delete_time.is_some()
        }
    }

    /// Whether this message was edited.
    pub fn is_edited(&self) -> bool {
        self.edit_time.is_some()
    }
}

/// An emotion / reaction on a message.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Emotion {
    #[serde(rename = "key")]
    pub key: String,
    #[serde(default)]
    pub users: Vec<EmotionUser>,
}

/// A user who reacted.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct EmotionUser {
    #[serde(default)]
    pub mri: String,
    #[serde(default)]
    pub time: Option<u64>,
}

/// Response from `/users/ME/conversations`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConversationsResponse {
    #[serde(default)]
    pub conversations: Vec<Conversation>,
}

/// Response from `/users/ME/conversations/{id}/messages`, newest first.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MessagesResponse {
    #[serde(default)]
    pub messages: Vec<Message>,
    #[serde(default, rename = "_metadata")]
    pub metadata: Option<MessagesMetadata>,
}

/// Paging for a messages page.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MessagesMetadata {
    /// The URL of the page of older messages, if there are any.
    #[serde(default, rename = "backwardLink")]
    pub backward_link: Option<String>,
}

/// The answer to posting a message: when the server took it, in epoch
/// milliseconds, which is also the new message's id.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PostedMessage {
    #[serde(default, rename = "OriginalArrivalTime")]
    pub original_arrival_time: Option<u64>,
}

/// Response from `/api/csa/api/v1/teams/users/me`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TeamsResponse {
    #[serde(default)]
    pub teams: Vec<Team>,
}

/// Trouter session negotiation response from `go.trouter.teams.microsoft.com`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TrouterSession {
    pub socketio: String,
    pub surl: String,
    pub url: String,
    #[serde(default)]
    pub ttl: u64,
    pub connectparams: TrouterConnectParams,
    #[serde(default)]
    pub ccid: Option<String>,
}

/// Query parameters for connecting to Trouter's Socket.IO.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TrouterConnectParams {
    pub sr: String,
    pub issuer: String,
    pub sp: String,
    pub se: String,
    pub st: String,
    pub sig: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_conversation_json() {
        let json = r#"{
            "id": "19:abc-123@thread.tacv2",
            "type": "Thread",
            "threadProperties": {
                "topic": "General Announcements",
                "lastjoinat": "1728287364000"
            },
            "lastMessage": {
                "id": "1728287364000",
                "imdisplayname": "Alice Smith",
                "content": "<p>Welcome team!</p>",
                "messagetype": "RichText/Html"
            }
        }"#;

        let conv: Conversation = serde_json::from_str(json).expect("valid conversation");
        assert_eq!(conv.id, "19:abc-123@thread.tacv2");
        assert!(conv.is_channel());
        assert_eq!(conv.display_name(), "General Announcements");
        assert_eq!(
            conv.last_message
                .as_ref()
                .map(|m| m.im_display_name.as_deref()),
            Some(Some("Alice Smith"))
        );
    }

    #[test]
    fn parses_messages_response() {
        let json = r#"{
            "messages": [
                {
                    "id": "1728287364000",
                    "from": "8:orgid:user-uuid-1",
                    "imdisplayname": "Bob Jones",
                    "content": "<p>Hello <b>world</b></p>",
                    "messagetype": "RichText/Html",
                    "properties": {
                        "emotions": [
                            {
                                "key": "like",
                                "users": [
                                    {"mri": "8:orgid:user-uuid-2", "time": 1728287370000}
                                ]
                            }
                        ]
                    }
                }
            ]
        }"#;

        let resp: MessagesResponse = serde_json::from_str(json).expect("valid response");
        assert_eq!(resp.messages.len(), 1);
        let msg = &resp.messages[0];
        assert_eq!(msg.id, "1728287364000");
        assert_eq!(msg.im_display_name.as_deref(), Some("Bob Jones"));
        let props = msg.properties.as_ref().expect("properties");
        let emotions = props.emotions.as_ref().expect("emotions");
        assert_eq!(emotions.len(), 1);
        assert_eq!(emotions[0].key, "like");
    }

    #[test]
    fn parses_trouter_session() {
        let json = r#"{
            "socketio": "https://emea.trouter.teams.microsoft.com:443/",
            "surl": "https://emea-01.trouter.teams.microsoft.com/v4/f/surl-id/",
            "url": "https://emea-01.trouter.teams.microsoft.com/v4/f/url-id/",
            "ttl": 86400,
            "ccid": "ccid-123",
            "connectparams": {
                "sr": "res",
                "issuer": "iss",
                "sp": "r",
                "se": "2026-10-08",
                "st": "2026-10-07",
                "sig": "signature"
            }
        }"#;

        let session: TrouterSession = serde_json::from_str(json).expect("valid session");
        assert_eq!(
            session.socketio,
            "https://emea.trouter.teams.microsoft.com:443/"
        );
        assert_eq!(session.ccid.as_deref(), Some("ccid-123"));
        assert_eq!(session.connectparams.issuer, "iss");
    }

    #[test]
    fn a_thread_names_its_members() {
        let thread: Thread = serde_json::from_str(
            r#"{"id":"19:x@thread.v2","type":"Thread","properties":{},"members":[
                {"id":"8:live:.cid.aaa","role":"Admin","linkedMri":null},
                {"id":"8:orgid:bbb","role":"User"}
            ]}"#,
        )
        .expect("a thread");
        let ids: Vec<&str> = thread.members.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(ids, ["8:live:.cid.aaa", "8:orgid:bbb"]);
    }
}
