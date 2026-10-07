//! A screen share on its way out (the `huddle-share` feature): the
//! encoder's thread between the capture and the share's own session.
//!
//! The capture puts each picture in a [`Frames`] slot; this thread takes
//! the newest, and if it differs from the last one (a screen is mostly
//! still, and PipeWire under GNOME sends a frame only when something
//! changed) shrinks it to at most 1920×1080 keeping its shape, encodes it
//! with the camera's [`Sender`] (the same encoder interface, with a
//! share's limits: level 4.0, up to 2.5 Mbit/s as the bandwidth estimate
//! allows) and hands it to the session. At most [`FPS`] pictures a
//! second; a still screen still sends its picture again once a second,
//! so the stream never looks stalled, and as a keyframe every
//! [`IDR_EVERY`] and whenever a receiver asks (PLI or FIR; Chime also
//! asks content senders every 10 s).
//!
//! If 1080p takes too long to encode on this machine (more than
//! [`SLOW`] a picture on average), the share steps down to 1280×720 for
//! the rest of it and says so in the log.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use tokio::sync::mpsc;

use super::camera::I420;
use super::camera_send::{SendControl, Sender, VideoFrame, hand_over, rtp_time};
use super::chime::VideoSend;
use super::microphone::Running;
use super::share::{FPS, Frames, MAX_SIZE, REDUCED_SIZE, ShareFrame, unchanged};
use super::video_encoder::{Backend, Limits};

/// How SUBSCRIBE describes a share: what it sends at most.
pub const DESCRIPTOR: VideoSend = VideoSend {
    width: MAX_SIZE.0 as u32,
    height: MAX_SIZE.1 as u32,
    fps: FPS,
    max_kbps: Limits::SHARE.max_bitrate / 1000,
};
/// A keyframe at least this often, even from a still screen: what a
/// receiver who joined late or lost one waits at most.
pub const IDR_EVERY: Duration = Duration::from_secs(4);
/// A still screen's picture is sent again at least this often.
pub const KEEPALIVE: Duration = Duration::from_secs(1);
/// Encoding slower than this on average steps the share down to 720p:
/// two thirds of a frame's time at 15 a second.
pub const SLOW: Duration = Duration::from_millis(45);
/// A keyframe asked for by a receiver at most this often (as the
/// camera's).
const KEYFRAME_GAP: Duration = Duration::from_millis(500);
/// How many pictures the average for [`SLOW`] is taken over.
const SLOW_OVER: u32 = 30;
/// How often the numbers reach the log.
const REPORT_EVERY: Duration = Duration::from_secs(10);

/// When to encode: the newest picture not sent yet as soon as a frame's
/// time has passed since the last; a still screen's last picture again
/// after [`KEEPALIVE`], or at once as a keyframe when one is due or
/// asked for.
#[derive(Clone, Copy, Debug, Default)]
pub struct Gate {
    last_sent: Option<Instant>,
}

/// What [`Gate::decide`] says to do now.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Next {
    /// Encode the picture as it comes.
    Picture,
    /// Encode it as a keyframe.
    Keyframe,
}

impl Gate {
    /// Whether to encode at `now`: `pending` a picture not sent yet,
    /// `asked` a receiver wants a keyframe, `last_keyframe` when the last
    /// was made. `None` while there is nothing to send or it is too soon.
    pub fn decide(
        &mut self,
        now: Instant,
        pending: bool,
        asked: bool,
        last_keyframe: Option<Instant>,
    ) -> Option<Next> {
        // A little under a frame's time, so 15 a second is not missed by
        // a capture a millisecond early.
        let frame = Duration::from_secs(1) / FPS - Duration::from_millis(5);
        if self.last_sent.is_some_and(|at| now < at + frame) {
            return None;
        }
        let idr_due = last_keyframe.is_none_or(|at| now >= at + IDR_EVERY);
        let keepalive = self.last_sent.is_none_or(|at| now >= at + KEEPALIVE);
        if !(pending || asked || idr_due || keepalive) {
            return None;
        }
        self.last_sent = Some(now);
        Some(if idr_due || asked {
            Next::Keyframe
        } else {
            Next::Picture
        })
    }
}

/// What the share's encoder did, for the log.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ShareCounts {
    /// Pictures captured that were the same as the last.
    pub unchanged: u64,
    /// Pictures that could not be converted (broken, too small).
    pub unusable: u64,
    /// A still picture sent again.
    pub repeated: u64,
}

/// The share's encoder thread: it stops, and is joined, when dropped.
#[derive(Debug)]
pub struct Encoding {
    _running: Running,
}

impl Encoding {
    /// Starts encoding what arrives in `latest` into `frames`, as
    /// `control` (a share's: [`SendControl::new`] with
    /// [`Limits::SHARE`]) says, with `backend`'s encoder.
    pub fn spawn(
        latest: Frames,
        frames: mpsc::Sender<VideoFrame>,
        control: SendControl,
        backend: Backend,
    ) -> Result<Self, String> {
        let stop = Arc::new(AtomicBool::new(false));
        let thread_stop = stop.clone();
        let thread = std::thread::Builder::new()
            .name("noslacking-share-encoder".into())
            .spawn(move || encode(&latest, &frames, &control, backend, &thread_stop))
            .map_err(|e| format!("no share encoder thread: {e}"))?;
        Ok(Self {
            _running: Running::new(stop, thread),
        })
    }
}

/// The newest picture as sent, and what it came from.
struct Held {
    /// As captured, to compare the next with.
    frame: ShareFrame,
    /// Converted and shrunk.
    picture: I420,
    /// Not sent yet.
    pending: bool,
}

/// The encoder's thread.
fn encode(
    latest: &Frames,
    frames: &mpsc::Sender<VideoFrame>,
    control: &SendControl,
    backend: Backend,
    stop: &AtomicBool,
) {
    let mut sender = Sender::new(backend, "share");
    let mut gate = Gate::default();
    let mut held: Option<Held> = None;
    let mut size = MAX_SIZE;
    let mut counts = ShareCounts::default();
    let epoch = Instant::now();
    let mut last_time = 0u64;
    // The encoding time over the last pictures, for SLOW.
    let (mut window, mut window_busy) = (0u32, Duration::ZERO);
    let mut next_report = Instant::now() + REPORT_EVERY;
    let wait = Duration::from_secs(1) / FPS / 2;
    while !stop.load(Ordering::Relaxed) {
        if let Some(frame) = latest.take(wait) {
            if held
                .as_ref()
                .is_some_and(|h| unchanged(&h.frame.picture, &frame.picture))
            {
                counts.unchanged += 1;
            } else {
                match frame.picture.to_send(size) {
                    Some(picture) => {
                        held = Some(Held {
                            frame,
                            picture,
                            pending: true,
                        });
                    }
                    None => counts.unusable += 1,
                }
            }
        }
        let Some(current) = held.as_mut() else {
            continue;
        };
        let now = Instant::now();
        // A receiver's request a moment after a keyframe waits its turn.
        let asked = control.keyframe_wanted()
            && sender
                .last_keyframe()
                .is_none_or(|at| now >= at + KEYFRAME_GAP);
        let Some(send) = gate.decide(now, current.pending, asked, sender.last_keyframe()) else {
            continue;
        };
        if send == Next::Keyframe {
            control.take_keyframe();
            sender.restart();
        }
        // A fresh picture carries its capture time; one sent again, now.
        let at = if current.pending {
            current.frame.at
        } else {
            counts.repeated += 1;
            now
        };
        current.pending = false;
        let started = Instant::now();
        let Some(encoded) = sender.encode(&current.picture, FPS, control, now) else {
            continue;
        };
        window += 1;
        window_busy += started.elapsed();
        let time = rtp_time(epoch, at).max(last_time + 1);
        last_time = time;
        let frame = VideoFrame {
            data: encoded.data,
            keyframe: encoded.keyframe,
            time,
            at,
        };
        if !hand_over(frames, &mut sender, frame) {
            break;
        }
        if window >= SLOW_OVER {
            let mean = window_busy / window;
            if mean > SLOW && size == MAX_SIZE {
                log::warn!(
                    "huddle share: encoding takes {} ms a picture, too slow for 1080p at \
                     {FPS} a second here; sending {}x{} from now on",
                    mean.as_millis(),
                    REDUCED_SIZE.0,
                    REDUCED_SIZE.1
                );
                size = REDUCED_SIZE;
                // The next picture is converted at the new size.
                held = None;
            }
            (window, window_busy) = (0, Duration::ZERO);
        }
        if Instant::now() >= next_report {
            next_report += REPORT_EVERY;
            log::info!(
                "huddle share: {:?}, {counts:?}; {:.1} ms a picture; {} pictures replaced \
                 before encoding",
                sender.counts(),
                sender.mean_ms(),
                latest.replaced()
            );
        }
    }
    log::info!(
        "huddle share: encoder stopped; {:?}, {counts:?}",
        sender.counts()
    );
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
    use super::super::share::{Choice, Picture, ShareControl, TestShare};
    use super::*;

    const FRAME: Duration = Duration::from_millis(67);

    #[test]
    fn a_still_screen_sends_a_keyframe_first_then_once_a_second() {
        let start = Instant::now();
        let mut gate = Gate::default();
        // The first picture is a keyframe.
        assert_eq!(gate.decide(start, true, false, None), Some(Next::Keyframe));
        let keyframe = Some(start);
        // Nothing new: nothing for a second.
        for n in 1..15 {
            let at = start + FRAME * n;
            assert_eq!(gate.decide(at, false, false, keyframe), None, "frame {n}");
        }
        // Then the same picture again.
        let second = start + KEEPALIVE + Duration::from_millis(1);
        assert_eq!(
            gate.decide(second, false, false, keyframe),
            Some(Next::Picture)
        );
        // And a keyframe once IDR_EVERY has passed, still or not.
        let idr = start + IDR_EVERY;
        assert_eq!(
            gate.decide(idr, false, false, keyframe),
            Some(Next::Keyframe)
        );
    }

    #[test]
    fn new_pictures_go_at_most_fifteen_a_second_and_keyframes_when_asked() {
        let start = Instant::now();
        let mut gate = Gate::default();
        assert!(gate.decide(start, true, false, None).is_some());
        let keyframe = Some(start);
        // A new picture 20 ms later waits for its frame's time.
        assert_eq!(
            gate.decide(start + Duration::from_millis(20), true, false, keyframe),
            None
        );
        assert_eq!(
            gate.decide(start + FRAME, true, false, keyframe),
            Some(Next::Picture)
        );
        // A receiver's PLI: a keyframe, even of a still screen.
        assert_eq!(
            gate.decide(start + FRAME * 2, false, true, keyframe),
            Some(Next::Keyframe)
        );
        // Counted by frames sent: over a second of new pictures every
        // 10 ms, at most 15 go.
        let mut gate = Gate::default();
        let sent = (0..100)
            .filter(|n| {
                gate.decide(start + Duration::from_millis(n * 10), true, false, keyframe)
                    .is_some()
            })
            .count();
        assert!((14..=16).contains(&sent), "{sent} sent");
    }

    /// The test screen through the share's encoder thread: 1080p H.264,
    /// a keyframe first, still pictures skipped (the test screen's clock
    /// moves, so here every picture is new), RTP time going up, and
    /// nothing once it stops.
    #[test]
    fn the_test_screen_goes_out_as_1080p_h264() {
        let latest = Frames::default();
        let (frames, mut out) = mpsc::channel(8);
        let control = SendControl::new(Limits::SHARE);
        let encoding = Encoding::spawn(latest.clone(), frames, control.clone(), Backend::Software)
            .expect("a thread");
        let mut share = ShareControl::new(TestShare::new(latest.clone()));
        share
            .start(&Choice::System { again: false })
            .expect("started");
        let mut got = Vec::new();
        let deadline = Instant::now() + Duration::from_secs(20);
        while got.len() < 3 && Instant::now() < deadline {
            match out.try_recv() {
                Ok(frame) => got.push(frame),
                Err(_) => std::thread::sleep(Duration::from_millis(10)),
            }
        }
        share.stop();
        drop(encoding);
        assert!(got.len() >= 3, "only {} frames", got.len());
        assert!(got[0].keyframe);
        assert!(got.windows(2).all(|w| w[1].time > w[0].time));
        let picture = rusty_h264_decoder::Decoder::new()
            .decode(&got[0].data)
            .expect("decodes")
            .expect("a picture");
        assert_eq!((picture.width, picture.height), MAX_SIZE);
        while out.try_recv().is_ok() {}
        std::thread::sleep(Duration::from_millis(100));
        assert!(out.try_recv().is_err(), "nothing after it stopped");
    }

    /// A screen that does not change is encoded once, then only again
    /// as the keepalive: the unchanged pictures are skipped.
    #[test]
    fn unchanged_pictures_are_not_encoded_again() {
        let latest = Frames::default();
        let (frames, mut out) = mpsc::channel(64);
        let control = SendControl::new(Limits::SHARE);
        let encoding =
            Encoding::spawn(latest.clone(), frames, control, Backend::Software).expect("a thread");
        let still = super::super::camera::pattern(320, 240, 0, Duration::ZERO);
        let started = Instant::now();
        // Half a second of the same picture, 30 a second.
        while started.elapsed() < Duration::from_millis(500) {
            latest.put(ShareFrame {
                picture: Picture::I420(still.clone()),
                at: Instant::now(),
            });
            std::thread::sleep(Duration::from_millis(33));
        }
        std::thread::sleep(Duration::from_millis(100));
        drop(encoding);
        let mut sent = 0;
        while out.try_recv().is_ok() {
            sent += 1;
        }
        assert_eq!(sent, 1, "one picture for half a second of a still screen");
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
