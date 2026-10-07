//! Our camera's pictures as H.264 (the `huddle-camera` feature): the
//! encoder, set up as Chime's receivers take it, and made safe to feed.
//!
//! The encoder is `rusty_h264`'s, pure Rust; why that one is in
//! docs/research/huddle-video.md (Stage 3). It is set up as WebRTC's
//! single-layer H.264 is: constrained baseline (`42e01f`, CAVLC, one
//! reference frame, no B-frames), one access unit out for each picture
//! in, nothing held back, its average-bitrate control on. Every IDR
//! carries the SPS and PPS in front of it, so a receiver that joins late
//! or lost packets can start from it; one comes every few seconds and
//! whenever asked ([`VideoEncoder::request_keyframe`], for a receiver's
//! PLI or FIR).
//!
//! The encoder takes only even sizes and returns errors rather than
//! panicking; [`VideoEncoder::new`] also refuses sizes larger than this
//! feature sends, so nothing past level 3.1 goes out.
//!
//! What may be asked of it depends on what is sent ([`Limits`]): a
//! camera up to 1280×720, a screen share up to 1920×1080 on the GPU but
//! only 1280×720 in software, which takes about 25–31 ms a 1080p picture
//! on a fast laptop (docs/research/huddle-video.md, Stage 4), too much
//! for 15 a second beside everything else.
//!
//! [`Encoder`] is what a sender holds: the GPU's encoder through the
//! video helper ([`super::hardware`]) when the setting is on and the
//! helper encodes the size, else this one. A GPU that fails, a helper
//! that crashes or hangs, or a stream from it that is not what was asked
//! hands over to software at once, the picture in hand encoded again
//! there as a keyframe.

use rusty_h264_common::YuvPlanes;
use rusty_h264_encoder::{Encoder as Rusty, EncoderConfig, Preset};

use super::camera::I420;
use super::hardware::{Helper, HwEncoder};

/// A keyframe at least this often, in seconds: what a receiver that
/// missed the last one waits at most, if its PLI is lost too.
pub const IDR_EVERY_SECONDS: u32 = 4;
/// The most pixels encoded: 1280×720, level 3.1's limit.
const MAX_PIXELS: usize = 1280 * 720;
/// The least bitrate set, in bit/s: below it the picture is mush anyway.
pub const MIN_BITRATE: u32 = 150_000;
/// The most, in bit/s: Slack's own cameras send up to about 500 kbit/s at
/// 480×480; 640×480 at 30 frames a second looks good from 900 to 1,800.
pub const MAX_BITRATE: u32 = 1_800_000;
/// What sending starts at, before bandwidth estimation says more.
pub const START_BITRATE: u32 = 600_000;

/// What an encoder may be asked for: the largest picture (on the GPU;
/// software stays within [`sendable`]) and the bitrates it is kept
/// between.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Limits {
    /// The most pixels a picture may have.
    pub max_pixels: usize,
    /// The longest side, in pixels.
    pub max_side: usize,
    /// The least bitrate, in bit/s.
    pub min_bitrate: u32,
    /// The most, in bit/s.
    pub max_bitrate: u32,
}

impl Limits {
    /// A camera: up to 1280×720 (level 3.1), up to 1.8 Mbit/s.
    pub const CAMERA: Self = Self {
        max_pixels: MAX_PIXELS,
        max_side: 2048,
        min_bitrate: MIN_BITRATE,
        max_bitrate: MAX_BITRATE,
    };
    /// A screen share: up to 1920×1080 (Slack's own shares arrive at that
    /// size; the helper picks the level), up to 2.5 Mbit/s, the JS SDK's
    /// ceiling for content (`setVideoMaxBandwidthKbps(2500)`).
    pub const SHARE: Self = Self {
        max_pixels: 1920 * 1088,
        max_side: 1920,
        min_bitrate: MIN_BITRATE,
        max_bitrate: 2_500_000,
    };

    /// Whether `width`×`height` can be encoded and sent within these
    /// limits (on the GPU; software also needs [`sendable`]).
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

/// The software encoder for one size and bitrate.
pub struct VideoEncoder {
    encoder: Rusty,
    settings: Settings,
}

impl std::fmt::Debug for VideoEncoder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VideoEncoder")
            .field("settings", &self.settings)
            .finish_non_exhaustive()
    }
}

/// Whether `width`×`height` can be encoded and sent.
pub fn sendable(width: usize, height: usize) -> bool {
    width >= 16
        && height >= 16
        && width.is_multiple_of(2)
        && height.is_multiple_of(2)
        && width <= 2048
        && height <= 2048
        && width * height <= MAX_PIXELS
}

impl VideoEncoder {
    /// An encoder for `settings`; its first picture is an IDR.
    pub fn new(settings: Settings) -> Result<Self, EncodeTrouble> {
        if !sendable(settings.width, settings.height) {
            return Err(EncodeTrouble::Size(settings.width, settings.height));
        }
        let fps = settings.fps.clamp(1, 60);
        let mut config = EncoderConfig::baseline(settings.width, settings.height);
        // Fast: integer-pel motion and SAD decisions, about 4 ms a 640×480
        // picture; Balanced was 6–7 ms for 3–5 % better pictures.
        config.preset = Preset::Fast;
        config.gop_size = fps * IDR_EVERY_SECONDS;
        // A keyframe may be forced any time.
        config.min_keyint = 1;
        config.framerate = fps as f32;
        config.bitrate = settings.limits.bitrate(settings.bitrate);
        // The rate control's starting quality: a little coarser than its
        // default, so the first IDR is not a burst of a second's bits.
        config.qp = 30;
        config.level_idc = 31;
        let encoder = Rusty::new(config).map_err(|e| EncodeTrouble::Encoder(e.to_string()))?;
        Ok(Self {
            encoder,
            settings: Settings {
                fps,
                bitrate: settings.limits.bitrate(settings.bitrate),
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

/// What does the encoding.
enum Engine {
    /// rusty_h264, on this thread.
    Software(Box<VideoEncoder>),
    /// The GPU, through the helper.
    Hardware(HwEncoder),
}

/// One stream's encoder: on the GPU when it can be, else in software,
/// and in software for good once the GPU has failed it.
pub struct Encoder {
    engine: Engine,
    settings: Settings,
    /// The next picture must be an IDR.
    keyframe: bool,
}

impl std::fmt::Debug for Encoder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Encoder")
            .field("settings", &self.settings)
            .field("on_gpu", &self.on_gpu())
            .finish_non_exhaustive()
    }
}

impl Encoder {
    /// An encoder for `settings`: through `gpu`'s helper if it encodes
    /// this size, else (or if opening fails) in software. Its first
    /// picture is an IDR.
    ///
    /// Within its [`Limits`] on the GPU; software takes no more than
    /// [`sendable`], so a share larger than 1280×720 without a GPU that
    /// encodes it is refused ([`EncodeTrouble::Size`]) and the sender
    /// shrinks it.
    pub fn new(settings: Settings, gpu: Option<&Helper>) -> Result<Self, EncodeTrouble> {
        if !settings.limits.fits(settings.width, settings.height) {
            return Err(EncodeTrouble::Size(settings.width, settings.height));
        }
        let settings = Settings {
            fps: settings.fps.clamp(1, 60),
            bitrate: settings.limits.bitrate(settings.bitrate),
            ..settings
        };
        if let Some(helper) = gpu
            && let Some(engine) = Self::on_the_gpu(helper, settings)
        {
            return Ok(Self {
                engine,
                settings,
                keyframe: true,
            });
        }
        Ok(Self {
            engine: Engine::Software(Box::new(VideoEncoder::new(settings)?)),
            settings,
            keyframe: true,
        })
    }

    /// The GPU's encoder for `settings`, if the helper has one.
    fn on_the_gpu(helper: &Helper, settings: Settings) -> Option<Engine> {
        let size = |n: usize| u32::try_from(n).ok();
        let (width, height) = (size(settings.width)?, size(settings.height)?);
        let codec = noslacking_video_ipc::Codec::H264;
        if !helper.encodes(codec, width, height) {
            return None;
        }
        match helper.open_encoder(codec, width, height, settings.fps, settings.bitrate) {
            Ok(encoder) => Some(Engine::Hardware(encoder)),
            Err(why) => {
                log::info!("video: no encoder on the GPU ({why}): in software");
                None
            }
        }
    }

    /// What it is set up for.
    pub fn settings(&self) -> Settings {
        self.settings
    }

    /// Whether it encodes on the GPU.
    pub fn on_gpu(&self) -> bool {
        matches!(self.engine, Engine::Hardware(_))
    }

    /// Makes the next picture an IDR.
    pub fn request_keyframe(&mut self) {
        self.keyframe = true;
    }

    /// Aims at `bitrate` from the next picture on, without a keyframe, if
    /// the encoder can (the GPU's); false when it cannot, and a new
    /// encoder must be made for it (software's).
    pub fn retune(&mut self, bitrate: u32) -> bool {
        let bitrate = self.settings.limits.bitrate(bitrate);
        let Engine::Hardware(encoder) = &mut self.engine else {
            return false;
        };
        match encoder.set_bitrate(bitrate) {
            Ok(()) => {
                self.settings.bitrate = bitrate;
                true
            }
            Err(trouble) => {
                self.hand_to_software(&format!("{trouble:?}"));
                false
            }
        }
    }

    /// Encodes one picture of the size it was set up for. A failure on
    /// the GPU is not an error: the picture is encoded in software, as a
    /// keyframe, and so is every picture after it.
    pub fn encode(&mut self, picture: &I420) -> Result<Encoded, EncodeTrouble> {
        let Settings { width, height, .. } = self.settings;
        if (picture.width, picture.height) != (width, height) || !picture.whole() {
            return Err(EncodeTrouble::Mismatch(
                picture.width,
                picture.height,
                width,
                height,
            ));
        }
        let keyframe = std::mem::take(&mut self.keyframe);
        if let Engine::Hardware(encoder) = &mut self.engine {
            let planes = noslacking_video_ipc::Planes {
                width: encoder.size().0,
                height: encoder.size().1,
                y: picture.y.clone(),
                u: picture.u.clone(),
                v: picture.v.clone(),
            };
            match encoder.encode(planes, keyframe) {
                Ok(encoded) => match checked(encoded.data, keyframe) {
                    Ok(encoded) => return Ok(encoded),
                    Err(why) => self.hand_to_software(why),
                },
                Err(trouble) => self.hand_to_software(&format!("{trouble:?}")),
            }
        }
        let Engine::Software(encoder) = &mut self.engine else {
            return Err(EncodeTrouble::Encoder("no encoder".into()));
        };
        // Asked for, or set by a hand-over to software just now (whose
        // fresh encoder starts with one anyway).
        if keyframe || std::mem::take(&mut self.keyframe) {
            encoder.request_keyframe();
        }
        encoder.encode(picture)
    }

    /// The GPU failed: software from now on, starting with a keyframe.
    fn hand_to_software(&mut self, why: &str) {
        log::warn!("video: encoding on the GPU failed ({why}): in software from here on");
        // A fresh software encoder's first picture is an IDR; on the
        // rare chance it cannot be made, the next picture tries again.
        match VideoEncoder::new(self.settings) {
            Ok(encoder) => self.engine = Engine::Software(Box::new(encoder)),
            Err(error) => log::warn!("video: {error}"),
        }
        self.keyframe = true;
    }
}

/// The helper's access unit, checked before it goes out: not empty, a
/// slice in it, an IDR with SPS and PPS in front when one was asked for.
/// Its keyframe flag is read from the stream, not taken from the helper.
fn checked(data: Vec<u8>, asked_keyframe: bool) -> Result<Encoded, &'static str> {
    let types = nal_types(&data);
    if !types.iter().any(|&t| t == 1 || t == 5) {
        return Err("no slice from the helper");
    }
    let keyframe = types.contains(&5);
    if asked_keyframe && !keyframe {
        return Err("no keyframe from the helper when asked");
    }
    if keyframe && !types.starts_with(&[7, 8]) {
        return Err("a keyframe from the helper without its parameter sets");
    }
    Ok(Encoded { data, keyframe })
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
        let mut share = settings(1280, 720);
        share.limits = Limits::SHARE;
        share.bitrate = u32::MAX;
        assert_eq!(
            Encoder::new(share, None)
                .expect("an encoder")
                .settings()
                .bitrate,
            2_500_000
        );
    }

    /// A share may be 1080p on the GPU, which a camera may not be; in
    /// software it is refused above 720p, so the sender shrinks it.
    #[test]
    fn a_share_is_up_to_1080p_on_the_gpu_and_720p_in_software() {
        assert!(Limits::SHARE.fits(1920, 1080));
        assert!(Limits::SHARE.fits(1080, 1920), "a portrait screen");
        assert!(!Limits::SHARE.fits(1920, 1200), "more pixels than 1080p");
        assert!(!Limits::SHARE.fits(2048, 512), "no side past 1920");
        assert!(!Limits::CAMERA.fits(1920, 1080));
        let mut share = settings(1920, 1080);
        share.limits = Limits::SHARE;
        assert_eq!(
            Encoder::new(share, None).map(|_| ()),
            Err(EncodeTrouble::Size(1920, 1080)),
            "no GPU: not in software"
        );
        share.width = 1280;
        share.height = 720;
        let mut encoder = Encoder::new(share, None).expect("an encoder");
        assert!(!encoder.on_gpu());
        let picture = pattern(1280, 720, 0, Duration::ZERO);
        assert!(encoder.encode(&picture).expect("encoded").keyframe);
    }

    #[test]
    fn bgra_converts_to_studio_range_i420() {
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
        let picture = from_bgra(&data, width, height, stride).expect("converted");
        assert!(picture.whole());
        assert!(
            picture.y[width * 20 + 1] < 25,
            "{}",
            picture.y[width * 20 + 1]
        );
        assert!(picture.y[width * 20 + 62] > 220);
        assert!(from_bgra(&data[..stride * 10], width, height, stride).is_none());
        assert!(
            from_bgra(&data, width, height, width).is_none(),
            "stride too small"
        );
    }

    /// What a share costs to encode in software, one thread: run with
    /// `cargo test --release --features huddle-camera -- --ignored
    /// share_encode_cost --nocapture`. A desktop that types a little
    /// (most of the picture still, a band changing each frame), and one
    /// where everything moves, the worst a share can be.
    #[test]
    #[ignore = "a measurement; run in release"]
    #[allow(clippy::print_stdout, reason = "the measurement is for the reader")]
    fn share_encode_cost() {
        for (width, height, bitrate, whole) in [
            (1920, 1080, 2_500_000, false),
            (1920, 1080, 2_500_000, true),
            (1280, 720, 1_500_000, false),
            (1280, 720, 1_500_000, true),
        ] {
            let mut config = EncoderConfig::baseline(width, height);
            config.preset = Preset::Fast;
            config.gop_size = 60;
            config.min_keyint = 1;
            config.framerate = 15.0;
            config.bitrate = bitrate;
            config.qp = 30;
            config.level_idc = 40;
            let mut encoder = Rusty::new(config).expect("an encoder");
            let still = pattern(width, height, 0, Duration::ZERO);
            let mut times = Vec::new();
            let mut bytes = 0usize;
            for n in 0..60u64 {
                let moving = pattern(width, height, n, Duration::from_millis(n * 66));
                let picture = if whole {
                    moving
                } else {
                    let mut picture = still.clone();
                    let band = height / 2 * width;
                    picture.y[band..].copy_from_slice(&moving.y[band..]);
                    picture
                };
                let planes = YuvPlanes::tight(width, height, &picture.y, &picture.u, &picture.v)
                    .expect("planes");
                let started = std::time::Instant::now();
                let data = encoder.encode_planes(&planes).expect("encoded");
                times.push(started.elapsed());
                bytes += data.len();
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

/// The GPU's encoder as the sender sees it, against a pretend helper.
#[cfg(test)]
mod gpu_tests {
    use super::*;
    use crate::huddle_audio::camera::pattern;
    use crate::huddle_audio::hardware::pretend::{Act, Pretend, welcome};
    use noslacking_video_ipc::{
        self as ipc, Capability, Codec, Direction, FailKind, Reply, Request,
    };
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    fn settings() -> Settings {
        Settings {
            width: 320,
            height: 240,
            fps: 15,
            bitrate: 600_000,
            limits: Limits::CAMERA,
        }
    }

    /// What a pretend GPU makes of a picture: an IDR with its parameter
    /// sets when asked (or first), else a P slice.
    fn unit(keyframe: bool) -> Vec<u8> {
        if keyframe {
            vec![
                0, 0, 0, 1, 0x67, 66, 0xe0, 31, 0, 0, 0, 1, 0x68, 0xce, 0, 0, 0, 1, 0x65, 0x88,
            ]
        } else {
            vec![0, 0, 0, 1, 0x61, 0x9a]
        }
    }

    /// A helper that encodes on its pretend GPU, except that `fail` may
    /// make the `n`th encode (from 0), or a new bit rate (`usize::MAX`),
    /// do something else; every request is recorded.
    fn helper(
        fail: impl Fn(usize, &Request) -> Option<Act> + Send + Sync + 'static,
    ) -> (Helper, Arc<Mutex<Vec<Request>>>) {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let record = Arc::clone(&seen);
        let encodes = AtomicUsize::new(0);
        let pretend = Pretend::new(move |request| {
            record.lock().expect("not poisoned").push(request.clone());
            match request {
                Request::Hello { .. } => Act::Reply(welcome()),
                Request::OpenEncoder { .. } => Act::Reply(Reply::Opened { id: 4 }),
                Request::Encode { force_keyframe, .. } => {
                    let n = encodes.fetch_add(1, Ordering::Relaxed);
                    fail(n, request).unwrap_or_else(|| {
                        Act::Reply(Reply::Encoded {
                            keyframe: *force_keyframe,
                            data: unit(*force_keyframe),
                        })
                    })
                }
                Request::SetBitrate { .. } => {
                    fail(usize::MAX, request).unwrap_or(Act::Reply(Reply::Done))
                }
                _ => Act::Reply(Reply::Done),
            }
        });
        let helper = Helper::with_timeouts(
            Arc::new(pretend),
            Duration::from_secs(5),
            Duration::from_millis(300),
        );
        (helper, seen)
    }

    fn picture(n: u64) -> I420 {
        pattern(320, 240, n, Duration::ZERO)
    }

    fn forced(seen: &Mutex<Vec<Request>>) -> Vec<bool> {
        seen.lock()
            .expect("not poisoned")
            .iter()
            .filter_map(|r| match r {
                Request::Encode { force_keyframe, .. } => Some(*force_keyframe),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn the_gpu_encodes_when_the_helper_can() {
        let (gpu, seen) = helper(|_, _| None);
        let mut encoder = Encoder::new(settings(), Some(&gpu)).expect("an encoder");
        assert!(encoder.on_gpu());
        let first = encoder.encode(&picture(0)).expect("encoded");
        assert!(first.keyframe, "the first is an IDR");
        assert_eq!(first.data, unit(true));
        let second = encoder.encode(&picture(1)).expect("encoded");
        assert!(!second.keyframe);
        encoder.request_keyframe();
        assert!(encoder.encode(&picture(2)).expect("encoded").keyframe);
        assert_eq!(forced(&seen), [true, false, true]);
        // The picture went over whole.
        let sizes: Vec<(u32, u32, usize)> = seen
            .lock()
            .expect("not poisoned")
            .iter()
            .filter_map(|r| match r {
                Request::Encode { picture, .. } => {
                    Some((picture.width, picture.height, picture.y.len()))
                }
                _ => None,
            })
            .collect();
        assert_eq!(sizes[0], (320, 240, 320 * 240));
        drop(encoder);
        assert!(
            seen.lock()
                .expect("not poisoned")
                .iter()
                .any(|r| matches!(r, Request::Close { id: 4 })),
            "closed when dropped"
        );
    }

    #[test]
    fn without_the_setting_or_the_size_it_is_software() {
        let mut software = Encoder::new(settings(), None).expect("an encoder");
        assert!(!software.on_gpu());
        assert!(software.encode(&picture(0)).expect("encoded").keyframe);
        // A helper that does not encode this size.
        let pretend = Pretend::new(|request| match request {
            Request::Hello { .. } => Act::Reply(Reply::Welcome {
                version: ipc::VERSION,
                backend: "pretend".into(),
                capabilities: vec![Capability {
                    codec: Codec::H264,
                    direction: Direction::Encode,
                    max_width: 160,
                    max_height: 120,
                }],
            }),
            _ => Act::Reply(Reply::Done),
        });
        let small = Helper::new(Arc::new(pretend));
        assert!(
            !Encoder::new(settings(), Some(&small))
                .expect("an encoder")
                .on_gpu()
        );
        // One whose GPU will not open an encoder.
        let pretend = Pretend::new(|request| match request {
            Request::Hello { .. } => Act::Reply(welcome()),
            _ => Act::Reply(Reply::Failed {
                kind: FailKind::Unsupported,
                detail: "no".into(),
            }),
        });
        let refusing = Helper::new(Arc::new(pretend));
        let mut encoder = Encoder::new(settings(), Some(&refusing)).expect("an encoder");
        assert!(!encoder.on_gpu());
        assert!(encoder.encode(&picture(0)).expect("encoded").keyframe);
    }

    /// The helper crashes on the third picture: that picture comes out of
    /// software as a keyframe the software decoder reads, and the rest
    /// follow in software.
    #[test]
    fn a_crashed_helper_hands_over_to_software_with_a_keyframe() {
        let (gpu, _) = helper(|n, _| (n == 2).then_some(Act::Crash));
        let mut encoder = Encoder::new(settings(), Some(&gpu)).expect("an encoder");
        assert!(encoder.on_gpu());
        for n in 0..2 {
            encoder.encode(&picture(n)).expect("encoded");
        }
        let handed = encoder.encode(&picture(2)).expect("encoded in software");
        assert!(!encoder.on_gpu());
        assert!(handed.keyframe);
        assert!(nal_types(&handed.data).starts_with(&[7, 8, 5]));
        let mut decoder = rusty_h264_decoder::Decoder::new();
        let decoded = decoder
            .decode(&handed.data)
            .expect("decodes")
            .expect("a picture");
        assert_eq!((decoded.width, decoded.height), (320, 240));
        let next = encoder.encode(&picture(3)).expect("encoded");
        assert!(!next.keyframe);
        assert!(decoder.decode(&next.data).expect("decodes").is_some());
    }

    #[test]
    fn a_stuck_helper_times_out_to_software() {
        let (gpu, _) = helper(|n, _| (n == 1).then_some(Act::Hang));
        let mut encoder = Encoder::new(settings(), Some(&gpu)).expect("an encoder");
        encoder.encode(&picture(0)).expect("encoded");
        let started = std::time::Instant::now();
        let handed = encoder.encode(&picture(1)).expect("encoded in software");
        assert!(started.elapsed() < Duration::from_secs(3), "the timeout");
        assert!(handed.keyframe && !encoder.on_gpu());
    }

    /// A GPU failure the helper survives, or a stream that is not what
    /// was asked, hands over the same way.
    #[test]
    fn failures_and_bad_streams_mean_software() {
        let cases: [fn(usize, &Request) -> Option<Act>; 4] = [
            |n, _| {
                (n == 1).then(|| {
                    Act::Reply(Reply::Failed {
                        kind: FailKind::Device,
                        detail: "the driver".into(),
                    })
                })
            },
            // No keyframe when one was asked for.
            |_, request| {
                matches!(
                    request,
                    Request::Encode {
                        force_keyframe: true,
                        ..
                    }
                )
                .then(|| {
                    Act::Reply(Reply::Encoded {
                        keyframe: true,
                        data: unit(false),
                    })
                })
            },
            // An IDR without its parameter sets.
            |n, _| {
                (n == 1).then(|| {
                    Act::Reply(Reply::Encoded {
                        keyframe: true,
                        data: vec![0, 0, 0, 1, 0x65, 0x88],
                    })
                })
            },
            // No slice at all.
            |n, _| {
                (n == 1).then(|| {
                    Act::Reply(Reply::Encoded {
                        keyframe: false,
                        data: vec![0, 0, 0, 1, 0x09, 0x10],
                    })
                })
            },
        ];
        for (i, fail) in cases.into_iter().enumerate() {
            let (gpu, _) = helper(fail);
            let mut encoder = Encoder::new(settings(), Some(&gpu)).expect("an encoder");
            let mut keyframes = Vec::new();
            for n in 0..3 {
                let encoded = encoder.encode(&picture(n)).expect("encoded");
                keyframes.push(encoded.keyframe);
            }
            assert!(!encoder.on_gpu(), "case {i}");
            assert!(keyframes.contains(&true), "case {i}: {keyframes:?}");
            assert!(!keyframes[2], "case {i}: software goes on with P pictures");
        }
    }

    #[test]
    fn the_gpu_takes_a_new_bitrate_without_a_keyframe() {
        let (gpu, seen) = helper(|_, _| None);
        let mut encoder = Encoder::new(settings(), Some(&gpu)).expect("an encoder");
        encoder.encode(&picture(0)).expect("encoded");
        assert!(encoder.retune(900_000));
        assert_eq!(encoder.settings().bitrate, 900_000);
        // Kept within what is sent.
        assert!(encoder.retune(u32::MAX));
        assert_eq!(encoder.settings().bitrate, MAX_BITRATE);
        assert!(!encoder.encode(&picture(1)).expect("encoded").keyframe);
        let rates: Vec<u32> = seen
            .lock()
            .expect("not poisoned")
            .iter()
            .filter_map(|r| match r {
                Request::SetBitrate { id: 4, bitrate } => Some(*bitrate),
                _ => None,
            })
            .collect();
        assert_eq!(rates, [900_000, MAX_BITRATE]);
        // Software cannot: the sender makes a new encoder.
        let mut software = Encoder::new(settings(), None).expect("an encoder");
        assert!(!software.retune(900_000));
        assert_eq!(software.settings().bitrate, 600_000);
        // A helper gone by the time the rate changes: software, which then
        // starts with a keyframe.
        let (dying, _) =
            helper(|_, r| matches!(r, Request::SetBitrate { .. }).then_some(Act::Crash));
        let mut encoder = Encoder::new(settings(), Some(&dying)).expect("an encoder");
        encoder.encode(&picture(0)).expect("encoded");
        assert!(!encoder.retune(900_000));
        assert!(!encoder.on_gpu());
        assert!(encoder.encode(&picture(1)).expect("encoded").keyframe);
    }
}
