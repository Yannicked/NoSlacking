//! What we send, on its way out (the `huddle-camera` feature): the
//! thread between the video helper, which captures and encodes our
//! camera (or our shared screen, `super::share_send`), and the session;
//! the controls the session turns (keyframes, bitrate, and the frame
//! rate and picture size a Teams meeting allows); and the camera's
//! self-preview the call bar shows.
//!
//! The helper ([`RemoteCapture`]) keeps the newest captured picture and
//! encodes it on request, on the GPU when the setting is on and it can,
//! else in software. [`Encoding`]'s thread decides when, by its [`Pace`]:
//! a picture a frame's time after the last one's capture (the helper
//! waits for the next and encodes it as it arrives), a keyframe every
//! [`IDR_EVERY`] and whenever a receiver asks (PLI or FIR), and the
//! bitrate as the bandwidth estimate allows, in a few steps. It hands
//! each access unit to the session as a [`VideoFrame`] with its 90 kHz
//! RTP time, from when the helper captured it. Nothing queues: an encoded
//! frame the session cannot take (its queue full) is dropped and the next
//! made a keyframe, since what follows would refer to it.
//!
//! A camera's frames each come with a small copy of the picture, made in
//! the helper before encoding (so the self-view keeps the camera's pace,
//! whatever encoding costs): mirrored, as a mirror shows you, and turned
//! into egui's pixels for the [`Preview`]. That is all the app does with
//! a picture of its own: it never captures, converts or encodes one.

use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use egui::{Color32, ColorImage};
use noslacking_video_ipc as ipc;
use tokio::sync::{mpsc, watch};

use super::chime::VideoSend;
use super::helper::{CaptureTrouble, RemoteCapture};
use super::microphone::Running;
use crate::failure::{Failure, HuddleTrouble};

/// The largest camera picture sent: what Slack's own clients send for a
/// few cameras (480×480 to 640×480 was seen), within level 3.1. The
/// helper shrinks larger ones to fit, keeping their shape.
pub const MAX_WIDTH: u32 = 640;
/// The largest camera picture's height.
pub const MAX_HEIGHT: u32 = 480;
/// Camera pictures a second, asked of the camera and sent.
pub const FPS: u32 = 30;
/// The least bitrate set, in bit/s: below it the picture is mush anyway.
pub const MIN_BITRATE: u32 = 150_000;
/// The most a camera sends, in bit/s: Slack's own cameras send up to
/// about 500 kbit/s at 480×480; 640×480 at 30 frames a second looks good
/// from 900 to 1,800.
pub const MAX_BITRATE: u32 = 1_800_000;
/// What sending starts at, before bandwidth estimation says more.
pub const START_BITRATE: u32 = 600_000;
/// How SUBSCRIBE describes our camera: what it sends at most.
pub const DESCRIPTOR: VideoSend = VideoSend {
    width: MAX_WIDTH,
    height: MAX_HEIGHT,
    fps: FPS,
    max_kbps: MAX_BITRATE / 1000,
};
/// Encoded frames waiting for the session at most: a few, since a frame
/// late by more than that is better dropped.
pub const QUEUE: usize = 8;
/// A keyframe at least this often, even from a still screen: what a
/// receiver who joined late or lost one waits at most.
pub const IDR_EVERY: Duration = Duration::from_secs(4);
/// A new bitrate asked of the helper at most this often: the GPU's
/// encoder takes it in place; software makes a new encoder (a keyframe)
/// at most every 8 s on its side.
pub(crate) const RETUNE_EVERY: Duration = Duration::from_secs(1);
/// A keyframe asked for by a receiver at most this often: Chime may pass
/// on several receivers' PLIs at once.
const KEYFRAME_GAP: Duration = Duration::from_millis(500);
/// The self-preview's width at most, in pixels.
pub const PREVIEW_WIDTH: u32 = 320;
/// How long the helper may wait for a new picture each time it is asked.
const WAIT: Duration = Duration::from_millis(50);
/// How often the numbers reach the log.
const REPORT_EVERY: Duration = Duration::from_secs(10);

/// The bitrates a sender is kept between.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Limits {
    /// The least, in bit/s.
    pub min_bitrate: u32,
    /// The most, in bit/s.
    pub max_bitrate: u32,
}

impl Limits {
    /// A camera: up to 1.8 Mbit/s.
    pub const CAMERA: Self = Self {
        min_bitrate: MIN_BITRATE,
        max_bitrate: MAX_BITRATE,
    };
    /// A screen share: up to 2.5 Mbit/s, the JS SDK's ceiling for content
    /// (`setVideoMaxBandwidthKbps(2500)`).
    pub const SHARE: Self = Self {
        min_bitrate: MIN_BITRATE,
        max_bitrate: 2_500_000,
    };

    /// `bitrate` kept within these limits.
    pub fn bitrate(&self, bitrate: u32) -> u32 {
        bitrate.clamp(self.min_bitrate, self.max_bitrate)
    }
}

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

/// What the session tells the sender: a receiver wants a keyframe, the
/// bandwidth estimate changed, a meeting allows fewer pictures a second
/// or smaller ones. It also keeps the stream's RTP clock, so a camera
/// turned off and on again goes on from where it was.
#[derive(Clone, Debug)]
pub struct SendControl {
    shared: Arc<ControlShared>,
    limits: Limits,
}

#[derive(Debug)]
struct ControlShared {
    keyframe: AtomicBool,
    /// The bitrate aimed at, as the bandwidth estimate says.
    bitrate: AtomicU32,
    /// The most bitrate whatever the estimate (a Teams meeting's
    /// `max-br`); 0 for no ceiling.
    max_bitrate: AtomicU32,
    /// The most pictures a second; 0 for no ceiling (the pace's own).
    max_fps: AtomicU32,
    /// The box pictures fit within, width in the high half; 0 for none.
    max_size: AtomicU64,
    /// Where the RTP clock starts.
    epoch: Instant,
    /// The last RTP time handed out.
    last_time: AtomicU64,
}

/// A camera's.
impl Default for SendControl {
    fn default() -> Self {
        Self::new(Limits::CAMERA)
    }
}

impl SendControl {
    /// The controls of a sender kept within `limits`.
    pub fn new(limits: Limits) -> Self {
        Self {
            shared: Arc::new(ControlShared {
                keyframe: AtomicBool::new(false),
                bitrate: AtomicU32::new(limits.bitrate(START_BITRATE)),
                max_bitrate: AtomicU32::new(0),
                max_fps: AtomicU32::new(0),
                max_size: AtomicU64::new(0),
                epoch: Instant::now(),
                last_time: AtomicU64::new(0),
            }),
            limits,
        }
    }

    /// What the sender is kept within.
    pub fn limits(&self) -> Limits {
        self.limits
    }

    /// Whether a keyframe has been asked for and not yet made.
    pub fn keyframe_wanted(&self) -> bool {
        self.shared.keyframe.load(Ordering::Relaxed)
    }

    /// The next picture should be a keyframe.
    pub fn want_keyframe(&self) {
        self.shared.keyframe.store(true, Ordering::Relaxed);
    }

    /// Whether a keyframe was asked for since the last call.
    pub(crate) fn take_keyframe(&self) -> bool {
        self.shared.keyframe.swap(false, Ordering::Relaxed)
    }

    /// The bitrate to aim at, in bit/s, kept within what is sent.
    pub fn set_bitrate(&self, bitrate: u32) {
        self.shared
            .bitrate
            .store(self.limits.bitrate(bitrate), Ordering::Relaxed);
    }

    /// The bitrate aimed at now: the estimate's, under any ceiling.
    pub fn bitrate(&self) -> u32 {
        let bitrate = self.shared.bitrate.load(Ordering::Relaxed);
        match self.shared.max_bitrate.load(Ordering::Relaxed) {
            0 => bitrate,
            max => self.limits.bitrate(bitrate.min(max)),
        }
    }

    /// At most `bitrate` bit/s whatever the estimate says (a Teams
    /// meeting's `max-br`); none lifts the ceiling.
    pub fn set_max_bitrate(&self, bitrate: Option<u32>) {
        let bitrate = bitrate.filter(|&b| b > 0).unwrap_or(0);
        self.shared.max_bitrate.store(bitrate, Ordering::Relaxed);
    }

    /// At most `fps` pictures a second, below the sender's own pace (a
    /// Teams meeting's `max-fps`); none lifts the ceiling.
    pub fn set_max_fps(&self, fps: Option<u32>) {
        let fps = fps.filter(|&fps| fps > 0).unwrap_or(0);
        self.shared.max_fps.store(fps, Ordering::Relaxed);
    }

    /// The ceiling on pictures a second, if any.
    pub fn max_fps(&self) -> Option<u32> {
        Some(self.shared.max_fps.load(Ordering::Relaxed)).filter(|&fps| fps > 0)
    }

    /// Pictures fit within `max` as well as their own limits (a Teams
    /// meeting's `max-fs`); none lifts the box.
    pub fn set_max_size(&self, max: Option<(u32, u32)>) {
        let packed = max
            .filter(|&(w, h)| w > 0 && h > 0)
            .map_or(0, |(w, h)| u64::from(w) << 32 | u64::from(h));
        self.shared.max_size.store(packed, Ordering::Relaxed);
    }

    /// The box pictures fit within, if any.
    pub fn max_size(&self) -> Option<(u32, u32)> {
        let packed = self.shared.max_size.load(Ordering::Relaxed);
        let (w, h) = ((packed >> 32) as u32, packed as u32);
        (w > 0 && h > 0).then_some((w, h))
    }

    /// The RTP time of a picture taken at `at`: 90 kHz from when these
    /// controls were made, and each a tick later than the last at least,
    /// should two be stamped alike.
    pub fn rtp_time(&self, at: Instant) -> u64 {
        let time = rtp_time(self.shared.epoch, at);
        let previous = self
            .shared
            .last_time
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |last| {
                Some(time.max(last + 1))
            })
            .unwrap_or(0);
        time.max(previous + 1)
    }
}

/// The bitrate a camera's encoder is asked for at `wanted`: a few steps,
/// so small swings of the estimate do not each retune it (in software,
/// each new rate costs a keyframe).
pub fn step(wanted: u32) -> u32 {
    step_within(wanted, MAX_BITRATE)
}

/// [`step`], up to `max` instead of the camera's most.
pub fn step_within(wanted: u32, max: u32) -> u32 {
    const STEPS: [u32; 8] = [
        150_000, 250_000, 400_000, 600_000, 900_000, 1_300_000, 1_800_000, 2_500_000,
    ];
    STEPS
        .iter()
        .rev()
        .copied()
        .find(|&s| s <= wanted.min(max))
        .unwrap_or(STEPS[0])
}

/// The RTP time of a picture taken at `at`, 90 kHz from `epoch`.
pub fn rtp_time(epoch: Instant, at: Instant) -> u64 {
    let micros = at.saturating_duration_since(epoch).as_micros();
    u64::try_from(micros * 9 / 100).unwrap_or(u64::MAX)
}

/// The newest picture of our own camera for the call bar: put by the
/// sending thread, taken by the interface.
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

/// The helper's self-view picture as egui's pixels for the preview,
/// mirrored left to right. The helper made it small already; none for
/// one wider than the preview is ever shown or broken.
pub fn preview_image(picture: &ipc::Planes) -> Option<ColorImage> {
    if picture.check().is_err() || picture.width > ipc::MAX_PREVIEW_SIDE {
        return None;
    }
    let (width, height) = (
        usize::try_from(picture.width).ok()?,
        usize::try_from(picture.height).ok()?,
    );
    let image = yuv::YuvPlanarImage {
        y_plane: &picture.y,
        y_stride: picture.width,
        u_plane: &picture.u,
        u_stride: picture.width.div_ceil(2),
        v_plane: &picture.v,
        v_stride: picture.width.div_ceil(2),
        width: picture.width,
        height: picture.height,
    };
    let mut pixels = vec![Color32::BLACK; width * height];
    yuv::yuv420_to_rgba(
        &image,
        bytemuck::cast_slice_mut(&mut pixels),
        picture.width * 4,
        yuv::YuvRange::Limited,
        yuv::YuvStandardMatrix::Bt601,
    )
    .ok()?;
    for row in pixels.chunks_exact_mut(width) {
        row.reverse();
    }
    Some(ColorImage::new([width, height], pixels))
}

/// What the sender did, for the log.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct EncodeCounts {
    /// Pictures encoded.
    pub encoded: u64,
    /// Of those, keyframes.
    pub keyframes: u64,
    /// Encoded frames dropped because the session's queue was full.
    pub dropped: u64,
    /// Bytes encoded.
    pub bytes: u64,
    /// Encoded on the GPU.
    pub gpu: u64,
    /// Sent again, the picture unchanged (a still screen).
    pub repeated: u64,
    /// Times the helper failed and the capture was started again in a
    /// new one.
    pub restarts: u64,
}

/// How often a sender asks for pictures, and for what.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Pace {
    /// Pictures a second at most.
    pub fps: u32,
    /// A still source's last picture is sent again at least this often
    /// (a screen); none for a camera, which never stands still and whose
    /// pictures are better waited for than repeated.
    pub keepalive: Option<Duration>,
    /// A picture this long after the last means the source stalled: the
    /// next is a keyframe (a camera; a screen has its keepalive).
    pub stall: Option<Duration>,
}

impl Pace {
    /// A camera's: 30 a second, nothing sent again, a keyframe after half
    /// a second without a picture.
    pub const CAMERA: Self = Self {
        fps: FPS,
        keepalive: None,
        stall: Some(Duration::from_millis(500)),
    };
}

/// When to ask the helper for a picture, and what for: a frame's time
/// after the last one sent; a keyframe when one is due or asked for, the
/// last picture again when a still source has been still for its
/// keepalive.
#[derive(Clone, Copy, Debug)]
pub struct Gate {
    pace: Pace,
    /// A ceiling under the pace's own frame rate (a meeting's).
    max_fps: Option<u32>,
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
    /// A gate keeping `pace`.
    pub fn new(pace: Pace) -> Self {
        Self {
            pace,
            max_fps: None,
            last_sent: None,
        }
    }

    /// At most `fps` pictures a second from now on, if that is fewer
    /// than the pace's; none goes back to the pace. A still screen's
    /// keepalive is not changed.
    pub fn set_max_fps(&mut self, fps: Option<u32>) {
        self.max_fps = fps;
    }

    /// The pictures a second asked for now.
    pub fn fps(&self) -> u32 {
        self.max_fps
            .map_or(self.pace.fps, |max| self.pace.fps.min(max))
            .max(1)
    }

    /// How long to wait before asking at `now`; zero when it is time.
    pub fn wait(&self, now: Instant) -> Duration {
        // A little under a frame's time, so a capture a millisecond early
        // is not missed.
        let frame = (Duration::from_secs(1) / self.fps()).saturating_sub(Duration::from_millis(5));
        self.last_sent.map_or(Duration::ZERO, |at| {
            (at + frame).saturating_duration_since(now)
        })
    }

    /// What to ask for at `now`: `asked` a receiver wants a keyframe,
    /// `last_keyframe` when the last was made.
    pub fn ask(&self, now: Instant, asked: bool, last_keyframe: Option<Instant>) -> Asking {
        let idr_due = last_keyframe.is_none_or(|at| now >= at + IDR_EVERY);
        let stalled = self
            .pace
            .stall
            .is_some_and(|stall| self.last_sent.is_some_and(|at| now >= at + stall));
        let force_keyframe = idr_due || asked || stalled;
        let repeat = self.pace.keepalive.is_some_and(|keepalive| {
            force_keyframe || self.last_sent.is_none_or(|at| now >= at + keepalive)
        });
        Asking {
            force_keyframe,
            repeat,
        }
    }

    /// A picture went out at `now`.
    pub fn sent(&mut self, now: Instant) {
        self.last_sent = Some(now);
    }
}

/// How a capture ended by itself.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Ending {
    /// The capture ended: the system's own "stop sharing", the window
    /// closed, the camera went away.
    Ended,
    /// The capture failed, or the helper did.
    Failed(Failure),
}

/// What a camera that would not start, or stopped, tells the interface:
/// a camera the helper could not open, or no helper (`given_up`: none
/// installed, or it keeps failing) or one that stopped.
pub fn camera_failure(trouble: &CaptureTrouble, given_up: bool) -> Failure {
    use ipc::CaptureProblem;
    Failure::Huddle(match trouble {
        CaptureTrouble::Problem(CaptureProblem::Unavailable | CaptureProblem::Gone, _) => {
            HuddleTrouble::NoCamera
        }
        CaptureTrouble::Problem(CaptureProblem::Denied, _) => HuddleTrouble::CameraDenied,
        CaptureTrouble::Problem(CaptureProblem::Ended, _) => HuddleTrouble::CameraGone,
        CaptureTrouble::Problem(
            CaptureProblem::Busy | CaptureProblem::Failed | CaptureProblem::Cancelled,
            _,
        ) => HuddleTrouble::Camera,
        CaptureTrouble::Lost(_) if given_up => HuddleTrouble::CameraNeedsHelper,
        CaptureTrouble::Lost(_) => HuddleTrouble::VideoHelperLost,
    })
}

/// The camera the helper is to start for the one `chosen` in the
/// settings, among the `cameras` it lists now: that camera by its
/// present id when it is there (found as [`crate::devices::find`] finds
/// a camera, by its name, since `v4l2:/dev/video2` and `native:1` are
/// only where it is plugged in now), the system's first while it is not.
pub fn camera_choice(
    chosen: &crate::devices::Choice,
    cameras: &[ipc::Source],
) -> ipc::CameraChoice {
    let present = devices_of(cameras);
    match crate::devices::find(crate::devices::Kind::Camera, chosen, &present) {
        Some(camera) => ipc::CameraChoice::Device(camera.id.clone()),
        None => {
            log::info!(
                "huddle camera: the chosen camera {:?} is not connected; the first",
                chosen.name
            );
            ipc::CameraChoice::First
        }
    }
}

/// The helper's cameras as the interface's devices.
pub fn devices_of(cameras: &[ipc::Source]) -> Vec<crate::devices::Device> {
    cameras
        .iter()
        .filter(|c| c.kind == ipc::SourceKind::Camera)
        .map(|c| crate::devices::Device {
            id: c.id.clone(),
            name: c.name.clone(),
        })
        .collect()
}

/// Starts the capture again in a fresh helper after the last one failed:
/// the camera's (it has no dialog, so it can), none for a share.
pub type Restart = Box<dyn FnMut() -> Result<RemoteCapture, CaptureTrouble> + Send>;

/// How a sending thread is set up.
pub struct Options {
    /// What it sends, for the log: "camera" or "share".
    pub what: &'static str,
    /// When it asks for pictures.
    pub pace: Pace,
    /// Where a camera's self-view goes.
    pub preview: Option<Preview>,
    /// How to start the capture again when the helper fails; without it,
    /// a failing helper ends the capture.
    pub restart: Option<Restart>,
    /// What a helper's trouble ending the capture comes to.
    pub ending: Box<dyn Fn(&CaptureTrouble) -> Ending + Send>,
}

impl std::fmt::Debug for Options {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Options")
            .field("what", &self.what)
            .field("pace", &self.pace)
            .field("preview", &self.preview.is_some())
            .field("restart", &self.restart.is_some())
            .finish_non_exhaustive()
    }
}

/// A capture's sending thread: it stops, and is joined, when dropped,
/// and the capture in the helper with it (a camera closes).
#[derive(Debug)]
pub struct Encoding {
    _running: Running,
}

impl Encoding {
    /// Starts sending what `capture` encodes into `frames` (none: only
    /// its self-view is wanted, the demo's camera), as `control` says;
    /// tells `ended` if the capture ends by itself.
    pub fn spawn(
        capture: RemoteCapture,
        frames: Option<mpsc::Sender<VideoFrame>>,
        control: SendControl,
        ended: watch::Sender<Option<Ending>>,
        options: Options,
    ) -> Result<Self, String> {
        let stop = Arc::new(AtomicBool::new(false));
        let thread_stop = stop.clone();
        let thread = std::thread::Builder::new()
            .name(format!("noslacking-{}-sender", options.what))
            .spawn(move || {
                let ending = send(capture, frames.as_ref(), &control, &thread_stop, options);
                if let Some(ending) = ending {
                    ended.send_replace(Some(ending));
                }
            })
            .map_err(|e| format!("no sending thread: {e}"))?;
        Ok(Self {
            _running: Running::new(stop, thread),
        })
    }
}

/// The sending thread: asks `capture` for pictures as the [`Gate`] says
/// until `stop`, or until the capture ends by itself (then why).
fn send(
    mut capture: RemoteCapture,
    frames: Option<&mpsc::Sender<VideoFrame>>,
    control: &SendControl,
    stop: &AtomicBool,
    mut options: Options,
) -> Option<Ending> {
    let what = options.what;
    let mut gate = Gate::new(options.pace);
    let mut counts = EncodeCounts::default();
    let mut last_keyframe: Option<Instant> = None;
    // The next picture must be a keyframe: one the session could not take
    // was dropped, and what follows would refer to it; or the capture
    // started again.
    let mut force_next = false;
    let mut bitrate = control.bitrate();
    let mut retuned: Option<Instant> = None;
    // The box last asked of the helper: none, as it starts.
    let mut boxed: Option<(u32, u32)> = None;
    let mut size = (0, 0);
    let mut next_report = Instant::now() + REPORT_EVERY;
    let ending = loop {
        if stop.load(Ordering::Relaxed) {
            break None;
        }
        let now = Instant::now();
        let fps = gate.fps();
        gate.set_max_fps(control.max_fps());
        if gate.fps() != fps {
            log::info!("huddle {what}: {} pictures a second", gate.fps());
        }
        let wait = gate.wait(now);
        if !wait.is_zero() {
            std::thread::park_timeout(wait);
            continue;
        }
        let answer = (|| {
            // The bandwidth estimate, in steps, taken in place by the
            // GPU's encoder.
            let wanted = step_within(control.bitrate(), control.limits().max_bitrate);
            if wanted != bitrate && retuned.is_none_or(|at| now >= at + RETUNE_EVERY) {
                capture.set_bitrate(wanted)?;
                bitrate = wanted;
                retuned = Some(now);
            }
            // A meeting's box, passed on as it changes: the helper starts
            // the new size with a keyframe.
            let wanted_box = control.max_size();
            if wanted_box != boxed {
                capture.set_max_size(wanted_box)?;
                boxed = wanted_box;
                log::info!(
                    "huddle {what}: pictures at most {}",
                    wanted_box.map_or_else(
                        || "as large as they come".to_owned(),
                        |(w, h)| format!("{w}x{h}")
                    )
                );
            }
            // A receiver's request a moment after a keyframe waits its
            // turn.
            let asked = force_next
                || (control.keyframe_wanted()
                    && last_keyframe.is_none_or(|at| now >= at + KEYFRAME_GAP));
            let asking = gate.ask(now, asked, last_keyframe);
            capture
                .next(asking.force_keyframe, asking.repeat, WAIT)
                .map(|frame| (asking, frame))
        })();
        let (asking, frame) = match answer {
            Ok((_, None)) => continue,
            Ok((asking, Some(frame))) => (asking, frame),
            Err(CaptureTrouble::Lost(why)) if options.restart.is_some() => {
                log::warn!("huddle {what}: the video helper failed ({why}): starting again");
                match options.restart.as_mut().map(|restart| restart()) {
                    Some(Ok(fresh)) => {
                        counts.restarts += 1;
                        capture = fresh;
                        // A new encoder: it starts with a keyframe, at the
                        // rate it was started with.
                        force_next = true;
                        retuned = None;
                        bitrate = 0;
                        boxed = None;
                        continue;
                    }
                    Some(Err(trouble)) => {
                        log::warn!("huddle {what}: it did not start again: {trouble:?}");
                        break Some((options.ending)(&trouble));
                    }
                    None => break Some((options.ending)(&CaptureTrouble::Lost(why))),
                }
            }
            Err(trouble) => {
                log::info!("huddle {what}: the capture stopped: {trouble:?}");
                break Some((options.ending)(&trouble));
            }
        };
        let now = Instant::now();
        if asking.force_keyframe {
            control.take_keyframe();
            force_next = false;
        }
        // Every new picture's self-view, before anything else: the bar
        // keeps the camera's pace.
        if let (Some(preview), Some(picture)) = (&options.preview, &frame.preview)
            && let Some(image) = preview_image(picture)
        {
            preview.put(image);
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
        counts.gpu += u64::from(frame.hardware);
        counts.repeated += u64::from(frame.age_us == 0);
        if (frame.width, frame.height) != size {
            size = (frame.width, frame.height);
            log::info!(
                "huddle {what}: sending {}x{} at {} kbit/s, {} a second, encoded {}",
                size.0,
                size.1,
                bitrate / 1000,
                gate.fps(),
                if frame.hardware {
                    "on the GPU"
                } else {
                    "in software"
                }
            );
        }
        counts.encoded += 1;
        counts.bytes += frame.data.len() as u64;
        if let Some(frames) = frames {
            let sent = VideoFrame {
                data: frame.data,
                keyframe: frame.keyframe,
                time: control.rtp_time(at),
                at,
            };
            match frames.try_send(sent) {
                Ok(()) => {}
                Err(mpsc::error::TrySendError::Full(_)) => {
                    counts.dropped += 1;
                    force_next = true;
                }
                Err(mpsc::error::TrySendError::Closed(_)) => break None,
            }
        }
        if now >= next_report {
            next_report += REPORT_EVERY;
            log::info!("huddle {what}: {counts:?}");
        }
    };
    if let Some(preview) = &options.preview {
        preview.clear();
    }
    log::info!("huddle {what}: stopped sending; {counts:?}");
    ending
}

/// What a session is handed to send our camera (or a share) with.
#[derive(Debug)]
pub struct CameraUplink {
    /// The encoded frames, in order.
    pub frames: mpsc::Receiver<VideoFrame>,
    /// Whether the camera is on (open and capturing); the session starts
    /// as it says.
    pub on: tokio::sync::watch::Receiver<bool>,
    /// Keyframe requests and the bandwidth estimate, for the sender.
    pub control: SendControl,
    /// Told when Chime takes no video from us (view only, its 206).
    pub refused: mpsc::Sender<()>,
    /// How SUBSCRIBE describes what is sent: [`DESCRIPTOR`] for a camera,
    /// the share's own for a screen share.
    pub descriptor: VideoSend,
}

#[cfg(test)]
mod tests {
    use super::super::helper::pretend::{Act, Pretend, welcome};
    use super::super::helper::{self, Helper, Lane};
    use super::*;
    use noslacking_video_ipc::{CameraChoice, CaptureProblem, Reply, Request};

    const FRAME: Duration = Duration::from_millis(33);

    #[test]
    fn the_chosen_camera_is_started_by_where_it_is_now() {
        let camera = |id: &str, name: &str| ipc::Source {
            id: id.into(),
            name: name.into(),
            kind: ipc::SourceKind::Camera,
        };
        let brio = crate::devices::Choice {
            id: "v4l2:/dev/video2".into(),
            name: "Logitech BRIO".into(),
        };
        // Where it was chosen.
        assert_eq!(
            camera_choice(
                &brio,
                &[
                    camera("v4l2:/dev/video0", "Integrated Camera"),
                    camera("v4l2:/dev/video2", "Logitech BRIO"),
                ]
            ),
            CameraChoice::Device("v4l2:/dev/video2".into())
        );
        // Plugged in before the built-in one after a reboot.
        assert_eq!(
            camera_choice(
                &brio,
                &[
                    camera("v4l2:/dev/video0", "Logitech BRIO"),
                    camera("v4l2:/dev/video2", "Integrated Camera"),
                ]
            ),
            CameraChoice::Device("v4l2:/dev/video0".into())
        );
        // Unplugged: the first, not whatever took its node.
        assert_eq!(
            camera_choice(&brio, &[camera("v4l2:/dev/video2", "Integrated Camera")]),
            CameraChoice::First
        );
        assert_eq!(camera_choice(&brio, &[]), CameraChoice::First);
    }

    #[test]
    fn bitrates_move_in_steps() {
        assert_eq!(step(0), 150_000);
        assert_eq!(step(300_000), 250_000);
        assert_eq!(step(600_000), 600_000);
        assert_eq!(step(999_999), 900_000);
        assert_eq!(step(u32::MAX), 1_800_000);
        let control = SendControl::default();
        assert_eq!(control.bitrate(), START_BITRATE);
        control.set_bitrate(10);
        assert_eq!(control.bitrate(), MIN_BITRATE);
        control.set_bitrate(50_000_000);
        assert_eq!(control.bitrate(), MAX_BITRATE);
        assert_eq!(Limits::SHARE.bitrate(50_000_000), 2_500_000);
    }

    #[test]
    fn a_bitrate_ceiling_holds_whatever_the_estimate() {
        let control = SendControl::new(Limits::SHARE);
        control.set_bitrate(2_000_000);
        control.set_max_bitrate(Some(825_000));
        assert_eq!(control.bitrate(), 825_000);
        // A lower estimate goes below it; a higher one stays under it.
        control.set_bitrate(400_000);
        assert_eq!(control.bitrate(), 400_000);
        control.set_bitrate(9_000_000);
        assert_eq!(control.bitrate(), 825_000);
        // Never below the sender's least, and lifted by none.
        control.set_max_bitrate(Some(1));
        assert_eq!(control.bitrate(), MIN_BITRATE);
        control.set_max_bitrate(None);
        assert_eq!(control.bitrate(), 2_500_000);
    }

    #[test]
    fn a_meetings_ceilings_are_kept_until_lifted() {
        let control = SendControl::default();
        assert_eq!((control.max_fps(), control.max_size()), (None, None));
        control.set_max_fps(Some(15));
        control.set_max_size(Some((1280, 720)));
        let shared = control.clone();
        assert_eq!(shared.max_fps(), Some(15));
        assert_eq!(shared.max_size(), Some((1280, 720)));
        // Zero is no ceiling, as none is.
        control.set_max_fps(Some(0));
        control.set_max_size(Some((0, 720)));
        assert_eq!((shared.max_fps(), shared.max_size()), (None, None));
    }

    #[test]
    fn a_ceiling_slows_the_gate_but_never_speeds_it() {
        let start = Instant::now();
        let mut gate = Gate::new(Pace::CAMERA);
        gate.sent(start);
        let at_30 = gate.wait(start);
        gate.set_max_fps(Some(15));
        assert_eq!(gate.fps(), 15);
        let at_15 = gate.wait(start);
        assert!(at_15 > at_30 + Duration::from_millis(30), "{at_15:?}");
        assert_eq!(gate.wait(start + Duration::from_millis(67)), Duration::ZERO);
        // A ceiling above the pace changes nothing; none is the pace.
        gate.set_max_fps(Some(60));
        assert_eq!((gate.fps(), gate.wait(start)), (30, at_30));
        gate.set_max_fps(None);
        assert_eq!(gate.fps(), 30);
    }

    #[test]
    fn rtp_time_is_ninety_kilohertz_and_only_goes_up() {
        let epoch = Instant::now();
        assert_eq!(rtp_time(epoch, epoch), 0);
        assert_eq!(rtp_time(epoch, epoch + Duration::from_secs(1)), 90_000);
        assert_eq!(
            rtp_time(epoch, epoch + Duration::from_micros(66_667)),
            6_000
        );
        // A picture stamped before the first is not negative.
        assert_eq!(rtp_time(epoch + Duration::from_secs(1), epoch), 0);
        // The controls' clock: a picture stamped like the last, or
        // earlier (a camera turned on again), is a tick later still.
        let control = SendControl::default();
        let later = Instant::now() + Duration::from_secs(2);
        let first = control.rtp_time(later);
        assert!(first >= 180_000);
        assert_eq!(control.rtp_time(later), first + 1);
        assert_eq!(control.clone().rtp_time(Instant::now()), first + 2);
    }

    #[test]
    fn the_preview_is_the_helpers_picture_mirrored() {
        let mut picture = ipc::Planes {
            width: 320,
            height: 240,
            y: vec![16; 320 * 240],
            u: vec![128; 160 * 120],
            v: vec![128; 160 * 120],
        };
        // The left edge white.
        for row in 0..240 {
            for x in 0..4 {
                picture.y[row * 320 + x] = 235;
            }
        }
        let image = preview_image(&picture).expect("an image");
        assert_eq!(image.size, [320, 240]);
        // Mirrored: white on the right.
        assert!(image.pixels[319].r() > 200, "{:?}", image.pixels[319]);
        assert!(image.pixels[0].r() < 40, "{:?}", image.pixels[0]);
        // Broken, or wider than ever shown: none.
        picture.y.pop();
        assert!(preview_image(&picture).is_none());
    }

    #[test]
    fn a_camera_waits_for_new_pictures_and_keyframes_when_due_or_stalled() {
        let start = Instant::now();
        let mut gate = Gate::new(Pace::CAMERA);
        assert_eq!(gate.wait(start), Duration::ZERO);
        let first = gate.ask(start, false, None);
        assert!(first.force_keyframe && !first.repeat, "never repeated");
        gate.sent(start);
        let keyframe = Some(start);
        assert!(gate.wait(start + Duration::from_millis(10)) > Duration::ZERO);
        assert_eq!(gate.wait(start + FRAME), Duration::ZERO);
        let next = gate.ask(start + FRAME, false, keyframe);
        assert!(!next.force_keyframe && !next.repeat);
        // A receiver's PLI, an IDR due, a stalled camera: a keyframe.
        assert!(gate.ask(start + FRAME, true, keyframe).force_keyframe);
        assert!(gate.ask(start + IDR_EVERY, false, keyframe).force_keyframe);
        let stalled = gate.ask(start + Duration::from_millis(600), false, keyframe);
        assert!(stalled.force_keyframe && !stalled.repeat);
    }

    #[test]
    fn camera_troubles_have_their_words() {
        let problem = |p| CaptureTrouble::Problem(p, "x".into());
        for (trouble, given_up, want) in [
            (
                problem(CaptureProblem::Unavailable),
                false,
                HuddleTrouble::NoCamera,
            ),
            (
                problem(CaptureProblem::Gone),
                false,
                HuddleTrouble::NoCamera,
            ),
            (
                problem(CaptureProblem::Denied),
                false,
                HuddleTrouble::CameraDenied,
            ),
            (problem(CaptureProblem::Busy), false, HuddleTrouble::Camera),
            (
                problem(CaptureProblem::Failed),
                false,
                HuddleTrouble::Camera,
            ),
            (
                problem(CaptureProblem::Ended),
                false,
                HuddleTrouble::CameraGone,
            ),
            (
                CaptureTrouble::Lost("crashed".into()),
                false,
                HuddleTrouble::VideoHelperLost,
            ),
            (
                CaptureTrouble::Lost("not there".into()),
                true,
                HuddleTrouble::CameraNeedsHelper,
            ),
        ] {
            assert_eq!(
                camera_failure(&trouble, given_up),
                Failure::Huddle(want),
                "{trouble:?}"
            );
            assert!(!Failure::Huddle(want).message().is_empty());
        }
    }

    /// The next frame from `out`, waiting up to `wait`.
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

    fn camera_options(preview: Option<Preview>, restart: Option<Restart>) -> Options {
        Options {
            what: "camera",
            pace: Pace::CAMERA,
            preview,
            restart,
            ending: Box::new(|trouble| match trouble {
                CaptureTrouble::Problem(CaptureProblem::Ended, _) => Ending::Ended,
                other => Ending::Failed(camera_failure(other, false)),
            }),
        }
    }

    /// The helper's test camera through the helper's own code on a
    /// thread, no GPU: 640×480 H.264 at up to 30 a second, a keyframe
    /// first and when asked, RTP time going up, a self-view for the bar
    /// with each picture, and nothing once it stops.
    #[test]
    fn the_test_camera_goes_out_as_640x480_h264_with_its_self_view() {
        let helper = helper::shared(Lane::Camera).expect("in tests, always");
        let camera = helper
            .start_camera(CameraChoice::Test, false, START_BITRATE, PREVIEW_WIDTH)
            .expect("started");
        let (frames, mut out) = mpsc::channel(QUEUE);
        let (ended, _) = watch::channel(None);
        let control = SendControl::default();
        let woken = Arc::new(AtomicU64::new(0));
        let woke = woken.clone();
        let preview = Preview::new(move || {
            woke.fetch_add(1, Ordering::Relaxed);
        });
        let encoding = Encoding::spawn(
            camera,
            Some(frames),
            control.clone(),
            ended,
            camera_options(Some(preview.clone()), None),
        )
        .expect("a thread");
        let mut got = Vec::new();
        let started = Instant::now();
        let wanted = 18;
        while got.len() < wanted && started.elapsed() < Duration::from_secs(20) {
            if got.len() == 6 {
                control.want_keyframe();
            }
            if let Some(frame) = next(&mut out, Duration::from_millis(200)) {
                got.push(frame);
            }
        }
        let took = started.elapsed();
        drop(encoding);
        assert!(got.len() >= wanted, "only {} frames", got.len());
        assert!(got[0].keyframe, "the first is a keyframe");
        assert!(got.windows(2).all(|w| w[1].time > w[0].time));
        assert!(
            got[6..].iter().any(|f| f.keyframe),
            "the keyframe asked for"
        );
        assert!(took >= FRAME * 12, "paced: {wanted} in {took:?}");
        let mut decoder = rusty_h264_decoder::Decoder::new();
        for frame in &got {
            let picture = decoder
                .decode(&frame.data)
                .expect("decodes")
                .expect("a picture");
            assert_eq!((picture.width, picture.height), (640, 480));
        }
        assert!(preview.pictures() >= 12, "{} previews", preview.pictures());
        assert!(woken.load(Ordering::Relaxed) >= 1);
        assert!(preview.take().is_none(), "cleared when it stopped");
        // Stopped: nothing more comes.
        while out.try_recv().is_ok() {}
        std::thread::sleep(Duration::from_millis(100));
        assert!(out.try_recv().is_err());
    }

    /// A pretend helper sending a camera: frames as asked (a keyframe
    /// when forced), each with a 4×2 self-view; it crashes when the `n`th
    /// frame asked for (counted across its restarts, from 1) makes
    /// `crash(n)` true. How many starts it saw is kept.
    fn pretend(starts: Arc<AtomicU64>, crash: fn(u64) -> bool) -> Helper {
        let asked = Arc::new(AtomicU64::new(0));
        let pretend = Pretend::new(move |request| match request {
            Request::Hello { .. } => Act::Reply(welcome()),
            Request::StartCamera { .. } => {
                starts.fetch_add(1, Ordering::Relaxed);
                Act::Reply(Reply::Started {
                    id: 1,
                    restore: String::new(),
                })
            }
            Request::NextFrame { force_keyframe, .. } => {
                if crash(asked.fetch_add(1, Ordering::Relaxed) + 1) {
                    return Act::Crash;
                }
                std::thread::sleep(Duration::from_millis(10));
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
                    width: 640,
                    height: 480,
                    age_us: 1_000,
                    data,
                    preview: Some(ipc::Planes {
                        width: 4,
                        height: 2,
                        y: vec![100; 8],
                        u: vec![128; 2],
                        v: vec![128; 2],
                    }),
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

    /// A helper that crashes mid-call: the camera starts again in a new
    /// one, with a keyframe, and goes on; the session never notices but
    /// for the keyframe.
    #[test]
    fn a_crashed_helper_starts_the_camera_again() {
        let starts = Arc::new(AtomicU64::new(0));
        let helper = pretend(Arc::clone(&starts), |n| n == 4);
        let camera = helper
            .start_camera(CameraChoice::First, true, START_BITRATE, PREVIEW_WIDTH)
            .expect("started");
        let again = helper.clone();
        let restart: Restart = Box::new(move || {
            again.start_camera(CameraChoice::First, true, START_BITRATE, PREVIEW_WIDTH)
        });
        let (frames, mut out) = mpsc::channel(64);
        let (ended, told) = watch::channel(None);
        let preview = Preview::new(|| {});
        let encoding = Encoding::spawn(
            camera,
            Some(frames),
            SendControl::default(),
            ended,
            camera_options(Some(preview.clone()), Some(restart)),
        )
        .expect("a thread");
        let mut got = Vec::new();
        let deadline = Instant::now() + Duration::from_secs(10);
        while got.len() < 6 && Instant::now() < deadline {
            if let Some(frame) = next(&mut out, Duration::from_millis(100)) {
                got.push(frame);
            }
        }
        drop(encoding);
        assert_eq!(got.len(), 6);
        let keyframes: Vec<bool> = got.iter().map(|f| f.keyframe).collect();
        assert_eq!(
            keyframes,
            [true, false, false, true, false, false],
            "the first, and the first from the new helper"
        );
        assert_eq!(starts.load(Ordering::Relaxed), 2, "started again once");
        assert!(told.borrow().is_none(), "it did not end");
        assert!(preview.pictures() >= 6);
        assert!(got.windows(2).all(|w| w[1].time > w[0].time));
    }

    /// A helper that keeps crashing is given up: the camera ends, saying
    /// the helper is missing or keeps failing.
    #[test]
    fn a_helper_that_keeps_crashing_ends_the_camera() {
        let starts = Arc::new(AtomicU64::new(0));
        let helper = pretend(Arc::clone(&starts), |_| true);
        let camera = helper
            .start_camera(CameraChoice::First, true, START_BITRATE, 0)
            .expect("started");
        let again = helper.clone();
        let restart: Restart =
            Box::new(move || again.start_camera(CameraChoice::First, true, START_BITRATE, 0));
        let given_up = helper.clone();
        let options = Options {
            ending: Box::new(move |trouble| {
                Ending::Failed(camera_failure(trouble, given_up.given_up()))
            }),
            ..camera_options(None, Some(restart))
        };
        let (frames, _out) = mpsc::channel(64);
        let (ended, mut told) = watch::channel(None);
        let encoding =
            Encoding::spawn(camera, Some(frames), SendControl::default(), ended, options)
                .expect("a thread");
        let deadline = Instant::now() + Duration::from_secs(10);
        while told.borrow_and_update().is_none() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        drop(encoding);
        assert_eq!(
            *told.borrow(),
            Some(Ending::Failed(Failure::Huddle(
                HuddleTrouble::CameraNeedsHelper
            )))
        );
        assert!(helper.given_up());
        assert_eq!(
            starts.load(Ordering::Relaxed),
            u64::from(helper::MAX_RESTARTS) + 1,
            "the first start and each restart"
        );
    }

    /// Without a session (the demo's camera), only the self-view is
    /// taken.
    #[test]
    fn a_preview_only_camera_feeds_the_bar() {
        let starts = Arc::new(AtomicU64::new(0));
        let helper = pretend(starts, |_| false);
        let camera = helper
            .start_camera(CameraChoice::Test, false, START_BITRATE, PREVIEW_WIDTH)
            .expect("started");
        let preview = Preview::new(|| {});
        let (ended, _) = watch::channel(None);
        let encoding = Encoding::spawn(
            camera,
            None,
            SendControl::default(),
            ended,
            camera_options(Some(preview.clone()), None),
        )
        .expect("a thread");
        let deadline = Instant::now() + Duration::from_secs(5);
        while preview.pictures() < 3 && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        drop(encoding);
        assert!(preview.pictures() >= 3);
    }
}
