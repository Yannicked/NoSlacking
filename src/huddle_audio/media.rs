//! The listening session: signaling, the TURN relay and the WebRTC peer,
//! driven together on one task.
//!
//! WebRTC is `str0m`: sans-IO like the TURN client, with its default
//! crypto (aws-lc-rs; DTLS 1.2 through `dimpl`), so no OpenSSL on any
//! platform; aws-lc builds with the C compiler `ring` already needs. It
//! gathers nothing itself; the only local candidate is the TURN relay
//! ([`super::turn`]), as Chime reaches media only through one. What
//! `str0m` sends from the relay's address goes to the TURN server as a
//! Send indication; what the server relays back is fed to `str0m` as if
//! it had arrived at that address.
//!
//! Every step logs a line at info level, named so a probe's log shows
//! where a mismatch is: the frames (by [`super::chime::describe`]), the
//! SDP (by [`super::sdp::summary`]), the relay, ICE and DTLS, the first
//! audio, and counts every five seconds. Secrets never appear.

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

use super::chime::{self, FrameType};
use super::join::ChimeJoin;
use super::sdp::{self, Mids};
use super::signaling::{Ending, Handshake, Incoming, Socket, Step, TurnCredentials};
use super::speaker::Feed;
use super::turn::{self, Server, Transport};

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
const SILENT_OPUS: [u8; 3] = [0xF8, 0xFF, 0xFE];
const AUDIO_TICK: Duration = Duration::from_millis(20);

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
/// from `local`): Opus, and VP8 and H.264 for the video m-line.
fn new_peer(relayed: SocketAddr, local: SocketAddr) -> Result<Rtc, String> {
    let mut rtc = RtcConfig::new()
        .clear_codecs()
        .enable_opus(true, false)
        .enable_vp8(true)
        .enable_h264(true)
        .set_crypto_provider(Arc::new(str0m::crypto::from_feature_flags()))
        .build(Instant::now());
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
    api.add_media(MediaKind::Video, Direction::Inactive, None, None, None);
    let (offer, pending) = api.apply()?;
    let offer = offer.to_sdp_string();
    let mids = Mids::of_offer(&offer);
    Some(Offer {
        sdp: mids.offer_for_chime(&offer),
        pending,
        mids,
        audio,
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
    audio_time: u64,
    feed: Option<Feed>,
    /// Told once the audio connection is up.
    live: Option<tokio::sync::oneshot::Sender<()>>,
    report: Report,
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
        if self.over.is_some() {
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
        match new_peer(relayed, local) {
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
        let offer = made.sdp;
        log::info!("subscribe: offer {}", sdp::summary(&offer));
        let sub = chime::Subscribe {
            sdp_offer: offer,
            audio_host: self.join.audio_host_url.clone(),
            attendee_id: self.join.attendee_id.clone(),
            muted: true,
        };
        let steps = self.handshake.subscribe(&sub, now_ms());
        Box::pin(self.carry(steps)).await;
    }

    /// Takes Chime's answer.
    fn answer(&mut self, answer: &str) {
        log::info!("subscribe: answer {}", sdp::summary(answer));
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
        self.connect_deadline = Some(Instant::now() + CONNECT_TIMEOUT);
        self.rtc_timeout = Some(Instant::now());
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
            RtcEvent::MediaData(data) => {
                if Some(data.mid) != self.audio {
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

    /// Sends 20 ms of silence, keeping the audio stream alive.
    fn send_silence(&mut self) {
        let now = Instant::now();
        let (Some(rtc), Some(mid)) = (&mut self.rtc, self.audio) else {
            return;
        };
        let Some(writer) = rtc.writer(mid) else {
            return;
        };
        let Some(pt) = writer
            .payload_params()
            .find(|p| p.spec().codec == Codec::Opus)
            .map(|p| p.pt())
        else {
            return;
        };
        let time = MediaTime::new(self.audio_time, str0m::media::Frequency::FORTY_EIGHT_KHZ);
        if let Err(error) = writer.write(pt, now, time, SILENT_OPUS.to_vec()) {
            log::debug!("media: could not send silence: {error}");
        }
        self.audio_time += u64::from(super::jitter::FRAME);
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
    }

    /// One frame from Chime.
    async fn on_frame(&mut self, frame: &chime::Frame) {
        self.last_inbound = Instant::now();
        let name = chime::type_name(frame);
        *self.report.frames.entry(name).or_default() += 1;
        if quiet(frame) {
            log::debug!("signaling: received {}", chime::describe(frame));
        } else {
            log::info!("signaling: received {}", chime::describe(frame));
        }
        let steps = self.handshake.on_frame(frame, now_ms());
        self.carry(steps).await;
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

/// Listens to the huddle `join` describes until `stop` turns true or the
/// session ends, feeding the audio to `feed` and telling `live` once the
/// audio connection is up. Leaves cleanly either way.
pub async fn listen(
    join: &ChimeJoin,
    feed: Option<Feed>,
    mut stop: tokio::sync::watch::Receiver<bool>,
    live: Option<tokio::sync::oneshot::Sender<()>>,
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
        audio_time: 0,
        feed,
        live,
        report: Report::default(),
        over: None,
    };
    let steps = session.handshake.start(now_ms());
    session.carry(steps).await;
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
    (session.report, result)
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
        let mut ours = new_peer(relayed, local).expect("a peer");
        let offer = make_offer(&mut ours).expect("an offer");
        assert!(offer.sdp.contains("o=mozilla-chrome "));
        assert!(offer.sdp.contains("a=mid:0\r\n") && offer.sdp.contains("a=mid:1\r\n"));
        assert!(offer.sdp.contains("typ relay"));
        assert!(offer.sdp.contains("opus/48000/2"));

        let mut chime = RtcConfig::new()
            .set_crypto_provider(Arc::new(str0m::crypto::from_feature_flags()))
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
        let mut ours = new_peer(relayed, local).expect("a peer");
        let offer = make_offer(&mut ours).expect("an offer");
        let mut chime = RtcConfig::new()
            .set_crypto_provider(Arc::new(str0m::crypto::from_feature_flags()))
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
    /// [`listen`] itself. Ignored by default: it opens local sockets and
    /// runs for seconds. `cargo test --all-features -- --ignored loopback`.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[ignore = "opens loopback sockets and takes a few seconds"]
    async fn loopback_session_joins_listens_and_leaves() {
        use futures_util::{SinkExt as _, StreamExt as _};
        use tokio_tungstenite::tungstenite::Message as Ws;
        use turn::{Class, Message, Method, attr, read_xor_address, xor_address};

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
                }
                tokio::select! {
                    offer = offer_inbox.recv() => {
                        let Some((offer, reply)) = offer else { return };
                        let mut rtc = RtcConfig::new()
                            .set_crypto_provider(Arc::new(str0m::crypto::from_feature_flags()))
                            .build(Instant::now());
                        rtc.add_local_candidate(Candidate::host(media_server, "udp").expect("a candidate"));
                        let answer = rtc.sdp_api()
                            .accept_offer(str0m::change::SdpOffer::from_sdp_string(&offer).expect("parses"))
                            .expect("accepted");
                        let _ = reply.send(answer.to_sdp_string());
                        chime = Some(rtc);
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
                        let _ = ws.send(reply(chime::frame(FrameType::Index, 2))).await;
                    }
                    Ok(FrameType::Subscribe) => {
                        let offer = frame.sub.and_then(|s| s.sdp_offer).expect("an offer");
                        let (answer_to, answer) = tokio::sync::oneshot::channel();
                        let _ = offers.send((offer, answer_to)).await;
                        let mut ack = chime::frame(FrameType::SubscribeAck, 3);
                        ack.suback = Some(chime::proto::SdkSubscribeAckFrame {
                            sdp_answer: Some(answer.await.expect("an answer")),
                            ..Default::default()
                        });
                        let _ = ws.send(reply(ack)).await;
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
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(3)).await;
            let _ = stop.send(true);
        });
        let (report, result) = listen(&join, None, stopped, None).await;
        assert_eq!(result, Ok(()), "{report:?}");
        assert_eq!(report.ending.as_deref(), Some("left"));
        assert!(report.relay.is_some(), "{report:?}");
        assert!(report.dtls_up.is_some(), "{report:?}");
        assert!(report.audio_frames > 20, "{report:?}");
    }

    #[test]
    fn failures_name_their_step() {
        assert_eq!(
            failure(Stage::Relay, "no TURN server gave a relay").to_string(),
            "Relay: no TURN server gave a relay"
        );
    }
}
