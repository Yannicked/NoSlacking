//! Joining a huddle through Slack: `rooms.join` and what it answers.
//!
//! Slack's web client asks `rooms.join` with the channel, the media
//! regions it can use and `multidevice=true`. The answer's
//! `call.free_willy` holds an Amazon Chime meeting and this person's
//! attendee in it, shaped as Chime's own `CreateMeeting` and
//! `CreateAttendee` answers (`MeetingId`, `MediaPlacement.SignalingUrl`,
//! `Attendee.JoinToken`, …), which [`ChimeJoin`] reads.
//!
//! Leaving is Chime's alone: a LEAVE frame on the signaling socket (in
//! the signaling module), as HuddleFM does; Slack hears of it from Chime.
//! There is no Slack call for it: a guessed `rooms.leave` was answered
//! `invalid_arguments`.

use serde_json::Value;

/// The media region asked for when none is given and the nearest is not
/// known (see [`super::region`]): Chime's first, in North America. Slack
/// picks the meeting's actual region; the answer says which.
pub const DEFAULT_REGION: &str = "us-east-1";

/// What `rooms.join` takes to join the huddle in `channel`, as HuddleFM
/// (`src/slack-huddle.ts`, `join`) sends it. The token travels with every
/// call, so it is not among them.
pub fn join_params(channel: &str, region: &str) -> Vec<(&'static str, String)> {
    vec![
        ("channel_id", channel.to_owned()),
        ("regions", region.to_owned()),
        ("multidevice", "true".to_owned()),
    ]
}

/// The attendee's join token: the one secret in a join. It opens the
/// signaling socket (as its subprotocol) and is shown nowhere.
#[derive(Clone, PartialEq, Eq)]
pub struct JoinToken(String);

impl JoinToken {
    /// Wraps a token.
    pub fn new(token: impl Into<String>) -> Self {
        Self(token.into())
    }

    /// The token itself, for the signaling socket's handshake only.
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for JoinToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(crate::redact::REDACTED)
    }
}

/// A huddle joined: Slack's call and the Chime meeting and attendee that
/// carry its sound.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChimeJoin {
    /// Slack's id for the call (`call.call_id`), the huddle's room.
    pub call_id: Option<String>,
    /// Chime's `MeetingId`.
    pub meeting_id: Option<String>,
    /// Chime's `MediaRegion`, such as `us-east-1`.
    pub media_region: Option<String>,
    /// `MediaPlacement.SignalingUrl`: the signaling WebSocket.
    pub signaling_url: String,
    /// `MediaPlacement.TurnControlUrl`. Chime's JOIN_ACK carries the TURN
    /// credentials nowadays, so this is kept for the log and as a
    /// fallback only.
    pub turn_control_url: Option<String>,
    /// `MediaPlacement.AudioHostUrl`, sent along in the SUBSCRIBE.
    pub audio_host_url: String,
    /// `Attendee.AttendeeId`.
    pub attendee_id: String,
    /// `Attendee.ExternalUserId`: Slack's name for this person in Chime.
    pub external_user_id: Option<String>,
    /// `Attendee.JoinToken`.
    pub join_token: JoinToken,
}

/// Why a `rooms.join` answer could not be read.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum JoinError {
    /// A field the connection needs is missing; the path names it.
    #[error("the answer has no {0}")]
    Missing(&'static str),
}

/// Why joining through Slack failed.
#[derive(Clone, Debug, PartialEq, thiserror::Error)]
pub enum JoinFailure {
    /// The sign-in is not a browser session's: `rooms.join` is only
    /// Slack's own clients'.
    #[error("rooms.join needs a browser sign-in")]
    NotSession,
    /// Slack refused or could not be reached.
    #[error("rooms.join: {0}")]
    Slack(#[from] crate::slack::SlackError),
    /// Slack answered, in a shape not understood.
    #[error("rooms.join: {0}")]
    Answer(#[from] JoinError),
}

/// Joins the huddle in `channel`, or starts one there.
pub async fn join(
    client: &crate::slack::Client,
    channel: &str,
    region: &str,
) -> Result<ChimeJoin, JoinFailure> {
    if !client.token().is_session() {
        return Err(JoinFailure::NotSession);
    }
    let answer: Value = client
        .act("rooms.join", &join_params(channel, region))
        .await?;
    Ok(parse(&answer)?)
}

/// Reads a successful `rooms.join` answer.
pub fn parse(answer: &Value) -> Result<ChimeJoin, JoinError> {
    let call = answer.get("call");
    let free_willy = call
        .and_then(|c| c.get("free_willy"))
        .ok_or(JoinError::Missing("call.free_willy"))?;
    parse_free_willy(free_willy, call.and_then(|c| text(c, "call_id")))
}

/// Reads a `free_willy` object: `{meeting, attendee}`. A `huddle_invite`
/// event carries one too.
pub fn parse_free_willy(
    free_willy: &Value,
    call_id: Option<String>,
) -> Result<ChimeJoin, JoinError> {
    let meeting = field(free_willy, "meeting").ok_or(JoinError::Missing("free_willy.meeting"))?;
    // Chime's own answers wrap these in `Meeting` and `Attendee`; Slack's
    // may or may not.
    let meeting = field(meeting, "Meeting").unwrap_or(meeting);
    let attendee =
        field(free_willy, "attendee").ok_or(JoinError::Missing("free_willy.attendee"))?;
    let attendee = field(attendee, "Attendee").unwrap_or(attendee);
    let placement =
        field(meeting, "MediaPlacement").ok_or(JoinError::Missing("meeting.MediaPlacement"))?;
    Ok(ChimeJoin {
        call_id,
        meeting_id: text(meeting, "MeetingId"),
        media_region: text(meeting, "MediaRegion"),
        signaling_url: text(placement, "SignalingUrl")
            .ok_or(JoinError::Missing("MediaPlacement.SignalingUrl"))?,
        turn_control_url: text(placement, "TurnControlUrl"),
        audio_host_url: text(placement, "AudioHostUrl")
            .ok_or(JoinError::Missing("MediaPlacement.AudioHostUrl"))?,
        attendee_id: text(attendee, "AttendeeId")
            .ok_or(JoinError::Missing("attendee.AttendeeId"))?,
        external_user_id: text(attendee, "ExternalUserId"),
        join_token: text(attendee, "JoinToken")
            .map(JoinToken)
            .ok_or(JoinError::Missing("attendee.JoinToken"))?,
    })
}

/// `object[key]`, matching the key in any case: Chime's SDK reads its
/// answers that way (`MeetingId` or `meetingId`).
fn field<'a>(object: &'a Value, key: &str) -> Option<&'a Value> {
    let map = object.as_object()?;
    map.get(key).or_else(|| {
        map.iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(key))
            .map(|(_, v)| v)
    })
}

/// A non-empty string at `object[key]`.
fn text(object: &Value, key: &str) -> Option<String> {
    field(object, key)
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A `rooms.join` answer shaped as HuddleFM reads it, with invented
    /// values.
    const ANSWER: &str = include_str!("fixtures/rooms_join.json");

    fn answer() -> Value {
        serde_json::from_str(ANSWER).expect("the fixture is JSON")
    }

    #[test]
    fn a_join_answer_gives_the_meeting_and_attendee() {
        let join = parse(&answer()).expect("reads");
        assert_eq!(join.call_id.as_deref(), Some("R0123ABCDEF"));
        assert_eq!(
            join.meeting_id.as_deref(),
            Some("11111111-2222-3333-4444-555555555555")
        );
        assert_eq!(join.media_region.as_deref(), Some("us-east-1"));
        assert_eq!(
            join.signaling_url,
            "wss://signal.m1.ue1.app.chime.aws/control/11111111-2222-3333-4444-555555555555"
        );
        assert_eq!(
            join.turn_control_url.as_deref(),
            Some("https://2713.cell.us-east-1.meetings.chime.aws/v2/turn_sessions")
        );
        assert_eq!(
            join.audio_host_url,
            "abcdef0123456789.k.m1.ue1.app.chime.aws:3478"
        );
        assert_eq!(join.attendee_id, "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee");
        assert_eq!(join.external_user_id.as_deref(), Some("U0123ABCDEF"));
        assert_eq!(join.join_token.expose(), "ZmFrZS1qb2luLXRva2Vu");
    }

    #[test]
    fn the_join_token_never_prints() {
        let join = parse(&answer()).expect("reads");
        let printed = format!("{join:?}");
        assert!(!printed.contains("ZmFrZS1qb2luLXRva2Vu"), "{printed}");
        assert!(printed.contains("<redacted>"));
    }

    #[test]
    fn wrapped_and_lower_case_shapes_read_too() {
        let wrapped = serde_json::json!({
            "meeting": {"Meeting": {
                "meetingId": "M1",
                "mediaPlacement": {"signalingUrl": "wss://s/x", "audioHostUrl": "a:3478"}
            }},
            "attendee": {"Attendee": {"attendeeId": "A1", "joinToken": "t"}}
        });
        let join = parse_free_willy(&wrapped, None).expect("reads");
        assert_eq!(join.meeting_id.as_deref(), Some("M1"));
        assert_eq!(join.attendee_id, "A1");
        assert_eq!(join.turn_control_url, None);
    }

    #[test]
    fn what_a_connection_needs_must_be_there() {
        let mut broken = answer();
        if let Some(attendee) = broken.pointer_mut("/call/free_willy/attendee") {
            attendee["JoinToken"] = Value::Null;
        }
        assert_eq!(
            parse(&broken),
            Err(JoinError::Missing("attendee.JoinToken"))
        );
        assert_eq!(
            parse(&serde_json::json!({"ok": true})),
            Err(JoinError::Missing("call.free_willy"))
        );
    }

    #[test]
    fn join_asks_like_the_web_client() {
        assert_eq!(
            join_params("C1", "us-east-1"),
            vec![
                ("channel_id", "C1".to_owned()),
                ("regions", "us-east-1".to_owned()),
                ("multidevice", "true".to_owned()),
            ]
        );
    }
}
