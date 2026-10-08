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
    AcceptAnswer, AcceptanceAcknowledgement, AcknowledgementLinks, AnswerDebug, AttachAnswer,
    CallInvitation, CancelDiagnostics, ConversationRequest, CpconvAnswer, CpconvRequest, Decline,
    EndpointMetadata, EndpointState, Leave, MediaContent, MuteState, OurMediaAnswer, Participant,
    Participants, RenegotiationAnswer, Roster, TransactionEnd, UpdateEndpointMetadata,
    UpdateEndpointState,
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

/// The conversation callbacks the web client gives when it joins an
/// incoming call's conversation (B.2).
const JOIN_EVENTS: &[&str] = &[
    "conversationEnd",
    "conversationUpdate",
    "localParticipantUpdate",
    "addParticipantSuccess",
    "addParticipantFailure",
    "receiveMessage",
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

impl CallIds {
    /// The ids of an incoming call: the notification's `call_id` as the
    /// chain id and the `participant_id` it gave us; the rest fresh.
    pub fn incoming(endpoint_id: &str, call_id: &str, participant_id: &str) -> Self {
        Self {
            call_id: call_id.to_owned(),
            participant_id: participant_id.to_owned(),
            ..Self::new(endpoint_id)
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

/// The `meetingData` of a meeting's first `cpconv` (§H.2): what was
/// typed, or the link's parts.
pub fn meeting_data(meeting: &crate::meetings::Meeting) -> serde_json::Value {
    serde_json::json!({
        "meetingCode": meeting.code(),
        "passcode": meeting.passcode(),
        "meetingUrl": meeting.url(),
    })
}

/// A meeting's `mediaDescriptions` (§H.8): our camera's line (`camera`,
/// its mid) sending and receiving while `camera_on`, else receiving only,
/// more camera lines (`more`) and the share's line receiving, numbered
/// `request_id`, which rises with each.
pub fn media_descriptions(
    camera: Option<&str>,
    camera_on: bool,
    more: &[&str],
    share: Option<&str>,
    request_id: u32,
) -> serde_json::Value {
    let mut descriptions = Vec::new();
    if let Some(mid) = camera {
        descriptions.push(if camera_on {
            serde_json::json!({"mid": mid, "direction": "sendrecv", "label": "main-video"})
        } else {
            serde_json::json!({"mid": mid, "direction": "recvonly"})
        });
    }
    for mid in more {
        descriptions.push(serde_json::json!({"mid": mid, "direction": "recvonly"}));
    }
    if let Some(mid) = share {
        descriptions.push(serde_json::json!({"mid": mid, "direction": "recvonly"}));
    }
    serde_json::json!({"descriptions": descriptions, "requestId": request_id})
}

/// The `mediaDescriptions` for an SDP of ours or the far end's: its
/// camera and share lines, the camera as `camera_on` says.
pub fn descriptions_for(sdp: &str, camera_on: bool, request_id: u32) -> serde_json::Value {
    let media = crate::teams::calling::sdp::read(sdp).unwrap_or_default();
    let more: Vec<&str> = media
        .other_cameras()
        .into_iter()
        .map(|l| l.mid.as_str())
        .collect();
    media_descriptions(
        media.camera().map(|l| l.mid.as_str()),
        camera_on,
        &more,
        media.share().map(|l| l.mid.as_str()),
        request_id,
    )
}

/// What a meeting's `cpconv` sends (§H.2): first without an offer, a
/// "preheat" that asks the meeting service for the conversation, then,
/// to `conversationController`, with our SDP `offer` to join the call.
/// `meeting_data` is the meeting's code, passcode and link: as typed for
/// the first, as the first answered for the second (recorded).
pub fn meeting_request(
    me: &Participant,
    ids: &CallIds,
    callbacks: &Callbacks,
    meeting_data: &serde_json::Value,
    offer: Option<&str>,
) -> serde_json::Value {
    // The join hands over the callbacks of one in the call; the preheat
    // those of one joining a conversation.
    let events = if offer.is_some() {
        CONVERSATION_EVENTS
    } else {
        JOIN_EVENTS
    };
    let mut conversation = serde_json::json!({
        "subject": null,
        "applicationType": "TFL",
        "roster": {
            "type": "Delta",
            "rosterUpdate": callbacks.link(Scope::Conversation, "rosterUpdate"),
        },
        "properties": {
            "allowConversationWithoutHost": true,
            "enableGroupCallEventMessages": true,
            "enableGroupCallUpgradeMessage": false,
            "enableGroupCallMeetupGeneration": false,
        },
        "links": callbacks.links(Scope::Conversation, events),
    });
    let mut endpoint_properties = serde_json::json!({
        "additionalEndpointProperties": {"infoShownInReportMode": "FullInformation"},
    });
    let mut body = serde_json::json!({
        "groupContext": null,
        "groupChat": null,
        "participants": {"from": me},
        "capabilities": null,
        "endpointCapabilities": ENDPOINT_CAPABILITIES,
        "clientEndpointCapabilities": CLIENT_ENDPOINT_CAPABILITIES,
        "endpointMetadata": {"holographicCapabilities": 3},
        "meetingInfo": null,
        "meetingData": meeting_data,
        "meetingPreferences": {"shouldResurrect": "resurrect"},
    });
    if let Some(offer) = offer {
        conversation["suppressDialout"] = true.into();
        // Joined "preheated": the call is set up before you are shown in
        // it, which `preheat_done` then ends.
        endpoint_properties["preheatProperties"] = 1.into();
        body["participants"]["to"] = serde_json::json!([]);
        let mut media_content = MediaContent::ours(offer.to_owned(), ids.media_leg_id.clone());
        media_content.client_location = Some("NL".to_owned());
        // Receiving on the video lines, the camera off: the meeting
        // sends video by this (§H.8).
        media_content.media_descriptions = Some(descriptions_for(offer, false, 1));
        body["callInvitation"] = serde_json::json!({
            "callModalities": crate::teams::calling::sdp::modalities(offer),
            "links": callbacks.links(Scope::Call, CALL_EVENTS),
            "clientContentForMediaController": callbacks.links(Scope::Call, MEDIA_CONTROLLER_EVENTS),
            "pstnContent": {
                "emergencyCallCountry": "",
                "platformName": client_header(),
                "publicApiCall": false,
            },
            "mediaContent": media_content,
            "voicemailSettings": {},
        });
        body["participantPropertyBag"] = serde_json::json!({
            "aiVoiceConsent": {"value": {"aiVoiceConsentValue": "0"}, "sequenceNumber": 0}
        });
    }
    body["conversationRequest"] = conversation;
    body["endpointState"] = serde_json::json!({
        "endpointStateSequenceNumber": 0,
        "endpointProperties": endpoint_properties,
    });
    body
}

/// What `POST {admit}` sends to let `mri` in from a meeting's lobby
/// (recorded; answered 202, then `admitParticipantSuccess` and a roster
/// with them in the call).
pub fn admit_body(me: &Participant, callbacks: &Callbacks, mri: &str) -> serde_json::Value {
    serde_json::json!({
        "participants": {"from": me, "to": [{"id": mri}]},
        "links": callbacks.links(Scope::Conversation, &["admitFailure", "admitSuccess"]),
        "debugContent": {"causeId": crate::model::new_client_msg_id()},
    })
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
            call_modalities: crate::teams::calling::sdp::modalities(answer),
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

/// The callbacks the web client hands over when it acknowledges a
/// pickup: where the far end may renegotiate, transfer and so on.
const ACKNOWLEDGED_EVENTS: &[&str] = &[
    "mediaRenegotiation",
    "transfer",
    "replacement",
    "balanceUpdate",
    "retargetCompletion",
    "controlVideoStreaming",
    "updateMediaDescriptions",
];

/// Our acknowledgement of the far end picking up, posted to the
/// acceptance's `acknowledgement` link. Without it Teams takes the call as
/// never set up and drops it a while into the talking.
pub fn acceptance_acknowledgement(callbacks: &Callbacks) -> serde_json::Value {
    serde_json::json!({
        "callAcceptanceAcknowledgement": {
            "links": callbacks.links(Scope::Call, ACKNOWLEDGED_EVENTS),
        }
    })
}

/// What `POST {attach}` sends for an incoming call (B.2): ties our
/// endpoint to the ringing leg and joins the `conversation` in one go.
pub fn attach_request(
    me: &Participant,
    callbacks: &Callbacks,
    conversation: &str,
) -> serde_json::Value {
    serde_json::json!({
        "attach": {
            "requireMediaContent": false,
            "links": {"end": callbacks.link(Scope::Call, "end")},
            "locationContent": null,
            "networkContent": null,
            "areaContent": null,
            "applicationType": "TFL",
        },
        "capabilities": null,
        "endpointCapabilities": ENDPOINT_CAPABILITIES,
        "additionalActions": [{
            "input": {
                "capabilities": null,
                "endpointCapabilities": ENDPOINT_CAPABILITIES,
                "conversationRequest": {
                    "applicationType": "TFL",
                    "roster": {
                        "type": "Delta",
                        "rosterUpdate": callbacks.link(Scope::Conversation, "rosterUpdate"),
                    },
                    "links": callbacks.links(Scope::Conversation, JOIN_EVENTS),
                },
                "endpointMetadata": {},
                "participants": {"from": me},
            },
            "name": "join",
            "url": conversation,
            "waitForResponse": true,
        }],
    })
}

/// What `POST {progress}` sends while an incoming call rings here (B.3):
/// the caller hears it ring.
pub fn ringing_body(me: &Participant) -> serde_json::Value {
    serde_json::json!({
        "callProgress": {"sender": me, "status": "ringing", "phrase": "ringing"}
    })
}

/// What `POST {acceptance}` sends when we pick up (B.5): our `answer` to
/// the caller's offer on their media leg, and our callbacks.
pub fn acceptance_body(
    me: &Participant,
    callbacks: &Callbacks,
    answer: &str,
    media_leg_id: &str,
) -> serde_json::Value {
    let mut media_content = MediaContent::ours(answer.to_owned(), media_leg_id.to_owned());
    media_content.client_location = Some("NL".to_owned());
    serde_json::json!({
        "callAcceptance": {
            "acceptedBy": me,
            "acceptedCallModalities": ["Audio"],
            "capabilities": null,
            "endpointCapabilities": ENDPOINT_CAPABILITIES,
            "clientEndpointCapabilities": CLIENT_ENDPOINT_CAPABILITIES,
            "links": callbacks.links(Scope::Call, ACKNOWLEDGED_EVENTS),
            "clientContentForMediaController": callbacks.links(Scope::Call, MEDIA_CONTROLLER_EVENTS),
            "mediaContent": media_content,
            "pstnContent": {
                "emergencyCallCountry": "",
                "platformName": client_header(),
                "publicApiCall": false,
            },
            "callKeepAliveInterval": null,
            "applicationType": "TFL",
        }
    })
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

    /// For an incoming call, whose first mute state is numbered 1: no
    /// `cpconv` sent one.
    #[must_use]
    pub fn incoming(self) -> Self {
        Self {
            sequence: AtomicU64::new(0),
            ..self
        }
    }

    /// For a meeting, whose `cpconv`s number the endpoint state 0: the
    /// first state sent after is 1.
    #[must_use]
    pub fn meeting(self) -> Self {
        self.incoming()
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
                    // An empty JSON request, as the web client sends an
                    // acknowledgement (recorded).
                    None if method != reqwest::Method::GET => request
                        .header(reqwest::header::CONTENT_TYPE, "application/json")
                        .header(reqwest::header::CONTENT_LENGTH, "0"),
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

    /// Asks the meeting service for `meeting`'s conversation, without
    /// joining its call yet (§H.2): answers its controller, its links and
    /// the meeting's own data, which [`Self::join_meeting`] takes.
    pub async fn preheat(
        &self,
        meeting: &crate::meetings::Meeting,
    ) -> Result<CpconvAnswer, Failure> {
        let body = meeting_request(
            &self.me,
            &self.ids,
            &self.callbacks,
            &meeting_data(meeting),
            None,
        );
        let resp = self
            .send(
                reqwest::Method::POST,
                CPCONV_URL,
                Some(body),
                "find a meeting",
            )
            .await?;
        resp.json::<CpconvAnswer>()
            .await
            .map_err(|e| Failure::Unexpected(e.without_url().to_string()))
    }

    /// Joins the call of the meeting `preheated` found, with our SDP
    /// `offer` (§H.3). The meeting's answer comes as a `call/acceptance`
    /// push: from its lobby, or from the call itself.
    pub async fn join_meeting(
        &self,
        preheated: &CpconvAnswer,
        offer: &str,
    ) -> Result<CpconvAnswer, Failure> {
        let meeting_data = preheated.meeting_data.clone().unwrap_or_default();
        let body = meeting_request(
            &self.me,
            &self.ids,
            &self.callbacks,
            &meeting_data,
            Some(offer),
        );
        let resp = self
            .send(
                reqwest::Method::POST,
                &preheated.conversation_controller,
                Some(body),
                "join a meeting",
            )
            .await?;
        resp.json::<CpconvAnswer>()
            .await
            .map_err(|e| Failure::Unexpected(e.without_url().to_string()))
    }

    /// Ends the "preheat" of a meeting just joined, at the conversation's
    /// `updateEndpointState` link: shown in it from now on (recorded,
    /// sent right after the join).
    pub async fn preheat_done(&self, url: &str) -> Result<(), Failure> {
        let sequence = self.sequence.fetch_add(1, Ordering::SeqCst) + 1;
        let body = serde_json::json!({
            "from": self.me,
            "endpointState": {
                "endpointStateSequenceNumber": sequence,
                "endpointProperties": {"preheatProperties": 0},
            },
        });
        self.send(reqwest::Method::POST, url, Some(body), "end the preheat")
            .await
            .map(drop)
    }

    /// Says what our camera can send, at a meeting call leg's
    /// `applyChannelParameters` link, before it goes on (recorded).
    pub async fn camera_capabilities(&self, url: &str, mid: &str) -> Result<(), Failure> {
        let caps = serde_json::json!({
            "maxVideoSendCapabilities": {"caps": {
                "max-width": 1280, "max-height": 1280, "max-fps": 30,
                "max-streams": 1, "max-layers": 1, "sequence-number": 1,
            }}
        });
        let body = serde_json::json!({
            "applyChannelParameters": {"multiChannelParameter": {
                "mids": [mid],
                "mediaParameter": caps.to_string(),
            }}
        });
        self.send(
            reqwest::Method::POST,
            url,
            Some(body),
            "say what the camera sends",
        )
        .await
        .map(drop)
    }

    /// Changes the video lines' use in a meeting, at the call leg's
    /// `updateMediaDescriptions` link: `descriptions` from
    /// [`media_descriptions`], tagged with the `number` of the change.
    pub async fn update_media_descriptions(
        &self,
        url: &str,
        mut descriptions: serde_json::Value,
        number: u32,
    ) -> Result<(), Failure> {
        descriptions["negotiationTag"] = format!(
            "{};v_{number}",
            self.me.participant_id.as_deref().unwrap_or_default()
        )
        .into();
        let body =
            serde_json::json!({"UpdateMediaDescriptions": {"mediaDescriptions": descriptions}});
        self.send(
            reqwest::Method::POST,
            url,
            Some(body),
            "change the video lines",
        )
        .await
        .map(drop)
    }

    /// Lets `mri` in from the meeting's lobby, at the conversation's
    /// `admit` link.
    pub async fn admit(&self, url: &str, mri: &str) -> Result<(), Failure> {
        let body = admit_body(&self.me, &self.callbacks, mri);
        self.send(reqwest::Method::POST, url, Some(body), "admit someone")
            .await
            .map(drop)
    }

    /// Answers a renegotiation offer with our SDP `answer`, at the
    /// `mediaAnswer` link the offer gave (C.1).
    pub async fn answer_renegotiation(
        &self,
        media_answer_url: &str,
        answer: &str,
        media_leg_id: &str,
        descriptions: Option<serde_json::Value>,
    ) -> Result<(), Failure> {
        let mut body =
            renegotiation_answer(&self.me, &self.ids, &self.callbacks, answer, media_leg_id);
        body.media_answer.media_content.media_descriptions = descriptions;
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

    /// Starts a renegotiation of ours (§C.6): our `offer` on the media leg
    /// `media_leg_id`, posted to the far end's `mediaRenegotiation` link
    /// as the web client's `StartRenegotiation` does. Its answer comes as
    /// a `call/mediaAnswer` push to the link given here, a refusal to
    /// `call/rejection`; the HTTP answer says nothing.
    ///
    /// `share` numbers the screen share it starts or stops, as the web
    /// client tags its offers (`{participant id};ss_{n}`, the same `n` for
    /// a share's start and its stop; recorded).
    pub async fn renegotiate(
        &self,
        url: &str,
        offer: &str,
        media_leg_id: &str,
        share: u32,
    ) -> Result<(), Failure> {
        let mut media_content = MediaContent::ours(offer.to_owned(), media_leg_id.to_owned());
        media_content.client_location = Some("NL".to_owned());
        media_content.required_features = Some("nonByPass".to_owned());
        let mut media_content =
            serde_json::to_value(media_content).map_err(|e| Failure::Unexpected(e.to_string()))?;
        media_content["negotiationTag"] = format!(
            "{};ss_{share}",
            self.me.participant_id.as_deref().unwrap_or_default()
        )
        .into();
        let body = serde_json::json!({
            "mediaNegotiation": {
                "callModalities": crate::teams::calling::sdp::modalities(offer),
                "sender": self.me,
                "links": {
                    "mediaAnswer": self.callbacks.link(Scope::Call, "mediaAnswer"),
                    "rejection": self.callbacks.link(Scope::Call, "rejection"),
                },
                "mediaContent": media_content,
            }
        });
        self.send(reqwest::Method::POST, url, Some(body), "renegotiate")
            .await
            .map(drop)
    }

    /// Acknowledges the far end's answer to a renegotiation of ours, at
    /// its `mediaAcknowledgement` link, as the web client does: an empty
    /// body (recorded).
    pub async fn acknowledge_answer(&self, url: &str) -> Result<(), Failure> {
        self.send(reqwest::Method::POST, url, None, "acknowledge an answer")
            .await
            .map(drop)
    }

    /// Acknowledges the far end picking up, at the acceptance's
    /// `acknowledgement` link, as the web client does on every pickup.
    pub async fn acknowledge_acceptance(&self, url: &str) -> Result<(), Failure> {
        let body = acceptance_acknowledgement(&self.callbacks);
        self.send(
            reqwest::Method::POST,
            url,
            Some(body),
            "acknowledge the pickup",
        )
        .await
        .map(drop)
    }

    /// Tells Teams the call leg is still here, at its `callLeg` link; due
    /// a little before each `callKeepAliveInterval` runs out.
    pub async fn keep_alive(&self, call_leg: &str) -> Result<(), Failure> {
        let body = serde_json::json!({ "callParticipantUpdate": {} });
        self.send(reqwest::Method::POST, call_leg, Some(body), "keep the call")
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

    /// Attaches to an incoming call and joins its `conversation` (B.2):
    /// answers the leg's links and the conversation's.
    pub async fn attach(&self, url: &str, conversation: &str) -> Result<AttachAnswer, Failure> {
        let body = attach_request(&self.me, &self.callbacks, conversation);
        let resp = self
            .send(reqwest::Method::POST, url, Some(body), "attach to a call")
            .await?;
        resp.json::<AttachAnswer>()
            .await
            .map_err(|e| Failure::Unexpected(e.without_url().to_string()))
    }

    /// Says the call rings here (B.3).
    pub async fn ringing(&self, progress_url: &str) -> Result<(), Failure> {
        self.send(
            reqwest::Method::POST,
            progress_url,
            Some(ringing_body(&self.me)),
            "say a call rings",
        )
        .await
        .map(drop)
    }

    /// Picks up with our SDP `answer` on the caller's `media_leg_id`
    /// (B.5): answers the live leg's links.
    pub async fn accept(
        &self,
        url: &str,
        answer: &str,
        media_leg_id: &str,
    ) -> Result<AcceptanceAcknowledgement, Failure> {
        let body = acceptance_body(&self.me, &self.callbacks, answer, media_leg_id);
        let resp = self
            .send(reqwest::Method::POST, url, Some(body), "pick up a call")
            .await?;
        resp.json::<AcceptAnswer>()
            .await
            .map(|answer| answer.call_acceptance_acknowledgement)
            .map_err(|e| Failure::Unexpected(e.without_url().to_string()))
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
    /// How long they last, in seconds (a week in a recording).
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
            // `api-version: 2` gives the answer in the shape the web
            // client reads (recorded); without it the answer differs.
            http.get(TRAP_TOKENS_URL)
                .header("X-Skypetoken", token)
                .header("api-version", "2")
                .header("x-ms-migration", "True")
                .header(reqwest::header::ACCEPT, "application/json, text/javascript")
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
    read_relay_credentials(&answer).ok_or_else(|| {
        // Its field names say what shape it came in; their values are secrets.
        let fields: Vec<&str> = answer
            .as_object()
            .map(|o| o.keys().map(String::as_str).collect())
            .unwrap_or_default();
        log::warn!("relay credentials in an unknown shape: fields {fields:?}");
        Failure::Unexpected("no relay credentials in the answer".into())
    })
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
    /// Microsoft's global relay name, which DNS answers with the nearest
    /// region's relays, for when the configuration names none: it names
    /// them only for some sign-ins (the recorded European web client got
    /// `gateway-eu.az.relay.teams.cloud.microsoft`; a request without the
    /// user's ids gets no relay block at all).
    fn default() -> Self {
        Self {
            hosts: vec!["worldaz.relay.teams.microsoft.com".to_owned()],
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
            log::info!("the Skype configuration names no relay servers; using the global ones");
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

    #[test]
    fn a_pickup_is_acknowledged_with_our_links() {
        let callbacks = Callbacks::new("https://trouter.example/v4/f/ID1/", "AGENT");
        let body = acceptance_acknowledgement(&callbacks);
        let links = &body["callAcceptanceAcknowledgement"]["links"];
        for event in ACKNOWLEDGED_EVENTS {
            let link = links[*event].as_str().expect("a link for each event");
            assert!(link.contains("/callAgent/AGENT/"), "{link}");
            assert!(link.contains(&format!("/call/{event}")), "{link}");
        }
    }
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

    #[test]
    fn a_meeting_is_found_first_then_joined_with_the_offer() {
        let ids = ids();
        let me = me(&ids);
        let callbacks = Callbacks::new(SURL, &ids.call_agent_id);
        let typed =
            serde_json::json!({"meetingCode": "123", "passcode": "token", "meetingUrl": "u"});
        let preheat = meeting_request(&me, &ids, &callbacks, &typed, None);
        assert_eq!(preheat["meetingData"], typed);
        assert!(preheat.get("callInvitation").is_none());
        assert!(preheat["participants"].get("to").is_none());
        assert!(
            preheat["conversationRequest"]
                .get("suppressDialout")
                .is_none()
        );
        let state = &preheat["endpointState"];
        assert_eq!(state["endpointStateSequenceNumber"], 0);
        assert!(
            state["endpointProperties"]
                .get("preheatProperties")
                .is_none()
        );
        // The preheat's callbacks are those of one joining.
        let links = preheat["conversationRequest"]["links"]
            .as_object()
            .expect("links");
        assert_eq!(links.len(), JOIN_EVENTS.len());

        let answered =
            serde_json::json!({"meetingCode": "123", "passcode": "123456", "meetingUrl": "u2"});
        let offer = "v=0\r\nm=audio 9 UDP/TLS/RTP/SAVPF 111\r\na=label:main-audio\r\n";
        let join = meeting_request(&me, &ids, &callbacks, &answered, Some(offer));
        assert_eq!(join["meetingData"], answered);
        assert_eq!(join["participants"]["to"], serde_json::json!([]));
        assert_eq!(join["conversationRequest"]["suppressDialout"], true);
        assert_eq!(
            join["endpointState"]["endpointProperties"]["preheatProperties"],
            1
        );
        let invitation = &join["callInvitation"];
        assert_eq!(invitation["callModalities"][0], "Audio");
        assert_eq!(
            invitation["mediaContent"]["mediaDescriptions"]["requestId"],
            1
        );
        assert_eq!(invitation["mediaContent"]["blob"], offer);
        assert_eq!(
            invitation["mediaContent"]["mediaLegId"],
            ids.media_leg_id.as_str()
        );
        let acceptance = invitation["links"]["acceptance"].as_str().expect("link");
        let pushed = read_push_path(acceptance).expect("a callback of this call");
        assert_eq!(pushed.event, "acceptance");
    }

    #[test]
    fn a_typed_meeting_is_sent_as_its_parts() {
        let meeting = crate::meetings::Meeting::parse("9312345678901", "a1B2c3").expect("an id");
        let data = meeting_data(&meeting);
        assert_eq!(data["meetingCode"], "9312345678901");
        assert_eq!(data["passcode"], "a1B2c3");
        assert_eq!(
            data["meetingUrl"],
            "https://teams.live.com/meet/9312345678901?p=a1B2c3"
        );
    }

    #[test]
    fn admitting_names_who_and_where_to_say_so() {
        let ids = ids();
        let me = me(&ids);
        let callbacks = Callbacks::new(SURL, &ids.call_agent_id);
        let body = admit_body(&me, &callbacks, "8:live:waiting");
        assert_eq!(body["participants"]["to"][0]["id"], "8:live:waiting");
        assert_eq!(body["participants"]["from"]["id"], "8:live:me");
        assert!(
            body["links"]["admitSuccess"]
                .as_str()
                .expect("link")
                .contains("/conversation/admitSuccess")
        );
    }

    #[test]
    fn a_meeting_is_told_which_video_lines_receive_and_send() {
        // As the web client said it on joining, its camera off.
        let off = media_descriptions(Some("1"), false, &[], Some("11"), 1);
        assert_eq!(
            off,
            serde_json::json!({"descriptions": [
                {"mid": "1", "direction": "recvonly"},
                {"mid": "11", "direction": "recvonly"},
            ], "requestId": 1})
        );
        // And once it was on.
        let on = media_descriptions(Some("1"), true, &["2"], Some("11"), 2);
        assert_eq!(on["descriptions"][0]["direction"], "sendrecv");
        assert_eq!(on["descriptions"][0]["label"], "main-video");
        assert_eq!(on["requestId"], 2);
        assert_eq!(on["descriptions"][1]["mid"], "2");
        assert_eq!(on["descriptions"][1]["direction"], "recvonly");
        // An SDP's own lines: the meeting's media server's renumbered ones.
        let read = descriptions_for(include_str!("fixtures/meeting_retarget.sdp"), false, 3);
        let mids: Vec<&str> = read["descriptions"]
            .as_array()
            .expect("a list")
            .iter()
            .filter_map(|d| d["mid"].as_str())
            .collect();
        // The camera's, its other camera lines, then the share's.
        assert_eq!(mids.first(), Some(&"2"));
        assert_eq!(mids.get(1), Some(&"5"));
        assert_eq!(mids.last(), Some(&"3"));
        // An audio-only SDP has none.
        let none = descriptions_for("v=0\r\nm=audio 9 RTP/SAVP 111\r\na=mid:0\r\n", false, 1);
        assert_eq!(none["descriptions"], serde_json::json!([]));
    }
}
