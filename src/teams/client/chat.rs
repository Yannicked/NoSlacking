//! Chats: their list, history, members, and what you send, edit, react
//! with and read in them, on the chat service; and new chats and
//! meetings made now.

use super::{PERSONAL_ORIGIN, TeamsClient, now_millis, refused, user_mri, why_unauthorized};
use crate::failure::Failure;
use crate::teams::auth::{Account, RESOURCE_GROUPS_PERSONAL, RESOURCE_MT_PERSONAL};
use crate::teams::types::{
    Conversation, ConversationsResponse, Message, MessagesResponse, PostedMessage,
};

/// Who a message is from, as the chat service's message bodies name them.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Author {
    /// The id messages name you by (`live:…` or an object id).
    pub id: String,
    /// Your name, which Teams shows with the message, if known.
    pub name: Option<String>,
}

/// A message as the Teams web client sends it, less what each call adds.
fn message_body(
    chat_id: &str,
    content: &crate::teams::html::Outgoing,
    me: &Author,
) -> serde_json::Value {
    let mri = user_mri(&me.id);
    let html_content = content.html.as_str();
    serde_json::json!({
        "type": "Message",
        "conversationid": chat_id,
        "from": mri,
        "fromUserId": mri,
        "content": html_content,
        "messagetype": "RichText/Html",
        "contenttype": "Text",
        "imdisplayname": me.name.clone().unwrap_or_default(),
        "amsreferences": [],
        "properties": {
            "importance": "",
            "subject": "",
            "title": "",
            "cards": "[]",
            "links": "[]",
            "mentions": content.mentions_json(),
            "files": "[]",
            "formatVariant": "TEAMS"
        }
    })
}

/// The chat service's `clientmessageid` for one of ours: the service
/// wants digits, where ours is a UUID, so it is the UUID's first 64 bits
/// as a number. The same UUID always gives the same number.
fn numeric_message_id(id: &str) -> String {
    if id.bytes().all(|b| b.is_ascii_digit()) {
        return id.to_owned();
    }
    let hex: String = id
        .chars()
        .filter(char::is_ascii_hexdigit)
        .take(16)
        .collect();
    u64::from_str_radix(&hex, 16).map_or_else(|_| id.to_owned(), |n| n.to_string())
}

/// Where a personal account's "Meet now" meetings are made (recorded).
const PERSONAL_MEET_NOW_URL: &str =
    "https://teams.live.com/api/mt/beta/me/calendarEvents/privateMeeting/schedulingService/create";

/// A meeting just made: where it is joined (and others invited), and its
/// chat.
#[derive(Clone, PartialEq, Eq)]
pub struct MadeMeeting {
    /// `https://teams.live.com/meet/{code}?p={token}`: lets anyone ask
    /// to join, so `Debug` does not show it.
    pub join: String,
    /// `19:meeting_…@thread.v2`.
    pub thread_id: String,
}

impl std::fmt::Debug for MadeMeeting {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MadeMeeting")
            .field("join", &crate::redact::REDACTED)
            .field("thread_id", &self.thread_id)
            .finish()
    }
}

/// Reads what making a meeting answered: `{value: {links: {join},
/// groupContext: {threadId}}}` (recorded).
fn made_meeting(answer: &serde_json::Value) -> Option<MadeMeeting> {
    let value = answer.get("value")?;
    let join = value.pointer("/links/join")?.as_str()?.to_owned();
    let thread_id = value
        .pointer("/groupContext/threadId")
        .and_then(|t| t.as_str())
        .unwrap_or_default()
        .to_owned();
    Some(MadeMeeting { join, thread_id })
}

/// Where a personal account's chats are started.
const PERSONAL_THREADS_URL: &str = "https://teams.live.com/api/groups/v1/threads";

/// The new chat's id, from the answer's body (the groups service:
/// `{"value":{"threadId":…}}`) or else its `Location` (the chat service:
/// `…/v1/threads/{id}`).
fn created_thread(body: &str, location: Option<&str>) -> Option<String> {
    serde_json::from_str::<serde_json::Value>(body)
        .ok()
        .and_then(|v| {
            v.pointer("/value/threadId")
                .and_then(|id| id.as_str())
                .map(str::to_owned)
        })
        .or_else(|| {
            location
                .and_then(|l| l.rsplit("/threads/").next())
                .filter(|id| id.starts_with("19:"))
                .map(str::to_owned)
        })
}

/// A page of a conversation's history, oldest first.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct HistoryPage {
    pub messages: Vec<Message>,
    /// Where the page before this one is, if there is one.
    pub older: Option<String>,
}

/// `link` if it is a page of the chat service at `base`: the skype token
/// goes with it, so a link elsewhere is refused rather than followed.
fn older_link<'a>(base: &str, link: &'a str) -> Result<&'a str, Failure> {
    let inside = link
        .strip_prefix(base)
        .is_some_and(|rest| rest.starts_with('/'));
    if inside {
        Ok(link)
    } else {
        Err(Failure::Unexpected(
            "older messages link is not on the chat service".into(),
        ))
    }
}

/// A messages answer as a page: control messages and deleted ones left
/// out, oldest first (Teams gives newest first), and the older page's link
/// only when there is something before this one.
fn history_page(data: MessagesResponse) -> HistoryPage {
    let older = data
        .metadata
        .and_then(|meta| meta.backward_link)
        .filter(|link| !link.is_empty() && !data.messages.is_empty());
    let mut messages: Vec<Message> = data
        .messages
        .into_iter()
        .filter(|m| {
            !m.message_type
                .as_deref()
                .is_some_and(|t| t.starts_with("Control/"))
                && !m.properties.as_ref().is_some_and(|p| p.is_deleted())
        })
        .collect();
    messages.reverse();
    HistoryPage { messages, older }
}

impl TeamsClient {
    /// Fetches recent conversations (chats and channel threads).
    pub async fn get_conversations(&self, limit: usize) -> Result<Vec<Conversation>, Failure> {
        let url = format!(
            "{}/v1/users/ME/conversations?view=mychats&pageSize={}",
            self.chat_service_url(),
            limit
        );
        let resp = self
            .authed_skype_request(|http, token| {
                http.get(&url)
                    .header("Authentication", format!("skypetoken={}", token))
            })
            .await?;
        if !resp.status().is_success() {
            return Err(Failure::Http(resp.status().as_u16()));
        }
        let data: ConversationsResponse = resp
            .json()
            .await
            .map_err(|e| Failure::Unexpected(e.to_string()))?;
        Ok(data.conversations)
    }

    /// Reads a page of a conversation's messages: the newest, or the one
    /// at `older`, a link an earlier page gave.
    pub async fn get_messages(
        &self,
        chat_id: &str,
        older: Option<&str>,
        limit: usize,
    ) -> Result<HistoryPage, Failure> {
        let base = self.chat_service_url();
        let url = match older {
            Some(link) => older_link(&base, link)?.to_owned(),
            None => format!(
                "{}/v1/users/ME/conversations/{}/messages?pageSize={}",
                base,
                crate::percent::encode_strict(chat_id),
                limit
            ),
        };
        self.messages_at(&url).await
    }

    /// The replies to the post `post` of the channel `channel`, newest
    /// first as any history, from its reply chain.
    pub async fn get_replies(
        &self,
        channel: &str,
        post: &str,
        limit: usize,
    ) -> Result<HistoryPage, Failure> {
        let url = format!(
            "{}/messages?pageSize={limit}",
            self.reply_chain_url(channel, post)
        );
        self.messages_at(&url).await
    }

    /// A page of messages from `url`.
    async fn messages_at(&self, url: &str) -> Result<HistoryPage, Failure> {
        let resp = self
            .authed_skype_request(|http, token| {
                http.get(url)
                    .header("Authentication", format!("skypetoken={}", token))
            })
            .await?;

        if !resp.status().is_success() {
            return Err(Failure::Http(resp.status().as_u16()));
        }

        let data: MessagesResponse = resp
            .json()
            .await
            .map_err(|e| Failure::Unexpected(e.to_string()))?;
        Ok(history_page(data))
    }

    /// The people in a chat, by MRI (`8:orgid:…`, `8:live:…`), from the
    /// thread's own record.
    pub async fn get_members(&self, chat_id: &str) -> Result<Vec<String>, Failure> {
        let url = format!(
            "{}/v1/threads/{}?view=msnp24Equivalent",
            self.chat_service_url(),
            crate::percent::encode_strict(chat_id)
        );
        let resp = self
            .authed_skype_request(|http, token| {
                http.get(&url)
                    .header("Authentication", format!("skypetoken={}", token))
            })
            .await?;
        if !resp.status().is_success() {
            return Err(refused(resp, "list a chat's members").await);
        }
        let thread: crate::teams::types::Thread = resp
            .json()
            .await
            .map_err(|e| Failure::Unexpected(e.to_string()))?;
        Ok(thread.members.into_iter().map(|m| m.id).collect())
    }

    /// Sends an HTML message to a conversation, answering with its id
    /// when the server gave one.
    pub async fn send_message(
        &self,
        chat_id: &str,
        reply_to: Option<&str>,
        content: &crate::teams::html::Outgoing,
        client_message_id: Option<&str>,
        me: &Author,
    ) -> Result<Option<String>, Failure> {
        // A reply goes to its post's reply chain, which names the post in
        // the conversation id too, as the web client writes it.
        let (url, conversation) = match reply_to {
            Some(post) => (
                format!("{}/messages", self.reply_chain_url(chat_id, post)),
                format!("{chat_id};messageid={post}"),
            ),
            None => (
                format!("{}/messages", self.conversation_url(chat_id)),
                chat_id.to_owned(),
            ),
        };

        let mut body = message_body(&conversation, content, me);
        if let Some(id) = client_message_id {
            body["clientmessageid"] = numeric_message_id(id).into();
        }

        let resp = self
            .authed_skype_request(|http, token| {
                http.post(&url)
                    .header("Authentication", format!("skypetoken={}", token))
                    .json(&body)
            })
            .await?;

        if !resp.status().is_success() {
            return Err(refused(resp, "send").await);
        }

        let posted: PostedMessage = resp.json().await.unwrap_or_default();
        Ok(posted.original_arrival_time.map(|ms| ms.to_string()))
    }

    /// Replaces a message's text, as the Teams web client edits: the
    /// message again with its new content and the time of the edit.
    pub async fn edit_message(
        &self,
        chat_id: &str,
        message_id: &str,
        content: &crate::teams::html::Outgoing,
        me: &Author,
    ) -> Result<(), Failure> {
        let url = format!("{}/messages/{}", self.conversation_url(chat_id), message_id);
        let mut body = message_body(chat_id, content, me);
        body["id"] = message_id.into();
        body["properties"]["edittime"] = now_millis().into();
        let resp = self
            .authed_skype_request(|http, token| {
                http.put(&url)
                    .header("Authentication", format!("skypetoken={}", token))
                    .json(&body)
            })
            .await?;
        if resp.status().is_success() {
            Ok(())
        } else {
            Err(refused(resp, "edit").await)
        }
    }

    /// Adds (or takes back) your reaction `key` (`like`, `heart`, …).
    pub async fn react(
        &self,
        chat_id: &str,
        message_id: &str,
        key: &str,
        add: bool,
    ) -> Result<(), Failure> {
        let url = format!(
            "{}/messages/{}/properties?name=emotions",
            self.conversation_url(chat_id),
            message_id
        );
        let body = if add {
            serde_json::json!({ "emotions": { "key": key, "value": now_millis() } })
        } else {
            serde_json::json!({ "emotions": { "key": key } })
        };
        let resp = self
            .authed_skype_request(|http, token| {
                let request = if add {
                    http.put(&url)
                } else {
                    http.delete(&url)
                };
                request
                    .header("Authentication", format!("skypetoken={}", token))
                    .json(&body)
            })
            .await?;
        if resp.status().is_success() {
            Ok(())
        } else {
            Err(refused(resp, if add { "react" } else { "take a reaction back" }).await)
        }
    }

    /// Deletes a message (soft-delete).
    pub async fn delete_message(&self, chat_id: &str, message_id: &str) -> Result<(), Failure> {
        let url = format!(
            "{}/messages/{}?behavior=softDelete",
            self.conversation_url(chat_id),
            message_id
        );

        let resp = self
            .authed_skype_request(|http, token| {
                http.delete(&url)
                    .header("Authentication", format!("skypetoken={}", token))
            })
            .await?;

        if resp.status().is_success() {
            Ok(())
        } else {
            Err(refused(resp, "delete").await)
        }
    }

    /// Updates read state (consumption horizon) for a conversation.
    pub async fn set_consumption_horizon(
        &self,
        chat_id: &str,
        message_id: &str,
    ) -> Result<(), Failure> {
        let url = format!(
            "{}/properties?name=consumptionhorizon",
            self.conversation_url(chat_id)
        );

        let body = serde_json::json!({
            "consumptionhorizon": format!("{};{};{}", message_id, message_id, message_id)
        });

        let resp = self
            .authed_skype_request(|http, token| {
                http.put(&url)
                    .header("Authentication", format!("skypetoken={}", token))
                    .json(&body)
            })
            .await?;

        if resp.status().is_success() {
            Ok(())
        } else {
            Err(refused(resp, "mark read").await)
        }
    }

    /// Takes up the chat `thread` of a meeting we are in, as the web
    /// client does once let in: accepts being a member, then turns its
    /// alerts on (recorded, a personal account in another organization's
    /// meeting). The chat service knows us in the thread only a few
    /// seconds after the join; until then this is `Failure::Http(404)`,
    /// which is not logged, so it can be tried again.
    pub async fn take_up_meeting_chat(&self, thread: &str) -> Result<(), Failure> {
        let me = self
            .user_from_token()
            .ok_or_else(|| Failure::Unexpected("who you are is not known".into()))?;
        let url = format!(
            "{}/v1/threads/{}/members/{}",
            self.chat_service_url(),
            crate::percent::encode_strict(thread),
            crate::percent::encode_strict(&user_mri(&me.id))
        );
        let resp = self
            .authed_skype_request(|http, token| {
                http.put(&url)
                    .header("Authentication", format!("skypetoken={token}"))
                    .json(&serde_json::json!({"relationshipState": "Accepted"}))
            })
            .await?;
        if resp.status() == reqwest::StatusCode::NOT_FOUND {
            return Err(Failure::Http(404));
        }
        if !resp.status().is_success() {
            return Err(refused(resp, "join the meeting's chat").await);
        }
        let url = format!("{}/properties?name=alerts", self.conversation_url(thread));
        let resp = self
            .authed_skype_request(|http, token| {
                http.put(&url)
                    .header("Authentication", format!("skypetoken={token}"))
                    .json(&serde_json::json!({"alerts": "true"}))
            })
            .await?;
        if resp.status().is_success() {
            Ok(())
        } else {
            Err(refused(resp, "turn the meeting chat's alerts on").await)
        }
    }

    /// Makes a meeting to start now ("Meet now") named `subject`, as the
    /// personal web client does (recorded, but for its sign-in header). Work accounts schedule
    /// theirs another way, not yet recorded.
    pub async fn meet_now(&self, subject: &str) -> Result<MadeMeeting, Failure> {
        let creds = self.ensure_fresh_tokens().await?;
        if creds.account != Account::Personal {
            return Err(Failure::Unsupported);
        }
        let body = serde_json::json!({
            "meetingType": "MeetNow",
            "isStreamEnabled": false,
            "subject": subject,
            "unhideChatThread": true,
        });
        // The middle tier's own token as well as the skype token, as its
        // other requests take: the recording was exported without its
        // `Authorization` header, and the skype token alone is refused.
        let skype = creds.skype_token.clone().unwrap_or_default();
        let resp = self
            .bearer(RESOURCE_MT_PERSONAL, |http, token| {
                crate::teams::auth::consumer_headers(
                    http.post(PERSONAL_MEET_NOW_URL)
                        .bearer_auth(token)
                        .header("x-skypetoken", &skype)
                        .header("x-ms-client-type", "web")
                        .header("x-ms-client-version", "1415/26091713344")
                        .header(reqwest::header::ORIGIN, PERSONAL_ORIGIN)
                        .header(reqwest::header::REFERER, format!("{PERSONAL_ORIGIN}/v2/"))
                        .json(&body),
                )
            })
            .await?;
        if !resp.status().is_success() {
            return Err(refused(resp, "make a meeting").await);
        }
        let answer: serde_json::Value = resp
            .json()
            .await
            .map_err(|e| Failure::Unexpected(e.without_url().to_string()))?;
        made_meeting(&answer).ok_or_else(|| Failure::Unexpected("no meeting in the answer".into()))
    }

    /// Starts a chat with `others` and you, answering its id: one other
    /// person makes a one-to-one chat, more a group.
    pub async fn create_chat(&self, me: &str, others: &[String]) -> Result<String, Failure> {
        let creds = self.ensure_fresh_tokens().await?;
        let members: Vec<serde_json::Value> = std::iter::once(me)
            .chain(others.iter().map(String::as_str))
            .map(|id| serde_json::json!({ "id": user_mri(id), "role": "User" }))
            .collect();
        let resp = match creds.account {
            // As the personal web client starts a chat (recorded).
            Account::Personal => {
                let skype = creds.skype_token.clone().unwrap_or_default();
                let body = serde_json::json!({
                    "members": members,
                    "properties": { "threadType": "chat", "isStickyThread": "true" },
                });
                let resp = self
                    .bearer(RESOURCE_GROUPS_PERSONAL, |http, token| {
                        crate::teams::auth::consumer_headers(
                            http.post(PERSONAL_THREADS_URL)
                                .bearer_auth(token)
                                .header("x-skypetoken", &skype)
                                .json(&body),
                        )
                    })
                    .await?;
                if resp.status() == reqwest::StatusCode::UNAUTHORIZED {
                    // The groups service cannot read our tokens: we sign in
                    // as the desktop app, whose personal tokens are opaque
                    // tickets, and only the web client's app (which cannot
                    // sign in by device code, AADSTS70002) gets the kind it
                    // reads ("S2S12008 … exception when validating the
                    // token"). The chat service takes our skype token, so
                    // the chat is made there, the Skype way (not recorded
                    // for personal accounts).
                    log::info!(
                        "the groups service refused to start a chat ({}); asking the chat service",
                        why_unauthorized(&resp)
                    );
                    // Its own properties, not the work ones: "Consumer users
                    // disallowed on uniquerosterthread" (HTTP 400).
                    self.create_chat_on_chat_service(&body).await?
                } else {
                    resp
                }
            }
            // The chat service's own way, which work clients have used:
            // not yet seen in a recording of the work web client.
            Account::Work => {
                let body = serde_json::json!({
                    "members": members,
                    "properties": { "threadType": "chat", "fixedRoster": "true", "uniquerosterthread": "true" },
                });
                self.create_chat_on_chat_service(&body).await?
            }
        };
        if !resp.status().is_success() {
            return Err(refused(resp, "start a chat").await);
        }
        let location = resp
            .headers()
            .get(reqwest::header::LOCATION)
            .and_then(|l| l.to_str().ok())
            .map(str::to_owned);
        let text = resp.text().await.unwrap_or_default();
        created_thread(&text, location.as_deref())
            .ok_or_else(|| Failure::Unexpected("no chat id in the answer".into()))
    }

    /// Asks the chat service itself to make the chat `body` describes
    /// (its members and properties), with the skype token.
    async fn create_chat_on_chat_service(
        &self,
        body: &serde_json::Value,
    ) -> Result<reqwest::Response, Failure> {
        let url = format!("{}/v1/threads", self.chat_service_url());
        self.authed_skype_request(|http, token| {
            http.post(&url)
                .header("Authentication", format!("skypetoken={}", token))
                .json(body)
        })
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn older_pages_must_be_on_the_chat_service() {
        let base = "https://emea.ng.msg.teams.microsoft.com";
        let good = "https://emea.ng.msg.teams.microsoft.com/v1/users/ME/conversations/x/messages?syncState=a";
        assert_eq!(older_link(base, good), Ok(good));
        for bad in [
            "https://evil.example/v1/messages",
            "https://emea.ng.msg.teams.microsoft.com.evil.example/v1",
            "",
        ] {
            assert!(older_link(base, bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn a_history_page_is_oldest_first_with_its_older_link() {
        let json = r#"{
            "messages": [
                {"id": "3", "messagetype": "RichText/Html", "content": "c"},
                {"id": "2", "messagetype": "Control/Typing", "content": ""},
                {"id": "1", "messagetype": "Text", "content": "a"}
            ],
            "_metadata": {"backwardLink": "https://chat/v1/older"}
        }"#;
        let page = history_page(serde_json::from_str(json).expect("valid page"));
        let ids: Vec<&str> = page.messages.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(ids, ["1", "3"]);
        assert_eq!(page.older.as_deref(), Some("https://chat/v1/older"));

        let empty = r#"{"messages": [], "_metadata": {"backwardLink": "https://chat/v1/older"}}"#;
        let page = history_page(serde_json::from_str(empty).expect("valid page"));
        assert_eq!(page.older, None);
    }

    #[test]
    fn a_new_chat_is_found_in_either_answer() {
        let groups =
            r#"{"value":{"threadId":"19:uni01_abc@thread.v2","membersStatus":[]},"Type":"x"}"#;
        assert_eq!(
            created_thread(groups, None).as_deref(),
            Some("19:uni01_abc@thread.v2")
        );
        let located = created_thread(
            "",
            Some("https://x.msg.teams.microsoft.com/v1/threads/19:abc@thread.v2"),
        );
        assert_eq!(located.as_deref(), Some("19:abc@thread.v2"));
        assert_eq!(created_thread("{}", None), None);
    }

    #[test]
    fn messages_go_out_as_the_web_client_sends_them() {
        let me = Author {
            id: "live:.cid.4a5b".into(),
            name: Some("Yan".into()),
        };
        let hi = crate::teams::html::wire_to_teams("hi");
        let body = message_body("19:x@thread.v2", &hi, &me);
        assert_eq!(body["from"], "8:live:.cid.4a5b");
        assert_eq!(body["imdisplayname"], "Yan");
        assert_eq!(body["messagetype"], "RichText/Html");
        assert_eq!(body["properties"]["formatVariant"], "TEAMS");
        assert_eq!(body["content"], "<p>hi</p>");
        assert_eq!(body["properties"]["mentions"], "[]");
        let nameless = message_body("19:x@thread.v2", &hi, &Author::default());
        assert_eq!(nameless["imdisplayname"], "");
    }

    #[test]
    fn client_message_ids_are_numbers() {
        let id = numeric_message_id("8d6f1c2a-0b3e-4f5a-9c7d-1e2f3a4b5c6d");
        assert!(id.bytes().all(|b| b.is_ascii_digit()), "{id}");
        assert_eq!(
            id,
            numeric_message_id("8d6f1c2a-0b3e-4f5a-9c7d-1e2f3a4b5c6d")
        );
        assert_eq!(
            id,
            u64::from_str_radix("8d6f1c2a0b3e4f5a", 16)
                .expect("hex")
                .to_string()
        );
        assert_eq!(numeric_message_id("12345"), "12345");
    }

    #[test]
    fn a_posted_message_gives_its_id() {
        let posted: PostedMessage =
            serde_json::from_str(r#"{"OriginalArrivalTime": 1700000000123}"#).expect("valid");
        assert_eq!(posted.original_arrival_time, Some(1700000000123));
    }

    #[test]
    fn a_meeting_made_now_answers_its_link_and_chat() {
        let answer = serde_json::json!({
            "value": {
                "subject": "Meeting with Ana",
                "groupContext": {"threadId": "19:meeting_ZmFrZQ@thread.v2"},
                "meetingUrl": "https://api.scheduler.teams.microsoft.com/teamsforlife/123",
                "links": {"join": "https://teams.live.com/meet/9312345678901?p=Fake", "update": "x"}
            },
            "type": "x"
        });
        let made = made_meeting(&answer).expect("a meeting");
        assert_eq!(
            made.join,
            "https://teams.live.com/meet/9312345678901?p=Fake"
        );
        assert_eq!(made.thread_id, "19:meeting_ZmFrZQ@thread.v2");
        assert!(!format!("{made:?}").contains("Fake"));
        assert_eq!(made_meeting(&serde_json::json!({"value": {}})), None);
    }
}
