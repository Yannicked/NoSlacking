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

use rusty_h264_common::YuvPlanes;
use rusty_h264_encoder::{Encoder, EncoderConfig, Preset};

use super::camera::I420;

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

/// The encoder for one size and bitrate.
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
        config.bitrate = settings.bitrate.clamp(MIN_BITRATE, MAX_BITRATE);
        // The rate control's starting quality: a little coarser than its
        // default, so the first IDR is not a burst of a second's bits.
        config.qp = 30;
        config.level_idc = 31;
        let encoder = Encoder::new(config).map_err(|e| EncodeTrouble::Encoder(e.to_string()))?;
        Ok(Self {
            encoder,
            settings: Settings {
                fps,
                bitrate: config_bitrate(settings.bitrate),
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

fn config_bitrate(bitrate: u32) -> u32 {
    bitrate.clamp(MIN_BITRATE, MAX_BITRATE)
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
    }
}
