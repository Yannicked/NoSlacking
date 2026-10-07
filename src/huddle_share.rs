//! Sharing your screen in a huddle, as the interface sees it (the
//! `huddle-share` feature): off on joining, captured only once you choose
//! to share, stopped when you stop or leave.
//!
//! Share screen asks the worker to start. Where the system has its own
//! dialog (the ScreenCast portal on Wayland and in the Flatpak) that is
//! where the user picks; elsewhere the worker answers with the screens
//! and windows there are, and the call bar shows them to pick from. The
//! share is "starting" until its own connection to the huddle is up,
//! then "on" ("You are sharing your screen", Stop sharing). A choice
//! cancelled, a capture the system refused, a huddle that already has two
//! shares, or a share that failed says so and is off. The worker keeps
//! the rule itself (see [`crate::huddle_audio::share`]); this is only its
//! picture.

use crate::app::App;
use crate::backend;
use crate::failure::{Failure, HuddleTrouble};
use crate::i18n::{t, tf};
use crate::people;

pub use crate::huddle_audio::share::{Source, SourceKind};

/// Where your share is.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Sharing {
    /// Not sharing.
    #[default]
    Off,
    /// The call bar shows what can be shared; nothing captured yet.
    Choosing,
    /// Asked to share: the system's dialog, the capture, the connection.
    Starting,
    /// Sharing: the others see your screen.
    On,
}

/// What the views ask of the share.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ShareAction {
    /// Share (the system's dialog, or the call bar's choice; the last
    /// choice again where the system remembers it).
    Start,
    /// Choose something else to share, stopping any share first.
    ChooseAgain,
    /// Share this one of the choices the call bar shows.
    Pick(String),
    /// Close the choices without sharing.
    CancelPick,
    /// Stop sharing.
    Stop,
}

/// What the interface asks the worker.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ShareRequest {
    /// Share; `again` forgets what the system remembers and asks.
    Start {
        /// Choose afresh.
        again: bool,
    },
    /// Share this one of the choices sent.
    Pick(String),
    /// Stop sharing.
    Stop,
}

/// What the worker says of the share.
#[derive(Clone, Debug, PartialEq)]
pub enum ShareNews {
    /// There is no system dialog: these can be shared; pick one.
    Choose(Vec<Source>),
    /// Sharing: the capture runs and the share's connection is up.
    On,
    /// Not sharing.
    Off,
    /// The share ended by itself: the system's own "stop sharing", or the
    /// window closed.
    Ended,
    /// It did not start, or it stopped by itself; it is off.
    Failed(Failure),
}

/// The state after asking for `action` in state `sharing`.
pub fn asked(sharing: Sharing, action: &ShareAction) -> Sharing {
    match action {
        ShareAction::Stop | ShareAction::CancelPick => Sharing::Off,
        ShareAction::Start if sharing == Sharing::Off => Sharing::Starting,
        ShareAction::Start => sharing,
        ShareAction::ChooseAgain | ShareAction::Pick(_) => Sharing::Starting,
    }
}

/// What to ask the worker for `action`, if anything.
pub fn request(sharing: Sharing, action: &ShareAction) -> Option<ShareRequest> {
    match action {
        ShareAction::Start if sharing == Sharing::Off => Some(ShareRequest::Start { again: false }),
        ShareAction::Start => None,
        ShareAction::ChooseAgain => Some(ShareRequest::Start { again: true }),
        ShareAction::Pick(id) => Some(ShareRequest::Pick(id.clone())),
        // Nothing is captured while choosing.
        ShareAction::CancelPick => None,
        ShareAction::Stop => Some(ShareRequest::Stop),
    }
}

/// The state after the worker's `news`, if still wanted: a late "on" or
/// a list of choices after stopping does not turn the share back on.
pub fn told(sharing: Sharing, news: &ShareNews) -> Sharing {
    match (sharing, news) {
        (Sharing::Off, ShareNews::On | ShareNews::Choose(_)) => Sharing::Off,
        (_, ShareNews::On) => Sharing::On,
        (_, ShareNews::Choose(_)) => Sharing::Choosing,
        // The last share's "off" after asking for the next.
        (Sharing::Starting, ShareNews::Off) => Sharing::Starting,
        (_, ShareNews::Off | ShareNews::Ended | ShareNews::Failed(_)) => Sharing::Off,
    }
}

/// Whether a failure is worth a toast that says "could not": a dialog
/// closed on purpose is only noted.
fn quiet(error: &Failure) -> bool {
    *error == Failure::Huddle(HuddleTrouble::ShareCancelled)
}

/// Applies a view's request to the huddle being listened to.
pub fn apply(app: &mut App, action: ShareAction) {
    // Chime takes two shares at a time; with `huddle-video` the bar knows
    // who shares, so a third is refused before anything is captured.
    #[cfg(feature = "huddle-video")]
    if matches!(action, ShareAction::Start | ShareAction::ChooseAgain)
        && let Some(listening) = &app.huddles.listening
        && listening.sharing == Sharing::Off
        && !crate::huddle_audio::share::may_share(listening.shares.len())
    {
        app.toast(Failure::Huddle(HuddleTrouble::ShareLimit).message(), false);
        return;
    }
    let Some(listening) = app.huddles.listening.as_mut().filter(|l| l.in_huddle()) else {
        return;
    };
    let sent = request(listening.sharing, &action);
    listening.sharing = asked(listening.sharing, &action);
    if listening.sharing != Sharing::Choosing {
        listening.share_sources.clear();
    }
    let Some(request) = sent else {
        return;
    };
    let team = listening.team.clone();
    app.backend.send(backend::Command::People {
        team,
        command: people::Command::ShareHuddle { request },
    });
}

/// Takes the worker's news of the share in `channel` of `team`.
pub fn news(app: &mut App, team: &str, channel: &str, news: ShareNews) {
    let Some(listening) = app
        .huddles
        .listening
        .as_mut()
        .filter(|l| l.team == team && l.channel == channel)
    else {
        return;
    };
    let was = listening.sharing;
    listening.sharing = told(was, &news);
    match news {
        ShareNews::Choose(sources) if listening.sharing == Sharing::Choosing => {
            listening.share_sources = sources;
        }
        ShareNews::Choose(_) => {}
        ShareNews::On | ShareNews::Off => listening.share_sources.clear(),
        ShareNews::Ended => {
            listening.share_sources.clear();
            if was == Sharing::On {
                app.toast(t("Your screen share ended"), false);
            }
        }
        ShareNews::Failed(error) => {
            listening.share_sources.clear();
            if quiet(&error) {
                app.toast(t("Screen sharing was cancelled"), false);
            } else if error == Failure::Huddle(HuddleTrouble::ShareLimit) {
                app.toast(error.message(), true);
            } else if was == Sharing::On {
                app.toast(
                    tf(
                        "Your screen share stopped: {error}",
                        &[("error", &error.message())],
                    ),
                    true,
                );
            } else {
                app.toast(
                    tf(
                        "Could not share your screen: {error}",
                        &[("error", &error.message())],
                    ),
                    true,
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sharing_starts_at_once_and_is_on_when_the_worker_says() {
        let start = ShareAction::Start;
        assert_eq!(
            request(Sharing::Off, &start),
            Some(ShareRequest::Start { again: false })
        );
        let sharing = asked(Sharing::Off, &start);
        assert_eq!(sharing, Sharing::Starting);
        assert_eq!(told(sharing, &ShareNews::On), Sharing::On);
        // Pressed again while it starts or runs: nothing more is asked.
        assert_eq!(request(Sharing::Starting, &start), None);
        assert_eq!(asked(Sharing::On, &start), Sharing::On);
    }

    #[test]
    fn without_a_system_dialog_the_bar_offers_choices() {
        let sources = vec![Source {
            id: "x11:all".into(),
            name: "All screens".into(),
            kind: SourceKind::Screen,
        }];
        let sharing = told(Sharing::Starting, &ShareNews::Choose(sources.clone()));
        assert_eq!(sharing, Sharing::Choosing);
        let pick = ShareAction::Pick("x11:all".into());
        assert_eq!(
            request(sharing, &pick),
            Some(ShareRequest::Pick("x11:all".into()))
        );
        assert_eq!(asked(sharing, &pick), Sharing::Starting);
        // Closing the choices captures nothing and asks nothing.
        assert_eq!(request(sharing, &ShareAction::CancelPick), None);
        assert_eq!(asked(sharing, &ShareAction::CancelPick), Sharing::Off);
        // Choices that come after stopping are not shown.
        assert_eq!(
            told(Sharing::Off, &ShareNews::Choose(sources)),
            Sharing::Off
        );
    }

    #[test]
    fn stopping_wins_over_late_news_and_failures_are_off() {
        let sharing = asked(Sharing::On, &ShareAction::Stop);
        assert_eq!(sharing, Sharing::Off);
        assert_eq!(
            request(Sharing::On, &ShareAction::Stop),
            Some(ShareRequest::Stop)
        );
        // The capture came up just before "stop" reached the worker.
        assert_eq!(told(sharing, &ShareNews::On), Sharing::Off);
        // Choosing something else: the old share's "off" does not undo it.
        let again = asked(Sharing::On, &ShareAction::ChooseAgain);
        assert_eq!(again, Sharing::Starting);
        assert_eq!(
            request(Sharing::On, &ShareAction::ChooseAgain),
            Some(ShareRequest::Start { again: true })
        );
        assert_eq!(told(again, &ShareNews::Off), Sharing::Starting);
        assert_eq!(told(Sharing::On, &ShareNews::Off), Sharing::Off);
        assert_eq!(told(Sharing::On, &ShareNews::Ended), Sharing::Off);
        for trouble in [
            HuddleTrouble::ShareCancelled,
            HuddleTrouble::ShareDenied,
            HuddleTrouble::NoScreenCapture,
            HuddleTrouble::ShareGone,
            HuddleTrouble::ShareCapture,
            HuddleTrouble::ShareLimit,
            HuddleTrouble::ShareRefused,
            HuddleTrouble::ShareLost,
        ] {
            let failed = ShareNews::Failed(Failure::Huddle(trouble));
            assert_eq!(told(Sharing::Starting, &failed), Sharing::Off);
            assert_eq!(told(Sharing::On, &failed), Sharing::Off);
            assert!(!Failure::Huddle(trouble).message().is_empty());
        }
        assert_eq!(Sharing::default(), Sharing::Off);
        assert!(quiet(&Failure::Huddle(HuddleTrouble::ShareCancelled)));
        assert!(!quiet(&Failure::Huddle(HuddleTrouble::ShareLimit)));
    }
}
