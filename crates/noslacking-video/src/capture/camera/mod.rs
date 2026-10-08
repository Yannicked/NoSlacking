//! The camera the user sends: which there are, and its frames.
//!
//! The rule, the microphone's: no camera is opened until the app starts
//! one ([`Cameras::start`]), and it is closed (its stream stopped, its
//! buffers let go, its thread joined: its light goes out) when the app
//! closes the capture, which it does when the user turns the camera
//! off, leaves, or the huddle takes no video. Where the frames come
//! from, chosen at build time ([`System`]):
//!
//! - Linux: V4L2, spoken directly ([`v4l2`]): no libclang, no PipeWire.
//! - macOS and Windows: `nokhwa` (AVFoundation, Media Foundation).
//! - [`CameraChoice::Test`]: a generated 640×480 picture with a moving
//!   clock ([`TestCamera`]), for the probe, the demo and the benchmarks.
//!
//! Each camera is read on a thread of its own (a [`Device`] blocks until
//! its next frame), which converts the frame to I420 and hands it to the
//! capture's pipeline thread ([`crate::pipeline::Ask::Picture`]): the
//! pipeline keeps the newest and encodes it when the app asks, so a slow
//! encode drops pictures rather than queueing them.

pub mod convert;
#[cfg(any(target_os = "macos", windows))]
mod native;
#[cfg(all(
    target_os = "linux",
    target_pointer_width = "64",
    any(
        target_arch = "x86_64",
        target_arch = "aarch64",
        target_arch = "riscv64",
        target_arch = "loongarch64"
    )
))]
#[allow(unsafe_code)]
pub mod v4l2;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::time::{Duration, Instant};

use noslacking_video_ipc::{CameraChoice, CaptureProblem, Planes, Source};

use super::Trouble;
use crate::pipeline::{Ask, Capture, Pipeline, Settings};

/// Failures in a row (each a tenth of a second apart) after which a
/// camera is taken for gone.
const FAILURES: u32 = 10;

/// An open camera, read on its reader thread (and made there: some
/// systems' camera objects must stay on the thread that opened them).
pub trait Device {
    /// Its name, for the log.
    fn name(&self) -> String;
    /// The next frame as I420 and when it was taken: none if nothing
    /// came within a fraction of a second (so the thread can look
    /// whether to stop) or the frame did not read. An error with
    /// [`CaptureProblem::Ended`] or [`CaptureProblem::Gone`] means the
    /// camera went away; others may pass.
    fn next(&mut self) -> Result<Option<(Planes, Instant)>, Trouble>;
}

/// What cameras can be started on: the system's, or pretend ones in
/// tests.
pub trait Cameras {
    /// The cameras there are.
    fn list(&mut self) -> Result<Vec<Source>, Trouble>;
    /// Opens `choice` and starts encoding it as `settings` say. Blocks
    /// while the system asks the user for the camera (macOS, the first
    /// time).
    fn start(&mut self, choice: &CameraChoice, settings: Settings) -> Result<Capture, Trouble>;
}

/// What opens a [`Device`], on the reader thread.
pub type Opener = Box<dyn FnOnce() -> Result<Box<dyn Device>, Trouble> + Send>;

/// Starts a camera: a capture thread running the pipeline, and on it a
/// reader thread that opens the device with `open` and puts each frame
/// in, until the capture is closed. Returns once the device opened, or
/// why it did not.
pub fn run(name: &str, settings: Settings, open: Opener) -> Result<Capture, Trouble> {
    let reader_name = format!("{name}-reader");
    super::spawn(name, settings, move |pipeline, inbox, feed, started| {
        let stop = Arc::new(AtomicBool::new(false));
        let (opened, result) = mpsc::channel();
        let reader_stop = Arc::clone(&stop);
        let reader = std::thread::Builder::new()
            .name(reader_name)
            .spawn(move || {
                let mut device = match open() {
                    Ok(device) => {
                        let _ = opened.send(Ok(()));
                        device
                    }
                    Err(trouble) => {
                        let _ = opened.send(Err(trouble));
                        return;
                    }
                };
                read(device.as_mut(), &feed, &reader_stop);
                // Closed here, on the thread that opened it.
                drop(device);
            });
        let reader = match reader {
            Ok(reader) => reader,
            Err(error) => {
                let _ = started.send(Err(Trouble::failed(format!("no reader thread: {error}"))));
                return;
            }
        };
        match result.recv() {
            Ok(Ok(())) => {
                let _ = started.send(Ok(()));
            }
            Ok(Err(trouble)) => {
                let _ = started.send(Err(trouble));
                let _ = reader.join();
                return;
            }
            Err(_) => {
                let _ = started.send(Err(Trouble::failed("the camera's thread stopped")));
                let _ = reader.join();
                return;
            }
        }
        fed(pipeline, &inbox);
        stop.store(true, Ordering::Relaxed);
        let _ = reader.join();
    })
}

/// The reader thread: hands `device`'s frames to the pipeline through
/// `feed` until `stop`, or until the camera goes away (then says so).
fn read(device: &mut dyn Device, feed: &mpsc::Sender<Ask>, stop: &AtomicBool) {
    let mut failures = 0;
    while !stop.load(Ordering::Relaxed) {
        match device.next() {
            Ok(Some((picture, at))) => {
                failures = 0;
                if feed.send(Ask::Picture { picture, at }).is_err() {
                    break;
                }
            }
            Ok(None) => {}
            Err(trouble)
                if matches!(
                    trouble.problem,
                    CaptureProblem::Ended | CaptureProblem::Gone
                ) =>
            {
                let _ = feed.send(Ask::Ended(Trouble::new(
                    CaptureProblem::Ended,
                    trouble.detail,
                )));
                break;
            }
            Err(trouble) => {
                failures += 1;
                eprintln!("noslacking-video: camera: {trouble}");
                if failures >= FAILURES {
                    let _ = feed.send(Ask::Ended(Trouble::new(
                        CaptureProblem::Ended,
                        format!("{failures} failures in a row: {}", trouble.detail),
                    )));
                    break;
                }
                std::thread::sleep(Duration::from_millis(100));
            }
        }
    }
}

/// The pipeline thread of a capture whose pictures come through its
/// inbox: hands every ask and picture to `pipeline` until the capture
/// is closed.
pub fn fed(pipeline: &mut Pipeline, inbox: &mpsc::Receiver<Ask>) {
    loop {
        let wait = pipeline.deadline().map_or(Duration::from_secs(1), |at| {
            at.saturating_duration_since(Instant::now())
        });
        match inbox.recv_timeout(wait) {
            Ok(Ask::Stop) | Err(RecvTimeoutError::Disconnected) => break,
            Ok(ask) => pipeline.ask(ask),
            Err(RecvTimeoutError::Timeout) => {}
        }
        pipeline.tick(Instant::now());
    }
}

/// The test camera: [`super::pattern::pattern`] at 640×480 and 30 a
/// second, its colour bars sliding and its clock running, never anyone's
/// camera.
#[derive(Debug)]
pub struct TestCamera {
    size: (u32, u32),
    began: Instant,
    due: Instant,
    n: u64,
}

impl TestCamera {
    /// A `width`×`height` test camera, starting now.
    pub fn new(width: u32, height: u32) -> Self {
        let now = Instant::now();
        Self {
            size: (width, height),
            began: now,
            due: now,
            n: 0,
        }
    }

    /// The test camera as a device to open.
    pub fn opener() -> Opener {
        Box::new(|| Ok(Box::new(Self::new(640, 480)) as Box<dyn Device>))
    }
}

impl Device for TestCamera {
    fn name(&self) -> String {
        "the test camera".into()
    }

    fn next(&mut self) -> Result<Option<(Planes, Instant)>, Trouble> {
        let now = Instant::now();
        if self.due > now {
            std::thread::sleep(self.due - now);
        }
        let at = Instant::now();
        let picture = super::pattern::pattern(
            self.size.0,
            self.size.1,
            self.n,
            at.duration_since(self.began),
        );
        self.n += 1;
        self.due += Duration::from_secs(1) / crate::pipeline::Profile::CAMERA.fps;
        if self.due < at {
            // Late: count from now rather than race to catch up.
            self.due = at;
        }
        Ok(Some((picture, at)))
    }
}

/// The system's cameras.
#[derive(Debug, Default)]
pub struct System;

impl System {
    /// How cameras are reached here, for the log.
    pub fn name(&self) -> &'static str {
        if cfg!(any(target_os = "macos", windows)) {
            "nokhwa"
        } else if cfg!(all(
            target_os = "linux",
            target_pointer_width = "64",
            any(
                target_arch = "x86_64",
                target_arch = "aarch64",
                target_arch = "riscv64",
                target_arch = "loongarch64"
            )
        )) {
            "V4L2"
        } else {
            "nothing"
        }
    }
}

/// No camera can be reached on this system.
#[allow(dead_code, reason = "systems with a camera back end never say this")]
fn unavailable() -> Trouble {
    Trouble::new(
        CaptureProblem::Unavailable,
        "this helper cannot reach a camera on this system",
    )
}

impl Cameras for System {
    fn list(&mut self) -> Result<Vec<Source>, Trouble> {
        #[cfg(any(target_os = "macos", windows))]
        return native::list();
        #[cfg(all(
            target_os = "linux",
            target_pointer_width = "64",
            any(
                target_arch = "x86_64",
                target_arch = "aarch64",
                target_arch = "riscv64",
                target_arch = "loongarch64"
            )
        ))]
        return v4l2::list();
        #[allow(unreachable_code)]
        Err(unavailable())
    }

    fn start(&mut self, choice: &CameraChoice, settings: Settings) -> Result<Capture, Trouble> {
        let open: Opener = match choice {
            CameraChoice::Test => TestCamera::opener(),
            #[cfg(any(target_os = "macos", windows))]
            CameraChoice::First => Box::new(|| {
                native::NativeCamera::open(None).map(|c| Box::new(c) as Box<dyn Device>)
            }),
            #[cfg(any(target_os = "macos", windows))]
            CameraChoice::Device(id) => {
                let id = id.clone();
                Box::new(move || {
                    native::NativeCamera::open(Some(&id)).map(|c| Box::new(c) as Box<dyn Device>)
                })
            }
            #[cfg(all(
                target_os = "linux",
                target_pointer_width = "64",
                any(
                    target_arch = "x86_64",
                    target_arch = "aarch64",
                    target_arch = "riscv64",
                    target_arch = "loongarch64"
                )
            ))]
            CameraChoice::First | CameraChoice::Device(_) => {
                let path = match choice {
                    CameraChoice::Device(id) => v4l2::path_of(id).ok_or_else(|| {
                        Trouble::new(CaptureProblem::Gone, format!("no camera {id}"))
                    })?,
                    _ => {
                        let cameras = v4l2::list()?;
                        let first = cameras.first().ok_or_else(|| {
                            Trouble::new(CaptureProblem::Unavailable, "no camera")
                        })?;
                        v4l2::path_of(&first.id)
                            .ok_or_else(|| Trouble::failed("a camera with no device"))?
                    }
                };
                Box::new(move || {
                    v4l2::V4l2Camera::open(&path).map(|c| Box::new(c) as Box<dyn Device>)
                })
            }
            #[allow(unreachable_patterns)]
            _ => return Err(unavailable()),
        };
        run("noslacking-camera", settings, open)
    }
}

#[cfg(all(
    target_os = "linux",
    target_pointer_width = "64",
    any(
        target_arch = "x86_64",
        target_arch = "aarch64",
        target_arch = "riscv64",
        target_arch = "loongarch64"
    )
))]
impl Device for v4l2::V4l2Camera {
    fn name(&self) -> String {
        self.name().to_owned()
    }

    fn next(&mut self) -> Result<Option<(Planes, Instant)>, Trouble> {
        v4l2::V4l2Camera::frame(self)
    }
}

#[cfg(any(target_os = "macos", windows))]
impl Device for native::NativeCamera {
    fn name(&self) -> String {
        self.name().to_owned()
    }

    fn next(&mut self) -> Result<Option<(Planes, Instant)>, Trouble> {
        native::NativeCamera::frame(self)
    }
}

/// Pretend cameras, all of them the test camera: what tests (and the
/// app's tests, through the helper's server on a thread) start.
#[derive(Clone, Debug, Default)]
pub struct Pretend {
    /// What [`Cameras::list`] gives.
    pub cameras: Vec<Source>,
    /// Why every camera but the test one fails to start, if it does.
    pub refuse: Option<Trouble>,
}

impl Cameras for Pretend {
    fn list(&mut self) -> Result<Vec<Source>, Trouble> {
        Ok(self.cameras.clone())
    }

    fn start(&mut self, choice: &CameraChoice, settings: Settings) -> Result<Capture, Trouble> {
        match choice {
            CameraChoice::Test => {}
            _ if self.refuse.is_some() => {
                return Err(self.refuse.clone().unwrap_or_else(unavailable));
            }
            CameraChoice::First if self.cameras.is_empty() => {
                return Err(Trouble::new(CaptureProblem::Unavailable, "no camera"));
            }
            CameraChoice::Device(id) if !self.cameras.iter().any(|c| &c.id == id) => {
                return Err(Trouble::new(
                    CaptureProblem::Gone,
                    format!("no camera {id}"),
                ));
            }
            _ => {}
        }
        run("noslacking-camera-pretend", settings, TestCamera::opener())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::nal;
    use crate::pipeline::{Answer, Settings};
    use noslacking_video_ipc::SourceKind;
    use std::sync::Mutex;

    fn settings() -> Settings {
        Settings::camera(false, 600_000, None, 320)
    }

    /// Asks `capture` for frames until `n` came.
    fn frames(capture: &Capture, n: usize) -> Vec<noslacking_video_ipc::CapturedFrame> {
        let mut got = Vec::new();
        let deadline = Instant::now() + Duration::from_secs(10);
        while got.len() < n && Instant::now() < deadline {
            if let Ok(Some(frame)) = capture.next(false, false, Duration::from_millis(100)) {
                got.push(frame);
            }
        }
        got
    }

    /// A pretend device: frames as told, then the given trouble; counts
    /// how many are open.
    struct Fake {
        script: Vec<Result<Option<(u32, u32)>, Trouble>>,
        open: Arc<Mutex<i32>>,
    }

    impl Device for Fake {
        fn name(&self) -> String {
            "a pretend camera".into()
        }
        fn next(&mut self) -> Result<Option<(Planes, Instant)>, Trouble> {
            std::thread::sleep(Duration::from_millis(25));
            if self.script.is_empty() {
                return Ok(None);
            }
            match self.script.remove(0) {
                Ok(Some((w, h))) => Ok(Some((
                    crate::capture::pattern::pattern(w, h, 0, Duration::ZERO),
                    Instant::now(),
                ))),
                Ok(None) => Ok(None),
                Err(trouble) => Err(trouble),
            }
        }
    }

    impl Drop for Fake {
        fn drop(&mut self) {
            *self.open.lock().expect("a lock") -= 1;
        }
    }

    fn fake(open: &Arc<Mutex<i32>>, script: Vec<Result<Option<(u32, u32)>, Trouble>>) -> Opener {
        let open = Arc::clone(open);
        Box::new(move || {
            *open.lock().expect("a lock") += 1;
            Ok(Box::new(Fake { script, open }) as Box<dyn Device>)
        })
    }

    #[test]
    fn the_test_camera_sends_640x480_with_its_self_view_and_closes() {
        let capture = Pretend::default()
            .start(&CameraChoice::Test, settings())
            .expect("started");
        let got = frames(&capture, 5);
        assert_eq!(got.len(), 5);
        assert!(got[0].keyframe);
        assert!(nal::types(&got[0].data).starts_with(&[7, 8]));
        for frame in &got {
            assert_eq!((frame.width, frame.height), (640, 480));
            let preview = frame.preview.as_ref().expect("a self-view");
            assert_eq!((preview.width, preview.height), (320, 240));
        }
        drop(capture);
    }

    #[test]
    fn a_device_is_open_only_while_its_capture_is() {
        let open = Arc::new(Mutex::new(0));
        let capture = run(
            "test-camera",
            settings(),
            fake(&open, vec![Ok(Some((320, 240))); 50]),
        )
        .expect("started");
        assert_eq!(*open.lock().expect("a lock"), 1);
        assert_eq!(frames(&capture, 2).len(), 2);
        drop(capture);
        assert_eq!(*open.lock().expect("a lock"), 0, "closed with its capture");
    }

    #[test]
    fn a_camera_that_will_not_open_says_why() {
        let refused: Opener = Box::new(|| {
            Err(Trouble::new(
                CaptureProblem::Busy,
                "/dev/video0: Device or resource busy",
            ))
        });
        let trouble = run("test-camera", settings(), refused).expect_err("refused");
        assert_eq!(trouble.problem, CaptureProblem::Busy);
        // Pretend cameras: none, one that is gone, one refused.
        let mut none = Pretend::default();
        assert_eq!(
            none.start(&CameraChoice::First, settings())
                .expect_err("none")
                .problem,
            CaptureProblem::Unavailable
        );
        let mut one = Pretend {
            cameras: vec![Source {
                id: "pretend:1".into(),
                name: "A pretend camera".into(),
                kind: SourceKind::Camera,
            }],
            refuse: None,
        };
        assert_eq!(one.list().expect("listed").len(), 1);
        assert_eq!(
            one.start(&CameraChoice::Device("pretend:9".into()), settings())
                .expect_err("gone")
                .problem,
            CaptureProblem::Gone
        );
        drop(
            one.start(&CameraChoice::Device("pretend:1".into()), settings())
                .expect("started"),
        );
        one.refuse = Some(Trouble::new(CaptureProblem::Denied, "not allowed"));
        assert_eq!(
            one.start(&CameraChoice::First, settings())
                .expect_err("denied")
                .problem,
            CaptureProblem::Denied
        );
    }

    #[test]
    fn a_camera_that_goes_away_ends_its_capture() {
        let open = Arc::new(Mutex::new(0));
        let capture = run(
            "test-camera",
            settings(),
            fake(
                &open,
                vec![
                    Ok(Some((320, 240))),
                    Err(Trouble::failed("one bad frame")),
                    Ok(Some((320, 240))),
                    Err(Trouble::new(CaptureProblem::Ended, "unplugged")),
                ],
            ),
        )
        .expect("started");
        let mut answers: Vec<Answer> = Vec::new();
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            let answer = capture.next(false, false, Duration::from_millis(100));
            let done = answer.is_err();
            answers.push(answer);
            if done {
                break;
            }
        }
        let pictures = answers.iter().filter(|a| matches!(a, Ok(Some(_)))).count();
        assert_eq!(pictures, 2, "one bad frame passes");
        assert_eq!(
            answers
                .last()
                .map(|a| a.as_ref().map_err(|t| t.problem).err()),
            Some(Some(CaptureProblem::Ended))
        );
        drop(capture);
        assert_eq!(*open.lock().expect("a lock"), 0);
    }
}
