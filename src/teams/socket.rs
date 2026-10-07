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
    /// Message or conversation update.
    Message(Box<crate::teams::types::Message>),
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
pub fn parse_frame(frame: &str) -> Option<TrouterEvent> {
    if frame == "2" || frame == "2::" {
        return Some(TrouterEvent::Ping);
    }

    if let Some(body) = frame.strip_prefix("3:::")
        && let Ok(val) = serde_json::from_str::<serde_json::Value>(body)
    {
        // Trouter payload often wraps in body.body, body.params, or body.resource
        let inner = val
            .get("body")
            .or_else(|| val.get("resource"))
            .unwrap_or(&val);
        if let Ok(msg) = serde_json::from_value::<crate::teams::types::Message>(inner.clone())
            && !msg.id.is_empty()
        {
            return Some(TrouterEvent::Message(Box::new(msg)));
        }
        return Some(TrouterEvent::Raw(body.to_string()));
    }

    if frame.starts_with("5:") {
        if let Some(pos) = frame.find("::{") {
            let json_str = &frame[pos + 2..];
            if let Ok(val) = serde_json::from_str::<serde_json::Value>(json_str) {
                let inner = val
                    .get("body")
                    .or_else(|| val.get("resource"))
                    .unwrap_or(&val);
                if let Ok(msg) =
                    serde_json::from_value::<crate::teams::types::Message>(inner.clone())
                    && !msg.id.is_empty()
                {
                    return Some(TrouterEvent::Message(Box::new(msg)));
                }
            }
        }
        return Some(TrouterEvent::Raw(frame.to_string()));
    }

    None
}

/// Negotiates a Trouter session via `https://go.trouter.teams.microsoft.com/v4/a`.
pub async fn negotiate_trouter(
    http: &reqwest::Client,
    skype_token: &str,
    epid: &str,
) -> Result<SessionResponse, crate::failure::Failure> {
    let url = format!("https://go.trouter.teams.microsoft.com/v4/a?epid={}", epid);

    let resp = http
        .get(&url)
        .header("X-Skypetoken", skype_token)
        .send()
        .await
        .map_err(|e| crate::failure::Failure::Network(e.to_string()))?;

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
        .map_err(|e| crate::failure::Failure::Network(e.to_string()))?;

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
            .map_err(|e| crate::failure::Failure::Network(e.to_string()))?;

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
    fn parses_nested_message_in_trouter_frame() {
        let frame = r#"3:::{"id":5,"body":{"id":"1728287364000","content":"<p>hi</p>","imdisplayname":"Bob"}}"#;
        let event = parse_frame(frame).expect("parses");
        match event {
            TrouterEvent::Message(msg) => {
                assert_eq!(msg.id, "1728287364000");
                assert_eq!(msg.im_display_name.as_deref(), Some("Bob"));
            }
            _ => panic!("expected Message event"),
        }
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
