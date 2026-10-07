//! A screen share, from captured frames to the H.264 the app sends.
//!
//! The capture ([`crate::capture`]) runs on a thread of its own and hands
//! each frame to the share's [`Pipeline`] on that thread. The pipeline
//! keeps at most 25 pictures a second (the app asks for at most
//! [`capture::FPS`]), skips pictures that
//! did not change (a screen is mostly still), and encodes when the app
//! asks for the next frame ([`Ask::Next`]): on the GPU when the app wants
//! it and it can ([`Gpu`]; a dma-buf from PipeWire goes there without
//! the processor touching a pixel), else in software at most 1280×720
//! ([`crate::software_encoder`]). Pacing, keyframe requests and the
//! bitrate are the app's: it asks for a picture a frame's time apart, a
//! keyframe when a receiver wants one, the last picture again for a still
//! screen, and a new rate as the bandwidth estimate moves.
//!
//! The server's side is [`Share`]: it passes each request to the capture
//! thread and waits for the answer.

use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use noslacking_video_ipc::{Planes, ShareFrame, ShareProblem};

use crate::backend::{Encoded, Failure};
use crate::capture::{self, Frame, Order, Packed, Trouble, convert};
use crate::software_encoder::{self, SoftwareEncoder};

/// The largest picture a share sends: 1080p, as Slack's own shares do.
pub const MAX_SIZE: (u32, u32) = (1920, 1080);
/// The bit rates a share is kept between, in bit/s.
pub const BITRATES: (u32, u32) = (50_000, 20_000_000);
/// A new software encoder for a new bit rate at most this often: making
/// one costs an IDR.
const RETUNE_EVERY: Duration = Duration::from_secs(8);
/// A frame kept at most this often: a compositor sending 60 a second is
/// not copied 60 times. Well under a frame's time at [`capture::FPS`],
/// since frames come unevenly (a polled capture waits while its thread
/// encodes) and the app paces what is sent.
const MIN_GAP: Duration = Duration::from_millis(40);
/// How often the numbers go to the log.
const REPORT_EVERY: Duration = Duration::from_secs(10);

/// What makes a [`Gpu`] on the capture thread; none if there is none.
pub type GpuOpener = Arc<dyn Fn() -> Option<Box<dyn Gpu>> + Send + Sync>;

/// How a share is set up.
#[derive(Clone)]
pub struct Settings {
    /// Encode on the GPU where it can.
    pub hardware: bool,
    /// The bit rate to start at.
    pub bitrate: u32,
    /// The GPU, if the back end has one that encodes.
    pub gpu: Option<GpuOpener>,
}

impl std::fmt::Debug for Settings {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Settings")
            .field("hardware", &self.hardware)
            .field("bitrate", &self.bitrate)
            .field("gpu", &self.gpu.is_some())
            .finish()
    }
}

/// A GPU's share encoder: takes frames in (scaling and converting them
/// on the GPU) and encodes the last one. Made, and used, on the capture
/// thread.
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

/// What the app's request for a share's next frame comes to.
pub type Answer = Result<Option<ShareFrame>, Trouble>;

/// What the server asks of the capture thread.
pub enum Ask {
    /// The next picture, encoded (see `Request::NextShareFrame`).
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
    /// The share is closed: stop capturing.
    Stop,
}

/// A running share, as the server holds it: closing it (dropping it)
/// stops the capture and waits for its thread.
pub struct Share {
    send: Box<dyn Fn(Ask) -> bool + Send>,
    thread: Option<JoinHandle<()>>,
    ended: Arc<Mutex<Option<Trouble>>>,
}

impl std::fmt::Debug for Share {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Share").finish_non_exhaustive()
    }
}

impl Share {
    /// A share whose capture thread `thread` takes asks through `send`
    /// and leaves why it ended in `ended`.
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
            .unwrap_or_else(|| Trouble::new(ShareProblem::Ended, "the capture stopped"))
    }

    /// The next frame (see `Request::NextShareFrame`).
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
        // answer this long is stuck, and the share with it.
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
}

impl Drop for Share {
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

/// A share's frames from capture to H.264, on the capture thread.
pub struct Pipeline {
    hardware: bool,
    bitrate: u32,
    gpu: Option<Box<dyn Gpu>>,
    /// The capture's size the GPU was opened for, and the size it
    /// encodes.
    gpu_shape: Option<((u32, u32), (u32, u32))>,
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
            .field("gpu", &self.gpu.is_some())
            .field("counts", &self.counts)
            .finish_non_exhaustive()
    }
}

impl Pipeline {
    /// A pipeline set up as `settings` say: the GPU opened now (on this,
    /// the capture thread) if it is wanted and there.
    pub fn new(settings: Settings) -> Self {
        let gpu = if settings.hardware {
            settings.gpu.as_ref().and_then(|open| open())
        } else {
            None
        };
        match &gpu {
            Some(gpu) => eprintln!("noslacking-video: share: encoding on {}", gpu.name()),
            None => eprintln!(
                "noslacking-video: share: encoding in software, at most {}x{}{}",
                software_encoder::MAX_SIZE.0,
                software_encoder::MAX_SIZE.1,
                if settings.hardware {
                    " (no GPU encoder)"
                } else {
                    ""
                }
            ),
        }
        Self {
            hardware: settings.hardware,
            bitrate: settings.bitrate.clamp(BITRATES.0, BITRATES.1),
            gpu,
            gpu_shape: None,
            gpu_loaded: false,
            software: None,
            software_made: None,
            software_picture: None,
            held: None,
            fresh: None,
            last_kept: None,
            pending: None,
            ended: None,
            wants_memory: false,
            counts: Counts::default(),
            next_report: Instant::now() + REPORT_EVERY,
        }
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

    /// Why the share ended, if it did.
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
            "noslacking-video: share: the GPU failed ({:?}: {}): software from here on",
            failure.kind, failure.detail
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
        if self.ended.is_some() {
            return;
        }
        let now = Instant::now();
        if self.last_kept.is_some_and(|last| now < last + MIN_GAP) {
            self.counts.skipped += 1;
            return;
        }
        let held = match frame {
            Frame::Packed(packed) if !packed.whole() => {
                self.counts.unusable += 1;
                return;
            }
            _ if self.held.as_ref().is_some_and(|held| held.same(frame)) => {
                self.counts.unchanged += 1;
                return;
            }
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
        let started = Instant::now();
        let loaded = self.load_on_gpu(frame);
        self.counts.load += started.elapsed();
        if matches!(held, Held::Gpu) && !loaded {
            // A dma-buf the GPU did not take is lost: frames in memory
            // from now on.
            if !self.wants_memory {
                eprintln!("noslacking-video: share: dma-bufs cannot be used: asking for memory");
            }
            self.wants_memory = true;
            return;
        }
        self.held = Some(held);
        self.software_picture = None;
        self.fresh = Some(at);
        self.last_kept = Some(now);
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
        if self.gpu_shape.is_none_or(|(was, _)| was != source) {
            let max = gpu.max_size();
            let limit = (max.0.min(MAX_SIZE.0), max.1.min(MAX_SIZE.1));
            let Some(size) = convert::fit(source.0, source.1, limit) else {
                self.counts.unusable += 1;
                return false;
            };
            if let Err(failure) = gpu.open(size, capture::FPS, self.bitrate) {
                self.drop_gpu(&failure);
                return false;
            }
            eprintln!(
                "noslacking-video: share: {}x{} captured, {}x{} encoded on the GPU",
                source.0, source.1, size.0, size.1
            );
            self.gpu_shape = Some((source, size));
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
                    "noslacking-video: share: a dma-buf did not import ({})",
                    failure.detail
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
    /// `force_keyframe`, or why the share ended. None means wait.
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
        Some(Ok(self.encode(force_keyframe).map(
            |(encoded, size, hardware)| ShareFrame {
                keyframe: encoded.keyframe,
                hardware,
                width: size.0,
                height: size.1,
                // As the answer goes out, the encoding included.
                age_us: captured.map_or(0, |at| {
                    u32::try_from(at.elapsed().as_micros()).unwrap_or(u32::MAX)
                }),
                data: encoded.data,
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
            self.software_picture = self
                .held
                .as_ref()
                .and_then(|held| held.for_software(software_encoder::MAX_SIZE));
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
            match SoftwareEncoder::new(size, capture::FPS, self.bitrate) {
                Ok(encoder) => {
                    self.software = Some(encoder);
                    self.software_made = Some(now);
                }
                Err(failure) => {
                    eprintln!("noslacking-video: share: {}", failure.detail);
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
                eprintln!("noslacking-video: share: {}", failure.detail);
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
            "noslacking-video: share: {} kept ({:.2} ms in), {} unchanged, {} over {} fps, {} \
             unusable; {} on the GPU, {} in software ({:.2} ms a picture), {} again, {} GPU \
             failures",
            c.kept,
            c.load.as_secs_f64() * 1000.0 / c.kept.max(1) as f64,
            c.unchanged,
            c.skipped,
            capture::FPS,
            c.unusable,
            c.encoded_gpu,
            c.encoded_software,
            c.encode.as_secs_f64() * 1000.0 / encoded as f64,
            c.repeated,
            c.gpu_failures
        );
    }

    /// The server's ask.
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
                self.bitrate = bitrate.clamp(BITRATES.0, BITRATES.1);
                if let Some(gpu) = self.gpu.as_mut()
                    && let Err(failure) = gpu.set_bitrate(self.bitrate)
                {
                    self.drop_gpu(&failure);
                }
            }
            // The capture's loop stops on it.
            Ask::Stop => {}
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
        eprintln!("noslacking-video: share: the capture ended: {trouble}");
        if let Some(pending) = self.pending.take() {
            let _ = pending.reply.send(Err(trouble.clone()));
        }
        self.ended = Some(trouble);
    }

    /// Whether the GPU was wanted for this share.
    pub fn hardware(&self) -> bool {
        self.hardware
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capture::pattern::pattern;
    use crate::nal;

    fn settings(gpu: Option<GpuOpener>) -> Settings {
        Settings {
            hardware: gpu.is_some(),
            bitrate: 1_000_000,
            gpu,
        }
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
        std::thread::sleep(MIN_GAP + Duration::from_millis(2));
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
        assert!(nal::types(&frame.data).starts_with(&[7, 8]));
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
        assert!(nal::types(&changed.data).contains(&1));
    }

    #[test]
    fn at_most_twenty_five_pictures_a_second_are_kept() {
        let mut pipeline = Pipeline::new(settings(None));
        let start = Instant::now();
        let mut n = 0;
        while start.elapsed() < Duration::from_millis(400) {
            let picture = pattern(64, 48, n, Duration::ZERO);
            pipeline.put(&Frame::I420(&picture), Instant::now());
            n += 1;
            std::thread::sleep(Duration::from_millis(5));
        }
        // 400 ms, one kept every 40 ms or a little more: about ten.
        assert!(
            (7..=11).contains(&pipeline.counts.kept),
            "{:?}",
            pipeline.counts
        );
        assert!(pipeline.counts.skipped > 20);
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
        pipeline.end(Trouble::new(ShareProblem::Ended, "closed"));
        assert_eq!(
            answer.recv().expect("answered").map_err(|t| t.problem),
            Err(ShareProblem::Ended)
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
        let pipeline = Pipeline::new(Settings {
            hardware: false,
            bitrate: 1,
            gpu: Some(opener),
        });
        assert!(!pipeline.on_gpu());
        assert_eq!(*opened.lock().expect("a lock"), 0);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn dmabufs_go_to_the_gpu_only() {
        use std::os::fd::AsFd;
        let file = std::fs::File::open("/dev/null").expect("/dev/null");
        let dmabuf = || {
            Frame::DmaBuf(capture::DmaBuf {
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
