//! The `str0m` peer of a call, a huddle's ([`super::media`]) or a Teams
//! call's, and how it is driven: [`Driver`] feeds it what arrives, runs
//! it until it waits and hands what it sends to the session, which knows
//! the way out (the relay, or a socket). A packet that does not read is
//! dropped rather than the call, and a DTLS flight nothing answered is
//! sent again.
//!
//! Beside it, what both sessions do alike with the peer: write Opus
//! ([`write_opus`]), read the send bandwidth estimate
//! ([`estimate_bps`]), and say why they stopped ([`Failure`]).

use std::net::SocketAddr;
use std::time::{Duration, Instant};

use str0m::media::{Frequency, MediaTime, Mid, Pt};
use str0m::net::{Protocol, Receive, Transmit};
use str0m::{Event, Input, Output, Rtc, RtcError};

use super::uplink::Stamp;

/// How often `str0m` reports what the far end receives of ours, and the
/// sessions log their counts.
pub const STATS_EVERY: Duration = Duration::from_secs(5);
/// Opus's frame: silence goes out this often while muted.
pub const AUDIO_TICK: Duration = Duration::from_millis(20);
/// Muted is still sending, as a browser does: Opus's 20 ms of silence.
pub const SILENT_OPUS: [u8; 3] = [0xF8, 0xFF, 0xFE];
/// Where bandwidth estimation starts, in kbit/s, when we may send video,
/// before the far end's feedback says more.
pub const BWE_START_KBPS: u64 = 700;
/// What the estimate leaves for the audio and the packets' overhead
/// (Opus, with headroom); the rest goes to our video.
pub const AUDIO_BPS: u64 = 80_000;

/// Why a session stopped before it was asked to, at which of its
/// `stages`. The worker turns it into a [`crate::failure::Failure`] for
/// the interface; `why` is for the log only.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Failure<S> {
    /// Where it failed.
    pub stage: S,
    /// Why, for the log.
    pub why: String,
}

impl<S: std::fmt::Debug> std::fmt::Display for Failure<S> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:?}: {}", self.stage, self.why)
    }
}

/// A [`Failure`] at `stage`.
pub fn failure<S>(stage: S, why: impl Into<String>) -> Failure<S> {
    Failure {
        stage,
        why: why.into(),
    }
}

/// Writes one Opus packet on `mid` at payload type `pt`: its RTP time
/// and marker from `stamp`, its level for RFC 6464 (0 loudest, 127
/// silence; written only if the far end took the extension). False if
/// it could not.
pub fn write_opus(
    rtc: &mut Rtc,
    mid: Mid,
    pt: Pt,
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
        .write(pt, Instant::now(), time, payload)
    {
        Ok(()) => true,
        Err(error) => {
            log::debug!("media: could not send audio: {error}");
            false
        }
    }
}

/// Opus's payload type on `mid`, as the far end's answer settled it.
pub fn opus_pt(rtc: &mut Rtc, mid: Mid) -> Option<Pt> {
    rtc.writer(mid)?
        .payload_params()
        .find(|p| p.spec().codec == str0m::format::Codec::Opus)
        .map(|p| p.pt())
}

/// The send bandwidth estimate, in bit/s, from transport-cc feedback or
/// REMB; `None` for anything else.
pub fn estimate_bps(estimate: &str0m::bwe::BweKind) -> Option<u64> {
    match estimate {
        str0m::bwe::BweKind::Twcc { estimate, .. } => Some(estimate.as_u64()),
        str0m::bwe::BweKind::Remb { estimate, .. } => Some(estimate.as_u64()),
        _ => None,
    }
}

/// Takes a new estimate of `bps` over `last`, logging it when it moved
/// by more than a fifth: it moves a little all the time. Says whether it
/// was logged.
pub fn note_estimate(last: &mut Option<u64>, bps: u64) -> bool {
    let moved = last.is_none_or(|was| was.abs_diff(bps) > was / 5);
    if moved {
        log::info!("media: send bandwidth estimate {} kbit/s", bps / 1000);
    }
    *last = Some(bps);
    moved
}

/// What went which way while connecting, by path and kind: which path
/// ICE settled on, and whether the DTLS handshake crossed it, shows in
/// the log when a call does not come up.
#[derive(Clone, Copy, Debug, Default)]
pub struct Paths {
    /// `[in, out][direct, relayed][stun, dtls, rtp]`.
    counts: [[[u64; 3]; 2]; 2],
}

impl Paths {
    /// Counts a datagram going `out` (or in) on the relay (or directly).
    pub fn count(&mut self, out: bool, relayed: bool, data: &[u8]) {
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
    pub fn line(&self) -> String {
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

/// A `str0m` peer and what driving it takes: when it wants its next
/// timeout, our DTLS flight while the handshake runs, and what went
/// which way.
#[derive(Debug)]
pub struct Driver {
    /// The peer.
    pub rtc: Rtc,
    /// When it wants [`Input::Timeout`] next.
    timeout: Option<Instant>,
    /// Our last DTLS flight, to send again if it goes unanswered.
    flight: Flight,
    /// Whether DTLS and SRTP are up: the flight is no longer kept.
    connected: bool,
    /// What went which way, by path and kind.
    pub paths: Paths,
}

impl Driver {
    /// Drives `rtc`; its timeouts start once it is first woken
    /// ([`Self::wake`]) or fed.
    pub fn new(rtc: Rtc) -> Self {
        Self {
            rtc,
            timeout: None,
            flight: Flight::default(),
            connected: false,
            paths: Paths::default(),
        }
    }

    /// The peer has something to do at `now` (input came, media was
    /// written): the next [`Self::drive`] runs it.
    pub fn wake(&mut self, now: Instant) {
        self.timeout = Some(now);
    }

    /// Whether DTLS and SRTP came up.
    pub fn connected(&self) -> bool {
        self.connected
    }

    /// The next moment the peer, or our unanswered flight, is due.
    pub fn deadline(&self) -> Option<Instant> {
        let resend = self.flight.resend_at.filter(|_| !self.connected);
        [self.timeout, resend].into_iter().flatten().min()
    }

    /// Feeds the peer what `source` sent to our `destination`, on the
    /// relay (`relayed`) or directly.
    pub fn receive(
        &mut self,
        now: Instant,
        source: SocketAddr,
        destination: SocketAddr,
        relayed: bool,
        data: &[u8],
    ) {
        self.paths.count(false, relayed, data);
        if is_dtls(data) {
            self.flight.heard();
        }
        let Ok(receive) = Receive::new(Protocol::Udp, source, destination, data) else {
            log::debug!(
                "connect: {} bytes from {source} that WebRTC does not read",
                data.len()
            );
            return;
        };
        if let Err(error) = self.rtc.handle_input(Input::Receive(now, receive)) {
            log::debug!("connect: input from {source}: {error}");
        }
        self.timeout = Some(now);
    }

    /// Runs the peer until it waits. What it sends goes to `send`, which
    /// says whether it went on the relay (`Some(true)`), directly
    /// (`Some(false)`) or nowhere; what it tells comes back, in order.
    /// Up to [`SKIP_AT_MOST`] packets that do not read are dropped each
    /// time (a meeting's media server sent an H.264 packet too short to
    /// unpack, seen ending a call); any other error ends the media.
    pub fn drive(
        &mut self,
        now: Instant,
        mut send: impl FnMut(&Transmit) -> Option<bool>,
    ) -> Result<Vec<Event>, RtcError> {
        if self.timeout.is_some_and(|at| at <= now)
            && let Err(error) = self.rtc.handle_input(Input::Timeout(now))
        {
            log::debug!("connect: timeout input: {error}");
        }
        let mut events = Vec::new();
        // Packets dropped this round, so a stream of them cannot spin here.
        let mut skipped = 0;
        loop {
            match self.rtc.poll_output() {
                Ok(Output::Timeout(at)) => {
                    self.timeout = Some(at);
                    return Ok(events);
                }
                Ok(Output::Transmit(transmit)) => {
                    let Some(relayed) = send(&transmit) else {
                        continue;
                    };
                    self.paths.count(true, relayed, &transmit.contents);
                    if !self.connected && is_dtls(&transmit.contents) {
                        self.flight
                            .sent(relayed, transmit.destination, &transmit.contents, now);
                    }
                }
                Ok(Output::Event(event)) => {
                    if matches!(event, Event::Connected) {
                        self.connected = true;
                    }
                    events.push(event);
                }
                Err(error) if is_one_packet(&error) && skipped < SKIP_AT_MOST => {
                    skipped += 1;
                    log::info!("media: a packet dropped: {error}");
                }
                Err(error) => return Err(error),
            }
        }
    }

    /// Our DTLS flight, when nothing answered it in time and it is to be
    /// sent again at `now`, each datagram the way it went first (on the
    /// relay, and to where).
    pub fn resend(&mut self, now: Instant) -> Option<Vec<(bool, SocketAddr, Vec<u8>)>> {
        if self.connected || !self.flight.due(now) {
            return None;
        }
        log::info!(
            "connect: no DTLS answer; sending our {} handshake packets again (try {})",
            self.flight.datagrams.len(),
            self.flight.tries
        );
        for (relayed, _, data) in &self.flight.datagrams {
            self.paths.count(true, *relayed, data);
        }
        Some(self.flight.datagrams.clone())
    }
}

/// How many unreadable packets one round of driving the peer drops before
/// the media counts as broken.
pub const SKIP_AT_MOST: u32 = 100;

/// Whether `error` is about one packet only (one that did not unpack or
/// parse), not the connection.
///
/// A meeting's media server once sent an H.264 packet too short to
/// unpack, and str0m's error for it ended the call; such a packet is
/// dropped instead, the call goes on.
pub fn is_one_packet(error: &str0m::RtcError) -> bool {
    matches!(
        error,
        str0m::RtcError::Packet(..) | str0m::RtcError::Rtp(_) | str0m::RtcError::Net(_)
    )
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
pub struct Flight {
    /// Each datagram with whether it went through the relay, and to where.
    pub datagrams: Vec<(bool, SocketAddr, Vec<u8>)>,
    /// Whether the far end has said anything since this flight began: the
    /// next datagram we send starts a new flight.
    answered: bool,
    /// When to send it again.
    pub resend_at: Option<Instant>,
    /// The wait before that.
    wait: Duration,
    /// How often it was sent again.
    pub tries: u32,
}

/// How many times a flight is sent again before the connection timeout
/// is left to end the call.
pub const FLIGHT_TRIES: u32 = 6;

impl Flight {
    /// We sent a DTLS datagram at `now`.
    pub fn sent(&mut self, relayed: bool, to: SocketAddr, data: &[u8], now: Instant) {
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
    pub fn heard(&mut self) {
        self.answered = true;
        self.resend_at = None;
    }

    /// Whether to send the flight again at `now`; moves the next time on.
    pub fn due(&mut self, now: Instant) -> bool {
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
pub fn is_dtls(data: &[u8]) -> bool {
    matches!(data.first(), Some(20..=63))
}

#[cfg(test)]
mod tests {
    use str0m::RtcError;
    use str0m::error::{NetError, PacketError};
    use str0m::media::{Mid, Pt};

    use super::*;

    #[test]
    fn a_bad_packet_is_one_packet_but_a_broken_connection_is_not() {
        let short = RtcError::Packet(Mid::from("1"), Pt::from(96), PacketError::ErrShortPacket);
        assert!(is_one_packet(&short));
        let unparsed = RtcError::Net(NetError::Io(std::io::Error::other("bad")));
        assert!(is_one_packet(&unparsed));
        assert!(!is_one_packet(&RtcError::RemoteSdp("no".to_owned())));
        assert!(!is_one_packet(&RtcError::Io(std::io::Error::other("gone"))));
        assert!(!is_one_packet(&RtcError::NoSenderSource));
    }

    #[test]
    fn dtls_is_told_from_stun_and_rtp() {
        assert!(is_dtls(&[22, 254, 253]));
        assert!(!is_dtls(&[0, 1]), "STUN");
        assert!(!is_dtls(&[0x80, 111]), "RTP");
        assert!(!is_dtls(&[]));
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
    fn datagrams_are_counted_by_path_and_kind() {
        let mut paths = Paths::default();
        paths.count(false, true, &[0, 1]);
        paths.count(true, false, &[22]);
        paths.count(true, false, &[0x80]);
        paths.count(true, false, &[255]);
        assert_eq!(
            paths.line(),
            "direct in [stun 0 dtls 0 rtp 0] out [stun 0 dtls 1 rtp 1]; \
             relayed in [stun 1 dtls 0 rtp 0] out [stun 0 dtls 0 rtp 0]"
        );
    }

    #[test]
    fn an_estimate_is_logged_only_when_it_moved_a_fifth() {
        let mut last = None;
        assert!(note_estimate(&mut last, 1_000_000), "the first");
        assert!(!note_estimate(&mut last, 1_100_000), "a tenth more");
        assert_eq!(last, Some(1_100_000), "taken, logged or not");
        assert!(note_estimate(&mut last, 800_000), "over a fifth less");
        assert!(!note_estimate(&mut last, 800_000));
    }

    #[test]
    fn a_failure_says_its_stage() {
        #[derive(Debug)]
        enum Stage {
            Connect,
        }
        assert_eq!(
            failure(Stage::Connect, "no answer").to_string(),
            "Connect: no answer"
        );
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
}
