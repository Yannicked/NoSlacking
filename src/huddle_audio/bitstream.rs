//! Just enough of the video bitstreams to say what a huddle sends,
//! without decoding a picture: H.264's sequence parameter set (profile,
//! level and size; read in [`noslacking_video_ipc::h264`], which the
//! video helper shares), VP8's keyframe header, and IVF files for
//! dumping VP8. `str0m` hands over H.264 frames in Annex B (start codes
//! before each NAL unit), and VP8 frames with the RTP payload descriptor
//! already gone.

use std::io::{Seek, SeekFrom, Write};

pub use noslacking_video_ipc::h264::{Sps, parse_sps, sps_of_frame};

/// What a VP8 frame's header says (RFC 6386 §9.1).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Vp8Header {
    /// A keyframe (intra frame).
    pub keyframe: bool,
    /// The first partition's size in bytes, from the frame tag.
    pub first_partition: u32,
    /// The header's size: 10 bytes on a keyframe (the 3-byte tag, the
    /// start code and the dimensions), 3 on an inter frame.
    pub header_bytes: usize,
    /// On a keyframe, the width and height in pixels.
    pub size: Option<(u16, u16)>,
}

/// Reads a VP8 frame's header. `None` if it is too short, or a keyframe
/// without VP8's start code.
pub fn vp8_header(frame: &[u8]) -> Option<Vp8Header> {
    let tag = u32::from(*frame.first()?)
        | u32::from(*frame.get(1)?) << 8
        | u32::from(*frame.get(2)?) << 16;
    let keyframe = tag & 1 == 0;
    let first_partition = tag >> 5;
    if !keyframe {
        return Some(Vp8Header {
            keyframe,
            first_partition,
            header_bytes: 3,
            size: None,
        });
    }
    if frame.get(3..6)? != [0x9d, 0x01, 0x2a] {
        return None;
    }
    let width = u16::from_le_bytes([*frame.get(6)?, *frame.get(7)?]) & 0x3fff;
    let height = u16::from_le_bytes([*frame.get(8)?, *frame.get(9)?]) & 0x3fff;
    Some(Vp8Header {
        keyframe,
        first_partition,
        header_bytes: 10,
        size: Some((width, height)),
    })
}

/// IVF's 32-byte file header for VP8 at a 90 kHz time base: what
/// ffmpeg, ffprobe and libvpx's tools read.
pub fn ivf_header(width: u16, height: u16, frames: u32) -> [u8; 32] {
    let mut h = [0u8; 32];
    h[0..4].copy_from_slice(b"DKIF");
    h[4..6].copy_from_slice(&0u16.to_le_bytes()); // version
    h[6..8].copy_from_slice(&32u16.to_le_bytes()); // header size
    h[8..12].copy_from_slice(b"VP80");
    h[12..14].copy_from_slice(&width.to_le_bytes());
    h[14..16].copy_from_slice(&height.to_le_bytes());
    // Time base: rate / scale = 90000 / 1, RTP's video clock.
    h[16..20].copy_from_slice(&90_000u32.to_le_bytes());
    h[20..24].copy_from_slice(&1u32.to_le_bytes());
    h[24..28].copy_from_slice(&frames.to_le_bytes());
    h
}

/// IVF's 12-byte header before each frame: its size and timestamp.
pub fn ivf_frame_header(size: u32, pts: u64) -> [u8; 12] {
    let mut h = [0u8; 12];
    h[0..4].copy_from_slice(&size.to_le_bytes());
    h[4..12].copy_from_slice(&pts.to_le_bytes());
    h
}

/// How many frames a dump keeps.
pub const DUMP_FRAMES: u32 = 300;

/// The two kinds of dump.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DumpKind {
    /// H.264 as `str0m` gives it: Annex B, frames one after another.
    AnnexB,
    /// VP8 in IVF.
    Ivf,
}

/// One stream's first frames, written to a file as they come.
pub struct Dump<W: Write + Seek> {
    out: W,
    kind: DumpKind,
    frames: u32,
    first_time: Option<u64>,
    size: (u16, u16),
}

impl<W: Write + Seek> std::fmt::Debug for Dump<W> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Dump")
            .field("kind", &self.kind)
            .field("frames", &self.frames)
            .finish_non_exhaustive()
    }
}

impl<W: Write + Seek> Dump<W> {
    /// Starts a dump; an IVF one writes its header now, with no size or
    /// count yet (they are filled in by [`Self::finish`]).
    pub fn new(mut out: W, kind: DumpKind) -> std::io::Result<Self> {
        if kind == DumpKind::Ivf {
            out.write_all(&ivf_header(0, 0, 0))?;
        }
        Ok(Self {
            out,
            kind,
            frames: 0,
            first_time: None,
            size: (0, 0),
        })
    }

    /// Frames written so far.
    pub fn frames(&self) -> u32 {
        self.frames
    }

    /// Whether it has all it keeps.
    pub fn full(&self) -> bool {
        self.frames >= DUMP_FRAMES
    }

    /// Writes one frame, `time` its RTP time at 90 kHz; nothing once full.
    pub fn write(&mut self, frame: &[u8], time: u64) -> std::io::Result<()> {
        if self.full() {
            return Ok(());
        }
        match self.kind {
            DumpKind::AnnexB => self.out.write_all(frame)?,
            DumpKind::Ivf => {
                if let Some(Vp8Header {
                    size: Some(size), ..
                }) = vp8_header(frame)
                {
                    self.size = size;
                }
                let first = *self.first_time.get_or_insert(time);
                let size = u32::try_from(frame.len()).unwrap_or(u32::MAX);
                self.out
                    .write_all(&ivf_frame_header(size, time.saturating_sub(first)))?;
                self.out.write_all(frame)?;
            }
        }
        self.frames += 1;
        Ok(())
    }

    /// Fills in an IVF header's size and frame count and flushes.
    pub fn finish(mut self) -> std::io::Result<W> {
        if self.kind == DumpKind::Ivf {
            let end = self.out.stream_position()?;
            self.out.seek(SeekFrom::Start(0))?;
            self.out
                .write_all(&ivf_header(self.size.0, self.size.1, self.frames))?;
            self.out.seek(SeekFrom::Start(end))?;
        }
        self.out.flush()?;
        Ok(self.out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(s: &str) -> Vec<u8> {
        (0..s.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&s[i..i + 2], 16).expect("hex"))
            .collect()
    }

    #[test]
    fn the_fixture_starts_with_its_sps() {
        let stream = include_bytes!("fixtures/test-pattern-320x180.h264");
        let sps = sps_of_frame(stream).expect("an SPS");
        assert_eq!((sps.width, sps.height, sps.level_idc), (320, 180, 12));
    }

    #[test]
    fn vp8_headers_read() {
        // libvpx's keyframe at 320x180.
        let key = vp8_header(&hex("7051009d012a4001b4000047")).expect("a header");
        assert!(key.keyframe);
        assert_eq!(key.size, Some((320, 180)));
        assert_eq!(key.header_bytes, 10);
        assert_eq!(key.first_partition, 651);
        let inter = vp8_header(&[0x31, 0x02, 0x00]).expect("a header");
        assert!(!inter.keyframe);
        assert_eq!((inter.header_bytes, inter.size), (3, None));
        assert_eq!(
            vp8_header(&[0x70, 0x51, 0x00, 1, 2, 3]),
            None,
            "no start code"
        );
        assert_eq!(vp8_header(&[0x70]), None);
    }

    #[test]
    fn ivf_is_written_as_libvpx_writes_it() {
        // The header libvpx wrote for one 320x180 frame at 1/15 s, but
        // at our 90 kHz time base.
        let header = ivf_header(320, 180, 1);
        assert_eq!(
            hex("444b494600002000565038304001b400").as_slice(),
            &header[..16]
        );
        assert_eq!(&header[16..24], &[0x90, 0x5f, 0x01, 0, 1, 0, 0, 0]);
        assert_eq!(&header[24..28], &[1, 0, 0, 0]);
        assert_eq!(
            ivf_frame_header(0x10bb, 3000),
            [0xbb, 0x10, 0, 0, 0xb8, 0x0b, 0, 0, 0, 0, 0, 0]
        );

        let mut dump = Dump::new(std::io::Cursor::new(Vec::new()), DumpKind::Ivf).expect("a dump");
        let key = hex("7051009d012a4001b4000047");
        dump.write(&key, 90_000).expect("written");
        dump.write(&[0x31, 0x02, 0x00], 96_000).expect("written");
        let bytes = dump.finish().expect("finished").into_inner();
        assert_eq!(&bytes[..32], &ivf_header(320, 180, 2));
        assert_eq!(&bytes[32..44], &ivf_frame_header(12, 0));
        assert_eq!(&bytes[44..56], &key[..]);
        assert_eq!(&bytes[56..68], &ivf_frame_header(3, 6000));
        assert_eq!(bytes.len(), 32 + 12 + 12 + 12 + 3);
    }

    #[test]
    fn dumps_stop_at_their_limit() {
        let mut dump =
            Dump::new(std::io::Cursor::new(Vec::new()), DumpKind::AnnexB).expect("a dump");
        for n in 0..DUMP_FRAMES + 5 {
            dump.write(&[0, 0, 1, 0x41], u64::from(n)).expect("written");
        }
        assert!(dump.full());
        assert_eq!(dump.frames(), DUMP_FRAMES);
        let bytes = dump.finish().expect("finished").into_inner();
        assert_eq!(bytes.len(), 4 * DUMP_FRAMES as usize);
    }
}
