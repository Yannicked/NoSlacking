//! What a platform's video API must provide to serve the app: one
//! [`Backend`] per API (VA-API today; VideoToolbox, Media Foundation,
//! Vulkan Video and V4L2 are planned, see docs/research/huddle-video.md),
//! opening [`Decoder`]s and [`Encoder`]s.
//!
//! A back end takes whole frames (access units, Annex B) and gives whole
//! I420 pictures, so stateful APIs (VideoToolbox, Media Foundation, V4L2
//! stateful decoders) and stateless ones (VA-API, Vulkan Video, V4L2
//! stateless decoders, which need the Linux build's `h264` module for
//! parsing and reference bookkeeping) fit the same shape. So does the
//! software decoder ([`crate::software`]), which [`open_decoder`] puts
//! behind the GPU's, or in its place.

use noslacking_video_ipc::{Capability, Codec, Decoded, FailKind, Planes};

use crate::software::Software;

/// Why a back end could not do what was asked; crosses the pipe as a
/// failure reply.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Failure {
    /// How it failed, which decides what the app does next.
    pub kind: FailKind,
    /// What happened, for the app's log.
    pub detail: String,
}

impl Failure {
    /// A failure of `kind` with `detail`.
    pub fn new(kind: FailKind, detail: impl Into<String>) -> Self {
        Self {
            kind,
            detail: detail.into(),
        }
    }

    /// The back end cannot do this; the app uses software.
    pub fn unsupported(detail: impl Into<String>) -> Self {
        Self::new(FailKind::Unsupported, detail)
    }

    /// The frame did not decode.
    pub fn broken(detail: impl Into<String>) -> Self {
        Self::new(FailKind::Broken, detail)
    }

    /// The decoder cannot go on until a keyframe.
    pub fn need_keyframe(detail: impl Into<String>) -> Self {
        Self::new(FailKind::NeedKeyframe, detail)
    }

    /// The device or driver failed.
    pub fn device(detail: impl Into<String>) -> Self {
        Self::new(FailKind::Device, detail)
    }
}

/// An encoded frame.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Encoded {
    /// Whether it is an IDR.
    pub keyframe: bool,
    /// Its NAL units, Annex B.
    pub data: Vec<u8>,
}

/// One platform video API.
pub trait Backend {
    /// Its name and device, for the app's log.
    fn name(&self) -> String;
    /// What it can do; empty when nothing.
    fn capabilities(&self) -> Vec<Capability>;
    /// A decoder for one stream of `codec`, about `width`×`height`.
    fn open_decoder(
        &mut self,
        codec: Codec,
        width: u32,
        height: u32,
    ) -> Result<Box<dyn Decoder>, Failure>;
    /// An encoder of `width`×`height` pictures at `fps` and `bitrate`
    /// bits a second.
    fn open_encoder(
        &mut self,
        codec: Codec,
        width: u32,
        height: u32,
        fps: u32,
        bitrate: u32,
    ) -> Result<Box<dyn Encoder>, Failure> {
        let _ = (codec, width, height, fps, bitrate);
        Err(Failure::unsupported("this back end does not encode"))
    }
}

/// One stream's decoder.
pub trait Decoder {
    /// Decodes one frame: its picture, or none for a frame of parameter
    /// sets only. Shrunk to cover the output box, if one was set.
    fn decode(&mut self, frame: &[u8], keyframe: bool) -> Result<Option<Decoded>, Failure>;
    /// The box pictures from now on should cover
    /// (`noslacking_video_ipc::output_size`); 0×0 for their own size.
    fn set_output_size(&mut self, width: u32, height: u32);
}

/// One stream's encoder.
pub trait Encoder {
    /// Encodes one picture.
    fn encode(&mut self, picture: &Planes, force_keyframe: bool) -> Result<Encoded, Failure>;
    /// A new target bit rate.
    fn set_bitrate(&mut self, bitrate: u32) -> Result<(), Failure>;
}

/// A decoder for one stream of `codec`, about `width`×`height`: the
/// GPU's from `backend` if `hardware` is wanted and it has one for the
/// stream, with software behind it; else software alone. Never fails:
/// software decodes whatever the GPU cannot.
pub fn open_decoder(
    backend: &mut dyn Backend,
    codec: Codec,
    width: u32,
    height: u32,
    hardware: bool,
) -> Box<dyn Decoder> {
    let Codec::H264 = codec;
    if !hardware {
        return Box::new(Software::new());
    }
    match backend.open_decoder(codec, width, height) {
        Ok(gpu) => Box::new(Fallback {
            gpu: Some(gpu),
            software: None,
            fit: (0, 0),
        }),
        Err(failure) => {
            eprintln!(
                "noslacking-video: {width}x{height} in software ({})",
                failure.detail
            );
            Box::new(Software::new())
        }
    }
}

/// The GPU's decoder, and software once it fails: on a frame it cannot
/// decode (`Unsupported`), a device failure, or a keyframe it breaks on.
/// The keyframe in hand then goes to software at once; any other frame
/// asks for one. A broken frame between keyframes stays on the GPU,
/// which starts over at the next keyframe as software would.
struct Fallback {
    gpu: Option<Box<dyn Decoder>>,
    /// Made when the GPU fails.
    software: Option<Software>,
    fit: (u32, u32),
}

impl Decoder for Fallback {
    fn decode(&mut self, frame: &[u8], keyframe: bool) -> Result<Option<Decoded>, Failure> {
        if let Some(gpu) = &mut self.gpu {
            let failure = match gpu.decode(frame, keyframe) {
                Ok(decoded) => return Ok(decoded),
                Err(failure) => failure,
            };
            let stay = match failure.kind {
                FailKind::NeedKeyframe => true,
                FailKind::Broken => !keyframe,
                FailKind::Unsupported
                | FailKind::Device
                | FailKind::Protocol
                | FailKind::UnknownId => false,
            };
            if stay {
                return Err(failure);
            }
            eprintln!(
                "noslacking-video: the GPU failed ({:?}: {}): software from here on",
                failure.kind, failure.detail
            );
            self.gpu = None;
            let mut software = Software::new();
            software.set_output_size(self.fit.0, self.fit.1);
            self.software = Some(software);
            if !keyframe {
                return Err(Failure::need_keyframe("software takes over at a keyframe"));
            }
        }
        match &mut self.software {
            Some(software) => software.decode(frame, keyframe),
            None => Err(Failure::device("no decoder")),
        }
    }

    fn set_output_size(&mut self, width: u32, height: u32) {
        self.fit = (width, height);
        if let Some(gpu) = &mut self.gpu {
            gpu.set_output_size(width, height);
        }
        if let Some(software) = &mut self.software {
            software.set_output_size(width, height);
        }
    }
}

/// The back end of a system with no usable hardware: it can do nothing,
/// and says why in its name.
#[derive(Debug)]
pub struct Nothing {
    why: String,
}

impl Nothing {
    /// A back end that can do nothing, named `why`.
    pub fn new(why: &str) -> Self {
        Self {
            why: why.to_owned(),
        }
    }
}

impl Backend for Nothing {
    fn name(&self) -> String {
        self.why.clone()
    }

    fn capabilities(&self) -> Vec<Capability> {
        Vec::new()
    }

    fn open_decoder(&mut self, _: Codec, _: u32, _: u32) -> Result<Box<dyn Decoder>, Failure> {
        Err(Failure::unsupported(self.why.clone()))
    }
}
