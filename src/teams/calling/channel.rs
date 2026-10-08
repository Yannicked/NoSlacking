//! A meeting's data channel, as the web client speaks it
//! (`docs/research/teams-calls.md` §H.9): a WebRTC data channel named
//! `main-channel` over the call's DTLS, on which the meeting's media
//! server says how much it can send and who speaks, and on which we ask
//! for someone's camera.
//!
//! Every message is a 16-byte header and a JSON array:
//!
//! - the bytes `10 0f 92 00`;
//! - a sequence number, little-endian, counted each way from 0;
//! - who sends it and to whom, each a big-endian 32-bit id followed by
//!   `01`. The server is -4. We are -2 until its `ack` names our id (its
//!   header's addressee).
//!
//! We send a `syn` with our capabilities, and the server answers `ack`.
//! Then an `sr` (source request) asks for one participant's video by its
//! roster `sourceId`, on the receiving stream `streamMsid` (the
//! `x-source-streamid` of our camera's line in the meeting's SDP), or
//! `sourceId` -1 for none. Each `sr` has an odd `sequenceNumber`, two above
//! the last, and is answered `sr_res`.

/// The channel's label.
pub const LABEL: &str = "main-channel";

/// The header's first bytes.
const MAGIC: [u8; 4] = [0x10, 0x0f, 0x92, 0x00];
/// The header's length.
const HEADER: usize = 16;
/// The meeting's media server's id.
pub const SERVER: i32 = -4;
/// Our id until the server gives one.
const UNNAMED: i32 = -2;
/// What we ask for in an `sr`: H.264 up to 1080p, as the web client
/// asks (recorded).
const VIDEO_FORMAT: &str =
    r#"{"max-fs":8160,"max-mbps":244800,"max-fps":3000,"profile-level-id":"64001f"}"#;

/// One message of the channel: its header's parts and its messages.
#[derive(Clone, Debug, PartialEq)]
pub struct Frame {
    pub seq: u16,
    pub from: i32,
    pub to: i32,
    pub messages: Vec<serde_json::Value>,
}

/// Writes one message.
pub fn encode(seq: u16, from: i32, to: i32, messages: &[serde_json::Value]) -> Vec<u8> {
    let mut out = Vec::with_capacity(HEADER + 64);
    out.extend_from_slice(&MAGIC);
    out.extend_from_slice(&seq.to_le_bytes());
    out.extend_from_slice(&from.to_be_bytes());
    out.push(1);
    out.extend_from_slice(&to.to_be_bytes());
    out.push(1);
    let body = serde_json::Value::Array(messages.to_vec()).to_string();
    out.extend_from_slice(body.as_bytes());
    out
}

/// Reads one message; `None` for one not in this shape.
pub fn decode(bytes: &[u8]) -> Option<Frame> {
    if bytes.len() < HEADER || bytes[..4] != MAGIC {
        return None;
    }
    let id = |at: usize| -> Option<i32> {
        Some(i32::from_be_bytes(bytes.get(at..at + 4)?.try_into().ok()?))
    };
    let seq = u16::from_le_bytes([bytes[4], bytes[5]]);
    let (from, to) = (id(6)?, id(11)?);
    let messages = match serde_json::from_slice(&bytes[HEADER..]).ok()? {
        serde_json::Value::Array(messages) => messages,
        one => vec![one],
    };
    Some(Frame {
        seq,
        from,
        to,
        messages,
    })
}

/// What a message from the server says that a call acts on.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Heard {
    /// The handshake is done: we may ask for video.
    Ready,
    /// A source request of ours was answered.
    SourceAnswered { sequence: u64, ok: bool },
    /// Who spoke last, by their audio's source id, newest first.
    Speakers(Vec<i64>),
}

/// Our side of the channel: the numbering of what we send, and our id.
#[derive(Debug, Default)]
pub struct Channel {
    /// The next message's sequence number.
    seq: u16,
    /// Our id, once the server's `ack` named it.
    us: Option<i32>,
    /// The last source request's number.
    request: u64,
}

impl Channel {
    /// Our `syn`, which opens the handshake: with the web client's
    /// capabilities (recorded).
    pub fn syn(&mut self) -> Vec<u8> {
        let message = serde_json::json!({
            "type": "syn",
            "client_capabilities": ["dsh", "bwe", "sr", "ssbwe"],
        });
        self.frame(UNNAMED, &[message])
    }

    /// Whether the handshake is done.
    pub fn ready(&self) -> bool {
        self.us.is_some()
    }

    /// Takes in what the server sent.
    pub fn heard(&mut self, bytes: &[u8]) -> Vec<Heard> {
        let Some(frame) = decode(bytes) else {
            log::debug!("meeting channel: a message in an unknown shape");
            return Vec::new();
        };
        let mut heard = Vec::new();
        for message in &frame.messages {
            match message.get("type").and_then(|t| t.as_str()) {
                Some("ack") => {
                    self.us = Some(frame.to);
                    heard.push(Heard::Ready);
                }
                Some("sr_res") => heard.push(Heard::SourceAnswered {
                    sequence: message
                        .get("sequenceNumber")
                        .and_then(serde_json::Value::as_u64)
                        .unwrap_or_default(),
                    ok: message.get("result").and_then(|r| r.as_str()) == Some("ok"),
                }),
                Some("dsh") => heard.push(Heard::Speakers(
                    message
                        .get("history")
                        .and_then(|h| h.as_array())
                        .map(|h| h.iter().filter_map(serde_json::Value::as_i64).collect())
                        .unwrap_or_default(),
                )),
                // Bandwidth estimates and heartbeats.
                _ => {}
            }
        }
        heard
    }

    /// A source request: `source`'s video (a roster `sourceId`, or none)
    /// on our receiving stream `stream`. `None` before the handshake is
    /// done.
    pub fn request_video(&mut self, source: Option<i64>, stream: u32) -> Option<Vec<u8>> {
        let us = self.us?;
        self.request = if self.request == 0 {
            1
        } else {
            self.request + 2
        };
        let format: serde_json::Value = serde_json::from_str(VIDEO_FORMAT).unwrap_or_default();
        let message = serde_json::json!({
            "type": "sr",
            "controlVideoStreaming": {
                "sequenceNumber": self.request,
                "controlInfo": {
                    "sourceId": source.unwrap_or(-1),
                    "streamMsid": stream,
                    "fmtParams": [format],
                },
            },
        });
        Some(self.frame(us, &[message]))
    }

    fn frame(&mut self, from: i32, messages: &[serde_json::Value]) -> Vec<u8> {
        let bytes = encode(self.seq, from, SERVER, messages);
        self.seq = self.seq.wrapping_add(1);
        bytes
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(text: &str) -> Vec<u8> {
        (0..text.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&text[i..i + 2], 16).expect("hex"))
            .collect()
    }

    #[test]
    fn the_recorded_handshake_and_request_read_and_write_alike() {
        // The web client's syn, as recorded.
        let syn = hex(concat!(
            "100f92000000fffffffe01fffffffc01",
            "5b7b2274797065223a2273796e222c22636c69656e745f6361706162696c6974696573223a",
            "5b22647368222c22627765222c227372222c227373627765225d7d5d"
        ));
        let mut channel = Channel::default();
        let ours = channel.syn();
        assert_eq!(ours[..HEADER], syn[..HEADER]);
        assert_eq!(decode(&ours), decode(&syn));
        assert!(!channel.ready());
        assert_eq!(channel.request_video(Some(202), 404), None);

        // The server's ack names us 415.
        let ack = hex(concat!(
            "100f92000000fffffffc010000019f01",
            "5b7b2274797065223a2261636b227d5d"
        ));
        assert_eq!(channel.heard(&ack), vec![Heard::Ready]);
        assert!(channel.ready());

        // The first request, as recorded but for its JSON's key order.
        let request = channel.request_video(Some(202), 404).expect("ready");
        assert_eq!(
            request[..HEADER],
            hex("100f920001000000019f01fffffffc01")[..]
        );
        let frame = decode(&request).expect("reads");
        let info = &frame.messages[0]["controlVideoStreaming"];
        assert_eq!(frame.messages[0]["type"], "sr");
        assert_eq!(info["sequenceNumber"], 1);
        assert_eq!(info["controlInfo"]["sourceId"], 202);
        assert_eq!(info["controlInfo"]["streamMsid"], 404);
        assert_eq!(
            info["controlInfo"]["fmtParams"][0]["profile-level-id"],
            "64001f"
        );
        // Then none, numbered two on.
        let none = decode(&channel.request_video(None, 404).expect("ready")).expect("reads");
        assert_eq!(none.seq, 2);
        let info = &none.messages[0]["controlVideoStreaming"];
        assert_eq!(info["sequenceNumber"], 3);
        assert_eq!(info["controlInfo"]["sourceId"], -1);
    }

    #[test]
    fn the_servers_answers_and_news_are_read() {
        let mut channel = Channel::default();
        let answered = encode(
            6,
            SERVER,
            415,
            &[serde_json::json!({"result": "ok", "sequenceNumber": 1, "type": "sr_res"})],
        );
        assert_eq!(
            channel.heard(&answered),
            vec![Heard::SourceAnswered {
                sequence: 1,
                ok: true
            }]
        );
        let speakers = encode(
            2,
            SERVER,
            415,
            &[serde_json::json!({"history": [403], "type": "dsh"})],
        );
        assert_eq!(channel.heard(&speakers), vec![Heard::Speakers(vec![403])]);
        let estimate = encode(
            1,
            SERVER,
            415,
            &[serde_json::json!({"bw": 613464, "type": "bwe"})],
        );
        assert!(channel.heard(&estimate).is_empty());
        assert!(channel.heard(b"not a frame").is_empty());
    }
}
