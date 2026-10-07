//! Who is in the huddle and who is speaking, as Chime's signaling says
//! it: attendees by audio stream (AUDIO_STREAM_ID_INFO, kept by the
//! signaling module), their volumes (AUDIO_METADATA, several times a
//! second) and the head count (INDEX). The media session turns these
//! into a [`Roster`] for the interface whenever it changes.
//!
//! Slack names each attendee by an external user id of the form
//! `TEAM-ROOM-USER`, with a suffix per device on some
//! (`T…-R…-U…-12241178121314`); the `U…` part is the Slack user.

use std::collections::{BTreeMap, HashMap};
use std::time::{Duration, Instant};

use super::chime::proto::SdkAudioMetadataFrame;
use super::signaling::Attendee;

/// How long someone shows as speaking after they were last heard, so the
/// mark does not flicker between words.
pub const SPEAKING_HOLD: Duration = Duration::from_millis(700);

/// The quietest volume Chime's own client still shows as sound. Chime
/// sends a volume as decibels below full scale (0 the loudest), and its
/// JS SDK scales -42 dB to silence (`DefaultVolumeIndicatorAdapter`,
/// with `minVolumeDecibels` -42); an attendee left out of a frame is
/// silent.
const SILENT_FROM: u32 = 42;

/// One person in the huddle.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Person {
    /// Their Slack user id, when Chime's external id carries one.
    pub user: Option<String>,
    /// This app's own attendee: you, listening.
    pub me: bool,
    /// Whether their microphone is off.
    pub muted: bool,
    /// Whether they are speaking now.
    pub speaking: bool,
}

/// Who is in the huddle, as the interface shows it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Roster {
    /// Everyone Chime lists with audio, in the order they came.
    pub people: Vec<Person>,
    /// How many Chime counts in the meeting, you included (INDEX), once
    /// it has said.
    pub count: Option<u32>,
}

impl Roster {
    /// How many others are here, by audio stream.
    pub fn others(&self) -> usize {
        self.people.iter().filter(|p| !p.me).count()
    }

    /// Whether only you are left: no one else has audio here and Chime's
    /// head count, if it gave one, agrees.
    pub fn alone(&self) -> bool {
        self.others() == 0 && self.count.is_none_or(|count| count <= 1)
    }
}

/// The Slack user an attendee's external user id names: the `U…` (or
/// `W…`, an Enterprise Grid user) part of `TEAM-ROOM-USER[-suffix]`, or
/// the whole id when it is a bare user id.
pub fn slack_user(external: &str) -> Option<&str> {
    let looks_like_user = |part: &str| {
        part.len() > 1
            && part.starts_with(['U', 'W'])
            && part
                .chars()
                .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit())
    };
    let parts: Vec<&str> = external.split('-').collect();
    match parts.as_slice() {
        [one] => Some(*one).filter(|p| looks_like_user(p)),
        // Past the team and the room: the first part that is a user.
        [_, _, rest @ ..] => rest.iter().copied().find(|p| looks_like_user(p)),
        _ => None,
    }
}

/// Whether a volume from AUDIO_METADATA is sound rather than silence.
pub fn audible(volume: Option<u32>) -> bool {
    volume.is_some_and(|v| v < SILENT_FROM)
}

/// When each audio stream was last heard.
#[derive(Clone, Debug, Default)]
pub struct Voices {
    heard: HashMap<u32, Instant>,
}

impl Voices {
    /// Takes in one AUDIO_METADATA frame received at `now`.
    pub fn metadata(&mut self, frame: &SdkAudioMetadataFrame, now: Instant) {
        for state in &frame.attendee_states {
            // Stream 0 is no one (the JS SDK skips it too).
            let Some(stream) = state.audio_stream_id.filter(|&s| s != 0) else {
                continue;
            };
            if audible(state.volume) && state.muted != Some(true) {
                self.heard.insert(stream, now);
            }
        }
        // Long silent streams are forgotten, so the map stays small.
        self.heard
            .retain(|_, at| now.saturating_duration_since(*at) < SPEAKING_HOLD);
    }

    /// Whether `stream` was heard within [`SPEAKING_HOLD`] of `now`.
    pub fn speaking(&self, stream: u32, now: Instant) -> bool {
        self.heard
            .get(&stream)
            .is_some_and(|at| now.saturating_duration_since(*at) < SPEAKING_HOLD)
    }
}

/// The roster at `now`: the attendees by stream, `me` being this app's
/// own attendee id, who of them is speaking, and Chime's head count.
pub fn roster(
    attendees: &BTreeMap<u32, Attendee>,
    me: &str,
    voices: &Voices,
    count: Option<u32>,
    now: Instant,
) -> Roster {
    let people = attendees
        .iter()
        // A screen share joins as a second attendee of the same person,
        // `…#content`, sending silence: it is not someone in the huddle.
        .filter(|(_, attendee)| {
            !attendee
                .attendee_id
                .ends_with(super::join::CONTENT_MODALITY)
        })
        .map(|(&stream, attendee)| Person {
            user: attendee
                .external_user_id
                .as_deref()
                .and_then(slack_user)
                .map(str::to_owned),
            me: attendee.attendee_id == me,
            muted: attendee.muted,
            speaking: !attendee.muted && voices.speaking(stream, now),
        })
        .collect();
    Roster { people, count }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::huddle_audio::chime::proto::SdkAudioAttendeeState;

    #[test]
    fn external_ids_name_the_slack_user() {
        assert_eq!(
            slack_user("T01CRS4S6D9-R0C6Y0V6337-U058QQGR6ER-12241178121314"),
            Some("U058QQGR6ER")
        );
        assert_eq!(
            slack_user("T01CRS4S6D9-R0C6Y0V6337-U06SC7VN56V"),
            Some("U06SC7VN56V")
        );
        assert_eq!(slack_user("E01-R02-W0123ABC"), Some("W0123ABC"));
        assert_eq!(slack_user("U0123ABCDEF"), Some("U0123ABCDEF"));
        // A room is never taken for the user.
        assert_eq!(slack_user("T01-R0C6Y0V6337"), None);
        assert_eq!(slack_user("T01-R02-something-else"), None);
        assert_eq!(slack_user("a1b2c3d4-e5f6"), None);
        assert_eq!(slack_user(""), None);
    }

    fn attendee(id: &str, external: &str, muted: bool) -> Attendee {
        Attendee {
            attendee_id: id.into(),
            external_user_id: Some(external.into()),
            muted,
        }
    }

    fn metadata(states: &[(u32, Option<u32>)]) -> SdkAudioMetadataFrame {
        SdkAudioMetadataFrame {
            attendee_states: states
                .iter()
                .map(|&(stream, volume)| SdkAudioAttendeeState {
                    audio_stream_id: Some(stream),
                    volume,
                    muted: None,
                    signal_strength: None,
                })
                .collect(),
        }
    }

    #[test]
    fn speaking_follows_the_volumes_and_lingers_a_little() {
        let now = Instant::now();
        let attendees = BTreeMap::from([
            (1, attendee("me", "T1-R1-U0-123", true)),
            (2, attendee("a2", "T1-R1-U2", false)),
            (3, attendee("a3", "T1-R1-U3", false)),
        ]);
        let mut voices = Voices::default();
        // Ana loud, Carla at the floor, stream 0 nobody's.
        voices.metadata(
            &metadata(&[(2, Some(10)), (3, Some(42)), (0, Some(0))]),
            now,
        );
        let roster = roster(&attendees, "me", &voices, Some(3), now);
        let who: Vec<(Option<&str>, bool, bool, bool)> = roster
            .people
            .iter()
            .map(|p| (p.user.as_deref(), p.me, p.muted, p.speaking))
            .collect();
        assert_eq!(
            who,
            [
                (Some("U0"), true, true, false),
                (Some("U2"), false, false, true),
                (Some("U3"), false, false, false),
            ]
        );
        assert_eq!(roster.others(), 2);
        // Left out of the next frames: still speaking for a moment, then not.
        let soon = now + SPEAKING_HOLD / 2;
        voices.metadata(&metadata(&[(3, Some(5))]), soon);
        assert!(voices.speaking(2, soon) && voices.speaking(3, soon));
        let later = now + SPEAKING_HOLD;
        assert!(!voices.speaking(2, later) && voices.speaking(3, later));
        // A frame without a volume says nothing of sound.
        voices.metadata(&metadata(&[(2, None)]), later);
        assert!(!voices.speaking(2, later));
    }

    /// A share's own attendee (ours or anyone's) is not a second person.
    #[test]
    fn a_screen_share_is_not_a_person() {
        let attendees = BTreeMap::from([
            (1, attendee("me", "T1-R1-U0", true)),
            (2, attendee("me#content", "T1-R1-U0", false)),
            (3, attendee("a3", "T1-R1-U3", false)),
            (4, attendee("a3#content", "T1-R1-U3", false)),
        ]);
        let roster = roster(&attendees, "me", &Voices::default(), None, Instant::now());
        let who: Vec<_> = roster.people.iter().map(|p| p.user.as_deref()).collect();
        assert_eq!(who, [Some("U0"), Some("U3")]);
        assert_eq!(roster.others(), 1);
    }

    #[test]
    fn alone_is_when_no_one_else_is_heard_or_counted() {
        let me = Person {
            user: Some("U0".into()),
            me: true,
            muted: true,
            speaking: false,
        };
        let other = Person {
            me: false,
            user: Some("U2".into()),
            ..me.clone()
        };
        let roster = |people: Vec<Person>, count| Roster { people, count };
        assert!(roster(vec![me.clone()], Some(1)).alone());
        assert!(roster(vec![me.clone()], None).alone());
        // Someone counted whose audio has not come yet.
        assert!(!roster(vec![me.clone()], Some(2)).alone());
        assert!(!roster(vec![me.clone(), other.clone()], Some(1)).alone());
        assert!(!roster(vec![other], None).alone());
    }
}
