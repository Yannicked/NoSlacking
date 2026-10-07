//! A screen share on its way out (the `huddle-share` feature): the thread
//! between the video helper, which captures and encodes the screen, and
//! the share's own session.
//!
//! The helper ([`RemoteShare`]) keeps the newest captured picture,
//! skips ones that did not change, and encodes on request: on the GPU
//! up to 1920×1080 when the setting is on and it can, else in software up
//! to 1280×720. This thread decides when: at most [`FPS`] pictures a
//! second, a still screen's picture again once a second so the stream
//! never looks stalled, a keyframe every [`IDR_EVERY`] and whenever a
//! receiver asks (PLI or FIR; Chime also asks content senders every
//! 10 s), and the bitrate as the bandwidth estimate allows (up to 2.5
//! Mbit/s). It hands each access unit to the session with its 90 kHz RTP
//! time, from when the helper captured it.
//!
//! A share whose capture ends (the compositor's "stop sharing", a window
//! closed) or whose helper fails stops here, and says why through its
//! [`Ending`].

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use tokio::sync::{mpsc, watch};

use super::camera_send::{
    EncodeCounts, RETUNE_GPU_EVERY, SendControl, VideoFrame, rtp_time, step_within,
};
use super::chime::VideoSend;
use super::helper::{RemoteShare, ShareTrouble};
use super::microphone::Running;
use super::share::{FPS, MAX_SIZE, ShareProblem};
use super::video_encoder::Limits;

/// How SUBSCRIBE describes a share: what it sends at most.
pub const DESCRIPTOR: VideoSend = VideoSend {
    width: MAX_SIZE.0,
    height: MAX_SIZE.1,
    fps: FPS,
    max_kbps: Limits::SHARE.max_bitrate / 1000,
};
/// A keyframe at least this often, even from a still screen: what a
/// receiver who joined late or lost one waits at most.
pub const IDR_EVERY: Duration = Duration::from_secs(4);
/// A still screen's picture is sent again at least this often.
pub const KEEPALIVE: Duration = Duration::from_secs(1);
/// A keyframe asked for by a receiver at most this often (as the
/// camera's).
const KEYFRAME_GAP: Duration = Duration::from_millis(500);
/// How long the helper may wait for a new picture each time it is asked.
const WAIT: Duration = Duration::from_millis(50);
/// How often the numbers reach the log.
const REPORT_EVERY: Duration = Duration::from_secs(10);

/// How a share ended by itself.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Ending {
    /// The capture ended: the system's own "stop sharing", the window
    /// closed.
    Ended,
    /// The capture failed, or the helper did.
    Failed(crate::failure::Failure),
}

/// What a helper's failure means for the share.
pub fn ending(trouble: &ShareTrouble) -> Ending {
    use crate::failure::{Failure, HuddleTrouble};
    match trouble {
        ShareTrouble::Problem(ShareProblem::Ended, _) => Ending::Ended,
        ShareTrouble::Problem(problem, _) => {
            Ending::Failed(super::share::problem_failure(*problem))
        }
        ShareTrouble::Lost(_) => Ending::Failed(Failure::Huddle(HuddleTrouble::ShareHelperLost)),
    }
}

/// When to ask the helper for a picture, and what for: a frame's time
/// after the last one sent; a keyframe when one is due or asked for, the
/// last picture again when the screen has been still for
/// [`KEEPALIVE`].
#[derive(Clone, Copy, Debug, Default)]
pub struct Gate {
    last_sent: Option<Instant>,
}

/// What [`Gate::ask`] says to ask the helper for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Asking {
    /// Make it a keyframe.
    pub force_keyframe: bool,
    /// Encode the last picture again if nothing new came.
    pub repeat: bool,
}

impl Gate {
    /// How long to wait before asking at `now`; zero when it is time.
    pub fn wait(&self, now: Instant) -> Duration {
        // A little under a frame's time, so 15 a second are not missed
        // by a capture a millisecond early.
        let frame = Duration::from_secs(1) / FPS - Duration::from_millis(5);
        self.last_sent.map_or(Duration::ZERO, |at| {
            (at + frame).saturating_duration_since(now)
        })
    }

    /// What to ask for at `now`: `asked` a receiver wants a keyframe,
    /// `last_keyframe` when the last was made.
    pub fn ask(&self, now: Instant, asked: bool, last_keyframe: Option<Instant>) -> Asking {
        let idr_due = last_keyframe.is_none_or(|at| now >= at + IDR_EVERY);
        let keepalive = self.last_sent.is_none_or(|at| now >= at + KEEPALIVE);
        let force_keyframe = idr_due || asked;
        Asking {
            force_keyframe,
            repeat: force_keyframe || keepalive,
        }
    }

    /// A picture went out at `now`.
    pub fn sent(&mut self, now: Instant) {
        self.last_sent = Some(now);
    }
}

/// The share's sending thread: it stops, and is joined, when dropped,
/// and the share in the helper with it.
#[derive(Debug)]
pub struct Encoding {
    _running: Running,
}

impl Encoding {
    /// Starts sending what `share` encodes into `frames`, as `control`
    /// (a share's: [`SendControl::new`] with [`Limits::SHARE`]) says;
    /// tells `ended` if the share ends by itself.
    pub fn spawn(
        share: RemoteShare,
        frames: mpsc::Sender<VideoFrame>,
        control: SendControl,
        ended: watch::Sender<Option<Ending>>,
    ) -> Result<Self, String> {
        let stop = Arc::new(AtomicBool::new(false));
        let thread_stop = stop.clone();
        let thread = std::thread::Builder::new()
            .name("noslacking-share-sender".into())
            .spawn(move || {
                let ending = send(share, &frames, &control, &thread_stop);
                if let Some(ending) = ending {
                    ended.send_replace(Some(ending));
                }
            })
            .map_err(|e| format!("no share thread: {e}"))?;
        Ok(Self {
            _running: Running::new(stop, thread),
        })
    }
}

/// The sending thread: asks `share` for pictures as the [`Gate`] says
/// until `stop`, or until the share ends by itself (then why).
fn send(
    mut share: RemoteShare,
    frames: &mpsc::Sender<VideoFrame>,
    control: &SendControl,
    stop: &AtomicBool,
) -> Option<Ending> {
    let mut gate = Gate::default();
    let mut counts = EncodeCounts::default();
    let (mut gpu, mut software, mut repeated) = (0u64, 0u64, 0u64);
    let mut last_keyframe: Option<Instant> = None;
    // The next picture must be a keyframe: one the session could not take
    // was dropped, and what follows would refer to it.
    let mut force_next = false;
    let mut bitrate = control.bitrate();
    let mut retuned: Option<Instant> = None;
    let mut size = (0, 0);
    let epoch = Instant::now();
    let mut last_time = 0u64;
    let mut next_report = Instant::now() + REPORT_EVERY;
    while !stop.load(Ordering::Relaxed) {
        let now = Instant::now();
        let wait = gate.wait(now);
        if !wait.is_zero() {
            std::thread::park_timeout(wait);
            continue;
        }
        // The bandwidth estimate, in steps, taken in place by the encoder.
        let wanted = step_within(control.bitrate(), Limits::SHARE.max_bitrate);
        if wanted != bitrate && retuned.is_none_or(|at| now >= at + RETUNE_GPU_EVERY) {
            if let Err(trouble) = share.set_bitrate(wanted) {
                log::warn!("huddle share: {trouble:?}");
                return Some(ending(&trouble));
            }
            bitrate = wanted;
            retuned = Some(now);
        }
        // A receiver's request a moment after a keyframe waits its turn.
        let asked = force_next
            || (control.keyframe_wanted()
                && last_keyframe.is_none_or(|at| now >= at + KEYFRAME_GAP));
        let asking = gate.ask(now, asked, last_keyframe);
        let frame = match share.next(asking.force_keyframe, asking.repeat, WAIT) {
            Ok(Some(frame)) => frame,
            Ok(None) => continue,
            Err(trouble) => {
                log::info!("huddle share: the share stopped: {trouble:?}");
                return Some(ending(&trouble));
            }
        };
        let now = Instant::now();
        if asking.force_keyframe {
            control.take_keyframe();
            force_next = false;
        }
        // A fresh picture carries its capture time; one sent again, now.
        let at = now
            .checked_sub(Duration::from_micros(u64::from(frame.age_us)))
            .unwrap_or(now);
        // Paced from the capture, not from when encoding finished: the
        // next ask comes a little before the next picture, which is then
        // encoded as it arrives.
        gate.sent(at);
        if frame.keyframe {
            counts.keyframes += 1;
            last_keyframe = Some(now);
        }
        if frame.hardware {
            gpu += 1;
        } else {
            software += 1;
        }
        if frame.age_us == 0 {
            repeated += 1;
        }
        if (frame.width, frame.height) != size {
            size = (frame.width, frame.height);
            log::info!(
                "huddle share: sending {}x{} at {} kbit/s, {FPS} a second, encoded {}",
                size.0,
                size.1,
                bitrate / 1000,
                if frame.hardware {
                    "on the GPU"
                } else {
                    "in software"
                }
            );
        }
        counts.encoded += 1;
        counts.bytes += frame.data.len() as u64;
        let time = rtp_time(epoch, at).max(last_time + 1);
        last_time = time;
        let sent = VideoFrame {
            data: frame.data,
            keyframe: frame.keyframe,
            time,
            at,
        };
        match frames.try_send(sent) {
            Ok(()) => {}
            Err(mpsc::error::TrySendError::Full(_)) => {
                counts.dropped += 1;
                force_next = true;
            }
            Err(mpsc::error::TrySendError::Closed(_)) => break,
        }
        if now >= next_report {
            next_report += REPORT_EVERY;
            log::info!(
                "huddle share: {counts:?}; {gpu} from the GPU, {software} from software, \
                 {repeated} sent again"
            );
        }
    }
    log::info!("huddle share: stopped sending; {counts:?}");
    None
}

/// What the share's session is handed: [`super::camera_send::CameraUplink`]
/// with the share's descriptor; `on` true for as long as it runs.
pub fn uplink(
    frames: mpsc::Receiver<VideoFrame>,
    on: tokio::sync::watch::Receiver<bool>,
    control: SendControl,
    refused: mpsc::Sender<()>,
) -> super::camera_send::CameraUplink {
    super::camera_send::CameraUplink {
        frames,
        on,
        control,
        refused,
        descriptor: DESCRIPTOR,
    }
}

#[cfg(test)]
mod tests {
    use super::super::helper::pretend::{Act, Pretend, welcome};
    use super::super::helper::{self, Helper, Lane};
    use super::*;
    use noslacking_video_ipc::{self as ipc, Reply, Request, ShareChoice};

    const FRAME: Duration = Duration::from_millis(67);

    #[test]
    fn a_still_screen_is_asked_for_a_keyframe_first_then_once_a_second() {
        let start = Instant::now();
        let mut gate = Gate::default();
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
            ending(&ShareTrouble::Problem(ShareProblem::Ended, "closed".into())),
            Ending::Ended
        );
        assert_eq!(
            ending(&ShareTrouble::Problem(ShareProblem::Failed, "x".into())),
            Ending::Failed(Failure::Huddle(HuddleTrouble::ShareCapture))
        );
        assert_eq!(
            ending(&ShareTrouble::Lost("it crashed".into())),
            Ending::Failed(Failure::Huddle(HuddleTrouble::ShareHelperLost))
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
        let encoding = Encoding::spawn(share, frames, control, ended).expect("a thread");
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
            Request::StartShare { .. } => Act::Reply(Reply::ShareStarted {
                id: 1,
                restore: String::new(),
            }),
            Request::NextShareFrame {
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
                Act::Reply(Reply::ShareFrame(ipc::ShareFrame {
                    keyframe: *force_keyframe,
                    hardware: true,
                    width: 1920,
                    height: 1080,
                    age_us: 2_000,
                    data,
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
        let encoding = Encoding::spawn(share, frames, control.clone(), ended).expect("a thread");
        let first = collect(&mut out, 3);
        control.want_keyframe();
        // Half a second after the last keyframe at most, it comes.
        let after = collect(&mut out, 9);
        drop(encoding);
        assert!(first[0].keyframe && !first[1].keyframe && !first[2].keyframe);
        assert!(after.iter().any(|f| f.keyframe), "a keyframe after the PLI");
        assert!(!control.keyframe_wanted());
    }

    /// A helper that crashes mid-share ends it, saying so.
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
        let encoding = Encoding::spawn(share, frames, SendControl::new(Limits::SHARE), ended)
            .expect("a thread");
        let got = collect(&mut out, 5);
        assert_eq!(got.len(), 3, "three frames, then the crash");
        let deadline = Instant::now() + Duration::from_secs(5);
        while told.borrow_and_update().is_none() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(
            *told.borrow(),
            Some(Ending::Failed(Failure::Huddle(
                HuddleTrouble::ShareHelperLost
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
