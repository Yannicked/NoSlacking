//! Our pictures as H.264: the camera's (the `huddle-camera` feature)
//! and a screen share's (`huddle-share`), behind one small interface,
//! [`Encode`], so the threads that feed an encoder and the session that
//! sends what it makes do not care which encoder it is. Today there is
//! one, [`VideoEncoder`], in software; a GPU encoder in the
//! `noslacking-video` helper can be another [`Backend`] later.
//!
//! The software encoder is `rusty_h264`'s, pure Rust; why that one is in
//! docs/research/huddle-video.md (Stage 3). It is set up as WebRTC's
//! single-layer H.264 is: constrained baseline (`42e01f`, CAVLC, one
//! reference frame, no B-frames), one access unit out for each picture
//! in, nothing held back, its average-bitrate control on. Every IDR
//! carries the SPS and PPS in front of it, so a receiver that joins late
//! or lost packets can start from it; one comes every few seconds and
//! whenever asked (the `keyframe` flag of [`Encode::encode`], for a
//! receiver's PLI or FIR).
//!
//! The encoder takes only even sizes and returns errors rather than
//! panicking; [`VideoEncoder::new`] also refuses sizes larger than its
//! [`Limits`] allow: a camera stays within level 3.1 (1280×720), a share
//! within level 4.0 (1920×1080).

use rusty_h264_common::YuvPlanes;
use rusty_h264_encoder::{Encoder, EncoderConfig, Preset};

use super::camera::I420;

/// A keyframe at least this often, in seconds: what a receiver that
/// missed the last one waits at most, if its PLI is lost too.
pub const IDR_EVERY_SECONDS: u32 = 4;
/// The least bitrate set, in bit/s: below it the picture is mush anyway.
pub const MIN_BITRATE: u32 = 150_000;
/// The most for a camera, in bit/s: Slack's own cameras send up to about
/// 500 kbit/s at 480×480; 640×480 at 30 frames a second looks good from
/// 900 to 1,800.
pub const MAX_BITRATE: u32 = 1_800_000;
/// What sending starts at, before bandwidth estimation says more.
pub const START_BITRATE: u32 = 600_000;

/// What an encoder may be asked for: the largest picture, the level its
/// SPS names, and the bitrates it is kept between.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Limits {
    /// The most pixels a picture may have.
    pub max_pixels: usize,
    /// The longest side, in pixels.
    pub max_side: usize,
    /// The level written in the SPS (31 is level 3.1).
    pub level_idc: u8,
    /// The least bitrate, in bit/s.
    pub min_bitrate: u32,
    /// The most, in bit/s.
    pub max_bitrate: u32,
}

impl Limits {
    /// A camera: up to 1280×720 (level 3.1), up to 1.8 Mbit/s.
    pub const CAMERA: Self = Self {
        max_pixels: 1280 * 720,
        max_side: 2048,
        level_idc: 31,
        min_bitrate: MIN_BITRATE,
        max_bitrate: MAX_BITRATE,
    };
    /// A screen share: up to 1920×1080 (level 4.0; Slack's own shares
    /// arrive at that size), up to 2.5 Mbit/s, the JS SDK's ceiling for
    /// content (`setVideoMaxBandwidthKbps(2500)`).
    pub const SHARE: Self = Self {
        max_pixels: 1920 * 1088,
        max_side: 1920,
        level_idc: 40,
        min_bitrate: MIN_BITRATE,
        max_bitrate: 2_500_000,
    };

    /// Whether `width`×`height` can be encoded and sent.
    pub fn fits(&self, width: usize, height: usize) -> bool {
        width >= 16
            && height >= 16
            && width.is_multiple_of(2)
            && height.is_multiple_of(2)
            && width <= self.max_side
            && height <= self.max_side
            && width * height <= self.max_pixels
    }

    /// `bitrate` kept within these limits.
    pub fn bitrate(&self, bitrate: u32) -> u32 {
        bitrate.clamp(self.min_bitrate, self.max_bitrate)
    }
}

/// What the encoder is set up for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Settings {
    /// The picture's width in pixels, even.
    pub width: usize,
    /// Its height, even.
    pub height: usize,
    /// Frames a second.
    pub fps: u32,
    /// The average bitrate aimed at, in bit/s.
    pub bitrate: u32,
    /// What it may be asked for.
    pub limits: Limits,
}

/// Why a picture was not encoded.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum EncodeTrouble {
    /// The size is odd, empty or too large to send.
    #[error("{0}x{1} cannot be sent")]
    Size(usize, usize),
    /// The picture is not the size the encoder was set up for.
    #[error("a {0}x{1} picture for a {2}x{3} encoder")]
    Mismatch(usize, usize, usize, usize),
    /// The encoder refused.
    #[error("the encoder: {0}")]
    Encoder(String),
}

/// One encoded picture: an access unit in Annex B, as `str0m` sends it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Encoded {
    /// The NAL units, each with its start code.
    pub data: Vec<u8>,
    /// Whether it is an IDR (then with SPS and PPS before it).
    pub keyframe: bool,
}

/// A picture to encode, in the form its source has it: I420 from a
/// camera, BGRA (four bytes a pixel, blue first, rows `stride` bytes
/// apart) from a screen. An encoder that converts on the GPU takes BGRA
/// as it is; the software one converts it first.
#[derive(Clone, Copy, Debug)]
pub enum Input<'a> {
    /// Planar 4:2:0.
    I420(&'a I420),
    /// Packed BGRA or BGRx.
    Bgra {
        /// Width in pixels.
        width: usize,
        /// Height in pixels.
        height: usize,
        /// Bytes from one row to the next.
        stride: usize,
        /// The pixels.
        data: &'a [u8],
    },
}

/// An H.264 encoder for one size, as the sending threads use it.
pub trait Encode: Send {
    /// What it is set up for.
    fn settings(&self) -> Settings;
    /// Encodes one picture of that size; with `keyframe`, as an IDR.
    fn encode(&mut self, input: Input<'_>, keyframe: bool) -> Result<Encoded, EncodeTrouble>;
    /// Aims at `bitrate` from the next picture on, if this encoder can
    /// change it in place; false if a new encoder must be made for it
    /// (which costs a keyframe), as the software one must.
    fn set_bitrate(&mut self, bitrate: u32) -> bool;
    /// Its name, for the log.
    fn name(&self) -> &'static str;
}

/// Which encoder to make.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Backend {
    /// `rusty_h264`'s, on the calling thread.
    #[default]
    Software,
}

/// An encoder for `settings` from `backend`; its first picture is an IDR.
pub fn open(backend: Backend, settings: Settings) -> Result<Box<dyn Encode>, EncodeTrouble> {
    match backend {
        Backend::Software => Ok(Box::new(VideoEncoder::new(settings)?)),
    }
}

/// The software encoder for one size and bitrate.
pub struct VideoEncoder {
    encoder: Encoder,
    settings: Settings,
}

impl std::fmt::Debug for VideoEncoder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VideoEncoder")
            .field("settings", &self.settings)
            .finish_non_exhaustive()
    }
}

/// Whether `width`×`height` can be encoded and sent as a camera.
pub fn sendable(width: usize, height: usize) -> bool {
    Limits::CAMERA.fits(width, height)
}

impl VideoEncoder {
    /// An encoder for `settings`; its first picture is an IDR.
    pub fn new(settings: Settings) -> Result<Self, EncodeTrouble> {
        let limits = settings.limits;
        if !limits.fits(settings.width, settings.height) {
            return Err(EncodeTrouble::Size(settings.width, settings.height));
        }
        let fps = settings.fps.clamp(1, 60);
        let bitrate = limits.bitrate(settings.bitrate);
        let mut config = EncoderConfig::baseline(settings.width, settings.height);
        // Fast: integer-pel motion and SAD decisions, about 4 ms a 640×480
        // picture; Balanced was 6–7 ms for 3–5 % better pictures.
        config.preset = Preset::Fast;
        config.gop_size = fps * IDR_EVERY_SECONDS;
        // A keyframe may be forced any time.
        config.min_keyint = 1;
        config.framerate = fps as f32;
        config.bitrate = bitrate;
        // The rate control's starting quality: a little coarser than its
        // default, so the first IDR is not a burst of a second's bits.
        config.qp = 30;
        config.level_idc = limits.level_idc;
        let encoder = Encoder::new(config).map_err(|e| EncodeTrouble::Encoder(e.to_string()))?;
        Ok(Self {
            encoder,
            settings: Settings {
                fps,
                bitrate,
                ..settings
            },
        })
    }

    /// What it is set up for.
    pub fn settings(&self) -> Settings {
        self.settings
    }

    /// Makes the next picture an IDR.
    pub fn request_keyframe(&mut self) {
        self.encoder.request_keyframe();
    }

    /// Encodes one picture of the size it was set up for.
    pub fn encode(&mut self, picture: &I420) -> Result<Encoded, EncodeTrouble> {
        let Settings { width, height, .. } = self.settings;
        if (picture.width, picture.height) != (width, height) {
            return Err(EncodeTrouble::Mismatch(
                picture.width,
                picture.height,
                width,
                height,
            ));
        }
        let planes = YuvPlanes::tight(width, height, &picture.y, &picture.u, &picture.v)
            .filter(YuvPlanes::is_valid)
            .ok_or(EncodeTrouble::Mismatch(
                picture.width,
                picture.height,
                width,
                height,
            ))?;
        let data = self
            .encoder
            .encode_planes(&planes)
            .map_err(|e| EncodeTrouble::Encoder(e.to_string()))?;
        Ok(Encoded {
            keyframe: is_idr(&data),
            data,
        })
    }
}

impl Encode for VideoEncoder {
    fn settings(&self) -> Settings {
        self.settings
    }

    fn encode(&mut self, input: Input<'_>, keyframe: bool) -> Result<Encoded, EncodeTrouble> {
        if keyframe {
            self.request_keyframe();
        }
        match input {
            Input::I420(picture) => VideoEncoder::encode(self, picture),
            Input::Bgra {
                width,
                height,
                stride,
                data,
            } => {
                let Settings {
                    width: w,
                    height: h,
                    ..
                } = self.settings;
                let picture = from_bgra(data, width, height, stride)
                    .ok_or(EncodeTrouble::Mismatch(width, height, w, h))?;
                VideoEncoder::encode(self, &picture)
            }
        }
    }

    fn set_bitrate(&mut self, bitrate: u32) -> bool {
        self.settings.limits.bitrate(bitrate) == self.settings.bitrate
    }

    fn name(&self) -> &'static str {
        "software (rusty_h264)"
    }
}

/// Packed BGRA (or BGRx: the fourth byte is ignored) with rows `stride`
/// bytes apart as I420, BT.601 studio range, as WebRTC senders send it;
/// an odd size loses its last row or column. `None` when the buffer is
/// shorter than the size and stride say.
pub fn from_bgra(data: &[u8], width: usize, height: usize, stride: usize) -> Option<I420> {
    let (w, h) = (width & !1, height & !1);
    if w == 0 || h == 0 || stride < width.checked_mul(4)? {
        return None;
    }
    // Only the even rows are read, each `stride` apart.
    let needed = stride.checked_mul(h)?;
    if data.len() < needed {
        return None;
    }
    let mut out = I420::black(w, h);
    let size = |n: usize| u32::try_from(n).ok();
    let mut planar = yuv::YuvPlanarImageMut {
        y_plane: yuv::BufferStoreMut::Borrowed(&mut out.y),
        y_stride: size(w)?,
        u_plane: yuv::BufferStoreMut::Borrowed(&mut out.u),
        u_stride: size(w / 2)?,
        v_plane: yuv::BufferStoreMut::Borrowed(&mut out.v),
        v_stride: size(w / 2)?,
        width: size(w)?,
        height: size(h)?,
    };
    yuv::bgra_to_yuv420(
        &mut planar,
        &data[..needed],
        size(stride)?,
        yuv::YuvRange::Limited,
        yuv::YuvStandardMatrix::Bt601,
        yuv::YuvConversionMode::Balanced,
    )
    .ok()?;
    Some(out)
}

/// The NAL unit types in an Annex B access unit, in order.
pub fn nal_types(unit: &[u8]) -> Vec<u8> {
    let mut types = Vec::new();
    let mut zeros = 0;
    let mut i = 0;
    while i < unit.len() {
        let byte = unit[i];
        if zeros >= 2 && byte == 1 {
            if let Some(&header) = unit.get(i + 1) {
                types.push(header & 0x1f);
            }
            zeros = 0;
        } else if byte == 0 {
            zeros += 1;
        } else {
            zeros = 0;
        }
        i += 1;
    }
    types
}

/// Whether an access unit holds an IDR slice.
pub fn is_idr(unit: &[u8]) -> bool {
    nal_types(unit).contains(&5)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::huddle_audio::camera::pattern;
    use std::time::Duration;

    fn settings(width: usize, height: usize) -> Settings {
        Settings {
            width,
            height,
            fps: 15,
            bitrate: 800_000,
            limits: Limits::CAMERA,
        }
    }

    fn psnr(a: &[u8], b: &[u8]) -> f64 {
        let mse = a
            .iter()
            .zip(b)
            .map(|(&x, &y)| (f64::from(x) - f64::from(y)).powi(2))
            .sum::<f64>()
            / a.len() as f64;
        if mse == 0.0 {
            99.0
        } else {
            10.0 * (255.0 * 255.0 / mse).log10()
        }
    }

    /// A second of the test pattern, encoded and decoded again by
    /// rusty_h264's decoder: every picture comes back at its size and
    /// close to what went in; the first is an IDR with SPS and PPS, the
    /// rest are not; the stream is constrained baseline at level 3.1.
    #[test]
    fn a_sequence_encodes_and_decodes_back_closely() {
        for (width, height) in [(640, 480), (640, 360), (320, 180)] {
            let mut encoder = VideoEncoder::new(settings(width, height)).expect("an encoder");
            let mut decoder = rusty_h264_decoder::Decoder::new();
            let mut worst = 99.0f64;
            for n in 0..15u64 {
                let picture = pattern(width, height, n, Duration::from_millis(n * 66));
                let encoded = encoder.encode(&picture).expect("encoded");
                let types = nal_types(&encoded.data);
                if n == 0 {
                    assert!(encoded.keyframe);
                    assert_eq!(&types[..3], [7, 8, 5], "SPS and PPS before the IDR");
                    // SPS: profile 66 (baseline), constraint_set1 (so
                    // constrained baseline), level 3.1.
                    let at = encoded
                        .data
                        .windows(4)
                        .position(|w| w[..3] == [0, 0, 1] && w[3] & 0x1f == 7)
                        .expect("an SPS");
                    assert_eq!(encoded.data[at + 4], 66, "baseline");
                    assert_eq!(encoded.data[at + 5] & 0x40, 0x40, "constraint_set1");
                    assert_eq!(encoded.data[at + 6], 31, "level 3.1");
                } else {
                    assert!(!encoded.keyframe, "frame {n}");
                    assert_eq!(types, [1]);
                }
                let decoded = decoder
                    .decode(&encoded.data)
                    .expect("decodes")
                    .expect("a picture");
                assert_eq!((decoded.width, decoded.height), (width, height));
                worst = worst.min(psnr(&decoded.y, &picture.y));
            }
            assert!(worst > 30.0, "{width}x{height}: luma PSNR {worst:.1} dB");
        }
    }

    #[test]
    fn a_keyframe_comes_when_asked_and_every_few_seconds() {
        let mut encoder = VideoEncoder::new(settings(320, 240)).expect("an encoder");
        let mut keyframes = Vec::new();
        for n in 0..(15 * IDR_EVERY_SECONDS + 10) {
            if n == 7 {
                encoder.request_keyframe();
            }
            let picture = pattern(320, 240, u64::from(n), Duration::ZERO);
            let encoded = encoder.encode(&picture).expect("encoded");
            if encoded.keyframe {
                let types = nal_types(&encoded.data);
                assert!(
                    types.starts_with(&[7, 8]),
                    "SPS and PPS with every IDR: {types:?}"
                );
                keyframes.push(n);
            }
        }
        // Asked for at 7, and then one at most four seconds after it.
        assert_eq!(
            keyframes,
            [0, 7, 7 + 15 * IDR_EVERY_SECONDS],
            "a keyframe when asked and within {IDR_EVERY_SECONDS} s"
        );
    }

    #[test]
    fn sizes_it_cannot_send_are_refused_not_panicked_on() {
        for (width, height) in [
            (0, 0),
            (1, 1),
            (15, 16),
            (641, 480),
            (640, 481),
            (1920, 1080),
            (4096, 16),
        ] {
            assert_eq!(
                VideoEncoder::new(settings(width, height)).map(|_| ()),
                Err(EncodeTrouble::Size(width, height))
            );
        }
        // Sizes that are not whole macroblocks are fine.
        for (width, height) in [(16, 16), (18, 10 + 8), (642, 362), (1280, 720)] {
            let mut encoder = VideoEncoder::new(settings(width, height)).expect("an encoder");
            let encoded = encoder
                .encode(&pattern(width, height, 0, Duration::ZERO))
                .expect("encoded");
            assert!(encoded.keyframe);
        }
        // A picture of another size is an error.
        let mut encoder = VideoEncoder::new(settings(320, 240)).expect("an encoder");
        assert_eq!(
            encoder.encode(&I420::black(640, 480)),
            Err(EncodeTrouble::Mismatch(640, 480, 320, 240))
        );
        // As is one whose planes are short.
        let mut short = I420::black(320, 240);
        short.u.truncate(10);
        assert!(matches!(
            encoder.encode(&short),
            Err(EncodeTrouble::Mismatch(..))
        ));
    }

    #[test]
    fn the_bitrate_stays_in_bounds() {
        let mut low = settings(320, 240);
        low.bitrate = 1;
        assert_eq!(
            VideoEncoder::new(low)
                .expect("an encoder")
                .settings()
                .bitrate,
            MIN_BITRATE
        );
        low.bitrate = u32::MAX;
        assert_eq!(
            VideoEncoder::new(low)
                .expect("an encoder")
                .settings()
                .bitrate,
            MAX_BITRATE
        );
        let mut share = settings(1920, 1080);
        share.limits = Limits::SHARE;
        share.bitrate = u32::MAX;
        assert_eq!(
            VideoEncoder::new(share)
                .expect("an encoder")
                .settings()
                .bitrate,
            2_500_000
        );
    }

    /// A share is 1080p at level 4.0, which a camera may not be.
    #[test]
    fn a_share_encodes_1080p_at_level_4() {
        let mut share = settings(1920, 1080);
        assert_eq!(
            VideoEncoder::new(share).map(|_| ()),
            Err(EncodeTrouble::Size(1920, 1080)),
            "not as a camera"
        );
        share.limits = Limits::SHARE;
        assert!(!Limits::SHARE.fits(2560, 1440));
        assert!(!Limits::SHARE.fits(1920, 1200), "more pixels than 1080p");
        assert!(!Limits::SHARE.fits(2048, 512), "no side past 1920");
        assert!(Limits::SHARE.fits(1080, 1920), "a portrait screen");
        let mut encoder = open(Backend::Software, share).expect("an encoder");
        let picture = pattern(1920, 1080, 0, Duration::ZERO);
        let encoded = encoder
            .encode(Input::I420(&picture), false)
            .expect("encoded");
        assert!(encoded.keyframe);
        let at = encoded
            .data
            .windows(4)
            .position(|w| w[..3] == [0, 0, 1] && w[3] & 0x1f == 7)
            .expect("an SPS");
        assert_eq!(encoded.data[at + 6], 40, "level 4.0");
        let decoded = rusty_h264_decoder::Decoder::new()
            .decode(&encoded.data)
            .expect("decodes")
            .expect("a picture");
        assert_eq!((decoded.width, decoded.height), (1920, 1080));
        assert!(psnr(&decoded.y, &picture.y) > 30.0);
    }

    /// BGRA goes through the same interface: the software encoder
    /// converts it, and a keyframe comes when asked for.
    #[test]
    fn bgra_is_taken_through_the_interface_and_keyframes_on_request() {
        let (width, height) = (64, 48);
        // A grey ramp with a pad of 16 bytes on every row.
        let stride = width * 4 + 16;
        let mut data = vec![0u8; stride * height];
        for row in 0..height {
            for x in 0..width {
                let level = u8::try_from(x * 255 / width).expect("a byte");
                data[row * stride + x * 4..][..4].copy_from_slice(&[level, level, level, 255]);
            }
        }
        let mut encoder = open(Backend::Software, settings(width, height)).expect("an encoder");
        assert_eq!(encoder.name(), "software (rusty_h264)");
        let bgra = Input::Bgra {
            width,
            height,
            stride,
            data: &data,
        };
        assert!(encoder.encode(bgra, false).expect("encoded").keyframe);
        assert!(!encoder.encode(bgra, false).expect("encoded").keyframe);
        let forced = encoder.encode(bgra, true).expect("encoded");
        assert!(forced.keyframe, "asked for");
        let decoded = rusty_h264_decoder::Decoder::new()
            .decode(&forced.data)
            .expect("decodes")
            .expect("a picture");
        // Left dark, right light, in studio range.
        assert!(
            decoded.y[width * 20 + 2] < 40,
            "{}",
            decoded.y[width * 20 + 2]
        );
        assert!(decoded.y[width * 20 + 60] > 200);
        // Software cannot change its bitrate in place.
        assert!(!encoder.set_bitrate(1_200_000));
        assert!(encoder.set_bitrate(800_000), "the bitrate it has");
        // Too short a buffer is an error, not a panic.
        let short = Input::Bgra {
            width,
            height,
            stride,
            data: &data[..stride * 10],
        };
        assert!(matches!(
            encoder.encode(short, false),
            Err(EncodeTrouble::Mismatch(..))
        ));
        assert!(
            from_bgra(&data, width, height, width).is_none(),
            "stride too small"
        );
    }

    /// What a 1080p share costs to encode in software, one thread: run
    /// with `cargo test --release -- --ignored share_encode_cost
    /// --nocapture`. A desktop that types and scrolls a little: most of
    /// the picture still, a band changing each frame.
    #[test]
    #[ignore = "a measurement; run in release"]
    fn share_encode_cost() {
        for (width, height, bitrate, whole) in [
            (1920, 1080, 2_500_000, false),
            (1920, 1080, 2_500_000, true),
            (1280, 720, 1_500_000, false),
            (1280, 720, 1_500_000, true),
        ] {
            let mut encoder = open(
                Backend::Software,
                Settings {
                    width,
                    height,
                    fps: 15,
                    bitrate,
                    limits: Limits::SHARE,
                },
            )
            .expect("an encoder");
            let still = pattern(width, height, 0, Duration::ZERO);
            let mut times = Vec::new();
            let mut bytes = 0usize;
            for n in 0..60u64 {
                // The clock's band changes and the rest stays, or (`whole`)
                // everything moves, the worst a share can be.
                let moving = pattern(width, height, n, Duration::from_millis(n * 66));
                let picture = if whole {
                    moving
                } else {
                    let mut picture = still.clone();
                    let band = height / 2 * width;
                    picture.y[band..].copy_from_slice(&moving.y[band..]);
                    picture
                };
                let started = std::time::Instant::now();
                let encoded = encoder
                    .encode(Input::I420(&picture), n % 60 == 0)
                    .expect("encoded");
                times.push(started.elapsed());
                bytes += encoded.data.len();
            }
            times.sort();
            let mean = times.iter().sum::<Duration>() / u32::try_from(times.len()).unwrap_or(1);
            let p95 = times[times.len() * 95 / 100];
            let max = times[times.len() - 1];
            let kbps = bytes * 8 * 15 / 60 / 1000;
            let scene = if whole { "all moving" } else { "mostly still" };
            println!(
                "{width}x{height} {scene}: {:.1} ms mean, {:.1} p95, {:.1} max; {kbps} kbit/s",
                mean.as_secs_f64() * 1e3,
                p95.as_secs_f64() * 1e3,
                max.as_secs_f64() * 1e3
            );
        }
    }
}
