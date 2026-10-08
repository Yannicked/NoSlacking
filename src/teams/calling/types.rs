//! The JSON of a Teams call: requests, answers and pushes, as
//! `docs/research/teams-calls.md` §A–C records them.
//!
//! Everything read is lenient: unknown fields are ignored and missing or
//! `null` ones take their default, because Microsoft adds fields freely and
//! the capture is all we know. What is sent mirrors the web client, nulls
//! included where it sends them.

use std::collections::BTreeMap;

use serde::{Deserialize, Deserializer, Serialize};

/// Reads `null` as the type's default, so a field Microsoft sometimes
/// sends as `null` does not fail the whole push.
fn nullable<'de, D, T>(deserializer: D) -> Result<T, D::Error>
where
    D: Deserializer<'de>,
    T: Default + Deserialize<'de>,
{
    Ok(Option::<T>::deserialize(deserializer)?.unwrap_or_default())
}

/// The content type of an SDP we send: the web client's, although the
/// blob is plain SDP text.
pub const OUR_SDP_TYPE: &str = "application/sdp-ngc-1.0";

/// A person in a call (§1.6): how every request names us (`from`,
/// `sender`, `acceptedBy`) and how pushes name the far end.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Participant {
    /// The MRI (`8:live:…`, `8:orgid:…`).
    #[serde(deserialize_with = "nullable")]
    pub id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    /// The device's id with Teams (ours: our Trouter endpoint id).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub endpoint_id: Option<String>,
    /// The id of this person in this one call.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub participant_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub language_id: Option<String>,
}

/// The two sides of a request: us, and whom we call.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Participants {
    pub from: Participant,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub to: Vec<Participant>,
}

/// An SDP and what goes with it, both ways.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct MediaContent {
    /// The SDP itself, as text.
    #[serde(deserialize_with = "nullable")]
    pub blob: String,
    #[serde(deserialize_with = "nullable")]
    pub content_type: String,
    /// The caller's 32-hex media leg id, echoed with every SDP of a call.
    #[serde(deserialize_with = "nullable")]
    pub media_leg_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub required_features: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub client_location: Option<String>,
    /// The web client's starting bandwidth hint; opaque, sent as recorded.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub apply_channel_parameters: Option<serde_json::Value>,
    /// In a meeting, which video lines we receive on and send on (see
    /// [`crate::teams::calling::api::media_descriptions`]): its media
    /// server sends and forwards video by these, not by the SDP's
    /// directions (recorded).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub media_descriptions: Option<serde_json::Value>,
    /// Only in what Microsoft sends.
    #[serde(skip_serializing)]
    pub new_offer: bool,
}

impl MediaContent {
    /// Our SDP, as the web client sends it in an offer or an answer.
    pub fn ours(blob: String, media_leg_id: String) -> Self {
        Self {
            blob,
            content_type: OUR_SDP_TYPE.to_owned(),
            media_leg_id,
            required_features: None,
            client_location: None,
            apply_channel_parameters: Some(serde_json::json!({
                "multiChannelParameter": {
                    "mids": ["*"],
                    "mediaParameter": "{\"sendSideBWSeed\":{\"seedValueBitsPerSec\":600000}}"
                }
            })),
            media_descriptions: None,
            new_offer: false,
        }
    }
}

/// Our endpoint's metadata (A.1, A.4).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct EndpointMetadata {
    pub holographic_capabilities: u32,
}

/// Our endpoint's state: whether we are muted, numbered so the server
/// keeps the latest (C.3).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct EndpointState {
    pub endpoint_state_sequence_number: u64,
    pub endpoint_properties: EndpointProperties,
    /// Left out when the call starts (A.1).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub state: Option<MuteState>,
}

/// What the web client always says about its endpoint.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct EndpointProperties {
    pub additional_endpoint_properties: serde_json::Value,
}

impl Default for EndpointProperties {
    fn default() -> Self {
        Self {
            additional_endpoint_properties: serde_json::json!({
                "infoShownInReportMode": "FullInformation"
            }),
        }
    }
}

/// Muted or not.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct MuteState {
    pub is_muted: bool,
}

// ---------------------------------------------------------------- A.1

/// `POST {fp}/cpconv`: start a conversation and ring the callee (A.1).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct CpconvRequest {
    pub conversation_request: ConversationRequest,
    pub participants: Participants,
    pub endpoint_capabilities: u64,
    pub client_endpoint_capabilities: u64,
    pub endpoint_metadata: EndpointMetadata,
    pub endpoint_state: EndpointState,
    pub call_invitation: CallInvitation,
    /// Opaque; sent as the web client does.
    pub participant_property_bag: serde_json::Value,
}

/// The conversation half of [`CpconvRequest`].
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct ConversationRequest {
    /// Always false: a 1:1 call never dials a phone number.
    pub suppress_dialout: bool,
    pub application_type: String,
    pub roster: Roster,
    pub properties: serde_json::Value,
    /// Our callbacks for the conversation, by name.
    pub links: BTreeMap<String, String>,
}

/// Where the roster is to be pushed.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Roster {
    #[serde(rename = "type")]
    pub kind: String,
    pub roster_update: String,
}

/// The call half of [`CpconvRequest`].
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct CallInvitation {
    /// `["Audio"]` for an audio call.
    pub call_modalities: Vec<String>,
    /// Our callbacks for the call, by name.
    pub links: BTreeMap<String, String>,
    pub client_content_for_media_controller: BTreeMap<String, String>,
    pub pstn_content: serde_json::Value,
    pub media_content: MediaContent,
    pub voicemail_settings: serde_json::Value,
}

/// What `cpconv` answers: the conversation and the links we keep.
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct CpconvAnswer {
    #[serde(deserialize_with = "nullable")]
    pub conversation_controller: String,
    #[serde(deserialize_with = "nullable")]
    pub links: ConversationLinks,
    /// A meeting's code, passcode and link, as the meeting service
    /// knows them: a `cpconv` that joins a meeting answers them, and
    /// joining it for real sends them back as they came (recorded; the
    /// passcode comes back as the meeting's own, not the link's token).
    /// Holds the passcode: never logged.
    pub meeting_data: Option<serde_json::Value>,
}

/// The conversation's links we use; the rest are for group calls.
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct ConversationLinks {
    /// Hanging up (C.5).
    pub leave: Option<String>,
    /// Mute (C.3).
    pub update_endpoint_state: Option<String>,
    /// A.4.
    pub update_endpoint_metadata: Option<String>,
    /// Letting someone in from a meeting's lobby, for those who may.
    pub admit: Option<String>,
}

impl ConversationLinks {
    /// These links, with any that `newer` has taking their place: a
    /// meeting's conversation hands out more of them once you are in.
    #[must_use]
    pub fn merged(self, newer: Self) -> Self {
        Self {
            leave: newer.leave.or(self.leave),
            update_endpoint_state: newer.update_endpoint_state.or(self.update_endpoint_state),
            update_endpoint_metadata: newer
                .update_endpoint_metadata
                .or(self.update_endpoint_metadata),
            admit: newer.admit.or(self.admit),
        }
    }
}

// ---------------------------------------------------------------- A.2, A.3

/// The `call/mediaAnswer` push: the callee's SDP, while it rings (A.2).
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct MediaAnswerPush {
    pub media_answer: MediaAnswer,
}

/// See [`MediaAnswerPush`].
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct MediaAnswer {
    pub sender: Option<Participant>,
    #[serde(deserialize_with = "nullable")]
    pub media_content: MediaContent,
    #[serde(deserialize_with = "nullable")]
    pub call_modalities: Vec<String>,
    #[serde(deserialize_with = "nullable")]
    pub links: AcknowledgementLinks,
}

/// Where an SDP's acknowledgement goes (never used by the web client).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct AcknowledgementLinks {
    pub media_acknowledgement: Option<String>,
}

/// The `call/acceptance` push: the callee picked up (A.3).
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct CallAcceptancePush {
    pub call_acceptance: CallAcceptance,
}

/// See [`CallAcceptancePush`].
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct CallAcceptance {
    pub accepted_by: Option<Participant>,
    /// Lower case here (`["audio"]`).
    #[serde(deserialize_with = "nullable")]
    pub accepted_call_modalities: Vec<String>,
    #[serde(deserialize_with = "nullable")]
    pub links: AcceptanceLinks,
    /// The answer again, with the directions that were accepted.
    pub media_content: Option<MediaContent>,
    /// In seconds.
    pub call_keep_alive_interval: Option<u64>,
    /// Which controller took the call: `lobby` while a meeting keeps
    /// you waiting to be let in (recorded); otherwise none.
    pub controller_name: Option<String>,
}

impl CallAcceptance {
    /// Whether this acceptance only puts you in a meeting's lobby.
    pub fn is_lobby(&self) -> bool {
        self.controller_name.as_deref() == Some("lobby")
    }
}

/// The live call leg's links we keep.
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct AcceptanceLinks {
    pub call_leg: Option<String>,
    /// For a renegotiation we start (adding video, later).
    pub media_renegotiation: Option<String>,
    pub acknowledgement: Option<String>,
    /// In a meeting: where the video lines' use changes (our camera on
    /// or off).
    pub update_media_descriptions: Option<String>,
    /// In a meeting: where what our camera can send is said.
    pub apply_channel_parameters: Option<String>,
}

/// `PUT {updateEndpointMetadata}` (A.4).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct UpdateEndpointMetadata {
    pub participants: Participants,
    pub endpoint_metadata: EndpointMetadata,
}

// ---------------------------------------------------------------- C.1

/// The `call/mediaRenegotiation` push: a new offer from the far end (C.1).
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct MediaNegotiationPush {
    pub media_negotiation: MediaNegotiation,
}

/// See [`MediaNegotiationPush`].
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct MediaNegotiation {
    pub sender: Option<Participant>,
    #[serde(deserialize_with = "nullable")]
    pub media_content: MediaContent,
    #[serde(deserialize_with = "nullable")]
    pub call_modalities: Vec<String>,
    #[serde(deserialize_with = "nullable")]
    pub links: NegotiationLinks,
}

/// Where our answer to a renegotiation goes, or our refusal.
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct NegotiationLinks {
    pub media_answer: Option<String>,
    pub rejection: Option<String>,
}

/// Our `POST {links.mediaAnswer}` to a renegotiation (C.1).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct RenegotiationAnswer {
    pub media_answer: OurMediaAnswer,
    pub debug_content: AnswerDebug,
}

/// See [`RenegotiationAnswer`].
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct OurMediaAnswer {
    /// Capitalised here (`["Audio"]`).
    pub call_modalities: Vec<String>,
    pub sender: Participant,
    pub links: AcknowledgementLinks,
    pub client_content_for_media_controller: BTreeMap<String, String>,
    pub media_content: MediaContent,
}

/// The ids the web client adds to a renegotiation answer.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct AnswerDebug {
    pub call_id: String,
    pub endpoint_id: String,
}

/// The `call/mediaAcknowledgement` push: whether our answer was taken.
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct MediaAcknowledgementPush {
    #[serde(deserialize_with = "nullable")]
    pub media_acknowledgement: Outcome,
    /// A new call leg's links, when the answer moved the call to another
    /// media server: a meeting's, once you are let in (recorded, with
    /// sub-code 10109, "Participant retarget was successful").
    #[serde(deserialize_with = "nullable")]
    pub links: AcceptanceLinks,
}

/// How something ended or was answered: an acknowledgement, a call's
/// end, a transaction's end. `code` 0 is success.
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Outcome {
    #[serde(deserialize_with = "nullable")]
    pub reason: String,
    pub sender: Option<Participant>,
    #[serde(deserialize_with = "nullable")]
    pub code: i64,
    #[serde(deserialize_with = "nullable")]
    pub sub_code: i64,
    /// Microsoft's English, for the log (`LocalUserInitiated`).
    #[serde(deserialize_with = "nullable")]
    pub phrase: String,
    #[serde(deserialize_with = "nullable")]
    pub result_categories: Vec<String>,
    /// Who took the call instead, when another device did.
    pub accepted_elsewhere_by: Option<Participant>,
}

// ---------------------------------------------------------------- C.2

/// The `conversation/rosterUpdate` push: the participants that changed
/// (C.2).
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct RosterUpdate {
    /// By MRI.
    #[serde(deserialize_with = "nullable")]
    pub participants: BTreeMap<String, RosterParticipant>,
    /// `Delta`: only the participants named changed.
    #[serde(rename = "type", deserialize_with = "nullable")]
    pub kind: String,
    /// Rises by one per push in a conversation.
    #[serde(deserialize_with = "nullable")]
    pub sequence_number: u64,
}

/// One participant in a [`RosterUpdate`].
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct RosterParticipant {
    /// Rises with each change to this participant; keep the highest.
    #[serde(deserialize_with = "nullable")]
    pub version: u64,
    /// `active` while in the call.
    #[serde(deserialize_with = "nullable")]
    pub state: String,
    pub details: Option<Participant>,
    /// By endpoint id.
    #[serde(deserialize_with = "nullable")]
    pub endpoints: BTreeMap<String, RosterEndpoint>,
    /// In a meeting: `admin` once in (everyone, in a personal account's
    /// meetings), `guest` while waiting.
    #[serde(deserialize_with = "nullable")]
    pub role: String,
}

impl RosterParticipant {
    /// The name to show, if the roster gave one.
    pub fn name(&self) -> Option<&str> {
        self.details
            .as_ref()?
            .display_name
            .as_deref()
            .filter(|n| !n.is_empty())
    }

    /// Whether the participant is muted: an endpoint of theirs says so.
    /// (`isMicrophoneOn` in the metadata is not trusted: the native
    /// client left it false while talking.)
    pub fn is_muted(&self) -> bool {
        self.endpoints.values().any(|e| {
            e.endpoint_state
                .as_ref()
                .and_then(|s| s.state)
                .is_some_and(|s| s.is_muted)
        })
    }

    /// Whether the participant is in the call.
    pub fn is_active(&self) -> bool {
        self.state == "active"
    }

    /// Their camera's source id, while it is on: the `sourceId` of a
    /// `main-video` stream sending from a device in the call (recorded:
    /// it turns `sendrecv` when the camera goes on).
    pub fn camera_source(&self) -> Option<i64> {
        self.endpoints
            .values()
            .filter_map(|e| e.call.as_ref()?.get("mediaStreams")?.as_array())
            .flatten()
            .find(|s| {
                s.get("type").and_then(|t| t.as_str()) == Some("video")
                    && s.get("label").and_then(|l| l.as_str()) == Some("main-video")
                    && s.get("direction").and_then(|d| d.as_str()) == Some("sendrecv")
            })
            .and_then(|s| s.get("sourceId")?.as_i64())
    }

    /// Whether they wait in a meeting's lobby: a device of theirs is
    /// there and none is in the call (recorded: a waiting endpoint has
    /// `lobby` where one in the call has `call`).
    pub fn is_waiting(&self) -> bool {
        self.is_active()
            && self.endpoints.values().any(RosterEndpoint::waits)
            && !self.endpoints.values().any(|e| e.call.is_some())
    }
}

/// One device of a [`RosterParticipant`].
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct RosterEndpoint {
    pub participant_id: Option<String>,
    pub endpoint_state: Option<EndpointState>,
    /// Its media in the call, while it is in it.
    pub call: Option<serde_json::Value>,
    /// Its media in a meeting's lobby, while it waits there.
    pub lobby: Option<serde_json::Value>,
    /// Where it has been: `Lobby` while waiting, `Lobby,Call` once let
    /// in, `Call` for one never kept waiting (recorded).
    #[serde(deserialize_with = "nullable")]
    pub modality_joined: String,
}

impl RosterEndpoint {
    /// Whether this device waits in the lobby: it is there, it has never
    /// been in the call, and it is not in it now. One let in and gone
    /// again can still show its lobby media.
    pub fn waits(&self) -> bool {
        self.lobby.is_some()
            && self.call.is_none()
            && !self
                .modality_joined
                .split(',')
                .any(|m| m.trim().eq_ignore_ascii_case("call"))
    }
}

// ---------------------------------------------------------------- meetings

/// The `conversation/conversationUpdate` push: what a conversation has
/// now. In a meeting it says when you are let in from the lobby (the
/// call modality comes, the lobby goes) and hands out the links of one
/// who is in, `admit` among them (recorded).
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct ConversationUpdate {
    #[serde(deserialize_with = "nullable")]
    pub active_modalities: ActiveModalities,
    #[serde(deserialize_with = "nullable")]
    pub links: ConversationLinks,
}

impl ConversationUpdate {
    /// Whether you are in the call rather than its lobby.
    pub fn in_call(&self) -> bool {
        self.active_modalities.call.is_some() && self.active_modalities.lobby.is_none()
    }
}

/// What a conversation has going on.
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct ActiveModalities {
    /// The call, once you are in it.
    pub call: Option<serde_json::Value>,
    /// The lobby, while you wait in it.
    pub lobby: Option<serde_json::Value>,
    /// The meeting's chat.
    pub group_chat: Option<GroupChat>,
}

/// A meeting's chat.
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct GroupChat {
    /// `19:meeting_…@thread.v2`.
    #[serde(deserialize_with = "nullable")]
    pub thread_id: String,
}

// ---------------------------------------------------------------- C.3

/// `POST {updateEndpointState}`: mute or unmute (C.3).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct UpdateEndpointState {
    pub from: Participant,
    pub endpoint_state: EndpointState,
}

// ---------------------------------------------------------------- C.4

/// The `call/end` push (C.4).
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct CallEndPush {
    #[serde(deserialize_with = "nullable")]
    pub call_end: Outcome,
}

/// The `conversation/conversationEnd` push (C.4).
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct ConversationEnd {
    #[serde(deserialize_with = "nullable")]
    pub code: i64,
    #[serde(deserialize_with = "nullable")]
    pub sub_code: i64,
    #[serde(deserialize_with = "nullable")]
    pub phrase: String,
    #[serde(deserialize_with = "nullable")]
    pub result_categories: Vec<String>,
    /// How the call itself ended.
    pub call_controller_transaction_end: Option<Outcome>,
}

// ---------------------------------------------------------------- C.5

/// `POST {links.leave}`: our hang-up (C.5, recorded since).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Leave {
    pub participants: Participants,
    pub conversation_transaction_end: TransactionEnd,
    pub call_transaction_end: TransactionEnd,
}

/// How we say a conversation or call ended, in [`Leave`] and [`Decline`].
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TransactionEnd {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    pub code: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sub_code: Option<i64>,
    pub phrase: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result_categories: Option<Vec<String>>,
    /// Only when hanging up while it still rings.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub call_quality_diagnostics_information: Option<CancelDiagnostics>,
    /// Only when declining.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub application_type: Option<String>,
}

/// How long a call rang before we hung up.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CancelDiagnostics {
    /// Whole seconds since the call started.
    pub cancelation_duration: u64,
}

/// `DELETE {callInvitation.links.reject}`: declining an incoming call.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Decline {
    pub call_end: TransactionEnd,
}

// ---------------------------------------------------------------- B

/// The incoming-call notification at the bare `{surl}` (B.1), with `gp`
/// already decoded from base64.
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct IncomingNotification {
    /// 107 for a call.
    #[serde(deserialize_with = "nullable")]
    pub evt: i64,
    pub gp: Option<NotificationPayload>,
}

/// The decoded `gp` of an [`IncomingNotification`]. Its `udpKey` (a
/// secret, for a fast path we do not use) is not read.
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct NotificationPayload {
    pub call_notification: Option<CallNotification>,
    pub conversation_invitation: Option<ConversationInvitation>,
    pub debug_content: Option<NotificationDebug>,
}

/// Who calls, and where to answer.
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct CallNotification {
    pub from: Option<Participant>,
    /// Us, with the participant id we have in this call.
    pub to: Option<Participant>,
    #[serde(deserialize_with = "nullable")]
    pub links: NotificationLinks,
    /// The caller's offer.
    pub media_content: Option<MediaContent>,
}

/// The forked call leg's links.
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct NotificationLinks {
    pub attach: Option<String>,
    pub progress: Option<String>,
    pub reject: Option<String>,
}

/// The conversation an incoming call belongs to.
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct ConversationInvitation {
    pub conversation_controller: Option<String>,
    #[serde(deserialize_with = "nullable")]
    pub is_multi_party: bool,
}

/// The ids of an incoming call.
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct NotificationDebug {
    /// The chain id of every request we make for this call.
    pub call_id: Option<String>,
}

/// What `POST {attach}` answers (B.2): the links of the incoming leg,
/// and what joining the conversation answered.
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct AttachAnswer {
    pub call_invitation: Option<IncomingInvitation>,
    #[serde(deserialize_with = "nullable")]
    pub additional_action_responses: Vec<ActionResponse>,
}

impl AttachAnswer {
    /// The conversation's links, from the `join` done with the attach.
    pub fn conversation(&self) -> Option<&ConversationLinks> {
        self.additional_action_responses
            .iter()
            .find_map(|r| r.output.as_ref())
            .map(|output| &output.links)
    }
}

/// One of the actions done with an attach, answered.
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct ActionResponse {
    pub output: Option<JoinOutput>,
}

/// What joining the conversation answered: its links, as `cpconv`'s.
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct JoinOutput {
    #[serde(deserialize_with = "nullable")]
    pub links: ConversationLinks,
}

/// What `POST {acceptance}` answers (B.5): the live leg's links.
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct AcceptAnswer {
    #[serde(deserialize_with = "nullable")]
    pub call_acceptance_acknowledgement: AcceptanceAcknowledgement,
}

/// See [`AcceptAnswer`].
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct AcceptanceAcknowledgement {
    #[serde(deserialize_with = "nullable")]
    pub links: AcceptanceLinks,
    /// In seconds.
    pub call_keep_alive_interval: Option<u64>,
}

/// See [`AttachAnswer`].
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct IncomingInvitation {
    #[serde(deserialize_with = "nullable")]
    pub call_modalities: Vec<String>,
    #[serde(deserialize_with = "nullable")]
    pub links: IncomingLinks,
}

/// The incoming leg's links.
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct IncomingLinks {
    pub progress: Option<String>,
    pub acceptance: Option<String>,
    /// Declining (recorded: `DELETE`).
    pub reject: Option<String>,
    /// Ends in `/reject` too, in the capture.
    pub call_leg: Option<String>,
}

impl IncomingLinks {
    /// Where a decline goes: `reject`, or the call leg, which in the
    /// capture is the same `…/reject` URL.
    pub fn decline(&self) -> Option<&str> {
        self.reject.as_deref().or(self.call_leg.as_deref())
    }
}

// ---------------------------------------------------------------- pushes

/// A push to one of a call's callbacks, read by its event name.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Push {
    MediaAnswer(MediaAnswer),
    Acceptance(CallAcceptance),
    MediaNegotiation(MediaNegotiation),
    /// Whether an answer of ours was taken, and the new call leg's links
    /// when it moved the call.
    MediaAcknowledgement(Outcome, AcceptanceLinks),
    RosterUpdate(RosterUpdate),
    CallEnd(Outcome),
    ConversationEnd(ConversationEnd),
    ConversationUpdate(ConversationUpdate),
    /// A callback a call does not act on (`progress`,
    /// `admitParticipantSuccess`, …), by its event name.
    Other(String),
}

impl Push {
    /// Reads the body of a push to the callback `event`.
    pub fn read(event: &str, body: &serde_json::Value) -> Result<Self, serde_json::Error> {
        let body = body.clone();
        Ok(match event {
            "mediaAnswer" => Self::MediaAnswer(MediaAnswerPush::deserialize(body)?.media_answer),
            "acceptance" => {
                Self::Acceptance(CallAcceptancePush::deserialize(body)?.call_acceptance)
            }
            "mediaRenegotiation" => {
                Self::MediaNegotiation(MediaNegotiationPush::deserialize(body)?.media_negotiation)
            }
            "mediaAcknowledgement" => {
                let push = MediaAcknowledgementPush::deserialize(body)?;
                Self::MediaAcknowledgement(push.media_acknowledgement, push.links)
            }
            "conversationUpdate" => {
                Self::ConversationUpdate(ConversationUpdate::deserialize(body)?)
            }
            "rosterUpdate" => Self::RosterUpdate(RosterUpdate::deserialize(body)?),
            "end" => Self::CallEnd(CallEndPush::deserialize(body)?.call_end),
            "conversationEnd" => Self::ConversationEnd(ConversationEnd::deserialize(body)?),
            other => Self::Other(other.to_owned()),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(name: &str) -> serde_json::Value {
        let text = match name {
            "cpconv_request" => include_str!("fixtures/cpconv_request.json"),
            "cpconv_answer" => include_str!("fixtures/cpconv_answer.json"),
            "media_answer" => include_str!("fixtures/media_answer.json"),
            "call_acceptance" => include_str!("fixtures/call_acceptance.json"),
            "media_renegotiation" => include_str!("fixtures/media_renegotiation.json"),
            "renegotiation_answer" => include_str!("fixtures/renegotiation_answer.json"),
            "media_acknowledgement" => include_str!("fixtures/media_acknowledgement.json"),
            "roster_update" => include_str!("fixtures/roster_update.json"),
            "roster_update_muted" => include_str!("fixtures/roster_update_muted.json"),
            "call_end" => include_str!("fixtures/call_end.json"),
            "conversation_end" => include_str!("fixtures/conversation_end.json"),
            "update_endpoint_state" => include_str!("fixtures/update_endpoint_state.json"),
            "update_endpoint_metadata" => include_str!("fixtures/update_endpoint_metadata.json"),
            "call_notification" => include_str!("fixtures/call_notification.json"),
            "conversation_update" => include_str!("fixtures/conversation_update.json"),
            "roster_lobby" => include_str!("fixtures/roster_lobby.json"),
            "media_acknowledgement_retarget" => {
                include_str!("fixtures/media_acknowledgement_retarget.json")
            }
            _ => panic!("no fixture {name}"),
        };
        serde_json::from_str(text).expect("fixture is JSON")
    }

    #[test]
    fn the_web_clients_cpconv_reads_as_ours() {
        let request: CpconvRequest =
            serde_json::from_value(fixture("cpconv_request")).expect("reads");
        assert_eq!(request.participants.from.id, "8:live:me");
        assert_eq!(request.participants.to[0].id, "8:live:other");
        assert_eq!(request.call_invitation.call_modalities, ["Audio"]);
        assert_eq!(
            request.call_invitation.media_content.content_type,
            OUR_SDP_TYPE
        );
        assert_eq!(request.endpoint_capabilities, 73463);
        assert_eq!(request.client_endpoint_capabilities, 42876960);
        assert!(request.call_invitation.links.contains_key("mediaAnswer"));
        assert!(
            request
                .conversation_request
                .links
                .contains_key("conversationEnd")
        );
        assert_eq!(request.conversation_request.roster.kind, "Delta");
    }

    #[test]
    fn the_cpconv_answer_keeps_its_links() {
        let answer: CpconvAnswer = serde_json::from_value(fixture("cpconv_answer")).expect("reads");
        assert!(answer.conversation_controller.contains("/conv/"));
        assert!(answer.links.leave.expect("leave").contains("/leave?"));
        assert!(
            answer
                .links
                .update_endpoint_state
                .expect("state")
                .contains("/updateEndpointState?")
        );
        assert!(answer.links.update_endpoint_metadata.is_some());
    }

    #[test]
    fn a_media_answer_carries_the_sdp_and_the_callee() {
        match Push::read("mediaAnswer", &fixture("media_answer")).expect("reads") {
            Push::MediaAnswer(answer) => {
                assert!(answer.media_content.blob.starts_with("v=0"));
                assert_eq!(answer.media_content.content_type, "application/sdp");
                let sender = answer.sender.expect("sender");
                assert_eq!(sender.id, "8:live:other");
                assert!(sender.language_id.is_none());
                assert!(answer.links.media_acknowledgement.is_some());
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn an_acceptance_keeps_the_call_leg() {
        match Push::read("acceptance", &fixture("call_acceptance")).expect("reads") {
            Push::Acceptance(acceptance) => {
                assert_eq!(acceptance.accepted_call_modalities, ["audio"]);
                let leg = acceptance.links.call_leg.expect("call leg");
                assert!(leg.contains("/cc/v1/active/"));
                assert!(
                    acceptance
                        .links
                        .media_renegotiation
                        .expect("renegotiation")
                        .contains("/renegotiate?")
                );
                assert_eq!(acceptance.call_keep_alive_interval, Some(2700));
                assert!(acceptance.media_content.is_some());
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_renegotiation_names_where_to_answer() {
        match Push::read("mediaRenegotiation", &fixture("media_renegotiation")).expect("reads") {
            Push::MediaNegotiation(offer) => {
                assert!(offer.sender.is_none());
                assert_eq!(offer.call_modalities, ["audio"]);
                assert!(
                    offer
                        .links
                        .media_answer
                        .expect("answer")
                        .ends_with("/answer?i=192-0-2-1")
                );
                assert!(offer.links.rejection.is_some());
                assert!(!offer.media_content.media_leg_id.is_empty());
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn the_web_clients_renegotiation_answer_reads_as_ours() {
        let answer: RenegotiationAnswer =
            serde_json::from_value(fixture("renegotiation_answer")).expect("reads");
        assert_eq!(answer.media_answer.sender.id, "8:live:me");
        assert!(answer.media_answer.links.media_acknowledgement.is_some());
        assert!(!answer.debug_content.call_id.is_empty());
    }

    #[test]
    fn an_acknowledgement_is_a_success() {
        match Push::read("mediaAcknowledgement", &fixture("media_acknowledgement")).expect("reads")
        {
            Push::MediaAcknowledgement(outcome, _) => {
                assert_eq!(outcome.code, 0);
                assert_eq!(outcome.phrase, "Success");
                assert_eq!(outcome.reason, "noError");
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_roster_names_both_and_who_is_muted() {
        let roster: RosterUpdate = serde_json::from_value(fixture("roster_update")).expect("reads");
        assert_eq!(roster.sequence_number, 1);
        let other = &roster.participants["8:live:other"];
        assert!(other.is_active());
        assert_eq!(other.name(), Some("Other Example"));
        assert!(!other.is_muted());
        assert!(roster.participants.contains_key("8:live:me"));

        let muted: RosterUpdate =
            serde_json::from_value(fixture("roster_update_muted")).expect("reads");
        assert!(muted.participants["8:live:me"].is_muted());
        assert_eq!(muted.participants.len(), 1);
    }

    #[test]
    fn the_ends_read() {
        match Push::read("end", &fixture("call_end")).expect("reads") {
            Push::CallEnd(end) => {
                assert_eq!((end.code, end.sub_code), (0, 0));
                assert_eq!(end.phrase, "LocalUserInitiated");
                assert_eq!(end.sender.expect("sender").id, "8:live:other");
            }
            other => panic!("{other:?}"),
        }
        match Push::read("conversationEnd", &fixture("conversation_end")).expect("reads") {
            Push::ConversationEnd(end) => {
                assert_eq!((end.code, end.sub_code), (0, 5002));
                let call = end.call_controller_transaction_end.expect("call end");
                assert!(call.accepted_elsewhere_by.is_none());
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn other_callbacks_are_named() {
        assert_eq!(
            Push::read("progress", &serde_json::json!({})).expect("reads"),
            Push::Other("progress".into())
        );
    }

    #[test]
    fn the_web_clients_endpoint_updates_read_as_ours() {
        let state: UpdateEndpointState =
            serde_json::from_value(fixture("update_endpoint_state")).expect("reads");
        assert_eq!(state.endpoint_state.endpoint_state_sequence_number, 3);
        assert_eq!(
            state.endpoint_state.state,
            Some(MuteState { is_muted: true })
        );
        // What we write has the recorded shape.
        let ours = serde_json::to_value(&state).expect("writes");
        assert_eq!(ours, fixture("update_endpoint_state"));

        let metadata: UpdateEndpointMetadata =
            serde_json::from_value(fixture("update_endpoint_metadata")).expect("reads");
        assert_eq!(metadata.endpoint_metadata.holographic_capabilities, 3);
        assert_eq!(
            serde_json::to_value(&metadata).expect("writes"),
            fixture("update_endpoint_metadata")
        );
    }

    #[test]
    fn an_incoming_call_names_the_caller_and_its_links() {
        let note: IncomingNotification =
            serde_json::from_value(fixture("call_notification")).expect("reads");
        assert_eq!(note.evt, 107);
        let gp = note.gp.expect("payload");
        let call = gp.call_notification.expect("call");
        assert_eq!(call.from.expect("from").id, "8:live:other");
        assert!(call.to.expect("to").participant_id.is_some());
        assert!(call.links.attach.is_some() && call.links.reject.is_some());
        assert!(call.media_content.is_some());
        assert!(
            gp.conversation_invitation
                .and_then(|c| c.conversation_controller)
                .is_some()
        );
        assert!(gp.debug_content.and_then(|d| d.call_id).is_some());
    }

    #[test]
    fn an_attach_answer_keeps_the_reject_link() {
        let answer: AttachAnswer = serde_json::from_value(serde_json::json!({
            "callInvitation": {"callModalities": ["audio"], "links": {
                "progress": "https://fp.example/cc/v1/incoming/x/progress",
                "callLeg": "https://fp.example/cc/v1/incoming/x/reject"}}
        }))
        .expect("reads");
        let links = answer.call_invitation.expect("invitation").links;
        assert_eq!(
            links.decline(),
            Some("https://fp.example/cc/v1/incoming/x/reject")
        );
    }

    #[test]
    fn a_meetings_conversation_update_says_you_are_in_and_how_to_admit() {
        match Push::read("conversationUpdate", &fixture("conversation_update")).expect("reads") {
            Push::ConversationUpdate(update) => {
                assert!(update.in_call());
                assert!(update.links.admit.expect("admit").contains("/admit?"));
                assert_eq!(
                    update.active_modalities.group_chat.expect("chat").thread_id,
                    "19:meeting_ZmFrZQ@thread.v2"
                );
            }
            other => panic!("{other:?}"),
        }
        // While waiting, the lobby is there and the call is not.
        let waiting: ConversationUpdate = serde_json::from_value(serde_json::json!({
            "activeModalities": {"lobby": {}, "call": null},
            "links": {"leave": "https://example.test/leave"}
        }))
        .expect("reads");
        assert!(!waiting.in_call());
    }

    #[test]
    fn newer_conversation_links_take_the_place_of_older_ones() {
        let older = ConversationLinks {
            leave: Some("old-leave".into()),
            update_endpoint_state: Some("state".into()),
            ..ConversationLinks::default()
        };
        let newer = ConversationLinks {
            leave: Some("new-leave".into()),
            admit: Some("admit".into()),
            ..ConversationLinks::default()
        };
        let merged = older.merged(newer);
        assert_eq!(merged.leave.as_deref(), Some("new-leave"));
        assert_eq!(merged.update_endpoint_state.as_deref(), Some("state"));
        assert_eq!(merged.admit.as_deref(), Some("admit"));
    }

    #[test]
    fn the_roster_tells_who_waits_in_the_lobby() {
        let Push::RosterUpdate(roster) =
            Push::read("rosterUpdate", &fixture("roster_lobby")).expect("reads")
        else {
            panic!("not a roster");
        };
        let who = |mri: &str| &roster.participants[mri];
        assert!(!who("8:live:organizer").is_waiting());
        assert!(who("8:live:organizer").is_muted());
        // The organizer's camera is off: its video stream receives only.
        assert_eq!(who("8:live:organizer").camera_source(), None);
        assert_eq!(who("8:live:waiting").camera_source(), None);
        assert!(who("8:live:waiting").is_waiting());
        assert_eq!(who("8:live:waiting").name(), Some("Wim Waiting"));
        assert_eq!(who("8:live:waiting").role, "guest");
        assert!(!who("8:live:gone").is_active());
        assert!(!who("8:live:gone").is_waiting());
        // Let in, then gone from the call: its lobby media left behind
        // does not make it wait again.
        let mut left = who("8:live:waiting").clone();
        for endpoint in left.endpoints.values_mut() {
            endpoint.modality_joined = "Lobby,Call".into();
        }
        assert!(!left.is_waiting());
    }

    #[test]
    fn a_lobby_acceptance_and_a_retarget_read_as_recorded() {
        let lobby: CallAcceptancePush = serde_json::from_value(serde_json::json!({
            "callAcceptance": {
                "acceptedCallModalities": [],
                "links": {"callLeg": "https://example.test/leg"},
                "mediaContent": {"blob": "v=0", "contentType": "application/sdp-ngc-1.0",
                                 "mediaLegId": "AB", "callLabel": "lobby", "fromMixer": true},
                "callKeepAliveInterval": 2700,
                "controllerName": "lobby"
            }
        }))
        .expect("reads");
        assert!(lobby.call_acceptance.is_lobby());
        assert!(!CallAcceptance::default().is_lobby());
        match Push::read(
            "mediaAcknowledgement",
            &fixture("media_acknowledgement_retarget"),
        )
        .expect("reads")
        {
            Push::MediaAcknowledgement(outcome, links) => {
                assert_eq!((outcome.code, outcome.sub_code), (0, 10109));
                assert!(links.call_leg.expect("leg").ends_with("/callLeg"));
                assert!(links.media_renegotiation.is_some());
            }
            other => panic!("{other:?}"),
        }
    }
}
