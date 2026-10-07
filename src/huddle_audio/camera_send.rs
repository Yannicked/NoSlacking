//! Our camera on its way out (the `huddle-camera` feature): the encoder's
//! thread between the camera and the session, the controls the session
//! turns (keyframes, bitrate), and the self-preview the call bar shows.
//!
//! The camera's thread puts each picture in a [`Latest`]; [`Encoding`]'s
//! thread takes the newest, encodes it ([`VideoEncoder`]) and hands it to
//! the session as a [`VideoFrame`] with its 90 kHz RTP time. Nothing
//! queues: a picture the encoder had no time for is replaced by the next,
//! and an encoded one the session cannot take (its queue full) is dropped
//! and the next made a keyframe, since what follows would refer to it.
//! Every other picture is also shrunk, mirrored as a mirror shows you,
//! and turned into egui's pixels for the [`Preview`]; neither the runtime
//! nor the interface ever encodes or converts.

use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use egui::{Color32, ColorImage};
use tokio::sync::mpsc;

use super::camera::{FPS, I420, Latest, MAX_HEIGHT, MAX_WIDTH};
use super::chime::VideoSend;
use super::microphone::Running;
use super::video_encoder::{self, Settings, VideoEncoder};

/// How SUBSCRIBE describes our camera: what it sends at most.
pub const DESCRIPTOR: VideoSend = VideoSend {
    width: MAX_WIDTH as u32,
    height: MAX_HEIGHT as u32,
    fps: FPS,
    max_kbps: video_encoder::MAX_BITRATE / 1000,
};
/// Encoded frames waiting for the session at most: a few, since a frame
/// late by more than that is better dropped.
pub const QUEUE: usize = 8;
/// The bitrate changes the encoder follows at most this often: each
/// change starts a new encoder, and so costs a keyframe.
const RETUNE_EVERY: Duration = Duration::from_secs(8);
/// A keyframe asked for by a receiver at most this often: Chime may pass
/// on several receivers' PLIs at once.
const KEYFRAME_EVERY: Duration = Duration::from_millis(500);
/// The preview's width at most, in pixels.
const PREVIEW_WIDTH: usize = 320;
/// No picture for this long: the camera is off or stalled, and the
/// stream starts again with a keyframe.
const STALLED: Duration = Duration::from_millis(500);
/// How often the encoder's numbers reach the log.
const REPORT_EVERY: Duration = Duration::from_secs(10);

/// One encoded picture for the session to send.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VideoFrame {
    /// The access unit, Annex B.
    pub data: Vec<u8>,
    /// Whether it is an IDR, with SPS and PPS.
    pub keyframe: bool,
    /// Its RTP time, 90 kHz, from the first picture.
    pub time: u64,
    /// When it was captured.
    pub at: Instant,
}

/// What the session tells the encoder: a receiver wants a keyframe, the
/// bandwidth estimate changed.
#[derive(Clone, Debug)]
pub struct SendControl {
    shared: Arc<ControlShared>,
}

#[derive(Debug)]
struct ControlShared {
    keyframe: AtomicBool,
    bitrate: AtomicU32,
}

impl Default for SendControl {
    fn default() -> Self {
        Self {
            shared: Arc::new(ControlShared {
                keyframe: AtomicBool::new(false),
                bitrate: AtomicU32::new(video_encoder::START_BITRATE),
            }),
        }
    }
}

impl SendControl {
    /// The next picture should be a keyframe.
    pub fn want_keyframe(&self) {
        self.shared.keyframe.store(true, Ordering::Relaxed);
    }

    /// Whether a keyframe was asked for since the last call.
    fn take_keyframe(&self) -> bool {
        self.shared.keyframe.swap(false, Ordering::Relaxed)
    }

    /// The bitrate to aim at, in bit/s, kept within what is sent.
    pub fn set_bitrate(&self, bitrate: u32) {
        self.shared.bitrate.store(
            bitrate.clamp(video_encoder::MIN_BITRATE, video_encoder::MAX_BITRATE),
            Ordering::Relaxed,
        );
    }

    /// The bitrate aimed at now.
    pub fn bitrate(&self) -> u32 {
        self.shared.bitrate.load(Ordering::Relaxed)
    }
}

/// The bitrate an encoder made for `wanted` uses: a few steps, so small
/// swings of the estimate do not each cost a keyframe.
pub fn step(wanted: u32) -> u32 {
    const STEPS: [u32; 6] = [150_000, 250_000, 400_000, 600_000, 850_000, 1_200_000];
    STEPS
        .iter()
        .rev()
        .copied()
        .find(|&s| s <= wanted)
        .unwrap_or(STEPS[0])
}

/// The newest picture of our own camera for the call bar: put by the
/// encoder's thread, taken by the interface.
#[derive(Clone)]
pub struct Preview {
    shared: Arc<PreviewShared>,
}

struct PreviewShared {
    newest: Mutex<Option<Arc<ColorImage>>>,
    pictures: AtomicU64,
    wake: Box<dyn Fn() + Send + Sync>,
}

impl std::fmt::Debug for Preview {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Preview")
            .field("pictures", &self.pictures())
            .finish_non_exhaustive()
    }
}

/// One preview is only ever equal to itself (its clones).
impl PartialEq for Preview {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.shared, &other.shared)
    }
}

impl Preview {
    /// An empty preview; `wake` is called when a picture arrives that the
    /// interface has not been woken for yet.
    pub fn new(wake: impl Fn() + Send + Sync + 'static) -> Self {
        Self {
            shared: Arc::new(PreviewShared {
                newest: Mutex::new(None),
                pictures: AtomicU64::new(0),
                wake: Box::new(wake),
            }),
        }
    }

    fn newest(&self) -> std::sync::MutexGuard<'_, Option<Arc<ColorImage>>> {
        self.shared
            .newest
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    /// Puts the newest picture in place of one not taken.
    pub fn put(&self, image: ColorImage) {
        let was_empty = self.newest().replace(Arc::new(image)).is_none();
        self.shared.pictures.fetch_add(1, Ordering::Relaxed);
        if was_empty {
            (self.shared.wake)();
        }
    }

    /// The newest picture, if one came since the last taken.
    pub fn take(&self) -> Option<Arc<ColorImage>> {
        self.newest().take()
    }

    /// Forgets a picture not taken: the camera went off.
    pub fn clear(&self) {
        self.newest().take();
    }

    /// How many pictures have been put.
    pub fn pictures(&self) -> u64 {
        self.shared.pictures.load(Ordering::Relaxed)
    }
}

/// `picture` as egui's pixels for the preview: shrunk by whole steps to
/// at most 320 pixels wide and mirrored left to right.
pub fn preview_image(picture: &I420) -> Option<ColorImage> {
    if !picture.whole() {
        return None;
    }
    let by = picture.width.div_ceil(PREVIEW_WIDTH).max(1);
    let small = if by > 1 {
        super::camera::scale(
            picture,
            (picture.width / by).max(2),
            (picture.height / by).max(2),
        )
    } else {
        picture.clone()
    };
    let size = |n: usize| u32::try_from(n).ok();
    let image = yuv::YuvPlanarImage {
        y_plane: &small.y,
        y_stride: size(small.width)?,
        u_plane: &small.u,
        u_stride: size(small.width / 2)?,
        v_plane: &small.v,
        v_stride: size(small.width / 2)?,
        width: size(small.width)?,
        height: size(small.height)?,
    };
    let mut pixels = vec![Color32::BLACK; small.width * small.height];
    yuv::yuv420_to_rgba(
        &image,
        bytemuck::cast_slice_mut(&mut pixels),
        size(small.width * 4)?,
        yuv::YuvRange::Limited,
        yuv::YuvStandardMatrix::Bt601,
    )
    .ok()?;
    for row in pixels.chunks_exact_mut(small.width) {
        row.reverse();
    }
    Some(ColorImage::new([small.width, small.height], pixels))
}

/// What the encoder did, for the log.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct EncodeCounts {
    /// Pictures encoded.
    pub encoded: u64,
    /// Of those, keyframes.
    pub keyframes: u64,
    /// Encoded frames dropped because the session's queue was full.
    pub dropped: u64,
    /// Pictures the encoder refused.
    pub failed: u64,
    /// Bytes encoded.
    pub bytes: u64,
}

/// The encoder's thread for one session: it waits for pictures while the
/// camera is off and stops, and is joined, when dropped.
#[derive(Debug)]
pub struct Encoding {
    _running: Running,
}

impl Encoding {
    /// Starts encoding what arrives in `latest` into `frames`, as
    /// `control` says; `preview` gets every picture.
    pub fn spawn(
        latest: Latest,
        frames: mpsc::Sender<VideoFrame>,
        control: SendControl,
        preview: Option<Preview>,
    ) -> Result<Self, String> {
        let stop = Arc::new(AtomicBool::new(false));
        let thread_stop = stop.clone();
        let thread = std::thread::Builder::new()
            .name("noslacking-huddle-encoder".into())
            .spawn(move || encode(&latest, &frames, &control, preview.as_ref(), &thread_stop))
            .map_err(|e| format!("no encoder thread: {e}"))?;
        Ok(Self {
            _running: Running::new(stop, thread),
        })
    }
}

/// The RTP time of a picture taken at `at`, 90 kHz from `epoch`.
pub fn rtp_time(epoch: Instant, at: Instant) -> u64 {
    let micros = at.saturating_duration_since(epoch).as_micros();
    u64::try_from(micros * 9 / 100).unwrap_or(u64::MAX)
}

/// The encoder's thread.
fn encode(
    latest: &Latest,
    frames: &mpsc::Sender<VideoFrame>,
    control: &SendControl,
    preview: Option<&Preview>,
    stop: &AtomicBool,
) {
    let mut encoder: Option<VideoEncoder> = None;
    let mut epoch: Option<Instant> = None;
    let mut last_time = 0u64;
    let mut retuned = Instant::now();
    let mut last_keyframe: Option<Instant> = None;
    let mut force_keyframe = false;
    let mut counts = EncodeCounts::default();
    let mut busy = Duration::ZERO;
    let mut next_report = Instant::now() + REPORT_EVERY;
    while !stop.load(Ordering::Relaxed) {
        let Some(frame) = latest.take(STALLED) else {
            // The camera is off or stalled; when it comes back the stream
            // starts again with a keyframe.
            if encoder.is_some() {
                force_keyframe = true;
            }
            continue;
        };
        let now = Instant::now();
        let picture = &frame.picture;
        // Every picture, before encoding: the self-view keeps the camera's
        // pace instead of waiting for the encoder.
        if let Some(preview) = preview
            && let Some(image) = preview_image(picture)
        {
            preview.put(image);
        }
        let wanted = step(control.bitrate());
        let rebuild = match &encoder {
            None => true,
            Some(e) => {
                let s = e.settings();
                (s.width, s.height) != (picture.width, picture.height)
                    || (s.bitrate != wanted && now >= retuned + RETUNE_EVERY)
            }
        };
        if rebuild {
            let settings = Settings {
                width: picture.width,
                height: picture.height,
                fps: FPS,
                bitrate: wanted,
            };
            match VideoEncoder::new(settings) {
                Ok(fresh) => {
                    log::info!(
                        "huddle camera: encoding {}x{} at {} kbit/s",
                        settings.width,
                        settings.height,
                        fresh.settings().bitrate / 1000
                    );
                    encoder = Some(fresh);
                    retuned = now;
                }
                Err(error) => {
                    counts.failed += 1;
                    if counts.failed == 1 {
                        log::warn!("huddle camera: {error}");
                    }
                    continue;
                }
            }
        }
        let Some(active) = encoder.as_mut() else {
            continue;
        };
        // A request too soon after the last keyframe waits for its turn
        // rather than being lost.
        if last_keyframe.is_none_or(|at| now >= at + KEYFRAME_EVERY) && control.take_keyframe() {
            force_keyframe = true;
        }
        if std::mem::take(&mut force_keyframe) {
            active.request_keyframe();
        }
        let started = Instant::now();
        let encoded = match active.encode(picture) {
            Ok(encoded) => encoded,
            Err(error) => {
                counts.failed += 1;
                log::debug!("huddle camera: {error}");
                // The reference chain may be broken: start over.
                encoder = None;
                continue;
            }
        };
        busy += started.elapsed();
        counts.encoded += 1;
        counts.bytes += encoded.data.len() as u64;
        if encoded.keyframe {
            counts.keyframes += 1;
            last_keyframe = Some(now);
        }
        let epoch = *epoch.get_or_insert(frame.at);
        // Each picture a tick later than the last at least, should the
        // camera stamp two alike.
        let time = rtp_time(epoch, frame.at).max(last_time + 1);
        last_time = time;
        match frames.try_send(VideoFrame {
            data: encoded.data,
            keyframe: encoded.keyframe,
            time,
            at: frame.at,
        }) {
            Ok(()) => {}
            Err(mpsc::error::TrySendError::Full(_)) => {
                counts.dropped += 1;
                force_keyframe = true;
            }
            Err(mpsc::error::TrySendError::Closed(_)) => break,
        }
        if Instant::now() >= next_report {
            next_report += REPORT_EVERY;
            let ms = busy.as_secs_f64() * 1000.0 / counts.encoded.max(1) as f64;
            log::info!(
                "huddle camera: {counts:?}; {ms:.1} ms a picture; {} pictures replaced before \
                 encoding",
                latest.replaced()
            );
        }
    }
    if let Some(preview) = preview {
        preview.clear();
    }
    log::info!("huddle camera: encoder stopped; {counts:?}");
}

/// Feeds `preview` from `latest` without encoding, every picture, until
/// dropped: the demo's camera, which goes nowhere.
pub fn preview_feed(latest: Latest, preview: Preview) -> Result<Running, String> {
    let stop = Arc::new(AtomicBool::new(false));
    let thread_stop = stop.clone();
    let thread = std::thread::Builder::new()
        .name("noslacking-camera-preview".into())
        .spawn(move || {
            while !thread_stop.load(Ordering::Relaxed) {
                if let Some(frame) = latest.take(STALLED)
                    && let Some(image) = preview_image(&frame.picture)
                {
                    preview.put(image);
                }
            }
            preview.clear();
        })
        .map_err(|e| format!("no preview thread: {e}"))?;
    Ok(Running::new(stop, thread))
}

/// What a session is handed to send our camera with.
#[derive(Debug)]
pub struct CameraUplink {
    /// The encoded frames, in order.
    pub frames: mpsc::Receiver<VideoFrame>,
    /// Whether the camera is on (open and capturing); the session starts
    /// as it says.
    pub on: tokio::sync::watch::Receiver<bool>,
    /// Keyframe requests and the bandwidth estimate, for the encoder.
    pub control: SendControl,
    /// Told when Chime takes no video from us (view only, its 206).
    pub refused: mpsc::Sender<()>,
}

/// The test picture as a camera that is always on: for the probe's
/// `--send-test-video`, which never opens a real camera.
pub fn test_pattern(latest: Latest) -> Result<super::camera::Capturing, String> {
    use super::camera::{Camera as _, TestPattern};
    TestPattern::new(latest).open().map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::super::camera::{Camera as _, CameraControl, Frame, TestPattern, pattern};
    use super::*;

    #[test]
    fn bitrates_move_in_steps() {
        assert_eq!(step(0), 150_000);
        assert_eq!(step(300_000), 250_000);
        assert_eq!(step(600_000), 600_000);
        assert_eq!(step(999_999), 850_000);
        assert_eq!(step(u32::MAX), 1_200_000);
        let control = SendControl::default();
        assert_eq!(control.bitrate(), video_encoder::START_BITRATE);
        control.set_bitrate(10);
        assert_eq!(control.bitrate(), video_encoder::MIN_BITRATE);
        control.set_bitrate(50_000_000);
        assert_eq!(control.bitrate(), video_encoder::MAX_BITRATE);
    }

    #[test]
    fn rtp_time_is_ninety_kilohertz() {
        let epoch = Instant::now();
        assert_eq!(rtp_time(epoch, epoch), 0);
        assert_eq!(rtp_time(epoch, epoch + Duration::from_secs(1)), 90_000);
        assert_eq!(
            rtp_time(epoch, epoch + Duration::from_micros(66_667)),
            6_000
        );
        // A picture stamped before the first is not negative.
        assert_eq!(rtp_time(epoch + Duration::from_secs(1), epoch), 0);
    }

    #[test]
    fn the_preview_is_small_and_mirrored() {
        let mut picture = I420::black(640, 480);
        // The left edge white.
        for row in 0..480 {
            for x in 0..8 {
                picture.y[row * 640 + x] = 235;
            }
        }
        let image = preview_image(&picture).expect("an image");
        assert_eq!(image.size, [320, 240]);
        // Mirrored: white on the right.
        assert!(image.pixels[319].r() > 200, "{:?}", image.pixels[319]);
        assert!(image.pixels[0].r() < 40, "{:?}", image.pixels[0]);
        assert!(preview_image(&I420::black(0, 0)).is_none());
    }

    /// The test pattern through the encoder thread, as the session gets
    /// it: keyframe first, RTP time going up at 90 kHz, a keyframe when
    /// asked, previews for the bar, and nothing once it stops.
    #[test]
    fn the_encoder_thread_sends_what_the_camera_gives() {
        let latest = Latest::default();
        let (frames, mut out) = mpsc::channel(QUEUE);
        let control = SendControl::default();
        let woken = Arc::new(AtomicU64::new(0));
        let woke = woken.clone();
        let preview = Preview::new(move || {
            woke.fetch_add(1, Ordering::Relaxed);
        });
        let encoding = Encoding::spawn(
            latest.clone(),
            frames,
            control.clone(),
            Some(preview.clone()),
        )
        .expect("a thread");
        let mut camera = CameraControl::new(TestPattern::new(latest.clone()));
        camera.set_on(true).expect("on");
        let mut got = Vec::new();
        let deadline = Instant::now() + Duration::from_secs(10);
        while got.len() < 12 && Instant::now() < deadline {
            if got.len() == 6 {
                control.want_keyframe();
            }
            match out.try_recv() {
                Ok(frame) => got.push(frame),
                Err(_) => std::thread::sleep(Duration::from_millis(10)),
            }
        }
        camera.set_on(false).expect("off");
        assert!(got.len() >= 12, "only {} frames", got.len());
        assert!(got[0].keyframe, "the first is a keyframe");
        assert!(got.windows(2).all(|w| w[1].time > w[0].time));
        assert!(
            got[6..].iter().any(|f| f.keyframe),
            "the keyframe asked for"
        );
        let mut decoder = rusty_h264_decoder::Decoder::new();
        for frame in &got {
            let picture = decoder
                .decode(&frame.data)
                .expect("decodes")
                .expect("a picture");
            assert_eq!((picture.width, picture.height), (640, 480));
        }
        assert!(preview.pictures() >= 3, "{} previews", preview.pictures());
        assert!(woken.load(Ordering::Relaxed) >= 1);
        drop(encoding);
        // Off and stopped: nothing more comes.
        while out.try_recv().is_ok() {}
        std::thread::sleep(Duration::from_millis(100));
        assert!(out.try_recv().is_err());
    }

    /// The next frame from the encoder thread, waiting up to `wait`.
    fn next(out: &mut mpsc::Receiver<VideoFrame>, wait: Duration) -> Option<VideoFrame> {
        let until = Instant::now() + wait;
        loop {
            if let Ok(frame) = out.try_recv() {
                return Some(frame);
            }
            if Instant::now() > until {
                return None;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    fn a_picture_of_a_new_size_starts_a_new_encoder() {
        let latest = Latest::default();
        let (frames, mut out) = mpsc::channel(QUEUE);
        let encoding = Encoding::spawn(latest.clone(), frames, SendControl::default(), None)
            .expect("a thread");
        let put = |n: u64, (w, h): (usize, usize)| {
            latest.put(Frame {
                picture: pattern(w, h, n, Duration::ZERO),
                at: Instant::now(),
            });
        };
        let wait = Duration::from_secs(5);
        put(0, (320, 240));
        assert!(next(&mut out, wait).is_some_and(|f| f.keyframe));
        put(1, (320, 240));
        assert!(next(&mut out, wait).is_some_and(|f| !f.keyframe));
        // Another size: a new encoder, starting with a keyframe.
        put(2, (640, 360));
        let frame = next(&mut out, wait).expect("a frame");
        assert!(frame.keyframe);
        let picture = rusty_h264_decoder::Decoder::new()
            .decode(&frame.data)
            .expect("decodes")
            .expect("a picture");
        assert_eq!((picture.width, picture.height), (640, 360));
        // A size that cannot be sent is skipped, not a panic.
        put(3, (2, 2));
        assert!(next(&mut out, Duration::from_millis(300)).is_none());
        put(4, (640, 360));
        assert!(next(&mut out, wait).is_some());
        drop(encoding);
    }

    #[test]
    fn the_test_pattern_camera_starts() {
        let latest = Latest::default();
        let running = test_pattern(latest.clone()).expect("started");
        assert!(latest.take(Duration::from_secs(2)).is_some());
        drop(running);
        let mut pattern = TestPattern::new(latest);
        drop(pattern.open().expect("opens again"));
    }
}
