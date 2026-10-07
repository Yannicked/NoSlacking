//! The camera, opened only while it is on (the `huddle-camera` feature).
//!
//! The rule this module keeps, the microphone's: the device is not
//! opened until the user turns the camera on, and is closed (its stream
//! stopped and dropped, its thread joined, so its light goes out) when
//! they turn it off or leave. Joining never turns it on.
//! [`CameraControl`] is that rule as a state machine over any [`Camera`],
//! so tests hold it to it with a pretend device.
//!
//! The real one, [`Nokhwa`], opens the first camera through `nokhwa`
//! (V4L2 on Linux, AVFoundation on macOS, Media Foundation on Windows)
//! on a thread of its own, asking for 640×480 at 30 frames a second in a
//! raw format if it has one (YUYV or NV12), MJPEG otherwise. Each frame
//! becomes I420 ([`I420`]), shrunk to fit 640×480 if the camera gave
//! more, and waits in a [`Latest`] for the encoder: a frame the encoder
//! has not taken yet is replaced, never queued.
//!
//! [`TestPattern`] is the probe's and the demo's camera: moving colour
//! bars, a bouncing square and a clock, made in the same thread shape,
//! so Slack can be checked to show our video without anyone's camera.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, PoisonError};
use std::time::{Duration, Instant};

use super::microphone::Running;

/// The largest picture sent: what Slack's own clients send for a few
/// cameras (480×480 to 640×480 was seen), within level 3.1.
pub const MAX_WIDTH: usize = 640;
/// The largest picture's height.
pub const MAX_HEIGHT: usize = 480;
/// Frames a second asked of the camera, and sent.
pub const FPS: u32 = 30;

/// A picture in I420: a full-size luma plane and two chroma planes of
/// half its width and height. Always of even size, as H.264's 4:2:0
/// wants.
#[derive(Clone, PartialEq, Eq)]
pub struct I420 {
    /// Its width in pixels, even.
    pub width: usize,
    /// Its height in pixels, even.
    pub height: usize,
    /// Luma, `width * height` bytes.
    pub y: Vec<u8>,
    /// Blue difference, `width / 2 * height / 2` bytes.
    pub u: Vec<u8>,
    /// Red difference, as many.
    pub v: Vec<u8>,
}

impl std::fmt::Debug for I420 {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "I420({}x{})", self.width, self.height)
    }
}

impl I420 {
    /// A black picture of `width`×`height`, rounded down to even sizes.
    pub fn black(width: usize, height: usize) -> Self {
        let (width, height) = (width & !1, height & !1);
        let chroma = (width / 2) * (height / 2);
        Self {
            width,
            height,
            y: vec![16; width * height],
            u: vec![128; chroma],
            v: vec![128; chroma],
        }
    }

    /// Whether the size is even and not empty, and the planes are as
    /// long as it says.
    pub fn whole(&self) -> bool {
        let chroma = (self.width / 2) * (self.height / 2);
        self.width > 0
            && self.height > 0
            && self.width.is_multiple_of(2)
            && self.height.is_multiple_of(2)
            && self.y.len() == self.width * self.height
            && self.u.len() == chroma
            && self.v.len() == chroma
    }
}

/// The size a `width`×`height` picture is sent at: shrunk, keeping its
/// shape, to fit `max`, never enlarged, and even both ways. `None` for a
/// picture too small to send (under 16 pixels a side).
pub fn fit(width: usize, height: usize, max: (usize, usize)) -> Option<(usize, usize)> {
    // An odd size loses its last row or column first, as the conversions
    // do.
    let (width, height) = (width & !1, height & !1);
    if width < 16 || height < 16 {
        return None;
    }
    let (max_w, max_h) = (max.0.max(16), max.1.max(16));
    // The smaller of the two scales, as a fraction, so no float rounds
    // a side past the limit.
    let (w, h) = if width * max_h > height * max_w {
        // Wider than the box: its width decides.
        let w = width.min(max_w);
        (w, height * w / width)
    } else {
        let h = height.min(max_h);
        (width * h / height, h)
    };
    let (w, h) = (w & !1, h & !1);
    (w >= 16 && h >= 16).then_some((w, h))
}

/// `picture` shrunk to `width`×`height` (even, no larger than it), each
/// pixel the average of the block of the source it covers. The picture
/// itself when it already has that size.
pub fn scale(picture: &I420, width: usize, height: usize) -> I420 {
    let (width, height) = (
        (width & !1).clamp(2, picture.width.max(2)),
        (height & !1).clamp(2, picture.height.max(2)),
    );
    if (width, height) == (picture.width, picture.height) || !picture.whole() {
        return picture.clone();
    }
    let (cw, ch) = (picture.width / 2, picture.height / 2);
    I420 {
        width,
        height,
        y: area(&picture.y, picture.width, picture.height, width, height),
        u: area(&picture.u, cw, ch, width / 2, height / 2),
        v: area(&picture.v, cw, ch, width / 2, height / 2),
    }
}

/// One plane averaged down: each output pixel covers the source pixels
/// from its left/top edge to the next one's, at least one.
fn area(plane: &[u8], width: usize, height: usize, out_w: usize, out_h: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(out_w * out_h);
    // The source columns each output column starts at, once.
    let columns: Vec<(usize, usize)> = (0..out_w)
        .map(|x| {
            let from = x * width / out_w;
            let to = ((x + 1) * width / out_w).max(from + 1).min(width);
            (from, to)
        })
        .collect();
    let mut sums = vec![0u32; out_w];
    for row in 0..out_h {
        let top = row * height / out_h;
        let bottom = ((row + 1) * height / out_h).max(top + 1).min(height);
        sums.fill(0);
        for line in plane.chunks_exact(width).skip(top).take(bottom - top) {
            for (sum, &(from, to)) in sums.iter_mut().zip(&columns) {
                *sum += line[from..to].iter().map(|&p| u32::from(p)).sum::<u32>();
            }
        }
        let rows = u32::try_from(bottom - top).unwrap_or(1);
        for (sum, &(from, to)) in sums.iter().zip(&columns) {
            let count = (rows * u32::try_from(to - from).unwrap_or(1)).max(1);
            out.push(u8::try_from((sum + count / 2) / count).unwrap_or(u8::MAX));
        }
    }
    out
}

/// A YUYV (YUY2) frame as I420: luma as is, chroma from each pair of
/// rows averaged. `None` when the buffer is shorter than the size says;
/// an odd size loses its last row or column.
pub fn from_yuyv(data: &[u8], width: usize, height: usize) -> Option<I420> {
    let (w, h) = (width & !1, height & !1);
    if w == 0 || h == 0 || data.len() < width * height * 2 {
        return None;
    }
    let stride = width * 2;
    let mut out = I420::black(w, h);
    for row in 0..h {
        let line = &data[row * stride..row * stride + w * 2];
        for (x, px) in line.as_chunks::<2>().0.iter().enumerate() {
            out.y[row * w + x] = px[0];
        }
    }
    for row in 0..h / 2 {
        let a = &data[2 * row * stride..2 * row * stride + w * 2];
        let b = &data[(2 * row + 1) * stride..(2 * row + 1) * stride + w * 2];
        for (x, (pa, pb)) in a
            .as_chunks::<4>()
            .0
            .iter()
            .zip(b.as_chunks::<4>().0)
            .enumerate()
        {
            let at = row * (w / 2) + x;
            out.u[at] = avg2(pa[1], pb[1]);
            out.v[at] = avg2(pa[3], pb[3]);
        }
    }
    Some(out)
}

/// An NV12 frame (luma, then interleaved blue and red) as I420.
pub fn from_nv12(data: &[u8], width: usize, height: usize) -> Option<I420> {
    let (w, h) = (width & !1, height & !1);
    let chroma_rows = height.div_ceil(2);
    let chroma_stride = width.div_ceil(2) * 2;
    if w == 0 || h == 0 || data.len() < width * height + chroma_stride * chroma_rows {
        return None;
    }
    let mut out = I420::black(w, h);
    for row in 0..h {
        out.y[row * w..(row + 1) * w].copy_from_slice(&data[row * width..row * width + w]);
    }
    let chroma = &data[width * height..];
    for row in 0..h / 2 {
        let line = &chroma[row * chroma_stride..row * chroma_stride + w];
        for (x, pair) in line.as_chunks::<2>().0.iter().enumerate() {
            out.u[row * (w / 2) + x] = pair[0];
            out.v[row * (w / 2) + x] = pair[1];
        }
    }
    Some(out)
}

/// Packed RGB (3 bytes a pixel) as I420, BT.601 studio range, as WebRTC
/// senders send it.
pub fn from_rgb(data: &[u8], width: usize, height: usize) -> Option<I420> {
    let (w, h) = (width & !1, height & !1);
    if w == 0 || h == 0 || data.len() < width * height * 3 {
        return None;
    }
    // The even part, tightly packed, for the converter.
    let rgb: std::borrow::Cow<'_, [u8]> = if (w, h) == (width, height) {
        std::borrow::Cow::Borrowed(&data[..width * height * 3])
    } else {
        std::borrow::Cow::Owned(
            data.chunks_exact(width * 3)
                .take(h)
                .flat_map(|line| &line[..w * 3])
                .copied()
                .collect(),
        )
    };
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
    yuv::rgb_to_yuv420(
        &mut planar,
        &rgb,
        size(w * 3)?,
        yuv::YuvRange::Limited,
        yuv::YuvStandardMatrix::Bt601,
        yuv::YuvConversionMode::Balanced,
    )
    .ok()?;
    Some(out)
}

/// A grey frame (one byte a pixel) as I420, without colour.
pub fn from_gray(data: &[u8], width: usize, height: usize) -> Option<I420> {
    let (w, h) = (width & !1, height & !1);
    if w == 0 || h == 0 || data.len() < width * height {
        return None;
    }
    let mut out = I420::black(w, h);
    for row in 0..h {
        for x in 0..w {
            // Full range to studio range.
            let p = u32::from(data[row * width + x]);
            out.y[row * w + x] = u8::try_from(16 + p * 219 / 255).unwrap_or(235);
        }
    }
    Some(out)
}

/// An MJPEG frame (one JPEG) as I420, decoded by `image`'s pure-Rust
/// JPEG decoder.
pub fn from_mjpeg(data: &[u8]) -> Option<I420> {
    let image = image::load_from_memory_with_format(data, image::ImageFormat::Jpeg).ok()?;
    let rgb = image.to_rgb8();
    let (width, height) = (
        usize::try_from(rgb.width()).ok()?,
        usize::try_from(rgb.height()).ok()?,
    );
    from_rgb(rgb.as_raw(), width, height)
}

fn avg2(a: u8, b: u8) -> u8 {
    u8::try_from((u16::from(a) + u16::from(b)).div_ceil(2)).unwrap_or(u8::MAX)
}

/// A captured frame: its picture and when it was taken.
#[derive(Clone, Debug)]
pub struct Frame {
    /// The picture, at the size it is sent.
    pub picture: I420,
    /// When it was captured, for its RTP time.
    pub at: Instant,
}

/// The newest frame from the camera, waiting for the encoder: put by the
/// capture thread, taken by the encoder's. A frame not taken in time is
/// replaced by the next, never queued, so a slow encoder sends fewer
/// frames rather than late ones.
#[derive(Clone, Default)]
pub struct Latest {
    shared: Arc<LatestShared>,
}

#[derive(Default)]
struct LatestShared {
    slot: Mutex<Option<Frame>>,
    ready: Condvar,
    /// Frames put, and frames replaced before they were taken.
    put: AtomicU64,
    replaced: AtomicU64,
}

impl std::fmt::Debug for Latest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Latest")
            .field("put", &self.put_count())
            .field("replaced", &self.replaced())
            .finish()
    }
}

impl Latest {
    fn slot(&self) -> std::sync::MutexGuard<'_, Option<Frame>> {
        self.shared
            .slot
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    /// Puts `frame` in place of one not taken yet.
    pub fn put(&self, frame: Frame) {
        if self.slot().replace(frame).is_some() {
            self.shared.replaced.fetch_add(1, Ordering::Relaxed);
        }
        self.shared.put.fetch_add(1, Ordering::Relaxed);
        self.shared.ready.notify_one();
    }

    /// The newest frame, waiting up to `wait` for one.
    pub fn take(&self, wait: Duration) -> Option<Frame> {
        let slot = self.slot();
        let (mut slot, _) = self
            .shared
            .ready
            .wait_timeout_while(slot, wait, |slot| slot.is_none())
            .unwrap_or_else(PoisonError::into_inner);
        slot.take()
    }

    /// Forgets a frame not taken: the camera went off.
    pub fn clear(&self) {
        self.slot().take();
    }

    /// Frames put since the start.
    pub fn put_count(&self) -> u64 {
        self.shared.put.load(Ordering::Relaxed)
    }

    /// Frames replaced before the encoder took them.
    pub fn replaced(&self) -> u64 {
        self.shared.replaced.load(Ordering::Relaxed)
    }
}

/// Why the camera did not open.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CameraError {
    /// There is no camera.
    NoDevice,
    /// The system did not let the app use it (macOS's camera permission,
    /// Windows' privacy settings).
    Denied,
    /// It would not open: in use by another app, or no format it can
    /// give was usable.
    Open(String),
}

impl std::fmt::Display for CameraError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoDevice => f.write_str("no camera"),
            Self::Denied => f.write_str("the system did not allow the camera"),
            Self::Open(why) => write!(f, "the camera would not open: {why}"),
        }
    }
}

/// Something that can be opened to capture pictures. What `open` returns
/// holds the device; dropping it closes it.
pub trait Camera: Send + 'static {
    /// The open device.
    type Open: Send + 'static;
    /// Opens the device and starts capturing.
    fn open(&mut self) -> Result<Self::Open, CameraError>;
}

/// The camera as the on/off button sees it: closed until turned on.
#[derive(Debug)]
pub struct CameraControl<C: Camera> {
    camera: C,
    open: Option<C::Open>,
}

impl<C: Camera> CameraControl<C> {
    /// Off, with `camera` closed.
    pub fn new(camera: C) -> Self {
        Self { camera, open: None }
    }

    /// Whether the device is open.
    pub fn is_open(&self) -> bool {
        self.open.is_some()
    }

    /// Turns the camera on (opening the device) or off (closing it). A
    /// failed open leaves it off.
    pub fn set_on(&mut self, on: bool) -> Result<(), CameraError> {
        if !on {
            self.open = None;
        } else if self.open.is_none() {
            self.open = Some(self.camera.open()?);
        }
        Ok(())
    }
}

/// A capture thread that stops, and is joined, when dropped.
pub type Capturing = Running;

/// Starts a thread named `name` that runs `body` until the stop flag is
/// set, after it has said through its sender whether it started.
fn spawn_capture(
    name: &str,
    body: impl FnOnce(&AtomicBool, &std::sync::mpsc::Sender<Result<(), CameraError>>) + Send + 'static,
) -> Result<Capturing, CameraError> {
    let stop = Arc::new(AtomicBool::new(false));
    let (opened, result) = std::sync::mpsc::channel();
    let thread_stop = stop.clone();
    let thread = std::thread::Builder::new()
        .name(name.into())
        .spawn(move || body(&thread_stop, &opened))
        .map_err(|e| CameraError::Open(format!("no thread: {e}")))?;
    let running = Running::new(stop, thread);
    match result.recv() {
        Ok(Ok(())) => Ok(running),
        Ok(Err(error)) => Err(error),
        Err(_) => Err(CameraError::Open("the camera thread stopped".into())),
    }
}

/// The probe's and the demo's camera: a moving test picture with a clock,
/// at [`FPS`], never a real device.
#[derive(Clone, Debug)]
pub struct TestPattern {
    frames: Latest,
    size: (usize, usize),
}

impl TestPattern {
    /// A 640×480 pattern whose frames go to `frames`.
    pub fn new(frames: Latest) -> Self {
        Self {
            frames,
            size: (MAX_WIDTH, MAX_HEIGHT),
        }
    }
}

impl Camera for TestPattern {
    type Open = Capturing;

    fn open(&mut self) -> Result<Capturing, CameraError> {
        let frames = self.frames.clone();
        let (width, height) = self.size;
        spawn_capture("noslacking-test-pattern", move |stop, opened| {
            let _ = opened.send(Ok(()));
            let started = Instant::now();
            let every = Duration::from_secs(1) / FPS;
            let mut due = started;
            let mut n = 0u64;
            while !stop.load(Ordering::Relaxed) {
                let at = Instant::now();
                frames.put(Frame {
                    picture: pattern(width, height, n, at.duration_since(started)),
                    at,
                });
                n += 1;
                due += every;
                std::thread::park_timeout(due.saturating_duration_since(Instant::now()));
            }
            frames.clear();
        })
    }
}

/// The test picture: frame `n` at `elapsed`. Colour bars sliding left, a
/// white square bouncing across, and the elapsed time as mm:ss.t in
/// large digits with the frame number under it, so a frozen or late
/// picture shows.
pub fn pattern(width: usize, height: usize, n: u64, elapsed: Duration) -> I420 {
    let mut out = I420::black(width, height);
    let (w, h) = (out.width, out.height);
    if w == 0 || h == 0 {
        return out;
    }
    // BT.601 studio-range colour bars: white, yellow, cyan, green,
    // magenta, red, blue, black.
    const BARS: [(u8, u8, u8); 8] = [
        (235, 128, 128),
        (210, 16, 146),
        (170, 166, 16),
        (145, 54, 34),
        (106, 202, 222),
        (81, 90, 240),
        (41, 240, 110),
        (16, 128, 128),
    ];
    let shift = usize::try_from(n * 4).unwrap_or(0);
    let bar = |x: usize| BARS[((x + shift) * BARS.len() / w) % BARS.len()];
    for row in 0..h {
        for x in 0..w {
            out.y[row * w + x] = bar(x).0;
        }
    }
    for row in 0..h / 2 {
        for x in 0..w / 2 {
            let (_, u, v) = bar(x * 2);
            out.u[row * (w / 2) + x] = u;
            out.v[row * (w / 2) + x] = v;
        }
    }
    // The bouncing square.
    let side = (h / 6).max(4) & !1;
    let span = w.saturating_sub(side).max(1);
    let travel = usize::try_from(n * 8).unwrap_or(0) % (2 * span);
    let left = (if travel < span {
        travel
    } else {
        2 * span - travel
    }) & !1;
    let top = (h / 8) & !1;
    fill(&mut out, left, top, side, side, (235, 128, 128));
    // The clock, on a dark band across the lower half.
    let band_top = (h / 2) & !1;
    let band = (h / 3) & !1;
    fill(&mut out, 0, band_top, w, band, (16, 128, 128));
    let tenths = elapsed.as_millis() / 100;
    let clock = format!(
        "{:02}:{:02}.{}",
        (tenths / 600) % 100,
        (tenths / 10) % 60,
        tenths % 10
    );
    let digit = (band * 5 / 8).max(10);
    text(&mut out, &clock, w / 16, band_top + band / 8, digit);
    let frame = format!("{n}");
    text(
        &mut out,
        &frame,
        w / 16,
        band_top + band / 8 + digit + digit / 6,
        (digit / 3).max(6),
    );
    out
}

/// Paints a rectangle in one colour.
fn fill(
    out: &mut I420,
    left: usize,
    top: usize,
    width: usize,
    height: usize,
    colour: (u8, u8, u8),
) {
    let (w, h) = (out.width, out.height);
    let right = (left + width).min(w);
    let bottom = (top + height).min(h);
    for row in top.min(h)..bottom {
        for x in left.min(w)..right {
            out.y[row * w + x] = colour.0;
        }
    }
    for row in top.min(h) / 2..bottom / 2 {
        for x in left.min(w) / 2..right / 2 {
            out.u[row * (w / 2) + x] = colour.1;
            out.v[row * (w / 2) + x] = colour.2;
        }
    }
}

/// Writes `text` (digits, `:` and `.`) in seven-segment strokes `size`
/// pixels high, from `left`, `top`.
fn text(out: &mut I420, text: &str, left: usize, top: usize, size: usize) {
    // Segments a–g, as bits.
    const DIGITS: [u8; 10] = [
        0b011_1111, 0b000_0110, 0b101_1011, 0b100_1111, 0b110_0110, 0b110_1101, 0b111_1101,
        0b000_0111, 0b111_1111, 0b110_1111,
    ];
    let white = (235, 128, 128);
    let stroke = (size / 8).max(2);
    let width = size / 2;
    let mut x = left;
    for c in text.chars() {
        match c {
            ':' => {
                fill(out, x, top + size / 4, stroke, stroke, white);
                fill(out, x, top + size * 3 / 4, stroke, stroke, white);
                x += stroke * 3;
            }
            '.' => {
                fill(out, x, top + size - stroke, stroke, stroke, white);
                x += stroke * 3;
            }
            _ => {
                let Some(bits) = c.to_digit(10).and_then(|d| DIGITS.get(d as usize)) else {
                    continue;
                };
                let half = size / 2;
                let segments = [
                    (x, top, width, stroke),                        // a
                    (x + width - stroke, top, stroke, half),        // b
                    (x + width - stroke, top + half, stroke, half), // c
                    (x, top + size - stroke, width, stroke),        // d
                    (x, top + half, stroke, half),                  // e
                    (x, top, stroke, half),                         // f
                    (x, top + half - stroke / 2, width, stroke),    // g
                ];
                for (i, &(sx, sy, sw, sh)) in segments.iter().enumerate() {
                    if bits & (1 << i) != 0 {
                        fill(out, sx, sy, sw, sh, white);
                    }
                }
                x += width + stroke * 2;
            }
        }
    }
}

/// The first camera, through `nokhwa`.
#[derive(Clone, Debug)]
pub struct Nokhwa {
    frames: Latest,
}

impl Nokhwa {
    /// The system's first camera, its frames going to `frames`.
    pub fn new(frames: Latest) -> Self {
        Self { frames }
    }
}

impl Camera for Nokhwa {
    type Open = Capturing;

    fn open(&mut self) -> Result<Capturing, CameraError> {
        allowed()?;
        let frames = self.frames.clone();
        spawn_capture("noslacking-huddle-camera", move |stop, opened| {
            capture(&frames, stop, opened);
        })
    }
}

/// Asks macOS for the camera, once: the system asks the user the first
/// time, with Info.plist's `NSCameraUsageDescription`. Elsewhere there
/// is nothing to ask before opening.
#[cfg(target_os = "macos")]
fn allowed() -> Result<(), CameraError> {
    if nokhwa::nokhwa_check() {
        return Ok(());
    }
    let (said, answer) = std::sync::mpsc::channel();
    nokhwa::nokhwa_initialize(move |granted| {
        let _ = said.send(granted);
    });
    // The user may take a while to answer the system's question.
    match answer.recv_timeout(Duration::from_secs(60)) {
        Ok(true) => Ok(()),
        Ok(false) => Err(CameraError::Denied),
        Err(_) => Err(CameraError::Denied),
    }
}

#[cfg(not(target_os = "macos"))]
fn allowed() -> Result<(), CameraError> {
    Ok(())
}

/// What a `nokhwa` error means to the user.
fn camera_error(error: &nokhwa::NokhwaError) -> CameraError {
    let text = error.to_string();
    let lower = text.to_lowercase();
    // Windows says E_ACCESSDENIED (0x80070005) when its privacy settings
    // keep apps from the camera; Linux says EACCES ("Permission denied").
    if lower.contains("denied") || lower.contains("0x80070005") || lower.contains("permission") {
        CameraError::Denied
    } else {
        CameraError::Open(text)
    }
}

/// The formats taken from the camera, cheapest to read first.
const FORMATS: [nokhwa::utils::FrameFormat; 5] = [
    nokhwa::utils::FrameFormat::YUYV,
    nokhwa::utils::FrameFormat::NV12,
    nokhwa::utils::FrameFormat::MJPEG,
    nokhwa::utils::FrameFormat::RAWRGB,
    nokhwa::utils::FrameFormat::GRAY,
];

/// One camera frame as the I420 sent, shrunk to fit.
fn convert(buffer: &nokhwa::Buffer) -> Option<I420> {
    use nokhwa::utils::FrameFormat;
    let resolution = buffer.resolution();
    let (width, height) = (
        usize::try_from(resolution.width()).ok()?,
        usize::try_from(resolution.height()).ok()?,
    );
    let data = buffer.buffer();
    let picture = match buffer.source_frame_format() {
        FrameFormat::YUYV => from_yuyv(data, width, height),
        FrameFormat::NV12 => from_nv12(data, width, height),
        FrameFormat::MJPEG => from_mjpeg(data),
        FrameFormat::RAWRGB => from_rgb(data, width, height),
        FrameFormat::GRAY => from_gray(data, width, height),
        FrameFormat::RAWBGR => None,
    }?;
    let (w, h) = fit(picture.width, picture.height, (MAX_WIDTH, MAX_HEIGHT))?;
    Some(scale(&picture, w, h))
}

/// The camera's thread: opens the first camera, says how that went, then
/// hands on each frame until told to stop, and closes the camera.
fn capture(
    frames: &Latest,
    stop: &AtomicBool,
    opened: &std::sync::mpsc::Sender<Result<(), CameraError>>,
) {
    use nokhwa::utils::{
        ApiBackend, CameraFormat, CameraIndex, RequestedFormat, RequestedFormatType, Resolution,
    };
    match nokhwa::query(ApiBackend::Auto) {
        Ok(cameras) if cameras.is_empty() => {
            let _ = opened.send(Err(CameraError::NoDevice));
            return;
        }
        Ok(cameras) => log::info!(
            "huddle camera: {} camera(s): {}",
            cameras.len(),
            cameras
                .iter()
                .map(|c| c.human_name())
                .collect::<Vec<_>>()
                .join(", ")
        ),
        Err(error) => log::info!("huddle camera: could not list cameras: {error}"),
    }
    let wanted = CameraFormat::new(
        Resolution::new(
            u32::try_from(MAX_WIDTH).unwrap_or(640),
            u32::try_from(MAX_HEIGHT).unwrap_or(480),
        ),
        nokhwa::utils::FrameFormat::YUYV,
        FPS,
    );
    let request = RequestedFormat::with_formats(RequestedFormatType::Closest(wanted), &FORMATS);
    let mut camera = match nokhwa::Camera::new(CameraIndex::Index(0), request) {
        Ok(camera) => camera,
        Err(error) => {
            let _ = opened.send(Err(camera_error(&error)));
            return;
        }
    };
    if let Err(error) = camera.open_stream() {
        let _ = opened.send(Err(camera_error(&error)));
        return;
    }
    let format = camera.camera_format();
    log::info!(
        "huddle camera: open: {} ({}x{} {} at {} fps)",
        camera.info().human_name(),
        format.width(),
        format.height(),
        format.format(),
        format.frame_rate()
    );
    let _ = opened.send(Ok(()));
    let mut taken = 0u64;
    let mut unreadable = 0u64;
    let mut failures = 0u32;
    while !stop.load(Ordering::Relaxed) {
        match camera.frame() {
            Ok(buffer) => {
                failures = 0;
                let at = Instant::now();
                match convert(&buffer) {
                    Some(picture) => {
                        taken += 1;
                        frames.put(Frame { picture, at });
                    }
                    None => {
                        unreadable += 1;
                        if unreadable == 1 {
                            log::warn!(
                                "huddle camera: a {} frame of {} bytes could not be read",
                                buffer.source_frame_format(),
                                buffer.buffer().len()
                            );
                        }
                    }
                }
            }
            Err(error) => {
                failures += 1;
                log::warn!("huddle camera: {error}");
                if failures >= 10 {
                    log::warn!("huddle camera: giving up after {failures} failures in a row");
                    break;
                }
                std::thread::sleep(Duration::from_millis(100));
            }
        }
    }
    frames.clear();
    // Stopped here, not in its drop: nokhwa's drop panics if stopping
    // fails, and a panic in a release build ends the app.
    if let Err(error) = camera.stop_stream() {
        log::warn!("huddle camera: stopping: {error}");
    }
    drop(camera);
    log::info!("huddle camera: closed; {taken} frames taken, {unreadable} unreadable");
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;

    /// A pretend camera that counts how many are open.
    #[derive(Clone, Default)]
    struct Pretend {
        open: Arc<AtomicUsize>,
        opened: Arc<AtomicUsize>,
        refuse: Arc<Mutex<Option<CameraError>>>,
    }

    struct PretendOpen(Arc<AtomicUsize>);

    impl Drop for PretendOpen {
        fn drop(&mut self) {
            self.0.fetch_sub(1, Ordering::SeqCst);
        }
    }

    impl Camera for Pretend {
        type Open = PretendOpen;

        fn open(&mut self) -> Result<PretendOpen, CameraError> {
            if let Some(error) = self.refuse.lock().expect("lock").clone() {
                return Err(error);
            }
            self.opened.fetch_add(1, Ordering::SeqCst);
            self.open.fetch_add(1, Ordering::SeqCst);
            Ok(PretendOpen(self.open.clone()))
        }
    }

    #[test]
    fn the_camera_is_open_only_while_on() {
        let device = Pretend::default();
        let mut control = CameraControl::new(device.clone());
        // Joined with the camera off: never opened.
        assert_eq!(device.opened.load(Ordering::SeqCst), 0);
        control.set_on(false).expect("off");
        assert_eq!(device.opened.load(Ordering::SeqCst), 0);
        assert!(!control.is_open());

        control.set_on(true).expect("on");
        assert_eq!(device.open.load(Ordering::SeqCst), 1);
        // On again opens nothing more.
        control.set_on(true).expect("on");
        assert_eq!(device.opened.load(Ordering::SeqCst), 1);

        control.set_on(false).expect("off");
        assert_eq!(device.open.load(Ordering::SeqCst), 0, "closed when off");
        assert!(!control.is_open());

        control.set_on(true).expect("on");
        assert_eq!(device.open.load(Ordering::SeqCst), 1);
        drop(control);
        assert_eq!(device.open.load(Ordering::SeqCst), 0, "closed on leave");
        assert_eq!(device.opened.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn a_camera_that_would_not_open_stays_off() {
        for error in [
            CameraError::NoDevice,
            CameraError::Denied,
            CameraError::Open("busy".into()),
        ] {
            let device = Pretend::default();
            *device.refuse.lock().expect("lock") = Some(error.clone());
            let mut control = CameraControl::new(device.clone());
            assert_eq!(control.set_on(true), Err(error));
            assert!(!control.is_open());
            assert_eq!(device.open.load(Ordering::SeqCst), 0);
            // It may be allowed later.
            *device.refuse.lock().expect("lock") = None;
            control.set_on(true).expect("on");
            assert!(control.is_open());
        }
    }

    #[test]
    fn the_test_pattern_runs_only_while_open_and_stops_its_thread() {
        let frames = Latest::default();
        let mut control = CameraControl::new(TestPattern::new(frames.clone()));
        std::thread::sleep(Duration::from_millis(100));
        assert_eq!(frames.put_count(), 0, "nothing before it is on");
        control.set_on(true).expect("on");
        let frame = frames.take(Duration::from_secs(2)).expect("a frame");
        assert_eq!((frame.picture.width, frame.picture.height), (640, 480));
        assert!(frame.picture.whole());
        control.set_on(false).expect("off");
        // Joined: no frame comes after it is off.
        let after = frames.put_count();
        std::thread::sleep(Duration::from_millis(150));
        assert_eq!(frames.put_count(), after);
        assert!(frames.take(Duration::from_millis(10)).is_none());
    }

    #[test]
    fn the_newest_frame_wins() {
        let latest = Latest::default();
        let at = Instant::now();
        for n in 0..3u8 {
            let mut picture = I420::black(16, 16);
            picture.y[0] = n;
            latest.put(Frame { picture, at });
        }
        assert_eq!(latest.replaced(), 2);
        let frame = latest.take(Duration::ZERO).expect("the newest");
        assert_eq!(frame.picture.y[0], 2);
        assert!(latest.take(Duration::from_millis(5)).is_none());
    }

    #[test]
    fn larger_pictures_shrink_to_fit_keeping_their_shape() {
        let max = (MAX_WIDTH, MAX_HEIGHT);
        assert_eq!(fit(640, 480, max), Some((640, 480)));
        assert_eq!(fit(1280, 720, max), Some((640, 360)));
        assert_eq!(fit(1920, 1080, max), Some((640, 360)));
        assert_eq!(fit(480, 480, max), Some((480, 480)));
        assert_eq!(fit(1080, 1920, max), Some((270, 480)));
        // Never enlarged; odd sizes become even.
        assert_eq!(fit(320, 240, max), Some((320, 240)));
        assert_eq!(fit(641, 481, max), Some((640, 480)));
        assert_eq!(fit(333, 211, max), Some((332, 210)));
        // Too small to send.
        assert_eq!(fit(0, 0, max), None);
        assert_eq!(fit(15, 300, max), None);
        assert_eq!(fit(10_000, 16, max), None);
    }

    #[test]
    fn scaling_averages_and_keeps_planes_whole() {
        let mut picture = I420::black(8, 4);
        // Left half white.
        for row in 0..4 {
            for x in 0..4 {
                picture.y[row * 8 + x] = 235;
            }
        }
        let half = scale(&picture, 4, 2);
        assert!(half.whole());
        assert_eq!(half.y, vec![235, 235, 16, 16, 235, 235, 16, 16]);
        // An odd or larger target is clamped, never panics.
        for (w, h) in [(0, 0), (3, 3), (9, 9), (100, 1)] {
            assert!(scale(&picture, w, h).whole(), "{w}x{h}");
        }
        let big = pattern(1280, 720, 3, Duration::from_millis(1234));
        let small = scale(&big, 640, 360);
        assert!(small.whole());
        assert_eq!((small.width, small.height), (640, 360));
    }

    #[test]
    fn camera_formats_become_i420() {
        // YUYV: Y0 U Y1 V for a 4x2 picture.
        let yuyv = [
            10, 100, 20, 200, 30, 110, 40, 210, //
            50, 102, 60, 202, 70, 112, 80, 212,
        ];
        let picture = from_yuyv(&yuyv, 4, 2).expect("YUYV");
        assert_eq!(picture.y, vec![10, 20, 30, 40, 50, 60, 70, 80]);
        assert_eq!(picture.u, vec![101, 111]);
        assert_eq!(picture.v, vec![201, 211]);
        assert!(from_yuyv(&yuyv[..10], 4, 2).is_none(), "too short");

        // NV12: 4x2 luma, then one row of U V U V.
        let nv12 = [1, 2, 3, 4, 5, 6, 7, 8, 100, 200, 110, 210];
        let picture = from_nv12(&nv12, 4, 2).expect("NV12");
        assert_eq!(picture.y, vec![1, 2, 3, 4, 5, 6, 7, 8]);
        assert_eq!(picture.u, vec![100, 110]);
        assert_eq!(picture.v, vec![200, 210]);
        assert!(from_nv12(&nv12[..9], 4, 2).is_none());

        // RGB: white and black, in studio range.
        let mut rgb = vec![255u8; 4 * 2 * 3];
        rgb[..6].fill(0);
        let picture = from_rgb(&rgb, 4, 2).expect("RGB");
        assert!(picture.whole());
        assert!(picture.y[0] <= 17 && picture.y[3] >= 234, "{:?}", picture.y);

        // Odd sizes lose their last row and column, never panic.
        let odd = from_rgb(&[128u8; 5 * 3 * 3], 5, 3).expect("RGB");
        assert_eq!((odd.width, odd.height), (4, 2));
        assert!(from_gray(&[0u8; 9], 3, 3).is_some_and(|p| p.whole()));
        assert!(from_yuyv(&[], 0, 0).is_none());
        assert!(from_mjpeg(b"not a jpeg").is_none());
    }

    #[test]
    fn a_jpeg_frame_decodes() {
        let mut jpeg = Vec::new();
        let image = image::RgbImage::from_pixel(32, 24, image::Rgb([200, 30, 30]));
        image::DynamicImage::ImageRgb8(image)
            .write_to(
                &mut std::io::Cursor::new(&mut jpeg),
                image::ImageFormat::Jpeg,
            )
            .expect("encoded");
        let picture = from_mjpeg(&jpeg).expect("decoded");
        assert_eq!((picture.width, picture.height), (32, 24));
        // Red: Cr high.
        assert!(picture.v[0] > 200, "{}", picture.v[0]);
    }

    #[test]
    fn the_pattern_moves_and_shows_the_time() {
        let a = pattern(640, 480, 0, Duration::ZERO);
        let b = pattern(640, 480, 1, Duration::from_millis(66));
        assert!(a.whole() && b.whole());
        assert_ne!(a, b, "it moves");
        let c = pattern(640, 480, 0, Duration::from_secs(61));
        assert_ne!(a, c, "the clock changes the picture");
        // Any size, never a panic.
        for (w, h) in [(0, 0), (2, 2), (16, 16), (18, 10), (642, 362)] {
            let p = pattern(w, h, 7, Duration::from_secs(3));
            assert_eq!((p.width, p.height), (w & !1, h & !1));
        }
    }
}
