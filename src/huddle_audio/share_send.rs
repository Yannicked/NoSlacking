//! A screen share on its way out (the `huddle-share` feature): the
//! camera's sending thread (`super::camera_send`) at a share's pace,
//! between the video helper, which captures and encodes the screen, and
//! the share's own session.
//!
//! The helper ([`RemoteCapture`]) keeps the newest captured picture,
//! skips ones that did not change, and encodes on request: on the GPU
//! up to 1920×1080 when the setting is on and it can, else in software up
//! to 1280×720. The sending thread decides when: at most [`FPS`] pictures
//! a second, a still screen's picture again once a second
//! ([`KEEPALIVE`]) so the stream never looks stalled, a keyframe every
//! [`IDR_EVERY`] and whenever a receiver asks (PLI or FIR; Chime also
//! asks content senders every 10 s), and the bitrate as the bandwidth
//! estimate allows (up to 2.5 Mbit/s).
//!
//! A share whose capture ends (the compositor's "stop sharing", a window
//! closed) or whose helper fails stops here, and says why through its
//! [`Ending`]: unlike the camera's, a share is not started again in a
//! new helper, since that may need the system's dialog again.

use std::time::Duration;

use tokio::sync::{mpsc, watch};

use super::camera_send::{
    CameraUplink, Encoding, Ending, Limits, Options, Pace, SendControl, VideoFrame,
};
use super::chime::VideoSend;
use super::helper::{CaptureTrouble, RemoteCapture};
use super::share::{CaptureProblem, FPS, MAX_SIZE};

pub use super::camera_send::IDR_EVERY;

/// How SUBSCRIBE describes a share: what it sends at most.
pub const DESCRIPTOR: VideoSend = VideoSend {
    width: MAX_SIZE.0,
    height: MAX_SIZE.1,
    fps: FPS,
    max_kbps: Limits::SHARE.max_bitrate / 1000,
};
/// A still screen's picture is sent again at least this often.
pub const KEEPALIVE: Duration = Duration::from_secs(1);
/// A share's pace: 15 a second, a still screen once a second.
pub const PACE: Pace = Pace {
    fps: FPS,
    keepalive: Some(KEEPALIVE),
    stall: None,
};

/// What a helper's failure means for the share.
pub fn ending(trouble: &CaptureTrouble) -> Ending {
    use crate::failure::{Failure, HuddleTrouble};
    match trouble {
        CaptureTrouble::Problem(CaptureProblem::Ended, _) => Ending::Ended,
        CaptureTrouble::Problem(problem, _) => {
            Ending::Failed(super::share::problem_failure(*problem))
        }
        CaptureTrouble::Lost(_) => Ending::Failed(Failure::Huddle(HuddleTrouble::VideoHelperLost)),
    }
}

/// Starts sending what `share` encodes into `frames`, as `control` (a
/// share's: [`SendControl::new`] with [`Limits::SHARE`]) says; tells
/// `ended` if the share ends by itself.
pub fn spawn(
    share: RemoteCapture,
    frames: mpsc::Sender<VideoFrame>,
    control: SendControl,
    ended: watch::Sender<Option<Ending>>,
) -> Result<Encoding, String> {
    let options = Options {
        what: "share",
        pace: PACE,
        preview: None,
        restart: None,
        ending: Box::new(ending),
    };
    Encoding::spawn(share, Some(frames), control, ended, options)
}

/// What the share's session is handed: a [`CameraUplink`] with the
/// share's descriptor; `on` true for as long as it runs.
pub fn uplink(
    frames: mpsc::Receiver<VideoFrame>,
    on: tokio::sync::watch::Receiver<bool>,
    control: SendControl,
    refused: mpsc::Sender<()>,
) -> CameraUplink {
    CameraUplink {
        frames,
        on,
        control,
        refused,
        descriptor: DESCRIPTOR,
    }
}

#[cfg(test)]
mod tests {
    use super::super::camera_send::Gate;
    use super::super::helper::pretend::{Act, Pretend, welcome};
    use super::super::helper::{self, Helper, Lane};
    use super::*;
    use noslacking_video_ipc::{self as ipc, Reply, Request, ShareChoice};
    use std::sync::Arc;
    use std::time::Instant;

    const FRAME: Duration = Duration::from_millis(67);

    #[test]
    fn a_still_screen_is_asked_for_a_keyframe_first_then_once_a_second() {
        let start = Instant::now();
        let mut gate = Gate::new(PACE);
        assert_eq!(gate.wait(start), Duration::ZERO);
        // The first picture: a keyframe, whatever the helper has.
        let first = gate.ask(start, false, None);
        assert!(first.force_keyframe && first.repeat);
        gate.sent(start);
        let keyframe = Some(start);
        // A frame's time later: only something new.
        assert!(gate.wait(start + Duration::from_millis(20)) > Duration::ZERO);
        assert_eq!(gate.wait(start + FRAME), Duration::ZERO);
        let next = gate.ask(start + FRAME, false, keyframe);
        assert!(!next.force_keyframe && !next.repeat);
        // A second without anything new: the same picture again.
        let still = gate.ask(
            start + KEEPALIVE + Duration::from_millis(1),
            false,
            keyframe,
        );
        assert!(!still.force_keyframe && still.repeat);
        // A keyframe once IDR_EVERY has passed, still or not, and when
        // a receiver asks.
        assert!(gate.ask(start + IDR_EVERY, false, keyframe).force_keyframe);
        let asked = gate.ask(start + FRAME * 2, true, keyframe);
        assert!(asked.force_keyframe && asked.repeat);
    }

    #[test]
    fn the_helpers_troubles_end_the_share_as_they_should() {
        use crate::failure::{Failure, HuddleTrouble};
        assert_eq!(
            ending(&CaptureTrouble::Problem(
                CaptureProblem::Ended,
                "closed".into()
            )),
            Ending::Ended
        );
        assert_eq!(
            ending(&CaptureTrouble::Problem(CaptureProblem::Failed, "x".into())),
            Ending::Failed(Failure::Huddle(HuddleTrouble::ShareCapture))
        );
        assert_eq!(
            ending(&CaptureTrouble::Lost("it crashed".into())),
            Ending::Failed(Failure::Huddle(HuddleTrouble::VideoHelperLost))
        );
    }

    /// Collects frames from `out` until `n` came or a while passed.
    fn collect(out: &mut mpsc::Receiver<VideoFrame>, n: usize) -> Vec<VideoFrame> {
        let mut got = Vec::new();
        let deadline = Instant::now() + Duration::from_secs(20);
        while got.len() < n && Instant::now() < deadline {
            match out.try_recv() {
                Ok(frame) => got.push(frame),
                Err(_) => std::thread::sleep(Duration::from_millis(10)),
            }
        }
        got
    }

    /// The helper's test screen through the helper's own code on a
    /// thread, no GPU: 720p H.264, a keyframe first, RTP time going up,
    /// at most 15 a second, and nothing once it stops.
    #[test]
    fn the_test_screen_goes_out_as_720p_h264_in_software() {
        let helper = helper::shared(Lane::Screen).expect("in tests, always");
        let (share, _) = helper
            .start_share(ShareChoice::Test, false, 1_000_000, "")
            .expect("started");
        let (frames, mut out) = mpsc::channel(8);
        let (ended, _) = watch::channel(None);
        let control = SendControl::new(Limits::SHARE);
        let encoding = spawn(share, frames, control, ended).expect("a thread");
        let started = Instant::now();
        let got = collect(&mut out, 6);
        let took = started.elapsed();
        drop(encoding);
        assert!(got.len() >= 6, "only {} frames", got.len());
        assert!(got[0].keyframe);
        assert!(got.windows(2).all(|w| w[1].time > w[0].time));
        assert!(took >= FRAME * 4, "paced: six in {took:?}");
        let picture = rusty_h264_decoder::Decoder::new()
            .decode(&got[0].data)
            .expect("decodes")
            .expect("a picture");
        assert_eq!((picture.width, picture.height), (1280, 720));
        while out.try_recv().is_ok() {}
        std::thread::sleep(Duration::from_millis(150));
        assert!(out.try_recv().is_err(), "nothing after it stopped");
    }

    /// A pretend helper: frames as asked, a keyframe when forced; how it
    /// was asked, and when, kept.
    fn pretend(asked: Arc<std::sync::Mutex<Vec<(bool, bool)>>>, crash_after: usize) -> Helper {
        let pretend = Pretend::new(move |request| match request {
            Request::Hello { .. } => Act::Reply(welcome()),
            Request::StartShare { .. } => Act::Reply(Reply::Started {
                id: 1,
                restore: String::new(),
            }),
            Request::NextFrame {
                force_keyframe,
                repeat,
                ..
            } => {
                let mut asked = asked.lock().expect("a lock");
                asked.push((*force_keyframe, *repeat));
                if asked.len() > crash_after {
                    return Act::Crash;
                }
                let data = if *force_keyframe {
                    vec![
                        0, 0, 0, 1, 0x67, 1, 0, 0, 0, 1, 0x68, 1, 0, 0, 0, 1, 0x65, 1,
                    ]
                } else {
                    vec![0, 0, 0, 1, 0x41, 1]
                };
                Act::Reply(Reply::Frame(ipc::CapturedFrame {
                    keyframe: *force_keyframe,
                    hardware: true,
                    width: 1920,
                    height: 1080,
                    age_us: 2_000,
                    data,
                    preview: None,
                }))
            }
            _ => Act::Reply(Reply::Done),
        });
        Helper::with_timeouts(
            Arc::new(pretend),
            Duration::from_secs(5),
            Duration::from_secs(1),
        )
    }

    /// A receiver's PLI makes the next picture a keyframe.
    #[test]
    fn a_receiver_gets_its_keyframe() {
        let asked = Arc::new(std::sync::Mutex::new(Vec::new()));
        let helper = pretend(Arc::clone(&asked), usize::MAX);
        let (share, _) = helper
            .start_share(ShareChoice::Test, true, 1_000_000, "")
            .expect("started");
        let (frames, mut out) = mpsc::channel(64);
        let (ended, _) = watch::channel(None);
        let control = SendControl::new(Limits::SHARE);
        let encoding = spawn(share, frames, control.clone(), ended).expect("a thread");
        let first = collect(&mut out, 3);
        control.want_keyframe();
        // Half a second after the last keyframe at most, it comes.
        let after = collect(&mut out, 9);
        drop(encoding);
        assert!(first[0].keyframe && !first[1].keyframe && !first[2].keyframe);
        assert!(after.iter().any(|f| f.keyframe), "a keyframe after the PLI");
        assert!(!control.keyframe_wanted());
    }

    /// A helper that crashes mid-share ends it, saying so: a share is not
    /// started again.
    #[test]
    fn a_crashed_helper_ends_the_share() {
        use crate::failure::{Failure, HuddleTrouble};
        let asked = Arc::new(std::sync::Mutex::new(Vec::new()));
        let helper = pretend(Arc::clone(&asked), 3);
        let (share, _) = helper
            .start_share(ShareChoice::Test, true, 1_000_000, "")
            .expect("started");
        let (frames, mut out) = mpsc::channel(64);
        let (ended, mut told) = watch::channel(None);
        let encoding =
            spawn(share, frames, SendControl::new(Limits::SHARE), ended).expect("a thread");
        let got = collect(&mut out, 5);
        assert_eq!(got.len(), 3, "three frames, then the crash");
        let deadline = Instant::now() + Duration::from_secs(5);
        while told.borrow_and_update().is_none() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(
            *told.borrow(),
            Some(Ending::Failed(Failure::Huddle(
                HuddleTrouble::VideoHelperLost
            )))
        );
        drop(encoding);
    }

    #[test]
    fn a_share_is_described_as_1080p_15_fps_up_to_2500_kbps() {
        assert_eq!(
            DESCRIPTOR,
            VideoSend {
                width: 1920,
                height: 1080,
                fps: 15,
                max_kbps: 2500
            }
        );
    }
}
