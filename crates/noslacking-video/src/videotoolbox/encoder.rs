//! A capture (a shared screen, the camera) encoded by VideoToolbox, set
//! up as WebRTC's single-layer H.264 and the other encoders are:
//! constrained baseline (plain baseline where the system has no
//! constrained profile, before macOS 12), no B-frames, real time, every
//! picture out before the next goes in. IDRs come when asked and at least
//! every [`IDR_EVERY_SECONDS`], each with the SPS and PPS in front of it
//! (VideoToolbox keeps them apart, in the stream's format description).
//! A new rate takes effect at the next picture, without a keyframe.
//!
//! Pictures go into a buffer from the session's own pool (NV12, in
//! IOSurface memory the GPU reads): a shared screen's packed RGB is
//! copied into a buffer of its own and converted and scaled into that
//! one on the GPU by a pixel transfer session; the camera's I420 (and
//! RGB, if the transfer fails) is converted on the processor and
//! written in.

use noslacking_video_ipc::h264::is_keyframe;

use super::avcc;
use super::planes;
use super::vt::{Compression, PixelBuffer, Property, Status, Transfer};
use crate::backend::{Encoded, Failure};
use crate::capture::{Frame, Order, Packed, convert};
use crate::pipeline::Gpu;

/// A keyframe at least this often, in seconds, as the other encoders.
pub const IDR_EVERY_SECONDS: u32 = 4;
/// The largest picture: what VideoToolbox's H.264 encoders take on
/// Apple's GPUs (level 5.2).
pub const MAX_SIZE: (u32, u32) = (4096, 2304);

/// `status` from VideoToolbox as a failure: an encoder that is not there
/// (or not in hardware) is unsupported, anything else the device's.
fn failure(status: Status) -> Failure {
    match status {
        Status::NO_ENCODER | Status::UNSUPPORTED_FORMAT => {
            Failure::unsupported(format!("videotoolbox: {status}"))
        }
        _ => Failure::device(format!("videotoolbox: {status}")),
    }
}

/// The at-most-this-many bytes a second for `bitrate`: half as much
/// again as the average, so a keyframe fits but a burst does not run on.
fn bytes_per_second(bitrate: u32) -> i32 {
    i32::try_from(u64::from(bitrate) * 3 / 16).unwrap_or(i32::MAX)
}

/// An open session and what it was opened for.
struct Open {
    session: Compression,
    size: (u32, u32),
    fps: u32,
    /// Pictures encoded, for each one's time.
    frames: i64,
}

/// A capture's encoder on the GPU, through VideoToolbox.
pub struct VtCapture {
    open: Option<Open>,
    /// The picture last put in, to encode (again, for a still screen).
    loaded: Option<PixelBuffer>,
    /// Converts and scales packed RGB into the encoder's NV12, made on
    /// first need.
    transfer: Option<Transfer>,
    /// The packed buffer RGB pictures are copied into, by size and byte
    /// order.
    packed: Option<((u32, u32), Order, PixelBuffer)>,
    /// Converting on the GPU failed once: packed pictures are converted
    /// on the processor from then on.
    no_transfer: bool,
}

impl std::fmt::Debug for VtCapture {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VtCapture")
            .field("size", &self.open.as_ref().map(|o| o.size))
            .field("no_transfer", &self.no_transfer)
            .finish_non_exhaustive()
    }
}

impl VtCapture {
    /// An encoder, opened at its first [`Gpu::open`].
    pub fn new() -> Self {
        Self {
            open: None,
            loaded: None,
            transfer: None,
            packed: None,
            // NOSLACKING_VIDEO_RGB_UPLOAD=0 converts packed pictures on
            // the processor, to compare the two (as on VA-API).
            no_transfer: std::env::var_os("NOSLACKING_VIDEO_RGB_UPLOAD").is_some_and(|v| v == "0"),
        }
    }

    /// Copies a packed picture into a buffer of its own and converts and
    /// scales it into `to` on the GPU.
    fn transfer(&mut self, packed: &Packed<'_>, to: &PixelBuffer) -> Result<(), String> {
        let size = (packed.width, packed.height);
        if self
            .packed
            .as_ref()
            .is_none_or(|(s, order, _)| *s != size || *order != packed.order)
        {
            self.packed = None;
            let n = |v: u32| usize::try_from(v).unwrap_or(0);
            let buffer = PixelBuffer::packed(n(size.0), n(size.1), packed.order == Order::Bgra)
                .map_err(|status| format!("no RGB buffer: {status}"))?;
            self.packed = Some((size, packed.order, buffer));
        }
        let Some((_, _, from)) = self.packed.as_mut() else {
            return Err("no RGB buffer".into());
        };
        let row = usize::try_from(packed.width).unwrap_or(0) * 4;
        let rows = usize::try_from(packed.height).unwrap_or(0);
        let copied = from
            .write_packed(|bytes, stride| {
                planes::copy_rows(packed.data, packed.stride, bytes, stride, row, rows)
            })
            .map_err(|status| status.to_string())?;
        if !copied {
            return Err("the picture does not fit its buffer".into());
        }
        if self.transfer.is_none() {
            self.transfer = Some(Transfer::new().map_err(|status| status.to_string())?);
        }
        let transfer = self.transfer.as_ref().ok_or("no transfer session")?;
        to.mark_bt601();
        transfer
            .transfer(from, to)
            .map_err(|status| status.to_string())
    }
}

impl Default for VtCapture {
    fn default() -> Self {
        Self::new()
    }
}

/// Writes `picture`, scaled to the buffer's size, into it.
fn write_i420(to: &mut PixelBuffer, picture: noslacking_video_ipc::Planes) -> Result<(), Failure> {
    let (width, height) = to.size();
    let side = |v: usize| u32::try_from(v).unwrap_or(0);
    let (width, height) = (side(width), side(height));
    let picture = if (picture.width, picture.height) == (width, height) {
        picture
    } else {
        convert::scale(picture, width, height)
    };
    let wrote = to
        .write_nv12(|out| planes::write_i420(&picture, out))
        .map_err(failure)?;
    if wrote {
        Ok(())
    } else {
        Err(Failure::broken(format!(
            "a {}x{} picture does not fit {to:?}",
            picture.width, picture.height
        )))
    }
}

impl Gpu for VtCapture {
    fn name(&self) -> String {
        let hardware = self.open.as_ref().is_some_and(|o| o.session.hardware());
        format!(
            "the GPU (videotoolbox{})",
            if hardware { ", hardware" } else { "" }
        )
    }

    fn takes_dmabuf(&self) -> bool {
        false
    }

    fn max_size(&self) -> (u32, u32) {
        MAX_SIZE
    }

    fn open(&mut self, size: (u32, u32), fps: u32, bitrate: u32) -> Result<(), Failure> {
        let (width, height) = size;
        if width < 16 || height < 16 || width % 2 != 0 || height % 2 != 0 {
            return Err(Failure::unsupported(format!("{width}x{height}")));
        }
        if width > MAX_SIZE.0 || height > MAX_SIZE.1 {
            return Err(Failure::unsupported(format!(
                "{width}x{height} is too large"
            )));
        }
        // The old session, and the picture in its pool, go first.
        self.loaded = None;
        self.open = None;
        let fps = fps.clamp(1, 60);
        let session = Compression::new(width, height, true).map_err(failure)?;
        // What the stream must be: no B-frames, (constrained) baseline.
        session
            .set(Property::AllowFrameReordering(false))
            .map_err(failure)?;
        if let Err(status) = session.set(Property::ConstrainedBaseline) {
            eprintln!(
                "noslacking-video: videotoolbox: no constrained baseline ({status}): baseline"
            );
            session.set(Property::Baseline).map_err(failure)?;
        }
        // What helps it be sent: hints an encoder may not have.
        let rate = i32::try_from(bitrate).unwrap_or(i32::MAX);
        let fps_i32 = i32::try_from(fps).unwrap_or(30);
        for property in [
            Property::RealTime(true),
            Property::AverageBitRate(rate),
            Property::BytesPerSecond(bytes_per_second(bitrate)),
            Property::ExpectedFrameRate(fps_i32),
            Property::MaxKeyFrameInterval(fps_i32 * IDR_EVERY_SECONDS as i32),
            Property::MaxKeyFrameIntervalDuration(f64::from(IDR_EVERY_SECONDS)),
            Property::MaxFrameDelayCount(0),
        ] {
            if let Err(status) = session.set(property) {
                eprintln!("noslacking-video: videotoolbox: {property:?} not taken ({status})");
            }
        }
        self.open = Some(Open {
            session,
            size,
            fps,
            frames: 0,
        });
        Ok(())
    }

    fn load(&mut self, frame: &Frame<'_>) -> Result<(), Failure> {
        let Some(open) = self.open.as_ref() else {
            return Err(Failure::device("not opened"));
        };
        let mut buffer = open.session.buffer().map_err(failure)?;
        match frame {
            Frame::Packed(packed) => {
                if !self.no_transfer {
                    match self.transfer(packed, &buffer) {
                        Ok(()) => {
                            self.loaded = Some(buffer);
                            return Ok(());
                        }
                        Err(why) => {
                            eprintln!(
                                "noslacking-video: capture: RGB on the GPU failed ({why}): the \
                                 processor converts from now on"
                            );
                            self.no_transfer = true;
                        }
                    }
                }
                let picture = convert::to_i420(packed)
                    .ok_or_else(|| Failure::broken("an unusable picture"))?;
                write_i420(&mut buffer, picture)?;
            }
            Frame::I420(planes) => write_i420(&mut buffer, (*planes).clone())?,
        }
        self.loaded = Some(buffer);
        Ok(())
    }

    fn encode(&mut self, force_keyframe: bool) -> Result<Encoded, Failure> {
        let (Some(open), Some(buffer)) = (self.open.as_mut(), self.loaded.as_ref()) else {
            return Err(Failure::device("nothing to encode"));
        };
        let at = (open.frames, i32::try_from(open.fps).unwrap_or(30));
        open.frames += 1;
        let sample = open
            .session
            .encode(buffer, at, force_keyframe)
            .map_err(failure)?
            .ok_or_else(|| Failure::broken("videotoolbox dropped the picture"))?;
        let data = avcc::to_annex_b(&sample.avcc, sample.length_size, &sample.parameter_sets)
            .ok_or_else(|| Failure::broken("videotoolbox gave a broken frame"))?;
        if data.is_empty() {
            return Err(Failure::broken("videotoolbox gave an empty frame"));
        }
        Ok(Encoded {
            keyframe: is_keyframe(&data),
            data,
        })
    }

    fn set_bitrate(&mut self, bitrate: u32) -> Result<(), Failure> {
        let Some(open) = self.open.as_ref() else {
            return Ok(());
        };
        open.session
            .set(Property::AverageBitRate(
                i32::try_from(bitrate).unwrap_or(i32::MAX),
            ))
            .map_err(failure)?;
        // The limit is a guard only: an encoder without it still keeps
        // the average.
        let _ = open
            .session
            .set(Property::BytesPerSecond(bytes_per_second(bitrate)));
        Ok(())
    }
}
