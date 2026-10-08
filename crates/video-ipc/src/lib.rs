//! What NoSlacking and `noslacking-video`, its video helper, say to each
//! other over the helper's standard input and output.
//!
//! The helper decodes every video stream the app shows: on the GPU
//! through the platform's C libraries (VA-API on Linux), which takes
//! `unsafe` code and trusts drivers with whatever a stranger's stream
//! holds, and otherwise in software. It also captures what the user
//! sends, the screen they share ([`Request::StartShare`]) and their
//! camera ([`Request::StartCamera`]), and encodes it, so the app only
//! ever sees the H.264 that goes out and, for the camera, a small
//! picture for its self-view. It runs as its own process so the
//! app keeps `forbid(unsafe_code)` and a decoder's crash or hang (a
//! panic aborts a release build) only costs the helper. This crate is
//! the one thing both sides share: the framing and the messages, in
//! plain safe Rust.
//!
//! # Framing
//!
//! Every message is one frame: its length (`u32`, little-endian, counting
//! what follows it), a sequence number (`u32`) the reply repeats, then the
//! message: a tag byte and its fields. Integers are little-endian; byte
//! strings and text carry a `u32` length first. A frame longer than
//! [`MAX_MESSAGE`] is refused before anything is allocated for it.
//!
//! # Conversation
//!
//! The app speaks first with [`Request::Hello`]; the helper answers
//! [`Reply::Welcome`] with its version and what its back end can do. Then
//! each request gets exactly one reply, in order. The helper ends when its
//! input closes.
//!
//! Neither side trusts the other: every length is checked against the
//! bytes there are, every enum value against the ones known, and a
//! picture's planes against its size ([`Planes::check`]).

use std::fmt;
use std::io::{self, Read, Write};

/// The first four bytes of the hello and the welcome, so a program that
/// is not the helper is told apart from a helper that is the wrong
/// version.
pub const MAGIC: [u8; 4] = *b"NSVH";

/// This protocol's version. Both sides must have the same; with a helper
/// of another version the app shows no video (they ship together, so
/// that only happens with a broken install). Version 2 added
/// [`Request::SetOutputSize`]; version 3 made the helper decode in
/// software too: [`Request::OpenDecoder`] says whether to try the GPU,
/// and a picture says its stream's size and which decoded it
/// ([`Decoded`]); version 4 moved screen sharing into the helper: it
/// lists what can be shared ([`Request::ListSources`]), and captures and
/// encodes it ([`Request::StartShare`], [`Request::NextFrame`]); version
/// 5 moved the camera there too ([`Request::ListCameras`],
/// [`Request::StartCamera`], a self-view picture with each frame), and
/// dropped encoding pictures the app sends, since it no longer has any;
/// version 6 sends only pictures the app will show: a frame can be
/// decoded without its picture ([`Request::Decode`]'s `show`, answered
/// [`Reply::Kept`]), which [`Request::Fetch`] asks for later, and a
/// picture the same as the last one sent is answered
/// [`Reply::Unchanged`] instead of being sent again.
pub const VERSION: u16 = 6;

/// The largest frame either side accepts, in bytes: room for an I420
/// picture at [`MAX_SIDE`] square (24 MiB) and its header.
pub const MAX_MESSAGE: usize = 32 << 20;

/// The largest width or height of a picture, in pixels: H.264's largest
/// level (6.2) tops out at 8192 wide, but nothing a huddle sends comes
/// close to 4096, and a bound this size keeps a picture under 24 MiB.
pub const MAX_SIDE: u32 = 4096;

/// The longest text in a message (a back end's name, a failure's
/// detail), in bytes.
pub const MAX_TEXT: usize = 4096;

/// The most capabilities a welcome lists.
pub const MAX_CAPABILITIES: usize = 64;

/// The most screens and windows a list of sources holds.
pub const MAX_SOURCES: usize = 256;

/// The longest a capture may be asked to wait for a new picture, in
/// milliseconds: well inside the app's timeout for one reply.
pub const MAX_WAIT_MS: u32 = 250;

/// The widest or tallest self-view picture a camera's frame carries: the
/// app shows it in the call bar, a few hundred pixels across.
pub const MAX_PREVIEW_SIDE: u32 = 640;

/// What went wrong reading or writing a frame.
#[derive(Debug)]
pub enum Error {
    /// The pipe failed.
    Io(io::Error),
    /// The input ended inside a frame.
    Truncated,
    /// A frame or field claims more bytes than allowed.
    TooLarge(usize),
    /// Bytes were left over after a message's last field.
    Trailing(usize),
    /// A message tag this version does not know.
    UnknownTag(u8),
    /// A field holds a value it cannot have; the name says which.
    BadValue(&'static str),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(f, "the pipe failed: {error}"),
            Self::Truncated => f.write_str("the message ends too soon"),
            Self::TooLarge(n) => write!(f, "{n} bytes is more than allowed"),
            Self::Trailing(n) => write!(f, "{n} bytes left over after the message"),
            Self::UnknownTag(tag) => write!(f, "unknown message {tag}"),
            Self::BadValue(what) => write!(f, "bad value for {what}"),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            _ => None,
        }
    }
}

impl From<io::Error> for Error {
    fn from(error: io::Error) -> Self {
        if error.kind() == io::ErrorKind::UnexpectedEof {
            Self::Truncated
        } else {
            Self::Io(error)
        }
    }
}

/// One frame off the pipe: the sequence number and the message's bytes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Frame {
    /// The request's number; a reply repeats its request's.
    pub seq: u32,
    /// The message, to give to [`Request::decode`] or [`Reply::decode`].
    pub body: Vec<u8>,
}

/// Writes one frame and flushes it.
pub fn write_frame(out: &mut impl Write, seq: u32, body: &[u8]) -> Result<(), Error> {
    let length = body.len() + 4;
    if length > MAX_MESSAGE {
        return Err(Error::TooLarge(length));
    }
    let length = u32::try_from(length).map_err(|_| Error::TooLarge(length))?;
    let mut header = [0u8; 8];
    header[..4].copy_from_slice(&length.to_le_bytes());
    header[4..].copy_from_slice(&seq.to_le_bytes());
    out.write_all(&header)?;
    out.write_all(body)?;
    out.flush()?;
    Ok(())
}

/// Reads one frame: none when the input ended cleanly between frames.
pub fn read_frame(input: &mut impl Read) -> Result<Option<Frame>, Error> {
    let mut length = [0u8; 4];
    let mut got = 0;
    while got < length.len() {
        match input.read(&mut length[got..]) {
            Ok(0) if got == 0 => return Ok(None),
            Ok(0) => return Err(Error::Truncated),
            Ok(n) => got += n,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => return Err(error.into()),
        }
    }
    let length = usize::try_from(u32::from_le_bytes(length)).unwrap_or(usize::MAX);
    if length > MAX_MESSAGE {
        return Err(Error::TooLarge(length));
    }
    if length < 5 {
        // A sequence number and at least a tag.
        return Err(Error::Truncated);
    }
    let mut seq = [0u8; 4];
    input.read_exact(&mut seq)?;
    let mut body = vec![0u8; length - 4];
    input.read_exact(&mut body)?;
    Ok(Some(Frame {
        seq: u32::from_le_bytes(seq),
        body,
    }))
}

/// Writes `reply` as one frame. A picture's planes go straight from its
/// vectors to the pipe, without first being copied into one message.
pub fn write_reply(out: &mut impl Write, seq: u32, reply: &Reply) -> Result<(), Error> {
    let Reply::Picture(decoded) = reply else {
        return write_frame(out, seq, &reply.encode());
    };
    let mut head = [PICTURE, 0, 0, 0, 0, 0, 0, 0, 0, u8::from(decoded.hardware)];
    head[1..5].copy_from_slice(&decoded.source.0.to_le_bytes());
    head[5..9].copy_from_slice(&decoded.source.1.to_le_bytes());
    write_planes(out, seq, &head, &decoded.planes)
}

/// Writes `request` as one frame.
pub fn write_request(out: &mut impl Write, seq: u32, request: &Request) -> Result<(), Error> {
    write_frame(out, seq, &request.encode())
}

/// Writes a frame of `head` (the tag and any fields before the picture)
/// and `planes`, each plane straight from its vector.
fn write_planes(out: &mut impl Write, seq: u32, head: &[u8], planes: &Planes) -> Result<(), Error> {
    let planes_length = planes.y.len() + planes.u.len() + planes.v.len();
    // Sequence number, head, size, three plane lengths, the planes.
    let length = 4 + head.len() + 8 + 12 + planes_length;
    if length > MAX_MESSAGE {
        return Err(Error::TooLarge(length));
    }
    let mut header = Vec::with_capacity(4 + 4 + head.len() + 8 + 4);
    let length = u32::try_from(length).map_err(|_| Error::TooLarge(length))?;
    header.extend_from_slice(&length.to_le_bytes());
    header.extend_from_slice(&seq.to_le_bytes());
    header.extend_from_slice(head);
    header.extend_from_slice(&planes.width.to_le_bytes());
    header.extend_from_slice(&planes.height.to_le_bytes());
    out.write_all(&header)?;
    for plane in [&planes.y, &planes.u, &planes.v] {
        let n = u32::try_from(plane.len()).map_err(|_| Error::TooLarge(plane.len()))?;
        out.write_all(&n.to_le_bytes())?;
        out.write_all(plane)?;
    }
    out.flush()?;
    Ok(())
}

/// A frame's sequence number and tag, read with its length: none when
/// the input ended cleanly between frames; else also the bytes left
/// after the tag.
fn read_head(input: &mut impl Read) -> Result<Option<(u32, u8, usize)>, Error> {
    let mut length = [0u8; 4];
    let mut got = 0;
    while got < length.len() {
        match input.read(&mut length[got..]) {
            Ok(0) if got == 0 => return Ok(None),
            Ok(0) => return Err(Error::Truncated),
            Ok(n) => got += n,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => return Err(error.into()),
        }
    }
    let length = usize::try_from(u32::from_le_bytes(length)).unwrap_or(usize::MAX);
    if length > MAX_MESSAGE {
        return Err(Error::TooLarge(length));
    }
    if length < 5 {
        return Err(Error::Truncated);
    }
    let mut seq_and_tag = [0u8; 5];
    input.read_exact(&mut seq_and_tag)?;
    let seq = u32::from_le_bytes([
        seq_and_tag[0],
        seq_and_tag[1],
        seq_and_tag[2],
        seq_and_tag[3],
    ]);
    Ok(Some((seq, seq_and_tag[4], length - 5)))
}

/// The rest of a frame whose tag was read: the whole message, tag first.
fn read_body(input: &mut impl Read, tag: u8, left: usize) -> Result<Vec<u8>, Error> {
    let mut body = vec![0u8; left + 1];
    body[0] = tag;
    input.read_exact(&mut body[1..])?;
    Ok(body)
}

/// Reads `n` of the `left` bytes of a frame.
fn read_exact_of(input: &mut impl Read, left: &mut usize, n: usize) -> Result<Vec<u8>, Error> {
    if n > *left {
        return Err(Error::Truncated);
    }
    *left -= n;
    let mut bytes = Vec::with_capacity(n);
    input.take(n as u64).read_to_end(&mut bytes)?;
    if bytes.len() != n {
        return Err(Error::Truncated);
    }
    Ok(bytes)
}

/// Reads a `u32` of the `left` bytes of a frame.
fn read_u32_of(input: &mut impl Read, left: &mut usize) -> Result<u32, Error> {
    let bytes = read_exact_of(input, left, 4)?;
    let mut word = [0u8; 4];
    word.copy_from_slice(&bytes);
    Ok(u32::from_le_bytes(word))
}

/// Reads a picture, the last field of a frame with `left` bytes to go,
/// each plane straight into its own vector once its length is checked
/// against the frame's and the picture's size.
fn read_planes(input: &mut impl Read, mut left: usize) -> Result<Planes, Error> {
    let width = read_u32_of(input, &mut left)?;
    let height = read_u32_of(input, &mut left)?;
    if width == 0 || height == 0 || width > MAX_SIDE || height > MAX_SIDE {
        return Err(Error::BadValue("picture size"));
    }
    let (cw, ch) = chroma_size(width, height);
    let sizes = [
        u64::from(width) * u64::from(height),
        u64::from(cw) * u64::from(ch),
        u64::from(cw) * u64::from(ch),
    ];
    let mut planes = Vec::with_capacity(3);
    for size in sizes {
        let n = read_u32_of(input, &mut left)?;
        if u64::from(n) != size {
            return Err(Error::BadValue("plane length"));
        }
        let n = usize::try_from(n).map_err(|_| Error::TooLarge(usize::MAX))?;
        planes.push(read_exact_of(input, &mut left, n)?);
    }
    if left != 0 {
        return Err(Error::Trailing(left));
    }
    let v = planes.pop().unwrap_or_default();
    let u = planes.pop().unwrap_or_default();
    let y = planes.pop().unwrap_or_default();
    let planes = Planes {
        width,
        height,
        y,
        u,
        v,
    };
    planes.check()?;
    Ok(planes)
}

/// Reads one reply and its sequence number: none when the input ended
/// cleanly between frames. A picture's planes are read straight into
/// their own vectors, each checked against the frame's length and the
/// picture's size before anything is allocated for it.
pub fn read_reply(input: &mut impl Read) -> Result<Option<(u32, Reply)>, Error> {
    let Some((seq, tag, left)) = read_head(input)? else {
        return Ok(None);
    };
    if tag != PICTURE {
        let body = read_body(input, tag, left)?;
        return Ok(Some((seq, Reply::decode(&body)?)));
    }
    let mut left = left;
    let source = (
        read_u32_of(input, &mut left)?,
        read_u32_of(input, &mut left)?,
    );
    let hardware = match read_exact_of(input, &mut left, 1)?[0] {
        0 => false,
        1 => true,
        _ => return Err(Error::BadValue("flag")),
    };
    let decoded = Decoded {
        planes: read_planes(input, left)?,
        source,
        hardware,
    };
    decoded.check()?;
    Ok(Some((seq, Reply::Picture(decoded))))
}

/// One request off the pipe: its sequence number and the request, or
/// why its message (read whole, so the next frame still lines up) did
/// not decode.
pub type Incoming = (u32, Result<Request, Error>);

/// Reads one request and its sequence number: none when the input ended
/// cleanly between frames. A request that does not decode is read whole,
/// so the next one still lines up.
pub fn read_request(input: &mut impl Read) -> Result<Option<Incoming>, Error> {
    let Some(frame) = read_frame(input)? else {
        return Ok(None);
    };
    Ok(Some((frame.seq, Request::decode(&frame.body))))
}

/// The picture reply's tag.
const PICTURE: u8 = 3;

/// A video coding format.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Codec {
    /// H.264 (Slack sends constrained baseline).
    H264,
}

impl Codec {
    fn to_byte(self) -> u8 {
        match self {
            Self::H264 => 1,
        }
    }

    fn from_byte(byte: u8) -> Result<Self, Error> {
        match byte {
            1 => Ok(Self::H264),
            _ => Err(Error::BadValue("codec")),
        }
    }
}

/// Which way a capability goes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Direction {
    /// Pictures from a stream.
    Decode,
    /// A stream from pictures.
    Encode,
}

impl Direction {
    fn to_byte(self) -> u8 {
        match self {
            Self::Decode => 1,
            Self::Encode => 2,
        }
    }

    fn from_byte(byte: u8) -> Result<Self, Error> {
        match byte {
            1 => Ok(Self::Decode),
            2 => Ok(Self::Encode),
            _ => Err(Error::BadValue("direction")),
        }
    }
}

/// One thing the helper's back end can do, and up to what size.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Capability {
    /// The format.
    pub codec: Codec,
    /// Decoding or encoding.
    pub direction: Direction,
    /// The widest picture, in pixels.
    pub max_width: u32,
    /// The tallest picture, in pixels.
    pub max_height: u32,
}

impl Capability {
    /// Whether this covers `codec` going `direction` at `width`×`height`.
    pub fn covers(&self, codec: Codec, direction: Direction, width: u32, height: u32) -> bool {
        self.codec == codec
            && self.direction == direction
            && width <= self.max_width
            && height <= self.max_height
    }
}

/// Why the helper could not do what was asked.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FailKind {
    /// The back end cannot do this (codec, profile, size): use software.
    Unsupported,
    /// The decoder lost its place: send a keyframe.
    NeedKeyframe,
    /// The frame did not decode (or the picture did not encode); the
    /// decoder starts over at the next keyframe.
    Broken,
    /// The device or driver failed: use software.
    Device,
    /// The request made no sense to the helper.
    Protocol,
    /// No decoder or encoder has that number.
    UnknownId,
}

impl FailKind {
    fn to_byte(self) -> u8 {
        match self {
            Self::Unsupported => 1,
            Self::NeedKeyframe => 2,
            Self::Broken => 3,
            Self::Device => 4,
            Self::Protocol => 5,
            Self::UnknownId => 6,
        }
    }

    fn from_byte(byte: u8) -> Result<Self, Error> {
        match byte {
            1 => Ok(Self::Unsupported),
            2 => Ok(Self::NeedKeyframe),
            3 => Ok(Self::Broken),
            4 => Ok(Self::Device),
            5 => Ok(Self::Protocol),
            6 => Ok(Self::UnknownId),
            _ => Err(Error::BadValue("failure kind")),
        }
    }
}

/// The chroma planes' width and height for a `width`×`height` picture.
pub fn chroma_size(width: u32, height: u32) -> (u32, u32) {
    (width.div_ceil(2), height.div_ceil(2))
}

/// A picture in I420: a full-size luma plane, rows packed, then the two
/// chroma planes at half the width and height (rounded up).
#[derive(Clone, PartialEq, Eq)]
pub struct Planes {
    /// Its width in pixels.
    pub width: u32,
    /// Its height in pixels.
    pub height: u32,
    /// Luma, `width * height` bytes.
    pub y: Vec<u8>,
    /// Blue difference.
    pub u: Vec<u8>,
    /// Red difference.
    pub v: Vec<u8>,
}

impl fmt::Debug for Planes {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Planes")
            .field("width", &self.width)
            .field("height", &self.height)
            .field("y", &self.y.len())
            .field("u", &self.u.len())
            .field("v", &self.v.len())
            .finish()
    }
}

impl Planes {
    /// Whether the size is one a picture can have and every plane is
    /// exactly as long as the size says. Nothing from the other side of
    /// the pipe is used before this holds.
    pub fn check(&self) -> Result<(), Error> {
        if self.width == 0 || self.height == 0 {
            return Err(Error::BadValue("picture size"));
        }
        if self.width > MAX_SIDE || self.height > MAX_SIDE {
            return Err(Error::BadValue("picture size"));
        }
        let (cw, ch) = chroma_size(self.width, self.height);
        let luma = u64::from(self.width) * u64::from(self.height);
        let chroma = u64::from(cw) * u64::from(ch);
        let length = |plane: &[u8]| u64::try_from(plane.len()).unwrap_or(u64::MAX);
        if length(&self.y) != luma || length(&self.u) != chroma || length(&self.v) != chroma {
            return Err(Error::BadValue("plane length"));
        }
        Ok(())
    }
}

/// A decoded picture and where it came from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Decoded {
    /// The picture, shrunk to cover the decoder's output box
    /// ([`Request::SetOutputSize`]) if it has one.
    pub planes: Planes,
    /// The stream's own size (its SPS, cropped), before any shrinking.
    pub source: (u32, u32),
    /// Whether the GPU decoded it (else software did).
    pub hardware: bool,
}

impl Decoded {
    /// Whether the planes are whole ([`Planes::check`]) and no larger
    /// than the source, itself a size a picture can have.
    pub fn check(&self) -> Result<(), Error> {
        self.planes.check()?;
        let (width, height) = self.source;
        if width == 0 || height == 0 || width > MAX_SIDE || height > MAX_SIDE {
            return Err(Error::BadValue("source size"));
        }
        if self.planes.width > width || self.planes.height > height {
            return Err(Error::BadValue("picture larger than its source"));
        }
        Ok(())
    }
}

/// What kind of thing can be captured.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum SourceKind {
    /// A whole screen.
    Screen,
    /// One window.
    Window,
    /// A camera.
    Camera,
}

impl SourceKind {
    fn to_byte(self) -> u8 {
        match self {
            Self::Screen => 1,
            Self::Window => 2,
            Self::Camera => 3,
        }
    }

    fn from_byte(byte: u8) -> Result<Self, Error> {
        match byte {
            1 => Ok(Self::Screen),
            2 => Ok(Self::Window),
            3 => Ok(Self::Camera),
            _ => Err(Error::BadValue("source kind")),
        }
    }
}

/// A screen, window or camera the helper offers: for the share picker
/// where the system has no dialog of its own, and the cameras there are.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Source {
    /// How the helper finds it again ([`ShareChoice::Source`],
    /// [`CameraChoice::Device`]).
    pub id: String,
    /// What a picker calls it: the screen's, the window's or the
    /// camera's name.
    pub name: String,
    /// A screen, a window or a camera.
    pub kind: SourceKind,
}

/// What to share.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ShareChoice {
    /// Whatever the system's own dialog lets the user pick (the
    /// ScreenCast portal); `again` asks it to ask rather than share what
    /// the restore token names. Where there is no dialog, the first
    /// screen.
    System {
        /// Choose afresh.
        again: bool,
    },
    /// One of [`Reply::Sources`]' sources, by its id.
    Source(String),
    /// A generated 1080p test screen with a moving clock, never anyone's
    /// screen (the probe's, and the benchmarks').
    Test,
}

/// Which camera to send.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CameraChoice {
    /// The system's first (on Linux, the first video device that
    /// captures in a format the helper reads).
    First,
    /// One of [`Request::ListCameras`]' cameras, by its id.
    Device(String),
    /// A generated 640×480 test picture with a moving clock, never
    /// anyone's camera (the probe's, the demo's and the benchmarks').
    Test,
}

/// Why a capture (a share or the camera) did not start, or stopped.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CaptureProblem {
    /// The user closed the system's dialog without choosing.
    Cancelled,
    /// The system did not let the helper capture (macOS's Screen
    /// Recording or camera permission, a portal that refused, a video
    /// device the user may not open).
    Denied,
    /// Nothing here can capture: no portal, no X server, no camera.
    Unavailable,
    /// The source chosen is gone (a window closed, a screen or camera
    /// unplugged).
    Gone,
    /// The capture ended by itself: the compositor's own "stop sharing",
    /// the window closed, PipeWire went away, the camera stopped.
    Ended,
    /// It failed; the detail says why, for the log.
    Failed,
    /// The device is in use by another program.
    Busy,
}

impl CaptureProblem {
    fn to_byte(self) -> u8 {
        match self {
            Self::Cancelled => 1,
            Self::Denied => 2,
            Self::Unavailable => 3,
            Self::Gone => 4,
            Self::Ended => 5,
            Self::Failed => 6,
            Self::Busy => 7,
        }
    }

    fn from_byte(byte: u8) -> Result<Self, Error> {
        match byte {
            1 => Ok(Self::Cancelled),
            2 => Ok(Self::Denied),
            3 => Ok(Self::Unavailable),
            4 => Ok(Self::Gone),
            5 => Ok(Self::Ended),
            6 => Ok(Self::Failed),
            7 => Ok(Self::Busy),
            _ => Err(Error::BadValue("capture problem")),
        }
    }
}

/// One encoded picture of a capture: an access unit and what the app
/// needs to send it, and for a camera what the self-view shows.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CapturedFrame {
    /// Whether it is an IDR (with its SPS and PPS in front).
    pub keyframe: bool,
    /// Whether the GPU encoded it (else software did).
    pub hardware: bool,
    /// The picture's width as encoded.
    pub width: u32,
    /// Its height.
    pub height: u32,
    /// How long ago, in microseconds, the picture was captured when this
    /// reply was written: the app's RTP time comes from it. A picture
    /// sent again (a still screen) says 0.
    pub age_us: u32,
    /// The NAL units, Annex B.
    pub data: Vec<u8>,
    /// The same picture as captured, before encoding, shrunk by a whole
    /// step to at most the width the camera was started with
    /// ([`Request::StartCamera`]), at most [`MAX_PREVIEW_SIDE`] a side:
    /// the self-view. None for a share, or a picture sent again.
    pub preview: Option<Planes>,
}

/// What the app asks the helper.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Request {
    /// The first message: who the app is and which version it speaks.
    Hello {
        /// The app's protocol version.
        version: u16,
    },
    /// A decoder for one stream; the reply is [`Reply::Opened`] or a
    /// failure. The size is a hint (the stream's SPS decides). The
    /// helper decodes in software whatever the GPU cannot, and
    /// everything when `hardware` is off.
    OpenDecoder {
        /// The stream's format.
        codec: Codec,
        /// The expected width.
        width: u32,
        /// The expected height.
        height: u32,
        /// Try the GPU first (Settings → Huddles → Use the graphics card
        /// for video).
        hardware: bool,
    },
    /// One frame (an access unit, Annex B) for decoder `id`; the reply is
    /// a picture, [`Reply::Unchanged`] (the same picture as the last
    /// sent), [`Reply::Kept`] (decoded, not sent: `show` was off), no
    /// picture (parameter sets only), or a failure.
    Decode {
        /// The decoder.
        id: u32,
        /// Whether the frame holds an IDR slice.
        keyframe: bool,
        /// Send its picture. Off when the app would not show it (it has
        /// not drawn the last one yet, or its window is hidden): the
        /// frame is still decoded, as the frames after it need, but its
        /// picture is neither shrunk, read back from the GPU nor sent;
        /// it is kept for a [`Request::Fetch`] until the next frame.
        show: bool,
        /// The frame.
        data: Vec<u8>,
    },
    /// Decoder `id`'s newest picture, kept back by a `Decode` without
    /// `show`: the reply is that picture (shrunk to the output size as it
    /// is now), [`Reply::Unchanged`], or no picture when every picture
    /// decoded has been sent (or the frame since gave none).
    Fetch {
        /// The decoder.
        id: u32,
    },
    /// A new target bit rate for capture `id` (a share or the camera),
    /// from its next picture on; the reply is [`Reply::Done`].
    SetBitrate {
        /// The capture.
        id: u32,
        /// Bits a second.
        bitrate: u32,
    },
    /// Decoder or capture `id` is no longer needed (a capture stops, its
    /// device closed); the reply is [`Reply::Done`].
    Close {
        /// The decoder or capture.
        id: u32,
    },
    /// Decoder `id`'s pictures from now on are shrunk to cover a
    /// `width`×`height` box ([`output_size`]): the size the app shows
    /// them at. 0×0 (the start) means at their own size. The reply is
    /// [`Reply::Done`].
    SetOutputSize {
        /// The decoder.
        id: u32,
        /// The box's width in pixels.
        width: u32,
        /// The box's height in pixels.
        height: u32,
    },
    /// What can be shared; the reply is [`Reply::Sources`], or a
    /// [`Reply::Problem`] when nothing can capture here.
    ListSources,
    /// Starts capturing and encoding `choice`; the reply is
    /// [`Reply::Started`] or a [`Reply::Problem`]. With the system's
    /// dialog this waits for the user. Nothing is captured before it,
    /// and nothing after the share's [`Request::Close`].
    StartShare {
        /// What to share.
        choice: ShareChoice,
        /// Encode on the GPU where it can (Settings → Huddles → Use the
        /// graphics card for video); otherwise software, at most
        /// 1280×720.
        hardware: bool,
        /// The bit rate to start at, in bits a second.
        bitrate: u32,
        /// What the portal said to remember the last choice by (empty:
        /// nothing), so sharing again in this run of the app does not
        /// ask again.
        restore: String,
    },
    /// Capture `id`'s next picture, encoded: the reply is
    /// [`Reply::Frame`], [`Reply::NoPicture`] when nothing new came
    /// within `wait_ms`, or a [`Reply::Problem`] once the capture ended.
    NextFrame {
        /// The capture.
        id: u32,
        /// Make it a keyframe.
        force_keyframe: bool,
        /// Encode the last picture again if nothing new came (a still
        /// screen's keepalive, or a keyframe asked for).
        repeat: bool,
        /// How long to wait for a new picture, at most [`MAX_WAIT_MS`].
        wait_ms: u32,
    },
    /// The cameras there are; the reply is [`Reply::Sources`] (never a
    /// dialog), or a [`Reply::Problem`] when nothing can capture here.
    ListCameras,
    /// Opens the camera `choice` names and starts encoding it, 640×480
    /// at most and 30 a second; the reply is [`Reply::Started`] or a
    /// [`Reply::Problem`]. Nothing is captured before it, and the camera
    /// is closed (its light out) at its [`Request::Close`]. On macOS the
    /// first start may wait for the user to answer the system's camera
    /// question.
    StartCamera {
        /// Which camera.
        choice: CameraChoice,
        /// Encode on the GPU where it can, else software.
        hardware: bool,
        /// The bit rate to start at, in bits a second.
        bitrate: u32,
        /// The self-view's widest, in pixels: each new picture's frame
        /// carries it shrunk by a whole step to at most this wide; 0 for
        /// none. At most [`MAX_PREVIEW_SIDE`].
        preview: u32,
    },
}

/// The size a `source`-sized picture is shrunk to so it still covers a
/// `fit`-sized box both ways: the aspect ratio kept (to within the
/// rounding), never larger than the source, and even each way so its
/// chroma halves exactly. The source itself when the box is as large or
/// has no size (0).
pub fn output_size(source: (u32, u32), fit: (u32, u32)) -> (u32, u32) {
    let (sw, sh) = (u64::from(source.0), u64::from(source.1));
    let (fw, fh) = (u64::from(fit.0), u64::from(fit.1));
    if sw == 0 || sh == 0 || fw == 0 || fh == 0 {
        return source;
    }
    // Scale by the larger of the two ratios, so both sides cover the box.
    let (w, h) = if fw * sh >= fh * sw {
        (fw, (sh * fw).div_ceil(sw))
    } else {
        ((sw * fh).div_ceil(sh), fh)
    };
    if w >= sw || h >= sh {
        return source;
    }
    let even = |n: u64, limit: u64| -> u32 {
        let n = (n + (n & 1)).max(2);
        // Rounding up to even never passes the source unless it was odd
        // and as large: then one less.
        let n = if n > limit { limit & !1 } else { n };
        u32::try_from(n.max(2)).unwrap_or(u32::MAX)
    };
    (even(w, sw), even(h, sh))
}

/// What the helper answers.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Reply {
    /// The answer to the hello.
    Welcome {
        /// The helper's protocol version.
        version: u16,
        /// Which back end it uses ("vaapi", "none", …), for the log.
        backend: String,
        /// What its GPU can do; empty when it found no hardware. Decoding
        /// H.264 in software needs no capability: the helper always can.
        capabilities: Vec<Capability>,
    },
    /// A decoder was opened with this number.
    Opened {
        /// Its number, for the requests that follow.
        id: u32,
    },
    /// A decoded picture.
    Picture(Decoded),
    /// The frame decoded to no picture (it held parameter sets only), a
    /// fetch found no picture not yet sent, or a capture had nothing new
    /// in time.
    NoPicture,
    /// The frame decoded to a picture, kept back as asked
    /// ([`Request::Decode`]'s `show` off) for a [`Request::Fetch`].
    Kept,
    /// The picture is exactly the last one sent for this decoder (its
    /// size, source and every byte): the app shows what it has.
    Unchanged,
    /// Done, nothing to say.
    Done,
    /// The request failed.
    Failed {
        /// How.
        kind: FailKind,
        /// What happened, for the log.
        detail: String,
    },
    /// What can be shared, or the cameras there are.
    Sources {
        /// The system shows its own dialog when the share starts (the
        /// ScreenCast portal): there is nothing for the app to list.
        dialog: bool,
        /// The screens and windows for the app's picker, or the cameras.
        sources: Vec<Source>,
    },
    /// A capture (a share or the camera) started with this number.
    Started {
        /// Its number, for the requests that follow.
        id: u32,
        /// What to give the next [`Request::StartShare`] so the portal
        /// shares the same again without asking (empty: nothing).
        restore: String,
    },
    /// A capture's picture.
    Frame(CapturedFrame),
    /// A capture did not start, or ended.
    Problem {
        /// What happened.
        problem: CaptureProblem,
        /// More, for the log.
        detail: String,
    },
}

/// Appends fields to a message being built.
struct Out(Vec<u8>);

impl Out {
    fn new(tag: u8) -> Self {
        Self(vec![tag])
    }
    fn u8(&mut self, value: u8) -> &mut Self {
        self.0.push(value);
        self
    }
    fn u16(&mut self, value: u16) -> &mut Self {
        self.0.extend_from_slice(&value.to_le_bytes());
        self
    }
    fn u32(&mut self, value: u32) -> &mut Self {
        self.0.extend_from_slice(&value.to_le_bytes());
        self
    }
    fn bytes(&mut self, value: &[u8]) -> &mut Self {
        // Lengths over u32 cannot be framed anyway; write_frame refuses
        // the whole message.
        self.u32(u32::try_from(value.len()).unwrap_or(u32::MAX));
        self.0.extend_from_slice(value);
        self
    }
    fn text(&mut self, value: &str) -> &mut Self {
        let mut end = value.len().min(MAX_TEXT);
        while !value.is_char_boundary(end) {
            end -= 1;
        }
        self.bytes(&value.as_bytes()[..end])
    }
    fn planes(&mut self, planes: &Planes) -> &mut Self {
        self.u32(planes.width)
            .u32(planes.height)
            .bytes(&planes.y)
            .bytes(&planes.u)
            .bytes(&planes.v)
    }
    fn done(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.0)
    }
}

/// Reads fields off a message, never past its end.
struct In<'a> {
    rest: &'a [u8],
}

impl<'a> In<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8], Error> {
        if n > self.rest.len() {
            return Err(Error::Truncated);
        }
        let (head, tail) = self.rest.split_at(n);
        self.rest = tail;
        Ok(head)
    }
    fn u8(&mut self) -> Result<u8, Error> {
        Ok(self.take(1)?[0])
    }
    fn u16(&mut self) -> Result<u16, Error> {
        let mut bytes = [0u8; 2];
        bytes.copy_from_slice(self.take(2)?);
        Ok(u16::from_le_bytes(bytes))
    }
    fn u32(&mut self) -> Result<u32, Error> {
        let mut bytes = [0u8; 4];
        bytes.copy_from_slice(self.take(4)?);
        Ok(u32::from_le_bytes(bytes))
    }
    fn flag(&mut self) -> Result<bool, Error> {
        match self.u8()? {
            0 => Ok(false),
            1 => Ok(true),
            _ => Err(Error::BadValue("flag")),
        }
    }
    fn bytes(&mut self, max: usize) -> Result<Vec<u8>, Error> {
        let n = usize::try_from(self.u32()?).unwrap_or(usize::MAX);
        if n > max {
            return Err(Error::TooLarge(n));
        }
        Ok(self.take(n)?.to_vec())
    }
    fn text(&mut self) -> Result<String, Error> {
        String::from_utf8(self.bytes(MAX_TEXT)?).map_err(|_| Error::BadValue("text"))
    }
    fn magic(&mut self) -> Result<(), Error> {
        if self.take(4)? == MAGIC {
            Ok(())
        } else {
            Err(Error::BadValue("magic"))
        }
    }
    fn side(&mut self, what: &'static str) -> Result<u32, Error> {
        let side = self.u32()?;
        if side > MAX_SIDE {
            return Err(Error::BadValue(what));
        }
        Ok(side)
    }
    fn planes(&mut self) -> Result<Planes, Error> {
        let planes = Planes {
            width: self.side("picture size")?,
            height: self.side("picture size")?,
            y: self.bytes(MAX_MESSAGE)?,
            u: self.bytes(MAX_MESSAGE)?,
            v: self.bytes(MAX_MESSAGE)?,
        };
        planes.check()?;
        Ok(planes)
    }
    fn end(self) -> Result<(), Error> {
        if self.rest.is_empty() {
            Ok(())
        } else {
            Err(Error::Trailing(self.rest.len()))
        }
    }
}

impl Request {
    /// The message's bytes, for [`write_frame`].
    pub fn encode(&self) -> Vec<u8> {
        match self {
            Self::Hello { version } => {
                let mut out = Out::new(1);
                out.0.extend_from_slice(&MAGIC);
                out.u16(*version).done()
            }
            Self::OpenDecoder {
                codec,
                width,
                height,
                hardware,
            } => Out::new(2)
                .u8(codec.to_byte())
                .u32(*width)
                .u32(*height)
                .u8(u8::from(*hardware))
                .done(),
            Self::Decode {
                id,
                keyframe,
                show,
                data,
            } => Out::new(3)
                .u32(*id)
                .u8(u8::from(*keyframe))
                .u8(u8::from(*show))
                .bytes(data)
                .done(),
            // 4 and 5 were the app's own pictures to encode (version 4).
            Self::SetBitrate { id, bitrate } => Out::new(6).u32(*id).u32(*bitrate).done(),
            Self::Close { id } => Out::new(7).u32(*id).done(),
            Self::SetOutputSize { id, width, height } => {
                Out::new(8).u32(*id).u32(*width).u32(*height).done()
            }
            Self::ListSources => Out::new(9).done(),
            Self::StartShare {
                choice,
                hardware,
                bitrate,
                restore,
            } => {
                let mut out = Out::new(10);
                match choice {
                    ShareChoice::System { again } => out.u8(1).u8(u8::from(*again)),
                    ShareChoice::Source(id) => out.u8(2).text(id),
                    ShareChoice::Test => out.u8(3),
                };
                out.u8(u8::from(*hardware))
                    .u32(*bitrate)
                    .text(restore)
                    .done()
            }
            Self::NextFrame {
                id,
                force_keyframe,
                repeat,
                wait_ms,
            } => Out::new(11)
                .u32(*id)
                .u8(u8::from(*force_keyframe))
                .u8(u8::from(*repeat))
                .u32(*wait_ms)
                .done(),
            Self::ListCameras => Out::new(12).done(),
            Self::Fetch { id } => Out::new(14).u32(*id).done(),
            Self::StartCamera {
                choice,
                hardware,
                bitrate,
                preview,
            } => {
                let mut out = Out::new(13);
                match choice {
                    CameraChoice::First => out.u8(1),
                    CameraChoice::Device(id) => out.u8(2).text(id),
                    CameraChoice::Test => out.u8(3),
                };
                out.u8(u8::from(*hardware))
                    .u32(*bitrate)
                    .u32(*preview)
                    .done()
            }
        }
    }

    /// The message in `body`, checked field by field.
    pub fn decode(body: &[u8]) -> Result<Self, Error> {
        let mut input = In { rest: body };
        let request = match input.u8()? {
            1 => {
                input.magic()?;
                Self::Hello {
                    version: input.u16()?,
                }
            }
            2 => Self::OpenDecoder {
                codec: Codec::from_byte(input.u8()?)?,
                width: input.side("width")?,
                height: input.side("height")?,
                hardware: input.flag()?,
            },
            3 => Self::Decode {
                id: input.u32()?,
                keyframe: input.flag()?,
                show: input.flag()?,
                data: input.bytes(MAX_MESSAGE)?,
            },
            6 => Self::SetBitrate {
                id: input.u32()?,
                bitrate: input.u32()?,
            },
            7 => Self::Close { id: input.u32()? },
            8 => Self::SetOutputSize {
                id: input.u32()?,
                width: input.side("width")?,
                height: input.side("height")?,
            },
            9 => Self::ListSources,
            10 => Self::StartShare {
                choice: match input.u8()? {
                    1 => ShareChoice::System {
                        again: input.flag()?,
                    },
                    2 => ShareChoice::Source(input.text()?),
                    3 => ShareChoice::Test,
                    _ => return Err(Error::BadValue("share choice")),
                },
                hardware: input.flag()?,
                bitrate: input.u32()?,
                restore: input.text()?,
            },
            11 => Self::NextFrame {
                id: input.u32()?,
                force_keyframe: input.flag()?,
                repeat: input.flag()?,
                wait_ms: match input.u32()? {
                    wait if wait <= MAX_WAIT_MS => wait,
                    _ => return Err(Error::BadValue("wait")),
                },
            },
            12 => Self::ListCameras,
            13 => Self::StartCamera {
                choice: match input.u8()? {
                    1 => CameraChoice::First,
                    2 => CameraChoice::Device(input.text()?),
                    3 => CameraChoice::Test,
                    _ => return Err(Error::BadValue("camera choice")),
                },
                hardware: input.flag()?,
                bitrate: input.u32()?,
                preview: match input.u32()? {
                    width if width <= MAX_PREVIEW_SIDE => width,
                    _ => return Err(Error::BadValue("preview")),
                },
            },
            14 => Self::Fetch { id: input.u32()? },
            tag => return Err(Error::UnknownTag(tag)),
        };
        input.end()?;
        Ok(request)
    }
}

impl Reply {
    /// The message's bytes, for [`write_frame`].
    pub fn encode(&self) -> Vec<u8> {
        match self {
            Self::Welcome {
                version,
                backend,
                capabilities,
            } => {
                let mut out = Out::new(1);
                out.0.extend_from_slice(&MAGIC);
                out.u16(*version).text(backend);
                let listed = &capabilities[..capabilities.len().min(MAX_CAPABILITIES)];
                out.u16(u16::try_from(listed.len()).unwrap_or(0));
                for capability in listed {
                    out.u8(capability.codec.to_byte())
                        .u8(capability.direction.to_byte())
                        .u32(capability.max_width)
                        .u32(capability.max_height);
                }
                out.done()
            }
            Self::Opened { id } => Out::new(2).u32(*id).done(),
            Self::Picture(decoded) => Out::new(PICTURE)
                .u32(decoded.source.0)
                .u32(decoded.source.1)
                .u8(u8::from(decoded.hardware))
                .planes(&decoded.planes)
                .done(),
            Self::NoPicture => Out::new(4).done(),
            // 5 was an encoded picture of the app's (version 4).
            Self::Done => Out::new(6).done(),
            Self::Failed { kind, detail } => Out::new(7).u8(kind.to_byte()).text(detail).done(),
            Self::Sources { dialog, sources } => {
                let mut out = Out::new(8);
                out.u8(u8::from(*dialog));
                let listed = &sources[..sources.len().min(MAX_SOURCES)];
                out.u16(u16::try_from(listed.len()).unwrap_or(0));
                for source in listed {
                    out.text(&source.id)
                        .text(&source.name)
                        .u8(source.kind.to_byte());
                }
                out.done()
            }
            Self::Started { id, restore } => Out::new(9).u32(*id).text(restore).done(),
            Self::Frame(frame) => {
                let mut out = Out::new(10);
                out.u8(u8::from(frame.keyframe))
                    .u8(u8::from(frame.hardware))
                    .u32(frame.width)
                    .u32(frame.height)
                    .u32(frame.age_us)
                    .bytes(&frame.data);
                match &frame.preview {
                    Some(preview) => out.u8(1).planes(preview),
                    None => out.u8(0),
                };
                out.done()
            }
            Self::Problem { problem, detail } => {
                Out::new(11).u8(problem.to_byte()).text(detail).done()
            }
            Self::Kept => Out::new(12).done(),
            Self::Unchanged => Out::new(13).done(),
        }
    }

    /// The message in `body`, checked field by field; a picture's planes
    /// are checked against its size ([`Planes::check`]).
    pub fn decode(body: &[u8]) -> Result<Self, Error> {
        let mut input = In { rest: body };
        let reply = match input.u8()? {
            1 => {
                input.magic()?;
                let version = input.u16()?;
                let backend = input.text()?;
                let count = usize::from(input.u16()?);
                if count > MAX_CAPABILITIES {
                    return Err(Error::TooLarge(count));
                }
                let mut capabilities = Vec::with_capacity(count);
                for _ in 0..count {
                    capabilities.push(Capability {
                        codec: Codec::from_byte(input.u8()?)?,
                        direction: Direction::from_byte(input.u8()?)?,
                        max_width: input.side("maximum width")?,
                        max_height: input.side("maximum height")?,
                    });
                }
                Self::Welcome {
                    version,
                    backend,
                    capabilities,
                }
            }
            2 => Self::Opened { id: input.u32()? },
            3 => {
                let source = (input.u32()?, input.u32()?);
                let hardware = input.flag()?;
                let decoded = Decoded {
                    planes: input.planes()?,
                    source,
                    hardware,
                };
                decoded.check()?;
                Self::Picture(decoded)
            }
            4 => Self::NoPicture,
            6 => Self::Done,
            7 => Self::Failed {
                kind: FailKind::from_byte(input.u8()?)?,
                detail: input.text()?,
            },
            8 => {
                let dialog = input.flag()?;
                let count = usize::from(input.u16()?);
                if count > MAX_SOURCES {
                    return Err(Error::TooLarge(count));
                }
                let mut sources = Vec::with_capacity(count);
                for _ in 0..count {
                    sources.push(Source {
                        id: input.text()?,
                        name: input.text()?,
                        kind: SourceKind::from_byte(input.u8()?)?,
                    });
                }
                Self::Sources { dialog, sources }
            }
            9 => Self::Started {
                id: input.u32()?,
                restore: input.text()?,
            },
            10 => {
                let frame = CapturedFrame {
                    keyframe: input.flag()?,
                    hardware: input.flag()?,
                    width: input.side("width")?,
                    height: input.side("height")?,
                    age_us: input.u32()?,
                    data: input.bytes(MAX_MESSAGE)?,
                    preview: if input.flag()? {
                        Some(input.planes()?)
                    } else {
                        None
                    },
                };
                if frame.width == 0 || frame.height == 0 || frame.data.is_empty() {
                    return Err(Error::BadValue("captured frame"));
                }
                if frame.preview.as_ref().is_some_and(|preview| {
                    preview.width > MAX_PREVIEW_SIDE || preview.height > MAX_PREVIEW_SIDE
                }) {
                    return Err(Error::BadValue("preview size"));
                }
                Self::Frame(frame)
            }
            11 => Self::Problem {
                problem: CaptureProblem::from_byte(input.u8()?)?,
                detail: input.text()?,
            },
            12 => Self::Kept,
            13 => Self::Unchanged,
            tag => return Err(Error::UnknownTag(tag)),
        };
        input.end()?;
        Ok(reply)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn picture(width: u32, height: u32) -> Planes {
        let (cw, ch) = chroma_size(width, height);
        let luma = usize::try_from(width * height).expect("small");
        let chroma = usize::try_from(cw * ch).expect("small");
        Planes {
            width,
            height,
            y: (0..luma).map(|i| (i % 251) as u8).collect(),
            u: vec![100; chroma],
            v: vec![200; chroma],
        }
    }

    /// A picture from software at its stream's own size.
    fn decoded(planes: Planes) -> Decoded {
        Decoded {
            source: (planes.width, planes.height),
            hardware: false,
            planes,
        }
    }

    fn requests() -> Vec<Request> {
        vec![
            Request::Hello { version: VERSION },
            Request::OpenDecoder {
                codec: Codec::H264,
                width: 1920,
                height: 1080,
                hardware: true,
            },
            Request::Decode {
                id: 7,
                keyframe: true,
                show: true,
                data: vec![0, 0, 0, 1, 0x65, 1, 2, 3],
            },
            Request::Decode {
                id: 7,
                keyframe: false,
                show: false,
                data: vec![0, 0, 0, 1, 0x41, 9],
            },
            Request::Fetch { id: 7 },
            Request::SetBitrate {
                id: 2,
                bitrate: 900_000,
            },
            Request::Close { id: 7 },
            Request::SetOutputSize {
                id: 7,
                width: 960,
                height: 540,
            },
            Request::ListSources,
            Request::StartShare {
                choice: ShareChoice::System { again: true },
                hardware: true,
                bitrate: 600_000,
                restore: String::new(),
            },
            Request::StartShare {
                choice: ShareChoice::Source("x11:0:0:1920:1080".into()),
                hardware: false,
                bitrate: 2_500_000,
                restore: "a-token".into(),
            },
            Request::StartShare {
                choice: ShareChoice::Test,
                hardware: true,
                bitrate: 1,
                restore: String::new(),
            },
            Request::NextFrame {
                id: 3,
                force_keyframe: true,
                repeat: false,
                wait_ms: MAX_WAIT_MS,
            },
            Request::ListCameras,
            Request::StartCamera {
                choice: CameraChoice::First,
                hardware: true,
                bitrate: 600_000,
                preview: 320,
            },
            Request::StartCamera {
                choice: CameraChoice::Device("v4l2:/dev/video2".into()),
                hardware: false,
                bitrate: 150_000,
                preview: 0,
            },
            Request::StartCamera {
                choice: CameraChoice::Test,
                hardware: true,
                bitrate: 1,
                preview: MAX_PREVIEW_SIDE,
            },
        ]
    }

    fn replies() -> Vec<Reply> {
        vec![
            Reply::Welcome {
                version: VERSION,
                backend: "vaapi (AMD Radeon)".into(),
                capabilities: vec![Capability {
                    codec: Codec::H264,
                    direction: Direction::Decode,
                    max_width: 4096,
                    max_height: 4096,
                }],
            },
            Reply::Opened { id: 1 },
            Reply::Picture(decoded(picture(3, 3))),
            Reply::NoPicture,
            Reply::Kept,
            Reply::Unchanged,
            Reply::Done,
            Reply::Failed {
                kind: FailKind::Broken,
                detail: "no slice".into(),
            },
            Reply::Sources {
                dialog: true,
                sources: Vec::new(),
            },
            Reply::Sources {
                dialog: false,
                sources: vec![
                    Source {
                        id: "screen:1".into(),
                        name: "DP-1 (2560×1440)".into(),
                        kind: SourceKind::Screen,
                    },
                    Source {
                        id: "window:7".into(),
                        name: "Release plan.md — Editor".into(),
                        kind: SourceKind::Window,
                    },
                    Source {
                        id: "v4l2:/dev/video0".into(),
                        name: "Integrated Camera".into(),
                        kind: SourceKind::Camera,
                    },
                ],
            },
            Reply::Started {
                id: 3,
                restore: "a-token".into(),
            },
            Reply::Frame(CapturedFrame {
                keyframe: true,
                hardware: false,
                width: 1280,
                height: 720,
                age_us: 4_000,
                data: vec![0, 0, 0, 1, 0x67, 0, 0, 0, 1, 0x68, 0, 0, 0, 1, 0x65],
                preview: None,
            }),
            Reply::Frame(CapturedFrame {
                keyframe: false,
                hardware: true,
                width: 640,
                height: 480,
                age_us: 1_200,
                data: vec![0, 0, 0, 1, 0x41, 0x9a],
                preview: Some(picture(320, 240)),
            }),
            Reply::Problem {
                problem: CaptureProblem::Ended,
                detail: "PipeWire: unconnected".into(),
            },
            Reply::Problem {
                problem: CaptureProblem::Busy,
                detail: "/dev/video0: Device or resource busy".into(),
            },
        ]
    }

    #[test]
    fn capture_messages_check_their_fields() {
        // A wait past the most, an unknown choice, problem or kind.
        let mut next = Request::NextFrame {
            id: 1,
            force_keyframe: false,
            repeat: false,
            wait_ms: 0,
        }
        .encode();
        next[7..11].copy_from_slice(&(MAX_WAIT_MS + 1).to_le_bytes());
        assert!(matches!(
            Request::decode(&next),
            Err(Error::BadValue("wait"))
        ));
        let mut start = Request::StartShare {
            choice: ShareChoice::Test,
            hardware: false,
            bitrate: 0,
            restore: String::new(),
        }
        .encode();
        start[1] = 9;
        assert!(matches!(
            Request::decode(&start),
            Err(Error::BadValue("share choice"))
        ));
        let mut camera = Request::StartCamera {
            choice: CameraChoice::Test,
            hardware: false,
            bitrate: 0,
            preview: 0,
        }
        .encode();
        camera[1] = 9;
        assert!(matches!(
            Request::decode(&camera),
            Err(Error::BadValue("camera choice"))
        ));
        // A self-view wider than allowed is refused, asked for or sent.
        let mut wide = Request::StartCamera {
            choice: CameraChoice::Test,
            hardware: false,
            bitrate: 0,
            preview: 0,
        }
        .encode();
        let last = wide.len() - 4;
        wide[last..].copy_from_slice(&(MAX_PREVIEW_SIDE + 1).to_le_bytes());
        assert!(matches!(
            Request::decode(&wide),
            Err(Error::BadValue("preview"))
        ));
        let large = Reply::Frame(CapturedFrame {
            keyframe: true,
            hardware: false,
            width: 640,
            height: 480,
            age_us: 0,
            data: vec![1],
            preview: Some(picture(MAX_PREVIEW_SIDE + 2, 4)),
        });
        assert!(matches!(
            Reply::decode(&large.encode()),
            Err(Error::BadValue("preview size"))
        ));
        // A self-view whose planes lie about its size.
        let mut lying = picture(8, 8);
        lying.y.pop();
        let lying = Reply::Frame(CapturedFrame {
            keyframe: true,
            hardware: false,
            width: 640,
            height: 480,
            age_us: 0,
            data: vec![1],
            preview: Some(lying),
        });
        assert!(Reply::decode(&lying.encode()).is_err());
        assert!(matches!(
            Reply::decode(&[11, 42, 0, 0, 0, 0]),
            Err(Error::BadValue("capture problem"))
        ));
        let mut sources = Reply::Sources {
            dialog: false,
            sources: vec![Source {
                id: "a".into(),
                name: "b".into(),
                kind: SourceKind::Window,
            }],
        }
        .encode();
        let last = sources.len() - 1;
        sources[last] = 4;
        assert!(matches!(
            Reply::decode(&sources),
            Err(Error::BadValue("source kind"))
        ));
        // A list longer than allowed is refused before it is read.
        let mut many = vec![8, 0];
        many.extend_from_slice(&u16::try_from(MAX_SOURCES + 1).expect("small").to_le_bytes());
        assert!(matches!(Reply::decode(&many), Err(Error::TooLarge(_))));
        // And one that long is cut to the most when written.
        let long = Reply::Sources {
            dialog: false,
            sources: vec![
                Source {
                    id: "x".into(),
                    name: "y".into(),
                    kind: SourceKind::Screen,
                };
                MAX_SOURCES + 5
            ],
        };
        let Reply::Sources { sources, .. } = Reply::decode(&long.encode()).expect("decodes") else {
            panic!("sources");
        };
        assert_eq!(sources.len(), MAX_SOURCES);
        // A frame with nothing in it, or of no size.
        for frame in [
            CapturedFrame {
                keyframe: false,
                hardware: false,
                width: 16,
                height: 16,
                age_us: 0,
                data: Vec::new(),
                preview: None,
            },
            CapturedFrame {
                keyframe: false,
                hardware: false,
                width: 0,
                height: 16,
                age_us: 0,
                data: vec![1],
                preview: None,
            },
        ] {
            assert!(Reply::decode(&Reply::Frame(frame).encode()).is_err());
        }
    }

    #[test]
    fn every_message_survives_the_pipe() {
        let mut pipe = Vec::new();
        for (seq, request) in requests().iter().enumerate() {
            write_frame(&mut pipe, seq as u32, &request.encode()).expect("written");
        }
        for (seq, reply) in replies().iter().enumerate() {
            write_frame(&mut pipe, 100 + seq as u32, &reply.encode()).expect("written");
        }
        let mut input = pipe.as_slice();
        for (seq, request) in requests().iter().enumerate() {
            let frame = read_frame(&mut input).expect("reads").expect("a frame");
            assert_eq!(frame.seq, seq as u32);
            assert_eq!(&Request::decode(&frame.body).expect("decodes"), request);
        }
        for (seq, reply) in replies().iter().enumerate() {
            let frame = read_frame(&mut input).expect("reads").expect("a frame");
            assert_eq!(frame.seq, 100 + seq as u32);
            assert_eq!(&Reply::decode(&frame.body).expect("decodes"), reply);
        }
        assert!(read_frame(&mut input).expect("a clean end").is_none());
    }

    #[test]
    fn broken_framing_is_an_error_not_a_panic_or_a_huge_allocation() {
        // Ends inside the length, inside the sequence number, inside the body.
        for cut in [1, 3, 6, 9] {
            let mut pipe = Vec::new();
            write_frame(&mut pipe, 1, &Reply::Done.encode()).expect("written");
            write_frame(&mut pipe, 2, &Reply::Opened { id: 3 }.encode()).expect("written");
            let short = &pipe[..pipe.len() - cut];
            let mut input = short;
            read_frame(&mut input).expect("the first is whole");
            assert!(matches!(read_frame(&mut input), Err(Error::Truncated)));
        }
        // A length of 4 GiB is refused before reading or allocating.
        let mut huge = u32::MAX.to_le_bytes().to_vec();
        huge.extend_from_slice(&[0; 16]);
        assert!(matches!(
            read_frame(&mut huge.as_slice()),
            Err(Error::TooLarge(_))
        ));
        // A frame with no tag.
        let mut empty = 4u32.to_le_bytes().to_vec();
        empty.extend_from_slice(&1u32.to_le_bytes());
        assert!(matches!(
            read_frame(&mut empty.as_slice()),
            Err(Error::Truncated)
        ));
        // Too large to send.
        assert!(matches!(
            write_frame(&mut Vec::new(), 0, &vec![0; MAX_MESSAGE]),
            Err(Error::TooLarge(_))
        ));
    }

    #[test]
    fn malformed_messages_are_errors() {
        assert!(matches!(Request::decode(&[]), Err(Error::Truncated)));
        assert!(matches!(Request::decode(&[99]), Err(Error::UnknownTag(99))));
        assert!(matches!(Reply::decode(&[0]), Err(Error::UnknownTag(0))));
        // A hello that is not ours.
        assert!(matches!(
            Request::decode(&[1, b'H', b'T', b'T', b'P', 1, 0]),
            Err(Error::BadValue("magic"))
        ));
        // Every message cut short anywhere is an error, and so is one
        // with a byte too many.
        for request in requests() {
            let body = request.encode();
            for n in 0..body.len() {
                assert!(
                    Request::decode(&body[..n]).is_err(),
                    "{request:?} cut at {n}"
                );
            }
            let mut longer = body.clone();
            longer.push(0);
            assert!(matches!(Request::decode(&longer), Err(Error::Trailing(1))));
        }
        for reply in replies() {
            let body = reply.encode();
            for n in 0..body.len() {
                assert!(Reply::decode(&body[..n]).is_err(), "{reply:?} cut at {n}");
            }
        }
        // An unknown codec, a flag that is neither 0 nor 1, a size over
        // the bound, a failure kind from a later version.
        assert!(Request::decode(&[2, 9, 0, 0, 0, 0, 0, 0, 0, 0, 0]).is_err());
        let mut decode = Request::Decode {
            id: 1,
            keyframe: false,
            show: true,
            data: vec![],
        }
        .encode();
        for flag in [5, 6] {
            let mut bad = decode.clone();
            bad[flag] = 2;
            assert!(matches!(
                Request::decode(&bad),
                Err(Error::BadValue("flag"))
            ));
        }
        decode[6] = 0;
        assert!(matches!(
            Request::decode(&decode),
            Ok(Request::Decode { show: false, .. })
        ));
        let open = Request::OpenDecoder {
            codec: Codec::H264,
            width: MAX_SIDE + 1,
            height: 1,
            hardware: false,
        };
        assert!(matches!(
            Request::decode(&open.encode()),
            Err(Error::BadValue("width"))
        ));
        assert!(matches!(
            Reply::decode(&[7, 42, 0, 0, 0, 0]),
            Err(Error::BadValue("failure kind"))
        ));
        // Text that is not UTF-8.
        assert!(matches!(
            Reply::decode(&[7, 3, 2, 0, 0, 0, 0xff, 0xfe]),
            Err(Error::BadValue("text"))
        ));
        // A byte string claiming more than there is.
        assert!(matches!(
            Request::decode(&[3, 1, 0, 0, 0, 0, 1, 0xff, 0xff, 0xff, 0x00]),
            Err(Error::Truncated)
        ));
    }

    #[test]
    fn pictures_whose_planes_do_not_match_their_size_are_refused() {
        assert!(picture(1920, 1080).check().is_ok());
        assert!(picture(5, 3).check().is_ok(), "odd sizes round chroma up");
        let mut short = picture(4, 4);
        short.y.pop();
        assert!(short.check().is_err());
        let mut long = picture(4, 4);
        long.v.push(0);
        assert!(long.check().is_err());
        let zero = Planes {
            width: 0,
            height: 4,
            y: vec![],
            u: vec![],
            v: vec![],
        };
        assert!(zero.check().is_err());
        // A picture reply whose planes lie about the size never decodes.
        let lying = Reply::Picture(Decoded {
            source: (8, 4),
            ..decoded(Planes {
                width: 8,
                ..picture(4, 4)
            })
        });
        assert!(matches!(
            Reply::decode(&lying.encode()),
            Err(Error::BadValue("plane length"))
        ));
        // Nor does a picture larger than the stream it came from, or
        // from a stream of no size.
        for source in [(3, 4), (4, 3), (0, 4), (MAX_SIDE + 1, 4)] {
            let larger = Reply::Picture(Decoded {
                source,
                ..decoded(picture(4, 4))
            });
            assert!(Reply::decode(&larger.encode()).is_err(), "{source:?}");
            let mut streamed = Vec::new();
            write_reply(&mut streamed, 1, &larger).expect("written");
            assert!(read_reply(&mut streamed.as_slice()).is_err(), "{source:?}");
        }
        // Shrunk below its source, and from the GPU, it does.
        let shrunk = Reply::Picture(Decoded {
            source: (1920, 1080),
            hardware: true,
            planes: picture(960, 540),
        });
        assert_eq!(
            Reply::decode(&shrunk.encode()).expect("decodes"),
            shrunk.clone()
        );
    }

    #[test]
    fn pictures_stream_through_without_a_whole_message_copy() {
        let mut pipe = Vec::new();
        let sent = replies();
        for (seq, reply) in sent.iter().enumerate() {
            write_reply(&mut pipe, seq as u32, reply).expect("written");
        }
        let mut input = pipe.as_slice();
        for (seq, reply) in sent.iter().enumerate() {
            let (got_seq, got) = read_reply(&mut input).expect("reads").expect("a reply");
            assert_eq!((got_seq, &got), (seq as u32, reply));
        }
        assert!(read_reply(&mut input).expect("a clean end").is_none());
        // Both ways of writing a picture read both ways.
        let picture = Reply::Picture(Decoded {
            source: (14, 10),
            hardware: true,
            planes: picture(7, 5),
        });
        let mut framed = Vec::new();
        write_frame(&mut framed, 9, &picture.encode()).expect("written");
        let mut streamed = Vec::new();
        write_reply(&mut streamed, 9, &picture).expect("written");
        assert_eq!(framed, streamed);
        // Cut anywhere, a streamed picture is an error.
        for n in 1..streamed.len() {
            assert!(read_reply(&mut &streamed[..n]).is_err(), "cut at {n}");
        }
        // Planes that lie about their length are refused before reading
        // (length, sequence, tag, source, flag, size: 26 bytes in).
        let mut lying = streamed.clone();
        lying[26] = lying[26].wrapping_add(1);
        assert!(matches!(
            read_reply(&mut lying.as_slice()),
            Err(Error::BadValue("plane length"))
        ));
        // A size over the bound.
        let mut huge = streamed.clone();
        huge[18..22].copy_from_slice(&(MAX_SIDE + 1).to_le_bytes());
        assert!(matches!(
            read_reply(&mut huge.as_slice()),
            Err(Error::BadValue("picture size"))
        ));
        // A flag that is neither 0 nor 1.
        let mut bad_flag = streamed;
        bad_flag[17] = 2;
        assert!(matches!(
            read_reply(&mut bad_flag.as_slice()),
            Err(Error::BadValue("flag"))
        ));
    }

    #[test]
    fn requests_read_one_by_one_and_a_bad_one_leaves_the_next_whole() {
        let mut pipe = Vec::new();
        let sent = requests();
        for (seq, request) in sent.iter().enumerate() {
            write_request(&mut pipe, seq as u32, request).expect("written");
        }
        let mut input = pipe.as_slice();
        for (seq, request) in sent.iter().enumerate() {
            let (got_seq, got) = read_request(&mut input).expect("reads").expect("a request");
            assert_eq!(
                (got_seq, got.expect("decodes")),
                (seq as u32, request.clone())
            );
        }
        assert!(read_request(&mut input).expect("a clean end").is_none());
        // A message that does not decode leaves the next one readable.
        let mut pipe = Vec::new();
        write_frame(&mut pipe, 1, &[42]).expect("written");
        write_request(&mut pipe, 2, &sent[4]).expect("written");
        let mut input = pipe.as_slice();
        let (_, bad) = read_request(&mut input).expect("framed").expect("a frame");
        assert!(matches!(bad, Err(Error::UnknownTag(42))));
        let (seq, good) = read_request(&mut input).expect("framed").expect("a frame");
        assert_eq!((seq, good.expect("decodes")), (2, sent[4].clone()));
        // Version 4's encode request and encoded reply are gone.
        assert!(matches!(
            Request::decode(&[5, 1, 0, 0, 0, 0]),
            Err(Error::UnknownTag(5))
        ));
        assert!(matches!(Reply::decode(&[5, 0]), Err(Error::UnknownTag(5))));
    }

    #[test]
    fn output_sizes_cover_the_box_keep_the_shape_and_never_grow() {
        // No box yet, or one as large: the source.
        assert_eq!(output_size((1920, 1080), (0, 0)), (1920, 1080));
        assert_eq!(output_size((1920, 1080), (1920, 1080)), (1920, 1080));
        assert_eq!(output_size((1920, 1080), (2560, 1440)), (1920, 1080));
        assert_eq!(output_size((1920, 1080), (3000, 10)), (1920, 1080));
        // Half and a third.
        assert_eq!(output_size((1920, 1080), (960, 540)), (960, 540));
        assert_eq!(output_size((1920, 1080), (640, 360)), (640, 360));
        // A box of another shape: the picture covers it both ways.
        assert_eq!(output_size((1920, 1080), (960, 200)), (960, 540));
        assert_eq!(output_size((1920, 1080), (100, 540)), (960, 540));
        // A square camera in a 4:3 tile.
        assert_eq!(output_size((480, 480), (256, 192)), (256, 256));
        // Odd results round up to even, odd sources keep their own size.
        assert_eq!(output_size((1920, 1080), (961, 100)), (962, 542));
        assert_eq!(output_size((321, 181), (0, 0)), (321, 181));
        assert_eq!(output_size((321, 181), (161, 10)), (162, 92));
        for source in [(1920, 1080), (480, 480), (1280, 720), (321, 181), (2, 2)] {
            for fit in [(1, 1), (7, 3), (200, 150), (959, 541), (5000, 5000)] {
                let (w, h) = output_size(source, fit);
                assert!(w <= source.0 && h <= source.1, "{source:?} in {fit:?}");
                if (w, h) != source {
                    assert!(w % 2 == 0 && h % 2 == 0, "{source:?} in {fit:?}");
                    assert!(w >= fit.0.min(source.0) && h >= fit.1.min(source.1));
                    // The shape to within the rounding.
                    let a = f64::from(w) / f64::from(h);
                    let b = f64::from(source.0) / f64::from(source.1);
                    assert!(
                        (a / b - 1.0).abs() < 0.05 || w <= 4 || h <= 4,
                        "{source:?} in {fit:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn capabilities_cover_what_they_say() {
        let decode = Capability {
            codec: Codec::H264,
            direction: Direction::Decode,
            max_width: 1920,
            max_height: 1088,
        };
        assert!(decode.covers(Codec::H264, Direction::Decode, 1920, 1080));
        assert!(!decode.covers(Codec::H264, Direction::Encode, 640, 480));
        assert!(!decode.covers(Codec::H264, Direction::Decode, 2560, 1440));
    }

    #[test]
    fn long_text_is_cut_on_a_character_boundary() {
        let detail = "é".repeat(MAX_TEXT);
        let reply = Reply::Failed {
            kind: FailKind::Device,
            detail,
        };
        let Reply::Failed { detail, .. } = Reply::decode(&reply.encode()).expect("decodes") else {
            panic!("a failure");
        };
        assert!(detail.len() <= MAX_TEXT && detail.chars().all(|c| c == 'é'));
    }
}
