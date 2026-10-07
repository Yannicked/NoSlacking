//! Encoding on the processor: `rusty_h264`'s encoder (pure Rust,
//! BSD-2-Clause; chosen after a spike, docs/research/huddle-video.md
//! Stage 3), set up as WebRTC's single-layer H.264 is: constrained
//! baseline (CAVLC, one reference, no B-frames), level 3.1, one access
//! unit out for each picture in, its average-bitrate control on, every
//! IDR with the SPS and PPS in front of it. What a share uses where the
//! GPU cannot encode or is not wanted; at most 1280×720, since 1080p
//! takes 25–31 ms a picture on a fast laptop, too much for 15 a second.

use noslacking_video_ipc::Planes;
use rusty_h264_common::YuvPlanes;
use rusty_h264_encoder::{Encoder as Rusty, EncoderConfig, Preset};

use crate::backend::{Encoded, Failure};
use crate::nal;

/// A keyframe at least this often, in seconds.
pub const IDR_EVERY_SECONDS: u32 = 4;
/// The largest picture software encodes: 1280×720, level 3.1's limit.
pub const MAX_SIZE: (u32, u32) = (1280, 720);

/// The software encoder for one size and bitrate.
pub struct SoftwareEncoder {
    encoder: Rusty,
    size: (u32, u32),
    fps: u32,
    bitrate: u32,
}

impl std::fmt::Debug for SoftwareEncoder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SoftwareEncoder")
            .field("size", &self.size)
            .field("bitrate", &self.bitrate)
            .finish_non_exhaustive()
    }
}

/// Whether software encodes `width`×`height`: even, at least 16 a side,
/// no more pixels than 1280×720 and no side past 2048.
pub fn encodes(width: u32, height: u32) -> bool {
    width >= 16
        && height >= 16
        && width.is_multiple_of(2)
        && height.is_multiple_of(2)
        && width <= 2048
        && height <= 2048
        && u64::from(width) * u64::from(height) <= u64::from(MAX_SIZE.0 * MAX_SIZE.1)
}

impl SoftwareEncoder {
    /// An encoder of `size` pictures at `fps` and `bitrate` bit/s; its
    /// first picture is an IDR.
    pub fn new(size: (u32, u32), fps: u32, bitrate: u32) -> Result<Self, Failure> {
        if !encodes(size.0, size.1) {
            return Err(Failure::unsupported(format!(
                "{}x{} is not for software",
                size.0, size.1
            )));
        }
        let fps = fps.clamp(1, 60);
        let mut config = EncoderConfig::baseline(size.0 as usize, size.1 as usize);
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
        config.level_idc = 31;
        let encoder = Rusty::new(config).map_err(|e| Failure::device(e.to_string()))?;
        Ok(Self {
            encoder,
            size,
            fps,
            bitrate,
        })
    }

    /// The pictures' size.
    pub fn size(&self) -> (u32, u32) {
        self.size
    }

    /// The rate it was made for.
    pub fn bitrate(&self) -> u32 {
        self.bitrate
    }

    /// Pictures a second.
    pub fn fps(&self) -> u32 {
        self.fps
    }

    /// Encodes one picture of its size, an IDR if `force_keyframe`.
    pub fn encode(&mut self, picture: &Planes, force_keyframe: bool) -> Result<Encoded, Failure> {
        if (picture.width, picture.height) != self.size || picture.check().is_err() {
            return Err(Failure::broken(format!(
                "a {}x{} picture for a {}x{} encoder",
                picture.width, picture.height, self.size.0, self.size.1
            )));
        }
        if force_keyframe {
            self.encoder.request_keyframe();
        }
        let (width, height) = (self.size.0 as usize, self.size.1 as usize);
        let planes = YuvPlanes::tight(width, height, &picture.y, &picture.u, &picture.v)
            .filter(YuvPlanes::is_valid)
            .ok_or_else(|| Failure::broken("the planes do not fit"))?;
        let data = self
            .encoder
            .encode_planes(&planes)
            .map_err(|e| Failure::broken(e.to_string()))?;
        let keyframe = nal::types(&data).contains(&5);
        Ok(Encoded { keyframe, data })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capture::pattern::pattern;
    use std::time::Duration;

    #[test]
    fn it_encodes_what_the_decoder_reads_back_with_keyframes_when_asked() {
        let mut encoder = SoftwareEncoder::new((320, 180), 15, 400_000).expect("an encoder");
        let mut decoder = rusty_h264_decoder::Decoder::new();
        for n in 0..5u64 {
            let picture = pattern(320, 180, n, Duration::from_millis(n * 67));
            let encoded = encoder.encode(&picture, n == 3).expect("encoded");
            assert_eq!(encoded.keyframe, n == 0 || n == 3, "picture {n}");
            if encoded.keyframe {
                assert!(nal::types(&encoded.data).starts_with(&[7, 8]));
            }
            let back = decoder
                .decode(&encoded.data)
                .expect("decodes")
                .expect("a picture");
            assert_eq!((back.width, back.height), (320, 180));
        }
        assert!(
            SoftwareEncoder::new((1920, 1080), 15, 1).is_err(),
            "too large"
        );
        assert!(SoftwareEncoder::new((321, 180), 15, 1).is_err(), "odd");
        assert!(
            encoder
                .encode(&pattern(160, 90, 0, Duration::ZERO), false)
                .is_err()
        );
    }
}
