//! Just enough of the video bitstreams to say what a huddle sends,
//! without decoding a picture: H.264's sequence parameter set (profile,
//! level and size), VP8's keyframe header, and IVF files for dumping VP8.
//!
//! The SPS reader is written here rather than taken from `h264-reader`:
//! it is one structure of exp-Golomb numbers (ITU-T H.264 §7.3.2.1.1),
//! about a hundred lines, and a crate for it would bring its own
//! dependencies into a build that only the probe uses. `str0m` hands over
//! H.264 frames in Annex B (start codes before each NAL unit) and VP8
//! frames with the RTP payload descriptor already gone.

use std::io::{Seek, SeekFrom, Write};

/// The NAL units of an Annex B frame, without their start codes or the
/// zero bytes that may pad before the next one.
pub fn nal_units(frame: &[u8]) -> Vec<&[u8]> {
    let mut starts = Vec::new();
    let mut i = 0;
    while i + 3 <= frame.len() {
        if frame[i] == 0 && frame[i + 1] == 0 && frame[i + 2] == 1 {
            starts.push(i + 3);
            i += 3;
        } else {
            i += 1;
        }
    }
    let mut units = Vec::with_capacity(starts.len());
    for (n, &start) in starts.iter().enumerate() {
        let end = starts.get(n + 1).map_or(frame.len(), |next| next - 3);
        let mut unit = &frame[start..end.max(start)];
        while let [rest @ .., 0] = unit {
            unit = rest;
        }
        if !unit.is_empty() {
            units.push(unit);
        }
    }
    units
}

/// A NAL unit's type: 5 an IDR slice, 7 an SPS, 8 a PPS, …
pub fn nal_type(unit: &[u8]) -> Option<u8> {
    unit.first().map(|b| b & 0x1f)
}

/// An Annex B stream split into frames as `str0m` hands them over: each
/// ends after its slice, the parameter sets going with the slice they
/// precede. For streams of one slice a picture, as the fixtures and the
/// demo's are.
pub fn access_units(stream: &[u8]) -> Vec<Vec<u8>> {
    let mut frames = Vec::new();
    let mut frame = Vec::new();
    for nal in nal_units(stream) {
        frame.extend_from_slice(&[0, 0, 0, 1]);
        frame.extend_from_slice(nal);
        if matches!(nal_type(nal), Some(1 | 5)) {
            frames.push(std::mem::take(&mut frame));
        }
    }
    frames
}

/// What an H.264 sequence parameter set says about the stream.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Sps {
    /// 66 baseline, 77 main, 100 high, …
    pub profile_idc: u8,
    /// The constraint flags byte; 0xC0 on baseline is constrained baseline.
    pub constraints: u8,
    /// Ten times the level: 31 is level 3.1.
    pub level_idc: u8,
    /// The picture's width in pixels, after cropping.
    pub width: u32,
    /// Its height in pixels, after cropping.
    pub height: u32,
}

impl Sps {
    /// The profile by name, as `profile-level-id` would call it.
    pub fn profile(&self) -> &'static str {
        match (self.profile_idc, self.constraints & 0x40 != 0) {
            (66, true) => "constrained baseline",
            (66, false) => "baseline",
            (77, _) => "main",
            (88, _) => "extended",
            (100, _) => "high",
            (110, _) => "high 10",
            (122, _) => "high 4:2:2",
            (244, _) => "high 4:4:4",
            _ => "other",
        }
    }
}

impl std::fmt::Display for Sps {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} (profile_idc {}, constraints {:#04x}) level {}.{} (level_idc {}), {}x{}",
            self.profile(),
            self.profile_idc,
            self.constraints,
            self.level_idc / 10,
            self.level_idc % 10,
            self.level_idc,
            self.width,
            self.height
        )
    }
}

/// Reads bits off a NAL unit's payload, emulation prevention bytes
/// (`00 00 03`) already taken out.
struct Bits {
    bytes: Vec<u8>,
    at: usize,
}

impl Bits {
    fn new(payload: &[u8]) -> Self {
        let mut bytes = Vec::with_capacity(payload.len());
        let mut zeros = 0;
        for &b in payload {
            if zeros >= 2 && b == 3 {
                zeros = 0;
                continue;
            }
            zeros = if b == 0 { zeros + 1 } else { 0 };
            bytes.push(b);
        }
        Self { bytes, at: 0 }
    }

    fn bit(&mut self) -> Option<u32> {
        let byte = self.bytes.get(self.at / 8)?;
        let bit = (byte >> (7 - self.at % 8)) & 1;
        self.at += 1;
        Some(u32::from(bit))
    }

    fn bits(&mut self, n: u32) -> Option<u32> {
        let mut v = 0;
        for _ in 0..n {
            v = (v << 1) | self.bit()?;
        }
        Some(v)
    }

    /// An unsigned exp-Golomb number, `ue(v)`.
    fn ue(&mut self) -> Option<u32> {
        let mut zeros = 0;
        while self.bit()? == 0 {
            zeros += 1;
            if zeros > 31 {
                return None;
            }
        }
        let rest = self.bits(zeros)?;
        ((1u64 << zeros) - 1 + u64::from(rest)).try_into().ok()
    }

    /// A signed one, `se(v)`.
    fn se(&mut self) -> Option<i64> {
        let k = i64::from(self.ue()?);
        Some(if k % 2 == 1 { (k + 1) / 2 } else { -(k / 2) })
    }
}

/// The profiles whose SPS carries chroma format and bit depths.
const HIGH_PROFILES: [u8; 12] = [100, 110, 122, 244, 44, 83, 86, 118, 128, 138, 139, 134];

/// Reads an SPS NAL unit (its header byte first). `None` if it is not
/// one or ends early.
pub fn parse_sps(unit: &[u8]) -> Option<Sps> {
    if nal_type(unit)? != 7 {
        return None;
    }
    let mut r = Bits::new(unit.get(1..)?);
    let profile_idc = u8::try_from(r.bits(8)?).ok()?;
    let constraints = u8::try_from(r.bits(8)?).ok()?;
    let level_idc = u8::try_from(r.bits(8)?).ok()?;
    r.ue()?; // seq_parameter_set_id
    let mut chroma_format_idc = 1;
    let mut separate_colour_plane = false;
    if HIGH_PROFILES.contains(&profile_idc) {
        chroma_format_idc = r.ue()?;
        if chroma_format_idc == 3 {
            separate_colour_plane = r.bit()? == 1;
        }
        r.ue()?; // bit_depth_luma_minus8
        r.ue()?; // bit_depth_chroma_minus8
        r.bit()?; // qpprime_y_zero_transform_bypass_flag
        if r.bit()? == 1 {
            // seq_scaling_matrix_present_flag: the lists are skipped.
            let lists = if chroma_format_idc == 3 { 12 } else { 8 };
            for i in 0..lists {
                if r.bit()? == 1 {
                    skip_scaling_list(&mut r, if i < 6 { 16 } else { 64 })?;
                }
            }
        }
    }
    r.ue()?; // log2_max_frame_num_minus4
    match r.ue()? {
        0 => {
            r.ue()?; // log2_max_pic_order_cnt_lsb_minus4
        }
        1 => {
            r.bit()?; // delta_pic_order_always_zero_flag
            r.se()?; // offset_for_non_ref_pic
            r.se()?; // offset_for_top_to_bottom_field
            for _ in 0..r.ue()? {
                r.se()?;
            }
        }
        _ => {}
    }
    r.ue()?; // max_num_ref_frames
    r.bit()?; // gaps_in_frame_num_value_allowed_flag
    let width_mbs = r.ue()?.checked_add(1)?;
    let height_units = r.ue()?.checked_add(1)?;
    let frame_mbs_only = r.bit()?;
    if frame_mbs_only == 0 {
        r.bit()?; // mb_adaptive_frame_field_flag
    }
    r.bit()?; // direct_8x8_inference_flag
    let (mut left, mut right, mut top, mut bottom) = (0, 0, 0, 0);
    if r.bit()? == 1 {
        left = r.ue()?;
        right = r.ue()?;
        top = r.ue()?;
        bottom = r.ue()?;
    }
    // Cropping is in chroma samples (§7.4.2.1.1).
    let chroma_array_type = if separate_colour_plane {
        0
    } else {
        chroma_format_idc
    };
    let field = 2 - frame_mbs_only;
    let (unit_x, unit_y) = match chroma_array_type {
        0 => (1, field),
        1 => (2, 2 * field),
        2 => (2, field),
        _ => (1, field),
    };
    let width = width_mbs
        .checked_mul(16)?
        .checked_sub(unit_x * (left + right))?;
    let height = height_units
        .checked_mul(16 * field)?
        .checked_sub(unit_y * (top + bottom))?;
    Some(Sps {
        profile_idc,
        constraints,
        level_idc,
        width,
        height,
    })
}

fn skip_scaling_list(r: &mut Bits, size: usize) -> Option<()> {
    let (mut last, mut next) = (8i64, 8i64);
    for _ in 0..size {
        if next != 0 {
            next = (last + r.se()? + 256).rem_euclid(256);
        }
        if next != 0 {
            last = next;
        }
    }
    Some(())
}

/// The first SPS in an Annex B frame.
pub fn sps_of_frame(frame: &[u8]) -> Option<Sps> {
    nal_units(frame).into_iter().find_map(parse_sps)
}

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

    /// SPSs from real encoders, with what ffprobe says of each stream.
    #[test]
    fn sps_from_real_encoders_read_as_ffprobe_reads_them() {
        // OpenH264 (as browsers), constrained baseline, 320x180: 192
        // lines of macroblocks cropped to 180.
        let sps = parse_sps(&hex("6742c00c8c68141979f0101e1108d4")).expect("an SPS");
        assert_eq!(
            sps,
            Sps {
                profile_idc: 66,
                constraints: 0xc0,
                level_idc: 12,
                width: 320,
                height: 180
            }
        );
        assert_eq!(sps.profile(), "constrained baseline");
        // OpenH264, constrained baseline 3.1 at 1280x720 (Chime's 42e01f
        // class of stream).
        let sps = parse_sps(&hex("6742c01f8c6805005bb0101e1108d4")).expect("an SPS");
        assert_eq!(
            (sps.profile_idc, sps.level_idc, sps.width, sps.height),
            (66, 31, 1280, 720)
        );
        // x264 main 3.0, 640x360, with an emulation prevention byte.
        let sps =
            parse_sps(&hex("674d401eeca05017fcb80880000003008000001e078b16cb")).expect("an SPS");
        assert_eq!(
            (sps.profile(), sps.level_idc, sps.width, sps.height),
            ("main", 30, 640, 360)
        );
        // x264 high 4.0, 1920x1080: chroma format and bit depths, and
        // 1088 cropped to 1080.
        let sps = parse_sps(&hex(
            "67640028acd940780227e5c044000003000400000300f03c60c658",
        ))
        .expect("an SPS");
        assert_eq!(
            (sps.profile(), sps.level_idc, sps.width, sps.height),
            ("high", 40, 1920, 1080)
        );
        assert_eq!(
            sps.to_string(),
            "high (profile_idc 100, constraints 0x00) level 4.0 (level_idc 40), 1920x1080"
        );
    }

    #[test]
    fn what_is_not_an_sps_is_none() {
        assert_eq!(parse_sps(&[]), None);
        assert_eq!(parse_sps(&hex("68ce3c80")), None, "a PPS");
        assert_eq!(parse_sps(&hex("6742c0")), None, "cut short");
    }

    #[test]
    fn annex_b_splits_into_nal_units() {
        let frame = hex("000000016742c00c0000000168ce3c80000001658880");
        let units = nal_units(&frame);
        assert_eq!(
            units.iter().filter_map(|u| nal_type(u)).collect::<Vec<_>>(),
            [7, 8, 5]
        );
        assert_eq!(units[1], &hex("68ce3c80")[..]);
        assert!(nal_units(&[0, 0]).is_empty());
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
