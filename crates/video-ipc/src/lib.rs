//! What NoSlacking and `noslacking-video`, its hardware video helper, say
//! to each other over the helper's standard input and output.
//!
//! The helper drives the GPU's video engine through the platform's C
//! libraries (VA-API on Linux), which takes `unsafe` code and trusts
//! drivers with whatever a stranger's stream holds. It runs as its own
//! process so the app keeps `forbid(unsafe_code)` and a crash or a hang
//! only costs the helper. This crate is the one thing both sides share:
//! the framing and the messages, in plain safe Rust.
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

/// This protocol's version. Both sides must have the same; the app uses
/// software video with a helper of another version.
pub const VERSION: u16 = 1;

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
    let Reply::Picture(planes) = reply else {
        return write_frame(out, seq, &reply.encode());
    };
    let planes_length = planes.y.len() + planes.u.len() + planes.v.len();
    // Sequence number, tag, size, three plane lengths, the planes.
    let length = 4 + 1 + 8 + 12 + planes_length;
    if length > MAX_MESSAGE {
        return Err(Error::TooLarge(length));
    }
    let mut header = Vec::with_capacity(4 + 4 + 1 + 8 + 4);
    let length = u32::try_from(length).map_err(|_| Error::TooLarge(length))?;
    header.extend_from_slice(&length.to_le_bytes());
    header.extend_from_slice(&seq.to_le_bytes());
    header.push(PICTURE);
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

/// Reads one reply and its sequence number: none when the input ended
/// cleanly between frames. A picture's planes are read straight into
/// their own vectors, each checked against the frame's length and the
/// picture's size before anything is allocated for it.
pub fn read_reply(input: &mut impl Read) -> Result<Option<(u32, Reply)>, Error> {
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
    let tag = seq_and_tag[4];
    let mut left = length - 5;
    if tag != PICTURE {
        let mut body = vec![0u8; left + 1];
        body[0] = tag;
        input.read_exact(&mut body[1..])?;
        return Ok(Some((seq, Reply::decode(&body)?)));
    }
    let mut take = |n: usize, input: &mut dyn Read| -> Result<Vec<u8>, Error> {
        if n > left {
            return Err(Error::Truncated);
        }
        left -= n;
        let mut bytes = Vec::with_capacity(n);
        input.take(n as u64).read_to_end(&mut bytes)?;
        if bytes.len() != n {
            return Err(Error::Truncated);
        }
        Ok(bytes)
    };
    let word = |bytes: Vec<u8>| -> u32 {
        let mut word = [0u8; 4];
        word.copy_from_slice(&bytes);
        u32::from_le_bytes(word)
    };
    let width = word(take(4, input)?);
    let height = word(take(4, input)?);
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
        let n = word(take(4, input)?);
        if u64::from(n) != size {
            return Err(Error::BadValue("plane length"));
        }
        planes.push(take(
            usize::try_from(n).map_err(|_| Error::TooLarge(usize::MAX))?,
            input,
        )?);
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
    Ok(Some((seq, Reply::Picture(planes))))
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

/// What the app asks the helper.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Request {
    /// The first message: who the app is and which version it speaks.
    Hello {
        /// The app's protocol version.
        version: u16,
    },
    /// A decoder for one stream; the reply is [`Reply::Opened`] or a
    /// failure. The size is a hint (the stream's SPS decides).
    OpenDecoder {
        /// The stream's format.
        codec: Codec,
        /// The expected width.
        width: u32,
        /// The expected height.
        height: u32,
    },
    /// One frame (an access unit, Annex B) for decoder `id`; the reply is
    /// a picture, no picture (parameter sets only), or a failure.
    Decode {
        /// The decoder.
        id: u32,
        /// Whether the frame holds an IDR slice.
        keyframe: bool,
        /// The frame.
        data: Vec<u8>,
    },
    /// An encoder; the reply is [`Reply::Opened`] or a failure.
    OpenEncoder {
        /// The stream's format.
        codec: Codec,
        /// The pictures' width.
        width: u32,
        /// The pictures' height.
        height: u32,
        /// Pictures a second.
        fps: u32,
        /// The target bit rate, in bits a second.
        bitrate: u32,
    },
    /// One picture for encoder `id`; the reply is [`Reply::Encoded`] or a
    /// failure.
    Encode {
        /// The encoder.
        id: u32,
        /// Make this picture a keyframe (an IDR with its parameter sets).
        force_keyframe: bool,
        /// The picture.
        picture: Planes,
    },
    /// A new target bit rate for encoder `id`; the reply is
    /// [`Reply::Done`].
    SetBitrate {
        /// The encoder.
        id: u32,
        /// Bits a second.
        bitrate: u32,
    },
    /// Decoder or encoder `id` is no longer needed; the reply is
    /// [`Reply::Done`].
    Close {
        /// The decoder or encoder.
        id: u32,
    },
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
        /// What it can do; empty when it found no hardware.
        capabilities: Vec<Capability>,
    },
    /// A decoder or encoder was opened with this number.
    Opened {
        /// Its number, for the requests that follow.
        id: u32,
    },
    /// A decoded picture.
    Picture(Planes),
    /// The frame decoded to no picture (it held parameter sets only).
    NoPicture,
    /// An encoded frame: Annex B NAL units.
    Encoded {
        /// Whether it is an IDR.
        keyframe: bool,
        /// The NAL units.
        data: Vec<u8>,
    },
    /// Done, nothing to say.
    Done,
    /// The request failed.
    Failed {
        /// How.
        kind: FailKind,
        /// What happened, for the log.
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
            } => Out::new(2)
                .u8(codec.to_byte())
                .u32(*width)
                .u32(*height)
                .done(),
            Self::Decode { id, keyframe, data } => Out::new(3)
                .u32(*id)
                .u8(u8::from(*keyframe))
                .bytes(data)
                .done(),
            Self::OpenEncoder {
                codec,
                width,
                height,
                fps,
                bitrate,
            } => Out::new(4)
                .u8(codec.to_byte())
                .u32(*width)
                .u32(*height)
                .u32(*fps)
                .u32(*bitrate)
                .done(),
            Self::Encode {
                id,
                force_keyframe,
                picture,
            } => Out::new(5)
                .u32(*id)
                .u8(u8::from(*force_keyframe))
                .planes(picture)
                .done(),
            Self::SetBitrate { id, bitrate } => Out::new(6).u32(*id).u32(*bitrate).done(),
            Self::Close { id } => Out::new(7).u32(*id).done(),
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
            },
            3 => Self::Decode {
                id: input.u32()?,
                keyframe: input.flag()?,
                data: input.bytes(MAX_MESSAGE)?,
            },
            4 => Self::OpenEncoder {
                codec: Codec::from_byte(input.u8()?)?,
                width: input.side("width")?,
                height: input.side("height")?,
                fps: input.u32()?,
                bitrate: input.u32()?,
            },
            5 => Self::Encode {
                id: input.u32()?,
                force_keyframe: input.flag()?,
                picture: input.planes()?,
            },
            6 => Self::SetBitrate {
                id: input.u32()?,
                bitrate: input.u32()?,
            },
            7 => Self::Close { id: input.u32()? },
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
            Self::Picture(planes) => Out::new(PICTURE).planes(planes).done(),
            Self::NoPicture => Out::new(4).done(),
            Self::Encoded { keyframe, data } => {
                Out::new(5).u8(u8::from(*keyframe)).bytes(data).done()
            }
            Self::Done => Out::new(6).done(),
            Self::Failed { kind, detail } => Out::new(7).u8(kind.to_byte()).text(detail).done(),
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
            3 => Self::Picture(input.planes()?),
            4 => Self::NoPicture,
            5 => Self::Encoded {
                keyframe: input.flag()?,
                data: input.bytes(MAX_MESSAGE)?,
            },
            6 => Self::Done,
            7 => Self::Failed {
                kind: FailKind::from_byte(input.u8()?)?,
                detail: input.text()?,
            },
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

    fn requests() -> Vec<Request> {
        vec![
            Request::Hello { version: VERSION },
            Request::OpenDecoder {
                codec: Codec::H264,
                width: 1920,
                height: 1080,
            },
            Request::Decode {
                id: 7,
                keyframe: true,
                data: vec![0, 0, 0, 1, 0x65, 1, 2, 3],
            },
            Request::OpenEncoder {
                codec: Codec::H264,
                width: 640,
                height: 480,
                fps: 30,
                bitrate: 1_800_000,
            },
            Request::Encode {
                id: 2,
                force_keyframe: false,
                picture: picture(5, 3),
            },
            Request::SetBitrate {
                id: 2,
                bitrate: 900_000,
            },
            Request::Close { id: 7 },
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
            Reply::Picture(picture(3, 3)),
            Reply::NoPicture,
            Reply::Encoded {
                keyframe: true,
                data: vec![0, 0, 0, 1, 0x67],
            },
            Reply::Done,
            Reply::Failed {
                kind: FailKind::Broken,
                detail: "no slice".into(),
            },
        ]
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
        assert!(Request::decode(&[2, 9, 0, 0, 0, 0, 0, 0, 0, 0]).is_err());
        let mut decode = Request::Decode {
            id: 1,
            keyframe: false,
            data: vec![],
        }
        .encode();
        decode[5] = 2;
        assert!(matches!(
            Request::decode(&decode),
            Err(Error::BadValue("flag"))
        ));
        let open = Request::OpenDecoder {
            codec: Codec::H264,
            width: MAX_SIDE + 1,
            height: 1,
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
            Request::decode(&[3, 1, 0, 0, 0, 0, 0xff, 0xff, 0xff, 0x00]),
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
        let lying = Reply::Picture(Planes {
            width: 8,
            ..picture(4, 4)
        });
        assert!(matches!(
            Reply::decode(&lying.encode()),
            Err(Error::BadValue("plane length"))
        ));
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
        let picture = Reply::Picture(picture(7, 5));
        let mut framed = Vec::new();
        write_frame(&mut framed, 9, &picture.encode()).expect("written");
        let mut streamed = Vec::new();
        write_reply(&mut streamed, 9, &picture).expect("written");
        assert_eq!(framed, streamed);
        // Cut anywhere, a streamed picture is an error.
        for n in 1..streamed.len() {
            assert!(read_reply(&mut &streamed[..n]).is_err(), "cut at {n}");
        }
        // Planes that lie about their length are refused before reading.
        let mut lying = streamed.clone();
        lying[17] = lying[17].wrapping_add(1);
        assert!(matches!(
            read_reply(&mut lying.as_slice()),
            Err(Error::BadValue("plane length"))
        ));
        // A size over the bound.
        let mut huge = streamed;
        huge[9..13].copy_from_slice(&(MAX_SIDE + 1).to_le_bytes());
        assert!(matches!(
            read_reply(&mut huge.as_slice()),
            Err(Error::BadValue("picture size"))
        ));
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
