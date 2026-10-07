//! The signalling requests of a Teams call (see `docs/research/teams-calls.md`
//! §A–C), on the sign-in's HTTP client and skype token, plus the relay
//! (TURN) servers and their credentials.
//!
//! The bodies are built by plain functions, so tests can hold them against
//! the web client's without a network; [`CallApi`] sends them with the
//! headers of §1.2. Nothing here logs a token, a TURN credential or a
//! callback URL (anyone holding one could push into the call): only what
//! was sent and the status it got.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use crate::failure::Failure;
use crate::teams::calling::links::{Callbacks, Scope};
use crate::teams::calling::types::{
    AcknowledgementLinks, AnswerDebug, CallInvitation, CancelDiagnostics, ConversationRequest,
    CpconvAnswer, CpconvRequest, Decline, EndpointMetadata, EndpointState, Leave, MediaContent,
    MuteState, OurMediaAnswer, Participant, Participants, RenegotiationAnswer, Roster,
    TransactionEnd, UpdateEndpointMetadata, UpdateEndpointState,
};
use crate::teams::client::{TeamsClient, refused};

/// Where a call starts (A.1).
pub const CPCONV_URL: &str = "https://api.flightproxy.skype.com/api/v2/cpconv";
/// Where the relay credentials come from.
pub const TRAP_TOKENS_URL: &str = "https://edge.skype.com/trap/tokens";
/// The Skype client configuration, which names the relay servers.
pub const SKYPE_CONFIG_URL: &str =
    "https://config.teams.microsoft.com/config/v1/Skype/1415_1.0.0.0";

/// What the web client's capabilities add up to (A.1); opaque, copied.
const ENDPOINT_CAPABILITIES: u64 = 73463;
const CLIENT_ENDPOINT_CAPABILITIES: u64 = 42876960;

/// The conversation callbacks the web client gives in `cpconv`.
const CONVERSATION_EVENTS: &[&str] = &[
    "conversationEnd",
    "conversationUpdate",
    "localParticipantUpdate",
    "addParticipantSuccess",
    "addParticipantFailure",
    "addModalitySuccess",
    "addModalityFailure",
    "confirmUnmute",
    "receiveMessage",
];

/// The call callbacks it gives.
const CALL_EVENTS: &[&str] = &[
    "progress",
    "mediaAnswer",
    "acceptance",
    "redirection",
    "end",
];

/// The callbacks for the media controller, in `cpconv` and every answer.
const MEDIA_CONTROLLER_EVENTS: &[&str] = &["controlVideoStreaming", "csrcInfo"];

/// The client the requests say they come from: the web client's string
/// (§1.2), with this system's name.
pub fn client_header() -> String {
    let os = match std::env::consts::OS {
        "macos" => "mac",
        "windows" => "windows",
        _ => "linux",
    };
    format!(
        "SkypeSpaces/1415/26090318549/os={os}; osVer=undefined; deviceType=computer; \
         browser=chrome; browserVer=154.0.0.0/TsCallingVersion=2026.34.01.12/\
         Ovb=930ed66546ba01d76d5ebd510d73c22663a5e437"
    )
}

/// The ids we make up for one call (§1.3).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CallIds {
    /// The chain id of every request: ours for an outgoing call, the
    /// notification's `callId` for an incoming one.
    pub call_id: String,
    /// Our endpoint id: the app session's, the one Trouter knows us by.
    pub endpoint_id: String,
    /// Our id in this call.
    pub participant_id: String,
    /// The callee's id in this call, which we give it.
    pub callee_participant_id: String,
    /// 32 upper-case hex digits, echoed in every SDP push.
    pub media_leg_id: String,
    /// Picks this call out of every callback push.
    pub call_agent_id: String,
}

impl CallIds {
    /// Fresh ids for an outgoing call from the endpoint `endpoint_id`.
    pub fn new(endpoint_id: &str) -> Self {
        let uuid = crate::model::new_client_msg_id;
        Self {
            call_id: uuid(),
            endpoint_id: endpoint_id.to_owned(),
            participant_id: uuid(),
            callee_participant_id: uuid(),
            media_leg_id: uuid().replace('-', "").to_uppercase(),
            call_agent_id: uuid(),
        }
    }
}

/// Us, as a call names us (§1.6), by the sign-in's own id and name.
pub fn own_participant(client: &TeamsClient, ids: &CallIds) -> Option<Participant> {
    let me = client.user_from_token()?;
    Some(Participant {
        id: crate::teams::client::user_mri(&me.id),
        display_name: Some(client.own_name().or(me.display_name).unwrap_or_default()),
        endpoint_id: Some(ids.endpoint_id.clone()),
        participant_id: Some(ids.participant_id.clone()),
        language_id: Some("en-us".to_owned()),
    })
}

/// The `cpconv` body for an audio-only call to `callee` with our `offer`
/// (A.1). Every callback gets a fresh link.
pub fn cpconv_request(
    me: &Participant,
    callee: &str,
    ids: &CallIds,
    callbacks: &Callbacks,
    offer: &str,
) -> CpconvRequest {
    let mut media_content = MediaContent::ours(offer.to_owned(), ids.media_leg_id.clone());
    media_content.required_features = Some("nonByPass".to_owned());
    media_content.client_location = Some("NL".to_owned());
    CpconvRequest {
        conversation_request: ConversationRequest {
            suppress_dialout: false,
            application_type: "TFL".to_owned(),
            roster: Roster {
                kind: "Delta".to_owned(),
                roster_update: callbacks.link(Scope::Conversation, "rosterUpdate"),
            },
            properties: serde_json::json!({
                "allowConversationWithoutHost": true,
                "enableGroupCallEventMessages": true,
                "enableGroupCallUpgradeMessage": false,
                "enableGroupCallMeetupGeneration": false
            }),
            links: callbacks.links(Scope::Conversation, CONVERSATION_EVENTS),
        },
        participants: Participants {
            from: me.clone(),
            to: vec![Participant {
                id: callee.to_owned(),
                participant_id: Some(ids.callee_participant_id.clone()),
                ..Participant::default()
            }],
        },
        endpoint_capabilities: ENDPOINT_CAPABILITIES,
        client_endpoint_capabilities: CLIENT_ENDPOINT_CAPABILITIES,
        endpoint_metadata: EndpointMetadata {
            holographic_capabilities: 3,
        },
        endpoint_state: EndpointState {
            endpoint_state_sequence_number: 1,
            ..EndpointState::default()
        },
        call_invitation: CallInvitation {
            call_modalities: vec!["Audio".to_owned()],
            links: callbacks.links(Scope::Call, CALL_EVENTS),
            client_content_for_media_controller: callbacks
                .links(Scope::Call, MEDIA_CONTROLLER_EVENTS),
            pstn_content: serde_json::json!({
                "emergencyCallCountry": "",
                "platformName": client_header(),
                "publicApiCall": false
            }),
            media_content,
            voicemail_settings: serde_json::json!({}),
        },
        participant_property_bag: serde_json::json!({
            "aiVoiceConsent": {"value": {"aiVoiceConsentValue": "0"}, "sequenceNumber": 0}
        }),
    }
}

/// Our answer to a renegotiation offer (C.1), for the media leg
/// `media_leg_id` the offer named.
pub fn renegotiation_answer(
    me: &Participant,
    ids: &CallIds,
    callbacks: &Callbacks,
    answer: &str,
    media_leg_id: &str,
) -> RenegotiationAnswer {
    let mut media_content = MediaContent::ours(answer.to_owned(), media_leg_id.to_owned());
    media_content.client_location = Some("NL".to_owned());
    RenegotiationAnswer {
        media_answer: OurMediaAnswer {
            call_modalities: vec!["Audio".to_owned()],
            sender: me.clone(),
            links: AcknowledgementLinks {
                media_acknowledgement: Some(callbacks.link(Scope::Call, "mediaAcknowledgement")),
            },
            client_content_for_media_controller: callbacks
                .links(Scope::Call, MEDIA_CONTROLLER_EVENTS),
            media_content,
        },
        debug_content: AnswerDebug {
            call_id: ids.call_id.clone(),
            endpoint_id: ids.endpoint_id.clone(),
        },
    }
}

/// Our mute state, numbered `sequence` (C.3).
pub fn endpoint_state(me: &Participant, sequence: u64, muted: bool) -> UpdateEndpointState {
    UpdateEndpointState {
        from: me.clone(),
        endpoint_state: EndpointState {
            endpoint_state_sequence_number: sequence,
            state: Some(MuteState { is_muted: muted }),
            ..EndpointState::default()
        },
    }
}

/// Our hang-up (C.5, recorded since the capture): while it still rings
/// (`connected` false) it is a cancel (487) that says how long it rang,
/// and names us without a participant id; once connected, a normal end.
pub fn leave_body(me: &Participant, connected: bool, rang_for: Duration) -> Leave {
    let mut from = me.clone();
    if !connected {
        from.participant_id = None;
    }
    let phrase = "CallEndReasonLocalUserInitiated".to_owned();
    Leave {
        participants: Participants {
            from,
            to: Vec::new(),
        },
        conversation_transaction_end: TransactionEnd {
            reason: Some("noError".to_owned()),
            code: 0,
            phrase: "ConversationEndNoModalityConnected".to_owned(),
            ..TransactionEnd::default()
        },
        call_transaction_end: TransactionEnd {
            code: if connected { 0 } else { 487 },
            sub_code: Some(0),
            phrase,
            result_categories: Some(vec!["Success".to_owned()]),
            call_quality_diagnostics_information: (!connected).then_some(CancelDiagnostics {
                cancelation_duration: rang_for.as_secs(),
            }),
            ..TransactionEnd::default()
        },
    }
}

/// Our decline of an incoming call (recorded): 603.
pub fn decline_body() -> Decline {
    Decline {
        call_end: TransactionEnd {
            code: 603,
            sub_code: Some(0),
            phrase: "CallEndReasonLocalUserInitiated".to_owned(),
            result_categories: Some(vec!["Success".to_owned()]),
            application_type: Some("TFL".to_owned()),
            ..TransactionEnd::default()
        },
    }
}

/// The requests of one call.
pub struct CallApi {
    client: TeamsClient,
    ids: CallIds,
    me: Participant,
    callbacks: Callbacks,
    /// The last endpoint state number sent; `cpconv` sends 1.
    sequence: AtomicU64,
    /// When the call started, for how long it rang if we hang up early.
    started: Instant,
}

impl std::fmt::Debug for CallApi {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CallApi")
            .field("call_id", &self.ids.call_id)
            .finish_non_exhaustive()
    }
}

impl CallApi {
    /// The requests of the call `ids`, from `me`, with callbacks under our
    /// Trouter `surl`.
    pub fn new(client: TeamsClient, ids: CallIds, me: Participant, surl: &str) -> Self {
        let callbacks = Callbacks::new(surl, &ids.call_agent_id);
        Self {
            client,
            ids,
            me,
            callbacks,
            sequence: AtomicU64::new(1),
            started: Instant::now(),
        }
    }

    /// The call's ids.
    pub fn ids(&self) -> &CallIds {
        &self.ids
    }

    /// Sends one request with the headers of §1.2, and answers the
    /// response if it succeeded. `what` names it in the log.
    async fn send(
        &self,
        method: reqwest::Method,
        url: &str,
        body: Option<serde_json::Value>,
        what: &str,
    ) -> Result<reqwest::Response, Failure> {
        let client_header = client_header();
        let resp = self
            .client
            .plain_skype_request(|http, token| {
                let request = http
                    .request(method.clone(), url)
                    .header("x-skypetoken", token)
                    .header("x-microsoft-skype-chain-id", &self.ids.call_id)
                    .header(
                        "x-microsoft-skype-message-id",
                        crate::model::new_client_msg_id(),
                    )
                    .header("x-microsoft-skype-client", &client_header)
                    .header("ms-teams-ring", "general");
                match &body {
                    Some(body) => request.json(body),
                    None => request,
                }
            })
            .await?;
        if resp.status().is_success() {
            log::info!("Teams call: {what}: HTTP {}", resp.status().as_u16());
            Ok(resp)
        } else {
            Err(refused(resp, what).await)
        }
    }

    /// Starts the call: rings `callee` (an MRI) with our SDP `offer`
    /// (A.1), and answers the conversation's links.
    pub async fn create_call(&self, callee: &str, offer: &str) -> Result<CpconvAnswer, Failure> {
        let body = cpconv_request(&self.me, callee, &self.ids, &self.callbacks, offer);
        let body = serde_json::to_value(&body).map_err(|e| Failure::Unexpected(e.to_string()))?;
        let resp = self
            .send(
                reqwest::Method::POST,
                CPCONV_URL,
                Some(body),
                "start a call",
            )
            .await?;
        resp.json::<CpconvAnswer>()
            .await
            .map_err(|e| Failure::Unexpected(e.without_url().to_string()))
    }

    /// Answers a renegotiation offer with our SDP `answer`, at the
    /// `mediaAnswer` link the offer gave (C.1).
    pub async fn answer_renegotiation(
        &self,
        media_answer_url: &str,
        answer: &str,
        media_leg_id: &str,
    ) -> Result<(), Failure> {
        let body = renegotiation_answer(&self.me, &self.ids, &self.callbacks, answer, media_leg_id);
        let body = serde_json::to_value(&body).map_err(|e| Failure::Unexpected(e.to_string()))?;
        self.send(
            reqwest::Method::POST,
            media_answer_url,
            Some(body),
            "answer a renegotiation",
        )
        .await
        .map(drop)
    }

    /// Says whether we are muted, at the conversation's
    /// `updateEndpointState` link, with a number above the last (C.3).
    pub async fn update_endpoint_state(&self, url: &str, muted: bool) -> Result<(), Failure> {
        let sequence = self.sequence.fetch_add(1, Ordering::SeqCst) + 1;
        let body = serde_json::to_value(endpoint_state(&self.me, sequence, muted))
            .map_err(|e| Failure::Unexpected(e.to_string()))?;
        self.send(
            reqwest::Method::POST,
            url,
            Some(body),
            "update the mute state",
        )
        .await
        .map(drop)
    }

    /// Sends our endpoint's metadata, at the conversation's
    /// `updateEndpointMetadata` link, as the web client does once the call
    /// is accepted (A.4).
    pub async fn update_endpoint_metadata(&self, url: &str) -> Result<(), Failure> {
        let body = UpdateEndpointMetadata {
            participants: Participants {
                from: self.me.clone(),
                to: Vec::new(),
            },
            endpoint_metadata: EndpointMetadata {
                holographic_capabilities: 3,
            },
        };
        let body = serde_json::to_value(&body).map_err(|e| Failure::Unexpected(e.to_string()))?;
        self.send(
            reqwest::Method::PUT,
            url,
            Some(body),
            "update the endpoint metadata",
        )
        .await
        .map(drop)
    }

    /// Hangs up: `POST` to the conversation's `leave` link, ringing or
    /// connected (recorded; answered 204). The server then pushes
    /// `call/end` and `conversation/conversationEnd`. The caller closes
    /// the media at once rather than waiting on this.
    pub async fn hang_up(&self, leave_url: &str, connected: bool) -> Result<(), Failure> {
        let body = serde_json::to_value(leave_body(&self.me, connected, self.started.elapsed()))
            .map_err(|e| Failure::Unexpected(e.to_string()))?;
        self.send(reqwest::Method::POST, leave_url, Some(body), "hang up")
            .await
            .map(drop)
    }

    /// Declines an incoming call: `DELETE` to the attach answer's
    /// `callInvitation.links.reject` (recorded; answered 202). `ids` must
    /// carry the notification's call id as its chain id.
    pub async fn decline(&self, reject_url: &str) -> Result<(), Failure> {
        let body =
            serde_json::to_value(decline_body()).map_err(|e| Failure::Unexpected(e.to_string()))?;
        self.send(
            reqwest::Method::DELETE,
            reject_url,
            Some(body),
            "decline a call",
        )
        .await
        .map(drop)
    }
}

/// The credentials for Microsoft's relay (TURN) servers.
#[derive(Clone, Default, PartialEq, Eq)]
pub struct RelayCredentials {
    pub realm: String,
    pub username: String,
    pub password: String,
    /// When they run out, as the service says it (seconds; unverified).
    pub expires: Option<u64>,
}

impl std::fmt::Debug for RelayCredentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RelayCredentials")
            .field("realm", &self.realm)
            .field("username", &"<redacted>")
            .field("password", &"<redacted>")
            .field("expires", &self.expires)
            .finish()
    }
}

/// Reads `trap/tokens`' answer: `{tokens: [{realm, username, password}],
/// expires}`.
pub fn read_relay_credentials(answer: &serde_json::Value) -> Option<RelayCredentials> {
    let token = answer.get("tokens")?.as_array()?.first()?;
    let text = |v: &serde_json::Value, key: &str| {
        v.get(key)
            .and_then(|s| s.as_str())
            .unwrap_or_default()
            .to_owned()
    };
    let username = text(token, "username");
    let password = text(token, "password");
    if username.is_empty() || password.is_empty() {
        return None;
    }
    let expires = answer
        .get("expires")
        .or_else(|| token.get("expires"))
        .and_then(|e| e.as_u64().or_else(|| e.as_str()?.parse().ok()));
    Some(RelayCredentials {
        realm: text(token, "realm"),
        username,
        password,
        expires,
    })
}

/// Fetches the relay credentials (`GET trap/tokens` with the skype token).
pub async fn relay_credentials(client: &TeamsClient) -> Result<RelayCredentials, Failure> {
    let resp = client
        .plain_skype_request(|http, token| {
            http.get(TRAP_TOKENS_URL)
                .header("X-Skypetoken", token)
                .header("x-microsoft-skype-client", client_header())
        })
        .await?;
    if !resp.status().is_success() {
        return Err(refused(resp, "hand out relay credentials").await);
    }
    let answer: serde_json::Value = resp
        .json()
        .await
        .map_err(|e| Failure::Unexpected(e.without_url().to_string()))?;
    read_relay_credentials(&answer)
        .ok_or_else(|| Failure::Unexpected("no relay credentials in the answer".into()))
}

/// Microsoft's relay (TURN) servers.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RelayServers {
    /// Host names (or addresses).
    pub hosts: Vec<String>,
    pub realm: String,
    pub udp_port: u16,
    pub tcp_port: u16,
    pub tls_port: u16,
}

impl Default for RelayServers {
    /// The servers the European personal web client was configured with
    /// (recorded), for when the configuration cannot be read.
    fn default() -> Self {
        Self {
            hosts: vec!["gateway-eu.az.relay.teams.cloud.microsoft".to_owned()],
            realm: "rtcmedia".to_owned(),
            udp_port: 3478,
            tcp_port: 443,
            tls_port: 443,
        }
    }
}

/// The `Turn` entry of the Skype client configuration, wherever it sits
/// in the tree, as an object or as JSON in a string.
pub fn read_relay_servers(config: &serde_json::Value) -> Option<RelayServers> {
    fn find(value: &serde_json::Value) -> Option<serde_json::Value> {
        match value {
            serde_json::Value::Object(map) => {
                if let Some(turn) = map.get("Turn") {
                    return match turn {
                        serde_json::Value::String(text) => serde_json::from_str(text).ok(),
                        other => Some(other.clone()),
                    };
                }
                map.values().find_map(find)
            }
            serde_json::Value::Array(items) => items.iter().find_map(find),
            _ => None,
        }
    }
    let turn = find(config)?;
    let hosts: Vec<String> = ["fqdns", "addresses"]
        .iter()
        .filter_map(|key| turn.get(*key)?.as_array())
        .flatten()
        .filter_map(|h| h.as_str().map(str::to_owned))
        .collect();
    if hosts.is_empty() {
        return None;
    }
    let fallback = RelayServers::default();
    let port = |key: &str, default: u16| {
        turn.get(key)
            .and_then(|p| p.as_u64())
            .and_then(|p| u16::try_from(p).ok())
            .unwrap_or(default)
    };
    Some(RelayServers {
        hosts,
        realm: turn
            .get("realm")
            .and_then(|r| r.as_str())
            .map_or(fallback.realm, str::to_owned),
        udp_port: port("udpPort", fallback.udp_port),
        tcp_port: port("tcpPort", fallback.tcp_port),
        tls_port: port("tlsPort", fallback.tls_port),
    })
}

/// Microsoft's relay servers, from the Skype client configuration, or the
/// recorded ones if it cannot be read.
pub async fn relay_servers(client: &TeamsClient) -> RelayServers {
    let answer = async {
        let resp = client
            .http()
            .get(SKYPE_CONFIG_URL)
            .send()
            .await
            .map_err(|e| e.without_url().to_string())?;
        if !resp.status().is_success() {
            return Err(format!("HTTP {}", resp.status().as_u16()));
        }
        resp.json::<serde_json::Value>()
            .await
            .map_err(|e| e.without_url().to_string())
    };
    match answer.await.map(|config| read_relay_servers(&config)) {
        Ok(Some(servers)) => servers,
        Ok(None) => {
            log::warn!("the Skype configuration names no relay servers; using the known ones");
            RelayServers::default()
        }
        Err(error) => {
            log::warn!(
                "could not read the Skype configuration ({error}); using the known relay servers"
            );
            RelayServers::default()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::teams::calling::links::read_push_path;

    const SURL: &str = "https://trouter.example:3443/v4/f/ID1/";

    fn ids() -> CallIds {
        CallIds::new("00000000-0000-4000-8000-000000000002")
    }

    fn me(ids: &CallIds) -> Participant {
        Participant {
            id: "8:live:me".into(),
            display_name: Some("Me Example".into()),
            endpoint_id: Some(ids.endpoint_id.clone()),
            participant_id: Some(ids.participant_id.clone()),
            language_id: Some("en-us".into()),
        }
    }

    fn fixture(text: &str) -> serde_json::Value {
        serde_json::from_str(text).expect("fixture")
    }

    /// Every key path of a JSON tree, leaves' values left out.
    fn shape(value: &serde_json::Value, at: &str, out: &mut Vec<String>) {
        if let serde_json::Value::Object(map) = value {
            for (key, child) in map {
                let path = format!("{at}/{key}");
                out.push(path.clone());
                // The links' values are URLs; their names are what matters.
                shape(child, &path, out);
            }
        }
    }

    #[test]
    fn ids_have_the_recorded_shapes() {
        let ids = ids();
        assert_eq!(ids.media_leg_id.len(), 32);
        assert!(
            ids.media_leg_id
                .bytes()
                .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_lowercase())
        );
        assert_eq!(ids.call_id.len(), 36);
        assert_ne!(ids.call_id, ids.call_agent_id);
    }

    #[test]
    fn our_cpconv_has_the_web_clients_shape() {
        let ids = ids();
        let callbacks = Callbacks::new(SURL, &ids.call_agent_id);
        let ours = cpconv_request(&me(&ids), "8:live:other", &ids, &callbacks, "v=0");
        let theirs = fixture(include_str!("fixtures/cpconv_request.json"));
        let mut want = Vec::new();
        shape(&theirs, "", &mut want);
        let mut got = Vec::new();
        shape(&serde_json::to_value(&ours).expect("writes"), "", &mut got);
        // What the web client sends that we leave out: nulls, and its
        // config's etag.
        let left_out = |path: &str| {
            theirs.pointer(path).is_some_and(serde_json::Value::is_null)
                || path.starts_with("/debugContent")
        };
        let missing: Vec<_> = want
            .iter()
            .filter(|p| !got.contains(p) && !left_out(p))
            .collect();
        assert!(missing.is_empty(), "missing {missing:#?}");
        let extra: Vec<_> = got.iter().filter(|p| !want.contains(p)).collect();
        assert!(extra.is_empty(), "extra {extra:#?}");

        // Every callback routes back to this call by its name.
        let json = serde_json::to_value(&ours).expect("writes");
        let link = json["callInvitation"]["links"]["mediaAnswer"]
            .as_str()
            .expect("link");
        let path = read_push_path(link).expect("routes");
        assert_eq!(path.call_agent_id, ids.call_agent_id);
        assert_eq!(path.event, "mediaAnswer");
        assert_eq!(
            json["callInvitation"]["mediaContent"]["mediaLegId"],
            ids.media_leg_id
        );
        assert_eq!(
            json["participants"]["to"][0]["participantId"],
            ids.callee_participant_id
        );
    }

    #[test]
    fn our_renegotiation_answer_has_the_web_clients_shape() {
        let ids = ids();
        let callbacks = Callbacks::new(SURL, &ids.call_agent_id);
        let ours = renegotiation_answer(&me(&ids), &ids, &callbacks, "v=0", "LEG");
        let theirs = fixture(include_str!("fixtures/renegotiation_answer.json"));
        let (mut want, mut got) = (Vec::new(), Vec::new());
        shape(&theirs, "", &mut want);
        shape(&serde_json::to_value(&ours).expect("writes"), "", &mut got);
        assert_eq!(want.len(), got.len());
        assert!(want.iter().all(|p| got.contains(p)), "{got:#?}");
        assert_eq!(ours.media_answer.media_content.media_leg_id, "LEG");
    }

    #[test]
    fn hanging_up_while_it_rings_is_a_cancel() {
        let ids = ids();
        let body = serde_json::to_value(leave_body(&me(&ids), false, Duration::from_millis(7400)))
            .expect("writes");
        assert!(body["participants"]["from"].get("participantId").is_none());
        assert_eq!(body["participants"]["from"]["endpointId"], ids.endpoint_id);
        assert_eq!(
            body["conversationTransactionEnd"],
            serde_json::json!({"reason": "noError", "code": 0, "phrase": "ConversationEndNoModalityConnected"})
        );
        assert_eq!(
            body["callTransactionEnd"],
            serde_json::json!({"code": 487, "subCode": 0,
                "phrase": "CallEndReasonLocalUserInitiated", "resultCategories": ["Success"],
                "callQualityDiagnosticsInformation": {"cancelationDuration": 7}})
        );
    }

    #[test]
    fn hanging_up_a_live_call_is_a_normal_end() {
        let ids = ids();
        let body = serde_json::to_value(leave_body(&me(&ids), true, Duration::from_secs(90)))
            .expect("writes");
        assert_eq!(
            body["participants"]["from"]["participantId"],
            ids.participant_id
        );
        assert_eq!(
            body["callTransactionEnd"],
            serde_json::json!({"code": 0, "subCode": 0,
                "phrase": "CallEndReasonLocalUserInitiated", "resultCategories": ["Success"]})
        );
    }

    #[test]
    fn a_decline_is_603() {
        assert_eq!(
            serde_json::to_value(decline_body()).expect("writes"),
            serde_json::json!({"callEnd": {"code": 603, "subCode": 0,
                "phrase": "CallEndReasonLocalUserInitiated", "resultCategories": ["Success"],
                "applicationType": "TFL"}})
        );
    }

    #[test]
    fn mute_numbers_rise() {
        let ids = ids();
        let body = endpoint_state(&me(&ids), 2, true);
        assert_eq!(body.endpoint_state.endpoint_state_sequence_number, 2);
        assert_eq!(
            body.endpoint_state.state,
            Some(MuteState { is_muted: true })
        );
    }

    #[test]
    fn relay_credentials_are_read_and_kept_out_of_the_log() {
        let answer = serde_json::json!({
            "tokens": [{"realm": "rtcmedia", "username": "user-secret", "password": "pass-secret"}],
            "expires": 3600
        });
        let creds = read_relay_credentials(&answer).expect("reads");
        assert_eq!(creds.realm, "rtcmedia");
        assert_eq!(creds.expires, Some(3600));
        let printed = format!("{creds:?}");
        assert!(!printed.contains("secret"), "{printed}");
        assert_eq!(
            read_relay_credentials(&serde_json::json!({"tokens": []})),
            None
        );
    }

    #[test]
    fn relay_servers_are_found_in_the_configuration() {
        let config = serde_json::json!({"Skype": {"Turn": {
            "addresses": ["relay.example"], "realm": "rtcmedia",
            "udpPort": 3478, "tcpPort": 443, "tlsPort": 443}}});
        let servers = read_relay_servers(&config).expect("reads");
        assert_eq!(servers.hosts, ["relay.example"]);
        assert_eq!(
            (servers.udp_port, servers.tcp_port, servers.tls_port),
            (3478, 443, 443)
        );

        let as_text = serde_json::json!({"Turn": "{\"fqdns\":[\"a.example\"],\"udpPort\":3479}"});
        let servers = read_relay_servers(&as_text).expect("reads");
        assert_eq!(servers.hosts, ["a.example"]);
        assert_eq!(servers.udp_port, 3479);
        assert_eq!(servers.realm, "rtcmedia");

        assert_eq!(read_relay_servers(&serde_json::json!({"Other": {}})), None);
    }
}
