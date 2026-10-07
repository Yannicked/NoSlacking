//! Microsoft's SDP dialect: read into [`super::RemoteMedia`], written from [`super::LocalMedia`].
//!
//! Teams speaks a browser's WebRTC (one bundled transport, DTLS-SRTP,
//! Opus) in an SDP that no WebRTC stack reads or writes as is
//! (`docs/research/teams-calls.md` §D): `RTP/SAVP` on every m-line,
//! `TCP-ACT` candidates and `MTURNID` suffixes, a separate ICE session
//! per m-line in the native client's offers, rejected lines without a
//! mid, and lines a browser never writes (`a=label`, `a=x-ssrc-range`).
//! str0m is driven through its direct API instead (§F.2), so this module
//! is plain text work: [`read`] takes what the call needs out of the far
//! end's SDP, and [`offer`] and [`answer`] write ours in the shape the web
//! client sends (§D.1, §D.4).

use std::net::{IpAddr, SocketAddr};

use super::{
    CAMERA_LABEL, Candidate, CandidateKind, Direction, Line, LineKind, LocalMedia, RemoteMedia,
    Setup, VideoCodec,
};

/// Why an SDP could not be read.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum SdpError {
    /// The text does not start with `v=0`: not an SDP at all.
    #[error("not an SDP")]
    NotSdp,
    /// An `m=` line whose port is not a number; the line number counts
    /// from 1, for the log.
    #[error("unreadable m-line at line {0}")]
    BadMediaLine(usize),
    /// There are no m-lines, so nothing to call with.
    #[error("no m-lines")]
    NoMedia,
    /// The bundle's transport has no `ice-ufrag` or `ice-pwd`, so ICE
    /// cannot start.
    #[error("no ICE credentials")]
    NoIceCredentials,
}

// Two header extensions Microsoft writes with backslashes, where the
// browser and str0m write slashes (§D.1). Its side expects them so.
const ABS_SEND_TIME: &str = r"http:\\www.webrtc.org\experiments\rtp-hdrext\abs-send-time";
const TRANSPORT_CC: &str =
    r"http:\\www.ietf.org\id\draft-holmer-rmcat-transport-wide-cc-extensions-01";

/// One m-line as written, before it means anything.
struct RawLine<'a> {
    media: &'a str,
    port: u16,
    attrs: Vec<(&'a str, &'a str)>,
}

impl<'a> RawLine<'a> {
    /// The first value of an attribute (`a=name:value`; `""` for a bare
    /// `a=name`).
    fn attr(&self, name: &str) -> Option<&'a str> {
        find(&self.attrs, name)
    }
}

fn find<'a>(attrs: &[(&'a str, &'a str)], name: &str) -> Option<&'a str> {
    attrs.iter().find(|(n, _)| *n == name).map(|(_, v)| *v)
}

/// Splits an attribute line's text (after `a=`) into name and value.
fn attribute(text: &str) -> (&str, &str) {
    match text.split_once(':') {
        Some((name, value)) => (name, value.trim()),
        None => (text, ""),
    }
}

/// Reads the far end's SDP: an answer to our offer, its offer for an
/// incoming call, or a renegotiation offer, in any of the shapes of §D.2
/// and §D.3, with CRLF or LF line ends.
///
/// The call has one transport, so the ICE credentials, candidates,
/// fingerprint and DTLS role come from the bundle's first m-line, even
/// when other m-lines bring their own (the native client's offers do):
/// feeding str0m a second set would look like an ICE restart. Only UDP
/// candidates for component 1 are kept, as the call uses UDP and
/// `rtcp-mux`.
pub fn read(sdp: &str) -> Result<RemoteMedia, SdpError> {
    let mut text = sdp
        .split('\n')
        .map(|l| l.trim_end_matches('\r'))
        .enumerate()
        .filter(|(_, l)| !l.trim().is_empty());
    match text.next() {
        Some((_, first)) if first.trim() == "v=0" => {}
        _ => return Err(SdpError::NotSdp),
    }

    let mut session: Vec<(&str, &str)> = Vec::new();
    let mut raw: Vec<RawLine<'_>> = Vec::new();
    for (index, line) in text {
        if let Some(m) = line.strip_prefix("m=") {
            let mut fields = m.split_whitespace();
            let media = fields.next().ok_or(SdpError::BadMediaLine(index + 1))?;
            // A port may carry a count (`port/2`), which nobody here uses.
            let port = fields
                .next()
                .and_then(|p| p.split('/').next())
                .and_then(|p| p.parse().ok())
                .ok_or(SdpError::BadMediaLine(index + 1))?;
            raw.push(RawLine {
                media,
                port,
                attrs: Vec::new(),
            });
        } else if let Some(a) = line.strip_prefix("a=") {
            let pair = attribute(a);
            match raw.last_mut() {
                Some(last) => last.attrs.push(pair),
                None => session.push(pair),
            }
        }
    }
    if raw.is_empty() {
        return Err(SdpError::NoMedia);
    }

    // A rejected line may have no mid at all (file 043); its position
    // stands in, which is what an answer to it must keep anyway.
    let mids: Vec<String> = raw
        .iter()
        .enumerate()
        .map(|(i, l)| l.attr("mid").map_or_else(|| i.to_string(), str::to_owned))
        .collect();

    let transport = transport_line(&session, &raw, &mids);
    let line = &raw[transport];
    let session_attr = |name: &str| find(&session, name);
    let ufrag = line.attr("ice-ufrag").or_else(|| session_attr("ice-ufrag"));
    let pwd = line.attr("ice-pwd").or_else(|| session_attr("ice-pwd"));
    let (Some(ufrag), Some(pwd)) = (ufrag, pwd) else {
        return Err(SdpError::NoIceCredentials);
    };

    let mut candidates = candidates_of(line);
    if candidates.is_empty() {
        // Candidates are listed once, normally on the transport line; if a
        // writer put them on another line of the same ICE session, take
        // them from there.
        if let Some(other) = raw.iter().find(|l| {
            l.attr("ice-ufrag").or_else(|| session_attr("ice-ufrag")) == Some(ufrag)
                && !candidates_of(l).is_empty()
        }) {
            candidates = candidates_of(other);
        }
    }

    let fingerprint = line
        .attrs
        .iter()
        .chain(session.iter())
        .filter(|(n, _)| *n == "fingerprint")
        .find_map(|(_, v)| sha256(v));
    let setup = line
        .attr("setup")
        .or_else(|| session_attr("setup"))
        .map_or(Setup::Unsaid, setup_of);

    let opus_pt = raw.iter().find(|l| l.media == "audio").and_then(|audio| {
        audio
            .attrs
            .iter()
            .filter(|(n, _)| *n == "rtpmap")
            .find_map(|(_, v)| opus_rtpmap(v))
    });

    // The camera's line: labelled `main-video`, else the first video
    // line without a label.
    let camera = raw
        .iter()
        .filter(|l| l.media == "video" && l.port != 0)
        .find(|l| l.attr("label") == Some(CAMERA_LABEL))
        .or_else(|| {
            raw.iter()
                .find(|l| l.media == "video" && l.port != 0 && l.attr("label").is_none())
        });
    let video = camera.and_then(h264_of);

    let session_direction = direction_in(&session);
    let lines = raw
        .iter()
        .zip(mids)
        .map(|(l, mid)| Line {
            mid,
            kind: kind_of(l.media),
            label: l.attr("label").map(str::to_owned),
            port: l.port,
            // A rejected line carries nothing either way, whatever it
            // says (it usually says nothing).
            direction: if l.port == 0 {
                Direction::Inactive
            } else {
                direction_in(&l.attrs)
                    .or(session_direction)
                    .unwrap_or(Direction::SendRecv)
            },
        })
        .collect();

    Ok(RemoteMedia {
        ice_ufrag: ufrag.to_owned(),
        ice_pwd: pwd.to_owned(),
        fingerprint,
        setup,
        candidates,
        opus_pt,
        video,
        lines,
    })
}

/// The first H.264 at packetization mode 1 a video line lists, and its
/// retransmission's payload type.
fn h264_of(line: &RawLine<'_>) -> Option<VideoCodec> {
    let fmtp = |pt: &str| {
        line.attrs
            .iter()
            .filter(|(n, _)| *n == "fmtp")
            .find_map(|(_, v)| v.strip_prefix(pt).and_then(|rest| rest.strip_prefix(' ')))
    };
    let pt = line
        .attrs
        .iter()
        .filter(|(n, _)| *n == "rtpmap")
        .filter_map(|(_, v)| v.split_once(' '))
        .filter(|(_, codec)| {
            codec
                .split('/')
                .next()
                .is_some_and(|name| name.eq_ignore_ascii_case("H264"))
        })
        .map(|(pt, _)| pt)
        .find(|pt| {
            fmtp(pt).is_some_and(|params| {
                params
                    .split(';')
                    .any(|p| p.trim().eq_ignore_ascii_case("packetization-mode=1"))
            })
        })?;
    let rtx = line
        .attrs
        .iter()
        .filter(|(n, _)| *n == "rtpmap")
        .filter_map(|(_, v)| v.split_once(' '))
        .filter(|(_, codec)| {
            codec
                .split('/')
                .next()
                .is_some_and(|name| name.eq_ignore_ascii_case("rtx"))
        })
        .find(|(rtx, _)| fmtp(rtx).is_some_and(|p| p.trim() == format!("apt={pt}")))
        .and_then(|(rtx, _)| rtx.parse().ok());
    Some(VideoCodec {
        pt: pt.parse().ok()?,
        rtx,
    })
}

/// Which m-line carries the call's transport: the bundle's first that is
/// not rejected, or without a bundle the first that is not rejected.
fn transport_line(session: &[(&str, &str)], raw: &[RawLine<'_>], mids: &[String]) -> usize {
    let bundled = session
        .iter()
        .filter(|(n, _)| *n == "group")
        .find_map(|(_, v)| v.strip_prefix("BUNDLE"))
        .into_iter()
        .flat_map(str::split_whitespace)
        .filter_map(|mid| mids.iter().position(|m| m == mid))
        .find(|&i| raw[i].port != 0);
    bundled
        .or_else(|| raw.iter().position(|l| l.port != 0))
        .unwrap_or(0)
}

fn candidates_of(line: &RawLine<'_>) -> Vec<Candidate> {
    line.attrs
        .iter()
        .filter(|(n, _)| *n == "candidate")
        .filter_map(|(_, v)| candidate(v))
        .collect()
}

/// Reads one `a=candidate:` value, keeping it only if it is UDP for
/// component 1 with a numeric address. Microsoft writes the transport in
/// upper case and its TCP forms as `TCP-ACT`/`TCP-PASS`; anything after
/// the type and related address (`MTURNID n`, `generation 0`, …) is
/// ignored, since the relay candidate that carries `MTURNID` is the one
/// most likely to work.
fn candidate(value: &str) -> Option<Candidate> {
    let fields: Vec<&str> = value.split_whitespace().collect();
    // foundation component transport priority address port "typ" type …
    let [
        foundation,
        component,
        transport,
        priority,
        address,
        port,
        typ,
        kind,
        ..,
    ] = fields.as_slice()
    else {
        return None;
    };
    if *component != "1" || !transport.eq_ignore_ascii_case("udp") || *typ != "typ" {
        return None;
    }
    let kind = match *kind {
        "host" => CandidateKind::Host,
        "srflx" => CandidateKind::ServerReflexive,
        "relay" => CandidateKind::Relay,
        // Peer reflexive candidates are learned, not signalled; anything
        // else is not a kind we know how to use.
        _ => return None,
    };
    // An mDNS name (`….local`) does not parse and is dropped: Microsoft's
    // side never writes one.
    let ip: IpAddr = address.parse().ok()?;
    Some(Candidate {
        foundation: (*foundation).to_owned(),
        priority: priority.parse().ok()?,
        addr: SocketAddr::new(ip, port.parse().ok()?),
        kind,
    })
}

/// The hash of an `a=fingerprint` value, if it is SHA-256.
fn sha256(value: &str) -> Option<String> {
    let (algorithm, hash) = value.split_once(' ')?;
    algorithm
        .eq_ignore_ascii_case("sha-256")
        .then(|| hash.trim().to_owned())
}

fn setup_of(value: &str) -> Setup {
    match value {
        "actpass" => Setup::ActPass,
        "active" => Setup::Active,
        "passive" => Setup::Passive,
        _ => Setup::Unsaid,
    }
}

/// The payload type of an `a=rtpmap` value naming Opus.
fn opus_rtpmap(value: &str) -> Option<u8> {
    let (pt, codec) = value.split_once(' ')?;
    let name = codec.split('/').next()?;
    if name.eq_ignore_ascii_case("opus") {
        pt.parse().ok()
    } else {
        None
    }
}

fn direction_in(attrs: &[(&str, &str)]) -> Option<Direction> {
    attrs.iter().find_map(|(n, _)| match *n {
        "sendrecv" => Some(Direction::SendRecv),
        "sendonly" => Some(Direction::SendOnly),
        "recvonly" => Some(Direction::RecvOnly),
        "inactive" => Some(Direction::Inactive),
        _ => None,
    })
}

fn kind_of(media: &str) -> LineKind {
    match media {
        "audio" => LineKind::Audio,
        "video" => LineKind::Video,
        "x-data" => LineKind::Data,
        _ => LineKind::Other,
    }
}

/// The DTLS role we answer with, given the role the offer asks for.
///
/// The web client answers `active` in every captured call: to an offer
/// that says `actpass`, `passive` (a renegotiation keeping the first
/// handshake's roles) or nothing at all (the native client's offers).
/// Only an offer that takes the client role itself leaves us the server.
pub fn answer_setup(offer: Setup) -> Setup {
    match offer {
        Setup::Active => Setup::Passive,
        Setup::ActPass | Setup::Passive | Setup::Unsaid => Setup::Active,
    }
}

/// Writes our offer for an audio call, in the shape of the web client's
/// audio-only offer (file 041, §D.1).
///
/// Four m-lines on one transport, as the web client always sends: `0`
/// audio (`main-audio`, Opus at `local.opus_pt`, CN and telephone-event
/// as the browser lists them), `1` and `2` video (`main-video`,
/// `applicationsharing-video`), and `3` the data line (`data`) when
/// `local.data_ssrc` is set; without it the data line is left out (the
/// second attempt of §F.3 4b). The candidates are listed on the audio
/// line, and every line's port and `c=` are those of the relay candidate.
///
/// With `local.video_ssrc` set, the camera's line (`1`) carries our
/// camera: H.264 constrained baseline, `sendrecv`. Otherwise, and always for the share
/// line, the video lines are offered `inactive`, with a single H.264
/// codec, rather than left out or rejected with port 0. The web client's share
/// line is offered just so, and Microsoft answers it in kind, so this is
/// a shape its side is known to take; a port-0 line in an offer is legal
/// but never seen in the capture, and leaving lines out would change the
/// mids and labels its side keys streams by.
pub fn offer(local: &LocalMedia) -> String {
    let (address, port) = media_address(local);
    let mut mids = vec!["0", "1", "2"];
    if local.data_ssrc.is_some() {
        mids.push("3");
    }

    let mut out = String::new();
    session_part(&mut out, local, &mids);

    // Audio: Opus first (the browser puts CN/48000 first, but Opus is what
    // we mean), then the extra codecs the web client lists, skipping any
    // that would take Opus's number.
    let opus = local.opus_pt;
    let extras: Vec<(u8, &str)> = [
        (105, "CN/48000"),
        (13, "CN/8000"),
        (110, "telephone-event/48000"),
        (126, "telephone-event/8000"),
    ]
    .into_iter()
    .filter(|(pt, _)| *pt != opus)
    .collect();
    let mut payloads = opus.to_string();
    for (pt, _) in &extras {
        payloads.push_str(&format!(" {pt}"));
    }
    push(&mut out, &format!("m=audio {port} RTP/SAVP {payloads}"));
    push(&mut out, &connection(address));
    push(&mut out, "a=x-signaling-fb:* x-message app recv:dsh");
    push(&mut out, &ssrc_range(local.audio_ssrc));
    push(&mut out, &format!("a=rtpmap:{opus} opus/48000/2"));
    for (pt, codec) in &extras {
        push(&mut out, &format!("a=rtpmap:{pt} {codec}"));
    }
    push(
        &mut out,
        &format!("a=fmtp:{opus} minptime=10;useinbandfec=1"),
    );
    push(&mut out, &format!("a=rtcp:{port}"));
    push(&mut out, &format!("a=rtcp-fb:{opus} transport-cc"));
    push(
        &mut out,
        "a=extmap:1 urn:ietf:params:rtp-hdrext:ssrc-audio-level",
    );
    push(&mut out, &format!("a=extmap:2 {ABS_SEND_TIME}"));
    push(&mut out, &format!("a=extmap:3 {TRANSPORT_CC}"));
    push(&mut out, "a=extmap:4 urn:ietf:params:rtp-hdrext:sdes:mid");
    push(&mut out, &format!("a=setup:{}", setup_text(local.setup)));
    push(&mut out, "a=mid:0");
    push(&mut out, direction_text(local.audio_direction));
    transport_part(&mut out, local, true);
    push(&mut out, "a=label:main-audio");

    // Video, for the lines' sake only. In a bundle a payload type must not
    // mean two codecs, so H.264 moves aside if Opus has its number.
    let (h264, rtx) = if opus == 108 || opus == 109 {
        (118, 119)
    } else {
        (108, 109)
    };
    for (mid, label) in [("1", CAMERA_LABEL), ("2", "applicationsharing-video")] {
        // The camera's line carries our camera when this build has video.
        if label == CAMERA_LABEL
            && let Some(ssrc) = local.video_ssrc
        {
            let setup = setup_text(local.setup);
            let codec = VideoCodec {
                pt: local.video_pt,
                rtx: local.video_rtx,
            };
            camera_line(
                &mut out,
                local,
                mid,
                codec,
                ssrc,
                local.video_direction,
                setup,
                false,
            );
            continue;
        }
        push(&mut out, &format!("m=video {port} RTP/SAVP {h264} {rtx}"));
        push(&mut out, &connection(address));
        push(
            &mut out,
            "a=x-signaling-fb:* x-message app send:src recv:src,vc",
        );
        push(&mut out, &format!("a=rtpmap:{h264} H264/90000"));
        push(&mut out, &format!("a=rtpmap:{rtx} rtx/90000"));
        push(
            &mut out,
            &format!(
                "a=fmtp:{h264} level-asymmetry-allowed=1;packetization-mode=1;profile-level-id=42e01f"
            ),
        );
        push(&mut out, &format!("a=fmtp:{rtx} apt={h264}"));
        push(&mut out, &format!("a=rtcp:{port}"));
        push(&mut out, "a=rtcp-fb:* transport-cc");
        push(&mut out, "a=rtcp-fb:* nack");
        push(&mut out, "a=rtcp-fb:* nack pli");
        push(&mut out, &format!("a=extmap:2 {ABS_SEND_TIME}"));
        push(&mut out, &format!("a=extmap:3 {TRANSPORT_CC}"));
        push(&mut out, &format!("a=setup:{}", setup_text(local.setup)));
        push(&mut out, &format!("a=mid:{mid}"));
        push(&mut out, "a=inactive");
        transport_part(&mut out, local, false);
        push(&mut out, "a=rtcp-rsize");
        push(&mut out, &format!("a=label:{label}"));
    }

    if let Some(ssrc) = local.data_ssrc {
        data_line(&mut out, local, "3", ssrc, setup_text(local.setup), false);
    }
    out
}

/// Writes our answer to the far end's offer or renegotiation offer, in
/// the shape of the web client's answers (§D.4).
///
/// The answer has the offer's mids, in its order and number. Audio is
/// accepted with Opus alone, at the offerer's payload type, and our
/// candidates; if the offer has no Opus the audio line is rejected too,
/// and the call cannot go on. The camera's line is kept, at the offer's
/// H.264 payload type, while `local.video_ssrc` is set and the offer has
/// H.264 we can use. Every other line is rejected as the web client
/// rejects the share line, `m=video 0 RTP/SAVP 34` (H.263's number
/// as a placeholder) followed only by its label, except the data line
/// while `local.data_ssrc` is set, which is kept as the web client keeps
/// it. The bundle groups the lines we keep.
///
/// No header extensions are answered: an answer must use the offerer's
/// extension ids, which [`RemoteMedia`] does not carry, and audio works
/// without them.
pub fn answer(local: &LocalMedia, remote: &RemoteMedia) -> String {
    let (address, port) = media_address(local);
    let setup = setup_text(answer_setup(remote.setup));
    let audio_index = remote
        .lines
        .iter()
        .position(|l| l.kind == LineKind::Audio && l.port != 0);
    let camera_mid = remote.camera().map(|l| l.mid.as_str());
    let accepted = |i: usize, line: &Line| -> bool {
        line.port != 0
            && match line.kind {
                LineKind::Audio => Some(i) == audio_index && remote.opus_pt.is_some(),
                LineKind::Data => local.data_ssrc.is_some(),
                LineKind::Video => {
                    Some(line.mid.as_str()) == camera_mid
                        && local.video_ssrc.is_some()
                        && remote.video.is_some()
                }
                LineKind::Other => false,
            }
    };
    let kept: Vec<&str> = remote
        .lines
        .iter()
        .enumerate()
        .filter(|(i, l)| accepted(*i, l))
        .map(|(_, l)| l.mid.as_str())
        .collect();

    let mut out = String::new();
    session_part(&mut out, local, &kept);

    // The candidates go on the first line we keep: the bundle's transport.
    let mut candidates_written = false;
    for (i, line) in remote.lines.iter().enumerate() {
        if !accepted(i, line) {
            reject(&mut out, line);
            continue;
        }
        let first = !candidates_written;
        candidates_written = true;
        match (line.kind, remote.opus_pt, local.data_ssrc) {
            (LineKind::Audio, Some(opus), _) => {
                push(&mut out, &format!("m=audio {port} RTP/SAVP {opus}"));
                push(&mut out, &connection(address));
                push(&mut out, "a=x-signaling-fb:* x-message app recv:dsh");
                push(&mut out, &ssrc_range(local.audio_ssrc));
                push(&mut out, &format!("a=rtpmap:{opus} opus/48000/2"));
                push(
                    &mut out,
                    &format!("a=fmtp:{opus} minptime=10;useinbandfec=1"),
                );
                push(&mut out, &format!("a=rtcp-fb:{opus} transport-cc"));
                push(&mut out, &format!("a=setup:{setup}"));
                push(&mut out, &format!("a=mid:{}", line.mid));
                push(
                    &mut out,
                    direction_text(answer_direction(local.audio_direction, line.direction)),
                );
                transport_part(&mut out, local, first);
                let label = line.label.as_deref().unwrap_or("main-audio");
                push(&mut out, &format!("a=label:{label}"));
            }
            (LineKind::Data, _, Some(ssrc)) => {
                data_line(&mut out, local, &line.mid, ssrc, setup, first);
            }
            (LineKind::Video, _, _) => {
                // `accepted` keeps the camera's line only with both.
                if let (Some(offered), Some(ssrc)) = (remote.video, local.video_ssrc) {
                    let direction = answer_direction(local.video_direction, line.direction);
                    // Resends only if both sides do them.
                    let codec = VideoCodec {
                        pt: offered.pt,
                        rtx: offered.rtx.filter(|_| local.video_rtx.is_some()),
                    };
                    camera_line(
                        &mut out, local, &line.mid, codec, ssrc, direction, setup, first,
                    );
                }
            }
            // `accepted` keeps nothing else.
            _ => reject(&mut out, line),
        }
    }
    out
}

/// Our direction for a line, given what we want and what the offer
/// allows: we send only if it receives, and receive only if it sends.
fn answer_direction(ours: Direction, theirs: Direction) -> Direction {
    let sends = |d: Direction| matches!(d, Direction::SendRecv | Direction::SendOnly);
    let receives = |d: Direction| matches!(d, Direction::SendRecv | Direction::RecvOnly);
    match (
        sends(ours) && receives(theirs),
        receives(ours) && sends(theirs),
    ) {
        (true, true) => Direction::SendRecv,
        (true, false) => Direction::SendOnly,
        (false, true) => Direction::RecvOnly,
        (false, false) => Direction::Inactive,
    }
}

/// The camera's line (`main-video`): H.264 at `codec.pt`, packetization
/// mode 1, constrained baseline as our encoder makes it, the receiver
/// free to send another level (`level-asymmetry-allowed`), as the web
/// client's answers write it (§D.4). With `codec.rtx`, lost packets are
/// asked for again (`nack`) and come on the retransmission's payload
/// type; a picture that cannot be mended asks for a keyframe (`nack
/// pli`).
#[expect(
    clippy::too_many_arguments,
    reason = "an m-line's parts, as `data_line` takes them"
)]
fn camera_line(
    out: &mut String,
    local: &LocalMedia,
    mid: &str,
    codec: VideoCodec,
    ssrc: u32,
    direction: Direction,
    setup: &str,
    candidates: bool,
) {
    let (address, port) = media_address(local);
    let pt = codec.pt;
    let payloads = match codec.rtx {
        Some(rtx) => format!("{pt} {rtx}"),
        None => pt.to_string(),
    };
    push(out, &format!("m=video {port} RTP/SAVP {payloads}"));
    push(out, &connection(address));
    push(out, "a=x-signaling-fb:* x-message app send:src recv:src,vc");
    push(out, &ssrc_range(ssrc));
    push(out, &format!("a=rtpmap:{pt} H264/90000"));
    push(
        out,
        &format!(
            "a=fmtp:{pt} level-asymmetry-allowed=1;packetization-mode=1;profile-level-id=42e01f"
        ),
    );
    if let Some(rtx) = codec.rtx {
        push(out, &format!("a=rtpmap:{rtx} rtx/90000"));
        push(out, &format!("a=fmtp:{rtx} apt={pt}"));
    }
    push(out, &format!("a=rtcp:{port}"));
    push(out, "a=rtcp-fb:* goog-remb");
    push(out, "a=rtcp-fb:* transport-cc");
    if codec.rtx.is_some() {
        push(out, "a=rtcp-fb:* nack");
    }
    push(out, "a=rtcp-fb:* nack pli");
    push(out, &format!("a=setup:{setup}"));
    push(out, &format!("a=mid:{mid}"));
    push(out, direction_text(direction));
    transport_part(out, local, candidates);
    push(out, "a=rtcp-rsize");
    push(out, &format!("a=label:{CAMERA_LABEL}"));
}

/// A rejected line, as the web client writes one: port 0, a placeholder
/// payload type, and the label so Microsoft's side knows which stream it
/// was. No mid, as in the capture.
fn reject(out: &mut String, line: &Line) {
    let (media, payload) = match line.kind {
        LineKind::Audio => ("audio", 0),
        LineKind::Video => ("video", 34),
        LineKind::Data => ("x-data", 127),
        // Microsoft's SDP has had no other kind; the media word is lost in
        // reading, so `application` stands in.
        LineKind::Other => ("application", 0),
    };
    push(out, &format!("m={media} 0 RTP/SAVP {payload}"));
    if let Some(label) = &line.label {
        push(out, &format!("a=label:{label}"));
    }
}

/// The data line of §D.1: the browser's SCTP data channel, dressed as an
/// RTP m-line the way the web client rewrites it.
fn data_line(
    out: &mut String,
    local: &LocalMedia,
    mid: &str,
    ssrc: u32,
    setup: &str,
    candidates: bool,
) {
    let (address, port) = media_address(local);
    push(out, &format!("m=x-data {port} RTP/SAVP 127 126"));
    push(out, &connection(address));
    push(out, "a=x-data-protocol:sctp");
    push(out, &ssrc_range(ssrc));
    push(out, "a=rtpmap:127 x-data/90000");
    push(out, "a=rtpmap:126 rtx/90000");
    push(out, "a=fmtp:126 apt=127");
    push(out, &format!("a=setup:{setup}"));
    push(out, &format!("a=mid:{mid}"));
    push(out, "a=sendrecv");
    transport_part(out, local, candidates);
    push(out, "a=label:data");
    push(out, "a=sctp-port:5000");
    push(out, "a=max-message-size:262144");
}

/// The session part of §D.1, with a bundle over `mids`.
fn session_part(out: &mut String, local: &LocalMedia, mids: &[&str]) {
    push(out, "v=0");
    push(
        out,
        &format!(
            "o=- {} {} IN IP4 127.0.0.1",
            local.session_id, local.session_version
        ),
    );
    push(out, "s=-");
    push(out, "b=CT:4000");
    push(out, "t=0 0");
    push(out, "a=extmap-allow-mixed");
    push(out, "a=msid-semantic: WMS *");
    if !mids.is_empty() {
        push(out, &format!("a=group:BUNDLE {}", mids.join(" ")));
    }
}

/// The transport attributes every kept m-line repeats, as the browser
/// writes them, with the candidates on the first line only.
fn transport_part(out: &mut String, local: &LocalMedia, candidates: bool) {
    push(out, &format!("a=ice-ufrag:{}", local.ice_ufrag));
    push(out, &format!("a=ice-pwd:{}", local.ice_pwd));
    push(out, &format!("a=fingerprint:sha-256 {}", local.fingerprint));
    if candidates {
        for c in &local.candidates {
            push(out, &candidate_line(c));
        }
    }
    push(out, "a=ice-options:trickle");
    push(out, "a=rtcp-mux");
}

/// An `a=candidate` line for one of ours, in the browser's lower case.
///
/// A server reflexive or relay candidate must name a related address;
/// [`Candidate`] does not keep it, so this writes the zero address, as a
/// browser does when it hides the local one.
fn candidate_line(c: &Candidate) -> String {
    let kind = match c.kind {
        CandidateKind::Host => "host",
        CandidateKind::ServerReflexive => "srflx",
        CandidateKind::Relay => "relay",
    };
    let mut line = format!(
        "a=candidate:{} 1 udp {} {} {} typ {kind}",
        c.foundation,
        c.priority,
        c.addr.ip(),
        c.addr.port()
    );
    if c.kind != CandidateKind::Host {
        let zero = if c.addr.is_ipv4() { "0.0.0.0" } else { "::" };
        line.push_str(&format!(" raddr {zero} rport 0"));
    }
    line
}

/// The address our m-lines name: the relay candidate's, as the web
/// client's (Microsoft's side reaches us through it most reliably), or
/// the first candidate's, or the browser's placeholder when there is none.
fn media_address(local: &LocalMedia) -> (Option<IpAddr>, u16) {
    local
        .candidates
        .iter()
        .find(|c| c.kind == CandidateKind::Relay)
        .or_else(|| local.candidates.first())
        .map_or((None, 9), |c| (Some(c.addr.ip()), c.addr.port()))
}

fn connection(address: Option<IpAddr>) -> String {
    match address {
        Some(IpAddr::V6(ip)) => format!("c=IN IP6 {ip}"),
        Some(IpAddr::V4(ip)) => format!("c=IN IP4 {ip}"),
        None => "c=IN IP4 0.0.0.0".to_owned(),
    }
}

/// Microsoft's `x-ssrc-range` for a line that sends on one SSRC.
fn ssrc_range(ssrc: u32) -> String {
    format!("a=x-ssrc-range:{ssrc}-{ssrc}")
}

fn setup_text(setup: Setup) -> &'static str {
    match setup {
        Setup::Active => "active",
        Setup::Passive => "passive",
        // An offer that does not choose leaves the choice to the answerer.
        Setup::ActPass | Setup::Unsaid => "actpass",
    }
}

fn direction_text(direction: Direction) -> &'static str {
    match direction {
        Direction::SendRecv => "a=sendrecv",
        Direction::SendOnly => "a=sendonly",
        Direction::RecvOnly => "a=recvonly",
        Direction::Inactive => "a=inactive",
    }
}

/// Adds one line with the CRLF Microsoft's own SDP uses.
fn push(out: &mut String, line: &str) {
    out.push_str(line);
    out.push_str("\r\n");
}

#[cfg(test)]
mod tests {
    use super::*;

    const OFFER_041: &str = include_str!("fixtures/offer_041_audio_only.sdp");
    const ANSWER_001: &str = include_str!("fixtures/answer_001_media_answer.sdp");
    const ANSWER_043: &str = include_str!("fixtures/answer_043_video_rejected.sdp");
    const INCOMING_020: &str = include_str!("fixtures/incoming_offer_020.sdp");
    const RENEGOTIATION_010: &str = include_str!("fixtures/renegotiation_010.sdp");
    const RENEGOTIATION_029: &str = include_str!("fixtures/renegotiation_029_data_own_ice.sdp");
    const OUR_ANSWER_024: &str = include_str!("fixtures/our_answer_024.sdp");

    fn fingerprint(first: &str) -> String {
        format!(
            "{first}:AA:BB:CC:DD:AA:BB:CC:DD:AA:BB:CC:DD:AA:BB:CC:DD:AA:BB:CC:DD:AA:BB:CC:DD:AA:BB:CC:DD:EE:FF:00"
        )
    }

    fn summary(media: &RemoteMedia) -> Vec<(&str, LineKind, Option<&str>, u16, Direction)> {
        media
            .lines
            .iter()
            .map(|l| {
                (
                    l.mid.as_str(),
                    l.kind,
                    l.label.as_deref(),
                    l.port,
                    l.direction,
                )
            })
            .collect()
    }

    fn addr(text: &str) -> SocketAddr {
        text.parse().expect("a test address")
    }

    fn local() -> LocalMedia {
        LocalMedia {
            ice_ufrag: "lu01".to_owned(),
            ice_pwd: "local-placeholder-pwd-01".to_owned(),
            fingerprint: fingerprint("0A"),
            setup: Setup::ActPass,
            candidates: vec![
                Candidate {
                    foundation: "1".to_owned(),
                    priority: 2_122_194_687,
                    addr: addr("192.0.2.5:50000"),
                    kind: CandidateKind::Host,
                },
                Candidate {
                    foundation: "2".to_owned(),
                    priority: 1_685_987_071,
                    addr: addr("203.0.113.5:50000"),
                    kind: CandidateKind::ServerReflexive,
                },
                Candidate {
                    foundation: "3".to_owned(),
                    priority: 58_597_887,
                    addr: addr("198.51.100.5:51000"),
                    kind: CandidateKind::Relay,
                },
            ],
            audio_ssrc: 1234,
            video_ssrc: None,
            video_pt: 108,
            video_rtx: Some(109),
            video_direction: Direction::SendRecv,
            opus_pt: 111,
            audio_direction: Direction::SendRecv,
            data_ssrc: Some(5678),
            session_id: 4242,
            session_version: 2,
        }
    }

    #[test]
    fn reads_the_callee_answer() {
        let media = read(ANSWER_001).expect("answer 001 reads");
        assert_eq!(media.ice_ufrag, "uf02");
        assert_eq!(media.ice_pwd, "placeholder-ice-pwd-0002");
        assert_eq!(media.fingerprint, Some(fingerprint("02")));
        assert_eq!(media.setup, Setup::Passive);
        assert_eq!(media.opus_pt, Some(111));
        // The MTURNID relay is kept; the four TCP-ACT/TCP-PASS lines go.
        assert_eq!(
            media.candidates,
            vec![
                Candidate {
                    foundation: "10".to_owned(),
                    priority: 1_258_286_078,
                    addr: addr("198.51.100.11:3480"),
                    kind: CandidateKind::Relay,
                },
                Candidate {
                    foundation: "7".to_owned(),
                    priority: 2_130_702_846,
                    addr: addr("192.0.2.142:9763"),
                    kind: CandidateKind::Host,
                },
                Candidate {
                    foundation: "9".to_owned(),
                    priority: 1_694_494_206,
                    addr: addr("203.0.113.10:9763"),
                    kind: CandidateKind::ServerReflexive,
                },
            ]
        );
        assert_eq!(
            summary(&media),
            vec![
                (
                    "0",
                    LineKind::Audio,
                    Some("main-audio"),
                    3480,
                    Direction::SendRecv
                ),
                (
                    "1",
                    LineKind::Video,
                    Some("main-video"),
                    3480,
                    Direction::SendRecv
                ),
                (
                    "2",
                    LineKind::Video,
                    Some("applicationsharing-video"),
                    3480,
                    Direction::Inactive
                ),
                ("3", LineKind::Data, Some("data"), 3480, Direction::SendRecv),
            ]
        );
    }

    #[test]
    fn a_rejected_line_without_a_mid_takes_its_position() {
        let media = read(ANSWER_043).expect("answer 043 reads");
        assert_eq!(media.ice_ufrag, "uf03");
        assert_eq!(media.candidates.len(), 3);
        assert_eq!(
            summary(&media)[1],
            ("1", LineKind::Video, None, 0, Direction::Inactive)
        );
        assert_eq!(media.lines[2].mid, "2");
    }

    #[test]
    fn reads_lf_line_ends_too() {
        let lf = ANSWER_001.replace("\r\n", "\n");
        assert_eq!(read(&lf), read(ANSWER_001));
        assert!(!read(&lf).expect("LF reads").candidates.is_empty());
    }

    #[test]
    fn reads_the_incoming_offer() {
        let media = read(INCOMING_020).expect("offer 020 reads");
        // The audio line's ICE session, not the video line's.
        assert_eq!(media.ice_ufrag, "uf04");
        assert_eq!(media.ice_pwd, "placeholder-ice-pwd-0004");
        assert_eq!(media.fingerprint, Some(fingerprint("04")));
        assert_eq!(media.setup, Setup::Unsaid);
        // SIREN takes 111 here; Opus is at 102.
        assert_eq!(media.opus_pt, Some(102));
        // Component 2 and TCP are dropped, leaving relay, host and srflx.
        let kept: Vec<_> = media.candidates.iter().map(|c| (c.addr, c.kind)).collect();
        assert_eq!(
            kept,
            vec![
                (addr("198.51.100.15:3480"), CandidateKind::Relay),
                (addr("192.0.2.142:31170"), CandidateKind::Host),
                (addr("203.0.113.10:31170"), CandidateKind::ServerReflexive),
            ]
        );
        assert_eq!(
            summary(&media),
            vec![
                (
                    "audio_0",
                    LineKind::Audio,
                    Some("main-audio"),
                    3480,
                    Direction::SendRecv
                ),
                (
                    "video_1",
                    LineKind::Video,
                    Some("main-video"),
                    3480,
                    Direction::SendRecv
                ),
            ]
        );
    }

    #[test]
    fn reads_renegotiation_offers() {
        let media = read(RENEGOTIATION_010).expect("offer 010 reads");
        assert_eq!(media.setup, Setup::Passive);
        assert_eq!(media.candidates.len(), 3);
        assert_eq!(
            summary(&media),
            vec![
                (
                    "0",
                    LineKind::Audio,
                    Some("main-audio"),
                    3480,
                    Direction::SendRecv
                ),
                (
                    "1",
                    LineKind::Video,
                    Some("main-video"),
                    3480,
                    Direction::RecvOnly
                ),
                (
                    "2",
                    LineKind::Video,
                    Some("applicationsharing-video"),
                    0,
                    Direction::Inactive
                ),
                ("3", LineKind::Data, Some("data"), 3480, Direction::SendRecv),
            ]
        );

        // The data line's own ICE session and `actpass` are not the call's.
        let media = read(RENEGOTIATION_029).expect("offer 029 reads");
        assert_eq!(media.ice_ufrag, "uf04");
        assert_eq!(media.setup, Setup::Passive);
        assert_eq!(media.candidates.len(), 1);
        assert_eq!(media.candidates[0].addr, addr("192.0.2.142:31170"));
        assert_eq!(media.lines[2].mid, "data_2");
        assert_eq!(media.lines[2].kind, LineKind::Data);
        assert_eq!(media.lines[2].port, 3481);
    }

    #[test]
    fn reads_the_web_clients_own_sdp() {
        let media = read(OFFER_041).expect("offer 041 reads");
        assert_eq!(media.ice_ufrag, "uf01");
        assert_eq!(media.setup, Setup::ActPass);
        assert_eq!(media.opus_pt, Some(111));
        assert_eq!(media.candidates.len(), 4);
        assert_eq!(media.lines[1].direction, Direction::RecvOnly);
        assert_eq!(media.lines.len(), 4);

        let media = read(OUR_ANSWER_024).expect("answer 024 reads");
        assert_eq!(media.setup, Setup::Active);
        assert_eq!(media.opus_pt, Some(102));
        assert_eq!(media.candidates.len(), 4);
    }

    #[test]
    fn keeps_only_udp_rtp_candidates() {
        let udp = "1 1 udp 10 192.0.2.1 5000 typ host generation 0";
        assert!(candidate(udp).is_some());
        let mturn = "10 1 UDP 10 198.51.100.1 3480 typ relay raddr 203.0.113.1 rport 1 MTURNID 99";
        assert_eq!(candidate(mturn).map(|c| c.kind), Some(CandidateKind::Relay));
        assert!(candidate("3 1 TCP-ACT 10 192.0.2.1 1024 typ host").is_none());
        assert!(candidate("3 1 TCP-PASS 10 192.0.2.1 1024 typ relay").is_none());
        assert!(candidate("3 1 tcp-act 10 192.0.2.1 9 typ host").is_none());
        assert!(candidate("7 2 UDP 10 192.0.2.1 5001 typ host").is_none());
        assert!(candidate("7 1 udp 10 abc.local 5001 typ host").is_none());
    }

    #[test]
    fn rejects_what_is_not_sdp() {
        assert_eq!(read(""), Err(SdpError::NotSdp));
        assert_eq!(read("{\"json\":1}"), Err(SdpError::NotSdp));
        assert_eq!(read("v=0\r\ns=-\r\n"), Err(SdpError::NoMedia));
        assert_eq!(
            read("v=0\r\nm=audio x RTP/SAVP 0\r\n"),
            Err(SdpError::BadMediaLine(2))
        );
        assert_eq!(
            read("v=0\r\nm=audio 9 RTP/SAVP 0\r\n"),
            Err(SdpError::NoIceCredentials)
        );
    }

    #[test]
    fn our_offer_has_the_web_clients_shape() {
        let local = local();
        let sdp = offer(&local);
        for line in [
            "o=- 4242 2 IN IP4 127.0.0.1",
            "b=CT:4000",
            "a=group:BUNDLE 0 1 2 3",
            "m=audio 51000 RTP/SAVP 111 105 13 110 126",
            "c=IN IP4 198.51.100.5",
            "a=rtpmap:111 opus/48000/2",
            "a=x-ssrc-range:1234-1234",
            "a=x-ssrc-range:5678-5678",
            "a=x-signaling-fb:* x-message app recv:dsh",
            r"a=extmap:2 http:\\www.webrtc.org\experiments\rtp-hdrext\abs-send-time",
            r"a=extmap:3 http:\\www.ietf.org\id\draft-holmer-rmcat-transport-wide-cc-extensions-01",
            "a=label:main-audio",
            "a=label:main-video",
            "a=label:applicationsharing-video",
            "a=label:data",
            "a=ice-options:trickle",
            "a=rtcp-mux",
            "a=setup:actpass",
            "m=x-data 51000 RTP/SAVP 127 126",
            "a=x-data-protocol:sctp",
            "a=candidate:3 1 udp 58597887 198.51.100.5 51000 typ relay raddr 0.0.0.0 rport 0",
        ] {
            assert!(sdp.split("\r\n").any(|l| l == line), "missing {line}");
        }
        assert!(sdp.ends_with("\r\n"));
        assert!(!sdp.contains("UDP/TLS/RTP/SAVPF"));
        assert_eq!(sdp.matches("a=candidate:").count(), 3);

        let media = read(&sdp).expect("our offer reads back");
        assert_eq!(media.ice_ufrag, local.ice_ufrag);
        assert_eq!(media.ice_pwd, local.ice_pwd);
        assert_eq!(media.fingerprint, Some(local.fingerprint.clone()));
        assert_eq!(media.setup, Setup::ActPass);
        assert_eq!(media.opus_pt, Some(111));
        assert_eq!(media.candidates, local.candidates);
        assert_eq!(
            summary(&media),
            vec![
                (
                    "0",
                    LineKind::Audio,
                    Some("main-audio"),
                    51000,
                    Direction::SendRecv
                ),
                (
                    "1",
                    LineKind::Video,
                    Some("main-video"),
                    51000,
                    Direction::Inactive
                ),
                (
                    "2",
                    LineKind::Video,
                    Some("applicationsharing-video"),
                    51000,
                    Direction::Inactive
                ),
                (
                    "3",
                    LineKind::Data,
                    Some("data"),
                    51000,
                    Direction::SendRecv
                ),
            ]
        );
    }

    #[test]
    fn our_offer_can_leave_the_data_line_out() {
        let local = LocalMedia {
            data_ssrc: None,
            ..local()
        };
        let sdp = offer(&local);
        assert!(sdp.contains("a=group:BUNDLE 0 1 2\r\n"));
        assert!(!sdp.contains("x-data"));
        assert_eq!(read(&sdp).expect("reads back").lines.len(), 3);
    }

    #[test]
    fn our_offer_without_a_relay_uses_the_first_candidate() {
        let mut local = local();
        local.candidates.retain(|c| c.kind != CandidateKind::Relay);
        let sdp = offer(&local);
        assert!(sdp.contains("m=audio 50000 RTP/SAVP"));
        assert!(sdp.contains("c=IN IP4 192.0.2.5\r\n"));

        local.candidates.clear();
        let sdp = offer(&local);
        assert!(sdp.contains("m=audio 9 RTP/SAVP"));
        assert!(sdp.contains("c=IN IP4 0.0.0.0\r\n"));
    }

    #[test]
    fn answers_the_incoming_offer() {
        let remote = read(INCOMING_020).expect("offer 020 reads");
        let local = LocalMedia {
            opus_pt: 102,
            setup: Setup::Active,
            ..local()
        };
        let sdp = answer(&local, &remote);
        for line in [
            "a=group:BUNDLE audio_0",
            "m=audio 51000 RTP/SAVP 102",
            "a=rtpmap:102 opus/48000/2",
            "a=setup:active",
            "a=mid:audio_0",
            "a=sendrecv",
            "a=label:main-audio",
            "a=x-ssrc-range:1234-1234",
            "m=video 0 RTP/SAVP 34",
            "a=label:main-video",
        ] {
            assert!(sdp.split("\r\n").any(|l| l == line), "missing {line}");
        }
        assert_eq!(sdp.matches("a=ice-ufrag:").count(), 1);
        assert!(!sdp.contains("a=crypto"));
        assert!(!sdp.contains("a=mid:video_1"));

        let media = read(&sdp).expect("our answer reads back");
        assert_eq!(media.ice_ufrag, local.ice_ufrag);
        assert_eq!(media.setup, Setup::Active);
        assert_eq!(media.opus_pt, Some(102));
        assert_eq!(media.candidates, local.candidates);
        assert_eq!(
            summary(&media),
            vec![
                (
                    "audio_0",
                    LineKind::Audio,
                    Some("main-audio"),
                    51000,
                    Direction::SendRecv
                ),
                (
                    "1",
                    LineKind::Video,
                    Some("main-video"),
                    0,
                    Direction::Inactive
                ),
            ]
        );
    }

    #[test]
    fn the_camera_lines_h264_is_read_past_microsofts_own_codecs() {
        // The native client lists AV1 and H.264 UC first; plain H.264 at
        // packetization mode 1 is the one to use.
        let incoming = read(INCOMING_020).expect("020 reads");
        assert_eq!(
            incoming.video,
            Some(VideoCodec {
                pt: 107,
                rtx: Some(99)
            })
        );
        assert_eq!(incoming.camera().map(|l| l.mid.as_str()), Some("video_1"));
        let renegotiation = read(RENEGOTIATION_010).expect("010 reads");
        assert_eq!(
            renegotiation.video,
            Some(VideoCodec {
                pt: 102,
                rtx: Some(103)
            })
        );
    }

    #[test]
    fn with_a_camera_its_line_is_offered_and_answered() {
        let local = LocalMedia {
            video_ssrc: Some(77),
            ..local()
        };
        let ours = read(&offer(&local)).expect("our offer reads back");
        let camera = ours.camera().expect("a camera line");
        assert_eq!(camera.mid, "1");
        assert_eq!(camera.direction, Direction::SendRecv);
        assert_eq!(
            ours.video,
            Some(VideoCodec {
                pt: 108,
                rtx: Some(109)
            })
        );
        assert!(offer(&local).contains("a=x-ssrc-range:77-77\r\n"));

        // To the native client's offer: its payload type and mid, both
        // ways.
        let sdp = answer(&local, &read(INCOMING_020).expect("020 reads"));
        let answered = read(&sdp).expect("our answer reads back");
        let camera = answered.camera().expect("the camera line kept");
        assert_eq!(camera.mid, "video_1");
        assert_ne!(camera.port, 0);
        assert_eq!(camera.direction, Direction::SendRecv);
        assert_eq!(
            answered.video,
            Some(VideoCodec {
                pt: 107,
                rtx: Some(99)
            }),
            "lost packets asked for again on the offer's rtx"
        );
        assert!(sdp.contains("a=rtcp-fb:* nack\r\n"));
        assert!(sdp.contains("a=group:BUNDLE audio_0 video_1"));

        // A far end that only receives (010): we only send.
        let sdp = answer(&local, &read(RENEGOTIATION_010).expect("010 reads"));
        let camera = read(&sdp)
            .expect("reads back")
            .camera()
            .cloned()
            .expect("kept");
        assert_eq!(camera.direction, Direction::SendOnly);
    }

    #[test]
    fn answers_renegotiation_offers_line_for_line() {
        for (fixture, data) in [(RENEGOTIATION_010, None), (RENEGOTIATION_029, Some(99))] {
            let remote = read(fixture).expect("fixture reads");
            let local = LocalMedia {
                data_ssrc: data,
                ..local()
            };
            let sdp = answer(&local, &remote);
            let media = read(&sdp).expect("our answer reads back");
            assert_eq!(media.lines.len(), remote.lines.len());
            for (ours, theirs) in media.lines.iter().zip(&remote.lines) {
                assert_eq!(ours.kind, theirs.kind);
                assert_eq!(ours.label, theirs.label);
                // Kept lines keep the offer's mid; rejected ones have none
                // and read back as their position, as the offer's do.
                assert_eq!(
                    ours.port == 0,
                    ours.kind == LineKind::Video || (ours.kind == LineKind::Data && data.is_none())
                );
                if ours.port != 0 {
                    assert_eq!(ours.mid, theirs.mid);
                }
            }
            assert_eq!(media.setup, Setup::Active);
            assert_eq!(media.opus_pt, remote.opus_pt);
            assert_eq!(sdp.matches("a=candidate:").count(), 3);
        }

        let sdp = answer(
            &LocalMedia {
                data_ssrc: Some(99),
                ..local()
            },
            &read(RENEGOTIATION_029).expect("029 reads"),
        );
        assert!(sdp.contains("a=group:BUNDLE audio_0 data_2\r\n"));
        assert!(sdp.contains("a=mid:data_2\r\n"));
        assert!(sdp.contains("a=x-ssrc-range:99-99\r\n"));
    }

    #[test]
    fn answers_the_far_ends_direction() {
        assert_eq!(
            answer_direction(Direction::SendRecv, Direction::SendOnly),
            Direction::RecvOnly
        );
        assert_eq!(
            answer_direction(Direction::SendRecv, Direction::RecvOnly),
            Direction::SendOnly
        );
        assert_eq!(
            answer_direction(Direction::SendOnly, Direction::SendOnly),
            Direction::Inactive
        );
        assert_eq!(
            answer_direction(Direction::SendRecv, Direction::SendRecv),
            Direction::SendRecv
        );
    }

    #[test]
    fn we_answer_as_the_dtls_client() {
        assert_eq!(answer_setup(Setup::Unsaid), Setup::Active);
        assert_eq!(answer_setup(Setup::ActPass), Setup::Active);
        assert_eq!(answer_setup(Setup::Passive), Setup::Active);
        assert_eq!(answer_setup(Setup::Active), Setup::Passive);
    }

    #[test]
    fn an_offer_without_opus_has_its_audio_rejected() {
        let mut remote = read(ANSWER_001).expect("001 reads");
        remote.opus_pt = None;
        let sdp = answer(&local(), &remote);
        assert!(sdp.contains("m=audio 0 RTP/SAVP 0\r\n"));
    }
}
