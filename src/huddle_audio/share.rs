//! Sharing your screen in a huddle (the `huddle-share` feature): what is
//! captured, and the rule that it is captured only while you share.
//!
//! The rule, the microphone's and the camera's: nothing is captured until
//! you choose to share, and capture stops (its thread joined, the portal
//! session closed) when you stop, leave, the share's session fails, or
//! the system ends it (the compositor's own "stop sharing"). Joining
//! never shares. [`ShareControl`] is that rule over any [`Capturer`], so
//! tests hold it to it with a pretend screen.
//!
//! Where the pictures come from ([`System`], chosen at run time):
//! - Linux under Wayland (and in the Flatpak): the ScreenCast portal over
//!   D-Bus (`ashpd`, on the `zbus` the app already has); the user picks
//!   a screen or a window in the portal's own dialog, and the frames come
//!   through PipeWire (the `pipewire` crate, bindings to the system's
//!   libpipewire). The portal remembers the choice for the app's run
//!   (its restore token), so sharing again does not ask again unless the
//!   user asks to choose something else.
//! - Linux under X11 without a portal: the X server itself (`x11rb`,
//!   pure Rust), whole screens only, from a picker in the call bar.
//! - macOS and Windows: `xcap` (ScreenCaptureKit/CoreGraphics through
//!   `objc2` on macOS, GDI and DXGI through the `windows` crate), screens
//!   and windows from a picker.
//! - [`TestShare`]: a generated 1080p picture with a moving clock, for the
//!   probe and the demo, never anyone's screen.
//!
//! Frames arrive in a [`Latest`] as they are captured ([`Picture`]); the
//! share's encoder ([`super::share_send`]) takes the newest, shrinks it
//! to at most 1920×1080 keeping its shape, and skips pictures that did
//! not change.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use super::camera::{self, I420, Latest};
use super::microphone::Running;

#[cfg(target_os = "linux")]
mod portal;
#[cfg(target_os = "linux")]
mod x11;
#[cfg(any(target_os = "macos", windows))]
mod xcap_capture;

/// The largest picture a share sends: 1080p, as Slack's own shares do.
pub const MAX_SIZE: (usize, usize) = (1920, 1080);
/// The smaller size a share falls back to when encoding 1080p does not
/// keep up on this machine.
pub const REDUCED_SIZE: (usize, usize) = (1280, 720);
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

/// What kind of thing can be shared.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SourceKind {
    /// A whole screen.
    Screen,
    /// One window.
    Window,
}

/// A screen or window the picker offers.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Source {
    /// How the capturer finds it again.
    pub id: String,
    /// What the picker calls it: the screen's or the window's name.
    pub name: String,
    /// A screen or a window.
    pub kind: SourceKind,
}

/// What to share.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Choice {
    /// Whatever the system's own dialog lets the user pick (the portal);
    /// `again` asks it to ask rather than share what was picked last.
    System {
        /// Choose afresh.
        again: bool,
    },
    /// One of the picker's sources, by its id.
    Source(String),
}

/// Why sharing did not start, or stopped.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ShareError {
    /// The user closed the system's dialog without choosing.
    Cancelled,
    /// The system did not let the app capture the screen (macOS's Screen
    /// Recording permission, a portal that refused).
    Denied,
    /// Nothing here can capture the screen: no portal, no X server.
    Unavailable,
    /// The source chosen is gone (a window closed, a screen unplugged).
    Gone,
    /// It failed; why, for the log.
    Failed(String),
}

impl std::fmt::Display for ShareError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Cancelled => f.write_str("the choice was cancelled"),
            Self::Denied => f.write_str("the system did not allow screen capture"),
            Self::Unavailable => f.write_str("no way to capture the screen here"),
            Self::Gone => f.write_str("what was chosen is gone"),
            Self::Failed(why) => write!(f, "screen capture failed: {why}"),
        }
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

/// A packed picture of four bytes a pixel.
#[derive(Clone, PartialEq, Eq)]
pub struct Packed {
    /// Width in pixels.
    pub width: usize,
    /// Height in pixels.
    pub height: usize,
    /// Bytes from one row to the next, at least `width * 4`.
    pub stride: usize,
    /// The byte order.
    pub order: Order,
    /// The pixels.
    pub data: Vec<u8>,
}

impl std::fmt::Debug for Packed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Packed({}x{} {:?})", self.width, self.height, self.order)
    }
}

impl Packed {
    /// Whether the buffer holds every row the size and stride say.
    pub fn whole(&self) -> bool {
        self.width > 0
            && self.height > 0
            && self.stride >= self.width * 4
            && self.data.len() >= self.stride * (self.height - 1) + self.width * 4
    }

    /// Each row's pixels, without the padding after them.
    fn rows(&self) -> impl Iterator<Item = &[u8]> {
        let width = self.width * 4;
        self.data
            .chunks(self.stride.max(1))
            .take(self.height)
            .map(move |row| &row[..width.min(row.len())])
    }
}

/// A captured picture, as its source gives it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Picture {
    /// Four bytes a pixel, from a screen.
    Packed(Packed),
    /// Already I420 (the test picture).
    I420(I420),
}

impl Picture {
    /// Its width and height.
    pub fn size(&self) -> (usize, usize) {
        match self {
            Self::Packed(p) => (p.width, p.height),
            Self::I420(p) => (p.width, p.height),
        }
    }

    /// The picture as sent: I420, shrunk to fit `max` keeping its shape,
    /// never enlarged. `None` for a picture too small or broken.
    pub fn to_send(&self, max: (usize, usize)) -> Option<I420> {
        let (width, height) = self.size();
        let (w, h) = camera::fit(width, height, max)?;
        let full = match self {
            Self::Packed(packed) => to_i420(packed)?,
            Self::I420(picture) if picture.whole() => picture.clone(),
            Self::I420(_) => return None,
        };
        Some(camera::scale(&full, w, h))
    }
}

/// Whether two pictures are the same, pixel for pixel (padding aside): a
/// still screen sends nothing new.
pub fn unchanged(a: &Picture, b: &Picture) -> bool {
    match (a, b) {
        (Picture::Packed(a), Picture::Packed(b)) => {
            (a.width, a.height, a.order) == (b.width, b.height, b.order) && a.rows().eq(b.rows())
        }
        (Picture::I420(a), Picture::I420(b)) => a == b,
        _ => false,
    }
}

/// A packed picture as I420, BT.601 studio range, at its own size (an odd
/// size loses its last row or column).
pub fn to_i420(packed: &Packed) -> Option<I420> {
    if !packed.whole() {
        return None;
    }
    // The converters read whole rows of `stride`: pad a last row that the
    // producer cut short after its pixels.
    let full = packed.stride * packed.height;
    let data: std::borrow::Cow<'_, [u8]> = if packed.data.len() >= full {
        std::borrow::Cow::Borrowed(&packed.data[..full])
    } else {
        let mut padded = packed.data.clone();
        padded.resize(full, 0);
        std::borrow::Cow::Owned(padded)
    };
    match packed.order {
        Order::Bgra => {
            super::video_encoder::from_bgra(&data, packed.width, packed.height, packed.stride)
        }
        Order::Rgba => from_rgba(&data, packed.width, packed.height, packed.stride),
    }
}

/// RGBA with rows `stride` apart as I420, as [`super::video_encoder::from_bgra`].
fn from_rgba(data: &[u8], width: usize, height: usize, stride: usize) -> Option<I420> {
    let (w, h) = (width & !1, height & !1);
    if w == 0 || h == 0 || stride < width * 4 || data.len() < stride * h {
        return None;
    }
    let mut out = I420::black(w, h);
    let size = |n: usize| u32::try_from(n).ok();
    let mut planar = yuv::YuvPlanarImageMut {
        y_plane: yuv::BufferStoreMut::Borrowed(&mut out.y),
        y_stride: size(w)?,
        u_plane: yuv::BufferStoreMut::Borrowed(&mut out.u),
        u_stride: size(w / 2)?,
        v_plane: yuv::BufferStoreMut::Borrowed(&mut out.v),
        v_stride: size(w / 2)?,
        width: size(w)?,
        height: size(h)?,
    };
    yuv::rgba_to_yuv420(
        &mut planar,
        &data[..stride * h],
        size(stride)?,
        yuv::YuvRange::Limited,
        yuv::YuvStandardMatrix::Bt601,
        yuv::YuvConversionMode::Balanced,
    )
    .ok()?;
    Some(out)
}

/// A captured frame: its picture and when it was taken.
#[derive(Clone, Debug)]
pub struct ShareFrame {
    /// The picture, as captured.
    pub picture: Picture,
    /// When it was captured, for its RTP time.
    pub at: Instant,
}

/// Where a share's frames wait for its encoder.
pub type Frames = Latest<ShareFrame>;

/// Told by a capture that ends by itself: the window closed, the
/// compositor's own "stop sharing" was pressed, PipeWire went away.
#[derive(Clone, Debug)]
pub struct Ended {
    tell: Arc<tokio::sync::watch::Sender<bool>>,
}

impl Ended {
    /// A new one, and what hears it.
    pub fn new() -> (Self, tokio::sync::watch::Receiver<bool>) {
        let (tell, heard) = tokio::sync::watch::channel(false);
        (
            Self {
                tell: Arc::new(tell),
            },
            heard,
        )
    }

    /// The capture ended; `why` goes to the log.
    pub fn end(&self, why: &str) {
        log::info!("huddle share: capture ended: {why}");
        self.tell.send_replace(true);
    }
}

/// A capture running: dropping it stops it (its thread stopped and
/// joined, then whatever else it holds let go, such as the portal's
/// session).
pub struct Capturing {
    running: Option<Running>,
    release: Option<Box<dyn FnOnce() + Send>>,
}

impl std::fmt::Debug for Capturing {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Capturing").finish_non_exhaustive()
    }
}

impl Capturing {
    /// A capture thread, and what to let go of after it stopped.
    pub fn new(running: Running, release: Option<Box<dyn FnOnce() + Send>>) -> Self {
        Self {
            running: Some(running),
            release,
        }
    }
}

impl Drop for Capturing {
    fn drop(&mut self) {
        drop(self.running.take());
        if let Some(release) = self.release.take() {
            release();
        }
    }
}

/// Something that can capture a screen or window.
pub trait Capturer: Send + 'static {
    /// What the picker offers; empty when the system asks itself (the
    /// portal's dialog), so there is nothing to pick here.
    fn sources(&mut self) -> Result<Vec<Source>, ShareError>;
    /// Starts capturing what `choice` says into the frames it was made
    /// with, telling `ended` if the capture ends by itself.
    fn start(&mut self, choice: &Choice, ended: Ended) -> Result<Capturing, ShareError>;
}

/// The share as the Share button sees it: nothing captured until asked.
pub struct ShareControl<C: Capturer> {
    capturer: C,
    capturing: Option<Capturing>,
    ended: Option<tokio::sync::watch::Receiver<bool>>,
}

impl<C: Capturer> std::fmt::Debug for ShareControl<C> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ShareControl")
            .field("capturing", &self.is_capturing())
            .finish_non_exhaustive()
    }
}

impl<C: Capturer> ShareControl<C> {
    /// Not sharing, with `capturer` idle.
    pub fn new(capturer: C) -> Self {
        Self {
            capturer,
            capturing: None,
            ended: None,
        }
    }

    /// Whether a capture is running.
    pub fn is_capturing(&self) -> bool {
        self.capturing.is_some()
    }

    /// What the picker offers (see [`Capturer::sources`]).
    pub fn sources(&mut self) -> Result<Vec<Source>, ShareError> {
        self.capturer.sources()
    }

    /// Starts capturing `choice`, stopping any capture first. A failed
    /// start leaves nothing captured.
    pub fn start(&mut self, choice: &Choice) -> Result<(), ShareError> {
        self.stop();
        let (ended, heard) = Ended::new();
        self.capturing = Some(self.capturer.start(choice, ended)?);
        self.ended = Some(heard);
        Ok(())
    }

    /// Stops capturing, if it was.
    pub fn stop(&mut self) {
        self.capturing = None;
        self.ended = None;
    }

    /// What hears the capture end by itself, while one runs.
    pub fn ended(&self) -> Option<tokio::sync::watch::Receiver<bool>> {
        self.ended.clone()
    }
}

/// Starts a capture thread named `name` running `body` until it is told
/// to stop, once it has said through its sender whether it started.
pub fn spawn_capture(
    name: &str,
    body: impl FnOnce(&AtomicBool, &std::sync::mpsc::Sender<Result<(), ShareError>>) + Send + 'static,
) -> Result<Running, ShareError> {
    let stop = Arc::new(AtomicBool::new(false));
    let (started, result) = std::sync::mpsc::channel();
    let thread_stop = stop.clone();
    let thread = std::thread::Builder::new()
        .name(name.into())
        .spawn(move || body(&thread_stop, &started))
        .map_err(|e| ShareError::Failed(format!("no thread: {e}")))?;
    let running = Running::new(stop, thread);
    match result.recv() {
        Ok(Ok(())) => Ok(running),
        Ok(Err(error)) => Err(error),
        Err(_) => Err(ShareError::Failed("the capture thread stopped".into())),
    }
}

/// Runs `capture` every [`FPS`]th of a second until `stop`, putting what
/// it gives in `frames`; a capture that fails five times in a row ends
/// it. For the X server and `xcap`, which are asked for each picture.
pub fn poll_frames(
    stop: &AtomicBool,
    frames: &Frames,
    ended: &Ended,
    mut capture: impl FnMut() -> Result<Picture, String>,
) {
    let every = Duration::from_secs(1) / FPS;
    let mut due = Instant::now();
    let mut failures = 0;
    while !stop.load(Ordering::Relaxed) {
        let at = Instant::now();
        match capture() {
            Ok(picture) => {
                failures = 0;
                frames.put(ShareFrame { picture, at });
            }
            Err(why) => {
                failures += 1;
                log::debug!("huddle share: capture: {why}");
                if failures >= 5 {
                    ended.end(&why);
                    break;
                }
            }
        }
        due += every;
        let now = Instant::now();
        if due < now {
            // Late: start counting from now rather than racing to catch up.
            due = now;
        }
        std::thread::park_timeout(due.saturating_duration_since(now));
    }
    frames.clear();
}

/// The probe's and the demo's screen: the test picture at 1920×1080 and
/// [`FPS`], its colour bars sliding and its clock running, never anyone's
/// screen.
#[derive(Clone, Debug)]
pub struct TestShare {
    frames: Frames,
}

impl TestShare {
    /// The test screen, its frames going to `frames`.
    pub fn new(frames: Frames) -> Self {
        Self { frames }
    }
}

impl Capturer for TestShare {
    fn sources(&mut self) -> Result<Vec<Source>, ShareError> {
        Ok(Vec::new())
    }

    fn start(&mut self, _choice: &Choice, ended: Ended) -> Result<Capturing, ShareError> {
        let frames = self.frames.clone();
        let running = spawn_capture("noslacking-test-share", move |stop, started| {
            let _ = started.send(Ok(()));
            let began = Instant::now();
            let mut n = 0u64;
            poll_frames(stop, &frames, &ended, || {
                let picture = camera::pattern(
                    MAX_SIZE.0,
                    MAX_SIZE.1,
                    n,
                    Instant::now().duration_since(began),
                );
                n += 1;
                Ok(Picture::I420(picture))
            });
        })?;
        Ok(Capturing::new(running, None))
    }
}

/// The system's way to capture the screen, chosen when made.
pub enum System {
    /// The ScreenCast portal and PipeWire.
    #[cfg(target_os = "linux")]
    Portal(portal::Portal),
    /// The X server.
    #[cfg(target_os = "linux")]
    X11(x11::X11),
    /// `xcap`.
    #[cfg(any(target_os = "macos", windows))]
    Xcap(xcap_capture::Xcap),
    /// Nothing that can capture the screen here.
    None,
}

impl std::fmt::Debug for System {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.name())
    }
}

impl System {
    /// The way to capture here, its frames going to `frames`. On Linux:
    /// the portal under Wayland or in a Flatpak (or when
    /// `NOSLACKING_SHARE_CAPTURE=portal`), else the X server if there is
    /// one (or `=x11`).
    pub fn new(frames: Frames) -> Self {
        #[cfg(target_os = "linux")]
        {
            let asked = std::env::var("NOSLACKING_SHARE_CAPTURE").unwrap_or_default();
            let wayland = std::env::var_os("WAYLAND_DISPLAY").is_some_and(|d| !d.is_empty());
            let flatpak = std::path::Path::new("/.flatpak-info").exists();
            let x11 = std::env::var_os("DISPLAY").is_some_and(|d| !d.is_empty());
            match asked.as_str() {
                "portal" => return Self::Portal(portal::Portal::new(frames)),
                "x11" => return Self::X11(x11::X11::new(frames)),
                _ => {}
            }
            if wayland || flatpak || !x11 {
                return Self::Portal(portal::Portal::new(frames));
            }
            Self::X11(x11::X11::new(frames))
        }
        #[cfg(any(target_os = "macos", windows))]
        {
            Self::Xcap(xcap_capture::Xcap::new(frames))
        }
        #[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
        {
            let _ = frames;
            Self::None
        }
    }

    /// Its name, for the log.
    pub fn name(&self) -> &'static str {
        match self {
            #[cfg(target_os = "linux")]
            Self::Portal(_) => "the ScreenCast portal and PipeWire",
            #[cfg(target_os = "linux")]
            Self::X11(_) => "the X server",
            #[cfg(any(target_os = "macos", windows))]
            Self::Xcap(_) => "xcap",
            Self::None => "nothing",
        }
    }
}

impl Capturer for System {
    fn sources(&mut self) -> Result<Vec<Source>, ShareError> {
        match self {
            #[cfg(target_os = "linux")]
            Self::Portal(_) => Ok(Vec::new()),
            #[cfg(target_os = "linux")]
            Self::X11(x11) => x11.sources(),
            #[cfg(any(target_os = "macos", windows))]
            Self::Xcap(xcap) => xcap.sources(),
            Self::None => Err(ShareError::Unavailable),
        }
    }

    fn start(&mut self, choice: &Choice, ended: Ended) -> Result<Capturing, ShareError> {
        match self {
            #[cfg(target_os = "linux")]
            Self::Portal(portal) => portal.start(choice, ended),
            #[cfg(target_os = "linux")]
            Self::X11(x11) => x11.start(choice, ended),
            #[cfg(any(target_os = "macos", windows))]
            Self::Xcap(xcap) => xcap.start(choice, ended),
            Self::None => {
                let _ = (choice, ended);
                Err(ShareError::Unavailable)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;

    /// A pretend screen that counts how often it is started and how many
    /// captures run, so a test can see nothing is captured unless shared.
    #[derive(Clone, Default)]
    struct Pretend {
        starts: Arc<AtomicUsize>,
        running: Arc<AtomicUsize>,
        refuse: Option<ShareError>,
        sources: Vec<Source>,
        ended: Arc<std::sync::Mutex<Option<Ended>>>,
    }

    impl Capturer for Pretend {
        fn sources(&mut self) -> Result<Vec<Source>, ShareError> {
            Ok(self.sources.clone())
        }

        fn start(&mut self, _choice: &Choice, ended: Ended) -> Result<Capturing, ShareError> {
            self.starts.fetch_add(1, Ordering::Relaxed);
            if let Some(error) = &self.refuse {
                return Err(error.clone());
            }
            *self.ended.lock().expect("a lock") = Some(ended);
            self.running.fetch_add(1, Ordering::Relaxed);
            let running = self.running.clone();
            let thread = spawn_capture("pretend", |stop, started| {
                let _ = started.send(Ok(()));
                while !stop.load(Ordering::Relaxed) {
                    std::thread::park_timeout(Duration::from_millis(50));
                }
            })?;
            Ok(Capturing::new(
                thread,
                Some(Box::new(move || {
                    running.fetch_sub(1, Ordering::Relaxed);
                })),
            ))
        }
    }

    #[test]
    fn nothing_is_captured_unless_sharing_and_every_stop_stops_it() {
        let pretend = Pretend::default();
        let running = pretend.running.clone();
        let mut control = ShareControl::new(pretend.clone());
        // Made, asked what there is: nothing captured.
        assert!(control.sources().expect("sources").is_empty());
        assert!(!control.is_capturing());
        assert_eq!(running.load(Ordering::Relaxed), 0);
        assert_eq!(pretend.starts.load(Ordering::Relaxed), 0);
        // Shared: one capture.
        control
            .start(&Choice::System { again: false })
            .expect("started");
        assert!(control.is_capturing());
        assert_eq!(running.load(Ordering::Relaxed), 1);
        // Stopped: none.
        control.stop();
        assert!(!control.is_capturing());
        assert_eq!(running.load(Ordering::Relaxed), 0);
        // Sharing something else stops the first before the second.
        control.start(&Choice::Source("a".into())).expect("started");
        control.start(&Choice::Source("b".into())).expect("started");
        assert_eq!(running.load(Ordering::Relaxed), 1);
        // Leaving (the control dropped) stops it too.
        drop(control);
        assert_eq!(running.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn a_capture_that_ends_by_itself_is_heard() {
        let pretend = Pretend::default();
        let mut control = ShareControl::new(pretend.clone());
        assert!(control.ended().is_none());
        control
            .start(&Choice::System { again: true })
            .expect("started");
        let heard = control.ended().expect("a receiver");
        assert!(!*heard.borrow());
        pretend
            .ended
            .lock()
            .expect("a lock")
            .as_ref()
            .expect("told")
            .end("the window closed");
        assert!(*heard.borrow());
        control.stop();
        assert!(control.ended().is_none());
    }

    #[test]
    fn a_refused_start_captures_nothing() {
        for error in [
            ShareError::Cancelled,
            ShareError::Denied,
            ShareError::Unavailable,
            ShareError::Gone,
            ShareError::Failed("no".into()),
        ] {
            let pretend = Pretend {
                refuse: Some(error.clone()),
                ..Pretend::default()
            };
            let mut control = ShareControl::new(pretend.clone());
            assert_eq!(
                control.start(&Choice::System { again: false }),
                Err(error.clone())
            );
            assert!(!control.is_capturing());
            assert_eq!(pretend.running.load(Ordering::Relaxed), 0);
            assert!(!error.to_string().is_empty());
        }
    }

    #[test]
    fn two_shares_are_the_most() {
        assert!(may_share(0));
        assert!(may_share(1));
        assert!(!may_share(2));
        assert!(!may_share(3));
    }

    fn packed(width: usize, height: usize, stride: usize, fill: u8) -> Packed {
        Packed {
            width,
            height,
            stride,
            order: Order::Bgra,
            data: vec![fill; stride * height],
        }
    }

    /// Larger screens shrink to fit 1080p keeping their shape; smaller
    /// ones are sent as they are.
    #[test]
    fn screens_are_shrunk_to_1080p_keeping_their_shape() {
        for ((w, h), want) in [
            ((3840, 2160), (1920, 1080)),
            ((2560, 1440), (1920, 1080)),
            ((2560, 1600), (1728, 1080)),
            ((3440, 1440), (1920, 802)),
            ((1080, 1920), (606, 1080)),
            ((1920, 1080), (1920, 1080)),
            ((1366, 768), (1366, 768)),
            ((1365, 767), (1364, 766)),
        ] {
            let picture = Picture::Packed(packed(w, h, w * 4, 128));
            let sent = picture.to_send(MAX_SIZE).expect("a picture");
            assert_eq!((sent.width, sent.height), want, "{w}x{h}");
            assert!(sent.whole());
            assert!(super::super::video_encoder::Limits::SHARE.fits(sent.width, sent.height));
        }
        // The fallback size.
        let picture = Picture::Packed(packed(2560, 1440, 2560 * 4, 0));
        let sent = picture.to_send(REDUCED_SIZE).expect("a picture");
        assert_eq!((sent.width, sent.height), (1280, 720));
        // Too small, or broken: nothing.
        assert!(
            Picture::Packed(packed(8, 8, 32, 0))
                .to_send(MAX_SIZE)
                .is_none()
        );
        let mut short = packed(64, 64, 256, 0);
        short.data.truncate(100);
        assert!(Picture::Packed(short).to_send(MAX_SIZE).is_none());
    }

    #[test]
    fn colours_come_through_in_either_byte_order() {
        // Pure red, in BGRA and in RGBA.
        let mut bgra = packed(32, 32, 32 * 4, 0);
        let mut rgba = Packed {
            order: Order::Rgba,
            ..bgra.clone()
        };
        for pixel in bgra.data.as_chunks_mut::<4>().0 {
            pixel.copy_from_slice(&[0, 0, 255, 255]);
        }
        for pixel in rgba.data.as_chunks_mut::<4>().0 {
            pixel.copy_from_slice(&[255, 0, 0, 255]);
        }
        let a = to_i420(&bgra).expect("converted");
        let b = to_i420(&rgba).expect("converted");
        assert_eq!(a, b);
        // BT.601 studio-range red: Y about 81, V about 240.
        assert!((70..95).contains(&a.y[0]), "{}", a.y[0]);
        assert!(a.v[0] > 220, "{}", a.v[0]);
    }

    #[test]
    fn unchanged_pictures_are_seen_as_such_padding_aside() {
        let a = packed(64, 32, 64 * 4 + 32, 10);
        let mut b = a.clone();
        // Only the padding after the pixels differs: unchanged.
        for row in 0..32 {
            b.data[row * b.stride + 64 * 4] = 99;
        }
        assert!(unchanged(
            &Picture::Packed(a.clone()),
            &Picture::Packed(b.clone())
        ));
        // One pixel differs: changed.
        b.data[5 * b.stride + 7] = 11;
        assert!(!unchanged(&Picture::Packed(a.clone()), &Picture::Packed(b)));
        // Another size, or another kind: changed.
        assert!(!unchanged(
            &Picture::Packed(a.clone()),
            &Picture::Packed(packed(64, 30, 64 * 4 + 32, 10))
        ));
        let still = camera::pattern(64, 32, 1, Duration::ZERO);
        assert!(unchanged(
            &Picture::I420(still.clone()),
            &Picture::I420(still.clone())
        ));
        assert!(!unchanged(&Picture::I420(still), &Picture::Packed(a)));
    }

    #[test]
    fn the_test_screen_is_1080p_and_stops() {
        let frames = Frames::default();
        let mut control = ShareControl::new(TestShare::new(frames.clone()));
        assert!(control.sources().expect("sources").is_empty(), "no picker");
        control
            .start(&Choice::System { again: false })
            .expect("started");
        let frame = frames.take(Duration::from_secs(2)).expect("a frame");
        assert_eq!(frame.picture.size(), MAX_SIZE);
        control.stop();
        // Stopped: nothing more comes.
        frames.clear();
        assert!(frames.take(Duration::from_millis(200)).is_none());
    }
}
