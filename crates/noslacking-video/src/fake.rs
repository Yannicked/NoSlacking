//! A back end with no hardware behind it, for tests: it "decodes" a
//! frame into a flat grey picture of the size it was opened at, as if
//! on the GPU, and has no GPU encoder (captures encode in software).
//! Chosen with `NOSLACKING_VIDEO_BACKEND=fake`.

use noslacking_video_ipc::{
    Capability, Codec, Decoded, Direction, MAX_SIDE, Planes, chroma_size, output_size,
};

use crate::backend::{Backend, Decoder, Failure};

/// The pretend back end.
#[derive(Debug, Default)]
pub struct Fake;

impl Backend for Fake {
    fn name(&self) -> String {
        "fake".to_owned()
    }

    fn capabilities(&self) -> Vec<Capability> {
        [Direction::Decode, Direction::Encode]
            .into_iter()
            .map(|direction| Capability {
                codec: Codec::H264,
                direction,
                max_width: 1920,
                max_height: 1088,
            })
            .collect()
    }

    fn open_decoder(
        &mut self,
        _: Codec,
        width: u32,
        height: u32,
    ) -> Result<Box<dyn Decoder>, Failure> {
        if width == 0 || height == 0 || width > MAX_SIDE || height > MAX_SIDE {
            return Err(Failure::unsupported(format!("{width}x{height}")));
        }
        Ok(Box::new(FakeDecoder {
            width,
            height,
            fit: (0, 0),
            started: false,
            frames: 0,
            unread: false,
        }))
    }
}

struct FakeDecoder {
    width: u32,
    height: u32,
    fit: (u32, u32),
    started: bool,
    frames: u8,
    /// A picture was decoded and not yet taken.
    unread: bool,
}

impl Decoder for FakeDecoder {
    fn decode_frame(&mut self, frame: &[u8], keyframe: bool) -> Result<bool, Failure> {
        self.unread = false;
        if !frame.starts_with(&[0, 0, 1]) && !frame.starts_with(&[0, 0, 0, 1]) {
            self.started = false;
            return Err(Failure::broken("no start code"));
        }
        if keyframe {
            self.started = true;
        }
        if !self.started {
            return Err(Failure::need_keyframe("no keyframe yet"));
        }
        // A frame of 0x41 0xff… stands for a picture that did not change.
        if !frame.ends_with(&[0x41, 0xff]) {
            self.frames = self.frames.wrapping_add(1);
        }
        self.unread = true;
        Ok(true)
    }

    fn picture(&mut self) -> Result<Option<Decoded>, Failure> {
        if !std::mem::take(&mut self.unread) {
            return Ok(None);
        }
        let (width, height) = output_size((self.width, self.height), self.fit);
        let (cw, ch) = chroma_size(width, height);
        let size = |w: u32, h: u32| usize::try_from(w * h).unwrap_or(0);
        Ok(Some(Decoded {
            planes: Planes {
                width,
                height,
                y: vec![self.frames; size(width, height)],
                u: vec![128; size(cw, ch)],
                v: vec![128; size(cw, ch)],
            },
            source: (self.width, self.height),
            hardware: true,
        }))
    }

    fn set_output_size(&mut self, width: u32, height: u32) {
        self.fit = (width, height);
    }
}
