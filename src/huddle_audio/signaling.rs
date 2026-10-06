//! The signaling session: Chime's WebSocket, and the join that runs over
//! it as a state machine of plain data.
//!
//! The order is the JS SDK's (`JoinAndReceiveIndexTask`, then
//! `SubscribeAndReceiveSubscribeAckTask`) and HuddleFM's
//! (`src/native-media/chime-link.ts`, `connect`):
//!
//! 1. open the socket to `SignalingUrl` with the join token as the second
//!    subprotocol after `_aws_wt_session`;
//! 2. JOIN; Chime answers JOIN_ACK with the TURN credentials, then INDEX
//!    (the video sources; HuddleFM waits for it five seconds at most);
//! 3. SUBSCRIBE with the SDP offer; SUBSCRIBE_ACK carries the answer;
//! 4. from then on AUDIO_STATUS, attendee presence (AUDIO_STREAM_ID_INFO),
//!    volumes (AUDIO_METADATA) and pings, which are answered; the server
//!    sends BITRATES about every four seconds, and HuddleFM pings every
//!    ten;
//! 5. LEAVE, answered by LEAVE_ACK.
//!
//! [`Handshake`] holds where the session is and answers each frame with
//! [`Step`]s for the driver (the media module) to carry out, so the whole
//! exchange is tested without a socket.

use std::collections::BTreeMap;

use futures_util::{SinkExt as _, StreamExt as _};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest as _;
use tokio_tungstenite::tungstenite::http::HeaderValue;

use super::chime::{self, AudioStatus, Frame, FrameType, proto};
use super::join::JoinToken;

/// The close code Chime gives when the meeting has ended (the JS SDK's
/// `JoinAndReceiveIndexTask`).
pub const MEETING_ENDED_CLOSE: u16 = 4410;

/// A TURN server's credentials from JOIN_ACK. The password is shown
/// nowhere; the username, which names the attendee, neither.
#[derive(Clone, PartialEq, Eq)]
pub struct TurnCredentials {
    /// The TURN username.
    pub username: String,
    /// The TURN password.
    pub password: String,
    /// How long they last, in seconds.
    pub ttl: Option<u32>,
    /// `turn:host:port?transport=udp`, `turns:host:443?transport=tcp`, …
    pub uris: Vec<String>,
}

impl std::fmt::Debug for TurnCredentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TurnCredentials")
            .field("username", &crate::redact::REDACTED)
            .field("password", &crate::redact::REDACTED)
            .field("ttl", &self.ttl)
            .field("uris", &self.uris)
            .finish()
    }
}

/// Where the session is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Phase {
    /// Before JOIN went out.
    Idle,
    /// JOIN sent, waiting for JOIN_ACK.
    Joining,
    /// JOIN_ACK came; waiting for the first INDEX.
    Indexing,
    /// Ready for the offer; the driver is making it.
    Offering,
    /// SUBSCRIBE sent, waiting for SUBSCRIBE_ACK.
    Subscribing,
    /// Media is negotiated.
    Live,
    /// LEAVE sent.
    Leaving,
    /// Done, one way or another.
    Over,
}

/// An attendee Chime says is here, by audio stream.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Attendee {
    /// Chime's id for them.
    pub attendee_id: String,
    /// Slack's user id, as Chime knows it.
    pub external_user_id: Option<String>,
    /// Whether their microphone is muted.
    pub muted: bool,
}

/// How the session ended.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Ending {
    /// LEAVE_ACK came: left cleanly.
    Left,
    /// A frame carried an error while the join was under way.
    Refused {
        /// Where the join was.
        phase: Phase,
        /// The frame's type.
        frame: String,
        /// Its error status.
        status: u32,
        /// Its error description.
        description: String,
    },
    /// AUDIO_STATUS ended it.
    Audio(AudioStatus),
    /// JOIN_ACK had no TURN servers: Chime's media is out of reach.
    NoTurn,
    /// SUBSCRIBE_ACK had no SDP answer (or only a compressed one, which
    /// was not asked for).
    NoAnswer,
    /// The socket closed; 4410 means the meeting had ended.
    Closed {
        /// The WebSocket close code.
        code: u16,
        /// The close reason.
        reason: String,
    },
}

impl std::fmt::Display for Ending {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Left => f.write_str("left"),
            Self::Refused {
                phase,
                frame,
                status,
                description,
            } => write!(
                f,
                "Chime refused while {phase:?}: {frame} error {status} {description:?}"
            ),
            Self::Audio(status) => write!(f, "Chime audio status: {status:?}"),
            Self::NoTurn => f.write_str("JOIN_ACK carried no TURN servers"),
            Self::NoAnswer => f.write_str("SUBSCRIBE_ACK carried no SDP answer"),
            Self::Closed { code, reason } if *code == MEETING_ENDED_CLOSE => {
                write!(f, "the meeting has ended (close {code} {reason:?})")
            }
            Self::Closed { code, reason } => {
                write!(f, "signaling closed: {code} {reason:?}")
            }
        }
    }
}

/// What the driver does next.
#[derive(Clone, Debug, PartialEq)]
pub enum Step {
    /// Send this frame.
    Send(Box<Frame>),
    /// JOIN_ACK's TURN servers: allocate a relay on one.
    Turn(TurnCredentials),
    /// Make the SDP offer and hand it to [`Handshake::subscribe`].
    Offer,
    /// SUBSCRIBE_ACK's SDP answer.
    Answer(String),
    /// Someone came or went (`present`), or was muted or unmuted.
    Presence {
        /// Who.
        attendee: Attendee,
        /// Whether they are here now.
        present: bool,
    },
    /// Something worth a line in the log that ends nothing.
    Note(String),
    /// The session is over.
    Over(Ending),
}

/// The join, as data.
#[derive(Debug)]
pub struct Handshake {
    phase: Phase,
    audio_session_id: u32,
    details: chime::ClientDetails,
    attendees: BTreeMap<u32, Attendee>,
}

impl Handshake {
    /// A session for one attendee; `audio_session_id` is random and kept
    /// for the attendee's stay.
    pub fn new(audio_session_id: u32) -> Self {
        Self {
            phase: Phase::Idle,
            audio_session_id,
            details: chime::ClientDetails::default(),
            attendees: BTreeMap::new(),
        }
    }

    /// Where the session is.
    pub fn phase(&self) -> Phase {
        self.phase
    }

    /// The attendees Chime said are here, by audio stream id.
    pub fn attendees(&self) -> &BTreeMap<u32, Attendee> {
        &self.attendees
    }

    /// Sends JOIN.
    pub fn start(&mut self, now_ms: u64) -> Vec<Step> {
        self.phase = Phase::Joining;
        vec![Step::Send(Box::new(chime::join(
            &self.details,
            self.audio_session_id,
            now_ms,
        )))]
    }

    /// No INDEX came in time; go on without it, as HuddleFM does.
    pub fn index_timed_out(&mut self) -> Vec<Step> {
        if self.phase != Phase::Indexing {
            return Vec::new();
        }
        self.phase = Phase::Offering;
        vec![
            Step::Note("no INDEX in time; subscribing anyway".into()),
            Step::Offer,
        ]
    }

    /// Sends SUBSCRIBE with the offer.
    pub fn subscribe(&mut self, sub: &chime::Subscribe, now_ms: u64) -> Vec<Step> {
        if self.phase != Phase::Offering {
            return vec![Step::Note(format!(
                "an offer while {:?}; not subscribing",
                self.phase
            ))];
        }
        self.phase = Phase::Subscribing;
        vec![Step::Send(Box::new(chime::subscribe(sub, now_ms)))]
    }

    /// Sends LEAVE, unless the session is already over.
    pub fn leave(&mut self, now_ms: u64) -> Vec<Step> {
        match self.phase {
            Phase::Over | Phase::Leaving => Vec::new(),
            // Nothing joined yet: there is nothing to leave.
            Phase::Idle => {
                self.phase = Phase::Over;
                vec![Step::Over(Ending::Left)]
            }
            _ => {
                self.phase = Phase::Leaving;
                vec![Step::Send(Box::new(chime::leave(now_ms)))]
            }
        }
    }

    /// The socket closed.
    pub fn closed(&mut self, code: u16, reason: &str) -> Vec<Step> {
        if self.phase == Phase::Over {
            return Vec::new();
        }
        let leaving = self.phase == Phase::Leaving;
        self.phase = Phase::Over;
        // A close right after LEAVE, before LEAVE_ACK, is leaving too.
        let ending = if leaving {
            Ending::Left
        } else {
            Ending::Closed {
                code,
                reason: reason.to_owned(),
            }
        };
        vec![Step::Over(ending)]
    }

    /// Answers one frame from Chime.
    pub fn on_frame(&mut self, frame: &Frame, now_ms: u64) -> Vec<Step> {
        if self.phase == Phase::Over {
            return Vec::new();
        }
        let kind = FrameType::try_from(frame.r#type).ok();
        let mut steps = Vec::new();
        // Pings are answered whatever else is going on.
        if let (Some(FrameType::PingPong), Some(ping)) = (kind, &frame.ping_pong)
            && ping.r#type == proto::SdkPingPongType::Ping as i32
        {
            steps.push(Step::Send(Box::new(chime::ping_pong(
                proto::SdkPingPongType::Pong,
                ping.ping_id,
                now_ms,
            ))));
        }
        // An error while joining ends the join, unless it rides on the
        // awaited answer itself (SUBSCRIBE_ACK reports view-only video,
        // 206, next to a usable answer).
        if let Some(error) = &frame.error
            && kind != Some(FrameType::PingPong)
        {
            let status = error.status.unwrap_or_default();
            let awaited = matches!(
                (self.phase, kind),
                (Phase::Joining, Some(FrameType::JoinAck))
                    | (Phase::Subscribing, Some(FrameType::SubscribeAck))
            );
            let joining = matches!(
                self.phase,
                Phase::Joining | Phase::Indexing | Phase::Subscribing
            );
            if joining && !awaited && !(200..300).contains(&status) {
                let ending = Ending::Refused {
                    phase: self.phase,
                    frame: chime::type_name(frame),
                    status,
                    description: error.description.clone().unwrap_or_default(),
                };
                self.phase = Phase::Over;
                steps.push(Step::Over(ending));
                return steps;
            }
        }
        match kind {
            Some(FrameType::JoinAck) if self.phase == Phase::Joining => {
                let turn = frame
                    .joinack
                    .as_ref()
                    .and_then(|ack| ack.turn_credentials.as_ref())
                    .filter(|turn| !turn.uris.is_empty());
                match turn {
                    Some(turn) => {
                        self.phase = Phase::Indexing;
                        steps.push(Step::Turn(TurnCredentials {
                            username: turn.username.clone().unwrap_or_default(),
                            password: turn.password.clone().unwrap_or_default(),
                            ttl: turn.ttl,
                            uris: turn.uris.clone(),
                        }));
                    }
                    None => {
                        self.phase = Phase::Over;
                        steps.push(Step::Over(Ending::NoTurn));
                    }
                }
            }
            Some(FrameType::Index) if self.phase == Phase::Indexing => {
                self.phase = Phase::Offering;
                steps.push(Step::Offer);
            }
            Some(FrameType::SubscribeAck) if self.phase == Phase::Subscribing => {
                match frame
                    .suback
                    .as_ref()
                    .and_then(|ack| ack.sdp_answer.clone())
                    .filter(|sdp| !sdp.is_empty())
                {
                    Some(answer) => {
                        self.phase = Phase::Live;
                        steps.push(Step::Answer(answer));
                    }
                    None => {
                        self.phase = Phase::Over;
                        steps.push(Step::Over(Ending::NoAnswer));
                    }
                }
            }
            Some(FrameType::LeaveAck) if self.phase == Phase::Leaving => {
                self.phase = Phase::Over;
                steps.push(Step::Over(Ending::Left));
            }
            Some(FrameType::AudioStatus) => {
                let code = frame
                    .audio_status
                    .as_ref()
                    .and_then(|s| s.audio_status)
                    .unwrap_or_default();
                let status = AudioStatus::of(code);
                if status.is_final() && self.phase != Phase::Leaving {
                    self.phase = Phase::Over;
                    steps.push(Step::Over(Ending::Audio(status)));
                } else if status != AudioStatus::Ok {
                    steps.push(Step::Note(format!("audio status {code}: {status:?}")));
                }
            }
            Some(FrameType::AudioStreamIdInfo) => {
                if let Some(info) = &frame.audio_stream_id_info {
                    for stream in &info.streams {
                        steps.extend(self.stream_info(stream));
                    }
                }
            }
            _ => {}
        }
        steps
    }

    /// Keeps the attendee list up to date from one stream's news: a new
    /// attendee, one muted or unmuted, or one dropped.
    fn stream_info(&mut self, stream: &proto::SdkAudioStreamIdInfo) -> Option<Step> {
        let id = stream.audio_stream_id?;
        if stream.dropped.unwrap_or_default() {
            let attendee = self.attendees.remove(&id)?;
            return Some(Step::Presence {
                attendee,
                present: false,
            });
        }
        match (self.attendees.get_mut(&id), &stream.attendee_id) {
            // Later frames for a known stream may carry only `muted`.
            (Some(known), _) => {
                let muted = stream.muted?;
                if known.muted == muted {
                    return None;
                }
                known.muted = muted;
                Some(Step::Presence {
                    attendee: known.clone(),
                    present: true,
                })
            }
            (None, Some(attendee_id)) => {
                let attendee = Attendee {
                    attendee_id: attendee_id.clone(),
                    external_user_id: stream.external_user_id.clone(),
                    muted: stream.muted.unwrap_or_default(),
                };
                self.attendees.insert(id, attendee.clone());
                Some(Step::Presence {
                    attendee,
                    present: true,
                })
            }
            (None, None) => None,
        }
    }
}

/// Why the signaling socket did not open.
#[derive(Debug, thiserror::Error)]
pub enum OpenError {
    /// The URL or the token cannot make a request.
    #[error("the signaling URL is not usable: {0}")]
    Url(String),
    /// The connection or the WebSocket handshake failed.
    #[error("{0}")]
    Socket(#[from] tokio_tungstenite::tungstenite::Error),
}

/// One thing off the socket.
#[derive(Debug)]
pub enum Incoming {
    /// A frame.
    Frame(Box<Frame>),
    /// A message that is not a frame this protocol reads; skipped, as the
    /// JS SDK skips them.
    Undecodable {
        /// Its length.
        bytes: usize,
        /// Why it does not read.
        error: chime::FrameError,
    },
    /// The socket closed (or failed: code 1006).
    Closed {
        /// The WebSocket close code.
        code: u16,
        /// The close reason.
        reason: String,
    },
}

/// Chime's signaling WebSocket.
pub struct Socket {
    inner: crate::slack::net::Socket,
}

impl std::fmt::Debug for Socket {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Socket")
    }
}

/// The subprotocol header: `_aws_wt_session, <join token>`, marked
/// sensitive so nothing prints it.
fn protocols(token: &JoinToken) -> Result<HeaderValue, OpenError> {
    let mut value = HeaderValue::from_str(&format!("{}, {}", chime::SUBPROTOCOL, token.expose()))
        .map_err(|_| OpenError::Url("the join token is not a header value".into()))?;
    value.set_sensitive(true);
    Ok(value)
}

impl Socket {
    /// Opens the socket to `signaling_url` (`MediaPlacement.SignalingUrl`)
    /// through the app's proxy settings.
    pub async fn open(signaling_url: &str, token: &JoinToken) -> Result<Self, OpenError> {
        let mut request = chime::signaling_url(signaling_url)
            .into_client_request()
            .map_err(|e| OpenError::Url(e.to_string()))?;
        request
            .headers_mut()
            .insert("Sec-WebSocket-Protocol", protocols(token)?);
        let (inner, _) = crate::slack::net::websocket(request).await?;
        Ok(Self { inner })
    }

    /// Sends one frame.
    pub async fn send(
        &mut self,
        frame: &Frame,
    ) -> Result<(), tokio_tungstenite::tungstenite::Error> {
        self.inner
            .send(Message::Binary(chime::encode(frame).into()))
            .await
    }

    /// The next frame, or how the socket ended.
    pub async fn next(&mut self) -> Incoming {
        loop {
            match self.inner.next().await {
                Some(Ok(Message::Binary(bytes))) => {
                    return match chime::decode(&bytes) {
                        Ok(frame) => Incoming::Frame(Box::new(frame)),
                        Err(error) => Incoming::Undecodable {
                            bytes: bytes.len(),
                            error,
                        },
                    };
                }
                Some(Ok(Message::Close(close))) => {
                    return close.map_or(
                        Incoming::Closed {
                            code: 1005,
                            reason: String::new(),
                        },
                        |close| Incoming::Closed {
                            code: close.code.into(),
                            reason: close.reason.to_string(),
                        },
                    );
                }
                // Text, WebSocket pings (tungstenite answers them) and
                // pongs carry nothing for us.
                Some(Ok(_)) => {}
                Some(Err(error)) => {
                    return Incoming::Closed {
                        code: 1006,
                        reason: error.to_string(),
                    };
                }
                None => {
                    return Incoming::Closed {
                        code: 1006,
                        reason: "the connection ended".into(),
                    };
                }
            }
        }
    }

    /// Closes the socket, politely.
    pub async fn close(&mut self) {
        let _ = self.inner.close(None).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ack_with_turn(uris: &[&str]) -> Frame {
        let mut frame = chime::frame(FrameType::JoinAck, 2);
        frame.joinack = Some(proto::SdkJoinAckFrame {
            turn_credentials: Some(proto::SdkTurnCredentials {
                username: Some("u".into()),
                password: Some("p".into()),
                ttl: Some(300),
                uris: uris.iter().map(|u| (*u).to_owned()).collect(),
            }),
            ..Default::default()
        });
        frame
    }

    fn sub() -> chime::Subscribe {
        chime::Subscribe {
            sdp_offer: "v=0\r\n".into(),
            audio_host: "h:3478".into(),
            attendee_id: "A1".into(),
            muted: true,
        }
    }

    fn sent_type(steps: &[Step]) -> Vec<String> {
        steps
            .iter()
            .filter_map(|s| match s {
                Step::Send(frame) => Some(chime::type_name(frame)),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn a_join_runs_from_join_to_answer_to_leave() {
        let mut session = Handshake::new(7);
        assert_eq!(sent_type(&session.start(1)), ["JOIN"]);
        assert_eq!(session.phase(), Phase::Joining);

        let steps = session.on_frame(&ack_with_turn(&["turn:t:3478?transport=udp"]), 2);
        assert!(matches!(&steps[..], [Step::Turn(turn)] if turn.uris.len() == 1));
        assert_eq!(session.phase(), Phase::Indexing);

        let steps = session.on_frame(&chime::frame(FrameType::Index, 3), 3);
        assert_eq!(steps, vec![Step::Offer]);

        assert_eq!(sent_type(&session.subscribe(&sub(), 4)), ["SUBSCRIBE"]);
        assert_eq!(session.phase(), Phase::Subscribing);

        let mut ack = chime::frame(FrameType::SubscribeAck, 5);
        ack.suback = Some(proto::SdkSubscribeAckFrame {
            sdp_answer: Some("v=0 answer".into()),
            ..Default::default()
        });
        assert_eq!(
            session.on_frame(&ack, 5),
            vec![Step::Answer("v=0 answer".into())]
        );
        assert_eq!(session.phase(), Phase::Live);

        assert_eq!(sent_type(&session.leave(6)), ["LEAVE"]);
        assert_eq!(
            session.on_frame(&chime::frame(FrameType::LeaveAck, 7), 7),
            vec![Step::Over(Ending::Left)]
        );
        assert_eq!(session.phase(), Phase::Over);
        assert!(session.leave(8).is_empty());
    }

    #[test]
    fn without_an_index_the_join_goes_on() {
        let mut session = Handshake::new(1);
        session.start(1);
        assert!(session.index_timed_out().is_empty(), "not yet acked");
        session.on_frame(&ack_with_turn(&["turn:t:3478"]), 2);
        let steps = session.index_timed_out();
        assert_eq!(steps.last(), Some(&Step::Offer));
        assert_eq!(session.phase(), Phase::Offering);
    }

    #[test]
    fn pings_are_answered_with_their_id() {
        let mut session = Handshake::new(1);
        let ping = chime::ping_pong(proto::SdkPingPongType::Ping, 77, 1);
        let steps = session.on_frame(&ping, 2);
        let [Step::Send(pong)] = &steps[..] else {
            panic!("{steps:?}");
        };
        let body = pong.ping_pong.expect("a pong");
        assert_eq!(body.r#type, proto::SdkPingPongType::Pong as i32);
        assert_eq!(body.ping_id, 77);
        // A pong needs no answer.
        let pong = chime::ping_pong(proto::SdkPingPongType::Pong, 3, 1);
        assert!(session.on_frame(&pong, 2).is_empty());
    }

    #[test]
    fn a_join_ack_without_turn_ends_the_join() {
        let mut session = Handshake::new(1);
        session.start(1);
        assert_eq!(
            session.on_frame(&ack_with_turn(&[]), 2),
            vec![Step::Over(Ending::NoTurn)]
        );
    }

    #[test]
    fn errors_while_joining_end_it_and_say_why() {
        let mut session = Handshake::new(1);
        session.start(1);
        let mut refused = chime::frame(FrameType::Index, 2);
        refused.error = Some(proto::SdkErrorFrame {
            status: Some(403),
            description: Some("bad token".into()),
        });
        let steps = session.on_frame(&refused, 2);
        assert_eq!(
            steps,
            vec![Step::Over(Ending::Refused {
                phase: Phase::Joining,
                frame: "INDEX".into(),
                status: 403,
                description: "bad token".into(),
            })]
        );
        assert_eq!(session.phase(), Phase::Over);
    }

    #[test]
    fn a_view_only_warning_rides_with_a_usable_answer() {
        let mut session = Handshake::new(1);
        session.start(1);
        session.on_frame(&ack_with_turn(&["turn:t:1"]), 2);
        session.on_frame(&chime::frame(FrameType::Index, 3), 3);
        session.subscribe(&sub(), 4);
        let mut ack = chime::frame(FrameType::SubscribeAck, 5);
        ack.error = Some(proto::SdkErrorFrame {
            status: Some(206),
            description: None,
        });
        ack.suback = Some(proto::SdkSubscribeAckFrame {
            sdp_answer: Some("answer".into()),
            ..Default::default()
        });
        assert_eq!(
            session.on_frame(&ack, 5),
            vec![Step::Answer("answer".into())]
        );
    }

    #[test]
    fn final_audio_statuses_end_the_session() {
        let mut session = Handshake::new(1);
        session.start(1);
        let mut status = chime::frame(FrameType::AudioStatus, 2);
        status.audio_status = Some(proto::SdkAudioStatusFrame {
            audio_status: Some(200),
        });
        assert!(session.on_frame(&status, 2).is_empty());
        status.audio_status = Some(proto::SdkAudioStatusFrame {
            audio_status: Some(410),
        });
        assert_eq!(
            session.on_frame(&status, 3),
            vec![Step::Over(Ending::Audio(AudioStatus::MeetingEnded))]
        );
    }

    #[test]
    fn presence_follows_stream_info() {
        let mut session = Handshake::new(1);
        let info = |streams: Vec<proto::SdkAudioStreamIdInfo>| {
            let mut frame = chime::frame(FrameType::AudioStreamIdInfo, 1);
            frame.audio_stream_id_info = Some(proto::SdkAudioStreamIdInfoFrame { streams });
            frame
        };
        let steps = session.on_frame(
            &info(vec![proto::SdkAudioStreamIdInfo {
                audio_stream_id: Some(3),
                attendee_id: Some("A3".into()),
                external_user_id: Some("U3".into()),
                muted: Some(false),
                dropped: None,
            }]),
            1,
        );
        assert!(matches!(
            &steps[..],
            [Step::Presence { attendee, present: true }] if attendee.attendee_id == "A3"
        ));
        let steps = session.on_frame(
            &info(vec![proto::SdkAudioStreamIdInfo {
                audio_stream_id: Some(3),
                muted: Some(true),
                ..Default::default()
            }]),
            2,
        );
        assert!(matches!(
            &steps[..],
            [Step::Presence { attendee, present: true }] if attendee.muted
        ));
        let steps = session.on_frame(
            &info(vec![proto::SdkAudioStreamIdInfo {
                audio_stream_id: Some(3),
                dropped: Some(true),
                ..Default::default()
            }]),
            3,
        );
        assert!(matches!(
            &steps[..],
            [Step::Presence { present: false, .. }]
        ));
        assert!(session.attendees().is_empty());
    }

    #[test]
    fn a_close_says_whether_the_meeting_ended() {
        let mut session = Handshake::new(1);
        session.start(1);
        let steps = session.closed(MEETING_ENDED_CLOSE, "");
        let [Step::Over(ending)] = &steps[..] else {
            panic!("{steps:?}");
        };
        assert!(ending.to_string().contains("meeting has ended"));
        // A close after LEAVE is a clean leave.
        let mut session = Handshake::new(1);
        session.start(1);
        session.leave(2);
        assert_eq!(session.closed(1000, ""), vec![Step::Over(Ending::Left)]);
    }

    #[test]
    fn credentials_never_print() {
        let turn = TurnCredentials {
            username: "1700000000:attendee".into(),
            password: "hunter2".into(),
            ttl: None,
            uris: vec![],
        };
        let printed = format!("{turn:?}");
        assert!(!printed.contains("hunter2") && !printed.contains("attendee"));
        let header = protocols(&JoinToken::new("tok")).expect("a header");
        assert!(header.is_sensitive());
        assert_eq!(header.to_str().ok(), Some("_aws_wt_session, tok"));
    }
}
