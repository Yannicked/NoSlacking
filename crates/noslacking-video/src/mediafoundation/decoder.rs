//! The back end, and H.264 decoded on the GPU through Microsoft's
//! decoder transform with a Direct3D 11 device: what it gives is NV12 in
//! GPU memory, read back (through Media Foundation's own copy) only for
//! the pictures the app takes.

use std::rc::Rc;

use noslacking_video_ipc::{Capability, Codec, Decoded, Direction, FailKind};
use windows::Win32::Media::MediaFoundation::{
    CLSID_MSH264DecoderMFT, CODECAPI_AVDecVideoAcceleration_H264, CODECAPI_AVLowLatencyMode,
    IMF2DBuffer2, IMFDXGIBuffer, IMFSample, IMFTransform, MF_E_NOTACCEPTING, MF_MT_FRAME_SIZE,
    MF_MT_INTERLACE_MODE, MF_MT_MINIMUM_DISPLAY_APERTURE, MF_MT_SUBTYPE, MF_SA_D3D11_AWARE,
    MF2DBuffer_LockFlags_Read, MFMediaType_Video, MFT_MESSAGE_COMMAND_FLUSH,
    MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, MFT_MESSAGE_NOTIFY_START_OF_STREAM,
    MFT_MESSAGE_SET_D3D_MANAGER, MFT_OUTPUT_STREAM_CAN_PROVIDE_SAMPLES,
    MFT_OUTPUT_STREAM_PROVIDES_SAMPLES, MFVideoArea, MFVideoFormat_H264, MFVideoFormat_NV12,
    MFVideoInterlace_Progressive,
};
use windows::Win32::System::Com::{CLSCTX_INPROC_SERVER, CoCreateInstance};
use windows::core::Interface;

use super::mf::{self, Device, Output, Platform, failure};
use crate::backend::{Backend, Decoder, Failure};
use crate::shrink;

/// A frame's duration as the decoder is told it, in 100 ns units: any
/// will do (nothing is timed by it), so 30 a second.
const FRAME_TIME: i64 = 333_333;

/// The Media Foundation back end: the GPU's device, what it decodes, and
/// whether a hardware encoder opened.
pub struct MediaFoundation {
    device: Rc<Device>,
    /// The largest picture decoded; none if the GPU does not decode
    /// H.264 through Media Foundation.
    decode: Option<(u32, u32)>,
    /// The hardware encoder's name, if one opened.
    encoder: Option<String>,
}

impl std::fmt::Debug for MediaFoundation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MediaFoundation")
            .field("device", &self.device)
            .field("decode", &self.decode)
            .field("encoder", &self.encoder)
            .finish()
    }
}

impl MediaFoundation {
    /// Starts Media Foundation on this thread (the server's) and finds
    /// what the GPU does: a decoder that opens with DXVA, and an encoder
    /// that takes a picture's settings. Why not if it does neither.
    pub fn open() -> Result<Self, String> {
        let platform = Rc::new(Platform::start()?);
        let device = Rc::new(Device::open(platform)?);
        let decode = device.decode_max().filter(|_| {
            // The decoder opens with the device as it will for a stream.
            match MfDecoder::new(&device, 1280, 720) {
                Ok(_) => true,
                Err(failure) => {
                    eprintln!(
                        "noslacking-video: mediafoundation: no decoding on the GPU ({})",
                        failure.detail
                    );
                    false
                }
            }
        });
        let encoder = match super::encoder::probe() {
            Ok(name) => Some(name),
            Err(why) => {
                eprintln!("noslacking-video: mediafoundation: no encoder on the GPU ({why})");
                None
            }
        };
        if decode.is_none() && encoder.is_none() {
            return Err(format!(
                "{} neither decodes nor encodes H.264",
                device.adapter
            ));
        }
        Ok(Self {
            device,
            decode,
            encoder,
        })
    }
}

impl Backend for MediaFoundation {
    fn name(&self) -> String {
        match &self.encoder {
            Some(encoder) => format!(
                "mediafoundation: {} (encoder: {encoder})",
                self.device.adapter
            ),
            None => format!("mediafoundation: {}", self.device.adapter),
        }
    }

    fn capabilities(&self) -> Vec<Capability> {
        let mut capabilities = Vec::new();
        if let Some((max_width, max_height)) = self.decode {
            capabilities.push(Capability {
                codec: Codec::H264,
                direction: Direction::Decode,
                max_width,
                max_height,
            });
        }
        if self.encoder.is_some() {
            capabilities.push(Capability {
                codec: Codec::H264,
                direction: Direction::Encode,
                max_width: super::ENCODE_MAX.0,
                max_height: super::ENCODE_MAX.1,
            });
        }
        capabilities
    }

    fn capture_gpu(&self) -> Option<crate::pipeline::GpuOpener> {
        self.encoder.as_ref()?;
        Some(std::sync::Arc::new(|| {
            match super::encoder::MfGpu::open() {
                Ok(gpu) => Some(Box::new(gpu) as Box<dyn crate::pipeline::Gpu>),
                Err(why) => {
                    eprintln!("noslacking-video: capture: no GPU encoder ({why})");
                    None
                }
            }
        }))
    }

    fn open_decoder(
        &mut self,
        codec: Codec,
        width: u32,
        height: u32,
    ) -> Result<Box<dyn Decoder>, Failure> {
        let Codec::H264 = codec;
        let Some(max) = self.decode else {
            return Err(Failure::unsupported("the GPU does not decode H.264"));
        };
        if width > max.0 || height > max.1 {
            return Err(Failure::unsupported(format!(
                "{width}x{height} is too large"
            )));
        }
        Ok(Box::new(MfDecoder::new(&self.device, width, height)?))
    }
}

/// What the decoder's output is, as last set.
#[derive(Clone, Copy, Debug)]
struct Shape {
    /// The decoded picture's size (macroblocks, so 1088 for 1080p).
    frame: (u32, u32),
    /// What of it is shown: `(x, y, width, height)`.
    crop: (u32, u32, u32, u32),
    /// The bytes of a sample to give it, if it does not make its own.
    allocate: Option<u32>,
}

/// One stream's decoder.
struct MfDecoder {
    transform: IMFTransform,
    streams: (u32, u32),
    shape: Option<Shape>,
    /// The last frame's picture, until it is taken or the next frame.
    last: Option<(IMFSample, Shape)>,
    fit: (u32, u32),
    /// Whether an IDR went in since the start or the last failure:
    /// nothing before one decodes.
    started: bool,
    /// Whether a picture came out yet, its buffer checked to be on the
    /// GPU.
    checked: bool,
    time: i64,
    // Last: the transform goes before the device it was given.
    _device: Rc<Device>,
}

impl MfDecoder {
    /// Microsoft's H.264 decoder for a stream of about `width`×`height`,
    /// on `device` (DXVA), in low-latency mode, giving NV12.
    fn new(device: &Rc<Device>, width: u32, height: u32) -> Result<Self, Failure> {
        let unsupported = |what: &'static str| {
            move |e: windows::core::Error| Failure::unsupported(format!("{what}: {e}"))
        };
        // SAFETY: COM is started on this thread (the device's platform);
        // every value handed over outlives its call.
        let transform: IMFTransform =
            unsafe { CoCreateInstance(&CLSID_MSH264DecoderMFT, None, CLSCTX_INPROC_SERVER) }
                .map_err(unsupported("no H.264 decoder"))?;
        // SAFETY: as above, on the transform just made.
        unsafe {
            let attributes = transform
                .GetAttributes()
                .map_err(unsupported("the decoder's attributes"))?;
            if attributes.GetUINT32(&MF_SA_D3D11_AWARE).unwrap_or(0) == 0 {
                return Err(Failure::unsupported("the decoder does not use Direct3D 11"));
            }
            // Set as attributes, as Chromium does; neither is fatal.
            let _ = attributes.SetUINT32(&CODECAPI_AVDecVideoAcceleration_H264, 1);
            // Each picture out as its frame goes in: Slack's streams have
            // no B-frames to wait for.
            let _ = attributes.SetUINT32(&CODECAPI_AVLowLatencyMode, 1);
            transform
                .ProcessMessage(
                    MFT_MESSAGE_SET_D3D_MANAGER,
                    device.manager.as_raw() as usize,
                )
                .map_err(unsupported("the decoder took no Direct3D 11 device"))?;
        }
        let streams = mf::stream_ids(&transform);
        let input = mf::media_type(&MFMediaType_Video, &MFVideoFormat_H264)
            .map_err(unsupported("an H.264 media type"))?;
        // SAFETY: as above.
        unsafe {
            input
                .SetUINT64(&MF_MT_FRAME_SIZE, super::pack(width, height))
                .map_err(unsupported("the frame size"))?;
            input
                .SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)
                .map_err(unsupported("the interlace mode"))?;
            transform
                .SetInputType(streams.0, &input, 0)
                .map_err(unsupported("the decoder refused H.264"))?;
        }
        let mut decoder = Self {
            transform,
            streams,
            shape: None,
            last: None,
            fit: (0, 0),
            started: false,
            checked: false,
            time: 0,
            _device: Rc::clone(device),
        };
        decoder.set_output()?;
        // SAFETY: plain messages to a transform whose types are set.
        unsafe {
            decoder
                .transform
                .ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0)
                .map_err(|e| failure("starting the decoder", &e))?;
            decoder
                .transform
                .ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0)
                .map_err(|e| failure("starting the decoder", &e))?;
        }
        Ok(decoder)
    }

    /// Sets the output to NV12, as the decoder offers it now (at the
    /// start, and when the stream's size changes), and notes its shape.
    fn set_output(&mut self) -> Result<(), Failure> {
        let stream = self.streams.1;
        // SAFETY: plain calls on the live transform; the aperture is
        // read into a value of its own size.
        unsafe {
            let mut index = 0;
            let chosen = loop {
                let offered = self
                    .transform
                    .GetOutputAvailableType(stream, index)
                    .map_err(|e| {
                        Failure::unsupported(format!("the decoder offers no NV12: {e}"))
                    })?;
                if offered.GetGUID(&MF_MT_SUBTYPE) == Ok(MFVideoFormat_NV12) {
                    break offered;
                }
                index += 1;
            };
            self.transform
                .SetOutputType(stream, &chosen, 0)
                .map_err(|e| failure("setting NV12 out", &e))?;
            let frame = chosen
                .GetUINT64(&MF_MT_FRAME_SIZE)
                .map(super::unpack)
                .unwrap_or((0, 0));
            let mut area = MFVideoArea::default();
            let aperture = chosen
                .GetBlob(
                    &MF_MT_MINIMUM_DISPLAY_APERTURE,
                    std::slice::from_raw_parts_mut(
                        (&raw mut area).cast::<u8>(),
                        std::mem::size_of::<MFVideoArea>(),
                    ),
                    None,
                )
                .ok()
                .map(|()| {
                    (
                        i32::from(area.OffsetX.value),
                        i32::from(area.OffsetY.value),
                        area.Area.cx,
                        area.Area.cy,
                    )
                });
            let info = self
                .transform
                .GetOutputStreamInfo(stream)
                .map_err(|e| failure("the decoder's output", &e))?;
            let own = MFT_OUTPUT_STREAM_PROVIDES_SAMPLES.0 as u32
                | MFT_OUTPUT_STREAM_CAN_PROVIDE_SAMPLES.0 as u32;
            self.shape = Some(Shape {
                frame,
                crop: super::visible(frame, aperture),
                allocate: (info.dwFlags & own == 0).then_some(info.cbSize.max(1)),
            });
        }
        Ok(())
    }

    /// Takes every picture the decoder has ready, keeping the last.
    fn drain(&mut self) -> Result<(), Failure> {
        loop {
            let allocate = self.shape.and_then(|shape| shape.allocate);
            match mf::process_output(&self.transform, self.streams.1, allocate) {
                Ok(Output::Sample(sample)) => {
                    let shape = self.shape.ok_or_else(|| Failure::device("no output set"))?;
                    if !self.checked {
                        self.check_on_gpu(&sample)?;
                    }
                    self.last = Some((sample, shape));
                }
                Ok(Output::NeedMoreInput) => return Ok(()),
                Ok(Output::StreamChange) => self.set_output()?,
                Err(e) => return Err(failure("decoding", &e)),
            }
        }
    }

    /// Fails as unsupported if the first picture is not in GPU memory:
    /// the decoder fell back to its own software, where rusty_h264 is
    /// wanted instead.
    fn check_on_gpu(&mut self, sample: &IMFSample) -> Result<(), Failure> {
        // SAFETY: a plain call on a live sample.
        let buffer = unsafe { sample.GetBufferByIndex(0) }.map_err(|e| failure("a picture", &e))?;
        if buffer.cast::<IMFDXGIBuffer>().is_err() {
            return Err(Failure::unsupported(
                "Media Foundation decoded in software, not on the GPU",
            ));
        }
        self.checked = true;
        Ok(())
    }

    /// Forgets the stream after a failure: the decoder starts again at
    /// the next keyframe.
    fn reset(&mut self) {
        self.last = None;
        self.started = false;
        // SAFETY: a plain message to the live transform.
        let _ = unsafe { self.transform.ProcessMessage(MFT_MESSAGE_COMMAND_FLUSH, 0) };
    }

    /// Feeds one frame in, draining first if the decoder is full.
    fn feed(&mut self, frame: &[u8]) -> Result<(), Failure> {
        let sample = mf::memory_sample(frame.len(), self.time, FRAME_TIME, |buffer| {
            buffer.copy_from_slice(frame);
            true
        })?;
        self.time += FRAME_TIME;
        // SAFETY: the sample is a live one made above.
        let fed = unsafe { self.transform.ProcessInput(self.streams.0, &sample, 0) };
        match fed {
            Ok(()) => Ok(()),
            Err(e) if e.code() == MF_E_NOTACCEPTING => {
                self.drain()?;
                // SAFETY: as above.
                unsafe { self.transform.ProcessInput(self.streams.0, &sample, 0) }
                    .map_err(|e| failure("decoding", &e))
            }
            Err(e) => Err(failure("decoding", &e)),
        }
    }
}

impl Decoder for MfDecoder {
    fn set_output_size(&mut self, width: u32, height: u32) {
        self.fit = (width, height);
    }

    fn decode_frame(&mut self, frame: &[u8], _keyframe: bool) -> Result<bool, Failure> {
        self.last = None;
        let (slice, idr) = super::slices(frame);
        if slice && !idr && !self.started {
            return Err(Failure::need_keyframe("nothing decodes before a keyframe"));
        }
        let result = self.feed(frame).and_then(|()| self.drain());
        if let Err(failure) = result {
            if failure.kind != FailKind::Unsupported {
                self.reset();
            }
            return Err(failure);
        }
        if idr {
            self.started = true;
        }
        Ok(self.last.is_some())
    }

    fn picture(&mut self) -> Result<Option<Decoded>, Failure> {
        let Some((sample, shape)) = self.last.take() else {
            return Ok(None);
        };
        let planes = read_nv12(&sample, shape)?;
        Ok(Some(Decoded {
            planes: shrink::shrink(planes, self.fit),
            source: (shape.crop.2, shape.crop.3),
            hardware: true,
        }))
    }
}

/// `sample`'s picture, its shown part as I420: locked for reading, which
/// for one in GPU memory has Media Foundation copy it out.
fn read_nv12(sample: &IMFSample, shape: Shape) -> Result<noslacking_video_ipc::Planes, Failure> {
    // SAFETY: the buffer is read only while locked, no further than the
    // length the lock gives, and unlocked before it goes.
    unsafe {
        let buffer = sample
            .GetBufferByIndex(0)
            .map_err(|e| failure("a picture", &e))?;
        let two_d: IMF2DBuffer2 = buffer
            .cast()
            .map_err(|e| Failure::device(format!("a picture with no rows: {e}")))?;
        let mut scanline = std::ptr::null_mut();
        let mut pitch = 0;
        let mut start = std::ptr::null_mut();
        let mut len = 0;
        two_d
            .Lock2DSize(
                MF2DBuffer_LockFlags_Read,
                &mut scanline,
                &mut pitch,
                &mut start,
                &mut len,
            )
            .map_err(|e| failure("reading a picture back", &e))?;
        let planes = match usize::try_from(pitch) {
            // A picture stored bottom up (a negative pitch) is not what
            // a decoder gives; refused rather than read backwards.
            Ok(pitch) if !scanline.is_null() && scanline == start => {
                let data = std::slice::from_raw_parts(scanline, len as usize);
                super::nv12_to_i420(data, pitch, shape.frame.1, shape.crop)
            }
            _ => None,
        };
        let _ = two_d.Unlock2D();
        planes.ok_or_else(|| {
            Failure::device(format!(
                "a picture not as its type says ({}x{}, pitch {pitch}, {len} bytes)",
                shape.frame.0, shape.frame.1
            ))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Decodes both fixtures on this machine's GPU and compares the
    /// pictures with ffmpeg's (the hashes the software decoder's tests
    /// check too). Needs a GPU that decodes H.264: ignored by default.
    /// `cargo test -p noslacking-video -- --ignored mediafoundation --nocapture`
    #[test]
    #[ignore = "needs a GPU with H.264 decoding"]
    #[allow(clippy::print_stdout, reason = "the timings are for the reader")]
    fn mediafoundation_decodes_the_fixtures_as_ffmpeg_does() {
        use sha2::{Digest, Sha256};
        let mut backend = MediaFoundation::open().expect("Media Foundation on a GPU");
        println!("{}", backend.name());
        for (stream, size, expected) in [
            (
                &include_bytes!("../../../../src/huddle_audio/fixtures/screen-1920x1080.h264")[..],
                (1920, 1080),
                "a2bd2fa8a81ad980725de116dbc10da22367e82186641f63c41bc914e5eb8812",
            ),
            (
                &include_bytes!("../../../../src/huddle_audio/fixtures/camera-480x480.h264")[..],
                (480, 480),
                "e433e34c83ae538aa67adc4fae754ca4afc8892ea91546d6d55ffc97d699e3c2",
            ),
        ] {
            let mut decoder = backend
                .open_decoder(Codec::H264, size.0, size.1)
                .expect("a decoder");
            let mut hash = Sha256::new();
            let started = std::time::Instant::now();
            let frames = noslacking_video_ipc::h264::access_units(stream);
            let mut pictures = 0;
            for frame in &frames {
                let Some(decoded) = decoder.decode(frame, false).expect("decodes") else {
                    continue;
                };
                pictures += 1;
                assert!(decoded.hardware);
                assert_eq!(decoded.source, size);
                let picture = decoded.planes;
                assert_eq!((picture.width, picture.height), size);
                hash.update(&picture.y);
                hash.update(&picture.u);
                hash.update(&picture.v);
            }
            let took = started.elapsed().as_secs_f64() * 1000.0;
            println!(
                "{}x{}: {} frames, {pictures} pictures, {:.2} ms a frame",
                size.0,
                size.1,
                frames.len(),
                took / frames.len() as f64
            );
            assert_eq!(pictures, frames.len(), "a picture for every frame");
            let hex: String = hash.finalize().iter().map(|b| format!("{b:02x}")).collect();
            assert_eq!(hex, expected, "{}x{}", size.0, size.1);
        }
    }
}
