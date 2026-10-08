//! The microphone in a huddle, as the interface sees it: muted on
//! joining, opened only when you unmute, closed when you mute or leave.
//!
//! Unmuting shows at once as "turning on" and becomes live when the worker
//! says the device is open; muting shows at once, as the device closes
//! then. A microphone that would not open says so and stays muted. The
//! worker keeps the rule itself (see
//! [`crate::huddle_audio::microphone`]); this is only its picture.

use crate::app::App;
use crate::backend;
use crate::failure::Failure;
use crate::i18n::tf;
use crate::people;

/// Where the microphone is.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Mic {
    /// Closed; the others hear nothing and see you muted.
    #[default]
    Muted,
    /// Asked to open, not open yet.
    Opening,
    /// Open: the others hear you.
    Live,
}

/// What the views ask of the microphone.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MicAction {
    /// Close it.
    Mute,
    /// Open it.
    Unmute,
}

/// What the worker says of the microphone.
#[derive(Clone, Debug, PartialEq)]
pub enum MicNews {
    /// It is closed.
    Muted,
    /// It is open and sending.
    Live,
    /// It would not open; still muted.
    Failed(Failure),
    /// Another microphone was chosen while it was open, and that one
    /// would not open; muted now.
    SwitchFailed(Failure),
}

/// The state after asking for `action` in state `mic`.
pub fn asked(mic: Mic, action: MicAction) -> Mic {
    match action {
        MicAction::Mute => Mic::Muted,
        MicAction::Unmute if mic == Mic::Live => Mic::Live,
        MicAction::Unmute => Mic::Opening,
    }
}

/// The state after the worker's `news`, if still wanted: a late "live"
/// after muting again does not unmute the picture.
pub fn told(mic: Mic, news: &MicNews) -> Mic {
    match (mic, news) {
        (Mic::Muted, MicNews::Live) => Mic::Muted,
        (_, MicNews::Live) => Mic::Live,
        (Mic::Opening, MicNews::Muted) => Mic::Opening,
        (_, MicNews::Muted | MicNews::Failed(_) | MicNews::SwitchFailed(_)) => Mic::Muted,
    }
}

/// Applies a view's request to the huddle being listened to.
pub fn apply(app: &mut App, action: MicAction) {
    let Some(listening) = app.huddles.listening.as_mut() else {
        return;
    };
    let next = asked(listening.mic, action);
    if next == listening.mic {
        return;
    }
    listening.mic = next;
    let team = listening.team.clone();
    app.backend.send(backend::Command::People {
        team,
        command: people::Command::MuteHuddle {
            muted: action == MicAction::Mute,
        },
    });
}

/// Takes the worker's news of the microphone in `channel` of `team`.
pub fn news(app: &mut App, team: &str, channel: &str, news: MicNews) {
    let Some(listening) = app
        .huddles
        .listening
        .as_mut()
        .filter(|l| l.team == team && l.channel == channel)
    else {
        return;
    };
    listening.mic = told(listening.mic, &news);
    let text = match news {
        MicNews::Failed(error) => tf("Could not unmute: {error}", &[("error", &error.message())]),
        MicNews::SwitchFailed(error) => tf(
            "Your microphone is muted: the one you chose could not be opened ({error})",
            &[("error", &error.message())],
        ),
        MicNews::Muted | MicNews::Live => return,
    };
    app.toast(text, true);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::failure::HuddleTrouble;

    #[test]
    fn unmuting_shows_at_once_and_goes_live_when_the_device_opens() {
        let mic = asked(Mic::Muted, MicAction::Unmute);
        assert_eq!(mic, Mic::Opening);
        assert_eq!(told(mic, &MicNews::Live), Mic::Live);
        assert_eq!(asked(Mic::Live, MicAction::Unmute), Mic::Live);
    }

    #[test]
    fn muting_wins_over_late_news() {
        let mic = asked(Mic::Opening, MicAction::Mute);
        assert_eq!(mic, Mic::Muted);
        // The device opened just before the mute reached the worker.
        assert_eq!(told(mic, &MicNews::Live), Mic::Muted);
        // An unmute in flight is not undone by the last mute's news.
        assert_eq!(told(Mic::Opening, &MicNews::Muted), Mic::Opening);
        assert_eq!(told(Mic::Live, &MicNews::Muted), Mic::Muted);
    }

    #[test]
    fn a_microphone_that_would_not_open_is_muted() {
        let failed = MicNews::Failed(Failure::Huddle(HuddleTrouble::Microphone));
        assert_eq!(told(Mic::Opening, &failed), Mic::Muted);
        assert_eq!(Mic::default(), Mic::Muted);
        // Live, then switched to one that would not open.
        let switched = MicNews::SwitchFailed(Failure::Huddle(HuddleTrouble::Microphone));
        assert_eq!(told(Mic::Live, &switched), Mic::Muted);
    }
}
