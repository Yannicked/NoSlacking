//! A capture (a shared screen, the camera) encoded on the GPU through
//! VA-API: frames come in as dma-bufs (imported as surfaces, never read
//! by the processor), as packed RGB in memory (written into an RGB
//! surface), or as I420 (the camera's, the test screen; written straight
//! into the encoder's input); video processing scales and converts the
//! first two into the encoder's NV12 input surface, and [`VaapiEncoder`]
//! encodes it.
//!
//! It opens a display of its own on the capture's thread (libva's state
//! here is kept to one thread), so a capture's GPU work never waits on
//! decoding in the same helper, or on another capture.

use std::rc::Rc;

use crate::backend::{Encoded, Encoder as _, Failure};
use crate::capture::{Frame, Order, Packed, convert};
use crate::pipeline::Gpu;

use super::encoder::{EncodeSupport, VaapiEncoder};
use super::va::prime::{DmaBufIn, Rgb};
use super::va::{Display, Scaler, Surfaces};

/// The RGB layout of `order`, with alpha or padding.
pub fn rgb(order: Order, alpha: bool) -> Rgb {
    match (order, alpha) {
        (Order::Bgra, false) => Rgb::BGRX,
        (Order::Bgra, true) => Rgb::BGRA,
        (Order::Rgba, false) => Rgb::RGBX,
        (Order::Rgba, true) => Rgb::RGBA,
    }
}

/// A capture's encoder on the GPU.
pub struct GpuCapture {
    // Fields drop in order: everything made on the display before it.
    encoder: Option<VaapiEncoder>,
    scaler: Option<Scaler>,
    /// The RGB surface packed pictures are written into, by size and
    /// layout.
    upload: Option<((u32, u32), Rgb, Surfaces)>,
    display: Rc<Display>,
    support: EncodeSupport,
    /// Whether to offer dma-bufs: the driver does video processing, and
    /// `NOSLACKING_VIDEO_DMABUF` is not `0`.
    dmabuf: bool,
    /// Writing RGB surfaces failed once: packed pictures are converted on
    /// the processor from then on.
    no_upload: bool,
}

impl std::fmt::Debug for GpuCapture {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GpuCapture")
            .field("display", &self.display)
            .field("dmabuf", &self.dmabuf)
            .finish_non_exhaustive()
    }
}

impl GpuCapture {
    /// The first render node's encoder, if its driver encodes H.264;
    /// with video processing if it has it.
    pub fn open() -> Result<Self, String> {
        let display = Display::open()?;
        let support = EncodeSupport::query(&display).ok_or_else(|| {
            format!(
                "{} ({}) does not encode H.264",
                display.vendor(),
                display.path
            )
        })?;
        let scaler = match Scaler::new(&display) {
            Ok(scaler) => Some(scaler),
            Err(why) => {
                eprintln!(
                    "noslacking-video: capture: no video processing ({why}): the processor \
                     converts"
                );
                None
            }
        };
        let off = |name: &str| std::env::var_os(name).is_some_and(|v| v == "0");
        let dmabuf = scaler.is_some() && !off("NOSLACKING_VIDEO_DMABUF");
        Ok(Self {
            encoder: None,
            scaler,
            upload: None,
            display,
            support,
            dmabuf,
            // NOSLACKING_VIDEO_RGB_UPLOAD=0 converts packed pictures on
            // the processor, to compare the two (examples/share.rs).
            no_upload: off("NOSLACKING_VIDEO_RGB_UPLOAD"),
        })
    }

    /// Whether packed pictures are written as RGB and converted on the
    /// GPU (the default), or converted on the processor and written as
    /// NV12.
    pub fn convert_on_gpu(&mut self, on: bool) {
        self.no_upload = !on;
    }

    /// Writes a packed picture into an RGB surface and converts it into
    /// the encoder's input on the GPU.
    fn upload(&mut self, packed: &Packed<'_>) -> Result<(), String> {
        let format = rgb(packed.order, false);
        let size = (packed.width, packed.height);
        let (Some(encoder), Some(scaler)) = (self.encoder.as_mut(), self.scaler.as_mut()) else {
            return Err("no video processing".into());
        };
        if self
            .upload
            .as_ref()
            .is_none_or(|(s, f, _)| *s != size || *f != format)
        {
            self.upload = None;
            let surfaces = self.display.rgb_surface(size.0, size.1, format, false)?;
            self.upload = Some((size, format, surfaces));
        }
        let Some((_, _, surfaces)) = &self.upload else {
            return Err("no RGB surface".into());
        };
        let surface = surfaces.ids[0];
        self.display
            .write_packed(surface, size, format, packed.data, packed.stride)?;
        scaler.blit(surface, size, true, encoder.input(), encoder.size())
    }
}

impl Gpu for GpuCapture {
    fn name(&self) -> String {
        format!(
            "the GPU (vaapi: {}, {}{})",
            self.display.vendor(),
            self.display.path,
            if self.dmabuf {
                ", dma-bufs straight in"
            } else {
                ""
            }
        )
    }

    fn takes_dmabuf(&self) -> bool {
        self.dmabuf
    }

    fn max_size(&self) -> (u32, u32) {
        self.support.max_size
    }

    fn open(&mut self, size: (u32, u32), fps: u32, bitrate: u32) -> Result<(), Failure> {
        // The old one, and its surfaces, go first.
        self.encoder = None;
        self.encoder = Some(VaapiEncoder::new(
            &self.display,
            self.support,
            size,
            fps,
            bitrate,
        )?);
        Ok(())
    }

    fn load(&mut self, frame: &Frame<'_>) -> Result<(), Failure> {
        let Some(encoder) = self.encoder.as_mut() else {
            return Err(Failure::device("not opened"));
        };
        let size = encoder.size();
        match frame {
            Frame::DmaBuf(buffer) => {
                let scaler = self
                    .scaler
                    .as_mut()
                    .ok_or_else(|| Failure::unsupported("no video processing"))?;
                let imported = self
                    .display
                    .import(&DmaBufIn {
                        fd: buffer.fd,
                        width: buffer.width,
                        height: buffer.height,
                        offset: buffer.offset,
                        pitch: buffer.stride,
                        format: rgb(buffer.order, buffer.alpha),
                        modifier: buffer.modifier,
                    })
                    .map_err(Failure::unsupported)?;
                // Scaled and converted while PipeWire still lends the
                // buffer; the surface over it goes when this returns.
                scaler
                    .blit(
                        imported.ids[0],
                        (buffer.width, buffer.height),
                        true,
                        encoder.input(),
                        size,
                    )
                    .map_err(Failure::unsupported)
            }
            Frame::Packed(packed) => {
                if !self.no_upload {
                    match self.upload(packed) {
                        Ok(()) => return Ok(()),
                        Err(why) => {
                            eprintln!(
                                "noslacking-video: capture: RGB onto the GPU failed ({why}): the \
                                 processor converts from now on"
                            );
                            self.no_upload = true;
                        }
                    }
                }
                let picture = convert::to_i420(packed)
                    .ok_or_else(|| Failure::broken("an unusable picture"))?;
                let Some(encoder) = self.encoder.as_mut() else {
                    return Err(Failure::device("not opened"));
                };
                encoder.write(&convert::scale(picture, size.0, size.1))
            }
            Frame::I420(planes) => {
                if (planes.width, planes.height) == size {
                    encoder.write(planes)
                } else {
                    encoder.write(&convert::scale((*planes).clone(), size.0, size.1))
                }
            }
        }
    }

    fn encode(&mut self, force_keyframe: bool) -> Result<Encoded, Failure> {
        self.encoder
            .as_mut()
            .ok_or_else(|| Failure::device("not opened"))?
            .encode_input(force_keyframe)
    }

    fn set_bitrate(&mut self, bitrate: u32) -> Result<(), Failure> {
        match self.encoder.as_mut() {
            Some(encoder) => encoder.set_bitrate(bitrate),
            None => Ok(()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capture::pattern::{pattern, to_bgrx};
    use std::time::{Duration, Instant};

    /// Mean absolute difference of two planes.
    fn difference(a: &[u8], b: &[u8]) -> f64 {
        a.iter()
            .zip(b)
            .map(|(x, y)| f64::from(x.abs_diff(*y)))
            .sum::<f64>()
            / a.len() as f64
    }

    /// The share's three ways onto this machine's GPU: the test screen
    /// as RGB written into a surface, exported as a dma-buf and imported
    /// again (as PipeWire would lend it), and as I420; each encoded and
    /// decoded back by rusty_h264, close to the picture the processor
    /// makes of the same RGB. Needs a VA-API driver that encodes H.264.
    /// `cargo test -p noslacking-video -- --ignored vaapi --nocapture`
    #[test]
    #[ignore = "needs a GPU with VA-API"]
    #[allow(clippy::print_stdout, reason = "the timings are for the reader")]
    fn vaapi_shares_dmabufs_packed_and_i420_pictures() {
        use std::os::fd::AsFd;
        let mut share = GpuCapture::open().expect("VA-API with an H.264 encoder");
        println!("{}", share.name());
        let size = (1920, 1080);
        share.open(size, 15, 2_500_000).expect("opened");
        let display = Display::open().expect("a second display, as the compositor's");
        let mut decoder = rusty_h264_decoder::Decoder::new();
        for n in 0..6u64 {
            let picture = pattern(size.0, size.1, n, Duration::from_millis(n * 67));
            let bgrx = to_bgrx(&picture);
            // What the processor makes of the same RGB: the reference.
            let reference = convert::to_i420(&Packed {
                width: size.0,
                height: size.1,
                stride: size.0 as usize * 4,
                order: Order::Bgra,
                data: &bgrx,
            })
            .expect("converted");
            let started = Instant::now();
            let way = match n % 3 {
                0 => {
                    let surfaces = display
                        .rgb_surface(size.0, size.1, Rgb::BGRX, true)
                        .expect("an RGB surface");
                    display
                        .write_packed(surfaces.ids[0], size, Rgb::BGRX, &bgrx, size.0 as usize * 4)
                        .expect("written");
                    let exported = display.export(surfaces.ids[0]).expect("exported");
                    share
                        .load(&Frame::DmaBuf(crate::capture::DmaBuf {
                            fd: exported.fd.as_fd(),
                            width: exported.width,
                            height: exported.height,
                            offset: exported.offset,
                            stride: exported.pitch,
                            order: Order::Bgra,
                            alpha: false,
                            modifier: exported.modifier,
                        }))
                        .expect("a dma-buf in");
                    format!("dma-buf (modifier {:#x})", exported.modifier)
                }
                1 => {
                    share
                        .load(&Frame::Packed(Packed {
                            width: size.0,
                            height: size.1,
                            stride: size.0 as usize * 4,
                            order: Order::Bgra,
                            data: &bgrx,
                        }))
                        .expect("packed in");
                    "packed".to_owned()
                }
                _ => {
                    share.load(&Frame::I420(&reference)).expect("I420 in");
                    "I420".to_owned()
                }
            };
            let loaded = started.elapsed();
            let encoded = share.encode(n == 0).expect("encoded");
            let took = started.elapsed();
            assert_eq!(encoded.keyframe, n == 0);
            let back = decoder
                .decode(&encoded.data)
                .expect("decodes")
                .expect("a picture");
            assert_eq!((back.width, back.height), (1920, 1080));
            let luma = difference(&back.y, &reference.y);
            let chroma = difference(&back.u, &reference.u);
            println!(
                "{way}: in {:.2} ms, encoded by {:.2} ms; mean difference from the processor's \
                 luma {luma:.2}, chroma {chroma:.2}",
                loaded.as_secs_f64() * 1000.0,
                took.as_secs_f64() * 1000.0
            );
            assert!(luma < 4.0 && chroma < 4.0, "{way}: {luma} {chroma}");
        }
    }
}
