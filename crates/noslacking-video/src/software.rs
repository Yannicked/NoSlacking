//! Decoding on the processor: `rusty_h264` (pure Rust, BSD-2-Clause;
//! why that one is in docs/research/huddle-video.md, Stage 1), then the
//! whole-step shrink of [`crate::shrink`], so the pipe carries no more
//! than the app shows. What the helper uses when the GPU cannot decode
//! a stream, fails on it, or is not wanted (Settings → Huddles → Use the
//! graphics card for video, off), and on systems with no back end yet.
//!
//! It runs here rather than in the app because the decoder reads other
//! people's network data: release builds abort on a panic, and a
//! decoder's bug should end the helper, which the app starts again, not
//! the app.
//!
//! `rusty_h264` returns an error for a broken frame, but once it has, it
//! refuses every frame after, even the next keyframe. So a decoder that
//! failed is made afresh, and decodes nothing until a keyframe.

use noslacking_video_ipc::{Decoded, Planes};

use crate::backend::{Decoder, Failure};
use crate::shrink;

/// One stream's software decoder.
pub struct Software {
    decoder: rusty_h264_decoder::Decoder,
    /// Until a keyframe comes: at the start and after an error.
    waiting: bool,
    /// The box pictures should cover; 0×0 for their own size.
    fit: (u32, u32),
    /// The last frame's picture at its own size, until taken.
    last: Option<Planes>,
}

impl std::fmt::Debug for Software {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Software")
            .field("waiting", &self.waiting)
            .field("fit", &self.fit)
            .finish_non_exhaustive()
    }
}

impl Default for Software {
    fn default() -> Self {
        Self::new()
    }
}

impl Software {
    /// A decoder waiting for its first keyframe.
    pub fn new() -> Self {
        Self {
            decoder: rusty_h264_decoder::Decoder::new(),
            waiting: true,
            fit: (0, 0),
            last: None,
        }
    }
}

impl Decoder for Software {
    fn decode_frame(&mut self, frame: &[u8], keyframe: bool) -> Result<bool, Failure> {
        self.last = None;
        if self.waiting {
            if !keyframe {
                return Err(Failure::need_keyframe("waiting for a keyframe"));
            }
            self.waiting = false;
        }
        let picture = match self.decoder.decode(frame) {
            Ok(Some(picture)) => picture,
            Ok(None) => return Ok(false),
            Err(error) => {
                // It refuses everything after an error: a new one, for
                // the next keyframe.
                self.decoder = rusty_h264_decoder::Decoder::new();
                self.waiting = true;
                return Err(Failure::broken(error.to_string()));
            }
        };
        let side = |n: usize| u32::try_from(n).unwrap_or(u32::MAX);
        let planes = Planes {
            width: side(picture.width),
            height: side(picture.height),
            y: picture.y,
            u: picture.u,
            v: picture.v,
        };
        if planes.check().is_err() {
            self.decoder = rusty_h264_decoder::Decoder::new();
            self.waiting = true;
            return Err(Failure::broken(format!(
                "planes do not match {}x{}",
                planes.width, planes.height
            )));
        }
        // Kept as decoded (the decoder hands over its own planes): shrunk
        // only if it is taken.
        self.last = Some(planes);
        Ok(true)
    }

    fn picture(&mut self) -> Result<Option<Decoded>, Failure> {
        Ok(self.last.take().map(|planes| Decoded {
            source: (planes.width, planes.height),
            planes: shrink::shrink(planes, self.fit),
            hardware: false,
        }))
    }

    fn set_output_size(&mut self, width: u32, height: u32) {
        self.fit = (width, height);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use noslacking_video_ipc::FailKind;
    use sha2::{Digest, Sha256};

    const SCREEN: &[u8] =
        include_bytes!("../../../src/huddle_audio/fixtures/screen-1920x1080.h264");
    const CAMERA: &[u8] = include_bytes!("../../../src/huddle_audio/fixtures/camera-480x480.h264");

    /// Decodes every frame at its own size, hashing the pictures as
    /// ffmpeg writes them (`-f rawvideo -pix_fmt yuv420p`).
    fn decode_all(stream: &[u8]) -> (usize, (u32, u32), String) {
        let mut decoder = Software::new();
        let mut hash = Sha256::new();
        let mut count = 0;
        let mut size = (0, 0);
        for frame in crate::nal::access_units(stream) {
            let keyframe = crate::nal::is_keyframe(&frame);
            let decoded = decoder
                .decode(&frame, keyframe)
                .expect("decodes")
                .expect("a picture");
            assert!(!decoded.hardware);
            assert!(decoded.check().is_ok());
            size = (decoded.planes.width, decoded.planes.height);
            assert_eq!(decoded.source, size);
            hash.update(&decoded.planes.y);
            hash.update(&decoded.planes.u);
            hash.update(&decoded.planes.v);
            count += 1;
        }
        let hex: String = hash.finalize().iter().map(|b| format!("{b:02x}")).collect();
        (count, size, hex)
    }

    /// Both fixtures (OpenH264, constrained baseline, made with ffmpeg)
    /// decode to exactly what ffmpeg's own decoder makes of them.
    #[test]
    fn fixtures_decode_exactly_as_ffmpeg_does() {
        assert_eq!(
            decode_all(SCREEN),
            (
                36,
                (1920, 1080),
                "a2bd2fa8a81ad980725de116dbc10da22367e82186641f63c41bc914e5eb8812".into()
            )
        );
        assert_eq!(
            decode_all(CAMERA),
            (
                66,
                (480, 480),
                "e433e34c83ae538aa67adc4fae754ca4afc8892ea91546d6d55ffc97d699e3c2".into()
            )
        );
    }

    #[test]
    fn pictures_shrink_to_the_box_and_say_their_source() {
        let frames = crate::nal::access_units(CAMERA);
        let mut decoder = Software::new();
        decoder.set_output_size(160, 120);
        let decoded = decoder
            .decode(&frames[0], true)
            .expect("decodes")
            .expect("a picture");
        assert_eq!(
            (decoded.planes.width, decoded.planes.height),
            (160, 160),
            "a third: still covers 160x120"
        );
        assert_eq!(decoded.source, (480, 480));
        decoder.set_output_size(0, 0);
        let decoded = decoder
            .decode(&frames[1], false)
            .expect("decodes")
            .expect("a picture");
        assert_eq!((decoded.planes.width, decoded.planes.height), (480, 480));
    }

    #[test]
    fn nothing_decodes_before_a_keyframe_or_after_an_error_until_the_next() {
        let frames = crate::nal::access_units(CAMERA);
        let mut decoder = Software::new();
        // Joined mid-stream: a P frame first.
        let failure = decoder.decode(&frames[3], false).expect_err("waits");
        assert_eq!(failure.kind, FailKind::NeedKeyframe);
        assert!(
            decoder
                .decode(&frames[0], true)
                .expect("a keyframe")
                .is_some()
        );
        assert!(decoder.decode(&frames[1], false).expect("next").is_some());
        // Garbage: an error, then nothing until the keyframe at 44.
        let failure = decoder
            .decode(&[0, 0, 0, 1, 0x41, 0xff, 0xff, 0xff], false)
            .expect_err("broken");
        assert_eq!(failure.kind, FailKind::Broken);
        let failure = decoder.decode(&frames[2], false).expect_err("waits");
        assert_eq!(failure.kind, FailKind::NeedKeyframe);
        assert!(
            decoder
                .decode(&frames[44], true)
                .expect("recovers")
                .is_some()
        );
        assert!(
            decoder
                .decode(&frames[45], false)
                .expect("goes on")
                .is_some()
        );
    }

    /// Broken frames are errors, never a panic (a panic would end the
    /// helper: release builds abort), and the next keyframe recovers.
    #[test]
    fn broken_frames_are_errors_and_the_next_keyframe_recovers() {
        let frames = crate::nal::access_units(CAMERA);
        let mut seed = 0x9e37_79b9_7f4a_7c15_u64;
        let mut random = move |n: usize| {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            usize::try_from(seed % (n.max(1) as u64)).unwrap_or(0)
        };
        let mut broken = 0;
        for round in 0..30 {
            let mut decoder = Software::new();
            decoder.set_output_size(200, 150);
            for (i, frame) in frames.iter().enumerate() {
                let mut frame = frame.clone();
                if i % 7 == round % 7 {
                    match round % 3 {
                        0 => frame.truncate(random(frame.len())),
                        1 => {
                            for _ in 0..8 {
                                let at = random(frame.len());
                                frame[at] ^= 1 << random(8);
                            }
                        }
                        _ => {
                            let at = random(frame.len());
                            for byte in &mut frame[at..] {
                                *byte = u8::try_from(random(256)).unwrap_or(0);
                            }
                        }
                    }
                }
                let keyframe = crate::nal::is_keyframe(&frame);
                match decoder.decode(&frame, keyframe) {
                    Err(failure) if failure.kind == FailKind::Broken => broken += 1,
                    Ok(Some(decoded)) => assert!(decoded.check().is_ok()),
                    _ => {}
                }
            }
        }
        assert!(broken > 0, "some of it must have been noticed");
        // Garbage of every length, and nothing at all.
        let mut decoder = Software::new();
        for n in [0, 1, 4, 5, 100] {
            let mut junk = vec![0u8, 0, 0, 1, 0x65];
            junk.extend((0..n).map(|i| u8::try_from(i * 37 % 256).unwrap_or(0)));
            let _ = decoder.decode(&junk, true);
            let _ = decoder.decode(&junk[..n.min(junk.len())], true);
        }
        // And a clean keyframe afterwards decodes.
        let mut decoder = Software::new();
        let _ = decoder.decode(&[0, 0, 0, 1, 0x65, 0xff, 0xff], true);
        assert!(
            decoder
                .decode(&frames[0], true)
                .expect("recovers")
                .is_some()
        );
    }
}
