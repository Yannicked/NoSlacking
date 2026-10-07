//! Trouter real-time WebSocket protocol handling.
//!
//! Handles Socket.IO v4 framing over WebSocket used by Microsoft Trouter,
//! session negotiation, registrar registration, automatic frame delivery
//! acknowledgments (which prevent server retry floods and 504 drops),
//! and parsing push notification payloads.

use serde::{Deserialize, Serialize};

/// An event decoded from a Trouter WebSocket frame.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum TrouterEvent {
    /// Server ping requiring pong response.
    Ping,
    /// A message posted, or one changed (edited, reacted to, deleted).
    Message {
        message: Box<crate::teams::types::Message>,
        changed: bool,
    },
    /// Other unparsed raw payload.
    Raw(String),
}

/// Connect parameters provided in Trouter session response.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConnectParams {
    #[serde(default)]
    pub sr: String,
    #[serde(default)]
    pub issuer: String,
    #[serde(default)]
    pub sp: String,
    #[serde(default)]
    pub se: String,
    #[serde(default)]
    pub st: String,
    #[serde(default)]
    pub sig: String,
}

/// Response returned by Trouter session negotiation (`https://go.trouter.teams.microsoft.com/v4/a`).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionResponse {
    #[serde(default)]
    pub socketio: String,
    #[serde(default)]
    pub surl: String,
    #[serde(default)]
    pub url: String,
    #[serde(default)]
    pub connectparams: ConnectParams,
    pub ccid: Option<String>,
    #[serde(default)]
    pub registrar_url: Option<String>,
}

impl SessionResponse {
    /// Builds the URL-encoded query string for `connectparams`.
    fn connectparams_query(&self) -> String {
        let cp = &self.connectparams;
        let enc = |s: &str| {
            percent_encoding::utf8_percent_encode(s, percent_encoding::NON_ALPHANUMERIC).to_string()
        };
        format!(
            "sr={}&issuer={}&sp={}&se={}&st={}&sig={}",
            enc(&cp.sr),
            enc(&cp.issuer),
            enc(&cp.sp),
            enc(&cp.se),
            enc(&cp.st),
            enc(&cp.sig),
        )
    }

    /// Builds the HTTP URL to obtain the Socket.IO session ID.
    pub fn session_url(&self, epid: &str) -> String {
        let host = self.socketio.trim_end_matches('/');
        let cp_query = self.connectparams_query();
        let enc = |s: &str| {
            percent_encoding::utf8_percent_encode(s, percent_encoding::NON_ALPHANUMERIC).to_string()
        };
        let tc =
            r#"{"cv":"TEAMS_TROUTER_TCCV","ua":"TeamsCDL","hr":"","v":"TEAMS_CLIENTINFO_VERSION"}"#;

        let mut url = format!(
            "{}/socket.io/1/?v=v4&{}&tc={}&con_num=0_1&auth=true&timeout=40&epid={}",
            host,
            cp_query,
            enc(tc),
            enc(epid),
        );

        if let Some(ref ccid) = self.ccid {
            url.push_str(&format!("&ccid={}", enc(ccid)));
        }

        url
    }

    /// Builds the WebSocket connection URL using the negotiated session ID.
    pub fn ws_url(&self, session_id: &str, epid: &str) -> String {
        let host = self.socketio.trim_end_matches('/');
        let cp_query = self.connectparams_query();
        let enc = |s: &str| {
            percent_encoding::utf8_percent_encode(s, percent_encoding::NON_ALPHANUMERIC).to_string()
        };
        let tc =
            r#"{"cv":"TEAMS_TROUTER_TCCV","ua":"TeamsCDL","hr":"","v":"TEAMS_CLIENTINFO_VERSION"}"#;

        let mut url = format!(
            "{}/socket.io/1/websocket/{}?v=v4&{}&tc={}&con_num=0_1&auth=true&timeout=40&epid={}",
            host,
            session_id,
            cp_query,
            enc(tc),
            enc(epid),
        );

        if let Some(ref ccid) = self.ccid {
            url.push_str(&format!("&ccid={}", enc(ccid)));
        }

        url.replace("https://", "wss://")
            .replace("http://", "ws://")
    }
}

/// Extracts the Trouter HTTP-over-WS request ID from a `3:::` data frame.
///
/// Trouter frames arrive as `3:::{"id":123,...}`. The client MUST respond
/// with `3:::{"id":123,"status":200}` to confirm delivery, otherwise Trouter drops
/// notifications with 504.
pub fn extract_trouter_request_id(frame: &str) -> Option<u64> {
    let body = frame.strip_prefix("3:::")?;
    let val: serde_json::Value = serde_json::from_str(body).ok()?;
    val.get("id").and_then(|v| v.as_u64())
}

/// Extracts the Socket.IO ack ID from an event frame like `5:42::`.
pub fn extract_socketio_ack_id(frame: &str) -> Option<String> {
    let rest = frame.strip_prefix("5:")?;
    let colons = rest.find("::")?;
    let id_str = &rest[..colons];
    if id_str.is_empty() {
        None
    } else {
        Some(id_str.to_string())
    }
}

/// Generates the appropriate response frame if `frame` requires an acknowledgment or pong.
pub fn handle_frame_control(frame: &str) -> Option<String> {
    if frame == "2" || frame == "2::" {
        return Some("2::".to_string());
    }

    if let Some(req_id) = extract_trouter_request_id(frame) {
        return Some(format!("3:::{{\"id\":{},\"status\":200}}", req_id));
    }

    if let Some(ack_id) = extract_socketio_ack_id(frame) {
        return Some(format!("6:{}::", ack_id));
    }

    None
}

/// Parses an incoming WebSocket text frame into a high-level [`TrouterEvent`].
///
/// A chat event comes as an HTTP request over the socket (`3:::{json}`)
/// whose `body` is the event as a string, plain JSON or gzip-compressed and
/// base64-encoded: `{"resourceType": "NewMessage", "resource": {…}}`.
pub fn parse_frame(frame: &str) -> Option<TrouterEvent> {
    if frame == "2" || frame == "2::" {
        return Some(TrouterEvent::Ping);
    }
    if let Some(body) = frame.strip_prefix("3:::")
        && let Ok(request) = serde_json::from_str::<serde_json::Value>(body)
    {
        return Some(
            request
                .get("body")
                .and_then(unpack)
                .and_then(|event| message_event(&event))
                .unwrap_or_else(|| TrouterEvent::Raw(body.to_owned())),
        );
    }
    if frame.starts_with("5:") {
        return Some(TrouterEvent::Raw(frame.to_owned()));
    }
    None
}

/// A request body as JSON: a string of JSON, or of gzip-compressed JSON in
/// base64, or JSON already.
pub fn unpack(body: &serde_json::Value) -> Option<serde_json::Value> {
    use base64::Engine as _;
    use std::io::Read as _;
    let text = match body {
        serde_json::Value::String(text) => text,
        serde_json::Value::Object(_) => return Some(body.clone()),
        _ => return None,
    };
    if let Ok(value) = serde_json::from_str(text) {
        return Some(value);
    }
    let compressed = base64::engine::general_purpose::STANDARD
        .decode(text.trim())
        .ok()?;
    let mut json = String::new();
    flate2::read::GzDecoder::new(compressed.as_slice())
        .read_to_string(&mut json)
        .ok()?;
    serde_json::from_str(&json).ok()
}

/// The message in a chat event, if it is one: a new message, or a changed
/// one (`MessageUpdate`: edits, reactions, deletions). Its conversation
/// is named only in its `conversationLink`.
fn message_event(event: &serde_json::Value) -> Option<TrouterEvent> {
    let kind = event.get("resourceType").and_then(|k| k.as_str());
    let changed = match kind {
        Some("NewMessage") => false,
        Some("MessageUpdate") => true,
        // A bare message, as some events carry it.
        None if event.get("id").is_some() => false,
        _ => return None,
    };
    let resource = event.get("resource").unwrap_or(event);
    let mut message: crate::teams::types::Message =
        serde_json::from_value(resource.clone()).ok()?;
    if message.id.is_empty() {
        return None;
    }
    if message.conversation_id.is_none() {
        message.conversation_id = resource
            .get("conversationLink")
            .and_then(|link| link.as_str())
            .and_then(conversation_of_link);
    }
    Some(TrouterEvent::Message {
        message: Box::new(message),
        changed,
    })
}

/// The conversation id at the end of a `…/conversations/{id}` link.
fn conversation_of_link(link: &str) -> Option<String> {
    let id = link.rsplit_once("/conversations/")?.1;
    let id = id.split([';', '?', '/']).next()?;
    (!id.is_empty()).then(|| id.to_owned())
}

/// Where a work account's live connection is negotiated.
pub const WORK_TROUTER: &str = "https://go.trouter.teams.microsoft.com";

/// Where a personal account's is: its chat events (`…/messaging`) and
/// presence come on Skype's Trouter, not Teams' (recorded).
pub const PERSONAL_TROUTER: &str = "https://go.trouter.skype.com";

/// Where a personal account's live connection is registered for chat
/// events.
const PERSONAL_REGISTRAR: &str = "https://edge.skype.com/registrar/prod/v2/registrations";

/// Negotiates a Trouter session at `host` (`{host}/v4/a`).
pub async fn negotiate_trouter(
    http: &reqwest::Client,
    host: &str,
    skype_token: &str,
    epid: &str,
) -> Result<SessionResponse, crate::failure::Failure> {
    let url = format!("{host}/v4/a?epid={epid}");

    let resp = http
        .get(&url)
        .header("X-Skypetoken", skype_token)
        .send()
        .await
        .map_err(|e| crate::failure::Failure::Network(e.without_url().to_string()))?;

    if !resp.status().is_success() {
        return Err(crate::failure::Failure::Http(resp.status().as_u16()));
    }

    resp.json::<SessionResponse>()
        .await
        .map_err(|e| crate::failure::Failure::Unexpected(e.to_string()))
}

/// Obtains the Socket.IO session ID from the negotiate response.
pub async fn obtain_session_id(
    http: &reqwest::Client,
    session: &SessionResponse,
    skype_token: &str,
    epid: &str,
) -> Result<String, crate::failure::Failure> {
    let url = session.session_url(epid);

    let resp = http
        .get(&url)
        .header("X-Skypetoken", skype_token)
        .send()
        .await
        .map_err(|e| crate::failure::Failure::Network(e.without_url().to_string()))?;

    if !resp.status().is_success() {
        return Err(crate::failure::Failure::Http(resp.status().as_u16()));
    }

    let text = resp
        .text()
        .await
        .map_err(|e| crate::failure::Failure::Unexpected(e.to_string()))?;

    let session_id = text
        .split(':')
        .next()
        .filter(|s| !s.is_empty())
        .ok_or_else(|| crate::failure::Failure::Unexpected("Empty session response".into()))?;

    Ok(session_id.to_string())
}

/// Registers a personal account's live connection for chat events, as the
/// personal web client does (recorded): the connection's own address,
/// with a slash, as `TeamsCDLWebWorker` in the `TFL` product.
pub async fn register_personal(
    http: &reqwest::Client,
    skype_token: &str,
    trouter_surl: &str,
) -> Result<(), crate::failure::Failure> {
    let payload = serde_json::json!({
        "clientDescription": {
            "appId": "TeamsCDLWebWorker",
            "aesKey": "",
            "languageId": "en-US",
            "platform": "chrome",
            "templateKey": "TeamsCDLWebWorker_2.6",
            "platformUIVersion": "1415/26091713344",
            "productContext": "TFL"
        },
        "registrationId": crate::model::new_client_msg_id(),
        "nodeId": "",
        "transports": {
            "TROUTER": [{
                "context": "",
                "path": format!("{}/", trouter_surl.trim_end_matches('/')),
                "ttl": 3600
            }]
        }
    });
    let resp = http
        .post(PERSONAL_REGISTRAR)
        .header("X-Skypetoken", skype_token)
        .header("X-MS-Migration", "True")
        .json(&payload)
        .send()
        .await
        .map_err(|e| crate::failure::Failure::Network(e.without_url().to_string()))?;
    if resp.status().is_success() {
        Ok(())
    } else {
        Err(crate::failure::Failure::Http(resp.status().as_u16()))
    }
}

/// Registers the endpoint with the Teams registrar service.
pub async fn register_endpoint(
    http: &reqwest::Client,
    skype_token: &str,
    registrar_url: &str,
    trouter_surl: &str,
) -> Result<(), crate::failure::Failure> {
    let url = registrar_url.trim_end_matches('/');

    let registrations = [
        ("TeamsCDLWebWorker", "TeamsCDLWebWorker_2.6", ""),
        ("SkypeSpacesWeb", "SkypeSpacesWeb_2.4", "SkypeSpacesWeb"),
    ];

    for (app_id, template_key, path_suffix) in registrations {
        let path = if path_suffix.is_empty() {
            trouter_surl.to_string()
        } else {
            format!("{}/{}", trouter_surl.trim_end_matches('/'), path_suffix)
        };

        let payload = serde_json::json!({
            "clientDescription": {
                "appId": app_id,
                "aesKey": "",
                "languageId": "en-US",
                "platform": "edge",
                "templateKey": template_key,
                "platformUIVersion": "49/1.0.0"
            },
            "registrationId": crate::model::new_client_msg_id(),
            "nodeId": "",
            "transports": {
                "TROUTER": [{
                    "context": "",
                    "path": path,
                    "ttl": 86400
                }]
            }
        });

        let resp = http
            .post(url)
            .header("X-Skypetoken", skype_token)
            .json(&payload)
            .send()
            .await
            .map_err(|e| crate::failure::Failure::Network(e.without_url().to_string()))?;

        if !resp.status().is_success() {
            log::warn!(
                "Teams registrar registration for {} returned status {}",
                app_id,
                resp.status()
            );
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ping_frame_replies_pong() {
        assert_eq!(handle_frame_control("2::"), Some("2::".into()));
        assert_eq!(parse_frame("2::"), Some(TrouterEvent::Ping));
    }

    #[test]
    fn trouter_data_frame_extracts_id_and_acks() {
        let frame = r#"3:::{"id":10042,"url":"/users/ME/conversations","body":{}}"#;
        assert_eq!(extract_trouter_request_id(frame), Some(10042));
        assert_eq!(
            handle_frame_control(frame),
            Some(r#"3:::{"id":10042,"status":200}"#.into())
        );
    }

    #[test]
    fn socketio_event_frame_extracts_ack_id() {
        let frame = "5:178::[\"event\",{\"foo\":\"bar\"}]";
        assert_eq!(extract_socketio_ack_id(frame), Some("178".into()));
        assert_eq!(handle_frame_control(frame), Some("6:178::".into()));
    }

    #[test]
    fn a_live_message_is_read_from_its_event() {
        // As Trouter delivers one: the event as a string in the body.
        let event = r#"{"time":"2026-10-07T13:41:03Z","type":"EventMessage","resourceType":"NewMessage","resource":{"id":"1791380463186","content":"hi","messagetype":"Text","imdisplayname":"Bob","from":"https://notifications.skype.net/v1/users/ME/contacts/8:live:bob","conversationLink":"https://notifications.skype.net/v1/users/ME/conversations/19:uni01_abc@thread.v2","properties":{"importance":"","subject":""}}}"#;
        let frame = format!(
            "3:::{}",
            serde_json::json!({ "id": 5, "method": "POST", "url": "/v4/f/x/messaging", "body": event })
        );
        match parse_frame(&frame).expect("parses") {
            TrouterEvent::Message { message, changed } => {
                assert_eq!(message.id, "1791380463186");
                assert_eq!(
                    message.conversation_id.as_deref(),
                    Some("19:uni01_abc@thread.v2")
                );
                assert!(!changed);
            }
            other => panic!("expected a message, got {other:?}"),
        }
    }

    #[test]
    fn a_compressed_update_is_unpacked_and_marked_changed() {
        use base64::Engine as _;
        use std::io::Write as _;
        // Reactions arrive as properties in text, as live events carry them.
        let event = r#"{"resourceType":"MessageUpdate","resource":{"id":"1","content":"hi","conversationLink":"https://x/v1/users/ME/conversations/19:a@thread.v2","properties":{"emotions":"[{\"key\":\"heart\",\"users\":[{\"mri\":\"8:live:bob\",\"time\":1}]}]"}}}"#;
        let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        gz.write_all(event.as_bytes()).expect("compresses");
        let body =
            base64::engine::general_purpose::STANDARD.encode(gz.finish().expect("compresses"));
        let frame = format!("3:::{}", serde_json::json!({ "id": 6, "body": body }));
        match parse_frame(&frame).expect("parses") {
            TrouterEvent::Message { message, changed } => {
                assert!(changed);
                let emotions = message
                    .properties
                    .and_then(|p| p.emotions)
                    .expect("reactions read from text");
                assert_eq!(emotions[0].key, "heart");
            }
            other => panic!("expected a message, got {other:?}"),
        }
    }

    #[test]
    fn other_events_are_left_raw() {
        let event = r#"{"resourceType":"ConversationUpdate","resource":{"id":"19:a@thread.v2"}}"#;
        let frame = format!("3:::{}", serde_json::json!({ "id": 7, "body": event }));
        assert!(matches!(parse_frame(&frame), Some(TrouterEvent::Raw(_))));
    }

    #[test]
    fn session_response_generates_correct_ws_url() {
        let session = SessionResponse {
            socketio: "https://trouter-emea.teams.microsoft.com".into(),
            surl: "https://emea-client-s.gateway.messenger.live.com/v1/users/ME/endpoints/ep1"
                .into(),
            url: "https://go.trouter.teams.microsoft.com".into(),
            connectparams: ConnectParams {
                sr: "test-sr".into(),
                issuer: "test-issuer".into(),
                sp: "connect".into(),
                se: "123456".into(),
                st: "123450".into(),
                sig: "mysig".into(),
            },
            ccid: Some("cc1".into()),
            registrar_url: Some("https://edge.skype.com/registrar/prod/v2/registrations".into()),
        };

        let ws_url = session.ws_url("session123", "epid-abc");
        assert!(ws_url.starts_with(
            "wss://trouter-emea.teams.microsoft.com/socket.io/1/websocket/session123"
        ));
        assert!(ws_url.contains("epid=epid%2Dabc") || ws_url.contains("epid=epid-abc"));
        assert!(ws_url.contains("ccid=cc1"));
        assert!(ws_url.contains("sr=test%2Dsr") || ws_url.contains("sr=test-sr"));
    }
}
