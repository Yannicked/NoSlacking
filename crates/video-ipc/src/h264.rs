//! Just enough of H.264's Annex B byte stream for both sides of the
//! pipe: NAL units found by their start codes, their types, and frames
//! (access units) as `str0m` hands them over. The app reads them to tell
//! a keyframe from the rest and to check what the helper encoded; the
//! helper, to cut its fixtures into frames. And the sequence parameter
//! set's profile, level and size: the app logs what a huddle sends, and
//! a helper back end whose platform API does not say crops its pictures
//! to it. Nothing here decodes a picture.
//!
//! The SPS reader is written here rather than taken from `h264-reader`:
//! it is one structure of exp-Golomb numbers (ITU-T H.264 §7.3.2.1.1),
//! about a hundred lines, and a crate for it would bring its own
//! dependencies into both sides of the pipe.

/// An Annex B start code, the four-byte form every NAL unit gets here.
pub const START: [u8; 4] = [0, 0, 0, 1];

/// A NAL unit's type: an IDR slice, where decoding can start.
pub const IDR: u8 = 5;
/// A NAL unit's type: a slice of a picture that refers to others.
pub const SLICE: u8 = 1;
/// A NAL unit's type: a sequence parameter set.
pub const SPS: u8 = 7;
/// A NAL unit's type: a picture parameter set.
pub const PPS: u8 = 8;

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

/// A NAL unit's type ([`IDR`], [`SPS`], [`PPS`], …), from its header.
pub fn nal_type(unit: &[u8]) -> Option<u8> {
    unit.first().map(|b| b & 0x1f)
}

/// The NAL unit types of an Annex B frame, in order.
pub fn nal_types(frame: &[u8]) -> Vec<u8> {
    nal_units(frame).into_iter().filter_map(nal_type).collect()
}

/// Whether a frame holds an IDR slice, where decoding can start.
pub fn is_keyframe(frame: &[u8]) -> bool {
    nal_units(frame)
        .into_iter()
        .any(|unit| nal_type(unit) == Some(IDR))
}

/// Whether a NAL unit is a slice of a picture, which ends a frame.
pub fn is_slice(unit: &[u8]) -> bool {
    matches!(nal_type(unit), Some(SLICE | IDR))
}

/// An Annex B stream split into frames as `str0m` hands them over: each
/// ends after its slice, the parameter sets going with the slice they
/// precede, each NAL unit behind a four-byte start code. For streams of
/// one slice a picture, as the fixtures and the demo's are.
pub fn access_units(stream: &[u8]) -> Vec<Vec<u8>> {
    let mut frames = Vec::new();
    let mut frame = Vec::new();
    for unit in nal_units(stream) {
        frame.extend_from_slice(&START);
        frame.extend_from_slice(unit);
        if is_slice(unit) {
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
    if nal_type(unit)? != SPS {
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
        // SPS, PPS (behind a four-byte start code) and an IDR slice.
        let frame = [
            0, 0, 0, 1, 0x67, 0x42, 0xc0, 0x0c, 0, 0, 0, 1, 0x68, 0xce, 0x3c, 0x80, 0, 0, 1, 0x65,
            0x88, 0x80,
        ];
        let units = nal_units(&frame);
        assert_eq!(
            units.iter().filter_map(|u| nal_type(u)).collect::<Vec<_>>(),
            [SPS, PPS, IDR]
        );
        assert_eq!(units[1], &[0x68, 0xce, 0x3c, 0x80][..]);
        assert!(nal_units(&[0, 0]).is_empty());
        assert_eq!(nal_types(&frame), [7, 8, 5]);
        assert_eq!(nal_types(&[0, 0, 1, 0x41, 0x9a]), [1]);
        assert!(nal_types(&[]).is_empty());
    }

    #[test]
    fn a_keyframe_is_a_frame_with_an_idr_slice() {
        let keyframe = [
            0, 0, 0, 1, 0x67, 0x42, 0, 0, 0, 1, 0x68, 0xce, 0, 0, 0, 1, 0x65, 0x88,
        ];
        assert!(is_keyframe(&keyframe));
        assert!(!is_keyframe(&[0, 0, 0, 1, 0x41, 0x9a]));
        assert!(!is_keyframe(&[]));
    }

    #[test]
    fn a_stream_is_cut_after_each_slice() {
        // SPS, PPS, IDR, then a P slice; three-byte start codes and a
        // padding zero, written back with four.
        let stream = [
            0, 0, 1, 0x67, 0x42, 0, 0, 1, 0x68, 0xce, 0, 0, 1, 0x65, 0x88, 0, 0, 0, 1, 0x41, 0x9a,
        ];
        let frames = access_units(&stream);
        assert_eq!(
            frames,
            [
                vec![
                    0, 0, 0, 1, 0x67, 0x42, 0, 0, 0, 1, 0x68, 0xce, 0, 0, 0, 1, 0x65, 0x88
                ],
                vec![0, 0, 0, 1, 0x41, 0x9a],
            ]
        );
        assert!(is_keyframe(&frames[0]) && !is_keyframe(&frames[1]));
        // Parameter sets with no slice after them make no frame.
        assert!(access_units(&[0, 0, 1, 0x67, 0x42]).is_empty());
    }
}
