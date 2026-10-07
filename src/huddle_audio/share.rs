//! Sharing your screen in a huddle (the `huddle-share` feature): what the
//! app knows of it. The screen itself never reaches the app: the video
//! helper (`noslacking-video`, [`super::helper::Lane::Screen`]) lists what
//! can be shared, shows the system's dialog where there is one, captures
//! and encodes, and hands over H.264 ([`super::share_send`]). Without the
//! helper there is no sharing.
//!
//! The rule, the microphone's and the camera's, is kept in the helper:
//! nothing is captured until you choose to share, and capture stops when
//! the app closes the share (you stop, leave, or its session fails) or
//! the system ends it (the compositor's own "stop sharing"). Joining
//! never shares.

use crate::failure::{Failure, HuddleTrouble};

pub use noslacking_video_ipc::{ShareProblem, Source, SourceKind};

/// The largest picture a share sends: 1080p, as Slack's own shares do
/// (the helper's GPU; software sends at most 1280×720).
pub const MAX_SIZE: (u32, u32) = (1920, 1080);
/// Frames a second asked of the capture and sent at most: the JS SDK's
/// default for content (`ContentShareMediaStreamBroker.defaultFrameRate`).
pub const FPS: u32 = 15;
/// How many people Chime lets share at a time (Slack's "up to two people
/// can share their screen at a time").
pub const MAX_SHARES: usize = 2;

/// Whether one more may share while `others` already do.
pub fn may_share(others: usize) -> bool {
    others < MAX_SHARES
}

/// What a share that did not start, or stopped, tells the interface,
/// from what the helper said.
pub fn problem_failure(problem: ShareProblem) -> Failure {
    Failure::Huddle(match problem {
        ShareProblem::Cancelled => HuddleTrouble::ShareCancelled,
        ShareProblem::Denied => HuddleTrouble::ShareDenied,
        ShareProblem::Unavailable => HuddleTrouble::NoScreenCapture,
        ShareProblem::Gone => HuddleTrouble::ShareGone,
        ShareProblem::Ended | ShareProblem::Failed => HuddleTrouble::ShareCapture,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn two_shares_are_the_most() {
        assert!(may_share(0));
        assert!(may_share(1));
        assert!(!may_share(2));
        assert!(!may_share(3));
    }

    #[test]
    fn the_helpers_problems_have_their_words() {
        for (problem, trouble) in [
            (ShareProblem::Cancelled, HuddleTrouble::ShareCancelled),
            (ShareProblem::Denied, HuddleTrouble::ShareDenied),
            (ShareProblem::Unavailable, HuddleTrouble::NoScreenCapture),
            (ShareProblem::Gone, HuddleTrouble::ShareGone),
            (ShareProblem::Failed, HuddleTrouble::ShareCapture),
        ] {
            assert_eq!(problem_failure(problem), Failure::Huddle(trouble));
        }
    }
}
