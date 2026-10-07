//! The listening session: signaling, the TURN relay and the WebRTC peer,
//! driven together on one task.
//!
//! WebRTC is `str0m`: sans-IO like the TURN client, with OpenSSL's DTLS
//! and SRTP ([`super::dtls`]), the one DTLS Chime was seen to take. It
//! gathers nothing itself; the only local candidate is the TURN relay
//! ([`super::turn`]), as Chime reaches media only through one. What
//! `str0m` sends from the relay's address goes to the TURN server as a
//! Send indication; what the server relays back is fed to `str0m` as if
//! it had arrived at that address.
//!
//! Every step logs a line at info level, named so a probe's log shows
//! where a mismatch is: the frames (by [`super::chime::describe`]), the
//! SDP (by [`super::sdp::summary`]), the relay, ICE and DTLS, the first
//! audio, and counts every five seconds, with what Chime's RTCP receiver
//! reports say of what we send. Secrets never appear.
//!
//! Sending: the audio track carries Opus silence every 20 ms while muted
//! (a muted browser's track does the same) and, once unmuted, the frames
//! an [`Uplink`] brings from the microphone, on one RTP clock
//! ([`Outbound`]). Each mute and unmute also goes to Chime as an
//! AUDIO_CONTROL frame, as the JS SDK sends it, so the others see it.
//!
//! With a camera (the `huddle-camera` feature, or the probe's test
//! picture), its encoded frames go out on the first video m-line once a
//! re-SUBSCRIBE has made it `sendrecv` (see [`super::watch`]), starting
//! at a keyframe; a receiver's PLI or FIR asks the encoder for another,
//! `str0m`'s bandwidth estimate sets its bitrate, and Chime's "view only"
//! (SUBSCRIBE_ACK 206) turns the camera off again.

use std::collections::VecDeque;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use str0m::change::{SdpAnswer, SdpPendingOffer};
use str0m::format::Codec;
use str0m::media::{Direction, MediaKind, MediaTime, Mid};
use str0m::net::{Protocol, Receive};
use str0m::{Candidate, Event as RtcEvent, IceConnectionState, Input, Output, Rtc, RtcConfig};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

pub use super::cameras::Wish;
use super::chime::{self, FrameType};
use super::dtls;
use super::join::ChimeJoin;
use super::roster::{self, Roster, Voices};
use super::sdp::{self, Mids};
use super::signaling::{Ending, Handshake, Incoming, Socket, Step, TurnCredentials};
use super::speaker::Feed;
use super::turn::{self, Server, Transport};
use super::uplink::{Outbound, Outgoing, Stamp};
use super::video;
pub use super::watch::Viewer;
use super::watch::{self, Watch};

/// How long each step may take.
const OPEN_TIMEOUT: Duration = Duration::from_secs(15);
const INDEX_WAIT: Duration = Duration::from_secs(5);
const RELAY_TIMEOUT: Duration = Duration::from_secs(8);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
const LEAVE_TIMEOUT: Duration = Duration::from_secs(3);
/// Signaling silent this long means it is gone; Chime sends BITRATES
/// every four seconds (HuddleFM's `silenceLimitMs`).
const SILENCE_LIMIT: Duration = Duration::from_secs(15);
const PING_EVERY: Duration = Duration::from_secs(10);
const STATS_EVERY: Duration = Duration::from_secs(5);
/// Muted is still sending, as a browser does: Opus's 20 ms of silence.
pub const SILENT_OPUS: [u8; 3] = [0xF8, 0xFF, 0xFE];
const AUDIO_TICK: Duration = Duration::from_millis(20);
/// Where bandwidth estimation starts, in kbit/s, when we may send video.
const BWE_START_KBPS: u64 = 700;
/// What the estimate leaves for the audio and the packets' overhead.
#[cfg(feature = "huddle-camera")]
const AUDIO_SHARE_BPS: u64 = 80_000;

/// The step that failed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stage {
    /// Opening Chime's signaling socket.
    Signaling,
    /// JOIN to JOIN_ACK and INDEX.
    Join,
    /// Getting a TURN relay.
    Relay,
    /// The SDP offer, SUBSCRIBE and its answer.
    Subscribe,
    /// ICE and DTLS through the relay.
    Connect,
    /// While listening.
    Media,
}

/// Why listening stopped before it was asked to.
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

/// What a session did, for the log's last lines.
#[derive(Clone, Debug, Default)]
pub struct Report {
    /// How it ended.
    pub ending: Option<String>,
    /// Signaling frames by type.
    pub frames: std::collections::BTreeMap<String, u64>,
    /// The TURN server used, and the relay it gave.
    pub relay: Option<String>,
    /// When ICE connected, since the signaling socket opened.
    pub ice_connected: Option<Duration>,
    /// When DTLS and SRTP were up.
    pub dtls_up: Option<Duration>,
    /// When the first audio came.
    pub first_audio: Option<Duration>,
    /// Opus frames received.
    pub audio_frames: u64,
    /// Their bytes.
    pub audio_bytes: u64,
    /// The most attendees Chime listed at once.
    pub most_attendees: usize,
    /// Whether it ended because the meeting did: Chime's close 4410 or
    /// its audio status 410. Not a failure; the huddle is over.
    pub meeting_ended: bool,
    /// Opus frames sent from the microphone (or the probe's tone).
    pub sent_frames: u64,
    /// Their bytes.
    pub sent_bytes: u64,
    /// Frames of silence sent while muted.
    pub silent_frames: u64,
    /// What the probe saw of video, when it looked.
    pub video: Option<video::Summary>,
    /// The error status Chime refused the join or a SUBSCRIBE with, if it
    /// ended the session (403, 409, 509, …), or 206 when it took no video
    /// from us: a screen share learns from it that the meeting has all
    /// the shares it takes.
    pub refused_status: Option<u32>,
}

/// What a session sends besides silence: frames from the microphone, and
/// whether it is muted. Muted, frames that still come are dropped.
#[derive(Debug)]
pub struct Uplink {
    /// Encoded frames, in order.
    pub frames: tokio::sync::mpsc::Receiver<Outgoing>,
    /// Whether the microphone is muted (closed); the session starts as it
    /// says.
    pub muted: tokio::sync::watch::Receiver<bool>,
}

/// A relay being set up or in use.
struct Relay {
    server: Server,
    client: turn::Client,
    io: RelayIo,
    /// Our end of the connection to the TURN server.
    local: SocketAddr,
    relayed: Option<SocketAddr>,
}

/// The connection to a TURN server.
enum RelayIo {
    Udp(tokio::net::UdpSocket),
    Stream {
        stream: Box<dyn Stream>,
        buffer: Vec<u8>,
    },
}

/// A TCP or TLS stream.
trait Stream: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send {}
impl<T: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send> Stream for T {}

impl RelayIo {
    async fn send(&mut self, bytes: &[u8]) -> std::io::Result<()> {
        match self {
            Self::Udp(socket) => socket.send(bytes).await.map(|_| ()),
            Self::Stream { stream, .. } => stream.write_all(bytes).await,
        }
    }

    /// The next messages from the server.
    async fn recv(&mut self) -> std::io::Result<Vec<Vec<u8>>> {
        match self {
            Self::Udp(socket) => {
                let mut buf = vec![0u8; 2048];
                let n = socket.recv(&mut buf).await?;
                buf.truncate(n);
                Ok(vec![buf])
            }
            Self::Stream { stream, buffer } => {
                let mut chunk = [0u8; 4096];
                let n = stream.read(&mut chunk).await?;
                if n == 0 {
                    return Err(std::io::Error::other(
                        "the TURN server closed the connection",
                    ));
                }
                buffer.extend_from_slice(&chunk[..n]);
                let mut messages = Vec::new();
                while let Some(message) = turn::split_stream(buffer) {
                    messages.push(message);
                }
                Ok(messages)
            }
        }
    }
}

/// The TLS setup for `turns:` servers: the system's roots, ring's crypto,
/// as the rest of the app's TLS.
fn tls_config() -> Result<Arc<tokio_rustls::rustls::ClientConfig>, String> {
    use tokio_rustls::rustls;
    let mut roots = rustls::RootCertStore::empty();
    for cert in rustls_native_certs::load_native_certs().certs {
        let _ = roots.add(cert);
    }
    let config = rustls::ClientConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .map_err(|e| e.to_string())?
    .with_root_certificates(roots)
    .with_no_client_auth();
    Ok(Arc::new(config))
}

/// Opens the connection to `server` and starts an allocation on it.
async fn open_relay(server: &Server, turn: &TurnCredentials) -> Result<Relay, String> {
    let address = tokio::net::lookup_host((server.host.as_str(), server.port))
        .await
        .map_err(|e| format!("{}: {e}", server.host))?
        .next()
        .ok_or_else(|| format!("{} has no address", server.host))?;
    let (io, local) = match server.transport {
        Transport::Udp => {
            let any: SocketAddr = if address.is_ipv4() {
                "0.0.0.0:0".parse().map_err(|_| "no IPv4 wildcard")?
            } else {
                "[::]:0".parse().map_err(|_| "no IPv6 wildcard")?
            };
            let socket = tokio::net::UdpSocket::bind(any)
                .await
                .map_err(|e| e.to_string())?;
            socket.connect(address).await.map_err(|e| e.to_string())?;
            let local = socket.local_addr().map_err(|e| e.to_string())?;
            (RelayIo::Udp(socket), local)
        }
        Transport::Tcp | Transport::Tls => {
            let tcp = tokio::net::TcpStream::connect(address)
                .await
                .map_err(|e| e.to_string())?;
            let _ = tcp.set_nodelay(true);
            let local = tcp.local_addr().map_err(|e| e.to_string())?;
            let stream: Box<dyn Stream> = if server.transport == Transport::Tls {
                let name =
                    tokio_rustls::rustls::pki_types::ServerName::try_from(server.host.clone())
                        .map_err(|e| e.to_string())?;
                let tls = tokio_rustls::TlsConnector::from(tls_config()?)
                    .connect(name, tcp)
                    .await
                    .map_err(|e| format!("TLS: {e}"))?;
                Box::new(tls)
            } else {
                Box::new(tcp)
            };
            (
                RelayIo::Stream {
                    stream,
                    buffer: Vec::new(),
                },
                local,
            )
        }
    };
    log::info!("relay: connected to {server} at {address} from {local}");
    Ok(Relay {
        server: server.clone(),
        client: turn::Client::new(server.transport, &turn.username, &turn.password),
        io,
        local,
        relayed: None,
    })
}

/// The WebRTC peer, its one candidate the relay at `relayed` (reached
/// from `local`): Opus, and VP8 (unless `h264_only`) and H.264 for the
/// video m-lines. Chime answers what is offered and tells the senders
/// the codecs every receiver takes, so leaving VP8 out asks them for
/// H.264.
#[cfg(test)]
fn new_peer(relayed: SocketAddr, local: SocketAddr, h264_only: bool) -> Result<Rtc, String> {
    new_peer_with(relayed, local, h264_only, false)
}

/// [`new_peer`], with `str0m`'s send-side bandwidth estimation when we
/// may send video (`bwe`): it sets the camera's bitrate.
fn new_peer_with(
    relayed: SocketAddr,
    local: SocketAddr,
    h264_only: bool,
    bwe: bool,
) -> Result<Rtc, String> {
    let mut config = RtcConfig::new()
        .clear_codecs()
        .enable_opus(true, false)
        .enable_vp8(!h264_only)
        .enable_h264(true)
        .set_crypto_provider(Arc::new(dtls::provider()))
        // Brings Chime's RTCP receiver reports on what we send.
        .set_stats_interval(Some(STATS_EVERY));
    if bwe {
        config = config.enable_bwe(Some(str0m::bwe::Bitrate::kbps(BWE_START_KBPS)));
    }
    let mut rtc = config.build(Instant::now());
    let candidate =
        Candidate::relayed(relayed, local, "udp").map_err(|e| format!("relay candidate: {e}"))?;
    rtc.add_local_candidate(candidate);
    Ok(rtc)
}

/// An offer made, in the form Chime takes.
struct Offer {
    sdp: String,
    pending: SdpPendingOffer,
    mids: Mids,
    audio: Mid,
    /// The inactive video m-line: the send line, the first of the video
    /// slots.
    video: Mid,
}

/// The offer: audio both ways (silence out, as a muted browser sends) and
/// an inactive video m-line, which Chime wants to see (HuddleFM's
/// `videoDescription`).
fn make_offer(rtc: &mut Rtc) -> Option<Offer> {
    let mut api = rtc.sdp_api();
    let audio = api.add_media(
        MediaKind::Audio,
        Direction::SendRecv,
        Some("noslacking".into()),
        Some("audio".into()),
        None,
    );
    let video = api.add_media(MediaKind::Video, Direction::Inactive, None, None, None);
    let (offer, pending) = api.apply()?;
    let offer = offer.to_sdp_string();
    let mids = Mids::of_offer(&offer);
    Some(Offer {
        sdp: mids.offer_for_chime(&offer),
        pending,
        mids,
        audio,
        video,
    })
}

/// Frames that come every few seconds and say little: logged at debug
/// level only, and counted.
fn quiet(frame: &chime::Frame) -> bool {
    matches!(
        FrameType::try_from(frame.r#type),
        Ok(FrameType::PingPong
            | FrameType::Bitrates
            | FrameType::AudioMetadata
            | FrameType::ClientMetric)
    )
}

/// Milliseconds since 1970, for frame timestamps.
fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}

/// Everything one session holds.
struct Session<'a> {
    join: &'a ChimeJoin,
    started: Instant,
    socket: Socket,
    handshake: Handshake,
    turn: Option<TurnCredentials>,
    servers: VecDeque<Server>,
    relay: Option<Relay>,
    relay_deadline: Option<Instant>,
    rtc: Option<Rtc>,
    rtc_timeout: Option<Instant>,
    pending: Option<SdpPendingOffer>,
    mids: Mids,
    audio: Option<Mid>,
    offer_wanted: bool,
    index_deadline: Option<Instant>,
    connect_deadline: Option<Instant>,
    leave_deadline: Option<Instant>,
    last_inbound: Instant,
    next_ping: Option<Instant>,
    ping_id: u32,
    next_stats: Instant,
    next_audio: Option<Instant>,
    /// The audio track's RTP clock.
    outbound: Outbound,
    /// Frames from the microphone, if there is one.
    frames: Option<tokio::sync::mpsc::Receiver<Outgoing>>,
    /// Whether it is muted, as it changes.
    muted_rx: Option<tokio::sync::watch::Receiver<bool>>,
    /// Muted now: silence goes out.
    muted: bool,
    /// Unmuted and the microphone's frames have started: silence stops.
    flowing: bool,
    feed: Option<Feed>,
    /// Told once the audio connection is up.
    live: Option<tokio::sync::oneshot::Sender<()>>,
    /// Told who is in the huddle and speaking, when that changes.
    roster: Option<tokio::sync::watch::Sender<Roster>>,
    /// When each attendee was last heard.
    voices: Voices,
    /// Chime's head count, from the last INDEX.
    count: Option<u32>,
    report: Report,
    /// What it follows of video: the probe's look, and the shares the
    /// app watches.
    video: Option<Watch>,
    /// What the call window wants, as it changes.
    wish: Option<tokio::sync::watch::Receiver<Wish>>,
    /// Our camera, if this session may send one.
    camera: Option<CameraSide>,
    /// How it ended, once it has.
    over: Option<Result<(), Failure>>,
}

impl Session<'_> {
    fn since(&self) -> Duration {
        self.started.elapsed()
    }

    async fn send(&mut self, frame: &chime::Frame) {
        if quiet(frame) {
            log::debug!("signaling: sending {}", chime::describe(frame));
        } else {
            log::info!("signaling: sending {}", chime::describe(frame));
        }
        if let Err(error) = self.socket.send(frame).await {
            log::warn!(
                "signaling: could not send {}: {error}",
                chime::type_name(frame)
            );
        }
    }

    /// Carries out what the handshake asks.
    async fn carry(&mut self, steps: Vec<Step>) {
        for step in steps {
            match step {
                Step::Send(frame) => self.send(&frame).await,
                Step::Turn(turn) => {
                    log::info!(
                        "join: JOIN_ACK; TURN servers {:?}, ttl {:?} s",
                        turn.uris,
                        turn.ttl
                    );
                    self.servers = turn::by_preference(&turn.uris).into();
                    self.turn = Some(turn);
                    self.index_deadline = Some(Instant::now() + INDEX_WAIT);
                    self.next_ping = Some(Instant::now() + PING_EVERY);
                    self.next_relay().await;
                }
                Step::Offer => {
                    self.index_deadline = None;
                    self.offer_wanted = true;
                    self.try_offer().await;
                }
                Step::Answer(answer) => self.answer(&answer),
                Step::Presence { attendee, present } => {
                    log::info!(
                        "presence: {} ({}) {}{}",
                        attendee.external_user_id.as_deref().unwrap_or("?"),
                        attendee.attendee_id,
                        if present { "here" } else { "left" },
                        if attendee.muted { ", muted" } else { "" }
                    );
                    self.report.most_attendees = self
                        .report
                        .most_attendees
                        .max(self.handshake.attendees().len());
                }
                Step::Note(note) => log::warn!("signaling: {note}"),
                Step::Over(ending) => self.end(ending),
            }
        }
    }

    fn end(&mut self, ending: Ending) {
        log::info!("signaling: over: {ending}");
        self.report.ending = Some(ending.to_string());
        if let Ending::Refused { status, .. } = &ending {
            self.report.refused_status = Some(*status);
        }
        if self.over.is_some() {
            return;
        }
        let meeting_ended = matches!(
            ending,
            Ending::Closed {
                code: super::signaling::MEETING_ENDED_CLOSE,
                ..
            } | Ending::Audio(chime::AudioStatus::MeetingEnded)
        );
        if meeting_ended {
            self.report.meeting_ended = true;
            self.over = Some(Ok(()));
            return;
        }
        let stage = match self.handshake_stage() {
            Some(stage) => stage,
            None => Stage::Media,
        };
        self.over = Some(match ending {
            Ending::Left => Ok(()),
            other => Err(failure(stage, other.to_string())),
        });
    }

    /// The stage a failure now belongs to.
    fn handshake_stage(&self) -> Option<Stage> {
        use super::signaling::Phase;
        Some(match self.handshake.phase() {
            Phase::Idle | Phase::Joining | Phase::Indexing => Stage::Join,
            Phase::Offering | Phase::Subscribing => Stage::Subscribe,
            Phase::Live if self.report.dtls_up.is_none() => Stage::Connect,
            _ => return None,
        })
    }

    /// Tries the next TURN server, or fails when none is left.
    async fn next_relay(&mut self) {
        let Some(turn) = self.turn.clone() else {
            return;
        };
        while let Some(server) = self.servers.pop_front() {
            log::info!("relay: trying {server}");
            match tokio::time::timeout(RELAY_TIMEOUT, open_relay(&server, &turn)).await {
                Ok(Ok(mut relay)) => {
                    relay.client.allocate(Instant::now());
                    self.relay = Some(relay);
                    self.relay_deadline = Some(Instant::now() + RELAY_TIMEOUT);
                    self.flush_relay().await;
                    return;
                }
                Ok(Err(why)) => log::warn!("relay: {server}: {why}"),
                Err(_) => log::warn!("relay: {server}: no connection in time"),
            }
        }
        self.over
            .get_or_insert(Err(failure(Stage::Relay, "no TURN server gave a relay")));
    }

    /// Writes what the TURN client queued.
    async fn flush_relay(&mut self) {
        let Some(relay) = &mut self.relay else {
            return;
        };
        while let Some(bytes) = relay.client.poll_transmit() {
            if let Err(error) = relay.io.send(&bytes).await {
                log::warn!("relay: could not write to {}: {error}", relay.server);
                break;
            }
        }
    }

    /// Reads TURN events: the allocation, data for `str0m`, failures.
    async fn relay_events(&mut self) {
        loop {
            let Some(relay) = &mut self.relay else {
                return;
            };
            let Some(event) = relay.client.poll_event() else {
                break;
            };
            match event {
                turn::Event::Allocated {
                    relayed,
                    mapped,
                    lifetime,
                } => {
                    relay.relayed = Some(relayed);
                    self.relay_deadline = None;
                    let line = format!(
                        "{} relays at {relayed} (it sees us at {}; {lifetime} s)",
                        relay.server,
                        mapped.map_or_else(|| "?".to_owned(), |m| m.to_string())
                    );
                    log::info!("relay: {line}");
                    self.report.relay = Some(line);
                    self.make_rtc(relayed);
                    self.try_offer().await;
                }
                turn::Event::Permitted(ip) => log::info!("relay: {ip} may reach us"),
                turn::Event::Data { peer, data } => self.relayed_in(peer, &data),
                turn::Event::Failed(why) => {
                    let allocated = relay.relayed.is_some();
                    log::warn!("relay: {}: {why}", relay.server);
                    self.relay = None;
                    if allocated {
                        self.over.get_or_insert(Err(failure(
                            Stage::Media,
                            format!("relay lost: {why}"),
                        )));
                    } else {
                        self.next_relay().await;
                    }
                }
                turn::Event::Note(note) => log::info!("relay: {note}"),
            }
        }
        self.flush_relay().await;
    }

    /// Builds the WebRTC peer once the relay is there.
    fn make_rtc(&mut self, relayed: SocketAddr) {
        if self.rtc.is_some() {
            return;
        }
        let Some(local) = self.relay.as_ref().map(|r| r.local) else {
            return;
        };
        let h264_only = self.video.as_ref().is_some_and(Watch::h264_only);
        match new_peer_with(relayed, local, h264_only, self.camera.is_some()) {
            Ok(rtc) => self.rtc = Some(rtc),
            Err(why) => {
                self.over.get_or_insert(Err(failure(Stage::Relay, why)));
            }
        }
    }

    /// Makes the offer and subscribes, once both the relay and the
    /// handshake are ready for it.
    async fn try_offer(&mut self) {
        if !self.offer_wanted || self.pending.is_some() {
            return;
        }
        let Some(rtc) = &mut self.rtc else {
            return;
        };
        let Some(made) = make_offer(rtc) else {
            self.over
                .get_or_insert(Err(failure(Stage::Subscribe, "no offer to make")));
            return;
        };
        self.audio = Some(made.audio);
        self.pending = Some(made.pending);
        self.mids = made.mids;
        if let Some(watch) = &mut self.video {
            watch.offered(made.video);
        }
        let offer = made.sdp;
        log::info!("subscribe: offer {}", sdp::summary(&offer));
        let sub = chime::Subscribe {
            sdp_offer: offer,
            audio_host: self.join.audio_host_url.clone(),
            attendee_id: self.join.attendee_id.clone(),
            muted: self.muted,
            receive_stream_ids: vec![0],
            video: None,
        };
        let steps = self.handshake.subscribe(&sub, now_ms());
        Box::pin(self.carry(steps)).await;
    }

    /// Once live, a new offer and SUBSCRIBE when other video streams are
    /// wanted than are received; audio goes on meanwhile.
    async fn try_resubscribe(&mut self) {
        use super::signaling::Phase;
        let now = Instant::now();
        if self.handshake.phase() != Phase::Live
            || self.pending.is_some()
            || self.leave_deadline.is_some()
            || self.over.is_some()
        {
            return;
        }
        let (Some(watch), Some(rtc), Some(dtls_up)) =
            (&mut self.video, &mut self.rtc, self.report.dtls_up)
        else {
            return;
        };
        let Some(made) = watch.reoffer(rtc, now, self.started + dtls_up, self.report.audio_frames)
        else {
            return;
        };
        self.pending = Some(made.pending);
        self.mids = Mids::of_offer(&made.offer);
        let offer = self.mids.offer_for_chime(&made.offer);
        log::info!("subscribe: new offer {}", sdp::summary(&offer));
        let sub = chime::Subscribe {
            sdp_offer: offer,
            audio_host: self.join.audio_host_url.clone(),
            attendee_id: self.join.attendee_id.clone(),
            muted: self.muted,
            receive_stream_ids: made.receive_stream_ids,
            video: made.video,
        };
        let steps = self.handshake.resubscribe(&sub, now_ms());
        Box::pin(self.carry(steps)).await;
    }

    /// Takes Chime's answer.
    fn answer(&mut self, answer: &str) {
        log::info!("subscribe: answer {}", sdp::summary(answer));
        let chime_answer = answer;
        let answer = self.mids.answer_from_chime(answer);
        let (Some(rtc), Some(pending)) = (&mut self.rtc, self.pending.take()) else {
            self.over
                .get_or_insert(Err(failure(Stage::Subscribe, "an answer with no offer")));
            return;
        };
        let parsed = match SdpAnswer::from_sdp_string(&answer) {
            Ok(parsed) => parsed,
            Err(error) => {
                self.over.get_or_insert(Err(failure(
                    Stage::Subscribe,
                    format!("the answer does not parse: {error}"),
                )));
                return;
            }
        };
        if let Err(error) = rtc.sdp_api().accept_answer(pending, parsed) {
            self.over.get_or_insert(Err(failure(
                Stage::Subscribe,
                format!("the answer was not accepted: {error}"),
            )));
            return;
        }
        let peers = sdp::candidate_addresses(&answer);
        log::info!("connect: media server candidates {peers:?}");
        if peers.is_empty() {
            self.over.get_or_insert(Err(failure(
                Stage::Subscribe,
                "the answer has no UDP candidate to reach",
            )));
            return;
        }
        if let Some(relay) = &mut self.relay {
            let ips: Vec<_> = peers.iter().map(SocketAddr::ip).collect();
            relay.client.permit(&ips, Instant::now());
        }
        self.rtc_timeout = Some(Instant::now());
        if self.report.dtls_up.is_some() {
            // A re-SUBSCRIBE's answer: the connection is up already.
            if let Some(watch) = &mut self.video {
                watch.answered(chime_answer, &self.mids, Instant::now());
            }
            return;
        }
        self.connect_deadline = Some(Instant::now() + CONNECT_TIMEOUT);
    }

    /// Feeds `str0m` what a peer sent through the relay.
    fn relayed_in(&mut self, peer: SocketAddr, data: &[u8]) {
        let (Some(rtc), Some(relayed)) =
            (&mut self.rtc, self.relay.as_ref().and_then(|r| r.relayed))
        else {
            return;
        };
        let Ok(receive) = Receive::new(Protocol::Udp, peer, relayed, data) else {
            log::debug!(
                "connect: {} bytes from {peer} that WebRTC does not read",
                data.len()
            );
            return;
        };
        if let Err(error) = rtc.handle_input(Input::Receive(Instant::now(), receive)) {
            log::debug!("connect: input from {peer}: {error}");
        }
        self.rtc_timeout = Some(Instant::now());
    }

    /// Runs `str0m` until it waits: what it sends goes to the relay, what
    /// it tells is logged or played.
    fn drive_rtc(&mut self) {
        let now = Instant::now();
        let Some(rtc) = &mut self.rtc else {
            return;
        };
        if self.rtc_timeout.is_some_and(|at| at <= now)
            && let Err(error) = rtc.handle_input(Input::Timeout(now))
        {
            log::debug!("connect: timeout input: {error}");
        }
        let mut events = Vec::new();
        loop {
            match rtc.poll_output() {
                Ok(Output::Timeout(at)) => {
                    self.rtc_timeout = Some(at);
                    break;
                }
                Ok(Output::Transmit(transmit)) => match &mut self.relay {
                    Some(relay) if Some(transmit.source) == relay.relayed => {
                        relay
                            .client
                            .send_to(transmit.destination, &transmit.contents, now);
                    }
                    _ => log::debug!(
                        "connect: dropped a packet from {} (not the relay)",
                        transmit.source
                    ),
                },
                Ok(Output::Event(event)) => events.push(event),
                Err(error) => {
                    self.over
                        .get_or_insert(Err(failure(Stage::Media, format!("WebRTC: {error}"))));
                    break;
                }
            }
        }
        for event in events {
            self.rtc_event(event);
        }
    }

    fn rtc_event(&mut self, event: RtcEvent) {
        match event {
            RtcEvent::IceConnectionStateChange(state) => {
                log::info!("connect: ICE {state:?}");
                if matches!(
                    state,
                    IceConnectionState::Connected | IceConnectionState::Completed
                ) && self.report.ice_connected.is_none()
                {
                    self.report.ice_connected = Some(self.since());
                }
                if state == IceConnectionState::Disconnected && self.report.dtls_up.is_some() {
                    self.over
                        .get_or_insert(Err(failure(Stage::Media, "ICE disconnected")));
                }
            }
            RtcEvent::Connected => {
                log::info!("connect: DTLS and SRTP are up");
                self.report.dtls_up = Some(self.since());
                self.connect_deadline = None;
                if let Some(live) = self.live.take() {
                    let _ = live.send(());
                }
                self.next_audio = Some(Instant::now());
            }
            RtcEvent::MediaAdded(added) => log::info!(
                "connect: media {:?} {:?} {:?}",
                added.mid,
                added.kind,
                added.direction
            ),
            RtcEvent::MediaEgressStats(stats) if Some(stats.mid) == self.audio => {
                let remote = stats.remote.as_ref().map_or_else(
                    || "no report yet".to_owned(),
                    |r| {
                        format!(
                            "jitter {} (48 kHz units), {} lost in all, up to seq {}",
                            r.jitter, r.packets_lost, *r.maximum_sequence_number
                        )
                    },
                );
                log::info!(
                    "media: Chime receives ours: {} packets ({} bytes) sent; loss {:?}, rtt {:?}; \
                     {remote}",
                    stats.packets,
                    stats.bytes,
                    stats.loss,
                    stats.rtt
                );
            }
            RtcEvent::KeyframeRequest(request) => self.keyframe_request(&request),
            RtcEvent::EgressBitrateEstimate(estimate) => self.bitrate_estimate(&estimate),
            RtcEvent::MediaData(data) => {
                if Some(data.mid) != self.audio {
                    if let (Some(watch), Some(rtc)) = (&mut self.video, &mut self.rtc) {
                        watch.media(rtc, &data, Instant::now());
                    }
                    return;
                }
                if self.report.first_audio.is_none() {
                    self.report.first_audio = Some(self.since());
                    log::info!(
                        "media: first audio: payload type {:?}, {} bytes, {:?}",
                        data.pt,
                        data.data.len(),
                        data.params.spec().codec
                    );
                }
                self.report.audio_frames += 1;
                self.report.audio_bytes += data.data.len() as u64;
                if let Some(feed) = &self.feed {
                    // RTP's 32-bit timestamp; the jitter buffer unwraps it.
                    feed.push(data.time.numer() as u32, &data.data);
                }
            }
            _ => {}
        }
    }

    /// Writes one Opus packet on the audio track: its RTP time and marker
    /// from `stamp`, and its level for the RFC 6464 extension (written
    /// only if Chime's answer took it). False if it could not.
    fn write_audio(&mut self, stamp: Stamp, payload: Vec<u8>, level: (u8, bool)) -> bool {
        let now = Instant::now();
        let (Some(rtc), Some(mid)) = (&mut self.rtc, self.audio) else {
            return false;
        };
        let Some(writer) = rtc.writer(mid) else {
            return false;
        };
        let Some(pt) = writer
            .payload_params()
            .find(|p| p.spec().codec == Codec::Opus)
            .map(|p| p.pt())
        else {
            return false;
        };
        // str0m takes the level negative, 0 to -127.
        let (level, voice) = level;
        let negative = -i8::try_from(level.min(127)).unwrap_or(127);
        let writer = writer
            .start_of_talkspurt(stamp.talkspurt)
            .audio_level(negative, voice);
        let time = MediaTime::new(stamp.time, str0m::media::Frequency::FORTY_EIGHT_KHZ);
        match writer.write(pt, now, time, payload) {
            Ok(()) => true,
            Err(error) => {
                log::debug!("media: could not send audio: {error}");
                false
            }
        }
    }

    /// Sends 20 ms of silence, keeping the audio stream alive while
    /// muted, or until the microphone's first frame.
    fn send_silence(&mut self) {
        if !self.muted && self.flowing {
            return;
        }
        let stamp = self.outbound.stamp(0);
        if self.write_audio(stamp, SILENT_OPUS.to_vec(), (127, false)) {
            self.report.silent_frames += 1;
        }
    }

    /// Sends a frame from the microphone, unless muted.
    fn send_frame(&mut self, frame: Outgoing) {
        if self.muted || self.report.dtls_up.is_none() || self.leave_deadline.is_some() {
            return;
        }
        self.flowing = true;
        let stamp = self.outbound.stamp(frame.gap);
        let bytes = frame.payload.len() as u64;
        if self.write_audio(stamp, frame.payload, (frame.level, frame.voice)) {
            self.report.sent_frames += 1;
            self.report.sent_bytes += bytes;
        }
    }

    /// The microphone was muted or unmuted: tell Chime, as the JS SDK
    /// does, once it has been subscribed to (until then SUBSCRIBE says
    /// it).
    async fn mute_changed(&mut self, muted: bool) {
        use super::signaling::Phase;
        if muted == self.muted {
            return;
        }
        self.muted = muted;
        self.flowing = false;
        log::info!("media: {}", if muted { "muted" } else { "unmuted" });
        if matches!(
            self.handshake.phase(),
            Phase::Subscribing | Phase::Live | Phase::Resubscribing
        ) {
            self.send(&chime::audio_control(muted, now_ms())).await;
        }
    }

    fn stats(&self) {
        let played = self
            .feed
            .as_ref()
            .map(|f| format!("{:?}", f.played()))
            .unwrap_or_default();
        log::info!(
            "media: {} s in: {} audio frames ({} bytes), {} attendees; {played}",
            self.since().as_secs(),
            self.report.audio_frames,
            self.report.audio_bytes,
            self.handshake.attendees().len(),
        );
        log::info!(
            "media: {} s out: {} frames ({} bytes) sent, {} of silence; {}",
            self.since().as_secs(),
            self.report.sent_frames,
            self.report.sent_bytes,
            self.report.silent_frames,
            if self.muted { "muted" } else { "unmuted" }
        );
        self.camera_stats();
    }

    /// The next moment something is due.
    fn deadline(&self) -> Instant {
        let mut at = self.last_inbound + SILENCE_LIMIT;
        for due in [
            self.rtc_timeout,
            self.relay.as_ref().and_then(|r| r.client.poll_timeout()),
            self.relay_deadline,
            self.index_deadline,
            self.connect_deadline,
            self.leave_deadline,
            self.next_ping,
            self.next_audio,
            Some(self.next_stats),
            self.video.as_ref().and_then(|w| w.due(Instant::now())),
            self.video
                .as_ref()
                .and_then(Watch::in_flight_since)
                .map(|at| at + watch::ANSWER_WAIT),
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
        if now >= self.last_inbound + SILENCE_LIMIT {
            self.over.get_or_insert(Err(failure(
                Stage::Media,
                format!("signaling silent for {} s", SILENCE_LIMIT.as_secs()),
            )));
            return;
        }
        if self.leave_deadline.is_some_and(|at| at <= now) {
            log::warn!("leave: no LEAVE_ACK in time");
            self.over.get_or_insert(Ok(()));
            return;
        }
        if self.relay_deadline.is_some_and(|at| at <= now) {
            log::warn!("relay: no allocation in time");
            self.relay = None;
            self.relay_deadline = None;
            self.next_relay().await;
        }
        if self.index_deadline.is_some_and(|at| at <= now) {
            self.index_deadline = None;
            let steps = self.handshake.index_timed_out();
            self.carry(steps).await;
        }
        if self.connect_deadline.is_some_and(|at| at <= now) {
            self.over.get_or_insert(Err(failure(
                Stage::Connect,
                format!(
                    "no media connection within {} s (ICE {}connected)",
                    CONNECT_TIMEOUT.as_secs(),
                    if self.report.ice_connected.is_some() {
                        ""
                    } else {
                        "never "
                    }
                ),
            )));
            return;
        }
        if self.next_ping.is_some_and(|at| at <= now) {
            self.ping_id = self.ping_id.wrapping_add(1);
            let ping =
                chime::ping_pong(chime::proto::SdkPingPongType::Ping, self.ping_id, now_ms());
            self.send(&ping).await;
            self.next_ping = Some(now + PING_EVERY);
        }
        if let Some(relay) = &mut self.relay
            && relay.client.poll_timeout().is_some_and(|at| at <= now)
        {
            relay.client.handle_timeout(now);
        }
        if self.next_audio.is_some_and(|at| at <= now) && self.leave_deadline.is_none() {
            self.send_silence();
            self.next_audio = Some(now + AUDIO_TICK);
        }
        if self.next_stats <= now {
            if self.report.dtls_up.is_some() {
                self.stats();
            }
            self.next_stats = now + STATS_EVERY;
        }
        // Speaking marks fade even when no frame comes to say so.
        self.tell_roster(now);
        self.video_tick(now).await;
    }

    /// Video: keyframe requests, counts, a re-SUBSCRIBE given up or due.
    async fn video_tick(&mut self, now: Instant) {
        let Some(watch) = &mut self.video else {
            return;
        };
        watch.tick(self.rtc.as_mut(), now, self.report.audio_frames);
        if watch
            .in_flight_since()
            .is_some_and(|at| now >= at + watch::ANSWER_WAIT)
        {
            watch.abandon();
            // The offer is dropped; `str0m` takes the next one fresh.
            self.pending = None;
            let steps = self.handshake.resubscribe_timed_out();
            self.carry(steps).await;
        }
        self.try_resubscribe().await;
    }

    /// Tells who is in the huddle and speaking, if that changed.
    fn tell_roster(&mut self, now: Instant) {
        let Some(tell) = &self.roster else {
            return;
        };
        let now = roster::roster(
            self.handshake.attendees(),
            &self.join.attendee_id,
            &self.voices,
            self.count,
            now,
        );
        tell.send_if_modified(|roster| {
            let changed = *roster != now;
            if changed {
                *roster = now;
            }
            changed
        });
    }

    /// One frame from Chime.
    async fn on_frame(&mut self, frame: &chime::Frame) {
        self.last_inbound = Instant::now();
        if let Some(metadata) = &frame.audio_metadata {
            self.voices.metadata(metadata, self.last_inbound);
            // Who speaks decides who gets a camera tile.
            if let Some(watch) = &mut self.video {
                for (&stream, attendee) in self.handshake.attendees() {
                    if self.voices.speaking(stream, self.last_inbound) {
                        watch.spoke(&attendee.attendee_id, self.last_inbound);
                    }
                }
            }
        }
        if let Some(count) = frame.index.as_ref().and_then(|i| i.num_participants) {
            self.count = Some(count);
        }
        if let Some(watch) = &mut self.video {
            watch.frame(frame, self.last_inbound);
        }
        let view_only = self.view_only(frame);
        let name = chime::type_name(frame);
        *self.report.frames.entry(name).or_default() += 1;
        if quiet(frame) {
            log::debug!("signaling: received {}", chime::describe(frame));
        } else {
            log::info!("signaling: received {}", chime::describe(frame));
        }
        let steps = self.handshake.on_frame(frame, now_ms());
        self.carry(steps).await;
        if view_only {
            self.refused();
        }
        self.tell_roster(self.last_inbound);
        if frame.index.is_some() {
            self.try_resubscribe().await;
        }
    }

    /// Starts leaving: LEAVE, then up to three seconds for LEAVE_ACK.
    async fn leave(&mut self) {
        if self.leave_deadline.is_some() || self.over.is_some() {
            return;
        }
        log::info!("leave: sending LEAVE");
        self.leave_deadline = Some(Instant::now() + LEAVE_TIMEOUT);
        let steps = self.handshake.leave(now_ms());
        self.carry(steps).await;
    }

    /// Lets go of the relay and the socket.
    async fn close(&mut self) {
        if let Some(rtc) = &mut self.rtc {
            rtc.disconnect();
        }
        if let Some(relay) = &mut self.relay {
            relay.client.close(Instant::now());
        }
        self.flush_relay().await;
        self.socket.close().await;
    }
}

/// The next messages from the relay, or never while there is none.
async fn relay_recv(relay: &mut Option<Relay>) -> std::io::Result<Vec<Vec<u8>>> {
    match relay {
        Some(relay) => relay.io.recv().await,
        None => std::future::pending().await,
    }
}

/// The stop signal's next change, or never once it has been heard: a
/// closed channel would otherwise answer at once, over and over.
async fn stopped(
    stop: &mut tokio::sync::watch::Receiver<bool>,
    heard: bool,
) -> Result<(), tokio::sync::watch::error::RecvError> {
    if heard {
        std::future::pending().await
    } else {
        stop.changed().await
    }
}

/// The next frame from the microphone, or never while there is none.
async fn next_frame(
    frames: &mut Option<tokio::sync::mpsc::Receiver<Outgoing>>,
) -> Option<Outgoing> {
    match frames {
        Some(frames) => frames.recv().await,
        None => std::future::pending().await,
    }
}

/// What a session does with video: the probe's or `--video`'s look at
/// it, and the app's viewer of screen shares; neither, none at all.
#[derive(Debug, Default)]
pub struct Video {
    /// The probe's options, or `--video`'s.
    pub options: Option<video::Options>,
    /// The call window's side, with `huddle-video`.
    pub viewer: Option<Viewer>,
    /// Our camera, with `huddle-camera` (or the probe's test picture).
    #[cfg(feature = "huddle-camera")]
    pub camera: Option<super::camera_send::CameraUplink>,
}

/// The call window's next wish, or never while nothing tells.
async fn wish_change(
    wish: &mut Option<tokio::sync::watch::Receiver<Wish>>,
) -> Result<Wish, tokio::sync::watch::error::RecvError> {
    match wish {
        Some(wish) => {
            wish.changed().await?;
            Ok(wish.borrow_and_update().clone())
        }
        None => std::future::pending().await,
    }
}

/// The mute state's next change, or never while there is none.
async fn mute_change(
    muted: &mut Option<tokio::sync::watch::Receiver<bool>>,
) -> Result<bool, tokio::sync::watch::error::RecvError> {
    match muted {
        Some(muted) => {
            muted.changed().await?;
            Ok(*muted.borrow_and_update())
        }
        None => std::future::pending().await,
    }
}

/// Listens to the huddle `join` describes until `stop` turns true or the
/// session ends, feeding the audio to `feed`, telling `live` once the
/// audio connection is up and `roster` who is in it whenever that
/// changes; talks too, if given an `uplink` (otherwise muted
/// throughout). With `video` (the probe's, or `--video`'s), logs what
/// Chime says of video and receives what it asks; with a `viewer`, tells
/// who shares their screen and receives the share it watches (see
/// [`super::watch`]). Leaves cleanly either way.
pub async fn listen(
    join: &ChimeJoin,
    feed: Option<Feed>,
    uplink: Option<Uplink>,
    mut stop: tokio::sync::watch::Receiver<bool>,
    live: Option<tokio::sync::oneshot::Sender<()>>,
    roster: Option<tokio::sync::watch::Sender<Roster>>,
    video: Video,
) -> (Report, Result<(), Failure>) {
    log::info!(
        "signaling: opening {} for attendee {}",
        super::host_of(&join.signaling_url),
        join.attendee_id
    );
    let socket = match tokio::time::timeout(
        OPEN_TIMEOUT,
        Socket::open(&join.signaling_url, &join.join_token),
    )
    .await
    {
        Ok(Ok(socket)) => socket,
        Ok(Err(error)) => {
            return (
                Report::default(),
                Err(failure(Stage::Signaling, error.to_string())),
            );
        }
        Err(_) => {
            return (
                Report::default(),
                Err(failure(Stage::Signaling, "no answer in time")),
            );
        }
    };
    log::info!("signaling: open");
    let started = Instant::now();
    let (frames, mut muted_rx) = match uplink {
        Some(uplink) => (Some(uplink.frames), Some(uplink.muted)),
        None => (None, None),
    };
    let muted = muted_rx.as_mut().is_none_or(|m| *m.borrow_and_update());
    #[cfg(feature = "huddle-camera")]
    let Video {
        options,
        viewer,
        camera,
    } = video;
    #[cfg(not(feature = "huddle-camera"))]
    let Video { options, viewer } = video;
    #[cfg(feature = "huddle-camera")]
    let camera = camera.map(CameraSide::new);
    #[cfg(not(feature = "huddle-camera"))]
    let camera: Option<CameraSide> = None;
    let wish = viewer.as_ref().map(|v| v.wish.clone());
    let watch = (options.is_some() || viewer.is_some() || camera.is_some())
        .then(|| Watch::new(options, viewer, &join.attendee_id));
    let mut session = Session {
        join,
        started,
        socket,
        handshake: Handshake::new(rand::random()),
        turn: None,
        servers: VecDeque::new(),
        relay: None,
        relay_deadline: None,
        rtc: None,
        rtc_timeout: None,
        pending: None,
        mids: Mids::default(),
        audio: None,
        offer_wanted: false,
        index_deadline: None,
        connect_deadline: None,
        leave_deadline: None,
        last_inbound: started,
        next_ping: None,
        ping_id: 0,
        next_stats: started + STATS_EVERY,
        next_audio: None,
        outbound: Outbound::default(),
        frames,
        muted_rx,
        muted,
        flowing: false,
        feed,
        live,
        roster,
        voices: Voices::default(),
        count: None,
        report: Report::default(),
        video: watch,
        wish,
        camera,
        over: None,
    };
    let steps = session.handshake.start(now_ms());
    session.carry(steps).await;
    session.camera_start();
    let mut stopping = *stop.borrow();
    if stopping {
        session.leave().await;
    }
    while session.over.is_none() {
        session.drive_rtc();
        session.flush_relay().await;
        if session.over.is_some() {
            break;
        }
        let deadline = session.deadline();
        tokio::select! {
            incoming = session.socket.next() => match incoming {
                Incoming::Frame(frame) => session.on_frame(&frame).await,
                Incoming::Undecodable { bytes, error } => {
                    session.last_inbound = Instant::now();
                    log::info!("signaling: skipped a {bytes}-byte message: {error}");
                }
                Incoming::Closed { code, reason } => {
                    let steps = session.handshake.closed(code, &reason);
                    session.carry(steps).await;
                }
            },
            received = relay_recv(&mut session.relay) => match received {
                Ok(messages) => {
                    let now = Instant::now();
                    if let Some(relay) = &mut session.relay {
                        for message in messages {
                            relay.client.handle_input(&message, now);
                        }
                    }
                    session.relay_events().await;
                }
                Err(error) => {
                    let lost = session.relay.take();
                    let server = lost.as_ref().map(|r| r.server.to_string()).unwrap_or_default();
                    log::warn!("relay: {server}: {error}");
                    if lost.and_then(|r| r.relayed).is_some() {
                        session.over.get_or_insert(Err(failure(Stage::Media, format!("relay lost: {error}"))));
                    } else {
                        session.next_relay().await;
                    }
                }
            },
            _ = tokio::time::sleep_until(deadline.into()) => {
                session.on_time().await;
                session.relay_events().await;
            }
            frame = next_frame(&mut session.frames) => match frame {
                Some(frame) => session.send_frame(frame),
                // The microphone's side is gone: silence from here.
                None => session.frames = None,
            },
            wish = wish_change(&mut session.wish) => {
                let wish = wish.unwrap_or_else(|_| {
                    session.wish = None;
                    Wish::closed()
                });
                if let Some(watch) = &mut session.video {
                    watch.set_wish(wish);
                }
                session.try_resubscribe().await;
            }
            news = camera_news(&mut session.camera) => session.camera_news(news).await,
            muted = mute_change(&mut session.muted_rx) => match muted {
                Ok(muted) => session.mute_changed(muted).await,
                Err(_) => {
                    session.muted_rx = None;
                    session.mute_changed(true).await;
                }
            },
            changed = stopped(&mut stop, stopping) => {
                if changed.is_err() || *stop.borrow() {
                    stopping = true;
                    session.leave().await;
                }
            }
        }
    }
    // Asked to stop while the session failed: still leave.
    if session.leave_deadline.is_none()
        && session.handshake.phase() != super::signaling::Phase::Over
    {
        let steps = session.handshake.leave(now_ms());
        session.carry(steps).await;
    }
    session.close().await;
    let result = session.over.take().unwrap_or(Ok(()));
    session.report.video = session.video.take().map(Watch::finish);
    (session.report, result)
}

/// The camera's side of a session (`huddle-camera`): its frames, whether
/// it is on, and what the session tells the encoder.
#[cfg(feature = "huddle-camera")]
struct CameraSide {
    frames: Option<tokio::sync::mpsc::Receiver<super::camera_send::VideoFrame>>,
    on_rx: Option<tokio::sync::watch::Receiver<bool>>,
    /// The camera is on now.
    on: bool,
    control: super::camera_send::SendControl,
    refused: tokio::sync::mpsc::Sender<()>,
    /// Frames are held back until a keyframe: at the start, and after
    /// any frame was not sent.
    need_keyframe: bool,
    sent: u64,
    bytes: u64,
    held: u64,
    keyframe_requests: u64,
    /// The bandwidth estimate last heard, in bit/s.
    estimate: Option<u64>,
    /// How SUBSCRIBE describes it.
    descriptor: super::chime::VideoSend,
}

#[cfg(feature = "huddle-camera")]
impl CameraSide {
    fn new(uplink: super::camera_send::CameraUplink) -> Self {
        let mut on_rx = uplink.on;
        let on = *on_rx.borrow_and_update();
        Self {
            frames: Some(uplink.frames),
            on_rx: Some(on_rx),
            on,
            control: uplink.control,
            refused: uplink.refused,
            need_keyframe: true,
            sent: 0,
            bytes: 0,
            held: 0,
            keyframe_requests: 0,
            estimate: None,
            descriptor: uplink.descriptor,
        }
    }
}

/// Without `huddle-camera` a session has no camera.
#[cfg(not(feature = "huddle-camera"))]
type CameraSide = std::convert::Infallible;

/// What the camera's side has to say.
#[cfg(feature = "huddle-camera")]
enum CameraNews {
    /// An encoded frame.
    Frame(super::camera_send::VideoFrame),
    /// The camera went on or off.
    On(bool),
    /// The frames' or the switch's sender is gone: the camera is off for
    /// good.
    Gone,
}

#[cfg(not(feature = "huddle-camera"))]
type CameraNews = std::convert::Infallible;

/// The camera's next news, or never while there is no camera.
#[cfg(feature = "huddle-camera")]
async fn camera_news(camera: &mut Option<CameraSide>) -> CameraNews {
    let Some(side) = camera else {
        return std::future::pending().await;
    };
    let (Some(frames), Some(on_rx)) = (&mut side.frames, &mut side.on_rx) else {
        return std::future::pending().await;
    };
    tokio::select! {
        frame = frames.recv() => frame.map_or(CameraNews::Gone, CameraNews::Frame),
        changed = on_rx.changed() => match changed {
            Ok(()) => CameraNews::On(*on_rx.borrow_and_update()),
            Err(_) => CameraNews::Gone,
        },
    }
}

#[cfg(not(feature = "huddle-camera"))]
async fn camera_news(_camera: &mut Option<CameraSide>) -> CameraNews {
    std::future::pending().await
}

#[cfg(not(feature = "huddle-camera"))]
impl Session<'_> {
    fn camera_start(&mut self) {}

    async fn camera_news(&mut self, news: CameraNews) {
        match news {}
    }

    fn keyframe_request(&mut self, _request: &str0m::media::KeyframeRequest) {}

    fn bitrate_estimate(&mut self, _estimate: &str0m::bwe::BweKind) {}

    fn view_only(&self, _frame: &chime::Frame) -> bool {
        false
    }

    fn refused(&mut self) {}

    fn camera_stats(&self) {}
}

#[cfg(feature = "huddle-camera")]
impl Session<'_> {
    /// A camera on from the start (the probe's test picture) is sent from
    /// the first re-SUBSCRIBE.
    fn camera_start(&mut self) {
        if let (Some(camera), Some(watch)) = (&self.camera, &mut self.video)
            && camera.on
        {
            watch.set_sending(Some(camera.descriptor));
        }
    }

    async fn camera_news(&mut self, news: CameraNews) {
        match news {
            CameraNews::Frame(frame) => self.send_video(frame),
            CameraNews::On(on) => self.camera_switched(on).await,
            CameraNews::Gone => {
                log::info!("media: the camera's side is gone; no video from here on");
                self.camera_switched(false).await;
                self.camera = None;
            }
        }
    }

    /// The camera went on or off: the send line follows from the next
    /// re-SUBSCRIBE.
    async fn camera_switched(&mut self, on: bool) {
        let Some(camera) = &mut self.camera else {
            return;
        };
        if camera.on == on {
            return;
        }
        camera.on = on;
        camera.need_keyframe = true;
        log::info!("media: camera {}", if on { "on" } else { "off" });
        if let Some(watch) = &mut self.video {
            watch.set_sending(on.then_some(camera.descriptor));
        }
        if on && let Some(rtc) = &mut self.rtc {
            let desired = u64::from(camera.descriptor.max_kbps) * 1000 + AUDIO_SHARE_BPS;
            rtc.bwe()
                .set_desired_bitrate(str0m::bwe::Bitrate::bps(desired));
        }
        self.try_resubscribe().await;
    }

    /// Sends one encoded frame on the send line, if it is negotiated to
    /// send; until then, and until a keyframe after any frame held back,
    /// frames are held back and a keyframe asked for.
    fn send_video(&mut self, frame: super::camera_send::VideoFrame) {
        let (Some(camera), Some(watch), Some(rtc)) = (&mut self.camera, &self.video, &mut self.rtc)
        else {
            return;
        };
        let live = camera.on
            && watch.sending()
            && self.report.dtls_up.is_some()
            && self.leave_deadline.is_none();
        if !live || (camera.need_keyframe && !frame.keyframe) {
            camera.held += 1;
            camera.need_keyframe = true;
            if live {
                camera.control.want_keyframe();
            }
            return;
        }
        let Some(writer) = watch.send_line().and_then(|mid| rtc.writer(mid)) else {
            return;
        };
        // Constrained baseline, packetization mode 1, as Chime's
        // receivers take it; any mode-1 H.264 if that is not there.
        let pt = {
            let h264 = |p: &&str0m::format::PayloadParams| {
                p.spec().codec == Codec::H264 && p.spec().format.packetization_mode == Some(1)
            };
            let params: Vec<_> = writer.payload_params().filter(h264).collect();
            params
                .iter()
                .find(|p| p.spec().format.profile_level_id == Some(0x42e01f))
                .or_else(|| params.first())
                .map(|p| p.pt())
        };
        let Some(pt) = pt else {
            log::debug!("media: no H.264 to send the camera with");
            camera.held += 1;
            return;
        };
        let bytes = frame.data.len() as u64;
        let time = MediaTime::new(frame.time, str0m::media::Frequency::NINETY_KHZ);
        match writer.write(pt, frame.at, time, frame.data) {
            Ok(()) => {
                camera.need_keyframe = false;
                camera.sent += 1;
                camera.bytes += bytes;
                if camera.sent == 1 {
                    log::info!(
                        "media: first camera frame sent: pt {pt}, {bytes} bytes, keyframe {}",
                        frame.keyframe
                    );
                }
            }
            Err(error) => {
                log::debug!("media: could not send a camera frame: {error}");
                camera.held += 1;
                camera.need_keyframe = true;
            }
        }
    }

    /// A receiver lost our picture: the encoder makes a keyframe.
    fn keyframe_request(&mut self, request: &str0m::media::KeyframeRequest) {
        let ours = self
            .video
            .as_ref()
            .and_then(Watch::send_line)
            .is_some_and(|mid| mid == request.mid);
        if let Some(camera) = &mut self.camera
            && ours
        {
            camera.keyframe_requests += 1;
            log::debug!("media: {:?} for our camera", request.kind);
            camera.control.want_keyframe();
        }
    }

    /// The bandwidth estimate: what is left after the audio goes to the
    /// camera.
    fn bitrate_estimate(&mut self, estimate: &str0m::bwe::BweKind) {
        let Some(camera) = &mut self.camera else {
            return;
        };
        let bps = match estimate {
            str0m::bwe::BweKind::Twcc { estimate, .. } => estimate.as_u64(),
            str0m::bwe::BweKind::Remb { estimate, .. } => estimate.as_u64(),
            _ => return,
        };
        if camera
            .estimate
            .is_none_or(|was| was.abs_diff(bps) > was / 5)
        {
            log::info!("media: send bandwidth estimate {} kbit/s", bps / 1000);
        }
        camera.estimate = Some(bps);
        let left = bps.saturating_sub(AUDIO_SHARE_BPS);
        camera
            .control
            .set_bitrate(u32::try_from(left).unwrap_or(u32::MAX));
    }

    /// Whether `frame` is Chime refusing our camera: the answer to a
    /// SUBSCRIBE that asked to send it, with error 206
    /// ("VideoCallSwitchToViewOnly") or receive-only service.
    fn view_only(&self, frame: &chime::Frame) -> bool {
        self.video.as_ref().is_some_and(Watch::offering_to_send) && chime::view_only_ack(frame)
    }

    /// Chime takes no video from us: the camera goes off, and the one who
    /// turned it on hears why.
    fn refused(&mut self) {
        self.report.refused_status = Some(206);
        if let Some(watch) = &mut self.video {
            watch.refuse_sending();
        }
        if let Some(camera) = &mut self.camera {
            camera.on = false;
            let _ = camera.refused.try_send(());
        }
    }

    fn camera_stats(&self) {
        let Some(camera) = &self.camera else {
            return;
        };
        if camera.sent == 0 && !camera.on {
            return;
        }
        log::info!(
            "media: {} s camera: {}, {} frames ({} bytes) sent, {} held back, {} keyframe \
             requests; estimate {}",
            self.since().as_secs(),
            if camera.on { "on" } else { "off" },
            camera.sent,
            camera.bytes,
            camera.held,
            camera.keyframe_requests,
            camera
                .estimate
                .map_or_else(|| "none".to_owned(), |b| format!("{} kbit/s", b / 1000))
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use str0m::change::SdpOffer;

    /// A second `str0m` plays Chime: it takes our offer as Chime gets it
    /// and answers with a browser's media ids, which come back to ours.
    #[test]
    fn our_offer_and_its_answer_go_both_ways() {
        let relayed: SocketAddr = "203.0.113.5:50000".parse().expect("an address");
        let local: SocketAddr = "192.168.1.2:40000".parse().expect("an address");
        let mut ours = new_peer(relayed, local, false).expect("a peer");
        let offer = make_offer(&mut ours).expect("an offer");
        assert!(offer.sdp.contains("o=mozilla-chrome "));
        assert!(offer.sdp.contains("a=mid:0\r\n") && offer.sdp.contains("a=mid:1\r\n"));
        assert!(offer.sdp.contains("typ relay"));
        assert!(offer.sdp.contains("opus/48000/2"));

        let mut chime = RtcConfig::new()
            .set_crypto_provider(Arc::new(dtls::provider()))
            .build(Instant::now());
        let server: SocketAddr = "192.0.2.10:3478".parse().expect("an address");
        chime.add_local_candidate(Candidate::host(server, "udp").expect("a candidate"));
        let answer = chime
            .sdp_api()
            .accept_offer(SdpOffer::from_sdp_string(&offer.sdp).expect("parses"))
            .expect("accepted")
            .to_sdp_string();
        assert!(answer.contains("a=mid:0\r\n"));

        let back = offer.mids.answer_from_chime(&answer);
        assert_eq!(sdp::candidate_addresses(&back), vec![server]);
        ours.sdp_api()
            .accept_answer(
                offer.pending,
                SdpAnswer::from_sdp_string(&back).expect("parses"),
            )
            .expect("our peer takes the answer");
        assert!(ours.media(offer.audio).is_some());
    }

    /// `--video-h264-only` leaves VP8 out of the video m-line and keeps
    /// all of `str0m`'s H.264 profiles; Opus is untouched.
    #[test]
    fn an_h264_only_offer_has_no_vp8() {
        let relayed: SocketAddr = "203.0.113.5:50000".parse().expect("an address");
        let local: SocketAddr = "192.168.1.2:40000".parse().expect("an address");
        let both = make_offer(&mut new_peer(relayed, local, false).expect("a peer"))
            .expect("an offer")
            .sdp;
        assert!(both.contains("VP8/90000") && both.contains("H264/90000"));
        let h264 = make_offer(&mut new_peer(relayed, local, true).expect("a peer"))
            .expect("an offer")
            .sdp;
        assert!(!h264.contains("VP8"), "{h264}");
        assert!(h264.contains("opus/48000/2"));
        for profile in ["42001f", "42e01f", "4d001f", "64001f"] {
            assert!(
                h264.contains(&format!("profile-level-id={profile}")),
                "{profile}: {h264}"
            );
        }
    }

    /// Our camera through renegotiation, with a pretend Chime: on, the
    /// first video m-line (slot 0, our send line) turns `sendrecv`, the
    /// SUBSCRIBE asks for both ways and describes the camera, and slot 0
    /// stays 0 beside a stream received on the next line; off, the line
    /// goes back to `inactive`; refused (view only), it stops sending at
    /// once and the next offer turns it off.
    #[test]
    fn the_camera_turns_the_send_line_on_and_off() {
        let relayed: SocketAddr = "203.0.113.5:50000".parse().expect("an address");
        let local: SocketAddr = "192.168.1.2:40000".parse().expect("an address");
        let mut ours = new_peer_with(relayed, local, true, true).expect("a peer");
        let offer = make_offer(&mut ours).expect("an offer");
        let mut chime_peer = RtcConfig::new()
            .set_crypto_provider(Arc::new(dtls::provider()))
            .build(Instant::now());
        let server: SocketAddr = "192.0.2.10:3478".parse().expect("an address");
        chime_peer.add_local_candidate(Candidate::host(server, "udp").expect("a candidate"));
        let answer = chime_peer
            .sdp_api()
            .accept_offer(SdpOffer::from_sdp_string(&offer.sdp).expect("parses"))
            .expect("accepted")
            .to_sdp_string();
        ours.sdp_api()
            .accept_answer(
                offer.pending,
                SdpAnswer::from_sdp_string(&offer.mids.answer_from_chime(&answer)).expect("parses"),
            )
            .expect("taken");

        // One stream to receive besides: a camera in INDEX.
        let mut watch = Watch::new(
            Some(video::Options {
                streams: 1,
                h264_only: true,
                dump: None,
            }),
            None,
            "A1",
        );
        watch.offered(offer.video);
        let mut index = chime::frame(FrameType::Index, 1);
        index.index = Some(chime::proto::SdkIndexFrame {
            sources: vec![chime::proto::SdkStreamDescriptor {
                stream_id: Some(7),
                group_id: Some(3),
                attendee_id: Some("B2".into()),
                media_type: Some(chime::proto::SdkStreamMediaType::Video as i32),
                max_bitrate_kbps: Some(300),
                ..Default::default()
            }],
            ..Default::default()
        });
        let mut now = Instant::now();
        let dtls_up = now - Duration::from_secs(10);
        watch.frame(&index, now);
        let camera = chime::VideoSend {
            width: 640,
            height: 480,
            fps: 15,
            max_kbps: 1200,
        };

        // Renegotiates as the session does, Chime answering with str0m.
        fn exchange(
            watch: &mut Watch,
            ours: &mut Rtc,
            chime_peer: &mut Rtc,
            made: watch::Reoffer,
            now: Instant,
        ) -> (Option<chime::VideoSend>, Vec<u32>, Vec<String>) {
            let mids = Mids::of_offer(&made.offer);
            let directions: Vec<String> = sdp::media_lines(&made.offer)
                .iter()
                .filter(|l| l.kind == "video")
                .map(|l| l.direction.clone())
                .collect();
            let answer = chime_peer
                .sdp_api()
                .accept_offer(
                    SdpOffer::from_sdp_string(&mids.offer_for_chime(&made.offer)).expect("parses"),
                )
                .expect("accepted")
                .to_sdp_string();
            ours.sdp_api()
                .accept_answer(
                    made.pending,
                    SdpAnswer::from_sdp_string(&mids.answer_from_chime(&answer)).expect("parses"),
                )
                .expect("taken");
            watch.answered(&answer, &mids, now);
            (made.video, made.receive_stream_ids, directions)
        }

        watch.set_sending(Some(camera));
        assert!(!watch.sending(), "not before the answer");
        // The stream to receive waits until the choice has stood still.
        let made = match watch.reoffer(&mut ours, now, dtls_up, 0) {
            Some(made) => made,
            None => {
                now += Duration::from_secs(1);
                watch
                    .reoffer(&mut ours, now, dtls_up, 0)
                    .expect("a new offer")
            }
        };
        assert!(watch.offering_to_send());
        let (video, ids, directions) = exchange(&mut watch, &mut ours, &mut chime_peer, made, now);
        assert_eq!(video, Some(camera));
        assert_eq!(ids, [0, 7], "slot 0 is ours, 0; the stream on the next");
        assert_eq!(directions, ["sendrecv", "recvonly"]);
        assert!(watch.sending());
        assert!(!watch.offering_to_send());
        let frame = chime::subscribe(
            &chime::Subscribe {
                sdp_offer: String::new(),
                audio_host: String::new(),
                attendee_id: "A1".into(),
                muted: true,
                receive_stream_ids: ids,
                video,
            },
            1,
        );
        let sub = frame.sub.expect("a subscribe");
        assert_eq!(
            sub.duplex,
            Some(chime::proto::SdkStreamServiceType::Duplex as i32)
        );
        assert_eq!(sub.send_streams.len(), 2);
        // Nothing changed: no new offer.
        now += Duration::from_secs(4);
        assert!(watch.reoffer(&mut ours, now, dtls_up, 0).is_none());

        // Off.
        watch.set_sending(None);
        let made = watch
            .reoffer(&mut ours, now, dtls_up, 0)
            .expect("a new offer");
        let (video, ids, directions) = exchange(&mut watch, &mut ours, &mut chime_peer, made, now);
        assert_eq!(video, None);
        assert_eq!(ids, [0, 7]);
        assert_eq!(directions, ["inactive", "recvonly"]);
        assert!(!watch.sending());

        // On again, and Chime says view only.
        now += Duration::from_secs(4);
        watch.set_sending(Some(camera));
        let made = watch
            .reoffer(&mut ours, now, dtls_up, 0)
            .expect("a new offer");
        assert!(watch.offering_to_send());
        exchange(&mut watch, &mut ours, &mut chime_peer, made, now);
        watch.refuse_sending();
        assert!(!watch.sending(), "nothing more is sent");
        now += Duration::from_secs(4);
        let made = watch
            .reoffer(&mut ours, now, dtls_up, 0)
            .expect("a new offer");
        let (video, _, directions) = exchange(&mut watch, &mut ours, &mut chime_peer, made, now);
        assert_eq!(video, None, "the next SUBSCRIBE sends no camera");
        assert_eq!(directions[0], "inactive");
    }

    /// What one side of the pretend call does with `str0m`'s output:
    /// packets out, events kept, and when to come back.
    fn poll(rtc: &mut Rtc, events: &mut Vec<RtcEvent>) -> (Vec<str0m::net::Transmit>, Instant) {
        let mut out = Vec::new();
        loop {
            match rtc.poll_output().expect("output") {
                Output::Timeout(at) => return (out, at),
                Output::Transmit(t) => out.push(t),
                Output::Event(e) => events.push(e),
            }
        }
    }

    /// The whole media path offline: our peer behind our TURN client, a
    /// pretend TURN server, and a second `str0m` as Chime's media server
    /// sending Opus. ICE, DTLS and SRTP run through the relay, and the
    /// audio comes out the other end as sent.
    #[test]
    fn audio_flows_through_the_relay() {
        use turn::{Class, Message, Method, attr, read_xor_address, xor_address};

        let relayed: SocketAddr = "203.0.113.5:50000".parse().expect("an address");
        let local: SocketAddr = "192.168.1.2:40000".parse().expect("an address");
        let server: SocketAddr = "192.0.2.10:3478".parse().expect("an address");
        let mut now = Instant::now();

        // The relay: a pretend server that grants everything, without
        // credentials.
        let mut client = turn::Client::new(Transport::Udp, "u", "p");
        let answer = |request: &[u8], extra: Vec<(u16, Vec<u8>)>| {
            let request = Message::decode(request).expect("STUN");
            Message {
                method: request.method,
                class: Class::Success,
                transaction: request.transaction,
                attributes: extra,
            }
            .encode(None)
        };
        client.allocate(now);
        let allocate = client.poll_transmit().expect("Allocate");
        let id = Message::decode(&allocate).expect("STUN").transaction;
        client.handle_input(
            &answer(
                &allocate,
                vec![(attr::XOR_RELAYED_ADDRESS, xor_address(relayed, &id))],
            ),
            now,
        );
        assert!(matches!(
            client.poll_event(),
            Some(turn::Event::Allocated { .. })
        ));

        // Offer, answer, as the session does.
        let mut ours = new_peer(relayed, local, false).expect("a peer");
        let offer = make_offer(&mut ours).expect("an offer");
        let mut chime = RtcConfig::new()
            .set_crypto_provider(Arc::new(dtls::provider()))
            .build(now);
        chime.add_local_candidate(Candidate::host(server, "udp").expect("a candidate"));
        let answer_sdp = chime
            .sdp_api()
            .accept_offer(SdpOffer::from_sdp_string(&offer.sdp).expect("parses"))
            .expect("accepted")
            .to_sdp_string();
        let back = offer.mids.answer_from_chime(&answer_sdp);
        ours.sdp_api()
            .accept_answer(
                offer.pending,
                SdpAnswer::from_sdp_string(&back).expect("parses"),
            )
            .expect("taken");
        client.permit(&[server.ip()], now);

        let mut ours_events = Vec::new();
        let mut chime_events = Vec::new();
        let mut chime_audio: Option<(Mid, str0m::media::Pt)> = None;
        let mut sent: Vec<Vec<u8>> = Vec::new();
        let mut next_send = now;
        let mut received: Vec<Vec<u8>> = Vec::new();
        let start = now;
        while now - start < Duration::from_secs(10) && received.len() < 20 {
            // Ours: out through the TURN client.
            let (out, ours_at) = poll(&mut ours, &mut ours_events);
            for t in out {
                assert_eq!(t.source, relayed, "everything leaves from the relay");
                client.send_to(t.destination, &t.contents, now);
            }
            // The pretend TURN server: answers requests, relays Sends.
            while let Some(bytes) = client.poll_transmit() {
                let message = Message::decode(&bytes).expect("STUN");
                match (message.method(), message.class) {
                    (Some(Method::Send), Class::Indication) => {
                        let peer = message
                            .get(attr::XOR_PEER_ADDRESS)
                            .and_then(|v| read_xor_address(v, &message.transaction))
                            .expect("a peer");
                        assert_eq!(peer, server);
                        let data = message.get(attr::DATA).expect("data");
                        let receive =
                            Receive::new(Protocol::Udp, relayed, server, data).expect("a datagram");
                        chime
                            .handle_input(Input::Receive(now, receive))
                            .expect("taken");
                    }
                    (_, Class::Request) => client.handle_input(&answer(&bytes, vec![]), now),
                    other => panic!("{other:?}"),
                }
            }
            while client.poll_event().is_some() {}
            // Chime: sends Opus once connected; its packets come back as
            // Data indications.
            let (out, chime_at) = poll(&mut chime, &mut chime_events);
            for t in out {
                assert_eq!(t.destination, relayed);
                let data = Message::new(Method::Data, Class::Indication, [7; 12])
                    .with(attr::XOR_PEER_ADDRESS, xor_address(server, &[7; 12]))
                    .with(attr::DATA, t.contents.to_vec())
                    .encode(None);
                client.handle_input(&data, now);
            }
            while let Some(event) = client.poll_event() {
                if let turn::Event::Data { peer, data } = event {
                    let receive =
                        Receive::new(Protocol::Udp, peer, relayed, &data).expect("a datagram");
                    ours.handle_input(Input::Receive(now, receive))
                        .expect("taken");
                }
            }
            for event in chime_events.drain(..) {
                if let RtcEvent::MediaAdded(added) = event
                    && added.kind == MediaKind::Audio
                {
                    let pt = chime
                        .writer(added.mid)
                        .and_then(|w| {
                            w.payload_params()
                                .find(|p| p.spec().codec == Codec::Opus)
                                .map(|p| p.pt())
                        })
                        .expect("Opus");
                    chime_audio = Some((added.mid, pt));
                }
            }
            let connected = ours_events.iter().any(|e| matches!(e, RtcEvent::Connected));
            if connected
                && now >= next_send
                && let Some((mid, pt)) = chime_audio
                && let Some(writer) = chime.writer(mid)
            {
                let n = u8::try_from(sent.len()).unwrap_or(u8::MAX);
                let frame = vec![0xF8, 0xFF, 0xFE, n];
                let time = MediaTime::new(
                    u64::from(super::super::jitter::FRAME) * sent.len() as u64,
                    str0m::media::Frequency::FORTY_EIGHT_KHZ,
                );
                writer.write(pt, now, time, frame.clone()).expect("written");
                sent.push(frame);
                next_send = now + AUDIO_TICK;
            }
            for event in ours_events.extract_if(.., |e| matches!(e, RtcEvent::MediaData(_))) {
                if let RtcEvent::MediaData(data) = event {
                    assert_eq!(Some(data.mid), Some(offer.audio));
                    received.push(data.data.to_vec());
                }
            }
            let next = ours_at
                .min(chime_at)
                .min(next_send.max(now + Duration::from_millis(1)));
            now = next.max(now);
            ours.handle_input(Input::Timeout(now)).expect("timeout");
            chime.handle_input(Input::Timeout(now)).expect("timeout");
        }
        assert!(
            ours_events.iter().any(|e| matches!(e, RtcEvent::Connected)) || !received.is_empty(),
            "never connected"
        );
        assert!(received.len() >= 10, "only {} frames came", received.len());
        assert_eq!(received[..], sent[..received.len()]);
    }

    /// A pretend Chime on loopback: a signaling WebSocket, a TURN server
    /// on UDP and a `str0m` media server sending Opus, all driven by
    /// [`listen`] itself. We talk too, the probe's tone through the real
    /// encoder: the pretend media server decodes what we send, and the
    /// signaling server hears the mute that ends it. Video: INDEX
    /// announces a screen share, two cameras (one in two layers) and our
    /// own camera, never taken; the session renegotiates a `recvonly`
    /// m-line for each stream wanted and re-SUBSCRIBEs for them, and the
    /// media server sends the H.264 fixture on each, which is counted and
    /// read while the audio goes on. With `huddle-video` the call window
    /// asks for the share and the cameras (the smaller layer, for small
    /// tiles), both are decoded, and closing it frees every m-line.
    /// With `huddle-camera` we send the test picture as our camera too:
    /// the same re-SUBSCRIBE asks for both ways, and the media server
    /// receives our H.264 on the first video m-line, asks once for a
    /// keyframe and decodes every frame. Ignored by default: it opens
    /// local sockets and runs for seconds.
    /// `cargo test --all-features -- --ignored loopback`.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[ignore = "opens loopback sockets and takes a few seconds"]
    async fn loopback_session_joins_listens_and_leaves() {
        use futures_util::{SinkExt as _, StreamExt as _};
        use std::sync::Mutex;
        use str0m::media::KeyframeRequestKind;
        use tokio_tungstenite::tungstenite::Message as Ws;
        use turn::{Class, Message, Method, attr, read_xor_address, xor_address};

        // What the pretend Chime hears from us: Opus with its RTP time and
        // audio level, and the AUDIO_CONTROL frames.
        type Heard = Vec<(u64, Option<i8>, Vec<u8>)>;
        let heard: Arc<Mutex<Heard>> = Arc::default();
        let controls: Arc<Mutex<Vec<bool>>> = Arc::default();
        let (media_heard, signaling_controls) = (heard.clone(), controls.clone());
        // Each SUBSCRIBE's receive_stream_ids, the keyframe requests the
        // media server got, and the video frames it sent.
        let subscribed: Arc<Mutex<Vec<Vec<u32>>>> = Arc::default();
        let signaling_subscribed = subscribed.clone();
        let keyframe_requests: Arc<Mutex<u32>> = Arc::default();
        let media_requests = keyframe_requests.clone();
        let video_sent: Arc<Mutex<u32>> = Arc::default();
        let media_video_sent = video_sent.clone();
        // Whether each SUBSCRIBE sent our camera, and the camera frames
        // the media server received (keyframe or not, the access unit).
        let subscribed_camera: Arc<Mutex<Vec<bool>>> = Arc::default();
        let signaling_camera = subscribed_camera.clone();
        type CameraHeard = Vec<(bool, Vec<u8>)>;
        let camera_heard: Arc<Mutex<CameraHeard>> = Arc::default();
        let media_camera_heard = camera_heard.clone();
        // The fixture, one access unit a frame: a new one at each SPS, or
        // at a slice when the unit already has one.
        let stream = include_bytes!("fixtures/test-pattern-320x180.h264");
        let mut units: Vec<Vec<u8>> = Vec::new();
        let mut has_slice = false;
        for nal in super::super::bitstream::nal_units(stream) {
            let kind = nal[0] & 0x1f;
            let slice = matches!(kind, 1 | 5);
            if units.is_empty() || kind == 7 || (slice && has_slice) {
                units.push(Vec::new());
                has_slice = false;
            }
            has_slice |= slice;
            if let Some(unit) = units.last_mut() {
                unit.extend_from_slice(&[0, 0, 0, 1]);
                unit.extend_from_slice(nal);
            }
        }
        assert_eq!(units.len(), 6, "six frames in the fixture");

        let media_server: SocketAddr = "127.0.0.2:3478".parse().expect("an address");
        let relayed: SocketAddr = "127.0.0.3:50000".parse().expect("an address");

        // The TURN server and, behind it, the media server.
        let turn_socket = tokio::net::UdpSocket::bind("127.0.0.1:0")
            .await
            .expect("bound");
        let turn_port = turn_socket.local_addr().expect("an address").port();
        let (offers, mut offer_inbox) =
            tokio::sync::mpsc::channel::<(String, tokio::sync::oneshot::Sender<String>)>(1);
        tokio::spawn(async move {
            let mut chime: Option<Rtc> = None;
            let mut client: Option<SocketAddr> = None;
            let mut audio: Option<(Mid, str0m::media::Pt)> = None;
            let mut video: Vec<(Mid, str0m::media::Pt)> = Vec::new();
            let mut next_video = Instant::now();
            let mut video_frames = 0u64;
            let mut connected = false;
            let mut sent = 0u64;
            let mut buf = vec![0u8; 2048];
            loop {
                let now = Instant::now();
                // Drive the media server.
                let mut wake = now + Duration::from_millis(20);
                if let Some(rtc) = &mut chime {
                    let _ = rtc.handle_input(Input::Timeout(now));
                    loop {
                        match rtc.poll_output() {
                            Ok(Output::Timeout(at)) => {
                                wake = wake.min(at);
                                break;
                            }
                            Ok(Output::Transmit(t)) => {
                                if let Some(client) = client {
                                    let data =
                                        Message::new(Method::Data, Class::Indication, [5; 12])
                                            .with(
                                                attr::XOR_PEER_ADDRESS,
                                                xor_address(media_server, &[5; 12]),
                                            )
                                            .with(attr::DATA, t.contents.to_vec())
                                            .encode(None);
                                    let _ = turn_socket.send_to(&data, client).await;
                                }
                            }
                            Ok(Output::Event(RtcEvent::Connected)) => connected = true,
                            // Our camera, H.264 on our send line: kept,
                            // and after five frames a keyframe is asked for.
                            Ok(Output::Event(RtcEvent::MediaData(data)))
                                if data.params.spec().codec == Codec::H264 =>
                            {
                                let count = media_camera_heard.lock().map_or(0, |mut heard| {
                                    heard.push((data.is_keyframe(), data.data.to_vec()));
                                    heard.len()
                                });
                                if count == 5
                                    && let Some(mut writer) = rtc.writer(data.mid)
                                {
                                    let _ = writer.request_keyframe(None, KeyframeRequestKind::Pli);
                                }
                            }
                            Ok(Output::Event(RtcEvent::MediaData(data))) => {
                                if let Ok(mut heard) = media_heard.lock() {
                                    heard.push((
                                        data.time.numer(),
                                        data.ext_vals.audio_level,
                                        data.data.to_vec(),
                                    ));
                                }
                            }
                            Ok(Output::Event(RtcEvent::MediaAdded(added)))
                                if added.kind == MediaKind::Audio =>
                            {
                                let pt = rtc.writer(added.mid).and_then(|w| {
                                    w.payload_params()
                                        .find(|p| p.spec().codec == Codec::Opus)
                                        .map(|p| p.pt())
                                });
                                audio = pt.map(|pt| (added.mid, pt));
                            }
                            // Our recvonly m-line: the media server sends
                            // on it, H.264 with packetization mode 1.
                            Ok(Output::Event(RtcEvent::MediaAdded(added)))
                                if added.kind == MediaKind::Video
                                    && added.direction == Direction::SendOnly =>
                            {
                                let pt = rtc.writer(added.mid).and_then(|w| {
                                    w.payload_params()
                                        .find(|p| {
                                            p.spec().codec == Codec::H264
                                                && p.spec().format.packetization_mode == Some(1)
                                        })
                                        .map(|p| p.pt())
                                });
                                video.extend(pt.map(|pt| (added.mid, pt)));
                            }
                            Ok(Output::Event(RtcEvent::KeyframeRequest(_))) => {
                                if let Ok(mut n) = media_requests.lock() {
                                    *n += 1;
                                }
                            }
                            Ok(Output::Event(_)) => {}
                            Err(_) => break,
                        }
                    }
                    if connected
                        && let Some((mid, pt)) = audio
                        && let Some(writer) = rtc.writer(mid)
                    {
                        let time =
                            MediaTime::new(sent * 960, str0m::media::Frequency::FORTY_EIGHT_KHZ);
                        let _ = writer.write(pt, now, time, SILENT_OPUS.to_vec());
                        sent += 1;
                    }
                    // The fixture over and over at 15 frames a second, on
                    // every video m-line.
                    if connected && now >= next_video && !video.is_empty() {
                        let n = usize::try_from(video_frames).unwrap_or(0) % units.len();
                        let time = MediaTime::new(
                            video_frames * 6000,
                            str0m::media::Frequency::NINETY_KHZ,
                        );
                        for &(mid, pt) in &video {
                            if let Some(writer) = rtc.writer(mid)
                                && writer.write(pt, now, time, units[n].clone()).is_ok()
                                && let Ok(mut sent) = media_video_sent.lock()
                            {
                                *sent += 1;
                            }
                        }
                        video_frames += 1;
                        next_video = now + Duration::from_millis(66);
                    }
                }
                tokio::select! {
                    offer = offer_inbox.recv() => {
                        let Some((offer, reply)) = offer else { return };
                        let offer = str0m::change::SdpOffer::from_sdp_string(&offer).expect("parses");
                        // A re-SUBSCRIBE renegotiates the same connection.
                        let rtc = chime.get_or_insert_with(|| {
                            let mut rtc = RtcConfig::new()
                                .set_crypto_provider(Arc::new(dtls::provider()))
                                .build(Instant::now());
                            rtc.add_local_candidate(Candidate::host(media_server, "udp").expect("a candidate"));
                            rtc
                        });
                        let answer = rtc.sdp_api().accept_offer(offer).expect("accepted");
                        let _ = reply.send(answer.to_sdp_string());
                    }
                    got = turn_socket.recv_from(&mut buf) => {
                        let Ok((n, from)) = got else { return };
                        client = Some(from);
                        let message = Message::decode(&buf[..n]).expect("STUN");
                        match (message.method(), message.class) {
                            (Some(Method::Send), Class::Indication) => {
                                let peer = message.get(attr::XOR_PEER_ADDRESS)
                                    .and_then(|v| read_xor_address(v, &message.transaction));
                                if let (Some(rtc), Some(data), Some(peer)) = (&mut chime, message.get(attr::DATA), peer)
                                    && peer == media_server
                                    && let Ok(receive) = Receive::new(Protocol::Udp, relayed, media_server, data)
                                {
                                    let _ = rtc.handle_input(Input::Receive(Instant::now(), receive));
                                }
                            }
                            (_, Class::Request) => {
                                let extra = if message.method() == Some(Method::Allocate) {
                                    vec![(attr::XOR_RELAYED_ADDRESS, xor_address(relayed, &message.transaction))]
                                } else {
                                    vec![]
                                };
                                let answer = Message {
                                    method: message.method,
                                    class: Class::Success,
                                    transaction: message.transaction,
                                    attributes: extra,
                                }
                                .encode(None);
                                let _ = turn_socket.send_to(&answer, from).await;
                            }
                            _ => {}
                        }
                    }
                    () = tokio::time::sleep_until(wake.into()) => {}
                }
            }
        });

        // The signaling server.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bound");
        let signaling_port = listener.local_addr().expect("an address").port();
        tokio::spawn(async move {
            let Ok((tcp, _)) = listener.accept().await else {
                return;
            };
            #[allow(clippy::result_large_err, reason = "tungstenite's callback type")]
            let check = |request: &tokio_tungstenite::tungstenite::handshake::server::Request,
                         mut response: tokio_tungstenite::tungstenite::handshake::server::Response| {
                let protocols = request
                    .headers()
                    .get("Sec-WebSocket-Protocol")
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or_default()
                    .to_owned();
                assert_eq!(protocols, "_aws_wt_session, the-token");
                assert!(request.uri().query().unwrap_or_default().contains("X-Chime-Control-Protocol-Version=3"));
                response.headers_mut().insert(
                    "Sec-WebSocket-Protocol",
                    tokio_tungstenite::tungstenite::http::HeaderValue::from_static("_aws_wt_session"),
                );
                Ok(response)
            };
            let mut ws = tokio_tungstenite::accept_hdr_async(tcp, check)
                .await
                .expect("a socket");
            let reply = |frame: chime::Frame| Ws::Binary(chime::encode(&frame).into());
            while let Some(Ok(message)) = ws.next().await {
                let Ws::Binary(bytes) = message else { continue };
                let frame = chime::decode(&bytes).expect("a frame");
                match FrameType::try_from(frame.r#type) {
                    Ok(FrameType::Join) => {
                        let mut ack = chime::frame(FrameType::JoinAck, 1);
                        ack.joinack = Some(chime::proto::SdkJoinAckFrame {
                            turn_credentials: Some(chime::proto::SdkTurnCredentials {
                                username: Some("u".into()),
                                password: Some("p".into()),
                                ttl: Some(300),
                                uris: vec![format!("turn:127.0.0.1:{turn_port}?transport=udp")],
                            }),
                            ..Default::default()
                        });
                        let _ = ws.send(reply(ack)).await;
                        // A screen share, two cameras (C3's in two
                        // layers), and our own camera, which is never
                        // received.
                        let source = |stream: u32, group: u32, attendee: &str| {
                            chime::proto::SdkStreamDescriptor {
                                stream_id: Some(stream),
                                group_id: Some(group),
                                attendee_id: Some(attendee.into()),
                                external_user_id: Some(format!("T1-R1-U{group}")),
                                media_type: Some(chime::proto::SdkStreamMediaType::Video as i32),
                                width: Some(320),
                                height: Some(180),
                                framerate: Some(15),
                                max_bitrate_kbps: Some(300),
                                ..Default::default()
                            }
                        };
                        let small = chime::proto::SdkStreamDescriptor {
                            width: Some(160),
                            height: Some(90),
                            max_bitrate_kbps: Some(100),
                            ..source(12, 5, "C3")
                        };
                        let mut index = chime::frame(FrameType::Index, 2);
                        index.index = Some(chime::proto::SdkIndexFrame {
                            sources: vec![
                                source(7, 3, "B2#content"),
                                source(9, 1, "A1"),
                                source(11, 5, "C3"),
                                small,
                                source(13, 6, "D4"),
                            ],
                            num_participants: Some(2),
                            supported_receive_codec_intersection: vec![3],
                            ..Default::default()
                        });
                        let _ = ws.send(reply(index)).await;
                    }
                    Ok(FrameType::Subscribe) => {
                        let sub = frame.sub.expect("a subscribe");
                        if let Ok(mut subscribed) = signaling_subscribed.lock() {
                            subscribed.push(sub.receive_stream_ids.clone());
                        }
                        if let Ok(mut camera) = signaling_camera.lock() {
                            let both = sub.duplex
                                == Some(chime::proto::SdkStreamServiceType::Duplex as i32);
                            let described = sub.send_streams.iter().any(|s| {
                                s.media_type == Some(chime::proto::SdkStreamMediaType::Video as i32)
                            });
                            assert_eq!(
                                both, described,
                                "DUPLEX exactly when the camera is described"
                            );
                            camera.push(both);
                        }
                        let offer = sub.sdp_offer.expect("an offer");
                        let (answer_to, answer) = tokio::sync::oneshot::channel();
                        let _ = offers.send((offer, answer_to)).await;
                        let answer = answer.await.expect("an answer");
                        // The stream on each video m-line, by its SSRC.
                        let tracks = sdp::media_lines(&answer)
                            .into_iter()
                            .filter(|m| m.kind == "video")
                            .zip(&sub.receive_stream_ids)
                            .filter(|&(_, &stream)| stream != 0)
                            .filter_map(|(line, &stream)| {
                                line.ssrcs
                                    .first()
                                    .map(|&ssrc| chime::proto::SdkTrackMapping {
                                        stream_id: Some(stream),
                                        ssrc: Some(ssrc),
                                        track_label: Some("video".into()),
                                    })
                            })
                            .collect();
                        let mut ack = chime::frame(FrameType::SubscribeAck, 3);
                        ack.suback = Some(chime::proto::SdkSubscribeAckFrame {
                            sdp_answer: Some(answer),
                            tracks,
                            ..Default::default()
                        });
                        let _ = ws.send(reply(ack)).await;
                    }
                    Ok(FrameType::AudioControl) => {
                        if let (Ok(mut controls), Some(control)) =
                            (signaling_controls.lock(), frame.audio_control)
                        {
                            controls.push(control.muted.unwrap_or_default());
                        }
                    }
                    Ok(FrameType::Leave) => {
                        let _ = ws.send(reply(chime::frame(FrameType::LeaveAck, 4))).await;
                    }
                    _ => {}
                }
            }
        });

        let join = ChimeJoin {
            call_id: Some("R1".into()),
            meeting_id: Some("M1".into()),
            media_region: Some("us-east-1".into()),
            signaling_url: format!("ws://127.0.0.1:{signaling_port}/control/M1"),
            turn_control_url: None,
            audio_host_url: "127.0.0.1:1".into(),
            attendee_id: "A1".into(),
            external_user_id: Some("U1".into()),
            join_token: super::super::join::JoinToken::new("the-token"),
        };
        let (stop, stopped) = tokio::sync::watch::channel(false);
        // Unmuted from the start, sending the tone; muted after two
        // seconds, then silence until leaving at nine, by when the video
        // has been renegotiated, has flowed for a while and (with
        // `huddle-video`) been closed again.
        let (frames, frames_in) = tokio::sync::mpsc::channel(25);
        let tone = super::super::microphone::ToneSource::start(frames).expect("a tone");
        let (mute, muted) = tokio::sync::watch::channel(false);
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(2)).await;
            let _ = mute.send(true);
            tokio::time::sleep(Duration::from_secs(7)).await;
            let _ = stop.send(true);
        });
        let uplink = Uplink {
            frames: frames_in,
            muted,
        };
        // With `huddle-video`, the share and the cameras are received
        // because the call window shows them (no `--video` streams), and
        // they are decoded; what the window had is noted before it closes.
        #[cfg(feature = "huddle-video")]
        let (viewer, screen, gallery, told, cameras_told, seen) = {
            let screen = super::super::screen::Screen::new(|| {});
            let gallery = super::super::gallery::Gallery::new(|| {});
            let (shares, told) = tokio::sync::watch::channel(Vec::new());
            let (cameras, cameras_told) = tokio::sync::watch::channel(Vec::new());
            let (wishing, wish) = tokio::sync::watch::channel(Wish::closed());
            type Seen = (
                Vec<super::super::cameras::Camera>,
                Vec<(String, [usize; 2])>,
            );
            let seen: Arc<Mutex<Seen>> = Arc::default();
            let (window_gallery, window_cameras, window_seen) =
                (gallery.clone(), cameras_told.clone(), seen.clone());
            // The window opens on the share with small tiles once the
            // session runs, and closes after a while.
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_millis(500)).await;
                let _ = wishing.send(Wish {
                    open: true,
                    share: Some("B2#content".to_owned()),
                    tiles: 4,
                    tile: [150, 90],
                });
                tokio::time::sleep(Duration::from_secs(4)).await;
                if let Ok(mut seen) = window_seen.lock() {
                    seen.0 = window_cameras.borrow().clone();
                    seen.1 = window_gallery
                        .take()
                        .into_iter()
                        .map(|(key, picture)| (key, picture.source))
                        .collect();
                }
                let _ = wishing.send(Wish::closed());
                tokio::time::sleep(Duration::from_secs(10)).await;
                drop(wishing);
            });
            let viewer = Viewer {
                shares,
                cameras,
                wish,
                screen: screen.clone(),
                gallery: gallery.clone(),
            };
            (Some(viewer), screen, gallery, told, cameras_told, seen)
        };
        #[cfg(not(feature = "huddle-video"))]
        let viewer = None;
        // Our camera: the test picture, on from the start.
        #[cfg(feature = "huddle-camera")]
        let (camera, _camera_threads) = {
            use super::super::camera::{Camera as _, Latest, TestPattern};
            use super::super::camera_send::{CameraUplink, Encoding, QUEUE, SendControl};
            let latest = Latest::default();
            let (frames, frames_in) = tokio::sync::mpsc::channel(QUEUE);
            let control = SendControl::default();
            let encoding =
                Encoding::spawn(latest.clone(), frames, control.clone(), None).expect("encoding");
            let pattern = TestPattern::new(latest).open().expect("the test picture");
            let (on, on_rx) = tokio::sync::watch::channel(true);
            let (refused, refusals) = tokio::sync::mpsc::channel(1);
            (
                Some(CameraUplink {
                    frames: frames_in,
                    on: on_rx,
                    control,
                    refused,
                    descriptor: super::super::camera_send::DESCRIPTOR,
                }),
                (encoding, pattern, on, refusals),
            )
        };
        let (report, result) = listen(
            &join,
            None,
            Some(uplink),
            stopped,
            None,
            None,
            Video {
                options: Some(video::Options {
                    streams: if cfg!(feature = "huddle-video") { 0 } else { 1 },
                    h264_only: true,
                    dump: None,
                }),
                viewer,
                #[cfg(feature = "huddle-camera")]
                camera,
            },
        )
        .await;
        drop(tone);
        #[cfg(feature = "huddle-video")]
        {
            // Who shares reached the viewer, and the share was decoded at
            // its size.
            let shares = told.borrow().clone();
            assert_eq!(shares.len(), 1, "{shares:?}");
            assert_eq!(shares[0].key, "B2#content");
            assert_eq!(shares[0].user.as_deref(), Some("U3"));
            assert!(screen.pictures() >= 5, "{} pictures", screen.pictures());
            log::info!(
                "loopback: {} pictures of the share decoded",
                screen.pictures()
            );
            // Both cameras had a tile and their pictures, at their size.
            let (cameras, pictures) = seen.lock().expect("seen").clone();
            let tiles: Vec<(&str, bool, Option<&str>)> = cameras
                .iter()
                .map(|c| (c.key.as_str(), c.tile, c.user.as_deref()))
                .collect();
            assert_eq!(tiles, [("C3", true, Some("U5")), ("D4", true, Some("U6"))]);
            assert_eq!(
                pictures,
                [("C3".to_owned(), [320, 180]), ("D4".to_owned(), [320, 180])]
            );
            assert!(gallery.pictures() >= 10, "{} pictures", gallery.pictures());
            // Closed: no tiles, the cameras still on, nothing left to draw.
            let after: Vec<bool> = cameras_told.borrow().iter().map(|c| c.tile).collect();
            assert_eq!(after, [false, false]);
            assert!(gallery.take().is_empty());
        }
        assert_eq!(result, Ok(()), "{report:?}");
        assert_eq!(report.ending.as_deref(), Some("left"));
        assert!(report.relay.is_some(), "{report:?}");
        assert!(report.dtls_up.is_some(), "{report:?}");
        assert!(report.audio_frames > 20, "{report:?}");
        assert!(report.sent_frames > 20, "{report:?}");
        assert!(report.silent_frames > 10, "muted for a second: {report:?}");

        // Chime heard the mute.
        assert_eq!(*controls.lock().expect("controls"), vec![true]);

        // Video: one re-SUBSCRIBE for the share (with `huddle-video` and
        // the cameras, C3's smaller layer), slot 0 our send line, then
        // with the window closed one freeing them all; the media server
        // was asked for keyframes and sent frames, which were counted and
        // read; audio went on through it.
        let subscribed = subscribed.lock().expect("subscribed").clone();
        if cfg!(feature = "huddle-video") {
            assert_eq!(subscribed, [vec![0], vec![0, 7, 12, 13], vec![0, 0, 0, 0]]);
        } else {
            assert_eq!(subscribed, [vec![0], vec![0, 7]]);
        }
        assert!(*keyframe_requests.lock().expect("requests") >= 1, "no PLI");
        // Our camera: asked for in the same re-SUBSCRIBE, both ways; its
        // frames arrived from a keyframe on, another keyframe came after
        // the PLI, and every frame decodes to the test picture's size.
        let camera_subscribes = subscribed_camera.lock().expect("subscribed").clone();
        let camera_frames = camera_heard.lock().expect("heard").clone();
        #[cfg(feature = "huddle-camera")]
        {
            // The first SUBSCRIBE is before the camera can be sent; every
            // re-SUBSCRIBE after it sends it, the camera being on throughout.
            assert!(camera_subscribes.len() >= 2, "{camera_subscribes:?}");
            assert!(!camera_subscribes[0] && camera_subscribes[1..].iter().all(|&c| c));
            assert!(
                camera_frames.len() >= 10,
                "{} camera frames",
                camera_frames.len()
            );
            assert!(camera_frames[0].0, "the first is a keyframe");
            assert!(
                camera_frames[5..].iter().any(|(keyframe, _)| *keyframe),
                "a keyframe after the PLI"
            );
            let mut decoder = rusty_h264_decoder::Decoder::new();
            for (_, frame) in &camera_frames {
                let picture = decoder
                    .decode(frame)
                    .expect("our H.264 decodes")
                    .expect("a picture");
                assert_eq!((picture.width, picture.height), (640, 480));
            }
            log::info!("loopback: {} camera frames decoded", camera_frames.len());
        }
        #[cfg(not(feature = "huddle-camera"))]
        {
            assert!(
                camera_subscribes.iter().all(|&c| !c),
                "{camera_subscribes:?}"
            );
            assert!(camera_frames.is_empty());
        }
        let sent = *video_sent.lock().expect("sent");
        let summary = report.video.clone().expect("a video summary");
        assert!(summary.saw_share, "{summary:?}");
        assert_eq!(summary.codecs, ["H264_CONSTRAINED_BASELINE_PROFILE"]);
        let streams: Vec<u32> = summary.streams.iter().map(|s| s.stream_id).collect();
        if cfg!(feature = "huddle-video") {
            assert_eq!(streams, [7, 12, 13]);
            for camera in &summary.streams[1..] {
                assert!(!camera.share && camera.frames >= 10, "{camera:?}");
                assert!(camera.plis >= 1, "{camera:?}");
            }
        } else {
            assert_eq!(streams, [7]);
        }
        let stream = &summary.streams[0];
        assert_eq!(stream.stream_id, 7);
        assert!(stream.share);
        assert_eq!(stream.user.as_deref(), Some("U3"));
        assert_eq!(stream.codec.as_ref().map(|c| c.0.as_str()), Some("H264"));
        assert!(stream.frames >= 10, "{} of {sent} frames", stream.frames);
        assert!(stream.keyframes >= 1, "{stream:?}");
        assert!(stream.plis >= 1, "{stream:?}");
        assert_eq!(stream.resolution(), Some((320, 180)));
        let resubscribe = &summary.resubscribes[0];
        assert_eq!(
            summary.resubscribes.len(),
            if cfg!(feature = "huddle-video") { 2 } else { 1 },
            "{summary:?}"
        );
        assert_eq!(resubscribe.stream_ids, subscribed[1]);
        assert!(
            resubscribe.audio_frames.is_some_and(|n| n > 20),
            "audio through the re-SUBSCRIBE: {resubscribe:?}"
        );
        // And it decodes what we sent: the tone, then silence; one RTP
        // clock throughout, 960 a frame; the level negotiated and sent.
        let heard = heard.lock().expect("heard").clone();
        assert!(
            heard.len() as u64 >= report.sent_frames / 2,
            "{}",
            heard.len()
        );
        assert!(
            heard.windows(2).all(|w| w[1].0 > w[0].0),
            "RTP time goes forward"
        );
        assert!(
            heard
                .iter()
                .all(|(time, _, _)| time % 960 == heard[0].0 % 960)
        );
        let mut decoder = opus_decoder::OpusDecoder::new(48_000, 1).expect("a decoder");
        let mut loud = 0;
        for (_, level, payload) in &heard {
            let mut pcm = vec![0.0; decoder.max_frame_size_per_channel()];
            let n = decoder
                .decode_float(payload, &mut pcm, false)
                .expect("our Opus decodes");
            assert_eq!(n, 960);
            if super::super::uplink::level(&pcm[..n]) < 30 {
                loud += 1;
                assert_eq!(*level, Some(-23), "the tone's level");
            }
        }
        assert!(loud > 20, "only {loud} frames of tone decoded");
        assert!(
            heard
                .iter()
                .any(|(_, level, payload)| payload[..] == SILENT_OPUS[..] && *level == Some(-127)),
            "silence after the mute"
        );
    }

    #[test]
    fn failures_name_their_step() {
        assert_eq!(
            failure(Stage::Relay, "no TURN server gave a relay").to_string(),
            "Relay: no TURN server gave a relay"
        );
    }
}
