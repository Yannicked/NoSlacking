//! The SDP offer and answer: shaped the way Chime takes them, and
//! summed up for the log.
//!
//! Chime expects what a browser sends. So the offer's origin line names
//! `mozilla-chrome`, as the JS SDK's `SDP.withUnifiedPlanFormat` makes it
//! (HuddleFM does the same), and its media ids are a browser's `0`, `1`
//! rather than `str0m`'s random ones, mapped back in the answer. The
//! answer loses its server-reflexive candidates, as the JS SDK's
//! `withoutServerReflexiveCandidates` drops them, and, since the relay is
//! UDP, its TCP ones.

use std::fmt::Write as _;
use std::net::{IpAddr, SocketAddr};

/// One `a=candidate` line, read.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CandidateLine {
    /// `udp` or `tcp`, in lower case.
    pub protocol: String,
    /// Where it is.
    pub address: SocketAddr,
    /// `host`, `srflx`, `prflx` or `relay`.
    pub kind: String,
}

/// Reads an `a=candidate:` line (RFC 8839 §5.1).
pub fn candidate(line: &str) -> Option<CandidateLine> {
    let rest = line.trim().strip_prefix("a=candidate:")?;
    let fields: Vec<&str> = rest.split_whitespace().collect();
    // foundation component transport priority address port "typ" type …
    let ip: IpAddr = fields.get(4)?.parse().ok()?;
    let port: u16 = fields.get(5)?.parse().ok()?;
    if fields.get(6) != Some(&"typ") {
        return None;
    }
    Some(CandidateLine {
        protocol: fields.get(2)?.to_ascii_lowercase(),
        address: SocketAddr::new(ip, port),
        kind: (*fields.get(7)?).to_owned(),
    })
}

/// The SDP's lines, whatever its line ends.
fn lines(sdp: &str) -> impl Iterator<Item = &str> {
    sdp.split('\n')
        .map(|l| l.trim_end_matches('\r'))
        .filter(|l| !l.is_empty())
}

fn join(lines: impl Iterator<Item = String>) -> String {
    let mut out = String::new();
    for line in lines {
        out.push_str(&line);
        out.push_str("\r\n");
    }
    out
}

/// The media ids of an SDP, in m-line order.
pub fn mids(sdp: &str) -> Vec<String> {
    lines(sdp)
        .filter_map(|l| l.strip_prefix("a=mid:"))
        .map(str::to_owned)
        .collect()
}

/// How `str0m`'s media ids map to a browser's: the n-th becomes `n`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Mids {
    ours: Vec<String>,
}

impl Mids {
    /// Learns the ids from our own offer.
    pub fn of_offer(offer: &str) -> Self {
        Self { ours: mids(offer) }
    }

    fn rename(&self, sdp: &str, to_browser: bool) -> String {
        let map = |mid: &str| -> String {
            if to_browser {
                self.ours
                    .iter()
                    .position(|m| m == mid)
                    .map_or_else(|| mid.to_owned(), |n| n.to_string())
            } else {
                mid.parse::<usize>()
                    .ok()
                    .and_then(|n| self.ours.get(n))
                    .cloned()
                    .unwrap_or_else(|| mid.to_owned())
            }
        };
        join(lines(sdp).map(|line| {
            if let Some(mid) = line.strip_prefix("a=mid:") {
                format!("a=mid:{}", map(mid))
            } else if let Some(group) = line.strip_prefix("a=group:BUNDLE") {
                let renamed: Vec<String> = group.split_whitespace().map(map).collect();
                format!("a=group:BUNDLE {}", renamed.join(" "))
            } else {
                line.to_owned()
            }
        }))
    }

    /// Our offer, as Chime takes it: browser media ids and a browser's
    /// origin line.
    pub fn offer_for_chime(&self, offer: &str) -> String {
        let renamed = self.rename(offer, true);
        join(lines(&renamed).map(|line| match line.strip_prefix("o=") {
            Some(origin) => {
                let rest = origin.split_once(' ').map_or("", |(_, rest)| rest);
                format!("o=mozilla-chrome {rest}")
            }
            None => line.to_owned(),
        }))
    }

    /// Chime's answer, for `str0m`: our media ids back, only the
    /// candidates a UDP relay can reach, and each payload type described
    /// once (see `once_per_payload`).
    pub fn answer_from_chime(&self, answer: &str) -> String {
        let renamed = self.rename(answer, false);
        once_per_payload(&join(
            lines(&renamed)
                .filter(|line| {
                    candidate(line).is_none_or(|c| c.protocol == "udp" && c.kind != "srflx")
                })
                .map(str::to_owned),
        ))
    }
}

/// The SDP with each media section naming a payload type once: on its
/// `m=` line, and in one `a=rtpmap` and one `a=fmtp` (the first). Chime's
/// answers have repeated `a=rtpmap:109` in the video section, which
/// browsers let pass and `str0m` refuses as a parse error.
fn once_per_payload(sdp: &str) -> String {
    let mut seen_map = std::collections::HashSet::new();
    let mut seen_fmtp = std::collections::HashSet::new();
    let payload = |rest: &str| rest.split_whitespace().next().unwrap_or("").to_owned();
    join(lines(sdp).filter_map(|line| {
        if let Some(media) = line.strip_prefix("m=") {
            seen_map.clear();
            seen_fmtp.clear();
            // "kind port proto pt pt …": the payload types start at the fourth.
            let mut parts = media.split_whitespace();
            let head: Vec<&str> = parts.by_ref().take(3).collect();
            let mut types: Vec<&str> = Vec::new();
            for pt in parts {
                if !types.contains(&pt) {
                    types.push(pt);
                }
            }
            return Some(format!("m={} {}", head.join(" "), types.join(" ")));
        }
        if let Some(rest) = line.strip_prefix("a=rtpmap:") {
            return seen_map.insert(payload(rest)).then(|| line.to_owned());
        }
        if let Some(rest) = line.strip_prefix("a=fmtp:") {
            return seen_fmtp.insert(payload(rest)).then(|| line.to_owned());
        }
        Some(line.to_owned())
    }))
}

/// The addresses of an SDP's candidates, for TURN permissions.
pub fn candidate_addresses(sdp: &str) -> Vec<SocketAddr> {
    lines(sdp)
        .filter_map(candidate)
        .map(|c| c.address)
        .collect()
}

/// One line about an SDP for the log: per m-line its kind, direction,
/// codecs and candidates, and the session's ICE and DTLS roles. Never the
/// ICE password or the certificate fingerprint itself.
pub fn summary(sdp: &str) -> String {
    let mut out = String::new();
    let mut media: Vec<String> = Vec::new();
    let mut current: Option<String> = None;
    let mut session = Vec::new();
    let flush = |current: &mut Option<String>, media: &mut Vec<String>| {
        if let Some(m) = current.take() {
            media.push(m);
        }
    };
    for line in lines(sdp) {
        if let Some(m) = line.strip_prefix("m=") {
            flush(&mut current, &mut media);
            let kind = m.split_whitespace().next().unwrap_or("?");
            current = Some(kind.to_owned());
            continue;
        }
        let note = if let Some(dir @ ("sendrecv" | "sendonly" | "recvonly" | "inactive")) =
            line.strip_prefix("a=")
        {
            Some(dir.to_owned())
        } else if let Some(map) = line.strip_prefix("a=rtpmap:") {
            Some(map.replace(' ', ":"))
        } else if let Some(c) = candidate(line) {
            Some(format!("cand:{}/{}/{}", c.kind, c.protocol, c.address))
        } else if let Some(mid) = line.strip_prefix("a=mid:") {
            Some(format!("mid:{mid}"))
        } else if let Some(setup) = line.strip_prefix("a=setup:") {
            Some(format!("setup:{setup}"))
        } else if let Some(fingerprint) = line.strip_prefix("a=fingerprint:") {
            Some(format!(
                "fingerprint:{}",
                fingerprint.split_whitespace().next().unwrap_or("?")
            ))
        } else if line == "a=ice-lite" {
            Some("ice-lite".into())
        } else if line.starts_with("a=ssrc:") {
            line.split_whitespace()
                .next()
                .map(|s| s.replace("a=ssrc:", "ssrc:"))
        } else {
            line.strip_prefix("a=group:")
                .map(|group| format!("group:{}", group.replace(' ', ",")))
        };
        if let Some(note) = note {
            match &mut current {
                Some(m) => {
                    if !m.contains(&format!(" {note}")) {
                        m.push(' ');
                        m.push_str(&note);
                    }
                }
                None => session.push(note),
            }
        }
    }
    flush(&mut current, &mut media);
    let _ = write!(out, "{} bytes", sdp.len());
    if !session.is_empty() {
        let _ = write!(out, "; session: {}", session.join(" "));
    }
    for m in media {
        let _ = write!(out, "; m={m}");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Our offer as `str0m` writes it, shortened.
    const OFFER: &str = "v=0\r\n\
o=str0m-0.24.1 4214103069889147331 2 IN IP4 0.0.0.0\r\n\
s=-\r\n\
t=0 0\r\n\
a=group:BUNDLE jaK qpL\r\n\
m=audio 9 UDP/TLS/RTP/SAVPF 111\r\n\
c=IN IP4 0.0.0.0\r\n\
a=candidate:fffe 1 udp 37748479 203.0.113.5 50000 typ relay raddr 0.0.0.0 rport 0\r\n\
a=ice-ufrag:IlmQFlOBg4xdt9nk\r\n\
a=ice-pwd:7DQ1Lio17mjm9JLArLM45D\r\n\
a=fingerprint:sha-256 47:12:B9:FD\r\n\
a=setup:actpass\r\n\
a=mid:jaK\r\n\
a=sendrecv\r\n\
a=rtpmap:111 opus/48000/2\r\n\
a=ssrc:4032505130 cname:audio\r\n\
m=video 9 UDP/TLS/RTP/SAVPF 96\r\n\
a=mid:qpL\r\n\
a=inactive\r\n\
a=rtpmap:96 VP8/90000\r\n";

    /// An answer as a Chime media server might give it (invented).
    const ANSWER: &str = "v=0\r\n\
o=- 1 2 IN IP4 127.0.0.1\r\n\
s=-\r\n\
t=0 0\r\n\
a=ice-lite\r\n\
a=group:BUNDLE 0 1\r\n\
m=audio 3478 UDP/TLS/RTP/SAVPF 111\r\n\
a=candidate:1 1 udp 2130706431 192.0.2.10 3478 typ host\r\n\
a=candidate:2 1 tcp 1694498815 192.0.2.10 443 typ host tcptype passive\r\n\
a=candidate:3 1 udp 1694498815 198.51.100.4 3478 typ srflx raddr 192.0.2.10 rport 3478\r\n\
a=ice-ufrag:abcd\r\n\
a=ice-pwd:secretsecretsecret\r\n\
a=fingerprint:sha-256 AA:BB\r\n\
a=setup:passive\r\n\
a=mid:0\r\n\
a=sendrecv\r\n\
a=rtpmap:111 opus/48000/2\r\n\
m=video 0 UDP/TLS/RTP/SAVPF 96\r\n\
a=mid:1\r\n\
a=inactive\r\n";

    #[test]
    fn the_offer_looks_like_a_browsers() {
        let mids = Mids::of_offer(OFFER);
        let offer = mids.offer_for_chime(OFFER);
        assert!(offer.contains("\r\no=mozilla-chrome 4214103069889147331 2 IN IP4 0.0.0.0\r\n"));
        assert!(offer.contains("a=group:BUNDLE 0 1\r\n"));
        assert!(offer.contains("a=mid:0\r\n") && offer.contains("a=mid:1\r\n"));
        assert!(!offer.contains("jaK"));
        assert!(offer.ends_with("\r\n"));
    }

    #[test]
    fn the_answer_gets_our_ids_and_reachable_candidates() {
        let mids = Mids::of_offer(OFFER);
        let answer = mids.answer_from_chime(ANSWER);
        assert!(answer.contains("a=group:BUNDLE jaK qpL\r\n"));
        assert!(answer.contains("a=mid:jaK\r\n") && answer.contains("a=mid:qpL\r\n"));
        assert!(answer.contains("192.0.2.10 3478 typ host"));
        assert!(!answer.contains("tcptype"), "TCP candidates go");
        assert!(!answer.contains("typ srflx"), "reflexive ones too");
        assert_eq!(
            candidate_addresses(&answer),
            vec!["192.0.2.10:3478".parse::<SocketAddr>().expect("an address")]
        );
    }

    #[test]
    fn a_payload_type_described_twice_is_kept_once() {
        // As Chime answered a real huddle: 109 twice in the video section.
        let answer = "v=0\r\n\
m=audio 9 UDP/TLS/RTP/SAVPF 111\r\n\
a=rtpmap:111 opus/48000/2\r\n\
a=fmtp:111 minptime=10;useinbandfec=1\r\n\
m=video 9 UDP/TLS/RTP/SAVPF 108 109 96 109\r\n\
a=rtpmap:108 H264/90000\r\n\
a=rtpmap:109 rtx/90000\r\n\
a=fmtp:109 apt=108\r\n\
a=rtpmap:96 VP8/90000\r\n\
a=rtpmap:109 rtx/90000\r\n\
a=fmtp:109 apt=108\r\n\
a=inactive\r\n";
        let tidy = once_per_payload(answer);
        assert_eq!(tidy.matches("a=rtpmap:109 ").count(), 1);
        assert_eq!(tidy.matches("a=fmtp:109 ").count(), 1);
        assert!(tidy.contains("m=video 9 UDP/TLS/RTP/SAVPF 108 109 96\r\n"));
        // Other sections and lines are untouched, in order.
        assert!(tidy.starts_with(
            "v=0\r\nm=audio 9 UDP/TLS/RTP/SAVPF 111\r\na=rtpmap:111 opus/48000/2\r\n"
        ));
        assert!(tidy.ends_with("a=rtpmap:96 VP8/90000\r\na=inactive\r\n"));
        // The same payload type in two sections is no repeat.
        let two = "m=audio 9 P 109\r\na=rtpmap:109 a/1\r\nm=video 9 P 109\r\na=rtpmap:109 b/1\r\n";
        assert_eq!(once_per_payload(two).matches("a=rtpmap:109").count(), 2);
    }

    #[test]
    fn candidates_read() {
        assert_eq!(
            candidate("a=candidate:1 1 UDP 2130706431 192.0.2.10 3478 typ host"),
            Some(CandidateLine {
                protocol: "udp".into(),
                address: "192.0.2.10:3478".parse().expect("an address"),
                kind: "host".into(),
            })
        );
        assert_eq!(candidate("a=candidate:1 1 udp 1 nonsense 1 typ host"), None);
        assert_eq!(candidate("a=mid:0"), None);
    }

    #[test]
    fn summaries_keep_the_shape_and_leave_the_secrets() {
        let line = summary(ANSWER);
        assert!(
            line.contains("session: ice-lite group:BUNDLE,0,1"),
            "{line}"
        );
        assert!(
            line.contains("m=audio cand:host/udp/192.0.2.10:3478"),
            "{line}"
        );
        assert!(line.contains("setup:passive"), "{line}");
        assert!(line.contains("111:opus/48000/2"), "{line}");
        assert!(line.contains("fingerprint:sha-256"), "{line}");
        assert!(line.contains("m=video mid:1 inactive"), "{line}");
        assert!(
            !line.contains("secretsecret") && !line.contains("AA:BB"),
            "{line}"
        );
        assert!(!summary(OFFER).contains("7DQ1Lio17mjm9JLArLM45D"));
    }
}
