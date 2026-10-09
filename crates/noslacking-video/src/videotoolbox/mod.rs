//! The VideoToolbox back end (macOS): H.264 decoding and encoding on
//! the GPU's media engine, Apple silicon's and the Intel Macs' alike.
//! VideoToolbox parses the stream itself, so this only reshapes it: the
//! pipe's Annex B to its AVCC and back ([`avcc`]), its NV12 pixel
//! buffers to the pipe's I420 and back ([`planes`]). Decoding is
//! `decoder`'s, encoding a capture `encoder`'s; the calls into the
//! system are all `vt`'s. Hardware is required: where VideoToolbox would
//! fall back to its own software codec, the helper's software is used
//! instead, as everywhere else.
//!
//! Built and checked, not yet run on a Mac.
//!
//! [`avcc`] and [`planes`] are plain code, built and tested on every
//! system; the rest is macOS's.

pub mod avcc;
#[cfg(target_os = "macos")]
mod decoder;
#[cfg(target_os = "macos")]
mod encoder;
pub mod planes;
#[cfg(target_os = "macos")]
#[allow(unsafe_code)]
mod vt;

#[cfg(target_os = "macos")]
pub use backend::VideoToolbox;

#[cfg(target_os = "macos")]
mod backend {
    use noslacking_video_ipc::{Capability, Codec, Direction, MAX_SIDE};

    use super::decoder::VtDecoder;
    use super::encoder::{self, VtCapture};
    use super::vt::{Compression, Decompression};
    use crate::backend::{Backend, Decoder, Failure};
    use crate::nal;

    /// The VideoToolbox back end: what the GPU was found to decode and
    /// encode when the helper started.
    #[derive(Debug)]
    pub struct VideoToolbox {
        decodes: bool,
        encodes: bool,
    }

    impl VideoToolbox {
        /// Tries a hardware decoder and encoder (a small session of
        /// each, closed again); why not, if there is neither.
        pub fn open() -> Result<Self, String> {
            // A constrained baseline stream's parameter sets, as Slack
            // sends, for a decoder to be made for.
            let sps = nal::sps(&nal::Sps {
                level_idc: 31,
                width_in_mbs: 40,
                height_in_mbs: 30,
                log2_max_frame_num_minus4: 4,
                crop_right: 0,
                crop_bottom: 0,
                fps: 30,
            });
            let pps = nal::pps();
            // Without their start codes.
            let decoder = Decompression::new(&sps[4..], &pps[4..], true);
            let encoder = Compression::new(640, 480, true);
            let decodes = decoder.is_ok();
            let encodes = encoder.is_ok();
            if !decodes && !encodes {
                return Err(format!(
                    "no H.264 in hardware (decoding: {}, encoding: {})",
                    decoder.err().map(|s| s.to_string()).unwrap_or_default(),
                    encoder.err().map(|s| s.to_string()).unwrap_or_default(),
                ));
            }
            Ok(Self { decodes, encodes })
        }
    }

    impl Backend for VideoToolbox {
        fn name(&self) -> String {
            let what = match (self.decodes, self.encodes) {
                (true, true) => "decoding and encoding",
                (true, false) => "decoding",
                _ => "encoding",
            };
            format!("videotoolbox: H.264 {what} in hardware")
        }

        fn capabilities(&self) -> Vec<Capability> {
            let mut capabilities = Vec::new();
            if self.decodes {
                capabilities.push(Capability {
                    codec: Codec::H264,
                    direction: Direction::Decode,
                    max_width: MAX_SIDE,
                    max_height: MAX_SIDE,
                });
            }
            if self.encodes {
                capabilities.push(Capability {
                    codec: Codec::H264,
                    direction: Direction::Encode,
                    max_width: encoder::MAX_SIZE.0,
                    max_height: encoder::MAX_SIZE.1,
                });
            }
            capabilities
        }

        fn open_decoder(
            &mut self,
            codec: Codec,
            width: u32,
            height: u32,
        ) -> Result<Box<dyn Decoder>, Failure> {
            let Codec::H264 = codec;
            if !self.decodes {
                return Err(Failure::unsupported("no H.264 decoder in hardware"));
            }
            if width > MAX_SIDE || height > MAX_SIDE {
                return Err(Failure::unsupported(format!(
                    "{width}x{height} is too large"
                )));
            }
            Ok(Box::new(VtDecoder::new()))
        }

        fn capture_gpu(&self) -> Option<crate::pipeline::GpuOpener> {
            if !self.encodes {
                return None;
            }
            Some(std::sync::Arc::new(|| {
                Some(Box::new(VtCapture::new()) as Box<dyn crate::pipeline::Gpu>)
            }))
        }
    }
}

#[cfg(all(test, target_os = "macos"))]
mod tests {
    use std::time::Duration;

    use noslacking_video_ipc::h264::{access_units, nal_types, parse_sps, sps_of_frame};
    use noslacking_video_ipc::{Codec, Planes};

    use super::VideoToolbox;
    use super::encoder::VtCapture;
    use crate::backend::Backend;
    use crate::capture::pattern::{pattern, to_bgrx};
    use crate::capture::{Frame, Order, Packed, convert};
    use crate::pipeline::Gpu;

    /// Decodes both fixtures on this Mac's GPU and compares the pictures
    /// with ffmpeg's (the hashes the VA-API and software decoders' tests
    /// check too): H.264 decoding is exact, so hardware gives the same
    /// pictures. Ignored by default, as the VA-API tests are.
    /// `cargo test -p noslacking-video -- --ignored videotoolbox --nocapture`
    #[test]
    #[ignore = "needs a Mac with H.264 in hardware"]
    #[allow(clippy::print_stdout, reason = "the timings are for the reader")]
    fn videotoolbox_decodes_the_fixtures_as_ffmpeg_does() {
        use sha2::{Digest, Sha256};
        let mut backend = VideoToolbox::open().expect("VideoToolbox with H.264");
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
            let frames = access_units(stream);
            for frame in &frames {
                let decoded = decoder
                    .decode(frame, false)
                    .expect("decodes")
                    .expect("a picture");
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
                "{}x{}: {} frames, {:.2} ms a frame",
                size.0,
                size.1,
                frames.len(),
                took / frames.len() as f64
            );
            let hex: String = hash.finalize().iter().map(|b| format!("{b:02x}")).collect();
            assert_eq!(hex, expected, "{}x{}", size.0, size.1);
        }
    }

    /// Mean absolute difference of two planes.
    fn difference(a: &[u8], b: &[u8]) -> f64 {
        a.iter()
            .zip(b)
            .map(|(x, y)| f64::from(x.abs_diff(*y)))
            .sum::<f64>()
            / a.len() as f64
    }

    /// The test screen onto this Mac's GPU as packed RGB (both byte
    /// orders, converted by a pixel transfer) and as I420 (written in);
    /// each encoded, and decoded back by rusty_h264, close to the picture
    /// the processor makes of the same RGB. The stream is constrained
    /// baseline with its parameter sets before each IDR, an IDR when one
    /// is asked for, and a new rate is taken without one.
    /// `cargo test -p noslacking-video -- --ignored videotoolbox --nocapture`
    #[test]
    #[ignore = "needs a Mac with H.264 in hardware"]
    #[allow(clippy::print_stdout, reason = "the timings are for the reader")]
    fn videotoolbox_shares_packed_and_i420_pictures() {
        let backend = VideoToolbox::open().expect("VideoToolbox with H.264");
        assert!(backend.capture_gpu().is_some(), "{}", backend.name());
        let mut share = VtCapture::new();
        let size = (1920, 1080);
        share.open(size, 15, 2_500_000).expect("opened");
        println!("{}", share.name());
        let mut decoder = rusty_h264_decoder::Decoder::new();
        for n in 0..9u64 {
            let picture = pattern(size.0, size.1, n, Duration::from_millis(n * 67));
            let bgrx = to_bgrx(&picture);
            let packed = |order| Packed {
                width: size.0,
                height: size.1,
                stride: size.0 as usize * 4,
                order,
                data: &bgrx,
            };
            // What the processor makes of the same RGB: the reference.
            let reference = convert::to_i420(&packed(Order::Bgra)).expect("converted");
            let started = std::time::Instant::now();
            let way = match n % 3 {
                0 => {
                    share
                        .load(&Frame::Packed(packed(Order::Bgra)))
                        .expect("BGRA in");
                    "BGRA"
                }
                1 => {
                    // Red and blue swapped, and swapped back by the
                    // byte order: the same picture.
                    let rgba: Vec<u8> = bgrx
                        .as_chunks::<4>()
                        .0
                        .iter()
                        .flat_map(|p| [p[2], p[1], p[0], p[3]])
                        .collect();
                    share
                        .load(&Frame::Packed(Packed {
                            data: &rgba,
                            ..packed(Order::Rgba)
                        }))
                        .expect("RGBA in");
                    "RGBA"
                }
                _ => {
                    share.load(&Frame::I420(&reference)).expect("I420 in");
                    "I420"
                }
            };
            let loaded = started.elapsed();
            if n == 5 {
                share.set_bitrate(1_000_000).expect("a new rate");
            }
            let encoded = share.encode(n == 3).expect("encoded");
            let took = started.elapsed();
            assert_eq!(encoded.keyframe, n == 0 || n == 3, "picture {n}");
            if encoded.keyframe {
                assert!(nal_types(&encoded.data).starts_with(&[7, 8]), "picture {n}");
                let sps = sps_of_frame(&encoded.data).expect("an SPS");
                assert_eq!(sps.profile(), "constrained baseline", "{sps}");
                println!("{sps}");
            }
            let back = decoder
                .decode(&encoded.data)
                .expect("decodes")
                .expect("a picture");
            assert_eq!((back.width, back.height), (1920, 1080));
            let luma = difference(&back.y, &reference.y);
            let chroma = difference(&back.u, &reference.u);
            println!(
                "{way}: in {:.2} ms, encoded by {:.2} ms, {} bytes; mean difference from the \
                 processor's luma {luma:.2}, chroma {chroma:.2}",
                loaded.as_secs_f64() * 1000.0,
                took.as_secs_f64() * 1000.0,
                encoded.data.len()
            );
            assert!(luma < 4.0 && chroma < 4.0, "{way}: {luma} {chroma}");
        }
        // A camera's picture, scaled on the way in.
        let mut camera = VtCapture::new();
        camera.open((640, 480), 30, 600_000).expect("opened");
        let picture: Planes = pattern(1280, 960, 0, Duration::ZERO);
        camera.load(&Frame::I420(&picture)).expect("I420 in");
        let encoded = camera.encode(false).expect("encoded");
        assert!(encoded.keyframe, "the first picture is an IDR");
        let sps = nal_types(&encoded.data);
        assert!(sps.starts_with(&[7, 8]));
        let first = noslacking_video_ipc::h264::nal_units(&encoded.data)[0];
        let sps = parse_sps(first).expect("an SPS");
        assert_eq!((sps.width, sps.height), (640, 480));
    }
}
