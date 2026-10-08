//! What the two `str0m` drivers, a huddle's ([`super::media`]) and a
//! Teams call's, share in keeping their peer up: a packet that does not
//! read is dropped rather than the call, and a DTLS flight nothing
//! answered is sent again.
//!
//! The drivers themselves are still two; these are the pieces that had
//! to behave the same in both, kept here until they become one.

use std::net::SocketAddr;
use std::time::{Duration, Instant};

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
