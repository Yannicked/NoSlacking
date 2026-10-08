//! `noslacking-video`, the video helper process (crates/noslacking-video):
//! everything that touches pixels. Every video stream we watch (the
//! `huddle-video` feature) is decoded there, on the GPU or in software;
//! our camera is opened, captured and encoded there (the `huddle-camera`
//! feature, [`Helper::start_camera`]), each frame coming back with a
//! small picture for the self-view; and the screen we share is captured
//! and encoded there (the `huddle-share` feature, [`Helper::start_share`]).
//! The app only shows pictures and sends H.264.
//!
//! Decoders read strangers' streams, the GPU's video APIs are C
//! libraries and drivers, and cameras and screens are reached through
//! the system's C interfaces (V4L2, PipeWire, AVFoundation, Media
//! Foundation): all take `unsafe` code this crate forbids or native
//! build dependencies, and a malformed stream or a driver may crash or
//! hang, and a panic aborts a release build. So they run in the helper,
//! which this module starts the first time something needs it and talks
//! to over its standard input and output (`noslacking-video-ipc`'s
//! messages). The helper is found next to this program, else on `PATH`.
//! Without it there is no video to watch (the call window says so, and
//! the call goes on), no camera and no screen to share.
//!
//! Each [`Lane`] has a helper of its own, so the share's decoding, the
//! cameras' and what we send never wait for each other's replies. A
//! reply that does not come within a timeout counts as a crash; after a
//! crash the helper is killed and started again on the next stream, at
//! most [`MAX_RESTARTS`] times, and then never again until the app
//! restarts. A stream that lost it asks for a keyframe and starts again
//! in the new helper; our camera starts again in it with a keyframe; a
//! share ends. Nothing from the helper is trusted: replies are checked
//! field by field, a picture's planes against its size, before use.

use std::io::{BufReader, BufWriter, Read, Write};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, SyncSender};
use std::sync::{Arc, Mutex, OnceLock, PoisonError};
use std::time::Duration;

use egui::{Color32, ColorImage};
#[cfg(feature = "huddle-video")]
use noslacking_video_ipc::FailKind;
use noslacking_video_ipc::{self as ipc, Codec, Reply, Request};

/// A picture from the helper as egui's opaque pixels, mirrored left to
/// right for a self-view (as a mirror shows you). H.264 from WebRTC
/// senders is BT.601 in studio range unless its VUI says otherwise, which
/// Chrome's screen shares do not.
pub fn to_image(planes: &ipc::Planes, mirror: bool) -> Result<ColorImage, String> {
    planes.check().map_err(|e| e.to_string())?;
    let size = |n: u32| usize::try_from(n).map_err(|e| e.to_string());
    let (width, height) = (size(planes.width)?, size(planes.height)?);
    let image = yuv::YuvPlanarImage {
        y_plane: &planes.y,
        y_stride: planes.width,
        u_plane: &planes.u,
        u_stride: planes.width.div_ceil(2),
        v_plane: &planes.v,
        v_stride: planes.width.div_ceil(2),
        width: planes.width,
        height: planes.height,
    };
    // Opaque: premultiplied and straight alpha are the same, so the
    // converter writes egui's own pixels.
    let mut pixels = vec![Color32::BLACK; width * height];
    yuv::yuv420_to_rgba(
        &image,
        bytemuck::cast_slice_mut(&mut pixels),
        planes.width * 4,
        yuv::YuvRange::Limited,
        yuv::YuvStandardMatrix::Bt601,
    )
    .map_err(|e| e.to_string())?;
    if mirror {
        for row in pixels.chunks_exact_mut(width) {
            row.reverse();
        }
    }
    Ok(ColorImage::new([width, height], pixels))
}

/// How many times a crashed or stuck helper is started again before it
/// is given up until the app restarts.
pub const MAX_RESTARTS: u32 = 3;
/// How long the helper may take to start and say what it can do: the
/// first open of a GPU driver can take a few hundred milliseconds.
const HELLO_TIMEOUT: Duration = Duration::from_secs(5);
/// How long one frame may take. A 1080p frame takes a few milliseconds
/// to decode or encode; a second means the driver hangs.
const CALL_TIMEOUT: Duration = Duration::from_secs(1);
/// The helper's name, beside this program or on `PATH`.
const HELPER: &str = "noslacking-video";

/// Settings → Huddles → Use the graphics card for video, set from the
/// settings at start-up (on by default there). Off until then, so tests
/// that want it turn it on for themselves.
static GPU: AtomicBool = AtomicBool::new(false);

/// Lets streams (and our camera and share) that start from now on use
/// the GPU, or not: the helper then decodes and encodes them in
/// software.
pub fn set_gpu(gpu: bool) {
    GPU.store(gpu, Ordering::Relaxed);
}

/// Whether streams starting now may decode, and our camera and share
/// encode, on the GPU.
pub fn gpu() -> bool {
    GPU.load(Ordering::Relaxed)
}

/// Why the helper cannot serve a request.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct Lost(String);

/// The pipes to a running helper and how to stop it.
pub struct Link {
    /// Its standard input.
    pub input: Box<dyn Write + Send>,
    /// Its standard output.
    pub output: Box<dyn Read + Send>,
    /// Kills it (and waits for it), when it crashed or hangs.
    pub stop: Box<dyn FnMut() + Send>,
}

/// Starts the helper. The real one runs the program; tests run a
/// pretend helper on a thread.
pub trait Launcher: Send + Sync {
    /// A newly started helper.
    fn launch(&self) -> std::io::Result<Link>;
}

/// Runs the `noslacking-video` program.
#[derive(Debug)]
pub struct ProcessLauncher {
    program: PathBuf,
}

impl ProcessLauncher {
    /// A launcher for the helper at `program`.
    pub fn new(program: PathBuf) -> Self {
        Self { program }
    }
}

impl Launcher for ProcessLauncher {
    fn launch(&self) -> std::io::Result<Link> {
        let mut child = Command::new(&self.program)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            // libva's greetings on every start are noise in our log;
            // its errors still come through.
            .env("LIBVA_MESSAGING_LEVEL", "1")
            .spawn()?;
        let missing = || std::io::Error::other("the helper's pipes are missing");
        let input = child.stdin.take().ok_or_else(missing)?;
        let output = child.stdout.take().ok_or_else(missing)?;
        if let Some(errors) = child.stderr.take() {
            // What the helper says (its back end, its failures) goes to
            // our log.
            let _ = std::thread::Builder::new()
                .name("video-helper-log".into())
                .spawn(move || {
                    use std::io::BufRead;
                    for line in std::io::BufReader::new(errors).lines() {
                        let Ok(line) = line else { break };
                        log::debug!("video helper: {line}");
                    }
                });
        }
        Ok(Link {
            input: Box::new(input),
            output: Box::new(output),
            stop: Box::new(move || {
                let _ = child.kill();
                let _ = child.wait();
            }),
        })
    }
}

/// Where the helper is: beside this program (how every package installs
/// it), else on `PATH`.
pub fn find_helper() -> Option<PathBuf> {
    let name = format!("{HELPER}{}", std::env::consts::EXE_SUFFIX);
    let beside = std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(|dir| dir.join(&name)))
        .filter(|path| path.is_file());
    beside.or_else(|| {
        std::env::var_os("PATH").and_then(|paths| {
            std::env::split_paths(&paths)
                .map(|dir| dir.join(&name))
                .find(|path| path.is_file())
        })
    })
}

/// What a helper process is for: each has its own, so one's work never
/// waits for another's replies, and one that keeps failing costs only
/// its own.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Lane {
    /// Decoding the screen share watched.
    Share,
    /// Decoding the camera tiles.
    Cameras,
    /// Capturing and encoding our camera: its own process, as opening it
    /// may keep a request waiting for the user (macOS asks the first
    /// time), and a camera driver that fails costs nothing else.
    Camera,
    /// Capturing and encoding the screen we share: its own process, as
    /// the system's dialog may keep a request waiting for the user.
    Screen,
}

/// The helper for `lane`, made the first time it is asked for and
/// started the first time it is used; none when the program is not
/// installed.
pub fn shared(lane: Lane) -> Option<Helper> {
    static SHARED: OnceLock<Option<[Helper; 4]>> = OnceLock::new();
    let helpers = SHARED.get_or_init(|| {
        let launcher = launcher()?;
        Some([(); 4].map(|()| Helper::new(Arc::clone(&launcher))))
    });
    let index = match lane {
        Lane::Share => 0,
        Lane::Cameras => 1,
        Lane::Camera => 2,
        Lane::Screen => 3,
    };
    helpers.as_ref().map(|helpers| helpers[index].clone())
}

/// A running helper: a thread writes requests to it and another reads
/// its replies, so a helper that stops reading or answering cannot hold
/// the caller past the timeout.
struct Live {
    requests: SyncSender<(u32, Request)>,
    replies: Receiver<Result<(u32, Reply), String>>,
    stop: Box<dyn FnMut() + Send>,
    seq: u32,
}

impl Drop for Live {
    fn drop(&mut self) {
        (self.stop)();
    }
}

struct State {
    live: Option<Live>,
    /// Crashes and hangs so far.
    failures: u32,
    /// No more tries: the helper is missing, of another version, or
    /// failed too often.
    given_up: bool,
    /// Moves on with every restart: decoders opened before it are gone.
    generation: u64,
}

/// Runs the installed program; none if it is not there.
#[cfg(not(test))]
fn launcher() -> Option<Arc<dyn Launcher>> {
    match find_helper() {
        Some(path) => {
            log::info!("video: the helper is {}", path.display());
            Some(Arc::new(ProcessLauncher::new(path)))
        }
        None => {
            log::warn!("video: no {HELPER} beside the app or on PATH: no video");
            None
        }
    }
}

/// Tests never start the real program, which would open the GPU: the
/// helper's own code serves them on a thread.
#[cfg(test)]
fn launcher() -> Option<Arc<dyn Launcher>> {
    Some(Arc::new(pretend::InThread))
}

/// The app's side of one helper, shared by the decoders and encoders of
/// its lane (cheap to clone). Requests go one at a time.
#[derive(Clone)]
pub struct Helper {
    state: Arc<Mutex<State>>,
    launcher: Arc<dyn Launcher>,
    hello_timeout: Duration,
    call_timeout: Duration,
}

impl std::fmt::Debug for Helper {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Helper").finish_non_exhaustive()
    }
}

impl Helper {
    /// A helper started by `launcher` when first needed.
    pub fn new(launcher: Arc<dyn Launcher>) -> Self {
        Self::with_timeouts(launcher, HELLO_TIMEOUT, CALL_TIMEOUT)
    }

    /// The same with other timeouts (tests).
    pub fn with_timeouts(launcher: Arc<dyn Launcher>, hello: Duration, call: Duration) -> Self {
        Self {
            state: Arc::new(Mutex::new(State {
                live: None,
                failures: 0,
                given_up: false,
                generation: 0,
            })),
            launcher,
            hello_timeout: hello,
            call_timeout: call,
        }
    }

    fn state(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Whether the helper has been given up on for good.
    pub fn given_up(&self) -> bool {
        self.state().given_up
    }

    /// Starts the helper if it is not running and says hello.
    fn ensure_started(&self, state: &mut State) -> Result<(), Lost> {
        if state.given_up {
            return Err(Lost("the helper was given up".into()));
        }
        if state.live.is_some() {
            return Ok(());
        }
        let link = match self.launcher.launch() {
            Ok(link) => link,
            Err(error) => {
                // Not there or not runnable: trying again will not help.
                log::warn!("video: the helper did not start ({error}): no video");
                state.given_up = true;
                return Err(Lost(error.to_string()));
            }
        };
        state.live = Some(start(link));
        let hello = Request::Hello {
            version: ipc::VERSION,
        };
        match self.exchange(state, hello, self.hello_timeout) {
            Ok(Reply::Welcome {
                version,
                backend,
                capabilities,
            }) if version == ipc::VERSION => {
                log::info!(
                    "video: helper back end {backend}; GPU: {}",
                    if capabilities.is_empty() {
                        "none".to_owned()
                    } else {
                        capabilities
                            .iter()
                            .map(|c| {
                                format!(
                                    "{:?} {:?} up to {}x{}",
                                    c.codec, c.direction, c.max_width, c.max_height
                                )
                            })
                            .collect::<Vec<_>>()
                            .join(", ")
                    }
                );
                Ok(())
            }
            Ok(Reply::Welcome { version, .. }) => {
                log::warn!(
                    "video: the helper speaks version {version}, not {}: no video",
                    ipc::VERSION
                );
                state.live = None;
                state.given_up = true;
                Err(Lost("another version".into()))
            }
            Ok(other) => Err(self.fail(state, &format!("an answer to hello: {other:?}"))),
            Err(lost) => Err(lost),
        }
    }

    /// Sends `request` and waits for its reply; a missing, late or
    /// unreadable reply counts as a crash.
    fn exchange(
        &self,
        state: &mut State,
        request: Request,
        timeout: Duration,
    ) -> Result<Reply, Lost> {
        let Some(live) = state.live.as_mut() else {
            return Err(Lost("not running".into()));
        };
        live.seq = live.seq.wrapping_add(1);
        let seq = live.seq;
        if live.requests.send((seq, request)).is_err() {
            return Err(self.fail(state, "its input closed"));
        }
        match live.replies.recv_timeout(timeout) {
            Ok(Ok((got, reply))) if got == seq => Ok(reply),
            Ok(Ok((got, _))) => Err(self.fail(state, &format!("reply {got} to request {seq}"))),
            Ok(Err(why)) => Err(self.fail(state, &why)),
            Err(RecvTimeoutError::Timeout) => {
                Err(self.fail(state, &format!("no reply in {} ms", timeout.as_millis())))
            }
            Err(RecvTimeoutError::Disconnected) => Err(self.fail(state, "it ended")),
        }
    }

    /// The helper failed: it is stopped, and started again next time
    /// unless it has failed too often.
    fn fail(&self, state: &mut State, why: &str) -> Lost {
        state.live = None;
        state.generation += 1;
        state.failures += 1;
        if state.failures > MAX_RESTARTS {
            state.given_up = true;
            log::warn!(
                "video: the helper failed ({why}), {} times now: no more video from it",
                state.failures
            );
        } else {
            log::warn!("video: the helper failed ({why}); it starts again for the next stream");
        }
        Lost(why.to_owned())
    }

    /// One request to a running helper, as part of decoder `generation`.
    fn call(&self, generation: u64, request: Request) -> Result<Reply, Lost> {
        let mut state = self.state();
        if state.generation != generation || state.live.is_none() {
            return Err(Lost("the helper restarted".into()));
        }
        self.exchange(&mut state, request, self.call_timeout)
    }

    /// Opens a decoder for a `codec` stream of about `width`×`height`, on
    /// the GPU first if `gpu` (the helper decodes in software whatever
    /// the GPU cannot).
    pub fn open_decoder(
        &self,
        codec: Codec,
        width: u32,
        height: u32,
        gpu: bool,
    ) -> Result<RemoteDecoder, Lost> {
        let mut state = self.state();
        self.ensure_started(&mut state)?;
        let request = Request::OpenDecoder {
            codec,
            width,
            height,
            hardware: gpu,
        };
        let generation = state.generation;
        match self.exchange(&mut state, request, self.call_timeout)? {
            Reply::Opened { id } => Ok(RemoteDecoder {
                helper: self.clone(),
                id,
                generation,
            }),
            // Too many open: this stream has none, the helper is fine.
            Reply::Failed { kind, detail } => Err(Lost(format!("{kind:?}: {detail}"))),
            other => Err(self.fail(&mut state, &format!("an answer to open: {other:?}"))),
        }
    }

    /// What can be shared: `(dialog, sources)`, `dialog` when the system
    /// shows its own when the share starts. Starts the helper if it is
    /// not running.
    #[cfg(feature = "huddle-share")]
    pub fn sources(&self) -> Result<(bool, Vec<ipc::Source>), CaptureTrouble> {
        self.list(Request::ListSources)
    }

    /// The cameras there are (none is opened to ask). Starts the helper
    /// if it is not running.
    #[cfg(feature = "huddle-camera")]
    pub fn cameras(&self) -> Result<Vec<ipc::Source>, CaptureTrouble> {
        self.list(Request::ListCameras).map(|(_, cameras)| cameras)
    }

    /// Asks for a list of sources: what can be shared, or the cameras.
    #[cfg(feature = "huddle-camera")]
    fn list(&self, request: Request) -> Result<(bool, Vec<ipc::Source>), CaptureTrouble> {
        let mut state = self.state();
        self.ensure_started(&mut state)
            .map_err(CaptureTrouble::lost)?;
        match self
            .exchange(&mut state, request, SOURCES_TIMEOUT)
            .map_err(CaptureTrouble::lost)?
        {
            Reply::Sources { dialog, sources } => Ok((dialog, sources)),
            Reply::Problem { problem, detail } => Err(CaptureTrouble::Problem(problem, detail)),
            other => Err(CaptureTrouble::lost(
                self.fail(&mut state, &format!("an answer to the sources: {other:?}")),
            )),
        }
    }

    /// Starts capturing and encoding `choice` (on the GPU if `gpu` and it
    /// can), at `bitrate` bit/s to begin with; `restore` is the portal's
    /// token from the last share (empty: none). The share, and the token
    /// to give next time. Waits as long as the system's dialog is open.
    #[cfg(feature = "huddle-share")]
    pub fn start_share(
        &self,
        choice: ipc::ShareChoice,
        gpu: bool,
        bitrate: u32,
        restore: &str,
    ) -> Result<(RemoteCapture, String), CaptureTrouble> {
        let request = Request::StartShare {
            choice,
            hardware: gpu,
            bitrate,
            restore: restore.to_owned(),
        };
        self.start(request, SHARE_START_TIMEOUT)
    }

    /// Opens the camera `choice` names and starts encoding it (on the GPU
    /// if `gpu` and it can) at `bitrate` bit/s to begin with, each new
    /// picture's frame carrying a self-view at most `preview` pixels
    /// wide. Waits while the system asks the user for the camera.
    #[cfg(feature = "huddle-camera")]
    pub fn start_camera(
        &self,
        choice: ipc::CameraChoice,
        gpu: bool,
        bitrate: u32,
        preview: u32,
    ) -> Result<RemoteCapture, CaptureTrouble> {
        let request = Request::StartCamera {
            choice,
            hardware: gpu,
            bitrate,
            preview: preview.min(ipc::MAX_PREVIEW_SIDE),
        };
        self.start(request, CAMERA_START_TIMEOUT)
            .map(|(camera, _)| camera)
    }

    /// Starts a capture with `request`, waiting up to `timeout` for it.
    #[cfg(feature = "huddle-camera")]
    fn start(
        &self,
        request: Request,
        timeout: Duration,
    ) -> Result<(RemoteCapture, String), CaptureTrouble> {
        let mut state = self.state();
        self.ensure_started(&mut state)
            .map_err(CaptureTrouble::lost)?;
        let generation = state.generation;
        match self
            .exchange(&mut state, request, timeout)
            .map_err(CaptureTrouble::lost)?
        {
            Reply::Started { id, restore } => Ok((
                RemoteCapture {
                    helper: self.clone(),
                    id,
                    generation,
                },
                restore,
            )),
            Reply::Problem { problem, detail } => Err(CaptureTrouble::Problem(problem, detail)),
            // Too many open: the helper is fine.
            Reply::Failed { kind, detail } => Err(CaptureTrouble::Problem(
                ipc::CaptureProblem::Failed,
                format!("{kind:?}: {detail}"),
            )),
            other => Err(CaptureTrouble::lost(
                self.fail(&mut state, &format!("an answer to the start: {other:?}")),
            )),
        }
    }
}

/// How long the helper may take to list what can be shared or the
/// cameras: windows, screens and devices are asked of the system, which
/// can take a moment.
#[cfg(feature = "huddle-camera")]
const SOURCES_TIMEOUT: Duration = Duration::from_secs(10);
/// How long starting a share may take: the system's dialog waits for
/// the user. Past this the helper is taken for stuck, and stopping it
/// closes the dialog.
#[cfg(feature = "huddle-share")]
const SHARE_START_TIMEOUT: Duration = Duration::from_secs(300);
/// How long opening the camera may take: macOS asks the user the first
/// time, and the helper waits a minute for the answer.
#[cfg(feature = "huddle-camera")]
const CAMERA_START_TIMEOUT: Duration = Duration::from_secs(75);

/// Why a capture (a share or the camera) did not start, or stopped.
#[cfg(feature = "huddle-camera")]
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CaptureTrouble {
    /// What the helper said: cancelled, not allowed, busy, gone, ended…
    Problem(ipc::CaptureProblem, String),
    /// The helper is not there, failed or stopped answering.
    Lost(String),
}

#[cfg(feature = "huddle-camera")]
impl CaptureTrouble {
    fn lost(lost: Lost) -> Self {
        Self::Lost(lost.0)
    }
}

/// What we send, captured and encoded in the helper: our screen share or
/// our camera. Stopped (its capture with it, a camera closed) when
/// dropped.
#[cfg(feature = "huddle-camera")]
#[derive(Debug)]
pub struct RemoteCapture {
    helper: Helper,
    id: u32,
    generation: u64,
}

#[cfg(feature = "huddle-camera")]
impl RemoteCapture {
    /// The next picture, encoded (see `Request::NextFrame`): none when
    /// nothing new came within `wait`. Its NAL units are checked: a
    /// slice, and an IDR with its parameter sets when one was asked; its
    /// self-view, if any, was checked against its size as it was read.
    pub fn next(
        &mut self,
        force_keyframe: bool,
        repeat: bool,
        wait: Duration,
    ) -> Result<Option<ipc::CapturedFrame>, CaptureTrouble> {
        let request = Request::NextFrame {
            id: self.id,
            force_keyframe,
            repeat,
            wait_ms: u32::try_from(wait.as_millis())
                .unwrap_or(u32::MAX)
                .min(ipc::MAX_WAIT_MS),
        };
        match self
            .helper
            .call(self.generation, request)
            .map_err(CaptureTrouble::lost)?
        {
            Reply::Frame(frame) => {
                let types = ipc::h264::nal_types(&frame.data);
                let keyframe = types.contains(&5);
                let whole = types.iter().any(|&t| t == 1 || t == 5)
                    && keyframe == frame.keyframe
                    && (!keyframe || types.starts_with(&[7, 8]))
                    && (keyframe || !force_keyframe);
                if whole {
                    Ok(Some(frame))
                } else {
                    Err(CaptureTrouble::Lost(format!(
                        "the helper's frame is not what was asked (NAL units {types:?})"
                    )))
                }
            }
            Reply::NoPicture => Ok(None),
            Reply::Problem { problem, detail } => Err(CaptureTrouble::Problem(problem, detail)),
            other => Err(CaptureTrouble::Lost(format!(
                "an answer to the next frame: {other:?}"
            ))),
        }
    }

    /// Aims at `bitrate` bit/s from the next picture on.
    pub fn set_bitrate(&mut self, bitrate: u32) -> Result<(), CaptureTrouble> {
        let request = Request::SetBitrate {
            id: self.id,
            bitrate,
        };
        match self
            .helper
            .call(self.generation, request)
            .map_err(CaptureTrouble::lost)?
        {
            Reply::Done => Ok(()),
            other => Err(CaptureTrouble::Lost(format!(
                "an answer to the bit rate: {other:?}"
            ))),
        }
    }

    /// Fits the pictures within `max` from the next picture on, a
    /// keyframe first; none lifts the box.
    pub fn set_max_size(&mut self, max: Option<(u32, u32)>) -> Result<(), CaptureTrouble> {
        let (width, height) = max.unwrap_or((0, 0));
        let request = Request::SetMaxSize {
            id: self.id,
            width,
            height,
        };
        match self
            .helper
            .call(self.generation, request)
            .map_err(CaptureTrouble::lost)?
        {
            Reply::Done => Ok(()),
            other => Err(CaptureTrouble::Lost(format!(
                "an answer to the picture size: {other:?}"
            ))),
        }
    }
}

#[cfg(feature = "huddle-camera")]
impl Drop for RemoteCapture {
    fn drop(&mut self) {
        // The helper stops the capture; one that restarted has stopped
        // it already.
        let _ = self
            .helper
            .call(self.generation, Request::Close { id: self.id });
    }
}

/// Starts the threads that write to and read from a launched helper.
fn start(link: Link) -> Live {
    let Link {
        input,
        output,
        stop,
    } = link;
    // Requests wait for their reply, so at most one is ever queued.
    let (requests, queued) = mpsc::sync_channel::<(u32, Request)>(1);
    let (answer, replies) = mpsc::sync_channel(1);
    let _ = std::thread::Builder::new()
        .name("video-helper-in".into())
        .spawn(move || {
            let mut input = BufWriter::new(input);
            for (seq, request) in queued {
                if ipc::write_request(&mut input, seq, &request).is_err() {
                    break;
                }
            }
        });
    let _ = std::thread::Builder::new()
        .name("video-helper-out".into())
        .spawn(move || {
            let mut output = BufReader::with_capacity(1 << 16, output);
            loop {
                let reply = match ipc::read_reply(&mut output) {
                    Ok(Some(reply)) => Ok(reply),
                    Ok(None) => Err("it ended".to_owned()),
                    Err(error) => Err(format!("an unreadable reply: {error}")),
                };
                let last = reply.is_err();
                if answer.send(reply).is_err() || last {
                    break;
                }
            }
        });
    Live {
        requests,
        replies,
        stop,
        seq: 0,
    }
}

/// Why the helper gave no decoded picture.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HelperTrouble {
    /// Wait for a keyframe (a loss, or joined mid-stream).
    NeedKeyframe,
    /// The frame did not decode; the next keyframe starts over.
    Broken(String),
    /// The helper cannot decode this stream.
    Unsupported(String),
    /// The helper failed or went away: the stream starts again in the
    /// next helper at a keyframe.
    Lost(String),
}

/// A picture back from the helper, checked.
#[cfg(feature = "huddle-video")]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Picture {
    /// The picture, at most as large as the size shown asks
    /// ([`RemoteDecoder::set_output_size`]).
    pub yuv: ipc::Planes,
    /// The stream's own size, before any shrinking.
    pub source: [usize; 2],
    /// Whether the GPU decoded it.
    pub gpu: bool,
}

/// What a frame, or a fetch, gave.
#[cfg(feature = "huddle-video")]
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Outcome {
    /// A new picture.
    Picture(Picture),
    /// Its picture is the one the helper sent last: the window shows
    /// what it has.
    Unchanged,
    /// Decoded, its picture kept back in the helper for a fetch
    /// ([`RemoteDecoder::fetch`]), as asked.
    Kept,
    /// No picture: a frame of parameter sets only, or nothing kept back
    /// to fetch.
    Nothing,
}

#[cfg(feature = "huddle-video")]
impl Outcome {
    /// The new picture, if it is one.
    pub fn picture(self) -> Option<Picture> {
        match self {
            Self::Picture(picture) => Some(picture),
            _ => None,
        }
    }
}

/// One stream's decoder in the helper. Closed when dropped.
#[derive(Debug)]
pub struct RemoteDecoder {
    helper: Helper,
    id: u32,
    generation: u64,
}

impl RemoteDecoder {
    /// Decodes one frame (an access unit, Annex B). Its picture comes
    /// back if `show`; otherwise the helper keeps it back (it is not
    /// shrunk, read back from the GPU or sent) for a [`Self::fetch`]
    /// until the next frame.
    #[cfg(feature = "huddle-video")]
    pub fn decode(
        &mut self,
        unit: &[u8],
        keyframe: bool,
        show: bool,
    ) -> Result<Outcome, HelperTrouble> {
        let request = Request::Decode {
            id: self.id,
            keyframe,
            show,
            data: unit.to_vec(),
        };
        self.answer(request)
    }

    /// The picture the helper kept back from the last frame, if it has
    /// not been sent since: the window can take a picture again.
    #[cfg(feature = "huddle-video")]
    pub fn fetch(&mut self) -> Result<Outcome, HelperTrouble> {
        self.answer(Request::Fetch { id: self.id })
    }

    /// Sends `request` (a decode or a fetch) and reads its answer.
    #[cfg(feature = "huddle-video")]
    fn answer(&mut self, request: Request) -> Result<Outcome, HelperTrouble> {
        match self.helper.call(self.generation, request) {
            Ok(Reply::Picture(decoded)) => {
                // Checked as it was read: the planes against their size,
                // which is no larger than the source, itself no larger
                // than any picture may be.
                let size = |n: u32| usize::try_from(n).unwrap_or(0);
                Ok(Outcome::Picture(Picture {
                    yuv: decoded.planes,
                    source: [size(decoded.source.0), size(decoded.source.1)],
                    gpu: decoded.hardware,
                }))
            }
            Ok(Reply::NoPicture) => Ok(Outcome::Nothing),
            Ok(Reply::Kept) => Ok(Outcome::Kept),
            Ok(Reply::Unchanged) => Ok(Outcome::Unchanged),
            Ok(Reply::Failed { kind, detail }) => Err(match kind {
                FailKind::NeedKeyframe => HelperTrouble::NeedKeyframe,
                FailKind::Broken => HelperTrouble::Broken(detail),
                FailKind::Unsupported => HelperTrouble::Unsupported(detail),
                FailKind::Device | FailKind::Protocol | FailKind::UnknownId => {
                    HelperTrouble::Lost(format!("{kind:?}: {detail}"))
                }
            }),
            Ok(other) => Err(HelperTrouble::Lost(format!(
                "an answer to decode: {other:?}"
            ))),
            Err(Lost(why)) => Err(HelperTrouble::Lost(why)),
        }
    }

    /// Asks the helper for pictures shrunk to cover `width`×`height`
    /// (0×0: their own size).
    pub fn set_output_size(&mut self, width: u32, height: u32) -> Result<(), HelperTrouble> {
        let request = Request::SetOutputSize {
            id: self.id,
            width,
            height,
        };
        match self.helper.call(self.generation, request) {
            Ok(Reply::Done) => Ok(()),
            Ok(other) => Err(HelperTrouble::Lost(format!(
                "an answer to the output size: {other:?}"
            ))),
            Err(Lost(why)) => Err(HelperTrouble::Lost(why)),
        }
    }
}

impl Drop for RemoteDecoder {
    fn drop(&mut self) {
        // Best effort: a helper that restarted has forgotten it anyway.
        let _ = self
            .helper
            .call(self.generation, Request::Close { id: self.id });
    }
}

/// The helper as a test sees it: what it does with each request; or the
/// helper's own code, on a thread.
#[cfg(test)]
#[cfg_attr(not(feature = "huddle-video"), allow(dead_code))]
pub(crate) mod pretend {
    use super::*;

    /// Runs the helper's own server, as the program would, on a thread
    /// and over pipes: with no GPU back end, so it decodes and encodes in
    /// software, and only the test screen and test camera to capture.
    pub struct InThread;

    impl Launcher for InThread {
        fn launch(&self) -> std::io::Result<Link> {
            let (from_app, to_helper) = std::io::pipe()?;
            let (from_helper, to_app) = std::io::pipe()?;
            std::thread::Builder::new()
                .name("video-helper-in-thread".into())
                .spawn(move || {
                    let mut input = std::io::BufReader::new(from_app);
                    let mut output = BufWriter::new(to_app);
                    let mut backend = noslacking_video::backend::Nothing::new("none: a test");
                    // Only the test screen, and a few pretend sources:
                    // no test captures a real one.
                    let mut screens = noslacking_video::capture::Pretend {
                        sources: pretend::sources(),
                    };
                    // And a pretend camera: the test camera too.
                    let mut cameras = noslacking_video::capture::camera::Pretend {
                        cameras: vec![ipc::Source {
                            id: "pretend:camera".into(),
                            name: "A pretend camera".into(),
                            kind: ipc::SourceKind::Camera,
                        }],
                        refuse: None,
                    };
                    let _ = noslacking_video::server::serve(
                        &mut input,
                        &mut output,
                        &mut backend,
                        noslacking_video::server::Sources {
                            screens: &mut screens,
                            cameras: &mut cameras,
                        },
                    );
                })?;
            Ok(Link {
                input: Box::new(to_helper),
                output: Box::new(from_helper),
                stop: Box::new(|| {}),
            })
        }
    }

    /// The screens and windows the helper on a thread offers to share,
    /// all of them the test screen.
    pub fn sources() -> Vec<ipc::Source> {
        vec![
            ipc::Source {
                id: "pretend:1".into(),
                name: "A pretend screen".into(),
                kind: ipc::SourceKind::Screen,
            },
            ipc::Source {
                id: "pretend:2".into(),
                name: "A pretend window".into(),
                kind: ipc::SourceKind::Window,
            },
        ]
    }

    /// What the pretend helper does with a request.
    pub enum Act {
        /// Answers with this.
        Reply(Reply),
        /// Writes these raw bytes as its answer.
        Raw(Vec<u8>),
        /// Never answers.
        Hang,
        /// Ends, as a crash would.
        Crash,
    }

    type Script = dyn Fn(&Request) -> Act + Send + Sync;

    /// Launches a pretend helper on a thread; counts its launches.
    pub struct Pretend {
        pub script: Arc<Script>,
        pub launches: Arc<std::sync::atomic::AtomicU32>,
    }

    impl Pretend {
        pub fn new(script: impl Fn(&Request) -> Act + Send + Sync + 'static) -> Self {
            Self {
                script: Arc::new(script),
                launches: Arc::default(),
            }
        }
    }

    /// A welcome listing H.264 decoding and encoding up to 1920×1088.
    pub fn welcome() -> Reply {
        Reply::Welcome {
            version: ipc::VERSION,
            backend: "pretend".into(),
            capabilities: [ipc::Direction::Decode, ipc::Direction::Encode]
                .into_iter()
                .map(|direction| ipc::Capability {
                    codec: Codec::H264,
                    direction,
                    max_width: 1920,
                    max_height: 1088,
                })
                .collect(),
        }
    }

    /// A grey picture.
    pub fn picture(width: u32, height: u32, grey: u8) -> Reply {
        let (cw, ch) = ipc::chroma_size(width, height);
        let n = |w: u32, h: u32| usize::try_from(w * h).unwrap_or(0);
        Reply::Picture(ipc::Decoded {
            planes: ipc::Planes {
                width,
                height,
                y: vec![grey; n(width, height)],
                u: vec![128; n(cw, ch)],
                v: vec![128; n(cw, ch)],
            },
            source: (width, height),
            hardware: true,
        })
    }

    impl Launcher for Pretend {
        fn launch(&self) -> std::io::Result<Link> {
            self.launches.fetch_add(1, Ordering::Relaxed);
            let (from_app, to_helper) = std::io::pipe()?;
            let (from_helper, to_app) = std::io::pipe()?;
            let script = Arc::clone(&self.script);
            std::thread::spawn(move || {
                let mut input = from_app;
                let mut output = to_app;
                while let Ok(Some(frame)) = ipc::read_frame(&mut input) {
                    let Ok(request) = Request::decode(&frame.body) else {
                        break;
                    };
                    match script(&request) {
                        Act::Reply(reply) => {
                            if ipc::write_reply(&mut output, frame.seq, &reply).is_err() {
                                break;
                            }
                        }
                        Act::Raw(bytes) => {
                            let _ = output.write_all(&bytes);
                        }
                        Act::Hang => {
                            // Holds its pipes open, answering nothing,
                            // until the app gives up and closes them.
                            while let Ok(Some(_)) = ipc::read_frame(&mut input) {}
                            break;
                        }
                        Act::Crash => break,
                    }
                }
            });
            Ok(Link {
                input: Box::new(to_helper),
                output: Box::new(from_helper),
                stop: Box::new(|| {}),
            })
        }
    }
}

#[cfg(all(test, feature = "huddle-video"))]
mod tests {
    use super::pretend::{Act, Pretend, picture, welcome};
    use super::*;
    use std::sync::atomic::AtomicU32;

    fn helper(pretend: Pretend) -> (Helper, Arc<AtomicU32>) {
        let launches = Arc::clone(&pretend.launches);
        let helper = Helper::with_timeouts(
            Arc::new(pretend),
            Duration::from_secs(5),
            Duration::from_millis(300),
        );
        (helper, launches)
    }

    /// Answers hello, opens decoder 1 and decodes every frame to a grey
    /// 64×48 picture.
    fn working(request: &Request) -> Act {
        match request {
            Request::Hello { .. } => Act::Reply(welcome()),
            Request::OpenDecoder { .. } => Act::Reply(Reply::Opened { id: 1 }),
            Request::Decode { .. } => Act::Reply(picture(64, 48, 77)),
            _ => Act::Reply(Reply::Done),
        }
    }

    const KEYFRAME: &[u8] = &[0, 0, 0, 1, 0x65];

    #[test]
    fn pictures_come_back_from_the_helper() {
        let asked = Arc::new(Mutex::new(Vec::new()));
        let seen = Arc::clone(&asked);
        let (helper, launches) = helper(Pretend::new(move |request| {
            if let Request::OpenDecoder { hardware, .. } = request {
                seen.lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .push(*hardware);
            }
            working(request)
        }));
        let mut decoder = helper
            .open_decoder(Codec::H264, 64, 48, true)
            .expect("opens");
        let picture = decoder
            .decode(KEYFRAME, true, true)
            .expect("decodes")
            .picture()
            .expect("a picture");
        assert_eq!(
            (picture.yuv.width, picture.yuv.height, picture.yuv.y[0]),
            (64, 48, 77)
        );
        assert_eq!(picture.source, [64, 48]);
        assert!(picture.gpu);
        drop(decoder);
        // The GPU, or not, as asked.
        drop(helper.open_decoder(Codec::H264, 64, 48, false));
        assert_eq!(
            *asked.lock().unwrap_or_else(PoisonError::into_inner),
            [true, false]
        );
        assert_eq!(launches.load(Ordering::Relaxed), 1, "started once");
    }

    #[test]
    fn a_crashed_helper_starts_again_a_few_times_then_is_given_up() {
        let pretend = Pretend::new(|request| match request {
            Request::Decode { .. } => Act::Crash,
            other => working(other),
        });
        let (helper, launches) = helper(pretend);
        for round in 1..=MAX_RESTARTS + 1 {
            let mut decoder = helper
                .open_decoder(Codec::H264, 64, 48, true)
                .expect("opens");
            assert!(matches!(
                decoder.decode(KEYFRAME, true, true),
                Err(HelperTrouble::Lost(_))
            ));
            assert_eq!(launches.load(Ordering::Relaxed), round);
            // The old decoder is gone with its helper.
            assert!(matches!(
                decoder.decode(KEYFRAME, true, true),
                Err(HelperTrouble::Lost(_))
            ));
        }
        assert!(helper.given_up());
        assert!(helper.open_decoder(Codec::H264, 64, 48, true).is_err());
        assert_eq!(
            launches.load(Ordering::Relaxed),
            MAX_RESTARTS + 1,
            "not started again"
        );
    }

    #[test]
    fn a_stuck_helper_times_out_and_is_replaced() {
        let hung = Arc::new(AtomicBool::new(true));
        let once = Arc::clone(&hung);
        let pretend = Pretend::new(move |request| match request {
            Request::Decode { .. } if once.swap(false, Ordering::Relaxed) => Act::Hang,
            other => working(other),
        });
        let (helper, launches) = helper(pretend);
        let mut decoder = helper
            .open_decoder(Codec::H264, 64, 48, true)
            .expect("opens");
        let started = std::time::Instant::now();
        assert!(matches!(
            decoder.decode(KEYFRAME, true, true),
            Err(HelperTrouble::Lost(_))
        ));
        assert!(
            started.elapsed() < Duration::from_secs(3),
            "the timeout, not forever"
        );
        assert!(!hung.load(Ordering::Relaxed));
        // The next stream gets a new helper, which works.
        let mut decoder = helper
            .open_decoder(Codec::H264, 64, 48, true)
            .expect("opens again");
        assert!(decoder.decode(KEYFRAME, true, true).is_ok());
        assert_eq!(launches.load(Ordering::Relaxed), 2);
    }

    #[test]
    fn pictures_whose_planes_do_not_match_are_refused() {
        // A picture frame from a 4×4 source claiming 4×4 with a 3-byte
        // luma plane.
        let mut lying = Vec::new();
        let body_length = 4 + 1 + 8 + 1 + 8 + 4 + 3 + 4 + 4 + 4 + 4;
        lying.extend_from_slice(&u32::to_le_bytes(body_length));
        lying.extend_from_slice(&2u32.to_le_bytes());
        lying.push(3);
        lying.extend_from_slice(&4u32.to_le_bytes());
        lying.extend_from_slice(&4u32.to_le_bytes());
        lying.push(0);
        lying.extend_from_slice(&4u32.to_le_bytes());
        lying.extend_from_slice(&4u32.to_le_bytes());
        lying.extend_from_slice(&3u32.to_le_bytes());
        lying.extend_from_slice(&[1, 2, 3]);
        lying.extend_from_slice(&4u32.to_le_bytes());
        lying.extend_from_slice(&[0; 4]);
        lying.extend_from_slice(&4u32.to_le_bytes());
        lying.extend_from_slice(&[0; 4]);
        let pretend = Pretend::new(move |request| match request {
            Request::Decode { .. } => Act::Raw(lying.clone()),
            other => working(other),
        });
        let (lying, _) = helper(pretend);
        let mut decoder = lying.open_decoder(Codec::H264, 4, 4, true).expect("opens");
        assert!(matches!(
            decoder.decode(KEYFRAME, true, true),
            Err(HelperTrouble::Lost(_))
        ));
        // A well-formed picture larger than the stream it says it came
        // from.
        let pretend = Pretend::new(|request| match request {
            Request::Decode { .. } => {
                let Reply::Picture(decoded) = picture(64, 48, 0) else {
                    return Act::Crash;
                };
                Act::Reply(Reply::Picture(ipc::Decoded {
                    source: (32, 24),
                    ..decoded
                }))
            }
            other => working(other),
        });
        let (larger, _) = helper(pretend);
        let mut decoder = larger
            .open_decoder(Codec::H264, 64, 48, true)
            .expect("opens");
        assert!(matches!(
            decoder.decode(KEYFRAME, true, true),
            Err(HelperTrouble::Lost(_))
        ));
    }

    #[test]
    fn a_helper_of_another_version_or_none_at_all_is_given_up() {
        let pretend = Pretend::new(|request| match request {
            Request::Hello { .. } => Act::Reply(Reply::Welcome {
                version: ipc::VERSION + 1,
                backend: "future".into(),
                capabilities: Vec::new(),
            }),
            other => working(other),
        });
        let (helper, launches) = helper(pretend);
        assert!(helper.open_decoder(Codec::H264, 64, 48, true).is_err());
        assert!(helper.given_up());
        assert!(helper.open_decoder(Codec::H264, 64, 48, true).is_err());
        assert_eq!(launches.load(Ordering::Relaxed), 1);
        struct Missing;
        impl Launcher for Missing {
            fn launch(&self) -> std::io::Result<Link> {
                Err(std::io::Error::from(std::io::ErrorKind::NotFound))
            }
        }
        let helper = Helper::new(Arc::new(Missing));
        assert!(helper.open_decoder(Codec::H264, 64, 48, true).is_err());
        assert!(helper.given_up());
    }

    #[test]
    fn helper_failures_map_to_what_the_stream_does_next() {
        let pretend = Pretend::new(|request| match request {
            Request::Decode {
                keyframe: false, ..
            } => Act::Reply(Reply::Failed {
                kind: FailKind::NeedKeyframe,
                detail: "joined late".into(),
            }),
            Request::Decode { .. } => Act::Reply(Reply::Failed {
                kind: FailKind::Broken,
                detail: "bad slice".into(),
            }),
            other => working(other),
        });
        let (helper, launches) = helper(pretend);
        let mut decoder = helper
            .open_decoder(Codec::H264, 64, 48, true)
            .expect("opens");
        assert_eq!(
            decoder.decode(&[0, 0, 1, 0x41], false, true),
            Err(HelperTrouble::NeedKeyframe)
        );
        assert!(matches!(
            decoder.decode(&[0, 0, 1, 0x65, 0], true, true),
            Err(HelperTrouble::Broken(_))
        ));
        // Neither was the helper's fault: it is still the first.
        assert_eq!(launches.load(Ordering::Relaxed), 1);
        assert!(!helper.given_up());
    }

    /// Each lane has a helper of its own, and tests get the helper's own
    /// code on a thread, decoding in software.
    #[test]
    fn each_lane_has_its_own_helper() {
        let share = shared(Lane::Share).expect("in tests, always");
        let cameras = shared(Lane::Cameras).expect("in tests, always");
        assert!(Arc::ptr_eq(
            &share.state,
            &shared(Lane::Share).expect("again").state
        ));
        assert!(!Arc::ptr_eq(&share.state, &cameras.state));
        let frames = ipc::h264::access_units(include_bytes!("fixtures/camera-480x480.h264"));
        let mut decoder = cameras
            .open_decoder(Codec::H264, 480, 480, true)
            .expect("opens");
        decoder.set_output_size(240, 180).expect("told");
        let picture = decoder
            .decode(&frames[0], true, true)
            .expect("decodes")
            .picture()
            .expect("a picture");
        assert_eq!(picture.source, [480, 480]);
        assert_eq!((picture.yuv.width, picture.yuv.height), (240, 240));
        assert!(!picture.gpu, "no GPU in tests: software");
    }

    #[test]
    fn the_helper_is_looked_for_beside_the_app() {
        // The test binary's folder has no helper in it, so whatever is
        // found came from PATH; either way, a file.
        if let Some(path) = find_helper() {
            assert!(path.is_file());
            assert!(
                std::path::Path::new(&path)
                    .ends_with(format!("{HELPER}{}", std::env::consts::EXE_SUFFIX))
            );
        }
    }
}
