//! A screen share on its way out (the `huddle-share` feature): the
//! encoder's thread between the capture and the share's own session.
//!
//! The capture puts each picture in a [`Frames`] slot; this thread takes
//! the newest, and if it differs from the last one (a screen is mostly
//! still, and PipeWire under GNOME sends a frame only when something
//! changed) shrinks it to at most 1920×1080 keeping its shape, encodes it
//! with the camera's [`Encoder`] (on the GPU through the video helper
//! when it can, else in software, which takes 1280×720 at most; up to
//! 2.5 Mbit/s as the bandwidth estimate allows) and hands it to the
//! session. At most [`FPS`] pictures a
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
use super::camera_send::{
    EncodeCounts, RETUNE_EVERY, RETUNE_GPU_EVERY, SendControl, VideoFrame, rtp_time, step_within,
};
use super::chime::VideoSend;
use super::helper::{self, Helper, Lane};
use super::microphone::Running;
use super::share::{FPS, Frames, MAX_SIZE, REDUCED_SIZE, ShareFrame, unchanged};
use super::video_encoder::{EncodeTrouble, Encoded, Encoder, Limits, Settings};

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
    /// [`Limits::SHARE`]) says: on the GPU through the video helper when
    /// the setting is on and the helper encodes the size, else in
    /// software.
    pub fn spawn(
        latest: Frames,
        frames: mpsc::Sender<VideoFrame>,
        control: SendControl,
    ) -> Result<Self, String> {
        Self::spawn_with(latest, frames, control, || {
            helper::gpu()
                .then(|| helper::shared(Lane::Sending))
                .flatten()
        })
    }

    /// The same, on the GPU through the helper `gpu` gives, if any.
    pub fn spawn_with(
        latest: Frames,
        frames: mpsc::Sender<VideoFrame>,
        control: SendControl,
        gpu: impl FnOnce() -> Option<Helper> + Send + 'static,
    ) -> Result<Self, String> {
        let stop = Arc::new(AtomicBool::new(false));
        let thread_stop = stop.clone();
        let thread = std::thread::Builder::new()
            .name("noslacking-share-encoder".into())
            .spawn(move || {
                // Asked for here: starting the helper may take a moment.
                let sender = Sender::new(gpu());
                encode(&latest, &frames, &control, sender, &thread_stop);
            })
            .map_err(|e| format!("no share encoder thread: {e}"))?;
        Ok(Self {
            _running: Running::new(stop, thread),
        })
    }
}

/// The share's encoder as its thread keeps it: made for the picture's
/// size (on the GPU while the helper serves, in software once it has
/// failed), following the bandwidth estimate in steps (in place on the
/// GPU; a new software encoder at most every [`RETUNE_EVERY`]), with
/// keyframes when asked and after anything was lost.
struct Sender {
    encoder: Option<Encoder>,
    gpu: Option<Helper>,
    retuned: Option<Instant>,
    last_keyframe: Option<Instant>,
    force_keyframe: bool,
    counts: EncodeCounts,
    busy: Duration,
}

impl Sender {
    fn new(gpu: Option<Helper>) -> Self {
        Self {
            encoder: None,
            gpu,
            retuned: None,
            last_keyframe: None,
            force_keyframe: false,
            counts: EncodeCounts::default(),
            busy: Duration::ZERO,
        }
    }

    /// Mean encoding time a picture so far, in milliseconds.
    fn mean_ms(&self) -> f64 {
        self.busy.as_secs_f64() * 1000.0 / self.counts.encoded.max(1) as f64
    }

    /// Encodes `picture` as `control` says at `now`. `Err(Size)` when no
    /// encoder takes its size (1080p without a GPU that encodes it): the
    /// caller shrinks it. Any other failure is `Ok(None)`, and the next
    /// picture starts afresh.
    fn encode(
        &mut self,
        picture: &I420,
        control: &SendControl,
        now: Instant,
    ) -> Result<Option<Encoded>, EncodeTrouble> {
        let limits = control.limits();
        let wanted = step_within(control.bitrate(), limits.max_bitrate);
        let rebuild = match &mut self.encoder {
            None => true,
            Some(e) => {
                let s = e.settings();
                if (s.width, s.height) != (picture.width, picture.height) {
                    true
                } else if s.bitrate == wanted {
                    false
                } else if e.on_gpu() {
                    if self.retuned.is_none_or(|at| now >= at + RETUNE_GPU_EVERY) {
                        if !e.retune(wanted) {
                            // The helper is gone: software from here on.
                            self.gpu = None;
                            self.counts.gpu_failures += 1;
                        }
                        self.retuned = Some(now);
                    }
                    false
                } else {
                    self.retuned.is_none_or(|at| now >= at + RETUNE_EVERY)
                }
            }
        };
        if rebuild {
            let settings = Settings {
                width: picture.width,
                height: picture.height,
                fps: FPS,
                bitrate: wanted,
                limits,
            };
            match Encoder::new(settings, self.gpu.as_ref()) {
                Ok(fresh) => {
                    log::info!(
                        "huddle share: encoding {}x{} at {} kbit/s, {FPS} a second, {}",
                        settings.width,
                        settings.height,
                        fresh.settings().bitrate / 1000,
                        if fresh.on_gpu() {
                            "on the GPU"
                        } else {
                            "in software"
                        }
                    );
                    self.encoder = Some(fresh);
                    self.retuned = Some(now);
                }
                Err(EncodeTrouble::Size(w, h)) => return Err(EncodeTrouble::Size(w, h)),
                Err(error) => {
                    self.counts.failed += 1;
                    log::warn!("huddle share: {error}");
                    return Ok(None);
                }
            }
        }
        let Some(active) = self.encoder.as_mut() else {
            return Ok(None);
        };
        if std::mem::take(&mut self.force_keyframe) {
            active.request_keyframe();
        }
        let started = Instant::now();
        let was_on_gpu = active.on_gpu();
        let encoded = match active.encode(picture) {
            Ok(encoded) => encoded,
            Err(error) => {
                self.counts.failed += 1;
                log::debug!("huddle share: {error}");
                if was_on_gpu {
                    // The GPU failed and software could not take over at
                    // this size: software, shrunk, from here on.
                    self.gpu = None;
                    self.counts.gpu_failures += 1;
                }
                // The reference chain may be broken: start over.
                self.encoder = None;
                return Ok(None);
            }
        };
        if was_on_gpu && !active.on_gpu() {
            self.gpu = None;
            self.counts.gpu_failures += 1;
        }
        self.busy += started.elapsed();
        self.counts.encoded += 1;
        self.counts.bytes += encoded.data.len() as u64;
        if encoded.keyframe {
            self.counts.keyframes += 1;
            self.last_keyframe = Some(now);
        }
        Ok(Some(encoded))
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
    mut sender: Sender,
    stop: &AtomicBool,
) {
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
                .last_keyframe
                .is_none_or(|at| now >= at + KEYFRAME_GAP);
        let Some(next) = gate.decide(now, current.pending, asked, sender.last_keyframe) else {
            continue;
        };
        if next == Next::Keyframe {
            control.take_keyframe();
            sender.force_keyframe = true;
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
        let encoded = match sender.encode(&current.picture, control, now) {
            Ok(Some(encoded)) => encoded,
            Ok(None) => continue,
            Err(_) if size == MAX_SIZE => {
                // No encoder here takes 1080p: 720p, which software does.
                log::info!(
                    "huddle share: no encoder for {}x{} here (no GPU that encodes it); \
                     sending {}x{}",
                    current.picture.width,
                    current.picture.height,
                    REDUCED_SIZE.0,
                    REDUCED_SIZE.1
                );
                size = REDUCED_SIZE;
                held = None;
                continue;
            }
            Err(error) => {
                counts.unusable += 1;
                log::debug!("huddle share: {error}");
                held = None;
                continue;
            }
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
        match frames.try_send(frame) {
            Ok(()) => {}
            Err(mpsc::error::TrySendError::Full(_)) => {
                // What follows would refer to it: a keyframe next.
                sender.counts.dropped += 1;
                sender.force_keyframe = true;
            }
            Err(mpsc::error::TrySendError::Closed(_)) => break,
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
                sender.counts,
                sender.mean_ms(),
                latest.replaced()
            );
        }
    }
    log::info!(
        "huddle share: encoder stopped; {:?}, {counts:?}",
        sender.counts
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

    /// The test screen through the share's encoder thread, no GPU: 720p H.264,
    /// a keyframe first, still pictures skipped (the test screen's clock
    /// moves, so here every picture is new), RTP time going up, and
    /// nothing once it stops.
    #[test]
    fn the_test_screen_goes_out_as_720p_h264_in_software() {
        let latest = Frames::default();
        let (frames, mut out) = mpsc::channel(8);
        let control = SendControl::new(Limits::SHARE);
        let encoding = Encoding::spawn_with(latest.clone(), frames, control.clone(), || None)
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
        // Without a GPU, 1080p is shrunk to what software encodes.
        assert_eq!((picture.width, picture.height), REDUCED_SIZE);
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
            Encoding::spawn_with(latest.clone(), frames, control, || None).expect("a thread");
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

    /// With a GPU that encodes 1080p (a pretend helper here), the test
    /// screen goes out at 1080p from it, at the share's bitrate, not
    /// shrunk to what software takes.
    #[test]
    fn a_gpu_shares_at_1080p() {
        use super::super::helper::pretend::{Act, Pretend, welcome};
        use noslacking_video_ipc::{Reply, Request};
        use std::sync::Mutex;
        // Width, height, frames a second, bitrate.
        type Opened = Vec<(u32, u32, u32, u32)>;
        let opened: Arc<Mutex<Opened>> = Arc::default();
        let seen = Arc::clone(&opened);
        let pretend = Pretend::new(move |request| match request {
            Request::Hello { .. } => Act::Reply(welcome()),
            Request::OpenEncoder {
                width,
                height,
                fps,
                bitrate,
                ..
            } => {
                seen.lock()
                    .expect("not poisoned")
                    .push((*width, *height, *fps, *bitrate));
                Act::Reply(Reply::Opened { id: 1 })
            }
            Request::Encode { force_keyframe, .. } => {
                // A stand-in access unit: SPS, PPS and an IDR when asked,
                // else a P slice.
                let data = if *force_keyframe {
                    vec![
                        0, 0, 0, 1, 0x67, 1, 0, 0, 0, 1, 0x68, 1, 0, 0, 0, 1, 0x65, 1,
                    ]
                } else {
                    vec![0, 0, 0, 1, 0x41, 1]
                };
                Act::Reply(Reply::Encoded {
                    keyframe: *force_keyframe,
                    data,
                })
            }
            _ => Act::Reply(Reply::Done),
        });
        let helper = Helper::with_timeouts(
            Arc::new(pretend),
            Duration::from_secs(5),
            Duration::from_secs(1),
        );
        let latest = Frames::default();
        let (frames, mut out) = mpsc::channel(8);
        let encoding = Encoding::spawn_with(
            latest.clone(),
            frames,
            SendControl::new(Limits::SHARE),
            move || Some(helper),
        )
        .expect("a thread");
        let mut share = ShareControl::new(TestShare::new(latest));
        share
            .start(&Choice::System { again: false })
            .expect("started");
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut got = Vec::new();
        while got.len() < 2 && Instant::now() < deadline {
            match out.try_recv() {
                Ok(frame) => got.push(frame),
                Err(_) => std::thread::sleep(Duration::from_millis(10)),
            }
        }
        share.stop();
        drop(encoding);
        assert!(got.len() >= 2, "only {} frames", got.len());
        assert!(got[0].keyframe);
        let opened = opened.lock().expect("not poisoned").clone();
        assert_eq!(
            opened.first().map(|o| (o.0, o.1, o.2)),
            Some((1920, 1080, 15))
        );
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
