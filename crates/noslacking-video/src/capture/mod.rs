//! Capturing the screen the user shares: what can be shared, and the
//! frames it gives.
//!
//! The rule, the microphone's and the camera's: nothing is captured until
//! the app asks to share ([`Screens::start`]), and capture stops (its
//! thread joined, the portal's session closed) when the share is closed,
//! which the app does when the user stops, leaves, or the share's session
//! fails. Where the frames come from, chosen at run time ([`System`]):
//!
//! - Linux under Wayland and in the Flatpak: the ScreenCast portal over
//!   D-Bus (`ashpd`) and PipeWire (the `pipewire` crate), with the
//!   `pipewire` feature; the user picks in the portal's own dialog. The
//!   frames come as dma-bufs straight to the GPU's encoder where it can
//!   take them, else in shared memory.
//! - Linux under X11 without a portal: the X server (`x11rb`, pure Rust),
//!   whole screens, from the app's picker.
//! - macOS and Windows: `xcap`, screens and windows from the app's
//!   picker.
//! - [`ShareChoice::Test`]: a generated 1080p screen with a moving clock
//!   (`pattern`), for the probe and the benchmarks.
//!
//! Each capture runs on a thread of its own, which also runs the share's
//! encoding ([`crate::share::Pipeline`]): a dma-buf is only good while
//! PipeWire lends it, so it must reach the GPU on the thread that has
//! it, and a GPU's state (libva's display) stays on one thread.

pub mod convert;
pub mod pattern;
#[cfg(all(target_os = "linux", feature = "pipewire"))]
#[allow(unsafe_code)]
mod portal;
#[cfg(target_os = "linux")]
mod x11;
#[cfg(any(target_os = "macos", windows))]
mod xcap_capture;

use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use noslacking_video_ipc::{Planes, ShareChoice, ShareProblem, Source};

use crate::share::{Ask, Pipeline, Settings, Share};

/// Frames a second asked of a capture and sent at most: the JS SDK's
/// default for content (`ContentShareMediaStreamBroker.defaultFrameRate`).
pub const FPS: u32 = 15;

/// Why a share did not start, or stopped: what the app is told, and more
/// for its log.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Trouble {
    /// What happened.
    pub problem: ShareProblem,
    /// Why, for the log.
    pub detail: String,
}

impl Trouble {
    /// `problem`, because of `detail`.
    pub fn new(problem: ShareProblem, detail: impl Into<String>) -> Self {
        Self {
            problem,
            detail: detail.into(),
        }
    }

    /// It failed, because of `detail`.
    pub fn failed(detail: impl Into<String>) -> Self {
        Self::new(ShareProblem::Failed, detail)
    }
}

impl std::fmt::Display for Trouble {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:?}: {}", self.problem, self.detail)
    }
}

/// The byte order of a packed picture.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Order {
    /// Blue, green, red, then alpha or padding (PipeWire's BGRx/BGRA,
    /// the X server's 32-bit pixels).
    Bgra,
    /// Red, green, blue, then alpha or padding (`xcap`'s images).
    Rgba,
}

/// A packed picture of four bytes a pixel, borrowed from its source.
#[derive(Clone, Copy)]
pub struct Packed<'a> {
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
    /// Bytes from one row to the next, at least `width * 4`.
    pub stride: usize,
    /// The byte order.
    pub order: Order,
    /// The pixels.
    pub data: &'a [u8],
}

impl std::fmt::Debug for Packed<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Packed({}x{} {:?})", self.width, self.height, self.order)
    }
}

impl Packed<'_> {
    /// Whether the buffer holds every row the size and stride say.
    pub fn whole(&self) -> bool {
        let (width, height) = (self.width as usize, self.height as usize);
        width > 0
            && height > 0
            && self.stride >= width * 4
            && self
                .stride
                .checked_mul(height - 1)
                .and_then(|n| n.checked_add(width * 4))
                .is_some_and(|n| self.data.len() >= n)
    }

    /// Each row's pixels, without the padding after them.
    pub fn rows(&self) -> impl Iterator<Item = &[u8]> {
        let width = self.width as usize * 4;
        self.data
            .chunks(self.stride.max(1))
            .take(self.height as usize)
            .map(move |row| &row[..width.min(row.len())])
    }
}

/// A dma-buf PipeWire lends for one frame: one plane of packed 8-bit
/// RGB in GPU memory, never read by the processor here.
#[cfg(target_os = "linux")]
#[derive(Debug)]
pub struct DmaBuf<'a> {
    /// The buffer's file descriptor, open while it is lent.
    pub fd: std::os::fd::BorrowedFd<'a>,
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
    /// Where the picture starts in the buffer.
    pub offset: u32,
    /// Bytes from one row to the next.
    pub stride: u32,
    /// The byte order.
    pub order: Order,
    /// Whether the fourth byte is alpha (BGRA, RGBA) or padding.
    pub alpha: bool,
    /// The layout's DRM format modifier (`DRM_FORMAT_MOD_LINEAR`: 0).
    pub modifier: u64,
}

/// One captured frame, as its source gives it.
#[derive(Debug)]
pub enum Frame<'a> {
    /// Four bytes a pixel in memory.
    Packed(Packed<'a>),
    /// Already I420 (the test screen).
    I420(&'a Planes),
    /// In GPU memory.
    #[cfg(target_os = "linux")]
    DmaBuf(DmaBuf<'a>),
}

impl Frame<'_> {
    /// Its width and height.
    pub fn size(&self) -> (u32, u32) {
        match self {
            Self::Packed(packed) => (packed.width, packed.height),
            Self::I420(planes) => (planes.width, planes.height),
            #[cfg(target_os = "linux")]
            Self::DmaBuf(buffer) => (buffer.width, buffer.height),
        }
    }
}

/// What a share can be started on: the system's ways to capture, or
/// pretend ones in tests.
pub trait Screens {
    /// What the app's picker offers: `(dialog, sources)`, `dialog` when
    /// the system shows its own when the share starts.
    fn sources(&mut self) -> Result<(bool, Vec<Source>), Trouble>;
    /// Starts capturing `choice` and encoding it as `settings` say: the
    /// share, and the restore token to give next time (empty: none).
    /// `restore` is the last one. Blocks while the system's dialog is
    /// open.
    fn start(
        &mut self,
        choice: &ShareChoice,
        settings: Settings,
        restore: &str,
    ) -> Result<(Share, String), Trouble>;
}

/// Starts a capture thread named `name`, running `body` with the share's
/// pipeline (made on the thread) and its asks, once `body` has said
/// through `started` whether the capture started. The share it returns
/// stops the thread, and waits for it, when dropped.
pub fn spawn(
    name: &str,
    settings: Settings,
    body: impl FnOnce(&mut Pipeline, Receiver<Ask>, &mpsc::Sender<Result<(), Trouble>>) + Send + 'static,
) -> Result<Share, Trouble> {
    let (asks, inbox) = mpsc::channel();
    let (started, result) = mpsc::channel();
    let ended = Arc::new(Mutex::new(None));
    let thread_ended = Arc::clone(&ended);
    let thread = std::thread::Builder::new()
        .name(name.into())
        .spawn(move || {
            let mut pipeline = Pipeline::new(settings);
            body(&mut pipeline, inbox, &started);
            *thread_ended.lock().unwrap_or_else(PoisonError::into_inner) = pipeline.ended();
        })
        .map_err(|e| Trouble::failed(format!("no capture thread: {e}")))?;
    let send = move |ask| asks.send(ask).is_ok();
    let share = Share::new(Box::new(send), thread, ended);
    match result.recv() {
        Ok(Ok(())) => Ok(share),
        Ok(Err(trouble)) => Err(trouble),
        Err(_) => Err(Trouble::failed("the capture thread stopped")),
    }
}

/// A capture that is asked for each picture (the X server, `xcap`, the
/// test screen): runs `grab` [`FPS`] times a second until the share is
/// closed, handing each picture to `pipeline` through `put`, and the
/// app's asks to it as they come. Five failures in a row end the share.
pub fn poll<T>(
    pipeline: &mut Pipeline,
    inbox: &Receiver<Ask>,
    mut grab: impl FnMut() -> Result<T, String>,
    put: impl Fn(&mut Pipeline, &T, Instant),
) {
    let every = Duration::from_secs(1) / FPS;
    let mut due = Instant::now();
    let mut failures = 0;
    loop {
        let now = Instant::now();
        if now >= due {
            match grab() {
                Ok(picture) => {
                    failures = 0;
                    put(pipeline, &picture, now);
                }
                Err(why) => {
                    failures += 1;
                    eprintln!("noslacking-video: share: capture: {why}");
                    if failures >= 5 {
                        pipeline.end(Trouble::new(ShareProblem::Ended, why));
                    }
                }
            }
            due += every;
            let now = Instant::now();
            if due < now {
                // Late: count from now rather than race to catch up.
                due = now;
            }
        }
        let until = pipeline.deadline().map_or(due, |at| at.min(due));
        match inbox.recv_timeout(until.saturating_duration_since(Instant::now())) {
            Ok(Ask::Stop) | Err(RecvTimeoutError::Disconnected) => break,
            Ok(ask) => pipeline.ask(ask),
            Err(RecvTimeoutError::Timeout) => {}
        }
        pipeline.tick(Instant::now());
    }
}

/// The test screen: [`pattern::pattern`] at 1920×1080, its colour bars
/// sliding and its clock running, never anyone's screen. With
/// `NOSLACKING_VIDEO_TEST_FRAMES=packed` it comes as BGRx pictures in
/// memory, as PipeWire's shared memory gives them, and with `=dmabuf`
/// (Linux, VA-API) as dma-bufs, as PipeWire's GPU buffers: so the
/// benchmarks run each of the real paths.
pub fn test_screen(settings: Settings) -> Result<Share, Trouble> {
    let kind = std::env::var("NOSLACKING_VIDEO_TEST_FRAMES").unwrap_or_default();
    #[cfg(target_os = "linux")]
    if kind == "dmabuf" {
        return crate::vaapi::synthetic::test_screen(settings);
    }
    spawn(
        "noslacking-share-test",
        settings,
        move |pipeline, inbox, started| {
            let _ = started.send(Ok(()));
            let began = Instant::now();
            let mut n = 0u64;
            if kind == "packed" {
                let pictures = pattern::packed_loop();
                poll(
                    pipeline,
                    &inbox,
                    || {
                        n += 1;
                        Ok(n)
                    },
                    |pipeline, n, at| {
                        let (width, height, data) =
                            &pictures[usize::try_from(*n).unwrap_or(0) % pictures.len()];
                        pipeline.put(
                            &Frame::Packed(Packed {
                                width: *width,
                                height: *height,
                                stride: *width as usize * 4,
                                order: Order::Bgra,
                                data,
                            }),
                            at,
                        );
                    },
                );
                return;
            }
            poll(
                pipeline,
                &inbox,
                || {
                    let picture = pattern::pattern(1920, 1080, n, began.elapsed());
                    n += 1;
                    Ok(picture)
                },
                |pipeline, picture, at| pipeline.put(&Frame::I420(picture), at),
            );
        },
    )
}

/// The system's way to capture the screen, chosen when made.
pub enum System {
    /// The ScreenCast portal and PipeWire.
    #[cfg(all(target_os = "linux", feature = "pipewire"))]
    Portal,
    /// The X server.
    #[cfg(target_os = "linux")]
    X11,
    /// `xcap`.
    #[cfg(any(target_os = "macos", windows))]
    Xcap,
    /// Nothing that can capture the screen here (a Wayland session with
    /// a helper built without PipeWire, or another system); only the
    /// test screen.
    None,
}

impl std::fmt::Debug for System {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.name())
    }
}

impl System {
    /// The way to capture here. On Linux: the portal under Wayland or in
    /// a Flatpak (or when `NOSLACKING_SHARE_CAPTURE=portal`), else the X
    /// server if there is one (or `=x11`).
    pub fn new() -> Self {
        #[cfg(target_os = "linux")]
        {
            let asked = std::env::var("NOSLACKING_SHARE_CAPTURE").unwrap_or_default();
            let wayland = std::env::var_os("WAYLAND_DISPLAY").is_some_and(|d| !d.is_empty());
            let flatpak = std::path::Path::new("/.flatpak-info").exists();
            let x11 = std::env::var_os("DISPLAY").is_some_and(|d| !d.is_empty());
            let portal = asked == "portal" || (asked != "x11" && (wayland || flatpak || !x11));
            if !portal {
                return Self::X11;
            }
            #[cfg(feature = "pipewire")]
            return Self::Portal;
            #[cfg(not(feature = "pipewire"))]
            return Self::None;
        }
        #[cfg(any(target_os = "macos", windows))]
        {
            Self::Xcap
        }
        #[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
        {
            Self::None
        }
    }

    /// Its name, for the log.
    pub fn name(&self) -> &'static str {
        match self {
            #[cfg(all(target_os = "linux", feature = "pipewire"))]
            Self::Portal => "the ScreenCast portal and PipeWire",
            #[cfg(target_os = "linux")]
            Self::X11 => "the X server",
            #[cfg(any(target_os = "macos", windows))]
            Self::Xcap => "xcap",
            Self::None => "nothing",
        }
    }
}

impl Default for System {
    fn default() -> Self {
        Self::new()
    }
}

impl Screens for System {
    fn sources(&mut self) -> Result<(bool, Vec<Source>), Trouble> {
        match self {
            #[cfg(all(target_os = "linux", feature = "pipewire"))]
            Self::Portal => Ok((true, Vec::new())),
            #[cfg(target_os = "linux")]
            Self::X11 => x11::sources().map(|s| (false, s)),
            #[cfg(any(target_os = "macos", windows))]
            Self::Xcap => xcap_capture::sources().map(|s| (false, s)),
            Self::None => Err(Trouble::new(
                ShareProblem::Unavailable,
                "this helper cannot capture the screen here (built without PipeWire?)",
            )),
        }
    }

    fn start(
        &mut self,
        choice: &ShareChoice,
        settings: Settings,
        restore: &str,
    ) -> Result<(Share, String), Trouble> {
        if *choice == ShareChoice::Test {
            return test_screen(settings).map(|share| (share, String::new()));
        }
        match self {
            #[cfg(all(target_os = "linux", feature = "pipewire"))]
            Self::Portal => portal::start(choice, settings, restore),
            #[cfg(target_os = "linux")]
            Self::X11 => x11::start(choice, settings).map(|share| (share, String::new())),
            #[cfg(any(target_os = "macos", windows))]
            Self::Xcap => xcap_capture::start(choice, settings).map(|share| (share, String::new())),
            Self::None => {
                let _ = restore;
                Err(Trouble::new(
                    ShareProblem::Unavailable,
                    "this helper cannot capture the screen here (built without PipeWire?)",
                ))
            }
        }
    }
}

/// Only the test screen, and a list of pretend sources: what tests (and
/// the app's tests, through the helper's server on a thread) share.
#[derive(Clone, Debug, Default)]
pub struct Pretend {
    /// What [`Screens::sources`] gives; empty means the system's dialog.
    pub sources: Vec<Source>,
}

impl Screens for Pretend {
    fn sources(&mut self) -> Result<(bool, Vec<Source>), Trouble> {
        Ok((self.sources.is_empty(), self.sources.clone()))
    }

    fn start(
        &mut self,
        choice: &ShareChoice,
        settings: Settings,
        _restore: &str,
    ) -> Result<(Share, String), Trouble> {
        match choice {
            ShareChoice::Source(id) if !self.sources.iter().any(|s| &s.id == id) => Err(
                Trouble::new(ShareProblem::Gone, format!("no source {id:?}")),
            ),
            // Whatever is chosen, the test screen is what it shows.
            _ => test_screen(settings).map(|share| (share, "pretend-token".into())),
        }
    }
}
