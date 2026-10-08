//! Your camera in a huddle, as the interface sees it (the
//! `huddle-camera` feature): off on joining, opened only when you turn it
//! on, closed when you turn it off or leave.
//!
//! Turning it on shows at once as "turning on" and becomes on when the
//! worker says the camera is open; turning it off shows at once, as the
//! camera closes then. A camera that would not open, or a huddle that
//! takes no video from you (Chime's "view only"), says so and stays off;
//! so does one that stops by itself while on. The video helper keeps the
//! rule itself (it opens the camera only when asked, closes it when its
//! capture is closed); the worker asks it (`backend::listen`); this is
//! only its picture.

use crate::app::{App, Tone};
use crate::backend;
use crate::failure::{Failure, HuddleTrouble};
use crate::i18n::tf;
use crate::people;

pub use crate::huddle_audio::camera_send::Preview;

/// Where the camera is.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Cam {
    /// Closed; the others see no video of you.
    #[default]
    Off,
    /// Asked to open, not open yet.
    Opening,
    /// Open: the others see you.
    On,
}

/// What the views ask of the camera.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CamAction {
    /// Close it.
    Off,
    /// Open it.
    On,
}

/// What the worker says of the camera.
#[derive(Clone, Debug, PartialEq)]
pub enum CamNews {
    /// It is closed.
    Off,
    /// It is open and sending.
    On,
    /// It would not open, or the huddle took no video; it is off.
    Failed(Failure),
    /// It was on and stopped by itself (unplugged, or the video helper
    /// failing for good); it is off.
    Stopped(Failure),
}

/// The state after asking for `action` in state `cam`.
pub fn asked(cam: Cam, action: CamAction) -> Cam {
    match action {
        CamAction::Off => Cam::Off,
        CamAction::On if cam == Cam::On => Cam::On,
        CamAction::On => Cam::Opening,
    }
}

/// The state after the worker's `news`, if still wanted: a late "on"
/// after turning it off again does not turn the picture on.
pub fn told(cam: Cam, news: &CamNews) -> Cam {
    match (cam, news) {
        (Cam::Off, CamNews::On) => Cam::Off,
        (_, CamNews::On) => Cam::On,
        (Cam::Opening, CamNews::Off) => Cam::Opening,
        (_, CamNews::Off | CamNews::Failed(_) | CamNews::Stopped(_)) => Cam::Off,
    }
}

/// Applies a view's request to the huddle being listened to.
pub fn apply(app: &mut App, action: CamAction) {
    let Some(listening) = app.huddles.listening.as_mut() else {
        return;
    };
    let next = asked(listening.camera, action);
    if next == listening.camera {
        return;
    }
    listening.camera = next;
    let team = listening.team.clone();
    app.backend.send(backend::Command::People {
        team,
        command: people::Command::CameraHuddle {
            on: action == CamAction::On,
        },
    });
}

/// Takes the worker's news of the camera in `channel` of `team`.
pub fn news(app: &mut App, team: &str, channel: &str, news: CamNews) {
    let Some(listening) = app
        .huddles
        .listening
        .as_mut()
        .filter(|l| l.team == team && l.channel == channel)
    else {
        return;
    };
    listening.camera = told(listening.camera, &news);
    let text = match news {
        CamNews::Failed(error) if error == Failure::Huddle(HuddleTrouble::ViewOnly) => {
            error.message()
        }
        CamNews::Failed(error) => tf(
            "Could not turn on your camera: {error}",
            &[("error", &error.message())],
        ),
        CamNews::Stopped(error) => tf(
            "Your camera stopped: {error}",
            &[("error", &error.message())],
        ),
        CamNews::Off | CamNews::On => return,
    };
    app.toast(text, Tone::Error);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn turning_on_shows_at_once_and_is_on_when_the_camera_opens() {
        let cam = asked(Cam::Off, CamAction::On);
        assert_eq!(cam, Cam::Opening);
        assert_eq!(told(cam, &CamNews::On), Cam::On);
        assert_eq!(asked(Cam::On, CamAction::On), Cam::On);
    }

    #[test]
    fn turning_off_wins_over_late_news() {
        let cam = asked(Cam::Opening, CamAction::Off);
        assert_eq!(cam, Cam::Off);
        // The camera opened just before "off" reached the worker.
        assert_eq!(told(cam, &CamNews::On), Cam::Off);
        // Turning on again is not undone by the last "off"'s news.
        assert_eq!(told(Cam::Opening, &CamNews::Off), Cam::Opening);
        assert_eq!(told(Cam::On, &CamNews::Off), Cam::Off);
    }

    #[test]
    fn a_camera_that_would_not_open_or_was_refused_is_off() {
        for trouble in [
            HuddleTrouble::NoCamera,
            HuddleTrouble::CameraDenied,
            HuddleTrouble::Camera,
            HuddleTrouble::CameraNeedsHelper,
            HuddleTrouble::VideoHelperLost,
            HuddleTrouble::ViewOnly,
        ] {
            let failed = CamNews::Failed(Failure::Huddle(trouble));
            assert_eq!(told(Cam::Opening, &failed), Cam::Off);
            assert_eq!(told(Cam::On, &failed), Cam::Off);
            assert!(!Failure::Huddle(trouble).message().is_empty());
        }
        assert_eq!(Cam::default(), Cam::Off);
        // One that was on and stopped by itself is off too.
        for trouble in [
            HuddleTrouble::CameraGone,
            HuddleTrouble::VideoHelperLost,
            HuddleTrouble::CameraNeedsHelper,
        ] {
            let stopped = CamNews::Stopped(Failure::Huddle(trouble));
            assert_eq!(told(Cam::On, &stopped), Cam::Off);
        }
    }
}
