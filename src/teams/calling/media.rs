//! The media of a Teams call: UDP, TURN and str0m driven directly, with
//! the huddle audio pipeline.
//!
//! A Teams call is browser WebRTC between two peers, so unlike a huddle
//! (where Chime's media servers sit behind a relay and nothing else) we
//! offer every way to reach us, as the web client does: a host candidate
//! on our UDP socket, the server-reflexive address Microsoft's TURN
//! server sees, and the relay it gives (`docs/research/teams-calls.md`,
//! §D.1). One UDP socket carries both the direct traffic and the TURN
//! client's own messages, so the reflexive address is that of the socket
//! the peer's checks reach; when the TURN server answers only over TCP
//! or TLS, the relay comes from that stream and the reflexive address is
//! left out, being no address of our UDP socket.
//!
//! str0m is driven through its direct API (§F.2): Microsoft's SDP never
//! reaches str0m's parser. [`MediaSession::start`] builds the peer with
//! Opus alone at the call's payload type, declares the audio m-line and
//! our sending SSRC, gathers, and hands back [`LocalMedia`] for the
//! signalling to write its SDP from. [`MediaSession::apply_remote`] takes
//! what the far end's SDP says ([`RemoteMedia`]): ICE credentials,
//! candidates, the fingerprint, and the DTLS role from `a=setup`
//! ([`dtls_active`]). The far end's SDP names no SSRC that str0m could
//! use and its RTP carries no `mid` extension, so the far end's audio
//! SSRC is learnt from its first RTP packet (`rtp_ssrc`) and str0m told
//! to expect it.
//!
//! The DTLS is OpenSSL's, as for huddles ([`crate::huddle_audio::dtls`]):
//! Chime refused str0m's own, and whether Microsoft's stack takes either
//! has not been tried yet.
//!
//! Audio is the huddle's: frames from the microphone pipeline (or the
//! probe's [`Tone`]) arrive as an [`Uplink`], what comes in goes to a
//! [`Feed`] for the jitter buffer and the speaker. Muted, 20 ms of Opus
//! silence goes out every 20 ms, as a muted browser sends and as the
//! huddle does.
//!
//! Every step logs at info level. The ICE password and the TURN
//! credentials never do.

use std::collections::{HashSet, VecDeque};
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use str0m::crypto::Fingerprint;
use str0m::format::{Codec, FormatParams};
use str0m::media::{Frequency, MediaKind, MediaTime, Mid, Pt};
use str0m::net::{Protocol, Receive};
use str0m::{
    Candidate as IceCandidate, CandidateKind as IceKind, Event as RtcEvent, IceConnectionState,
    IceCreds, Input, Output, Rtc, RtcConfig,
};
use tokio::sync::{mpsc, oneshot, watch};

use super::{Candidate, CandidateKind, Direction, LocalMedia, RemoteMedia, Setup};
use crate::huddle_audio::dtls;
use crate::huddle_audio::media::{RelayIo, SILENT_OPUS, Uplink, connect_relay};
use crate::huddle_audio::microphone::ToneSource;
use crate::huddle_audio::speaker::Feed;
use crate::huddle_audio::turn::{self, Server, Transport};
use crate::huddle_audio::uplink::{Outbound, Outgoing, Stamp};

/// How long one TURN server, over one transport, may take to give a
/// relay before the next is tried.
const RELAY_TIMEOUT: Duration = Duration::from_secs(5);
/// How long ICE and DTLS may take once the far end's media is known. A
/// peer's checks can take longer than a media server's, so more than the
/// huddle's 15 s.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(20);
/// How long ICE may stay disconnected, once it was up, before the call's
/// media counts as lost: a peer on Wi-Fi drops checks for a moment.
const RECONNECT_GRACE: Duration = Duration::from_secs(10);
const STATS_EVERY: Duration = Duration::from_secs(5);
const AUDIO_TICK: Duration = Duration::from_millis(20);
/// The audio m-line's mid in our offer (§D.1).
pub const AUDIO_MID: &str = "0";
/// Our camera line's mid in an offer, as the web client's (§D.1).
pub const VIDEO_MID: &str = "1";
/// Our screen share line's mid in an offer.
pub const SHARE_MID: &str = "2";
/// Our Opus payload type in an offer, as the web client's (§D.1).
pub const OPUS_PT: u8 = 111;
/// The largest datagram read: more than any Ethernet MTU.
const DATAGRAM: usize = 2048;

/// One of Microsoft's TURN servers, as the client configuration names it
/// (`Turn.addresses`, `udpPort`, `tcpPort`, `tlsPort`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RelayServer {
    /// Its name, `gateway-eu.az.relay.teams.cloud.microsoft` and the like.
    pub host: String,
    /// TURN over UDP, tried first.
    pub udp_port: Option<u16>,
    /// TURN over plain TCP, for networks that block UDP.
    pub tcp_port: Option<u16>,
    /// TURN over TLS (`turns:`), for networks that allow only HTTPS.
    pub tls_port: Option<u16>,
}

/// Microsoft's TURN relay for a call: its servers and the credentials
/// from `trap/tokens`.
#[derive(Clone, PartialEq, Eq)]
pub struct Relay {
    /// The servers, in the order to try them.
    pub servers: Vec<RelayServer>,
    /// The realm Microsoft names (`rtcmedia`). The TURN client answers
    /// the realm the server's 401 names, which should be this one; it is
    /// kept for the log.
    pub realm: String,
    /// The TURN username. Secret.
    pub username: String,
    /// The TURN password. Secret.
    pub password: String,
}

impl Relay {
    /// The relay from what [`super::api::relay_servers`] and
    /// [`super::api::relay_credentials`] fetch; the credentials' realm
    /// wins, being the one the server will name. A port of 0 is taken as
    /// none.
    pub fn new(
        servers: &super::api::RelayServers,
        credentials: super::api::RelayCredentials,
    ) -> Self {
        let port = |p: u16| (p != 0).then_some(p);
        Self {
            servers: servers
                .hosts
                .iter()
                .map(|host| RelayServer {
                    host: host.clone(),
                    udp_port: port(servers.udp_port),
                    tcp_port: port(servers.tcp_port),
                    tls_port: port(servers.tls_port),
                })
                .collect(),
            realm: if credentials.realm.is_empty() {
                servers.realm.clone()
            } else {
                credentials.realm
            },
            username: credentials.username,
            password: credentials.password,
        }
    }
}

impl std::fmt::Debug for Relay {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Relay")
            .field("servers", &self.servers)
            .field("realm", &self.realm)
            .field("username", &crate::redact::REDACTED)
            .field("password", &crate::redact::REDACTED)
            .finish()
    }
}

/// What a media session is started with.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MediaConfig {
    /// Microsoft's TURN relay; without one only the host candidate is
    /// offered, which reaches a peer on the same network only.
    pub relay: Option<Relay>,
    /// Whether we are the ICE controlling agent: the offerer is.
    pub controlling: bool,
    /// The Opus payload type: ours ([`OPUS_PT`]) in an offer, the
    /// offerer's in an answer. str0m knows Opus at this number only.
    pub opus_pt: u8,
    /// The audio m-line's mid: [`AUDIO_MID`] in an offer, the offerer's
    /// in an answer. str0m writes it in its RTP `mid` extension.
    pub audio_mid: String,
    /// The address to offer as our host candidate. Left out, it is the
    /// address the system would send to the internet from; tests set it
    /// to the loopback address.
    pub host: Option<IpAddr>,
    /// The camera line, in a build with video: its mid and H.264 payload
    /// type (ours in an offer, the offerer's in an answer).
    pub video: Option<VideoLine>,
    /// The screen share's line, likewise.
    pub share: Option<VideoLine>,
    /// Whether to offer (or answer) the data line and open the meeting's
    /// data channel on it ([`super::channel`]): a meeting asks for
    /// video there.
    pub data: bool,
    /// More camera lines, receive-only: a meeting shows one more
    /// participant's camera on each.
    pub cameras: Vec<VideoLine>,
}

/// The camera's m-line, as a media session is started with it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VideoLine {
    pub mid: String,
    pub pt: u8,
    /// The retransmission's payload type, if lost packets are asked for
    /// again.
    pub rtx: Option<u8>,
}

/// Whether this build shows or sends video at all.
const HAS_VIDEO: bool = cfg!(any(feature = "huddle-video", feature = "huddle-camera"));

/// The H.264 payload type of a meeting's media server, and of its
/// resends (recorded).
pub const MEETING_VIDEO_PT: u8 = 107;
pub const MEETING_VIDEO_RTX: u8 = 99;

/// The H.264 payload type of our offer's camera line: the web client's
/// constrained baseline number, moved aside if Opus has it (in a bundle a
/// payload type must not mean two codecs).
pub fn offer_video_pt(opus_pt: u8) -> u8 {
    if opus_pt == 108 || opus_pt == 109 {
        118
    } else {
        108
    }
}

impl MediaConfig {
    /// A configuration for an outgoing call's offer: Opus at
    /// [`OPUS_PT`], mid [`AUDIO_MID`], and the host address found from the
    /// routing table.
    pub fn offer(relay: Option<Relay>) -> Self {
        Self {
            relay,
            controlling: true,
            opus_pt: OPUS_PT,
            audio_mid: AUDIO_MID.to_owned(),
            host: None,
            video: HAS_VIDEO.then(|| VideoLine {
                mid: VIDEO_MID.to_owned(),
                pt: offer_video_pt(OPUS_PT),
                rtx: Some(offer_video_pt(OPUS_PT) + 1),
            }),
            // The same H.264 as the camera's: one codec, one number.
            share: HAS_VIDEO.then(|| VideoLine {
                mid: SHARE_MID.to_owned(),
                pt: offer_video_pt(OPUS_PT),
                rtx: Some(offer_video_pt(OPUS_PT) + 1),
            }),
            data: false,
            cameras: Vec::new(),
        }
    }

    /// A configuration for joining a meeting: an offer as
    /// [`Self::offer`]'s, but with H.264 at [`MEETING_VIDEO_PT`]. A
    /// meeting's media server answers the camera at that number whatever
    /// was offered (recorded: offered 108, answered 107), and video at a
    /// number the line does not know would go unseen both ways.
    pub fn meeting(relay: Option<Relay>) -> Self {
        let line = |mid: &str| VideoLine {
            mid: mid.to_owned(),
            pt: MEETING_VIDEO_PT,
            rtx: Some(MEETING_VIDEO_RTX),
        };
        // After the data line's mid, 3.
        let cameras = if HAS_VIDEO {
            ["4", "5", "6"].into_iter().map(line).collect()
        } else {
            Vec::new()
        };
        Self {
            video: HAS_VIDEO.then(|| line(VIDEO_MID)),
            share: HAS_VIDEO.then(|| line(SHARE_MID)),
            data: true,
            cameras,
            ..Self::offer(relay)
        }
    }

    /// A configuration for answering the caller's `offer`: ICE controlled,
    /// Opus at the payload type and the audio mid the offer gave.
    pub fn answer(relay: Option<Relay>, offer: &RemoteMedia) -> Self {
        Self {
            relay,
            controlling: false,
            opus_pt: offer.opus_pt.unwrap_or(OPUS_PT),
            audio_mid: offer
                .audio()
                .map_or_else(|| AUDIO_MID.to_owned(), |line| line.mid.clone()),
            host: None,
            video: offer
                .camera()
                .zip(offer.video)
                .filter(|_| HAS_VIDEO)
                .map(|(line, codec)| VideoLine {
                    mid: line.mid.clone(),
                    pt: codec.pt,
                    rtx: codec.rtx,
                }),
            share: offer
                .share()
                .zip(offer.share_video.or(offer.video))
                .filter(|_| HAS_VIDEO)
                .map(|(line, codec)| VideoLine {
                    mid: line.mid.clone(),
                    pt: codec.pt,
                    rtx: codec.rtx,
                }),
            data: false,
            cameras: Vec::new(),
        }
    }

    /// A meeting's answer to its media server's `offer`: as
    /// [`Self::answer`]'s, with the data channel and as many more of its
    /// camera lines as a meeting offer of ours has.
    pub fn meeting_answer(relay: Option<Relay>, offer: &RemoteMedia) -> Self {
        let cameras = match offer.video.filter(|_| HAS_VIDEO) {
            Some(codec) => offer
                .other_cameras()
                .into_iter()
                .take(MORE_CAMERAS)
                .map(|line| VideoLine {
                    mid: line.mid.clone(),
                    pt: codec.pt,
                    rtx: codec.rtx,
                })
                .collect(),
            None => Vec::new(),
        };
        Self {
            data: true,
            cameras,
            ..Self::answer(relay, offer)
        }
    }
}

/// How many more cameras than one a meeting shows.
pub const MORE_CAMERAS: usize = 3;

/// Where the sound goes and comes from.
#[derive(Debug, Default)]
pub struct Audio {
    /// The far end's Opus frames go here, to the jitter buffer and the
    /// speaker; dropped when there is none (a headless probe).
    pub feed: Option<Feed>,
    /// Our encoded frames and whether the microphone is muted, as
    /// `backend/listen.rs` wires them for a huddle; without one we stay
    /// muted and send silence.
    pub uplink: Option<Uplink>,
    /// Where the far end's camera is shown and ours comes from.
    pub video: super::video::Video,
}

/// A call's sound and pictures, taken back from a media session that
/// stopped ([`MediaSession::release`]) for the next session of the same
/// call: a meeting moves you to another media server when you are let in
/// from its lobby, with new keys, which takes a new session (§H.4).
#[derive(Default)]
pub struct Held {
    feed: Option<Feed>,
    uplink: Option<Uplink>,
    camera: super::video::Ends,
    share: super::video::Ends,
    /// More camera lines' ends.
    more: Vec<super::video::Ends>,
}

impl std::fmt::Debug for Held {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Held")
            .field("feed", &self.feed.is_some())
            .field("uplink", &self.uplink.is_some())
            .finish_non_exhaustive()
    }
}

impl From<Audio> for Held {
    fn from(audio: Audio) -> Self {
        let Audio {
            feed,
            uplink,
            video,
        } = audio;
        let (camera, share) = video.split();
        Self {
            feed,
            uplink,
            camera,
            share,
            more: Vec::new(),
        }
    }
}

/// The probe's quiet 440 Hz tone in place of the microphone, through the
/// same Opus encoder, so a call can be heard working without anyone
/// talking. The tone stops when this is dropped.
#[derive(Debug)]
pub struct Tone {
    _source: ToneSource,
    /// Held so the session does not take a closed channel for a mute.
    _unmuted: watch::Sender<bool>,
}

impl Tone {
    /// Starts the tone, unmuted, and hands back the [`Uplink`] it
    /// arrives on, for [`Audio::uplink`].
    pub fn start() -> Result<(Self, Uplink), String> {
        let (frames, frames_in) = mpsc::channel(25);
        let source = ToneSource::start(frames)?;
        let (unmuted, muted) = watch::channel(false);
        Ok((
            Self {
                _source: source,
                _unmuted: unmuted,
            },
            Uplink {
                frames: frames_in,
                muted,
            },
        ))
    }
}

/// The step that failed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stage {
    /// Opening the socket and finding candidates.
    Gather,
    /// The far end's media description could not be used.
    Remote,
    /// ICE and DTLS did not come up in time.
    Connect,
    /// The connection broke during the call.
    Media,
}

/// Why the media stopped before it was asked to. The worker turns it
/// into a [`crate::failure::Failure`] for the interface; `why` is for the
/// log only.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Failure {
    /// Where it failed.
    pub stage: Stage,
    /// Why, for the log.
    pub why: String,
}

impl std::fmt::Display for Failure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:?}: {}", self.stage, self.why)
    }
}

fn failure(stage: Stage, why: impl Into<String>) -> Failure {
    Failure {
        stage,
        why: why.into(),
    }
}

/// What a media session tells as it goes. The last is always
/// [`MediaEvent::Failed`] or [`MediaEvent::Stopped`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MediaEvent {
    /// ICE, DTLS and SRTP are up.
    Connected,
    /// Audio has gone out and come in: the call can be heard both ways.
    AudioFlowing,
    /// The far end's camera on camera line `line` started (`on`) or
    /// stopped showing: the far end's own on line 0; in a meeting, one
    /// participant's on each line.
    FarCamera { line: usize, on: bool },
    /// The far end's screen share started (`true`) or stopped showing.
    FarShare(bool),
    /// The meeting's data channel opened.
    ChannelOpen,
    /// A message on the meeting's data channel.
    ChannelData(Vec<u8>),
    /// The media broke; nothing more comes.
    Failed(Failure),
    /// Stopped as asked.
    Stopped,
}

/// What went over the wire so far, for a probe's log.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Counts {
    /// Datagrams in: from the socket, and relayed through TURN.
    pub packets_in: u64,
    /// Their bytes.
    pub bytes_in: u64,
    /// Datagrams str0m sent: ICE, DTLS, RTP and RTCP.
    pub packets_out: u64,
    /// Their bytes.
    pub bytes_out: u64,
    /// Opus frames received.
    pub audio_in: u64,
    /// Opus frames sent, silence included.
    pub audio_out: u64,
    /// Of those, frames of silence sent while muted.
    pub silent_out: u64,
}

/// [`Counts`], kept where the session and its handle both reach.
#[derive(Debug, Default)]
struct Counters {
    packets_in: AtomicU64,
    bytes_in: AtomicU64,
    packets_out: AtomicU64,
    bytes_out: AtomicU64,
    audio_in: AtomicU64,
    audio_out: AtomicU64,
    silent_out: AtomicU64,
}

impl Counters {
    fn snapshot(&self) -> Counts {
        let get = |c: &AtomicU64| c.load(Ordering::Relaxed);
        Counts {
            packets_in: get(&self.packets_in),
            bytes_in: get(&self.bytes_in),
            packets_out: get(&self.packets_out),
            bytes_out: get(&self.bytes_out),
            audio_in: get(&self.audio_in),
            audio_out: get(&self.audio_out),
            silent_out: get(&self.silent_out),
        }
    }
}

fn bump(counter: &AtomicU64, by: usize) {
    counter.fetch_add(by as u64, Ordering::Relaxed);
}

/// What the far end's media description asks of str0m, checked and
/// converted before it is handed to the session.
#[derive(Clone)]
struct Plan {
    creds: IceCreds,
    fingerprint: Vec<u8>,
    /// Whether we are the DTLS client.
    active: bool,
    candidates: Vec<IceCandidate>,
    /// The far end's addresses the TURN server must let through.
    permits: Vec<IpAddr>,
    /// Whether to send audio, and whether to play what comes.
    send: bool,
    receive: bool,
    /// Whether to send our camera and show the far end's, if the
    /// description has a camera line.
    video: Option<(bool, bool)>,
    /// Whether to send our screen and show the far end's, if the
    /// description has the share line in use.
    share: Option<(bool, bool)>,
    /// The SSRCs the far end sends each on, as its lines say.
    video_ssrcs: Option<(u32, u32)>,
    share_ssrcs: Option<(u32, u32)>,
    /// Every video line's mid, what flows on it, and its SSRCs: for more
    /// camera lines.
    lines: Vec<LineFlows>,
}

/// What flows on one video line of the far end's description, and the
/// SSRCs it sends there.
#[derive(Clone, Debug)]
struct LineFlows {
    mid: String,
    /// Whether we send, and whether we receive.
    flows: (bool, bool),
    ssrcs: Option<(u32, u32)>,
}

impl std::fmt::Debug for Plan {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Plan")
            .field("ufrag", &self.creds.ufrag)
            .field("pass", &crate::redact::REDACTED)
            .field("active", &self.active)
            .field("candidates", &self.candidates.len())
            .field("permits", &self.permits)
            .field("send", &self.send)
            .field("receive", &self.receive)
            .finish_non_exhaustive()
    }
}

/// What the handle asks of the session.
enum Command {
    Apply(Box<Plan>),
    Muted(bool),
    Stop,
    /// Stop, and hand back the sound and pictures.
    Release(oneshot::Sender<Held>),
    /// Send this on the meeting's data channel.
    Data(Vec<u8>),
}

/// A running media session. Dropping it stops the session too, without
/// waiting for [`MediaEvent::Stopped`].
pub struct MediaSession {
    commands: mpsc::UnboundedSender<Command>,
    events: mpsc::UnboundedReceiver<MediaEvent>,
    counters: Arc<Counters>,
    opus_pt: u8,
}

impl std::fmt::Debug for MediaSession {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MediaSession")
            .field("opus_pt", &self.opus_pt)
            .field("counts", &self.counts())
            .finish_non_exhaustive()
    }
}

impl MediaSession {
    /// Opens the socket, gets a relay (when `config` has one), builds the
    /// peer and runs it on a task of its own; hands back the session and
    /// what our SDP is to say, its candidates all gathered (Microsoft's
    /// clients send no trickle candidates). `setup` in it is `actpass`,
    /// for an offer; an answer writes [`super::sdp::answer_setup`] instead.
    pub async fn start(
        config: MediaConfig,
        audio: Audio,
    ) -> Result<(MediaSession, LocalMedia), Failure> {
        Self::resume(config, Held::from(audio)).await
    }

    /// Starts a session as [`Self::start`] does, on the sound and
    /// pictures an earlier session of the call handed back.
    pub async fn resume(
        config: MediaConfig,
        held: Held,
    ) -> Result<(MediaSession, LocalMedia), Failure> {
        let (commands, commands_in) = mpsc::unbounded_channel();
        let (tell, events) = mpsc::unbounded_channel();
        let (gathered, gathering) = oneshot::channel();
        let counters = Arc::new(Counters::default());
        let opus_pt = config.opus_pt;
        let session = Session::open(config, held, tell, gathered, counters.clone()).await?;
        tokio::spawn(run(session, commands_in));
        let local = gathering
            .await
            .map_err(|_| failure(Stage::Gather, "the session ended while gathering"))??;
        Ok((
            MediaSession {
                commands,
                events,
                counters,
                opus_pt,
            },
            local,
        ))
    }

    /// Takes the far end's media description (an answer to our offer, an
    /// offer we answer, or a renegotiation): what is new in it is applied,
    /// what is already known is left alone, so a renegotiation does not
    /// restart ICE. Refused here, before the session sees it, when it
    /// cannot be used.
    pub fn apply_remote(&self, remote: &RemoteMedia) -> Result<(), Failure> {
        let plan = plan(remote, self.opus_pt).map_err(|why| failure(Stage::Remote, why))?;
        self.commands
            .send(Command::Apply(Box::new(plan)))
            .map_err(|_| failure(Stage::Media, "the media session is over"))
    }

    /// Mutes or unmutes us. The microphone's own mute (the [`Uplink`]'s)
    /// still counts: we are heard only when neither is muted.
    pub fn set_muted(&self, muted: bool) {
        let _ = self.commands.send(Command::Muted(muted));
    }

    /// Asks the session to stop; [`MediaEvent::Stopped`] follows.
    pub fn stop(&self) {
        let _ = self.commands.send(Command::Stop);
    }

    /// Sends `message` on the meeting's data channel, once it is open.
    pub fn send_data(&self, message: Vec<u8>) {
        let _ = self.commands.send(Command::Data(message));
    }

    /// Stops the session and waits for its sound and pictures, for
    /// [`Self::resume`]; `None` if it had already ended, taking them
    /// with it.
    pub async fn release(&self) -> Option<Held> {
        let (give, given) = oneshot::channel();
        self.commands.send(Command::Release(give)).ok()?;
        given.await.ok()
    }

    /// The next event, or `None` once the session is over and every event
    /// has been read.
    pub async fn next_event(&mut self) -> Option<MediaEvent> {
        self.events.recv().await
    }

    /// What went over the wire so far.
    pub fn counts(&self) -> Counts {
        self.counters.snapshot()
    }
}

/// Whether we are the DTLS client, from the far end's `a=setup`: passive
/// makes us active; active makes us passive; `actpass` in an offer leaves
/// it to us and we answer active, as the web client does; and the native
/// client's offers, which say nothing, were answered active by the web
/// client too (§D.3), where str0m's own SDP code would go passive. The
/// same choice as the answer [`super::sdp::answer_setup`] writes, so the
/// SDP and the handshake agree.
pub fn dtls_active(remote: Setup) -> bool {
    super::sdp::answer_setup(remote) == Setup::Active
}

/// Whether we send and whether we receive, from the direction the far
/// end wrote for its audio.
fn flows(remote: Direction) -> (bool, bool) {
    match remote {
        Direction::SendRecv => (true, true),
        Direction::SendOnly => (false, true),
        Direction::RecvOnly => (true, false),
        Direction::Inactive => (false, false),
    }
}

/// Reads a SHA-256 fingerprint written `AB:CD:…`.
fn parse_fingerprint(text: &str) -> Option<Vec<u8>> {
    let bytes = text
        .trim()
        .split(':')
        .map(|pair| {
            if pair.len() == 2 {
                u8::from_str_radix(pair, 16).ok()
            } else {
                None
            }
        })
        .collect::<Option<Vec<u8>>>()?;
    (bytes.len() == 32).then_some(bytes)
}

/// Writes a fingerprint as SDP does: `AB:CD:…`, upper case.
fn format_fingerprint(bytes: &[u8]) -> String {
    bytes
        .iter()
        .map(|b| format!("{b:02X}"))
        .collect::<Vec<_>>()
        .join(":")
}

/// Whether an address could be reached at all: not a wildcard, not
/// multicast or broadcast, not link-local, and with a port. Loopback is
/// allowed, for tests.
fn usable(addr: SocketAddr) -> bool {
    if addr.port() == 0 {
        return false;
    }
    match addr.ip() {
        IpAddr::V4(ip) => {
            !ip.is_unspecified() && !ip.is_multicast() && !ip.is_broadcast() && !ip.is_link_local()
        }
        IpAddr::V6(ip) => !ip.is_unspecified() && !ip.is_multicast(),
    }
}

/// The far end's candidate for str0m, keeping the priority and foundation
/// it wrote so both ends rank the pairs alike; `None` for one that cannot
/// be reached.
fn ice_candidate(candidate: &Candidate) -> Option<IceCandidate> {
    if !usable(candidate.addr) {
        return None;
    }
    let kind = match candidate.kind {
        CandidateKind::Host => IceKind::Host,
        CandidateKind::ServerReflexive => IceKind::ServerReflexive,
        CandidateKind::Relay => IceKind::Relayed,
    };
    Some(IceCandidate::from_parts(
        candidate.foundation.clone(),
        1,
        Protocol::Udp,
        candidate.priority,
        candidate.addr,
        kind,
        None,
        None,
        None,
    ))
}

/// The far end's addresses our TURN relay must let through: every
/// candidate's, of the relay's address family, once each. A peer's
/// packets can come from any of them, not one media server's.
fn permissions(remote: &RemoteMedia, ipv4: bool) -> Vec<IpAddr> {
    let mut ips: Vec<IpAddr> = remote
        .candidates
        .iter()
        // A relay refuses private and loopback addresses (403), and
        // would only be asked again and again.
        .filter(|c| usable(c.addr) && c.addr.is_ipv4() == ipv4 && public(c.addr.ip()))
        .map(|c| c.addr.ip())
        .collect();
    ips.sort();
    ips.dedup();
    ips
}

/// Whether `ip` can be reached across the internet: not a private,
/// loopback or unique-local address.
fn public(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(ip) => !(ip.is_private() || ip.is_loopback() || ip.is_link_local()),
        IpAddr::V6(ip) => !(ip.is_loopback() || (ip.segments()[0] & 0xfe00) == 0xfc00),
    }
}

/// The DTLS datagrams of the last flight we sent, kept to send again.
///
/// str0m's OpenSSL backend never retransmits a lost handshake flight
/// (its timeout only notes the next deadline), and across two NATs the
/// first ClientHello is often lost as the path opens: the call then
/// never comes up. So while the handshake runs, a flight nothing has
/// answered is sent again, as DTLS (RFC 6347 §4.2.4) would: after 1 s,
/// then 2, then 4, a few times. A repeated handshake record is harmless
/// to the far end.
#[derive(Debug, Default)]
struct Flight {
    /// Each datagram with whether it went through the relay, and to where.
    datagrams: Vec<(bool, SocketAddr, Vec<u8>)>,
    /// Whether the far end has said anything since this flight began: the
    /// next datagram we send starts a new flight.
    answered: bool,
    /// When to send it again.
    resend_at: Option<Instant>,
    /// The wait before that.
    wait: Duration,
    /// How often it was sent again.
    tries: u32,
}

/// How many times a flight is sent again before the connection timeout
/// is left to end the call.
const FLIGHT_TRIES: u32 = 6;

impl Flight {
    /// We sent a DTLS datagram at `now`.
    fn sent(&mut self, relayed: bool, to: SocketAddr, data: &[u8], now: Instant) {
        if self.answered || self.datagrams.is_empty() {
            self.datagrams.clear();
            self.answered = false;
            self.tries = 0;
            self.wait = Duration::from_secs(1);
            self.resend_at = Some(now + self.wait);
        }
        self.datagrams.push((relayed, to, data.to_vec()));
    }

    /// The far end sent a DTLS datagram: it heard us.
    fn heard(&mut self) {
        self.answered = true;
        self.resend_at = None;
    }

    /// Whether to send the flight again at `now`; moves the next time on.
    fn due(&mut self, now: Instant) -> bool {
        if !self.resend_at.is_some_and(|at| at <= now) || self.datagrams.is_empty() {
            return false;
        }
        self.tries += 1;
        self.wait = (self.wait * 2).min(Duration::from_secs(4));
        self.resend_at = (self.tries < FLIGHT_TRIES).then(|| now + self.wait);
        true
    }
}

/// Whether `data` is a DTLS record (RFC 7983's first byte 20 to 63).
fn is_dtls(data: &[u8]) -> bool {
    matches!(data.first(), Some(20..=63))
}

/// What went which way while connecting, by path and kind: which path
/// ICE settled on, and whether the DTLS handshake crossed it, shows in
/// the log when a call does not come up.
#[derive(Clone, Copy, Debug, Default)]
struct Paths {
    /// `[in, out][direct, relayed][stun, dtls, rtp]`.
    counts: [[[u64; 3]; 2]; 2],
}

impl Paths {
    /// Counts a datagram going `out` (or in) on the relay (or directly).
    fn count(&mut self, out: bool, relayed: bool, data: &[u8]) {
        // RFC 7983's demultiplexing by the first byte.
        let kind = match data.first() {
            Some(0..=3) => 0,
            Some(20..=63) => 1,
            Some(128..=191) => 2,
            _ => return,
        };
        self.counts[usize::from(out)][usize::from(relayed)][kind] += 1;
    }

    /// One line for the log.
    fn line(&self) -> String {
        let side = |dir: usize, path: usize| {
            let [stun, dtls, rtp] = self.counts[dir][path];
            format!("stun {stun} dtls {dtls} rtp {rtp}")
        };
        format!(
            "direct in [{}] out [{}]; relayed in [{}] out [{}]",
            side(0, 0),
            side(1, 0),
            side(0, 1),
            side(1, 1)
        )
    }
}

/// Checks the far end's media description and turns it into what str0m
/// is told; `opus_pt` is ours, which an answer must keep.
fn plan(remote: &RemoteMedia, opus_pt: u8) -> Result<Plan, String> {
    if remote.ice_ufrag.is_empty() || remote.ice_pwd.is_empty() {
        return Err("no ICE credentials".into());
    }
    let fingerprint = remote.fingerprint.as_deref().ok_or("no DTLS fingerprint")?;
    let fingerprint =
        parse_fingerprint(fingerprint).ok_or("the DTLS fingerprint is not a SHA-256 one")?;
    let audio = remote.audio().ok_or("no audio m-line")?;
    if audio.port == 0 {
        return Err("the audio m-line is refused".into());
    }
    match remote.opus_pt {
        Some(pt) if pt == opus_pt => {}
        Some(pt) => return Err(format!("Opus at payload type {pt}, not {opus_pt}")),
        None => return Err("no Opus".into()),
    }
    let candidates: Vec<IceCandidate> =
        remote.candidates.iter().filter_map(ice_candidate).collect();
    if candidates.is_empty() {
        return Err("no candidate to reach".into());
    }
    let (send, receive) = flows(audio.direction);
    let video = remote
        .camera()
        .filter(|_| remote.video.is_some())
        .map(|line| flows(line.direction));
    let share = remote
        .share()
        .filter(|line| line.port != 0 && remote.share_video.is_some())
        .map(|line| flows(line.direction));
    let video_ssrcs = remote.camera().and_then(|line| line.ssrc_range);
    let lines = remote
        .lines
        .iter()
        .filter(|l| l.kind == super::LineKind::Video)
        .map(|l| {
            let flows = if l.port == 0 {
                (false, false)
            } else {
                flows(l.direction)
            };
            LineFlows {
                mid: l.mid.clone(),
                flows,
                ssrcs: l.ssrc_range,
            }
        })
        .collect();
    let share_ssrcs = remote.share().and_then(|line| line.ssrc_range);
    Ok(Plan {
        creds: IceCreds {
            ufrag: remote.ice_ufrag.clone(),
            pass: remote.ice_pwd.clone(),
        },
        fingerprint,
        active: dtls_active(remote.setup),
        candidates,
        permits: permissions(remote, true),
        send,
        receive,
        video,
        share,
        video_ssrcs,
        share_ssrcs,
        lines,
    })
}

/// What has been applied of the far end's descriptions so far.
#[derive(Debug, Default)]
struct Applied {
    creds: Option<IceCreds>,
    fingerprint: Option<Vec<u8>>,
    dtls: bool,
    candidates: HashSet<SocketAddr>,
}

/// Applies what is new in `plan` to `rtc`; says what was odd about it.
fn apply(rtc: &mut Rtc, applied: &mut Applied, plan: &Plan) -> Result<Vec<String>, String> {
    let mut notes = Vec::new();
    if applied.creds.as_ref() != Some(&plan.creds) {
        if applied.creds.is_some() {
            notes.push("the far end's ICE credentials changed: an ICE restart".to_owned());
        }
        rtc.direct_api()
            .set_remote_ice_credentials(plan.creds.clone());
        applied.creds = Some(plan.creds.clone());
    }
    match &applied.fingerprint {
        None => {
            rtc.direct_api().set_remote_fingerprint(Fingerprint {
                hash_func: "sha-256".into(),
                bytes: plan.fingerprint.clone(),
            });
            applied.fingerprint = Some(plan.fingerprint.clone());
        }
        Some(known) if *known != plan.fingerprint => {
            notes.push("the far end's DTLS fingerprint changed; keeping the first".to_owned());
        }
        Some(_) => {}
    }
    for candidate in &plan.candidates {
        if applied.candidates.insert(candidate.addr()) {
            rtc.add_remote_candidate(candidate.clone());
        }
    }
    if !applied.dtls {
        rtc.direct_api()
            .start_dtls(plan.active)
            .map_err(|e| format!("DTLS would not start: {e}"))?;
        applied.dtls = true;
    }
    Ok(notes)
}

/// The peer: Opus alone at `opus_pt`, OpenSSL's DTLS, full ICE.
fn new_rtc(opus_pt: u8, video: &[(u8, Option<u8>)], controlling: bool, now: Instant) -> Rtc {
    let mut config = RtcConfig::new()
        .clear_codecs()
        .set_ice_lite(false)
        .set_crypto_provider(Arc::new(dtls::provider()))
        // Brings the far end's RTCP receiver reports on what we send.
        .set_stats_interval(Some(STATS_EVERY));
    config.codec_config().add_config(
        Pt::from(opus_pt),
        None,
        Codec::Opus,
        Frequency::FORTY_EIGHT_KHZ,
        Some(2),
        FormatParams {
            min_p_time: Some(10),
            use_inband_fec: Some(true),
            ..Default::default()
        },
    );
    // Each video line's H.264, once per number.
    let mut registered = Vec::new();
    for &(pt, rtx) in video {
        if registered.contains(&pt) {
            continue;
        }
        registered.push(pt);
        // H.264 as our encoder makes it and the far end sends it:
        // packetization mode 1, constrained baseline, any level the other
        // side likes.
        config.codec_config().add_config(
            Pt::from(pt),
            rtx.map(Pt::from),
            Codec::H264,
            Frequency::NINETY_KHZ,
            None,
            FormatParams {
                level_asymmetry_allowed: Some(true),
                packetization_mode: Some(1),
                profile_level_id: Some(0x42e01f),
                ..Default::default()
            },
        );
    }
    let mut rtc = config.build(now);
    rtc.direct_api().set_ice_controlling(controlling);
    rtc
}

/// Declares the audio m-line, sending on `ssrc`.
fn declare_audio(rtc: &mut Rtc, mid: Mid, ssrc: u32) {
    let mut api = rtc.direct_api();
    api.declare_media(mid, MediaKind::Audio);
    api.declare_stream_tx(ssrc.into(), None, mid, None);
}

/// The SSRC of an RTP packet carrying payload type `pt`; `None` for
/// anything else (STUN, DTLS, RTCP, other payloads). SRTP leaves the
/// header in the clear, so this reads it before str0m decrypts it.
pub(super) fn rtp_ssrc(data: &[u8], pt: u8) -> Option<u32> {
    let [first, second, _, _, _, _, _, _, a, b, c, d, ..] = *data else {
        return None;
    };
    // RFC 7983: 128 to 191 is RTP or RTCP; RTCP's packet types, 192 to
    // 223 in the second byte, would read as payload types 64 to 95 with
    // the marker set.
    if !(128..=191).contains(&first) || (192..=223).contains(&second) {
        return None;
    }
    ((second & 0x7F) == pt).then(|| u32::from_be_bytes([a, b, c, d]))
}

/// Tells str0m to expect the far end's audio on the SSRC `data` carries,
/// if it is RTP at `pt` on an SSRC not seen before.
fn expect_remote(rtc: &mut Rtc, mid: Mid, pt: u8, seen: &mut HashSet<u32>, data: &[u8]) {
    if let Some(ssrc) = rtp_ssrc(data, pt)
        && seen.insert(ssrc)
    {
        log::info!("media: the far end's audio comes on SSRC {ssrc}");
        rtc.direct_api()
            .expect_stream_rx(ssrc.into(), None, mid, None);
    }
}

/// Writes one Opus packet: its RTP time and marker from `stamp`, its
/// level for RFC 6464 (0 loudest, 127 silence). False if it could not.
fn write_opus(
    rtc: &mut Rtc,
    mid: Mid,
    pt: u8,
    stamp: Stamp,
    payload: Vec<u8>,
    level: (u8, bool),
) -> bool {
    let Some(writer) = rtc.writer(mid) else {
        return false;
    };
    // str0m takes the level negative, 0 to -127.
    let (level, voice) = level;
    let negative = -i8::try_from(level.min(127)).unwrap_or(127);
    let time = MediaTime::new(stamp.time, Frequency::FORTY_EIGHT_KHZ);
    match writer
        .start_of_talkspurt(stamp.talkspurt)
        .audio_level(negative, voice)
        .write(Pt::from(pt), Instant::now(), time, payload)
    {
        Ok(()) => true,
        Err(error) => {
            log::debug!("media: could not send audio: {error}");
            false
        }
    }
}

/// The foundation we write for each kind of candidate: one base each.
fn foundation(kind: CandidateKind) -> &'static str {
    match kind {
        CandidateKind::Host => "1",
        CandidateKind::ServerReflexive => "2",
        CandidateKind::Relay => "3",
    }
}

/// A candidate of ours as the SDP lists it.
fn listed(addr: SocketAddr, kind: CandidateKind, priority: u32) -> Candidate {
    Candidate {
        foundation: foundation(kind).to_owned(),
        priority,
        addr,
        kind,
    }
}

/// What our SDP is to say, from the peer's credentials and fingerprint
/// and the candidates gathered: offered `actpass`, sending both ways.
fn local_media(
    creds: &IceCreds,
    fingerprint: &[u8],
    candidates: Vec<Candidate>,
    audio_ssrc: u32,
    opus_pt: u8,
) -> LocalMedia {
    LocalMedia {
        ice_ufrag: creds.ufrag.clone(),
        ice_pwd: creds.pass.clone(),
        fingerprint: format_fingerprint(fingerprint),
        setup: Setup::ActPass,
        candidates,
        audio_ssrc,
        opus_pt,
        audio_direction: Direction::SendRecv,
        // No camera line until the session says it has one.
        video_ssrc: None,
        video_pt: offer_video_pt(opus_pt),
        video_rtx: None,
        video_rtx_ssrc: None,
        share_ssrc: None,
        share_rtx_ssrc: None,
        share_pt: offer_video_pt(opus_pt),
        share_rtx: None,
        sharing: false,
        receive_cameras: Vec::new(),
        video_direction: Direction::SendRecv,
        // No data channel offered: audio only, the third attempt of §F.3.
        data_ssrc: None,
        session_id: rand::random::<u64>() >> 1,
        // The o= line's first version; the signalling raises it per
        // renegotiation.
        session_version: 2,
    }
}

/// Each way to reach a TURN server, in the order to try them: every
/// server over UDP first, then over TCP, then over TLS, as a browser
/// falls back.
fn relay_attempts(relay: &Relay) -> Vec<Server> {
    let mut attempts = Vec::new();
    for (transport, port) in [
        (
            Transport::Udp,
            (|s: &RelayServer| s.udp_port) as fn(&RelayServer) -> Option<u16>,
        ),
        (Transport::Tcp, |s| s.tcp_port),
        (Transport::Tls, |s| s.tls_port),
    ] {
        for server in &relay.servers {
            if let Some(port) = port(server) {
                attempts.push(Server {
                    host: server.host.clone(),
                    port,
                    transport,
                });
            }
        }
    }
    attempts
}

/// The address the system sends to the internet from: a UDP socket
/// "connected" to a documentation address (RFC 5737) asks the routing
/// table without a packet going out.
fn route_ip() -> Option<IpAddr> {
    let socket = std::net::UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)).ok()?;
    socket.connect((Ipv4Addr::new(203, 0, 113, 1), 9)).ok()?;
    let ip = socket.local_addr().ok()?.ip();
    (!ip.is_unspecified() && !ip.is_loopback()).then_some(ip)
}

/// How the TURN server is reached.
enum LinkIo {
    /// Over UDP, from our one socket, at this address.
    Shared(SocketAddr),
    /// Over a TCP or TLS stream of its own.
    Stream(RelayIo),
}

/// A TURN allocation being made or in use.
struct Link {
    server: Server,
    client: turn::Client,
    io: LinkIo,
    /// Our end of the connection to the server.
    local: SocketAddr,
    relayed: Option<SocketAddr>,
}

/// Everything one session holds.
struct Session {
    started: Instant,
    socket: tokio::net::UdpSocket,
    /// Our host candidate's address: what str0m sees direct traffic
    /// arrive at and leave from.
    host: Option<SocketAddr>,
    rtc: Rtc,
    rtc_timeout: Option<Instant>,
    mid: Mid,
    ssrc: u32,
    opus_pt: u8,
    /// The far end's SSRCs str0m was told to expect.
    seen: HashSet<u32>,
    applied: Applied,
    send: bool,
    receive: bool,
    /// The TURN credentials, while there are servers left to try.
    turn: Option<(String, String)>,
    attempts: VecDeque<Server>,
    /// Whether a relay already sent us elsewhere: a second redirect is
    /// not followed.
    redirected: bool,
    relay: Option<Link>,
    relay_deadline: Option<Instant>,
    /// The far end's addresses the relay must let through.
    permits: Vec<IpAddr>,
    /// Our candidates, as the SDP lists them.
    candidates: Vec<Candidate>,
    /// Told once gathering is over.
    gathered: Option<oneshot::Sender<Result<LocalMedia, Failure>>>,
    tell: mpsc::UnboundedSender<MediaEvent>,
    counters: Arc<Counters>,
    connected: bool,
    flowing_told: bool,
    connect_deadline: Option<Instant>,
    /// What went which way, by path and kind (see [`Paths`]).
    paths: Paths,
    /// Our last DTLS flight, to send again if it goes unanswered.
    flight: Flight,
    disconnected_at: Option<Instant>,
    next_audio: Option<Instant>,
    next_stats: Instant,
    outbound: Outbound,
    feed: Option<Feed>,
    /// The camera line, in a build with video.
    video: Option<super::video::CallVideo>,
    /// The screen share's line, likewise.
    share: Option<super::video::CallVideo>,
    frames: Option<mpsc::Receiver<Outgoing>>,
    muted_rx: Option<watch::Receiver<bool>>,
    /// The microphone's mute.
    uplink_muted: bool,
    /// [`MediaSession::set_muted`]'s.
    forced_muted: bool,
    /// Unmuted and the microphone's frames have started: silence stops.
    flowing: bool,
    over: Option<Result<(), Failure>>,
    /// Where the sound and pictures go back once stopped, if asked.
    release: Option<oneshot::Sender<Held>>,
    /// The data line's SSRC, when there is one.
    data_ssrc: Option<u32>,
    /// The meeting's data channel, once SCTP is started.
    channel: Option<str0m::channel::ChannelId>,
    /// More camera lines, in a meeting.
    more: Vec<super::video::CallVideo>,
}

impl Session {
    /// Binds the socket and builds the peer, with the host candidate.
    async fn open(
        config: MediaConfig,
        held: Held,
        tell: mpsc::UnboundedSender<MediaEvent>,
        gathered: oneshot::Sender<Result<LocalMedia, Failure>>,
        counters: Arc<Counters>,
    ) -> Result<Self, Failure> {
        let bind = config.host.unwrap_or(IpAddr::V4(Ipv4Addr::UNSPECIFIED));
        let socket = tokio::net::UdpSocket::bind((bind, 0))
            .await
            .map_err(|e| failure(Stage::Gather, format!("no UDP socket: {e}")))?;
        let port = socket
            .local_addr()
            .map_err(|e| failure(Stage::Gather, e.to_string()))?
            .port();
        let now = Instant::now();
        let video_codecs: Vec<(u8, Option<u8>)> = [&config.video, &config.share]
            .into_iter()
            .flatten()
            .map(|v| (v.pt, v.rtx))
            .collect();
        let mut rtc = new_rtc(config.opus_pt, &video_codecs, config.controlling, now);
        let mid = Mid::from(config.audio_mid.as_str());
        // Any but zero, which libwebrtc keeps for its bandwidth probes.
        let ssrc = rand::random::<u32>().max(1);
        declare_audio(&mut rtc, mid, ssrc);
        let Held {
            feed,
            uplink,
            camera: camera_ends,
            share: share_ends,
            more: mut more_ends,
        } = held;
        // A tile each for more cameras, in the camera's gallery.
        let more_ends: Vec<super::video::Ends> = (0..config.cameras.len())
            .map(|i| {
                if more_ends.is_empty() {
                    camera_ends.more(i + 1)
                } else {
                    more_ends.remove(0)
                }
            })
            .collect();
        let fresh_ssrc = || loop {
            let candidate = rand::random::<u32>().max(1);
            if candidate != ssrc {
                break candidate;
            }
        };
        let mut line = |which, config: &Option<VideoLine>, ends| {
            config.as_ref().map(|line| {
                super::video::CallVideo::new(
                    &mut rtc,
                    which,
                    Mid::from(line.mid.as_str()),
                    (line.pt, line.rtx),
                    fresh_ssrc(),
                    ends,
                    tell.clone(),
                )
            })
        };
        let video = line(super::video::Which::Camera, &config.video, camera_ends);
        let share = line(super::video::Which::Share, &config.share, share_ends);
        let more: Vec<super::video::CallVideo> = config
            .cameras
            .iter()
            .zip(more_ends)
            .enumerate()
            .filter_map(|(i, (spec, ends))| {
                let mut camera = line(super::video::Which::Camera, &Some(spec.clone()), ends)?;
                camera.set_line(i + 1);
                Some(camera)
            })
            .collect();
        let data_ssrc = config.data.then(fresh_ssrc);
        let mut candidates = Vec::new();
        let host = config
            .host
            .or_else(route_ip)
            .map(|ip| SocketAddr::new(ip, port));
        if let Some(host) = host {
            match IceCandidate::host(host, "udp") {
                Ok(candidate) => {
                    if let Some(added) = rtc.add_local_candidate(candidate) {
                        candidates.push(listed(host, CandidateKind::Host, added.prio()));
                    }
                }
                Err(error) => log::warn!("gather: no host candidate at {host}: {error}"),
            }
        }
        log::info!(
            "gather: socket on port {port}, host candidate {}; audio SSRC {ssrc}, Opus at {}, \
             ICE {}",
            host.map_or_else(|| "none".to_owned(), |h| h.to_string()),
            config.opus_pt,
            if config.controlling {
                "controlling"
            } else {
                "controlled"
            }
        );
        let (turn, attempts) = match &config.relay {
            Some(relay) => {
                log::info!(
                    "gather: TURN servers {:?} in realm {}",
                    relay.servers.iter().map(|s| &s.host).collect::<Vec<_>>(),
                    relay.realm
                );
                (
                    Some((relay.username.clone(), relay.password.clone())),
                    relay_attempts(relay).into(),
                )
            }
            None => (None, VecDeque::new()),
        };
        let (frames, mut muted_rx) = match uplink {
            Some(uplink) => (Some(uplink.frames), Some(uplink.muted)),
            None => (None, None),
        };
        let uplink_muted = muted_rx.as_mut().is_none_or(|m| *m.borrow_and_update());
        Ok(Self {
            started: now,
            socket,
            host,
            rtc,
            rtc_timeout: Some(now),
            mid,
            ssrc,
            opus_pt: config.opus_pt,
            seen: HashSet::new(),
            applied: Applied::default(),
            send: true,
            receive: true,
            turn,
            attempts,
            redirected: false,
            relay: None,
            relay_deadline: None,
            permits: Vec::new(),
            candidates,
            gathered: Some(gathered),
            tell,
            counters,
            connected: false,
            flowing_told: false,
            connect_deadline: None,
            paths: Paths::default(),
            flight: Flight::default(),
            disconnected_at: None,
            next_audio: None,
            next_stats: now + STATS_EVERY,
            outbound: Outbound::default(),
            feed,
            video,
            share,
            frames,
            muted_rx,
            uplink_muted,
            forced_muted: false,
            flowing: false,
            over: None,
            release: None,
            data_ssrc,
            channel: None,
            more,
        })
    }

    fn since(&self) -> Duration {
        self.started.elapsed()
    }

    /// Muted by either switch, or with no microphone at all.
    fn muted(&self) -> bool {
        self.forced_muted || self.uplink_muted || self.frames.is_none()
    }

    /// Tries the next TURN server, or ends gathering without a relay when
    /// none is left.
    async fn next_relay(&mut self) {
        self.relay = None;
        self.relay_deadline = None;
        let Some((username, password)) = self.turn.clone() else {
            self.finish_gathering();
            return;
        };
        while let Some(server) = self.attempts.pop_front() {
            log::info!("relay: trying {server}");
            let opened = match server.transport {
                Transport::Udp => match self.host_or_socket() {
                    Ok(local) => shared_link(&server, local).await,
                    Err(why) => Err(why),
                },
                Transport::Tcp | Transport::Tls => {
                    match tokio::time::timeout(RELAY_TIMEOUT, connect_relay(&server)).await {
                        Ok(Ok((io, local))) => Ok((LinkIo::Stream(io), local)),
                        Ok(Err(why)) => Err(why),
                        Err(_) => Err("no connection in time".to_owned()),
                    }
                }
            };
            match opened {
                Ok((io, local)) => {
                    let mut client = turn::Client::new(server.transport, &username, &password);
                    client.allocate(Instant::now());
                    self.relay = Some(Link {
                        server,
                        client,
                        io,
                        local,
                        relayed: None,
                    });
                    self.relay_deadline = Some(Instant::now() + RELAY_TIMEOUT);
                    self.flush_relay().await;
                    return;
                }
                Err(why) => log::warn!("relay: {server}: {why}"),
            }
        }
        log::warn!("relay: no TURN server gave a relay; offering the host candidate only");
        self.turn = None;
        self.finish_gathering();
    }

    /// Our end of TURN over UDP: the host candidate's address, or the
    /// socket's own when there is none.
    fn host_or_socket(&self) -> Result<SocketAddr, String> {
        match self.host {
            Some(host) => Ok(host),
            None => self.socket.local_addr().map_err(|e| e.to_string()),
        }
    }

    /// Hands back [`LocalMedia`] once, when the candidates are all there.
    fn finish_gathering(&mut self) {
        let Some(gathered) = self.gathered.take() else {
            return;
        };
        if self.candidates.is_empty() {
            let failed = failure(Stage::Gather, "no candidate: no host address and no relay");
            let _ = gathered.send(Err(failed.clone()));
            self.over = Some(Err(failed));
            return;
        }
        let creds = self.rtc.direct_api().local_ice_credentials();
        let fingerprint = self.rtc.direct_api().local_dtls_fingerprint().bytes.clone();
        let mut local = local_media(
            &creds,
            &fingerprint,
            self.candidates.clone(),
            self.ssrc,
            self.opus_pt,
        );
        if let Some(video) = &self.video {
            local.video_ssrc = Some(video.ssrc());
            local.video_pt = video.pt();
            local.video_rtx = video.rtx();
            local.video_rtx_ssrc = video.rtx_ssrc();
        }
        if let Some(share) = &self.share {
            local.share_ssrc = Some(share.ssrc());
            local.share_pt = share.pt();
            local.share_rtx = share.rtx();
            local.share_rtx_ssrc = share.rtx_ssrc();
        }
        local.data_ssrc = self.data_ssrc;
        local.receive_cameras = self.more.iter().map(|m| m.mid().to_string()).collect();
        log::info!(
            "gather: done after {:?}: {}",
            self.since(),
            local
                .candidates
                .iter()
                .map(|c| format!("{:?} {}", c.kind, c.addr))
                .collect::<Vec<_>>()
                .join(", ")
        );
        let _ = gathered.send(Ok(local));
    }

    /// Adds a local candidate the relay gave, for the SDP too.
    fn add_candidate(&mut self, candidate: Result<IceCandidate, impl std::fmt::Display>) {
        let candidate = match candidate {
            Ok(candidate) => candidate,
            Err(error) => {
                log::warn!("gather: a candidate str0m does not take: {error}");
                return;
            }
        };
        let (addr, kind) = (candidate.addr(), candidate.kind());
        let kind = match kind {
            IceKind::Host => CandidateKind::Host,
            IceKind::Relayed => CandidateKind::Relay,
            IceKind::ServerReflexive | IceKind::PeerReflexive => CandidateKind::ServerReflexive,
        };
        if let Some(added) = self.rtc.add_local_candidate(candidate) {
            self.candidates.push(listed(addr, kind, added.prio()));
        }
    }

    /// Writes what the TURN client queued.
    async fn flush_relay(&mut self) {
        let Some(link) = &mut self.relay else {
            return;
        };
        while let Some(bytes) = link.client.poll_transmit() {
            let sent = match &mut link.io {
                LinkIo::Shared(server) => self.socket.send_to(&bytes, *server).await.map(|_| ()),
                LinkIo::Stream(io) => io.send(&bytes).await,
            };
            if let Err(error) = sent {
                log::warn!("relay: could not write to {}: {error}", link.server);
                break;
            }
        }
    }

    /// Reads TURN events: the allocation, data for str0m, failures.
    async fn relay_events(&mut self) {
        loop {
            let Some(link) = &mut self.relay else {
                return;
            };
            let Some(event) = link.client.poll_event() else {
                break;
            };
            match event {
                turn::Event::Allocated {
                    relayed,
                    mapped,
                    lifetime,
                } => {
                    link.relayed = Some(relayed);
                    self.relay_deadline = None;
                    log::info!(
                        "relay: {} relays at {relayed} (it sees us at {}; {lifetime} s)",
                        link.server,
                        mapped.map_or_else(|| "?".to_owned(), |m| m.to_string())
                    );
                    let shared = matches!(link.io, LinkIo::Shared(_));
                    let local = link.local;
                    if !self.permits.is_empty() {
                        link.client.permit(&self.permits, Instant::now());
                    }
                    // The address the server saw is our socket's only when
                    // TURN went over that socket.
                    if shared
                        && let (Some(mapped), Some(host)) = (mapped, self.host)
                        && mapped != host
                        && mapped.is_ipv4() == host.is_ipv4()
                    {
                        self.add_candidate(IceCandidate::server_reflexive(mapped, host, "udp"));
                    }
                    self.add_candidate(IceCandidate::relayed(relayed, local, "udp"));
                    self.turn = None;
                    self.finish_gathering();
                }
                turn::Event::Permitted(ip) => log::info!("relay: {ip} may reach us"),
                turn::Event::Data { peer, data } => {
                    if let Some(relayed) = link.relayed {
                        self.feed_rtc(peer, relayed, &data);
                    }
                }
                turn::Event::Failed(why) => {
                    let allocated = link.relayed.is_some();
                    log::warn!("relay: {}: {why}", link.server);
                    // Sent elsewhere (300 Try Alternate): ask there next,
                    // over UDP, once, before falling back to TCP.
                    if !allocated
                        && link.server.transport == Transport::Udp
                        && let Some(alternate) = link.client.alternate()
                        && !self.redirected
                    {
                        log::info!("relay: sent to {alternate}");
                        self.redirected = true;
                        self.attempts.push_front(turn::Server {
                            host: alternate.ip().to_string(),
                            port: alternate.port(),
                            transport: Transport::Udp,
                        });
                    }
                    if allocated {
                        // The call may still go on a direct path; ICE
                        // says if it does not.
                        self.relay = None;
                    } else {
                        self.next_relay().await;
                    }
                }
                turn::Event::Note(note) => log::info!("relay: {note}"),
            }
        }
        self.flush_relay().await;
    }

    /// One datagram on our socket: the TURN server's, or the far end's.
    async fn datagram(&mut self, from: SocketAddr, data: &[u8]) {
        if let Some(link) = &mut self.relay
            && matches!(link.io, LinkIo::Shared(server) if server == from)
        {
            link.client.handle_input(data, Instant::now());
            self.relay_events().await;
            return;
        }
        if let Some(host) = self.host {
            self.feed_rtc(from, host, data);
        }
    }

    /// Feeds str0m what `source` sent to our `destination`.
    fn feed_rtc(&mut self, source: SocketAddr, destination: SocketAddr, data: &[u8]) {
        let relayed = self.relay.as_ref().and_then(|l| l.relayed) == Some(destination);
        self.paths.count(false, relayed, data);
        if is_dtls(data) {
            self.flight.heard();
        }
        bump(&self.counters.packets_in, 1);
        bump(&self.counters.bytes_in, data.len());
        expect_remote(&mut self.rtc, self.mid, self.opus_pt, &mut self.seen, data);
        for line in self
            .video
            .iter_mut()
            .chain(self.share.iter_mut())
            .chain(self.more.iter_mut())
        {
            line.learn(&mut self.rtc, data);
        }
        let Ok(receive) = Receive::new(Protocol::Udp, source, destination, data) else {
            log::debug!(
                "connect: {} bytes from {source} that WebRTC does not read",
                data.len()
            );
            return;
        };
        if let Err(error) = self
            .rtc
            .handle_input(Input::Receive(Instant::now(), receive))
        {
            log::debug!("connect: input from {source}: {error}");
        }
        self.rtc_timeout = Some(Instant::now());
    }

    /// Runs str0m until it waits: what it sends goes out directly or
    /// through the relay, what it tells is acted on.
    async fn drive_rtc(&mut self) {
        let now = Instant::now();
        if self.rtc_timeout.is_some_and(|at| at <= now)
            && let Err(error) = self.rtc.handle_input(Input::Timeout(now))
        {
            log::debug!("connect: timeout input: {error}");
        }
        let mut direct = Vec::new();
        let mut events = Vec::new();
        loop {
            match self.rtc.poll_output() {
                Ok(Output::Timeout(at)) => {
                    self.rtc_timeout = Some(at);
                    break;
                }
                Ok(Output::Transmit(transmit)) => {
                    bump(&self.counters.packets_out, 1);
                    bump(&self.counters.bytes_out, transmit.contents.len());
                    match &mut self.relay {
                        // A relay never carries traffic to a private address
                        // (it refuses the permission, 403, every time ICE
                        // tries): those checks are left unsent.
                        Some(link)
                            if Some(transmit.source) == link.relayed
                                && !public(transmit.destination.ip()) => {}
                        Some(link) if Some(transmit.source) == link.relayed => {
                            self.paths.count(true, true, &transmit.contents);
                            if !self.connected && is_dtls(&transmit.contents) {
                                self.flight.sent(
                                    true,
                                    transmit.destination,
                                    &transmit.contents,
                                    now,
                                );
                            }
                            link.client
                                .send_to(transmit.destination, &transmit.contents, now);
                        }
                        _ => {
                            self.paths.count(true, false, &transmit.contents);
                            if !self.connected && is_dtls(&transmit.contents) {
                                self.flight.sent(
                                    false,
                                    transmit.destination,
                                    &transmit.contents,
                                    now,
                                );
                            }
                            direct.push((transmit.destination, transmit.contents.to_vec()));
                        }
                    }
                }
                Ok(Output::Event(event)) => events.push(event),
                Err(error) => {
                    self.over
                        .get_or_insert(Err(failure(Stage::Media, format!("WebRTC: {error}"))));
                    break;
                }
            }
        }
        for (destination, bytes) in direct {
            if let Err(error) = self.socket.send_to(&bytes, destination).await {
                log::debug!("connect: could not send to {destination}: {error}");
            }
        }
        self.flush_relay().await;
        for event in events {
            self.rtc_event(event);
        }
    }

    fn rtc_event(&mut self, event: RtcEvent) {
        match event {
            RtcEvent::IceConnectionStateChange(state) => {
                log::info!("connect: ICE {state:?}");
                match state {
                    IceConnectionState::Connected | IceConnectionState::Completed => {
                        self.disconnected_at = None;
                    }
                    IceConnectionState::Disconnected if self.connected => {
                        self.disconnected_at.get_or_insert(Instant::now());
                    }
                    _ => {}
                }
            }
            RtcEvent::Connected => {
                log::info!(
                    "connect: DTLS and SRTP are up after {:?} (DTLS {:?})",
                    self.since(),
                    self.rtc.direct_api().dtls_protocol_version()
                );
                self.connected = true;
                self.connect_deadline = None;
                self.next_audio = Some(Instant::now());
                let _ = self.tell.send(MediaEvent::Connected);
            }
            RtcEvent::MediaEgressStats(stats) if stats.mid == self.mid => {
                let remote = stats.remote.as_ref().map_or_else(
                    || "no report yet".to_owned(),
                    |r| {
                        format!(
                            "jitter {} (48 kHz units), {} lost in all",
                            r.jitter, r.packets_lost
                        )
                    },
                );
                log::info!(
                    "media: the far end receives ours: {} packets ({} bytes) sent; loss {:?}, \
                     rtt {:?}; {remote}",
                    stats.packets,
                    stats.bytes,
                    stats.loss,
                    stats.rtt
                );
            }
            RtcEvent::MediaData(data) if data.mid == self.mid => {
                let count = self.counters.audio_in.fetch_add(1, Ordering::Relaxed);
                if count == 0 {
                    log::info!(
                        "media: first audio after {:?}: payload type {:?}, {} bytes",
                        self.since(),
                        data.pt,
                        data.data.len()
                    );
                }
                if self.receive
                    && let Some(feed) = &self.feed
                {
                    // RTP's 32-bit timestamp; the jitter buffer unwraps it.
                    feed.push(data.time.numer() as u32, &data.data);
                }
                self.check_flowing();
            }
            RtcEvent::MediaData(data) => {
                for line in self
                    .video
                    .iter_mut()
                    .chain(self.share.iter_mut())
                    .chain(self.more.iter_mut())
                {
                    if line.is(data.mid) {
                        line.data(&data, Instant::now());
                        self.rtc_timeout = Some(Instant::now());
                    }
                }
            }
            RtcEvent::KeyframeRequest(request) => {
                for line in self
                    .video
                    .iter_mut()
                    .chain(self.share.iter_mut())
                    .chain(self.more.iter_mut())
                {
                    line.keyframe_request(&request);
                }
            }
            RtcEvent::ChannelOpen(id, _) if Some(id) == self.channel => {
                log::info!("media: the meeting's data channel is open");
                let _ = self.tell.send(MediaEvent::ChannelOpen);
            }
            RtcEvent::ChannelData(data) if Some(data.id) == self.channel => {
                let _ = self.tell.send(MediaEvent::ChannelData(data.data));
            }
            RtcEvent::ChannelClose(id) if Some(id) == self.channel => {
                log::info!("media: the meeting's data channel closed");
            }
            _ => {}
        }
    }

    /// Tells once that audio has gone both ways.
    fn check_flowing(&mut self) {
        if self.flowing_told {
            return;
        }
        let counts = self.counters.snapshot();
        if counts.audio_in > 0 && counts.audio_out > 0 {
            self.flowing_told = true;
            log::info!("media: audio flows both ways after {:?}", self.since());
            let _ = self.tell.send(MediaEvent::AudioFlowing);
        }
    }

    /// Writes one packet on the audio track, if connected and sending.
    fn write_audio(&mut self, stamp: Stamp, payload: Vec<u8>, level: (u8, bool)) -> bool {
        if !self.connected || !self.send {
            return false;
        }
        let written = write_opus(&mut self.rtc, self.mid, self.opus_pt, stamp, payload, level);
        if written {
            bump(&self.counters.audio_out, 1);
            self.check_flowing();
        }
        written
    }

    /// 20 ms of silence, while muted or until the microphone's first
    /// frame.
    fn send_silence(&mut self) {
        if !self.muted() && self.flowing {
            return;
        }
        let stamp = self.outbound.stamp(0);
        if self.write_audio(stamp, SILENT_OPUS.to_vec(), (127, false)) {
            bump(&self.counters.silent_out, 1);
        }
    }

    /// A frame from the microphone, unless muted.
    fn send_frame(&mut self, frame: Outgoing) {
        if self.muted() || !self.connected {
            return;
        }
        self.flowing = true;
        let stamp = self.outbound.stamp(frame.gap);
        self.write_audio(stamp, frame.payload, (frame.level, frame.voice));
    }

    /// Either mute switch moved: silence stands in until the
    /// microphone's frames come again.
    fn mute_changed(&mut self, before: bool) {
        let now = self.muted();
        if now != before {
            self.flowing = false;
            log::info!("media: {}", if now { "muted" } else { "unmuted" });
        }
    }

    /// Applies the far end's description.
    fn apply(&mut self, plan: &Plan) {
        log::info!(
            "remote: {} candidates, DTLS {}, {}sending, {}receiving",
            plan.candidates.len(),
            if plan.active { "active" } else { "passive" },
            if plan.send { "" } else { "not " },
            if plan.receive { "" } else { "not " }
        );
        match apply(&mut self.rtc, &mut self.applied, plan) {
            Ok(notes) => {
                for note in notes {
                    log::warn!("remote: {note}");
                }
            }
            Err(why) => {
                self.over.get_or_insert(Err(failure(Stage::Remote, why)));
                return;
            }
        }
        // SCTP over the DTLS just started, as a browser's data channels
        // go: the DTLS client starts the association too.
        if self.data_ssrc.is_some() && self.channel.is_none() && self.applied.dtls {
            let mut api = self.rtc.direct_api();
            api.start_sctp(plan.active);
            self.channel = Some(api.create_data_channel(str0m::channel::ChannelConfig {
                label: super::channel::LABEL.to_owned(),
                ..Default::default()
            }));
            log::info!("media: the meeting's data channel is opening");
        }
        self.send = plan.send;
        self.receive = plan.receive;
        if let (Some(video), Some((send, receive))) = (&mut self.video, plan.video) {
            video.set_flows(send, receive, plan.video_ssrcs);
        }
        for more in &mut self.more {
            let mid = more.mid();
            let (flows, ssrcs) = plan
                .lines
                .iter()
                .find(|l| Mid::from(l.mid.as_str()) == mid)
                .map_or(((false, false), None), |l| (l.flows, l.ssrcs));
            // Receive-only: nothing of ours goes on it.
            more.set_flows(false, flows.1, ssrcs);
        }
        if let Some(share) = &mut self.share {
            // A description without the share line in use shares nothing.
            let (send, receive) = plan.share.unwrap_or((false, false));
            share.set_flows(send, receive, plan.share_ssrcs);
        }
        for ip in &plan.permits {
            if !self.permits.contains(ip) {
                self.permits.push(*ip);
            }
        }
        if let Some(link) = &mut self.relay
            && link.relayed.is_some()
        {
            link.client.permit(&self.permits, Instant::now());
        }
        if !self.connected && self.connect_deadline.is_none() {
            self.connect_deadline = Some(Instant::now() + CONNECT_TIMEOUT);
        }
        self.rtc_timeout = Some(Instant::now());
    }

    /// One command from the handle; `None` when the handle is gone.
    fn command(&mut self, command: Option<Command>) {
        match command {
            Some(Command::Apply(plan)) => self.apply(&plan),
            Some(Command::Muted(muted)) => {
                let before = self.muted();
                self.forced_muted = muted;
                self.mute_changed(before);
            }
            Some(Command::Stop) | None => {
                log::info!("media: stopping");
                self.over.get_or_insert(Ok(()));
            }
            Some(Command::Data(message)) => {
                let written = self
                    .channel
                    .and_then(|id| self.rtc.channel(id))
                    .map(|mut channel| channel.write(true, &message));
                match written {
                    Some(Ok(_)) => {}
                    Some(Err(error)) => log::warn!("media: data channel: {error}"),
                    None => log::info!("media: the data channel is not open; message dropped"),
                }
                self.rtc_timeout = Some(Instant::now());
            }
            Some(Command::Release(give)) => {
                log::info!("media: stopping, the sound and pictures handed on");
                self.release = Some(give);
                self.over.get_or_insert(Ok(()));
            }
        }
    }

    fn stats(&self) {
        let counts = self.counters.snapshot();
        let played = self
            .feed
            .as_ref()
            .map(|f| format!("; played {:?}", f.played()))
            .unwrap_or_default();
        log::info!(
            "media: {} s: in {} packets ({} bytes), {} audio frames; out {} packets ({} bytes), \
             {} audio frames ({} of silence); {}{played}",
            self.since().as_secs(),
            counts.packets_in,
            counts.bytes_in,
            counts.audio_in,
            counts.packets_out,
            counts.bytes_out,
            counts.audio_out,
            counts.silent_out,
            if self.muted() { "muted" } else { "unmuted" }
        );
        log::info!("media: paths: {}", self.paths.line());
    }

    /// The next moment something is due.
    fn deadline(&self) -> Instant {
        let mut at = self.next_stats;
        for due in [
            self.rtc_timeout,
            self.relay.as_ref().and_then(|l| l.client.poll_timeout()),
            self.relay_deadline,
            self.connect_deadline,
            self.flight.resend_at.filter(|_| !self.connected),
            self.disconnected_at.map(|at| at + RECONNECT_GRACE),
            self.next_audio,
            self.video
                .as_ref()
                .and_then(super::video::CallVideo::deadline),
            self.share
                .as_ref()
                .and_then(super::video::CallVideo::deadline),
        ]
        .into_iter()
        .flatten()
        {
            at = at.min(due);
        }
        at
    }

    /// What is due now.
    async fn on_time(&mut self) {
        let now = Instant::now();
        if let Some(link) = &mut self.relay
            && link.client.poll_timeout().is_some_and(|at| at <= now)
        {
            link.client.handle_timeout(now);
        }
        if self.relay_deadline.is_some_and(|at| at <= now) {
            if let Some(link) = &self.relay {
                log::warn!("relay: {}: no allocation in time", link.server);
            }
            self.next_relay().await;
        }
        if !self.connected && self.flight.due(now) {
            self.resend_flight(now).await;
        }
        for line in self
            .video
            .iter_mut()
            .chain(self.share.iter_mut())
            .chain(self.more.iter_mut())
        {
            line.on_time(&mut self.rtc, now);
        }
        if self.connect_deadline.is_some_and(|at| at <= now) {
            log::warn!("connect: gave up; paths: {}", self.paths.line());
            self.over.get_or_insert(Err(failure(
                Stage::Connect,
                format!("no media connection within {} s", CONNECT_TIMEOUT.as_secs()),
            )));
            return;
        }
        if self
            .disconnected_at
            .is_some_and(|at| at + RECONNECT_GRACE <= now)
        {
            self.over.get_or_insert(Err(failure(
                Stage::Media,
                format!("ICE disconnected for {} s", RECONNECT_GRACE.as_secs()),
            )));
            return;
        }
        if self.next_audio.is_some_and(|at| at <= now) {
            self.send_silence();
            self.next_audio = Some(now + AUDIO_TICK);
        }
        if self.next_stats <= now {
            if self.connected {
                self.stats();
            }
            self.next_stats = now + STATS_EVERY;
        }
    }

    /// Sends our unanswered DTLS flight again, each datagram the way it
    /// went first.
    async fn resend_flight(&mut self, now: Instant) {
        log::info!(
            "connect: no DTLS answer; sending our {} handshake packets again (try {})",
            self.flight.datagrams.len(),
            self.flight.tries
        );
        let datagrams = self.flight.datagrams.clone();
        for (relayed, to, data) in datagrams {
            self.paths.count(true, relayed, &data);
            if relayed {
                if let Some(link) = &mut self.relay {
                    link.client.send_to(to, &data, now);
                }
            } else if let Err(error) = self.socket.send_to(&data, to).await {
                log::debug!("connect: could not send to {to}: {error}");
            }
        }
        self.flush_relay().await;
    }

    /// Lets go of the peer and the relay, and says how it ended.
    async fn close(&mut self) {
        self.rtc.disconnect();
        if let Some(link) = &mut self.relay {
            link.client.close(Instant::now());
        }
        self.flush_relay().await;
        let result = self.over.take().unwrap_or(Ok(()));
        if let Some(give) = self.release.take() {
            let uplink = self.frames.take().zip(self.muted_rx.take());
            let _ = give.send(Held {
                feed: self.feed.take(),
                uplink: uplink.map(|(frames, muted)| Uplink { frames, muted }),
                camera: self.video.take().map(|v| v.into_ends()).unwrap_or_default(),
                share: self.share.take().map(|v| v.into_ends()).unwrap_or_default(),
                more: self.more.drain(..).map(|v| v.into_ends()).collect(),
            });
        }
        if let Some(gathered) = self.gathered.take() {
            let _ = gathered.send(Err(match &result {
                Err(failed) => failed.clone(),
                Ok(()) => failure(Stage::Gather, "stopped while gathering"),
            }));
        }
        self.stats();
        let _ = self.tell.send(match result {
            Ok(()) => MediaEvent::Stopped,
            Err(failed) => {
                log::warn!("media: {failed}");
                MediaEvent::Failed(failed)
            }
        });
    }
}

/// TURN over UDP from our own socket, which is at `local`: the server's
/// IPv4 address.
async fn shared_link(server: &Server, local: SocketAddr) -> Result<(LinkIo, SocketAddr), String> {
    let address = tokio::time::timeout(
        RELAY_TIMEOUT,
        tokio::net::lookup_host((server.host.as_str(), server.port)),
    )
    .await
    .map_err(|_| "no address in time".to_owned())?
    .map_err(|e| format!("{}: {e}", server.host))?
    .find(SocketAddr::is_ipv4)
    .ok_or_else(|| format!("{} has no IPv4 address", server.host))?;
    log::info!("relay: {server} at {address}, from our socket");
    Ok((LinkIo::Shared(address), local))
}

/// The next messages from a TURN server reached over a stream, or never.
async fn stream_recv(relay: &mut Option<Link>) -> std::io::Result<Vec<Vec<u8>>> {
    match relay {
        Some(Link {
            io: LinkIo::Stream(io),
            ..
        }) => io.recv().await,
        _ => std::future::pending().await,
    }
}

/// The next frame from the microphone, or never while there is none.
async fn next_frame(frames: &mut Option<mpsc::Receiver<Outgoing>>) -> Option<Outgoing> {
    match frames {
        Some(frames) => frames.recv().await,
        None => std::future::pending().await,
    }
}

/// The microphone's next mute change, or never while there is none.
async fn mute_change(
    muted: &mut Option<watch::Receiver<bool>>,
) -> Result<bool, watch::error::RecvError> {
    match muted {
        Some(muted) => {
            muted.changed().await?;
            Ok(*muted.borrow_and_update())
        }
        None => std::future::pending().await,
    }
}

/// The session, gathering to stopping.
/// What the camera side has next; never without a camera line.
async fn next_video(video: &mut Option<super::video::CallVideo>) -> super::video::Input {
    match video {
        Some(video) => video.next().await,
        None => std::future::pending().await,
    }
}

async fn run(mut session: Session, mut commands: mpsc::UnboundedReceiver<Command>) {
    session.next_relay().await;
    let mut buf = vec![0u8; DATAGRAM];
    while session.over.is_none() {
        session.drive_rtc().await;
        if session.over.is_some() {
            break;
        }
        let deadline = session.deadline();
        tokio::select! {
            received = session.socket.recv_from(&mut buf) => match received {
                Ok((n, from)) => session.datagram(from, &buf[..n]).await,
                // Windows reports an ICMP "port unreachable" from an
                // earlier send this way; the socket itself is fine.
                Err(error) => log::debug!("connect: socket: {error}"),
            },
            received = stream_recv(&mut session.relay) => match received {
                Ok(messages) => {
                    let now = Instant::now();
                    if let Some(link) = &mut session.relay {
                        for message in messages {
                            link.client.handle_input(&message, now);
                        }
                    }
                    session.relay_events().await;
                }
                Err(error) => {
                    let allocated = session.relay.as_ref().is_some_and(|l| l.relayed.is_some());
                    log::warn!("relay: {error}");
                    if allocated {
                        session.relay = None;
                    } else {
                        session.next_relay().await;
                    }
                }
            },
            command = commands.recv() => session.command(command),
            input = next_video(&mut session.video) => {
                let connected = session.connected;
                if let Some(video) = &mut session.video {
                    video.input(&mut session.rtc, input, connected);
                }
            }
            input = next_video(&mut session.share) => {
                let connected = session.connected;
                if let Some(share) = &mut session.share {
                    share.input(&mut session.rtc, input, connected);
                }
            }
            frame = next_frame(&mut session.frames) => match frame {
                Some(frame) => session.send_frame(frame),
                // The microphone's side is gone: silence from here.
                None => {
                    let before = session.muted();
                    session.frames = None;
                    session.mute_changed(before);
                }
            },
            muted = mute_change(&mut session.muted_rx) => {
                let before = session.muted();
                match muted {
                    Ok(muted) => session.uplink_muted = muted,
                    Err(_) => {
                        session.muted_rx = None;
                        session.uplink_muted = true;
                    }
                }
                session.mute_changed(before);
            }
            () = tokio::time::sleep_until(deadline.into()) => {
                session.on_time().await;
                session.relay_events().await;
            }
        }
    }
    session.close().await;
}

#[cfg(test)]
mod tests {
    use super::super::{Line, LineKind};
    use super::*;

    fn addr(text: &str) -> SocketAddr {
        text.parse().expect("an address")
    }

    fn candidate(kind: CandidateKind, at: &str, priority: u32) -> Candidate {
        Candidate {
            foundation: "7".into(),
            priority,
            addr: addr(at),
            kind,
        }
    }

    const FINGERPRINT: &str = "8B:2A:01:FF:00:10:20:30:40:50:60:70:80:90:A0:B0:\
                               C0:D0:E0:F0:01:02:03:04:05:06:07:08:09:0A:0B:0C";

    fn audio_line(direction: Direction) -> Line {
        Line {
            mid: "0".into(),
            kind: LineKind::Audio,
            label: Some("main-audio".into()),
            port: 3478,
            direction,
            ssrc_range: None,
        }
    }

    /// An answer as §D.2 has it: passive, candidates of every kind.
    fn answer() -> RemoteMedia {
        RemoteMedia {
            ice_ufrag: "abcd".into(),
            ice_pwd: "a-password-of-twenty-four".into(),
            fingerprint: Some(FINGERPRINT.into()),
            setup: Setup::Passive,
            candidates: vec![
                candidate(CandidateKind::Relay, "20.202.0.1:3478", 33_553_407),
                candidate(CandidateKind::Host, "192.168.1.20:50000", 2_130_706_431),
                candidate(
                    CandidateKind::ServerReflexive,
                    "198.51.100.7:50000",
                    1_694_498_815,
                ),
                candidate(CandidateKind::Host, "0.0.0.0:9", 1),
                candidate(CandidateKind::Host, "[2001:db8::1]:50000", 2_130_706_430),
                candidate(
                    CandidateKind::ServerReflexive,
                    "198.51.100.7:50002",
                    1_694_498_814,
                ),
            ],
            opus_pt: Some(111),
            video: None,
            share_video: None,
            lines: vec![
                audio_line(Direction::SendRecv),
                Line {
                    mid: "1".into(),
                    kind: LineKind::Video,
                    label: Some("main-video".into()),
                    port: 0,
                    direction: Direction::Inactive,
                    ssrc_range: None,
                },
            ],
            video_streams: Vec::new(),
        }
    }

    #[test]
    fn an_unanswered_flight_is_sent_again_with_backoff() {
        let start = Instant::now();
        let to: SocketAddr = "198.51.100.7:5000".parse().expect("an address");
        let mut flight = Flight::default();
        flight.sent(false, to, &[22, 1], start);
        flight.sent(false, to, &[22, 2], start);
        assert!(!flight.due(start), "not before a second");
        assert!(flight.due(start + Duration::from_secs(1)));
        assert_eq!(flight.datagrams.len(), 2, "the whole flight");
        assert!(
            !flight.due(start + Duration::from_millis(2500)),
            "then two seconds"
        );
        assert!(flight.due(start + Duration::from_secs(3)));
        // An answer stops it; the next datagram starts a new flight.
        flight.heard();
        assert!(!flight.due(start + Duration::from_secs(60)));
        flight.sent(false, to, &[22, 3], start + Duration::from_secs(60));
        assert_eq!(flight.datagrams.len(), 1);
        assert!(flight.due(start + Duration::from_secs(61)));
    }

    #[test]
    fn a_flight_is_sent_again_a_few_times_only() {
        let start = Instant::now();
        let to: SocketAddr = "198.51.100.7:5000".parse().expect("an address");
        let mut flight = Flight::default();
        flight.sent(false, to, &[22], start);
        let mut sent = 0;
        let mut at = start;
        for _ in 0..100 {
            at += Duration::from_secs(5);
            if flight.due(at) {
                sent += 1;
            }
        }
        assert_eq!(sent, FLIGHT_TRIES);
    }

    #[test]
    fn paths_count_by_kind() {
        let mut paths = Paths::default();
        paths.count(true, true, &[0x00, 0x01]);
        paths.count(false, false, &[22, 254]);
        paths.count(false, false, &[0x80, 111]);
        paths.count(true, false, &[]);
        assert_eq!(
            paths.line(),
            "direct in [stun 0 dtls 1 rtp 1] out [stun 0 dtls 0 rtp 0]; \
             relayed in [stun 0 dtls 0 rtp 0] out [stun 1 dtls 0 rtp 0]"
        );
    }

    #[test]
    fn a_relay_is_asked_only_for_public_addresses() {
        assert!(public("178.230.119.236".parse().expect("an address")));
        assert!(!public("10.9.243.230".parse().expect("an address")));
        assert!(!public("192.168.0.73".parse().expect("an address")));
        assert!(!public("127.0.0.1".parse().expect("an address")));
    }

    #[test]
    fn the_dtls_role_follows_the_far_ends_setup() {
        assert!(dtls_active(Setup::Passive), "they are the server");
        assert!(!dtls_active(Setup::Active), "they are the client");
        assert!(dtls_active(Setup::ActPass), "we answer active");
        assert!(dtls_active(Setup::Unsaid), "as the web client did");
    }

    #[test]
    fn fingerprints_read_and_write_as_sdp_has_them() {
        let bytes = parse_fingerprint(FINGERPRINT).expect("a fingerprint");
        assert_eq!(bytes.len(), 32);
        assert_eq!(bytes[0], 0x8B);
        assert_eq!(format_fingerprint(&bytes), FINGERPRINT);
        assert_eq!(
            parse_fingerprint(&FINGERPRINT.to_lowercase()),
            Some(bytes),
            "either case"
        );
        assert_eq!(parse_fingerprint("8B:2A"), None, "too short for SHA-256");
        assert_eq!(parse_fingerprint(&FINGERPRINT.replace(':', "")), None);
        assert_eq!(parse_fingerprint(&FINGERPRINT.replace("8B", "XY")), None);
    }

    #[test]
    fn candidates_keep_the_far_ends_priority_and_drop_the_unreachable() {
        let relay = ice_candidate(&candidate(CandidateKind::Relay, "20.202.0.1:3478", 42))
            .expect("a candidate");
        assert_eq!(relay.addr(), addr("20.202.0.1:3478"));
        assert_eq!(relay.prio(), 42);
        assert_eq!(relay.kind(), IceKind::Relayed);
        assert_eq!(relay.proto(), Protocol::Udp);
        let srflx = ice_candidate(&candidate(CandidateKind::ServerReflexive, "1.2.3.4:5", 7))
            .expect("a candidate");
        assert_eq!(srflx.kind(), IceKind::ServerReflexive);
        for unreachable in [
            "0.0.0.0:9",
            "1.2.3.4:0",
            "224.0.0.1:5",
            "169.254.1.1:5",
            "[::]:5",
        ] {
            assert!(
                ice_candidate(&candidate(CandidateKind::Host, unreachable, 1)).is_none(),
                "{unreachable}"
            );
        }
    }

    #[test]
    fn the_relay_lets_every_far_end_address_through_once() {
        let remote = answer();
        assert_eq!(
            permissions(&remote, true),
            vec![
                "20.202.0.1".parse::<IpAddr>().expect("an IP"),
                "198.51.100.7".parse().expect("an IP"),
            ],
            "IPv4 only, public only (no wildcard, no private host), the srflx address once"
        );
        assert_eq!(
            permissions(&remote, false),
            vec!["2001:db8::1".parse::<IpAddr>().expect("an IP")]
        );
    }

    #[test]
    fn an_answer_becomes_a_plan() {
        let plan = plan(&answer(), 111).expect("a plan");
        assert!(plan.active);
        assert_eq!(plan.creds.ufrag, "abcd");
        assert_eq!(plan.fingerprint.len(), 32);
        assert_eq!(plan.candidates.len(), 5, "the wildcard dropped");
        assert_eq!(plan.permits.len(), 2, "the private host address left out");
        assert!(plan.send && plan.receive);
        let shown = format!("{plan:?}");
        assert!(!shown.contains("password"), "{shown}");
    }

    #[test]
    fn the_far_ends_direction_says_what_flows() {
        assert_eq!(flows(Direction::SendRecv), (true, true));
        assert_eq!(flows(Direction::SendOnly), (false, true));
        assert_eq!(flows(Direction::RecvOnly), (true, false));
        assert_eq!(flows(Direction::Inactive), (false, false));
        let mut held = answer();
        held.lines[0].direction = Direction::Inactive;
        let plan = plan(&held, 111).expect("a plan");
        assert!(!plan.send && !plan.receive);
    }

    #[test]
    fn an_unusable_description_is_refused() {
        let refused = |change: fn(&mut RemoteMedia), opus_pt: u8| {
            let mut remote = answer();
            change(&mut remote);
            plan(&remote, opus_pt).expect_err("refused")
        };
        assert_eq!(refused(|_| {}, 102), "Opus at payload type 111, not 102");
        assert_eq!(refused(|r| r.opus_pt = None, 111), "no Opus");
        assert_eq!(refused(|r| r.ice_pwd.clear(), 111), "no ICE credentials");
        assert_eq!(
            refused(|r| r.fingerprint = None, 111),
            "no DTLS fingerprint"
        );
        assert_eq!(
            refused(|r| r.lines[0].port = 0, 111),
            "the audio m-line is refused"
        );
        assert_eq!(refused(|r| r.lines.clear(), 111), "no audio m-line");
        assert_eq!(
            refused(|r| r.candidates.retain(|c| c.addr.port() == 9), 111),
            "no candidate to reach"
        );
    }

    #[test]
    fn rtp_at_the_opus_payload_type_gives_its_ssrc() {
        let mut packet = vec![0x80, 111, 0, 1, 0, 0, 0, 0, 0x00, 0x00, 0x0C, 0x07, 0xF8];
        assert_eq!(rtp_ssrc(&packet, 111), Some(3079));
        packet[1] = 111 | 0x80;
        assert_eq!(rtp_ssrc(&packet, 111), Some(3079), "with the marker");
        assert_eq!(rtp_ssrc(&packet, 102), None, "another payload type");
        packet[1] = 200;
        assert_eq!(rtp_ssrc(&packet, 72), None, "RTCP, a sender report");
        packet[0] = 0x16;
        packet[1] = 111;
        assert_eq!(rtp_ssrc(&packet, 111), None, "DTLS");
        assert_eq!(rtp_ssrc(&[0x80, 111, 0], 111), None, "too short");
    }

    #[test]
    fn relay_servers_are_tried_udp_first() {
        let relay = Relay {
            servers: vec![
                RelayServer {
                    host: "a.example".into(),
                    udp_port: Some(3478),
                    tcp_port: Some(443),
                    tls_port: Some(443),
                },
                RelayServer {
                    host: "b.example".into(),
                    udp_port: Some(3479),
                    tcp_port: None,
                    tls_port: Some(5349),
                },
            ],
            realm: "rtcmedia".into(),
            username: "user-secret".into(),
            password: "password-secret".into(),
        };
        let tried: Vec<String> = relay_attempts(&relay)
            .iter()
            .map(ToString::to_string)
            .collect();
        assert_eq!(
            tried,
            [
                "a.example:3478 over Udp",
                "b.example:3479 over Udp",
                "a.example:443 over Tcp",
                "a.example:443 over Tls",
                "b.example:5349 over Tls",
            ]
        );
        let shown = format!("{relay:?}");
        assert!(!shown.contains("secret"), "{shown}");
        assert!(shown.contains("rtcmedia"));
    }

    #[test]
    fn the_relay_is_built_from_what_the_api_fetches() {
        let servers = super::super::api::RelayServers::default();
        let relay = Relay::new(
            &servers,
            super::super::api::RelayCredentials {
                realm: String::new(),
                username: "u".into(),
                password: "p".into(),
                expires: None,
            },
        );
        assert_eq!(relay.servers.len(), servers.hosts.len());
        assert_eq!(relay.servers[0].udp_port, Some(servers.udp_port));
        assert_eq!(
            relay.realm, servers.realm,
            "the configuration's, when unsaid"
        );
        assert_eq!(relay.username, "u");
    }

    #[test]
    fn local_media_says_what_our_sdp_needs() {
        let creds = IceCreds {
            ufrag: "wxyz".into(),
            pass: "pass".into(),
        };
        let candidates = vec![
            listed(
                addr("192.168.1.2:40000"),
                CandidateKind::Host,
                2_130_706_431,
            ),
            listed(addr("52.114.0.9:3478"), CandidateKind::Relay, 16_777_215),
        ];
        let local = local_media(&creds, &[0xAB, 0x01], candidates, 1234, 111);
        assert_eq!(local.ice_ufrag, "wxyz");
        assert_eq!(local.ice_pwd, "pass");
        assert_eq!(local.fingerprint, "AB:01");
        assert_eq!(local.setup, Setup::ActPass);
        assert_eq!(local.audio_ssrc, 1234);
        assert_eq!(local.opus_pt, 111);
        assert_eq!(local.audio_direction, Direction::SendRecv);
        assert_eq!(local.data_ssrc, None);
        assert_eq!(local.session_version, 2);
        assert_eq!(local.candidates[0].foundation, "1");
        assert_eq!(local.candidates[1].foundation, "3");
        assert_eq!(local.candidates[1].kind, CandidateKind::Relay);
    }

    /// One side of an offline call: a peer as the session builds it, with
    /// a host candidate at `at`.
    struct Side {
        rtc: Rtc,
        at: SocketAddr,
        mid: Mid,
        applied: Applied,
        seen: HashSet<u32>,
        events: Vec<RtcEvent>,
        received: Vec<Vec<u8>>,
        outbound: Outbound,
    }

    impl Side {
        fn new(at: &str, controlling: bool, opus_pt: u8, ssrc: u32, now: Instant) -> Self {
            let at = addr(at);
            let mut rtc = new_rtc(opus_pt, &[], controlling, now);
            let mid = Mid::from(AUDIO_MID);
            declare_audio(&mut rtc, mid, ssrc);
            rtc.add_local_candidate(IceCandidate::host(at, "udp").expect("a candidate"));
            Self {
                rtc,
                at,
                mid,
                applied: Applied::default(),
                seen: HashSet::new(),
                events: Vec::new(),
                received: Vec::new(),
                outbound: Outbound::default(),
            }
        }

        /// What this side's SDP would say, read back as the far end's.
        fn as_remote(&mut self, setup: Setup, opus_pt: u8, ssrc: u32) -> RemoteMedia {
            let creds = self.rtc.direct_api().local_ice_credentials();
            let fingerprint = self.rtc.direct_api().local_dtls_fingerprint().bytes.clone();
            let host = IceCandidate::host(self.at, "udp")
                .expect("a candidate")
                .prio();
            let local = local_media(
                &creds,
                &fingerprint,
                vec![listed(self.at, CandidateKind::Host, host)],
                ssrc,
                opus_pt,
            );
            RemoteMedia {
                ice_ufrag: local.ice_ufrag,
                ice_pwd: local.ice_pwd,
                fingerprint: Some(local.fingerprint),
                setup,
                candidates: local.candidates,
                opus_pt: Some(local.opus_pt),
                video: None,
                share_video: None,
                lines: vec![audio_line(Direction::SendRecv)],
                video_streams: Vec::new(),
            }
        }

        /// Runs str0m until it waits; hands back what it sent.
        fn poll(&mut self) -> (Vec<str0m::net::Transmit>, Instant) {
            let mut out = Vec::new();
            loop {
                match self.rtc.poll_output().expect("output") {
                    Output::Timeout(at) => return (out, at),
                    Output::Transmit(t) => out.push(t),
                    Output::Event(RtcEvent::MediaData(data)) => {
                        self.received.push(data.data.to_vec());
                    }
                    Output::Event(e) => self.events.push(e),
                }
            }
        }

        fn take(&mut self, t: &str0m::net::Transmit, opus_pt: u8, now: Instant) {
            assert_eq!(t.destination, self.at);
            expect_remote(
                &mut self.rtc,
                self.mid,
                opus_pt,
                &mut self.seen,
                &t.contents,
            );
            let receive =
                Receive::new(Protocol::Udp, t.source, t.destination, &t.contents).expect("read");
            self.rtc
                .handle_input(Input::Receive(now, receive))
                .expect("taken");
        }

        fn connected(&self) -> bool {
            self.events.iter().any(|e| matches!(e, RtcEvent::Connected))
        }
    }

    /// A whole call offline: two peers built as the session builds them,
    /// each told the other's media as `plan` and `apply` take it (the
    /// offer `actpass`, the answer `active`, Opus at the offerer's 102),
    /// ICE and OpenSSL's DTLS between them, and Opus both ways, each
    /// side learning the other's SSRC from its first packet. A
    /// renegotiation's description, applied again, changes nothing.
    #[test]
    fn a_call_connects_and_audio_flows_both_ways() {
        let pt = 102;
        let mut now = Instant::now();
        let mut ours = Side::new("192.168.1.2:40000", true, pt, 1111, now);
        let mut theirs = Side::new("192.168.1.3:50000", false, pt, 3079, now);

        let offer = ours.as_remote(Setup::ActPass, pt, 1111);
        let answer = theirs.as_remote(super::super::sdp::answer_setup(Setup::ActPass), pt, 3079);
        let to_theirs = plan(&offer, pt).expect("the offer is usable");
        assert!(to_theirs.active, "the answerer takes active");
        let to_ours = plan(&answer, pt).expect("the answer is usable");
        assert!(!to_ours.active, "so the offerer is passive");
        apply(&mut theirs.rtc, &mut theirs.applied, &to_theirs).expect("applied");
        apply(&mut ours.rtc, &mut ours.applied, &to_ours).expect("applied");
        let again = apply(&mut ours.rtc, &mut ours.applied, &to_ours).expect("applied");
        assert!(again.is_empty(), "{again:?}");

        let start = now;
        let mut next_send = now;
        let mut sent = 0u8;
        while now - start < Duration::from_secs(10)
            && (ours.received.len() < 10 || theirs.received.len() < 10)
        {
            let (out, ours_at) = ours.poll();
            for t in &out {
                theirs.take(t, pt, now);
            }
            let (out, theirs_at) = theirs.poll();
            for t in &out {
                ours.take(t, pt, now);
            }
            if ours.connected() && theirs.connected() && now >= next_send {
                for side in [&mut ours, &mut theirs] {
                    let stamp = side.outbound.stamp(0);
                    let payload = vec![0xF8, 0xFF, 0xFE, sent];
                    assert!(write_opus(
                        &mut side.rtc,
                        side.mid,
                        pt,
                        stamp,
                        payload,
                        (40, true)
                    ));
                }
                sent = sent.wrapping_add(1);
                next_send = now + AUDIO_TICK;
            }
            let next = ours_at
                .min(theirs_at)
                .min(next_send.max(now + Duration::from_millis(1)));
            now = next.max(now);
            ours.rtc.handle_input(Input::Timeout(now)).expect("timeout");
            theirs
                .rtc
                .handle_input(Input::Timeout(now))
                .expect("timeout");
        }
        assert!(ours.connected() && theirs.connected(), "never connected");
        assert!(
            ours.received.len() >= 10,
            "we heard {}",
            ours.received.len()
        );
        assert!(
            theirs.received.len() >= 10,
            "they heard {}",
            theirs.received.len()
        );
        assert_eq!(ours.received[0], vec![0xF8, 0xFF, 0xFE, 0]);
        assert_eq!(theirs.seen, HashSet::from([1111]));
        assert_eq!(ours.seen, HashSet::from([3079]));
    }

    #[test]
    fn failures_say_their_stage() {
        assert_eq!(
            failure(Stage::Connect, "no media connection").to_string(),
            "Connect: no media connection"
        );
        assert_eq!(MediaConfig::offer(None).opus_pt, OPUS_PT);
        assert!(MediaConfig::offer(None).controlling);
    }

    #[test]
    fn an_answer_takes_the_callers_camera_line_in_a_build_with_video() {
        let offer = super::super::sdp::read(include_str!("fixtures/incoming_offer_020.sdp"))
            .expect("the recorded offer reads");
        let config = MediaConfig::answer(None, &offer);
        if HAS_VIDEO {
            assert_eq!(
                config.video,
                Some(VideoLine {
                    mid: "video_1".into(),
                    pt: 107,
                    rtx: Some(99),
                })
            );
        } else {
            assert_eq!(config.video, None);
        }
        assert_eq!(offer_video_pt(111), 108);
        assert_eq!(offer_video_pt(108), 118, "Opus's number is not H.264's too");
    }

    #[test]
    fn an_answer_takes_the_callers_opus_and_mid() {
        let offer = super::super::sdp::read(include_str!("fixtures/incoming_offer_020.sdp"))
            .expect("the recorded offer reads");
        let config = MediaConfig::answer(None, &offer);
        assert!(!config.controlling, "the caller controls ICE");
        assert_eq!(Some(config.opus_pt), offer.opus_pt);
        assert_eq!(
            Some(config.audio_mid.as_str()),
            offer.audio().map(|l| l.mid.as_str())
        );
    }

    #[test]
    fn a_meetings_media_server_is_answered_by_a_session_of_its_own() {
        let remote =
            super::super::sdp::read(include_str!("fixtures/meeting_retarget.sdp")).expect("reads");
        let config = MediaConfig::answer(None, &remote);
        assert!(!config.controlling);
        assert_eq!(config.opus_pt, 102);
        assert_eq!(config.audio_mid, "1");
        if HAS_VIDEO {
            assert_eq!(config.video.as_ref().map(|v| v.mid.as_str()), Some("2"));
            assert_eq!(config.share.as_ref().map(|v| v.mid.as_str()), Some("3"));
        }
        let meeting = MediaConfig::meeting_answer(None, &remote);
        assert!(meeting.data);
        if HAS_VIDEO {
            let mids: Vec<&str> = meeting.cameras.iter().map(|c| c.mid.as_str()).collect();
            assert_eq!(mids, ["5", "6", "7"]);
        }
        let plan = plan(&remote, config.opus_pt).expect("a usable offer");
        // No `a=setup`: we are the DTLS client, as the web client was.
        assert!(plan.active);
        assert!(!plan.candidates.is_empty());
    }

    #[test]
    fn a_meeting_is_offered_h264_at_its_media_servers_number() {
        let config = MediaConfig::meeting(None);
        assert!(config.controlling);
        // With the data line, for the meeting's data channel, and more
        // cameras to receive after it.
        assert!(config.data);
        if HAS_VIDEO {
            let mids: Vec<&str> = config.cameras.iter().map(|c| c.mid.as_str()).collect();
            assert_eq!(mids, ["4", "5", "6"]);
        }
        assert!(!MediaConfig::offer(None).data);
        assert_eq!(config.opus_pt, OPUS_PT);
        if HAS_VIDEO {
            for line in [&config.video, &config.share] {
                let line = line.as_ref().expect("a video line");
                assert_eq!((line.pt, line.rtx), (107, Some(99)));
            }
            assert_ne!(MEETING_VIDEO_PT, OPUS_PT);
            assert_ne!(MEETING_VIDEO_RTX, OPUS_PT);
        }
    }
}
