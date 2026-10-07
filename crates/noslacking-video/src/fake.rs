//! A back end with no hardware behind it, for tests: it "decodes" a
//! frame into a flat grey picture of the size it was opened at, and
//! "encodes" a picture into a made-up NAL unit. Chosen with
//! `NOSLACKING_VIDEO_BACKEND=fake`.

use noslacking_video_ipc::{
    Capability, Codec, Direction, MAX_SIDE, Planes, chroma_size, output_size,
};

use crate::backend::{Backend, Decoder, Encoded, Encoder, Failure};

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
        }))
    }

    fn open_encoder(
        &mut self,
        _: Codec,
        width: u32,
        height: u32,
        _: u32,
        _: u32,
    ) -> Result<Box<dyn Encoder>, Failure> {
        Ok(Box::new(FakeEncoder {
            width,
            height,
            frames: 0,
        }))
    }
}

struct FakeDecoder {
    width: u32,
    height: u32,
    fit: (u32, u32),
    started: bool,
    frames: u8,
}

impl Decoder for FakeDecoder {
    fn decode(&mut self, frame: &[u8], keyframe: bool) -> Result<Option<Planes>, Failure> {
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
        self.frames = self.frames.wrapping_add(1);
        let (width, height) = output_size((self.width, self.height), self.fit);
        let (cw, ch) = chroma_size(width, height);
        let size = |w: u32, h: u32| usize::try_from(w * h).unwrap_or(0);
        Ok(Some(Planes {
            width,
            height,
            y: vec![self.frames; size(width, height)],
            u: vec![128; size(cw, ch)],
            v: vec![128; size(cw, ch)],
        }))
    }

    fn set_output_size(&mut self, width: u32, height: u32) {
        self.fit = (width, height);
    }
}

struct FakeEncoder {
    width: u32,
    height: u32,
    frames: u32,
}

impl Encoder for FakeEncoder {
    fn encode(&mut self, picture: &Planes, force_keyframe: bool) -> Result<Encoded, Failure> {
        if picture.width != self.width || picture.height != self.height {
            return Err(Failure::broken("not the size the encoder was opened at"));
        }
        let keyframe = force_keyframe || self.frames == 0;
        self.frames += 1;
        Ok(Encoded {
            keyframe,
            data: vec![0, 0, 0, 1, if keyframe { 0x65 } else { 0x41 }, 0x80],
        })
    }

    fn set_bitrate(&mut self, _: u32) -> Result<(), Failure> {
        Ok(())
    }
}
