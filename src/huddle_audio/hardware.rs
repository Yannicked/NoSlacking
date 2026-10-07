//! Video decoded and encoded on the GPU (the `huddle-video` and
//! `huddle-camera` features), through `noslacking-video`, a helper
//! process (crates/noslacking-video).
//!
//! The platform video APIs are C libraries and GPU drivers, which take
//! `unsafe` code this crate forbids, and a driver handed a stranger's
//! malformed stream may crash or hang. So they run in the helper, which
//! this module starts the first time a stream could use it and talks to
//! over its standard input and output (`noslacking-video-ipc`'s
//! messages). The helper is found next to this program, else on `PATH`;
//! without it there is no hardware video, and nothing else changes.
//!
//! Software (rusty_h264: `decode` for streams in, `video_encoder` for our
//! camera out) is always the fallback: hardware is used only for what the
//! helper says it can do. A reply that does not come within a timeout
//! counts as a crash; after a crash the helper is killed and started
//! again on the next stream, at most [`MAX_RESTARTS`] times, and then
//! never again until the app restarts. A stream that lost it asks for a
//! keyframe and goes on in software; our camera goes on in software with
//! a keyframe. Nothing from the helper is trusted: replies are checked
//! field by field, a picture's planes against its size, before use.

use std::io::{BufReader, BufWriter, Read, Write};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, SyncSender};
use std::sync::{Arc, Mutex, OnceLock, PoisonError};
use std::time::Duration;

use noslacking_video_ipc::{self as ipc, Capability, Codec, Direction, FailKind, Reply, Request};

#[cfg(feature = "huddle-video")]
use super::decode::Yuv;

/// How many times a crashed or stuck helper is started again before
/// hardware decoding is given up until the app restarts.
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
static ENABLED: AtomicBool = AtomicBool::new(false);

/// Turns hardware video on or off for streams (and our camera) that
/// start from now on.
pub fn set_enabled(enabled: bool) {
    ENABLED.store(enabled, Ordering::Relaxed);
}

/// Whether streams starting now may decode, and our camera encode, on
/// the GPU.
pub fn enabled() -> bool {
    ENABLED.load(Ordering::Relaxed)
}

/// Why the helper cannot serve a request: the stream goes on in
/// software.
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

/// The process-wide helper, made the first time it is asked for; none
/// when the program is not installed.
pub fn shared() -> Option<Helper> {
    // Tests never start the real helper: it would open the GPU.
    if cfg!(test) {
        return None;
    }
    static SHARED: OnceLock<Option<Helper>> = OnceLock::new();
    SHARED
        .get_or_init(|| match find_helper() {
            Some(path) => {
                log::info!("video: hardware decoding through {}", path.display());
                Some(Helper::new(Arc::new(ProcessLauncher::new(path))))
            }
            None => {
                log::info!("video: no {HELPER} beside the app or on PATH: software decoding");
                None
            }
        })
        .clone()
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
    capabilities: Vec<Capability>,
}

/// The app's side of the helper, shared by every decoder (cheap to
/// clone). Requests go one at a time.
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
                capabilities: Vec::new(),
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

    /// Whether the helper decodes `codec` at `width`×`height`, starting
    /// it if it is not running.
    pub fn decodes(&self, codec: Codec, width: u32, height: u32) -> bool {
        self.can(codec, Direction::Decode, width, height)
    }

    /// Whether the helper encodes `codec` (for H.264, constrained
    /// baseline) at `width`×`height`, starting it if it is not running.
    pub fn encodes(&self, codec: Codec, width: u32, height: u32) -> bool {
        self.can(codec, Direction::Encode, width, height)
    }

    fn can(&self, codec: Codec, direction: Direction, width: u32, height: u32) -> bool {
        let mut state = self.state();
        if self.ensure_started(&mut state).is_err() {
            return false;
        }
        state
            .capabilities
            .iter()
            .any(|c| c.covers(codec, direction, width, height))
    }

    /// Starts the helper if it is not running and says hello.
    fn ensure_started(&self, state: &mut State) -> Result<(), Lost> {
        if state.given_up {
            return Err(Lost("hardware decoding was given up".into()));
        }
        if state.live.is_some() {
            return Ok(());
        }
        let link = match self.launcher.launch() {
            Ok(link) => link,
            Err(error) => {
                // Not there or not runnable: trying again will not help.
                log::info!("video: the helper did not start ({error}): software decoding");
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
                    "video: helper back end {backend}; {} hardware capabilities",
                    capabilities.len()
                );
                state.capabilities = capabilities;
                Ok(())
            }
            Ok(Reply::Welcome { version, .. }) => {
                log::info!(
                    "video: the helper speaks version {version}, not {}: software decoding",
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
                "video: the helper failed ({why}), {} times now: software decoding from here on",
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

    /// Opens a decoder for a `codec` stream of about `width`×`height`.
    pub fn open_decoder(&self, codec: Codec, width: u32, height: u32) -> Result<HwDecoder, Lost> {
        let mut state = self.state();
        self.ensure_started(&mut state)?;
        let request = Request::OpenDecoder {
            codec,
            width,
            height,
            hardware: true,
        };
        let generation = state.generation;
        match self.exchange(&mut state, request, self.call_timeout)? {
            Reply::Opened { id } => Ok(HwDecoder {
                helper: self.clone(),
                id,
                generation,
                max: state
                    .capabilities
                    .iter()
                    .filter(|c| c.codec == codec && c.direction == Direction::Decode)
                    .map(|c| (c.max_width, c.max_height))
                    .max()
                    .unwrap_or((0, 0)),
            }),
            Reply::Failed { kind, detail } => Err(Lost(format!("{kind:?}: {detail}"))),
            other => Err(self.fail(&mut state, &format!("an answer to open: {other:?}"))),
        }
    }

    /// Opens an encoder of `codec` (constrained baseline H.264) for
    /// `width`×`height` pictures at `fps` and `bitrate` bit/s.
    pub fn open_encoder(
        &self,
        codec: Codec,
        width: u32,
        height: u32,
        fps: u32,
        bitrate: u32,
    ) -> Result<HwEncoder, Lost> {
        let mut state = self.state();
        self.ensure_started(&mut state)?;
        let request = Request::OpenEncoder {
            codec,
            width,
            height,
            fps,
            bitrate,
        };
        let generation = state.generation;
        match self.exchange(&mut state, request, self.call_timeout)? {
            Reply::Opened { id } => Ok(HwEncoder {
                helper: self.clone(),
                id,
                generation,
                size: (width, height),
            }),
            Reply::Failed { kind, detail } => Err(Lost(format!("{kind:?}: {detail}"))),
            other => Err(self.fail(&mut state, &format!("an answer to open: {other:?}"))),
        }
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
            // A picture to encode goes from its planes into the pipe.
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

/// Why a frame did not decode in hardware.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HwTrouble {
    /// Wait for a keyframe (a loss, or joined mid-stream).
    NeedKeyframe,
    /// The frame did not decode; the next keyframe starts over.
    Broken(String),
    /// The GPU cannot decode this stream (a profile, a size, a feature
    /// it lacks): software, for good.
    Unsupported(String),
    /// The helper failed or went away: software, and the GPU again at
    /// the next start if the helper comes back.
    Lost(String),
}

/// One stream's decoder in the helper. Closed when dropped.
#[derive(Debug)]
pub struct HwDecoder {
    helper: Helper,
    id: u32,
    generation: u64,
    /// The largest picture the helper said it decodes.
    #[cfg_attr(not(feature = "huddle-video"), allow(dead_code))]
    max: (u32, u32),
}

impl HwDecoder {
    /// Decodes one frame (an access unit, Annex B).
    #[cfg(feature = "huddle-video")]
    pub fn decode(&mut self, unit: &[u8], keyframe: bool) -> Result<Option<Yuv>, HwTrouble> {
        let request = Request::Decode {
            id: self.id,
            keyframe,
            data: unit.to_vec(),
        };
        match self.helper.call(self.generation, request) {
            Ok(Reply::Picture(ipc::Decoded { planes, .. })) => {
                // Checked against their size as they were read; and no
                // larger than the helper said it decodes.
                if planes.width > self.max.0 || planes.height > self.max.1 {
                    return Err(HwTrouble::Lost("a picture larger than promised".into()));
                }
                let size = |n: u32| usize::try_from(n).unwrap_or(0);
                Ok(Some(Yuv {
                    width: size(planes.width),
                    height: size(planes.height),
                    y: planes.y,
                    u: planes.u,
                    v: planes.v,
                }))
            }
            Ok(Reply::NoPicture) => Ok(None),
            Ok(Reply::Failed { kind, detail }) => Err(match kind {
                FailKind::NeedKeyframe => HwTrouble::NeedKeyframe,
                FailKind::Broken => HwTrouble::Broken(detail),
                FailKind::Unsupported => HwTrouble::Unsupported(detail),
                FailKind::Device | FailKind::Protocol | FailKind::UnknownId => {
                    HwTrouble::Lost(format!("{kind:?}: {detail}"))
                }
            }),
            Ok(other) => Err(HwTrouble::Lost(format!("an answer to decode: {other:?}"))),
            Err(Lost(why)) => Err(HwTrouble::Lost(why)),
        }
    }
}

impl HwDecoder {
    /// Asks the helper for pictures shrunk to cover `width`×`height`
    /// (0×0: their own size).
    pub fn set_output_size(&mut self, width: u32, height: u32) -> Result<(), HwTrouble> {
        let request = Request::SetOutputSize {
            id: self.id,
            width,
            height,
        };
        match self.helper.call(self.generation, request) {
            Ok(Reply::Done) => Ok(()),
            Ok(other) => Err(HwTrouble::Lost(format!(
                "an answer to the output size: {other:?}"
            ))),
            Err(Lost(why)) => Err(HwTrouble::Lost(why)),
        }
    }
}

impl Drop for HwDecoder {
    fn drop(&mut self) {
        // Best effort: a helper that restarted has forgotten it anyway.
        let _ = self
            .helper
            .call(self.generation, Request::Close { id: self.id });
    }
}

/// One encoder in the helper: constrained baseline H.264, one access unit
/// out for each picture in. Closed when dropped.
#[derive(Debug)]
pub struct HwEncoder {
    helper: Helper,
    id: u32,
    generation: u64,
    /// The pictures' size, which the helper's reply must not change.
    size: (u32, u32),
}

/// What a GPU-encoded picture came back as.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HwEncoded {
    /// The access unit, Annex B.
    pub data: Vec<u8>,
    /// Whether the helper says it is an IDR (the caller checks).
    pub keyframe: bool,
}

impl HwEncoder {
    /// The size it encodes.
    pub fn size(&self) -> (u32, u32) {
        self.size
    }

    /// Encodes one picture, an IDR if `force_keyframe`. Any failure means
    /// the stream goes on in software.
    pub fn encode(
        &mut self,
        picture: ipc::Planes,
        force_keyframe: bool,
    ) -> Result<HwEncoded, HwTrouble> {
        if (picture.width, picture.height) != self.size {
            return Err(HwTrouble::Unsupported("another size".into()));
        }
        let request = Request::Encode {
            id: self.id,
            force_keyframe,
            picture,
        };
        match self.helper.call(self.generation, request) {
            Ok(Reply::Encoded { keyframe, data }) if !data.is_empty() => {
                Ok(HwEncoded { data, keyframe })
            }
            Ok(Reply::Failed { kind, detail }) => Err(match kind {
                FailKind::Device | FailKind::Protocol | FailKind::UnknownId => {
                    HwTrouble::Lost(format!("{kind:?}: {detail}"))
                }
                _ => HwTrouble::Unsupported(format!("{kind:?}: {detail}")),
            }),
            Ok(other) => Err(HwTrouble::Lost(format!("an answer to encode: {other:?}"))),
            Err(Lost(why)) => Err(HwTrouble::Lost(why)),
        }
    }

    /// Aims at `bitrate` bit/s from the next picture on, with no
    /// keyframe.
    pub fn set_bitrate(&mut self, bitrate: u32) -> Result<(), HwTrouble> {
        let request = Request::SetBitrate {
            id: self.id,
            bitrate,
        };
        match self.helper.call(self.generation, request) {
            Ok(Reply::Done) => Ok(()),
            Ok(other) => Err(HwTrouble::Lost(format!(
                "an answer to the bit rate: {other:?}"
            ))),
            Err(Lost(why)) => Err(HwTrouble::Lost(why)),
        }
    }
}

impl Drop for HwEncoder {
    fn drop(&mut self) {
        // Best effort, as for a decoder.
        let _ = self
            .helper
            .call(self.generation, Request::Close { id: self.id });
    }
}

/// The helper as a test sees it: what it does with each request.
#[cfg(test)]
#[cfg_attr(not(feature = "huddle-video"), allow(dead_code))]
pub(crate) mod pretend {
    use super::*;

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
            capabilities: [Direction::Decode, Direction::Encode]
                .into_iter()
                .map(|direction| Capability {
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

    #[test]
    fn pictures_come_back_from_the_helper() {
        let (helper, launches) = helper(Pretend::new(working));
        assert!(helper.decodes(Codec::H264, 1920, 1080));
        assert!(
            !helper.decodes(Codec::H264, 2560, 1440),
            "larger than it said"
        );
        let mut decoder = helper.open_decoder(Codec::H264, 64, 48).expect("opens");
        let yuv = decoder
            .decode(&[0, 0, 0, 1, 0x65], true)
            .expect("decodes")
            .expect("a picture");
        assert_eq!((yuv.width, yuv.height, yuv.y[0]), (64, 48, 77));
        drop(decoder);
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
            let mut decoder = helper.open_decoder(Codec::H264, 64, 48).expect("opens");
            assert!(matches!(
                decoder.decode(&[0, 0, 0, 1, 0x65], true),
                Err(HwTrouble::Lost(_))
            ));
            assert_eq!(launches.load(Ordering::Relaxed), round);
            // The old decoder is gone with its helper.
            assert!(matches!(
                decoder.decode(&[0, 0, 0, 1, 0x65], true),
                Err(HwTrouble::Lost(_))
            ));
        }
        assert!(helper.given_up());
        assert!(helper.open_decoder(Codec::H264, 64, 48).is_err());
        assert!(!helper.decodes(Codec::H264, 64, 48));
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
        let mut decoder = helper.open_decoder(Codec::H264, 64, 48).expect("opens");
        let started = std::time::Instant::now();
        assert!(matches!(
            decoder.decode(&[0, 0, 0, 1, 0x65], true),
            Err(HwTrouble::Lost(_))
        ));
        assert!(
            started.elapsed() < Duration::from_secs(3),
            "the timeout, not forever"
        );
        assert!(!hung.load(Ordering::Relaxed));
        // The next stream gets a new helper, which works.
        let mut decoder = helper
            .open_decoder(Codec::H264, 64, 48)
            .expect("opens again");
        assert!(decoder.decode(&[0, 0, 0, 1, 0x65], true).is_ok());
        assert_eq!(launches.load(Ordering::Relaxed), 2);
    }

    #[test]
    fn pictures_whose_planes_do_not_match_are_refused() {
        // A picture frame claiming 4×4 with a 3-byte luma plane.
        let mut lying = Vec::new();
        let body_length = 4 + 1 + 8 + 4 + 3 + 4 + 4 + 4 + 4;
        lying.extend_from_slice(&u32::to_le_bytes(body_length));
        lying.extend_from_slice(&2u32.to_le_bytes());
        lying.push(3);
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
        let mut decoder = lying.open_decoder(Codec::H264, 4, 4).expect("opens");
        assert!(matches!(
            decoder.decode(&[0, 0, 0, 1, 0x65], true),
            Err(HwTrouble::Lost(_))
        ));
        // A well-formed picture larger than the helper said it decodes.
        let pretend = Pretend::new(|request| match request {
            Request::Decode { .. } => Act::Reply(picture(2048, 16, 0)),
            other => working(other),
        });
        let (larger, _) = helper(pretend);
        let mut decoder = larger.open_decoder(Codec::H264, 64, 48).expect("opens");
        assert!(matches!(
            decoder.decode(&[0, 0, 0, 1, 0x65], true),
            Err(HwTrouble::Lost(_))
        ));
    }

    #[test]
    fn a_helper_of_another_version_or_none_at_all_means_software() {
        let pretend = Pretend::new(|request| match request {
            Request::Hello { .. } => Act::Reply(Reply::Welcome {
                version: ipc::VERSION + 1,
                backend: "future".into(),
                capabilities: Vec::new(),
            }),
            other => working(other),
        });
        let (helper, launches) = helper(pretend);
        assert!(!helper.decodes(Codec::H264, 64, 48));
        assert!(helper.given_up());
        assert!(helper.open_decoder(Codec::H264, 64, 48).is_err());
        assert_eq!(launches.load(Ordering::Relaxed), 1);
        struct Missing;
        impl Launcher for Missing {
            fn launch(&self) -> std::io::Result<Link> {
                Err(std::io::Error::from(std::io::ErrorKind::NotFound))
            }
        }
        let helper = Helper::new(Arc::new(Missing));
        assert!(!helper.decodes(Codec::H264, 64, 48));
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
            Request::Decode { data, .. } if data.len() > 5 => Act::Reply(Reply::Failed {
                kind: FailKind::Unsupported,
                detail: "B slices".into(),
            }),
            Request::Decode { .. } => Act::Reply(Reply::Failed {
                kind: FailKind::Broken,
                detail: "bad slice".into(),
            }),
            other => working(other),
        });
        let (helper, launches) = helper(pretend);
        let mut decoder = helper.open_decoder(Codec::H264, 64, 48).expect("opens");
        assert_eq!(
            decoder.decode(&[0, 0, 1, 0x41], false),
            Err(HwTrouble::NeedKeyframe)
        );
        assert!(matches!(
            decoder.decode(&[0, 0, 1, 0x65, 0], true),
            Err(HwTrouble::Broken(_))
        ));
        assert!(matches!(
            decoder.decode(&[0, 0, 1, 0x65, 0, 0], true),
            Err(HwTrouble::Unsupported(_))
        ));
        // None of these was the helper's fault: it is still the first.
        assert_eq!(launches.load(Ordering::Relaxed), 1);
        assert!(!helper.given_up());
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
