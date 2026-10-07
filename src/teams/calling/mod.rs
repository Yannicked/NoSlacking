//! Microsoft Teams calls: what the web client does, as
//! `docs/research/teams-calls.md` records it.
//!
//! A call is signalled with JSON over HTTPS through Microsoft's flight
//! proxy, answered by pushes to callback URLs on our Trouter connection,
//! and carried as browser-style WebRTC media (DTLS-SRTP, BUNDLE, Opus),
//! described in Microsoft's own SDP dialect. str0m drives the media
//! through its direct API; the SDP is read and written here.
//!
//! - [`sdp`]: Microsoft's SDP, read into [`RemoteMedia`] and written from
//!   [`LocalMedia`].
//! - [`types`], [`links`], [`api`], [`codes`]: the signalling.
//! - [`media`]: the media session, on the huddle stack's audio pipeline.

pub mod api;
pub mod call;
pub mod codes;
pub mod links;
pub mod media;
pub mod router;
pub mod sdp;
pub mod types;
pub mod video;

use std::net::SocketAddr;

/// The DTLS role an SDP asks for (`a=setup`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Setup {
    /// `actpass`: either; the answerer chooses.
    ActPass,
    /// `active`: this side is the DTLS client.
    Active,
    /// `passive`: this side is the DTLS server.
    Passive,
    /// No `a=setup` at all, as the native client's offers have.
    #[default]
    Unsaid,
}

/// Which way an m-line's media goes, from the side that wrote the SDP.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Direction {
    /// `sendrecv`, or no direction attribute at all (Microsoft's answers
    /// leave it out when it is this).
    #[default]
    SendRecv,
    SendOnly,
    RecvOnly,
    Inactive,
}

/// What an m-line carries.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LineKind {
    Audio,
    Video,
    /// `m=x-data`: the data channel, dressed as an RTP m-line.
    Data,
    Other,
}

/// One m-line of an SDP, as far as a call needs it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Line {
    /// `a=mid`; for a rejected line without one, its position as text.
    pub mid: String,
    pub kind: LineKind,
    /// `a=label` (`main-audio`, `main-video`, `applicationsharing-video`,
    /// `data`), which Microsoft keys its streams by.
    pub label: Option<String>,
    /// The m-line's port; 0 for a rejected line.
    pub port: u16,
    pub direction: Direction,
}

/// What kind of address an ICE candidate is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CandidateKind {
    Host,
    ServerReflexive,
    Relay,
}

/// A UDP ICE candidate for the RTP component. TCP and RTCP candidates are
/// left out when an SDP is read: the call uses UDP and `rtcp-mux`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Candidate {
    pub foundation: String,
    pub priority: u32,
    pub addr: SocketAddr,
    pub kind: CandidateKind,
}

/// What a call takes from the far end's SDP (an answer, an offer, or a
/// renegotiation offer).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RemoteMedia {
    /// The ICE credentials of the bundle's first m-line: the call's one
    /// transport.
    pub ice_ufrag: String,
    pub ice_pwd: String,
    /// The SHA-256 DTLS fingerprint, as written (`AB:CD:…`).
    pub fingerprint: Option<String>,
    pub setup: Setup,
    /// The bundle's UDP candidates for the RTP component.
    pub candidates: Vec<Candidate>,
    /// The Opus payload type, if audio offers or answers Opus.
    pub opus_pt: Option<u8>,
    /// The H.264 the camera line (`main-video`) offers or answers, if it
    /// has one we can use.
    pub video: Option<VideoCodec>,
    /// Every m-line, in order.
    pub lines: Vec<Line>,
}

impl RemoteMedia {
    /// The audio m-line, if there is one.
    pub fn audio(&self) -> Option<&Line> {
        self.lines.iter().find(|l| l.kind == LineKind::Audio)
    }

    /// The camera's m-line: the video line labelled `main-video`, or the
    /// first video line when none is labelled.
    pub fn camera(&self) -> Option<&Line> {
        let video = || self.lines.iter().filter(|l| l.kind == LineKind::Video);
        video()
            .find(|l| l.label.as_deref() == Some(CAMERA_LABEL))
            .or_else(|| video().find(|l| l.label.is_none()))
    }
}

/// The label of the camera's video line, which Microsoft keys it by.
pub const CAMERA_LABEL: &str = "main-video";

/// An H.264 codec on a video line: packetization mode 1, the only one
/// str0m and our encoder speak.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VideoCodec {
    /// Its payload type.
    pub pt: u8,
    /// Its retransmission's (`rtx` with `apt=` it), if listed.
    pub rtx: Option<u8>,
}

/// What our SDP says about us: what [`sdp`] writes an offer or an answer
/// from.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LocalMedia {
    pub ice_ufrag: String,
    pub ice_pwd: String,
    /// Our SHA-256 DTLS fingerprint (`AB:CD:…`).
    pub fingerprint: String,
    pub setup: Setup,
    /// Every candidate we have: host, server reflexive, relay.
    pub candidates: Vec<Candidate>,
    /// The SSRC our audio goes out on (`a=x-ssrc-range`).
    pub audio_ssrc: u32,
    /// The Opus payload type: ours (111) in an offer, the offerer's in an
    /// answer.
    pub opus_pt: u8,
    /// Whether we send audio (unmuted or not, the line stays `sendrecv`;
    /// this is for holding a call, later).
    pub audio_direction: Direction,
    /// The SSRC our camera goes out on, or `None` to offer the camera
    /// line `inactive` and reject it in an answer (a build without
    /// video).
    pub video_ssrc: Option<u32>,
    /// The camera line's H.264 payload type: ours in an offer, the
    /// offerer's in an answer.
    pub video_pt: u8,
    /// Its retransmission's payload type, if lost packets are sent again
    /// (`nack` and `rtx`).
    pub video_rtx: Option<u8>,
    /// Whether we send and receive on the camera line. Kept `sendrecv`
    /// for the whole call, the camera sending only while it is on, so
    /// turning it on needs no renegotiation.
    pub video_direction: Direction,
    /// The SSRC of the data m-line (`m=x-data`), or `None` to leave the
    /// data line out of an offer and reject it in an answer. Nothing
    /// speaks SCTP over it yet; offering it is only to look like the web
    /// client, in case Microsoft's side expects the line.
    pub data_ssrc: Option<u32>,
    /// The origin line's session id (`o=- {id} …`): made up once per call,
    /// kept for every offer and answer of it, as a browser does.
    pub session_id: u64,
    /// The origin line's version, raised by the caller for each new SDP
    /// of the call.
    pub session_version: u32,
}
