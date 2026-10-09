//! A capture (a screen share or the camera), from captured frames to the
//! H.264 the app sends.
//!
//! The capture ([`crate::capture`]) runs on a thread of its own and hands
//! each frame to the capture's [`Pipeline`] on that thread, or has a
//! reader thread of its own put pictures in through [`Ask::Picture`] (the
//! camera). The pipeline keeps at most one picture every
//! [`Profile::min_gap`], skips pictures that did not change where that is
//! worth checking (a screen is mostly still; a camera never is), and
//! encodes when the app asks for the next frame ([`Ask::Next`]): on the
//! GPU when the app wants it and it can ([`Gpu`]; a dma-buf from PipeWire
//! goes there without the processor touching a pixel), else in software
//! ([`crate::software_encoder`]), each at most as large as the
//! [`Profile`] says. Pacing, keyframe requests and the bitrate are the
//! app's: it asks for a picture a frame's time apart, a keyframe when a
//! receiver wants one, the last picture again for a still screen, and a
//! new rate as the bandwidth estimate moves.
//!
//! A camera's pipeline also keeps a small copy of each picture it keeps,
//! made before encoding ([`Settings::preview`]), and hands it out with
//! that picture's frame: the app's self-view, at the camera's pace, with
//! no full-size picture crossing the pipe.
//!
//! The server's side is [`Capture`]: it passes each request to the capture
//! thread and waits for the answer.

use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use noslacking_video_ipc::{CaptureProblem, CapturedFrame, Planes};

use crate::backend::{Encoded, Failure};
use crate::capture::{Frame, Order, Packed, Trouble, convert};
use crate::shrink;
use crate::software_encoder::SoftwareEncoder;

/// A new software encoder for a new bit rate at most this often: making
/// one costs an IDR.
const RETUNE_EVERY: Duration = Duration::from_secs(8);
/// How often the numbers go to the log.
const REPORT_EVERY: Duration = Duration::from_secs(10);

/// What a kind of capture sends, and how its pictures are treated.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Profile {
    /// What it is, for the log.
    pub what: &'static str,
    /// Pictures a second sent at most (and asked of the source).
    pub fps: u32,
    /// The largest picture encoded on the GPU.
    pub max_size: (u32, u32),
    /// The largest picture encoded in software.
    pub software_max: (u32, u32),
    /// A picture kept at most this often: a compositor sending 60 a
    /// second is not copied 60 times. Well under a frame's time, since
    /// frames come unevenly (a polled capture waits while its thread
    /// encodes) and the app paces what is sent.
    pub min_gap: Duration,
    /// Whether a picture the same as the last is dropped (a still
    /// screen); comparing a camera's noisy pictures would never find one.
    pub skip_unchanged: bool,
    /// The bit rates it is kept between, in bit/s.
    pub bitrates: (u32, u32),
}

impl Profile {
    /// A screen share: 1080p at 15 a second on the GPU, as Slack's own
    /// shares are (the JS SDK's content default), 720p in software, where
    /// 1080p takes 25–31 ms a picture.
    pub const SHARE: Self = Self {
        what: "share",
        fps: 15,
        max_size: (1920, 1080),
        software_max: crate::software_encoder::MAX_SIZE,
        min_gap: Duration::from_millis(40),
        skip_unchanged: true,
        bitrates: (50_000, 20_000_000),
    };

    /// A camera: 640×480 at most (480×480 to 640×480 is what Slack's own
    /// clients were seen sending), 30 a second, on the GPU or in software
    /// alike (about 4 ms a picture there).
    pub const CAMERA: Self = Self {
        what: "camera",
        fps: 30,
        max_size: (640, 480),
        software_max: (640, 480),
        min_gap: Duration::from_millis(20),
        skip_unchanged: false,
        bitrates: (50_000, 4_000_000),
    };
}

/// What makes a [`Gpu`] on the capture thread; none if there is none.
pub type GpuOpener = Arc<dyn Fn() -> Option<Box<dyn Gpu>> + Send + Sync>;

/// How a capture is set up.
#[derive(Clone)]
pub struct Settings {
    /// What it sends.
    pub profile: Profile,
    /// Encode on the GPU where it can.
    pub hardware: bool,
    /// The bit rate to start at.
    pub bitrate: u32,
    /// The GPU, if the back end has one that encodes.
    pub gpu: Option<GpuOpener>,
    /// The self-view's widest, in pixels: each kept picture is also
    /// shrunk by a whole step to at most this wide and handed out with
    /// its frame. 0 for none (a share).
    pub preview: u32,
}

impl Settings {
    /// A share's, as the app asks for it.
    pub fn share(hardware: bool, bitrate: u32, gpu: Option<GpuOpener>) -> Self {
        Self {
            profile: Profile::SHARE,
            hardware,
            bitrate,
            gpu,
            preview: 0,
        }
    }

    /// A camera's, as the app asks for it.
    pub fn camera(hardware: bool, bitrate: u32, gpu: Option<GpuOpener>, preview: u32) -> Self {
        Self {
            profile: Profile::CAMERA,
            hardware,
            bitrate,
            gpu,
            preview,
        }
    }
}

impl std::fmt::Debug for Settings {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Settings")
            .field("profile", &self.profile.what)
            .field("hardware", &self.hardware)
            .field("bitrate", &self.bitrate)
            .field("gpu", &self.gpu.is_some())
            .field("preview", &self.preview)
            .finish()
    }
}

/// A GPU's encoder for a capture: takes frames in (scaling and
/// converting them on the GPU) and encodes the last one. Made, and used,
/// on the capture thread.
pub trait Gpu {
    /// Its name, for the log.
    fn name(&self) -> String;
    /// Whether frames may come as dma-bufs, straight from GPU memory.
    fn takes_dmabuf(&self) -> bool;
    /// The largest picture it encodes.
    fn max_size(&self) -> (u32, u32);
    /// Readies it to encode `size` pictures at `fps` and `bitrate`; the
    /// next picture is an IDR.
    fn open(&mut self, size: (u32, u32), fps: u32, bitrate: u32) -> Result<(), Failure>;
    /// Puts `frame`, scaled to the size it was opened for, in as the
    /// picture to encode.
    fn load(&mut self, frame: &Frame<'_>) -> Result<(), Failure>;
    /// Encodes the picture last put in, an IDR if `force_keyframe`.
    fn encode(&mut self, force_keyframe: bool) -> Result<Encoded, Failure>;
    /// A new target bit rate, from the next picture on.
    fn set_bitrate(&mut self, bitrate: u32) -> Result<(), Failure>;
}

/// What the app's request for a capture's next frame comes to.
pub type Answer = Result<Option<CapturedFrame>, Trouble>;

/// What the capture thread is handed: the server's asks, and from a
/// reader thread (the camera's) its pictures.
pub enum Ask {
    /// The next picture, encoded (see `Request::NextFrame`).
    Next {
        /// Make it a keyframe.
        force_keyframe: bool,
        /// Encode the last picture again if nothing new comes.
        repeat: bool,
        /// How long to wait for something new.
        wait: Duration,
        /// Where the answer goes.
        reply: mpsc::Sender<Answer>,
    },
    /// A new bit rate.
    Bitrate(u32),
    /// Pictures from now on fit within this box too (none: only the
    /// profile's own limits).
    MaxSize(Option<(u32, u32)>),
    /// A captured picture, taken at `at`.
    Picture {
        /// The picture.
        picture: Planes,
        /// When it was taken.
        at: Instant,
    },
    /// The source ended (a camera unplugged, failing again and again).
    Ended(Trouble),
    /// The capture is closed: stop capturing.
    Stop,
}

/// A running capture, as the server holds it: closing it (dropping it)
/// stops the capture and waits for its thread.
pub struct Capture {
    send: Box<dyn Fn(Ask) -> bool + Send>,
    thread: Option<JoinHandle<()>>,
    ended: Arc<Mutex<Option<Trouble>>>,
}

impl std::fmt::Debug for Capture {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Capture").finish_non_exhaustive()
    }
}

impl Capture {
    /// A capture whose thread `thread` takes asks through `send` and
    /// leaves why it ended in `ended`.
    pub fn new(
        send: Box<dyn Fn(Ask) -> bool + Send>,
        thread: JoinHandle<()>,
        ended: Arc<Mutex<Option<Trouble>>>,
    ) -> Self {
        Self {
            send,
            thread: Some(thread),
            ended,
        }
    }

    /// Why the capture thread is gone.
    fn gone(&self) -> Trouble {
        self.ended
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
            .unwrap_or_else(|| Trouble::new(CaptureProblem::Ended, "the capture stopped"))
    }

    /// The next frame (see `Request::NextFrame`).
    pub fn next(&self, force_keyframe: bool, repeat: bool, wait: Duration) -> Answer {
        let (reply, answer) = mpsc::channel();
        let ask = Ask::Next {
            force_keyframe,
            repeat,
            wait,
            reply,
        };
        if !(self.send)(ask) {
            return Err(self.gone());
        }
        // An encode takes milliseconds; a capture thread that keeps the
        // answer this long is stuck, and the capture with it.
        match answer.recv_timeout(wait + Duration::from_millis(800)) {
            Ok(answer) => answer,
            Err(RecvTimeoutError::Timeout) => {
                Err(Trouble::failed("the capture thread does not answer"))
            }
            Err(RecvTimeoutError::Disconnected) => Err(self.gone()),
        }
    }

    /// A new bit rate from the next picture on.
    pub fn set_bitrate(&self, bitrate: u32) {
        (self.send)(Ask::Bitrate(bitrate));
    }

    /// A box the pictures fit within from now on (see
    /// [`Ask::MaxSize`]).
    pub fn set_max_size(&self, max: Option<(u32, u32)>) {
        (self.send)(Ask::MaxSize(max));
    }
}

impl Drop for Capture {
    fn drop(&mut self) {
        (self.send)(Ask::Stop);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// The last picture kept, for the next encode and to compare the next
/// one with.
enum Held {
    /// Packed RGB, copied out of the capture's buffer.
    Packed {
        width: u32,
        height: u32,
        stride: usize,
        order: Order,
        data: Vec<u8>,
    },
    /// I420.
    I420(Planes),
    /// Only on the GPU (a dma-buf): nothing the processor can encode.
    /// Linux's alone, where PipeWire hands dma-bufs over.
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    Gpu,
}

impl Held {
    fn packed(&self) -> Option<Packed<'_>> {
        match self {
            Self::Packed {
                width,
                height,
                stride,
                order,
                data,
            } => Some(Packed {
                width: *width,
                height: *height,
                stride: *stride,
                order: *order,
                data,
            }),
            _ => None,
        }
    }

    /// Whether `frame` shows the same as this, pixel for pixel (padding
    /// aside). A dma-buf is never compared: it is not read here.
    fn same(&self, frame: &Frame<'_>) -> bool {
        match (self, frame) {
            (Self::I420(held), Frame::I420(new)) => held == *new,
            (Self::Packed { .. }, Frame::Packed(new)) => self.packed().is_some_and(|held| {
                (held.width, held.height, held.order) == (new.width, new.height, new.order)
                    && held.rows().eq(new.rows())
            }),
            _ => false,
        }
    }

    /// As a frame to put in on the GPU again: none when it is only there
    /// already.
    fn frame(&self) -> Option<Frame<'_>> {
        match self {
            Self::Packed { .. } => self.packed().map(Frame::Packed),
            Self::I420(planes) => Some(Frame::I420(planes)),
            Self::Gpu => None,
        }
    }

    /// As the software encoder takes it, at most `max`.
    fn for_software(&self, max: (u32, u32)) -> Option<Planes> {
        match self {
            Self::Packed { .. } => convert::for_software(&self.packed()?, max),
            Self::I420(planes) => {
                let (w, h) = convert::fit(planes.width, planes.height, max)?;
                Some(convert::scale(planes.clone(), w, h))
            }
            Self::Gpu => None,
        }
    }
}

/// The largest box within both `limit` and `max` (none: `limit`).
fn within(limit: (u32, u32), max: Option<(u32, u32)>) -> (u32, u32) {
    max.map_or(limit, |(w, h)| (limit.0.min(w), limit.1.min(h)))
}

/// `frame`'s self-view, at most `width` wide: none for a dma-buf, which
/// the processor does not read.
fn preview_of(frame: &Frame<'_>, width: u32) -> Option<Planes> {
    match frame {
        Frame::I420(planes) => Some(shrink::preview(planes, width)),
        Frame::Packed(packed) => Some(shrink::preview(&convert::to_i420(packed)?, width)),
        #[cfg(target_os = "linux")]
        Frame::DmaBuf(_) => None,
    }
}

/// An ask for the next frame waiting for one.
struct Pending {
    force_keyframe: bool,
    repeat: bool,
    deadline: Instant,
    reply: mpsc::Sender<Answer>,
}

/// What the pipeline did, for the log.
#[derive(Clone, Copy, Debug, Default)]
struct Counts {
    kept: u64,
    unchanged: u64,
    skipped: u64,
    unusable: u64,
    encoded_gpu: u64,
    encoded_software: u64,
    repeated: u64,
    gpu_failures: u64,
    load: Duration,
    encode: Duration,
}

/// A capture's frames from capture to H.264, on the capture thread.
pub struct Pipeline {
    profile: Profile,
    hardware: bool,
    bitrate: u32,
    gpu: Option<Box<dyn Gpu>>,
    /// The capture's size the GPU was opened for, and the size it
    /// encodes.
    gpu_shape: Option<((u32, u32), (u32, u32))>,
    /// A box the pictures fit within as well as the profile's limits,
    /// as the app asks ([`Ask::MaxSize`]).
    max_box: Option<(u32, u32)>,
    /// The box changed since the GPU was opened: open it again, at the
    /// new size, with the next picture put in.
    reshape: bool,
    /// The GPU holds the last picture.
    gpu_loaded: bool,
    software: Option<SoftwareEncoder>,
    software_made: Option<Instant>,
    /// The last picture as the software encoder takes it, made on first
    /// need.
    software_picture: Option<Planes>,
    held: Option<Held>,
    /// When the picture not sent yet was captured.
    fresh: Option<Instant>,
    /// The self-view's widest; 0 for none.
    preview_width: u32,
    /// The self-view of the picture not sent yet.
    preview: Option<Planes>,
    last_kept: Option<Instant>,
    pending: Option<Pending>,
    ended: Option<Trouble>,
    /// Dma-bufs cannot be used (no GPU, or it would not take them): the
    /// capture should ask for frames in memory.
    wants_memory: bool,
    counts: Counts,
    next_report: Instant,
}

impl std::fmt::Debug for Pipeline {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Pipeline")
            .field("profile", &self.profile.what)
            .field("gpu", &self.gpu.is_some())
            .field("counts", &self.counts)
            .finish_non_exhaustive()
    }
}

impl Pipeline {
    /// A pipeline set up as `settings` say: the GPU opened now (on this,
    /// the capture thread) if it is wanted and there.
    pub fn new(settings: Settings) -> Self {
        let profile = settings.profile;
        let gpu = if settings.hardware {
            settings.gpu.as_ref().and_then(|open| open())
        } else {
            None
        };
        match &gpu {
            Some(gpu) => eprintln!(
                "noslacking-video: {}: encoding on {}",
                profile.what,
                gpu.name()
            ),
            None => eprintln!(
                "noslacking-video: {}: encoding in software, at most {}x{}{}",
                profile.what,
                profile.software_max.0,
                profile.software_max.1,
                if settings.hardware {
                    " (no GPU encoder)"
                } else {
                    ""
                }
            ),
        }
        Self {
            profile,
            hardware: settings.hardware,
            bitrate: settings
                .bitrate
                .clamp(profile.bitrates.0, profile.bitrates.1),
            gpu,
            gpu_shape: None,
            max_box: None,
            reshape: false,
            gpu_loaded: false,
            software: None,
            software_made: None,
            software_picture: None,
            held: None,
            fresh: None,
            preview_width: settings.preview.min(noslacking_video_ipc::MAX_PREVIEW_SIDE),
            preview: None,
            last_kept: None,
            pending: None,
            ended: None,
            wants_memory: false,
            counts: Counts::default(),
            next_report: Instant::now() + REPORT_EVERY,
        }
    }

    /// What it sends.
    pub fn profile(&self) -> Profile {
        self.profile
    }

    /// Whether the capture may hand over dma-bufs.
    pub fn takes_dmabuf(&self) -> bool {
        !self.wants_memory && self.gpu.as_ref().is_some_and(|gpu| gpu.takes_dmabuf())
    }

    /// Whether dma-bufs stopped being usable since the capture asked for
    /// them: it should ask for frames in memory instead.
    pub fn wants_memory(&self) -> bool {
        self.wants_memory
    }

    /// Why the capture ended, if it did.
    pub fn ended(&self) -> Option<Trouble> {
        self.ended.clone()
    }

    /// When a waiting ask must be answered, if one waits.
    pub fn deadline(&self) -> Option<Instant> {
        self.pending.as_ref().map(|p| p.deadline)
    }

    /// Whether the encoder runs on the GPU.
    pub fn on_gpu(&self) -> bool {
        self.gpu.is_some()
    }

    /// The GPU failed: software from here on.
    fn drop_gpu(&mut self, failure: &Failure) {
        eprintln!(
            "noslacking-video: {}: the GPU failed ({:?}: {}): software from here on",
            self.profile.what, failure.kind, failure.detail
        );
        self.gpu = None;
        self.gpu_shape = None;
        self.gpu_loaded = false;
        self.counts.gpu_failures += 1;
        if matches!(self.held, Some(Held::Gpu)) {
            // The last picture was only on the GPU.
            self.held = None;
            self.wants_memory = true;
        }
    }

    /// A captured frame, taken at `at`.
    pub fn put(&mut self, frame: &Frame<'_>, at: Instant) {
        let Some(only_on_gpu) = self.take_in(frame, at) else {
            return;
        };
        let held = match frame {
            _ if only_on_gpu => Held::Gpu,
            Frame::Packed(packed) => Held::Packed {
                width: packed.width,
                height: packed.height,
                stride: packed.stride,
                order: packed.order,
                data: packed.data.to_vec(),
            },
            Frame::I420(planes) => Held::I420((*planes).clone()),
            #[cfg(target_os = "linux")]
            Frame::DmaBuf(_) => Held::Gpu,
        };
        self.keep(held, at);
    }

    /// A captured picture of its own, taken at `at`: kept without a copy.
    pub fn put_picture(&mut self, picture: Planes, at: Instant) {
        if self.take_in(&Frame::I420(&picture), at).is_some() {
            self.keep(Held::I420(picture), at);
        }
    }

    /// Whether `frame`, taken at `at`, is kept: none if it is not (too
    /// soon, unchanged, unusable, a dma-buf the GPU would not take), else
    /// whether it is only on the GPU. A kept frame is on the GPU already,
    /// if it is used, and its self-view made. Paced by when frames were
    /// taken, not when they arrive here, so a late one does not crowd out
    /// the next.
    fn take_in(&mut self, frame: &Frame<'_>, at: Instant) -> Option<bool> {
        if self.ended.is_some() {
            return None;
        }
        if self
            .last_kept
            .is_some_and(|last| at < last + self.profile.min_gap)
        {
            self.counts.skipped += 1;
            return None;
        }
        if let Frame::Packed(packed) = frame
            && !packed.whole()
        {
            self.counts.unusable += 1;
            return None;
        }
        if self.profile.skip_unchanged && self.held.as_ref().is_some_and(|held| held.same(frame)) {
            self.counts.unchanged += 1;
            return None;
        }
        #[cfg(target_os = "linux")]
        let dmabuf = matches!(frame, Frame::DmaBuf(_));
        #[cfg(not(target_os = "linux"))]
        let dmabuf = false;
        let started = Instant::now();
        let loaded = self.load_on_gpu(frame);
        self.counts.load += started.elapsed();
        if dmabuf && !loaded {
            // A dma-buf the GPU did not take is lost: frames in memory
            // from now on.
            if !self.wants_memory {
                eprintln!(
                    "noslacking-video: {}: dma-bufs cannot be used: asking for memory",
                    self.profile.what
                );
            }
            self.wants_memory = true;
            return None;
        }
        // Before encoding, so the self-view keeps the camera's pace.
        if self.preview_width > 0 {
            self.preview = preview_of(frame, self.preview_width);
        }
        Some(dmabuf)
    }

    /// Keeps `held`, taken at `at`, as the picture to send next, and
    /// answers an ask waiting for it.
    fn keep(&mut self, held: Held, at: Instant) {
        self.held = Some(held);
        self.software_picture = None;
        self.fresh = Some(at);
        self.last_kept = Some(at);
        self.counts.kept += 1;
        if let Some(pending) = self.pending.take() {
            match self.answer(pending.force_keyframe, pending.repeat) {
                Some(answer) => {
                    let _ = pending.reply.send(answer);
                }
                None => self.pending = Some(pending),
            }
        }
    }

    /// Puts `frame` in on the GPU, opening it for the frame's size first
    /// if that changed; whether it went in.
    fn load_on_gpu(&mut self, frame: &Frame<'_>) -> bool {
        let Some(gpu) = self.gpu.as_mut() else {
            return false;
        };
        let source = frame.size();
        if self.reshape || self.gpu_shape.is_none_or(|(was, _)| was != source) {
            let max = gpu.max_size();
            let wanted = self.profile.max_size;
            let limit = within((max.0.min(wanted.0), max.1.min(wanted.1)), self.max_box);
            let Some(size) = convert::fit(source.0, source.1, limit) else {
                self.counts.unusable += 1;
                return false;
            };
            if let Err(failure) = gpu.open(size, self.profile.fps, self.bitrate) {
                self.drop_gpu(&failure);
                return false;
            }
            eprintln!(
                "noslacking-video: {}: {}x{} captured, {}x{} encoded on the GPU",
                self.profile.what, source.0, source.1, size.0, size.1
            );
            self.gpu_shape = Some((source, size));
            self.reshape = false;
        }
        match gpu.load(frame) {
            Ok(()) => {
                self.gpu_loaded = true;
                true
            }
            #[cfg(target_os = "linux")]
            Err(failure) if matches!(frame, Frame::DmaBuf(_)) => {
                // The GPU still encodes; it only would not take this
                // kind of buffer.
                eprintln!(
                    "noslacking-video: {}: a dma-buf did not import ({})",
                    self.profile.what, failure.detail
                );
                false
            }
            Err(failure) => {
                self.drop_gpu(&failure);
                false
            }
        }
    }

    /// An answer to an ask for the next frame now, if there is one: a
    /// new picture encoded, the last one again if `repeat` or
    /// `force_keyframe`, or why the capture ended. None means wait.
    fn answer(&mut self, force_keyframe: bool, repeat: bool) -> Option<Answer> {
        if let Some(trouble) = &self.ended {
            return Some(Err(trouble.clone()));
        }
        // When the picture was captured; none for one sent again.
        let captured = if let Some(at) = self.fresh.take() {
            Some(at)
        } else if (repeat || force_keyframe) && (self.gpu_loaded || self.held.is_some()) {
            self.counts.repeated += 1;
            None
        } else {
            return None;
        };
        // A picture sent again has had its self-view already.
        let preview = if captured.is_some() {
            self.preview.take()
        } else {
            None
        };
        Some(Ok(self.encode(force_keyframe).map(
            |(encoded, size, hardware)| CapturedFrame {
                keyframe: encoded.keyframe,
                hardware,
                width: size.0,
                height: size.1,
                // As the answer goes out, the encoding included.
                age_us: captured.map_or(0, |at| {
                    u32::try_from(at.elapsed().as_micros()).unwrap_or(u32::MAX)
                }),
                data: encoded.data,
                preview,
            },
        )))
    }

    /// Encodes the last picture: on the GPU while it serves, else in
    /// software. None if it could not be.
    fn encode(&mut self, force_keyframe: bool) -> Option<(Encoded, (u32, u32), bool)> {
        let started = Instant::now();
        let mut force_keyframe = force_keyframe;
        if self.gpu_loaded
            && let (Some(gpu), Some((_, size))) = (self.gpu.as_mut(), self.gpu_shape)
        {
            match gpu.encode(force_keyframe) {
                Ok(encoded) => {
                    self.counts.encoded_gpu += 1;
                    self.counts.encode += started.elapsed();
                    self.report();
                    return Some((encoded, size, true));
                }
                Err(failure) => {
                    self.drop_gpu(&failure);
                    force_keyframe = true;
                }
            }
        }
        if self.software_picture.is_none() {
            let max = within(self.profile.software_max, self.max_box);
            self.software_picture = self.held.as_ref().and_then(|held| held.for_software(max));
        }
        let picture = self.software_picture.as_ref()?;
        let size = (picture.width, picture.height);
        let now = Instant::now();
        let stale = self.software.as_ref().is_none_or(|encoder| {
            encoder.size() != size
                || (encoder.bitrate() != self.bitrate
                    && self.software_made.is_none_or(|at| now >= at + RETUNE_EVERY))
        });
        if stale {
            match SoftwareEncoder::new(size, self.profile.fps, self.bitrate) {
                Ok(encoder) => {
                    self.software = Some(encoder);
                    self.software_made = Some(now);
                }
                Err(failure) => {
                    eprintln!(
                        "noslacking-video: {}: {}",
                        self.profile.what, failure.detail
                    );
                    return None;
                }
            }
        }
        let encoder = self.software.as_mut()?;
        match encoder.encode(picture, force_keyframe) {
            Ok(encoded) => {
                self.counts.encoded_software += 1;
                self.counts.encode += started.elapsed();
                self.report();
                Some((encoded, size, false))
            }
            Err(failure) => {
                eprintln!(
                    "noslacking-video: {}: {}",
                    self.profile.what, failure.detail
                );
                // The reference chain may be broken: start over.
                self.software = None;
                None
            }
        }
    }

    /// The numbers, to the log every [`REPORT_EVERY`].
    fn report(&mut self) {
        let now = Instant::now();
        if now < self.next_report {
            return;
        }
        self.next_report = now + REPORT_EVERY;
        let c = &self.counts;
        let encoded = (c.encoded_gpu + c.encoded_software).max(1);
        eprintln!(
            "noslacking-video: {}: {} kept ({:.2} ms in), {} unchanged, {} over {} fps, {} \
             unusable; {} on the GPU, {} in software ({:.2} ms a picture), {} again, {} GPU \
             failures",
            self.profile.what,
            c.kept,
            c.load.as_secs_f64() * 1000.0 / c.kept.max(1) as f64,
            c.unchanged,
            c.skipped,
            self.profile.fps,
            c.unusable,
            c.encoded_gpu,
            c.encoded_software,
            c.encode.as_secs_f64() * 1000.0 / encoded as f64,
            c.repeated,
            c.gpu_failures
        );
    }

    /// The server's ask, or a reader thread's picture.
    pub fn ask(&mut self, ask: Ask) {
        match ask {
            Ask::Next {
                force_keyframe,
                repeat,
                wait,
                reply,
            } => {
                if let Some(answer) = self.answer(force_keyframe, repeat) {
                    let _ = reply.send(answer);
                } else if wait.is_zero() {
                    let _ = reply.send(Ok(None));
                } else {
                    // A newer ask replaces one still waiting, which is
                    // answered with nothing.
                    if let Some(old) = self.pending.take() {
                        let _ = old.reply.send(Ok(None));
                    }
                    self.pending = Some(Pending {
                        force_keyframe,
                        repeat,
                        deadline: Instant::now() + wait,
                        reply,
                    });
                }
            }
            Ask::Bitrate(bitrate) => {
                self.bitrate = bitrate.clamp(self.profile.bitrates.0, self.profile.bitrates.1);
                if let Some(gpu) = self.gpu.as_mut()
                    && let Err(failure) = gpu.set_bitrate(self.bitrate)
                {
                    self.drop_gpu(&failure);
                }
            }
            Ask::MaxSize(max) => self.set_max_size(max),
            Ask::Picture { picture, at } => self.put_picture(picture, at),
            Ask::Ended(trouble) => self.end(trouble),
            // The capture's loop stops on it.
            Ask::Stop => {}
        }
    }

    /// Keeps pictures within `max` from now on. The picture held is put
    /// in again at the new size, so a still screen changes size too
    /// (one only on the GPU, a dma-buf, waits for the next); either
    /// encoder starts the new size with a keyframe.
    fn set_max_size(&mut self, max: Option<(u32, u32)>) {
        let max = max.filter(|&(w, h)| w > 0 && h > 0);
        if max == self.max_box {
            return;
        }
        eprintln!(
            "noslacking-video: {}: pictures at most {}",
            self.profile.what,
            max.map_or_else(
                || "as large as before".to_owned(),
                |(w, h)| format!("{w}x{h}")
            )
        );
        self.max_box = max;
        self.reshape = true;
        self.software_picture = None;
        if let Some(held) = self.held.take() {
            if let Some(frame) = held.frame() {
                self.load_on_gpu(&frame);
            }
            self.held = Some(held);
        }
    }

    /// Answers an ask whose wait is over with nothing.
    pub fn tick(&mut self, now: Instant) {
        if self.pending.as_ref().is_some_and(|p| now >= p.deadline)
            && let Some(pending) = self.pending.take()
        {
            let _ = pending.reply.send(Ok(None));
        }
    }

    /// The capture ended (by itself, or failing): every ask from now on is
    /// told so.
    pub fn end(&mut self, trouble: Trouble) {
        if self.ended.is_some() {
            return;
        }
        eprintln!(
            "noslacking-video: {}: the capture ended: {trouble}",
            self.profile.what
        );
        if let Some(pending) = self.pending.take() {
            let _ = pending.reply.send(Err(trouble.clone()));
        }
        self.ended = Some(trouble);
    }

    /// Whether the GPU was wanted for this capture.
    pub fn hardware(&self) -> bool {
        self.hardware
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capture::pattern::pattern;

    fn settings(gpu: Option<GpuOpener>) -> Settings {
        Settings::share(gpu.is_some(), 1_000_000, gpu)
    }

    fn next(pipeline: &mut Pipeline, force_keyframe: bool, repeat: bool) -> Answer {
        let (reply, answer) = mpsc::channel();
        pipeline.ask(Ask::Next {
            force_keyframe,
            repeat,
            wait: Duration::ZERO,
            reply,
        });
        answer.recv().expect("an answer")
    }

    fn bgrx(width: u32, height: u32, fill: u8) -> Vec<u8> {
        vec![fill; width as usize * height as usize * 4]
    }

    fn packed(width: u32, height: u32, data: &[u8]) -> Frame<'_> {
        Frame::Packed(Packed {
            width,
            height,
            stride: width as usize * 4,
            order: Order::Bgra,
            data,
        })
    }

    /// Waits out the frame gap, so the next frame is kept.
    fn later() {
        std::thread::sleep(Profile::SHARE.min_gap + Duration::from_millis(2));
    }

    #[test]
    fn software_sends_new_pictures_shrunk_to_720p_and_skips_still_ones() {
        let mut pipeline = Pipeline::new(settings(None));
        assert!(!pipeline.on_gpu() && !pipeline.takes_dmabuf());
        // Nothing yet: nothing to send.
        assert_eq!(next(&mut pipeline, false, true), Ok(None));
        let grey = bgrx(1920, 1080, 120);
        pipeline.put(&packed(1920, 1080, &grey), Instant::now());
        let frame = next(&mut pipeline, false, false)
            .expect("no trouble")
            .expect("a frame");
        assert!(frame.keyframe && !frame.hardware);
        assert_eq!((frame.width, frame.height), (1280, 720));
        assert!(noslacking_video_ipc::h264::nal_types(&frame.data).starts_with(&[7, 8]));
        // The same picture again is not new.
        later();
        pipeline.put(&packed(1920, 1080, &grey), Instant::now());
        assert_eq!(next(&mut pipeline, false, false), Ok(None));
        // But it is sent again when asked to repeat, as a keyframe when
        // asked for one.
        let again = next(&mut pipeline, true, true)
            .expect("fine")
            .expect("a frame");
        assert!(again.keyframe);
        assert_eq!(again.age_us, 0);
        // A changed picture is new, and not a keyframe.
        later();
        let white = bgrx(1920, 1080, 230);
        pipeline.put(&packed(1920, 1080, &white), Instant::now());
        let changed = next(&mut pipeline, false, false)
            .expect("fine")
            .expect("a frame");
        assert!(!changed.keyframe);
        assert!(noslacking_video_ipc::h264::nal_types(&changed.data).contains(&1));
    }

    #[test]
    fn at_most_twenty_five_pictures_a_second_are_kept() {
        let mut pipeline = Pipeline::new(settings(None));
        // 400 ms of a camera giving a picture every 5 ms, timed by when
        // each was taken, not by the clock: one kept every 40 ms.
        let start = Instant::now();
        for n in 0..80 {
            let picture = pattern(64, 48, n, Duration::ZERO);
            let at = start + Duration::from_millis(5 * n);
            pipeline.put(&Frame::I420(&picture), at);
        }
        assert_eq!(pipeline.counts.kept, 10, "{:?}", pipeline.counts);
        assert_eq!(pipeline.counts.skipped, 70, "{:?}", pipeline.counts);
    }

    #[test]
    fn an_ask_waits_for_a_picture_or_its_deadline() {
        let mut pipeline = Pipeline::new(settings(None));
        let (reply, answer) = mpsc::channel();
        pipeline.ask(Ask::Next {
            force_keyframe: false,
            repeat: false,
            wait: Duration::from_millis(50),
            reply,
        });
        assert!(answer.try_recv().is_err(), "it waits");
        assert!(pipeline.deadline().is_some());
        let picture = pattern(320, 180, 0, Duration::ZERO);
        pipeline.put(&Frame::I420(&picture), Instant::now());
        let frame = answer
            .recv()
            .expect("answered")
            .expect("fine")
            .expect("a frame");
        assert_eq!((frame.width, frame.height), (320, 180));
        assert!(pipeline.deadline().is_none());
        // No picture comes: nothing, once the wait is over.
        let (reply, answer) = mpsc::channel();
        pipeline.ask(Ask::Next {
            force_keyframe: false,
            repeat: false,
            wait: Duration::from_millis(20),
            reply,
        });
        pipeline.tick(Instant::now());
        assert!(answer.try_recv().is_err());
        pipeline.tick(Instant::now() + Duration::from_millis(30));
        assert_eq!(answer.recv().expect("answered"), Ok(None));
        // An ended capture says so, to a waiting ask and every one after.
        let (reply, answer) = mpsc::channel();
        pipeline.ask(Ask::Next {
            force_keyframe: false,
            repeat: false,
            wait: Duration::from_millis(100),
            reply,
        });
        pipeline.end(Trouble::new(CaptureProblem::Ended, "closed"));
        assert_eq!(
            answer.recv().expect("answered").map_err(|t| t.problem),
            Err(CaptureProblem::Ended)
        );
        assert!(next(&mut pipeline, true, true).is_err());
    }

    /// A pretend GPU: counts what it is given, fails as told.
    #[derive(Default)]
    struct FakeGpu {
        log: Arc<Mutex<Vec<String>>>,
        fail_encode_after: Option<usize>,
        dmabuf: bool,
        encoded: usize,
    }

    impl Gpu for FakeGpu {
        fn name(&self) -> String {
            "a pretend GPU".into()
        }
        fn takes_dmabuf(&self) -> bool {
            self.dmabuf
        }
        fn max_size(&self) -> (u32, u32) {
            (4096, 4096)
        }
        fn open(&mut self, size: (u32, u32), fps: u32, bitrate: u32) -> Result<(), Failure> {
            // As a real one: the first picture after opening is an IDR.
            self.encoded = 0;
            self.log
                .lock()
                .expect("a lock")
                .push(format!("open {}x{} {fps} {bitrate}", size.0, size.1));
            Ok(())
        }
        fn load(&mut self, frame: &Frame<'_>) -> Result<(), Failure> {
            let (w, h) = frame.size();
            self.log
                .lock()
                .expect("a lock")
                .push(format!("load {w}x{h}"));
            Ok(())
        }
        fn encode(&mut self, force_keyframe: bool) -> Result<Encoded, Failure> {
            if self.fail_encode_after.is_some_and(|n| self.encoded >= n) {
                return Err(Failure::device("hung"));
            }
            let keyframe = force_keyframe || self.encoded == 0;
            self.encoded += 1;
            Ok(Encoded {
                keyframe,
                data: if keyframe {
                    vec![0, 0, 0, 1, 0x67, 0, 0, 0, 1, 0x68, 0, 0, 0, 1, 0x65]
                } else {
                    vec![0, 0, 0, 1, 0x41]
                },
            })
        }
        fn set_bitrate(&mut self, bitrate: u32) -> Result<(), Failure> {
            self.log
                .lock()
                .expect("a lock")
                .push(format!("rate {bitrate}"));
            Ok(())
        }
    }

    fn fake(gpu: impl Fn() -> FakeGpu + Send + Sync + 'static) -> GpuOpener {
        Arc::new(move || Some(Box::new(gpu()) as Box<dyn Gpu>))
    }

    #[test]
    fn the_gpu_encodes_1080p_and_takes_new_rates_in_place() {
        let log = Arc::new(Mutex::new(Vec::new()));
        let seen = Arc::clone(&log);
        let mut pipeline = Pipeline::new(settings(Some(fake(move || FakeGpu {
            log: Arc::clone(&seen),
            ..FakeGpu::default()
        }))));
        assert!(pipeline.on_gpu());
        let screen = bgrx(2560, 1440, 50);
        pipeline.put(&packed(2560, 1440, &screen), Instant::now());
        let frame = next(&mut pipeline, false, false)
            .expect("fine")
            .expect("a frame");
        assert!(frame.hardware && frame.keyframe);
        assert_eq!(
            (frame.width, frame.height),
            (1920, 1080),
            "1440p fits 1080p"
        );
        pipeline.ask(Ask::Bitrate(600_000));
        let repeated = next(&mut pipeline, false, true)
            .expect("fine")
            .expect("a frame");
        assert!(!repeated.keyframe);
        assert_eq!(
            *log.lock().expect("a lock"),
            ["open 1920x1080 15 1000000", "load 2560x1440", "rate 600000"]
        );
    }

    #[test]
    fn a_box_resizes_the_gpus_pictures_even_on_a_still_screen() {
        let log = Arc::new(Mutex::new(Vec::new()));
        let seen = Arc::clone(&log);
        let mut pipeline = Pipeline::new(settings(Some(fake(move || FakeGpu {
            log: Arc::clone(&seen),
            ..FakeGpu::default()
        }))));
        let screen = bgrx(1728, 1080, 50);
        pipeline.put(&packed(1728, 1080, &screen), Instant::now());
        let first = next(&mut pipeline, false, false)
            .expect("fine")
            .expect("a frame");
        assert_eq!((first.width, first.height), (1728, 1080));
        // Asked to fit 1280×720, with nothing new captured: the picture
        // held goes in again, smaller, and starts with a keyframe.
        pipeline.ask(Ask::MaxSize(Some((1280, 720))));
        let smaller = next(&mut pipeline, false, true)
            .expect("fine")
            .expect("a frame");
        assert_eq!((smaller.width, smaller.height), (1152, 720));
        assert!(smaller.hardware && smaller.keyframe);
        // The same box again changes nothing; no box is the full size.
        pipeline.ask(Ask::MaxSize(Some((1280, 720))));
        pipeline.ask(Ask::MaxSize(None));
        let full = next(&mut pipeline, false, true)
            .expect("fine")
            .expect("a frame");
        assert_eq!((full.width, full.height), (1728, 1080));
        assert!(full.keyframe);
        assert_eq!(
            *log.lock().expect("a lock"),
            [
                "open 1728x1080 15 1000000",
                "load 1728x1080",
                "open 1152x720 15 1000000",
                "load 1728x1080",
                "open 1728x1080 15 1000000",
                "load 1728x1080",
            ]
        );
    }

    #[test]
    fn a_box_shrinks_software_pictures_too() {
        let mut pipeline = Pipeline::new(settings(None));
        let picture = pattern(1280, 720, 0, Duration::ZERO);
        pipeline.put(&Frame::I420(&picture), Instant::now());
        let first = next(&mut pipeline, false, false)
            .expect("fine")
            .expect("a frame");
        assert_eq!((first.width, first.height), (1280, 720));
        pipeline.ask(Ask::MaxSize(Some((640, 360))));
        let smaller = next(&mut pipeline, false, true)
            .expect("fine")
            .expect("a frame");
        assert_eq!((smaller.width, smaller.height), (640, 360));
        assert!(!smaller.hardware && smaller.keyframe);
        // A box larger than software's own limit leaves that limit.
        pipeline.ask(Ask::MaxSize(Some((1920, 1080))));
        let back = next(&mut pipeline, false, true)
            .expect("fine")
            .expect("a frame");
        assert_eq!((back.width, back.height), (1280, 720));
    }

    #[test]
    fn a_failing_gpu_hands_over_to_software_with_a_keyframe() {
        let mut pipeline = Pipeline::new(settings(Some(fake(|| FakeGpu {
            fail_encode_after: Some(1),
            ..FakeGpu::default()
        }))));
        let picture = pattern(1920, 1080, 0, Duration::ZERO);
        pipeline.put(&Frame::I420(&picture), Instant::now());
        let first = next(&mut pipeline, false, false)
            .expect("fine")
            .expect("a frame");
        assert!(first.hardware);
        later();
        let picture = pattern(1920, 1080, 1, Duration::ZERO);
        pipeline.put(&Frame::I420(&picture), Instant::now());
        let second = next(&mut pipeline, false, false)
            .expect("fine")
            .expect("a frame");
        assert!(!second.hardware && second.keyframe);
        assert_eq!((second.width, second.height), (1280, 720));
        assert!(!pipeline.on_gpu());
    }

    #[test]
    fn without_the_setting_the_gpu_is_not_opened() {
        let opened = Arc::new(Mutex::new(0));
        let count = Arc::clone(&opened);
        let opener: GpuOpener = Arc::new(move || {
            *count.lock().expect("a lock") += 1;
            Some(Box::new(FakeGpu::default()) as Box<dyn Gpu>)
        });
        let pipeline = Pipeline::new(Settings::share(false, 1, Some(opener)));
        assert!(!pipeline.on_gpu());
        assert_eq!(*opened.lock().expect("a lock"), 0);
    }

    /// A camera's pictures, put in as the camera's reader thread hands
    /// them over: every one kept (none compared), 640×480 at most, a
    /// self-view with each new picture and none with one sent again.
    #[test]
    fn a_camera_keeps_every_picture_and_hands_out_its_self_view() {
        let mut pipeline = Pipeline::new(Settings::camera(false, 600_000, None, 320));
        let still = pattern(640, 480, 0, Duration::ZERO);
        pipeline.ask(Ask::Picture {
            picture: still.clone(),
            at: Instant::now(),
        });
        let first = next(&mut pipeline, false, false)
            .expect("fine")
            .expect("a frame");
        assert!(first.keyframe && !first.hardware);
        assert_eq!((first.width, first.height), (640, 480));
        let preview = first.preview.expect("a self-view");
        assert_eq!((preview.width, preview.height), (320, 240));
        assert!(preview.check().is_ok());
        // The same picture again is new for a camera.
        std::thread::sleep(Profile::CAMERA.min_gap + Duration::from_millis(2));
        pipeline.put_picture(still, Instant::now());
        let again = next(&mut pipeline, false, false)
            .expect("fine")
            .expect("a frame");
        assert!(!again.keyframe && again.preview.is_some());
        // Asked to repeat with nothing new: the picture again, without a
        // self-view.
        let repeated = next(&mut pipeline, true, true)
            .expect("fine")
            .expect("a frame");
        assert!(repeated.keyframe && repeated.preview.is_none());
        assert_eq!(repeated.age_us, 0);
        // A larger camera is sent at 640×480 at most, its shape kept.
        std::thread::sleep(Profile::CAMERA.min_gap + Duration::from_millis(2));
        pipeline.put_picture(pattern(1280, 720, 1, Duration::ZERO), Instant::now());
        let wide = next(&mut pipeline, false, false)
            .expect("fine")
            .expect("a frame");
        assert_eq!((wide.width, wide.height), (640, 360));
        let preview = wide.preview.expect("a self-view");
        assert_eq!((preview.width, preview.height), (320, 180));
        // A camera that stops says so to the next ask.
        pipeline.ask(Ask::Ended(Trouble::new(CaptureProblem::Ended, "unplugged")));
        assert_eq!(
            next(&mut pipeline, false, false).map_err(|t| t.problem),
            Err(CaptureProblem::Ended)
        );
    }

    /// Without a self-view asked for, and for a share, there is none.
    #[test]
    fn a_share_has_no_self_view() {
        let mut pipeline = Pipeline::new(settings(None));
        pipeline.put_picture(pattern(320, 180, 0, Duration::ZERO), Instant::now());
        let frame = next(&mut pipeline, false, false)
            .expect("fine")
            .expect("a frame");
        assert!(frame.preview.is_none());
        let mut camera = Pipeline::new(Settings::camera(false, 600_000, None, 0));
        camera.put_picture(pattern(320, 240, 0, Duration::ZERO), Instant::now());
        let frame = next(&mut camera, false, false)
            .expect("fine")
            .expect("a frame");
        assert!(frame.preview.is_none());
    }

    /// The camera's GPU is opened for 640×480 at 30 a second.
    #[test]
    fn a_camera_on_the_gpu_is_opened_at_its_size_and_rate() {
        let log = Arc::new(Mutex::new(Vec::new()));
        let seen = Arc::clone(&log);
        let mut pipeline = Pipeline::new(Settings::camera(
            true,
            600_000,
            Some(fake(move || FakeGpu {
                log: Arc::clone(&seen),
                ..FakeGpu::default()
            })),
            160,
        ));
        pipeline.put_picture(pattern(1280, 960, 0, Duration::ZERO), Instant::now());
        let frame = next(&mut pipeline, false, false)
            .expect("fine")
            .expect("a frame");
        assert!(frame.hardware && frame.keyframe);
        assert_eq!((frame.width, frame.height), (640, 480));
        let preview = frame.preview.expect("a self-view, made before the GPU");
        assert_eq!((preview.width, preview.height), (160, 120));
        assert_eq!(
            *log.lock().expect("a lock"),
            ["open 640x480 30 600000", "load 1280x960"]
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn dmabufs_go_to_the_gpu_only() {
        use std::os::fd::AsFd;
        let file = std::fs::File::open("/dev/null").expect("/dev/null");
        let dmabuf = || {
            Frame::DmaBuf(crate::capture::DmaBuf {
                fd: file.as_fd(),
                width: 1920,
                height: 1080,
                offset: 0,
                stride: 1920 * 4,
                order: Order::Bgra,
                alpha: false,
                modifier: 0,
            })
        };
        let mut pipeline = Pipeline::new(settings(Some(fake(|| FakeGpu {
            dmabuf: true,
            ..FakeGpu::default()
        }))));
        assert!(pipeline.takes_dmabuf());
        pipeline.put(&dmabuf(), Instant::now());
        let frame = next(&mut pipeline, false, false)
            .expect("fine")
            .expect("a frame");
        assert!(frame.hardware);
        // In software, a dma-buf cannot be read: memory is asked for.
        let mut software = Pipeline::new(settings(None));
        software.put(&dmabuf(), Instant::now());
        assert!(software.wants_memory() && !software.takes_dmabuf());
        assert_eq!(next(&mut software, false, true), Ok(None));
    }
}
