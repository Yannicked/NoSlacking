//! H.264 headers written out, for an encoder whose driver wants them
//! packed (Mesa's does: it writes the stream's headers from what it
//! parses out of ours) or writes none: the SPS and PPS of a constrained
//! baseline stream as WebRTC sends it (`42e0xx`: profile 66 with
//! constraint_set0, 1 and 2; CAVLC; one reference; picture order type 2,
//! so no picture order in the slice header), each picture's slice
//! header, and a reader of the NAL unit types in an Annex B access unit.

/// What the SPS says.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Sps {
    /// `level_idc`: 31 for 3.1 and so on.
    pub level_idc: u8,
    /// The coded width, in macroblocks.
    pub width_in_mbs: u32,
    /// The coded height, in macroblocks.
    pub height_in_mbs: u32,
    /// `log2_max_frame_num_minus4`.
    pub log2_max_frame_num_minus4: u32,
    /// Columns cropped off the right, in chroma samples (2 pixels).
    pub crop_right: u32,
    /// Rows cropped off the bottom, in chroma samples (2 pixels).
    pub crop_bottom: u32,
    /// Pictures a second, for the timing information.
    pub fps: u32,
}

/// What a slice header says: one slice a picture, every picture a
/// reference.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Slice {
    /// An IDR (an I slice), else a P slice.
    pub idr: bool,
    /// `frame_num`.
    pub frame_num: u32,
    /// The SPS's `log2_max_frame_num_minus4`: `frame_num`'s width.
    pub log2_max_frame_num_minus4: u32,
    /// `idr_pic_id`, for an IDR.
    pub idr_pic_id: u32,
}

/// Writes bits, most significant first.
#[derive(Debug, Default)]
struct Bits {
    bytes: Vec<u8>,
    /// Bits used in the last byte (0 when it is full or there is none).
    used: u32,
}

impl Bits {
    fn bit(&mut self, bit: bool) {
        if self.used == 0 {
            self.bytes.push(0);
        }
        if bit && let Some(last) = self.bytes.last_mut() {
            *last |= 0x80 >> self.used;
        }
        self.used = (self.used + 1) % 8;
    }

    fn bits(&mut self, value: u32, count: u32) {
        for i in (0..count).rev() {
            self.bit(value >> i & 1 == 1);
        }
    }

    /// `ue(v)`: unsigned Exp-Golomb.
    fn ue(&mut self, value: u32) {
        let coded = u64::from(value) + 1;
        let length = 64 - coded.leading_zeros();
        for _ in 1..length {
            self.bit(false);
        }
        for i in (0..length).rev() {
            self.bit(coded >> i & 1 == 1);
        }
    }

    /// `se(v)`: signed Exp-Golomb.
    fn se(&mut self, value: i32) {
        let mapped = if value > 0 {
            value.unsigned_abs() * 2 - 1
        } else {
            value.unsigned_abs() * 2
        };
        self.ue(mapped);
    }

    /// The NAL unit with its start code and emulation prevention, and
    /// its length in bits (the last byte's unused bits not counted).
    fn unit(self, header: u8) -> (Vec<u8>, u32) {
        let unused = (8 - self.used) % 8;
        let mut out = vec![0, 0, 0, 1, header];
        let mut zeros = 0;
        for byte in self.bytes {
            if zeros >= 2 && byte <= 3 {
                out.push(3);
                zeros = 0;
            }
            out.push(byte);
            zeros = if byte == 0 { zeros + 1 } else { 0 };
        }
        let length = u32::try_from(out.len() * 8).unwrap_or(u32::MAX) - unused;
        (out, length)
    }

    /// `rbsp_trailing_bits()`, then the whole NAL unit.
    fn nal(mut self, header: u8) -> Vec<u8> {
        self.bit(true);
        while self.used != 0 {
            self.bit(false);
        }
        self.unit(header).0
    }
}

/// The SPS, Annex B with its start code.
pub fn sps(sps: &Sps) -> Vec<u8> {
    let mut b = Bits::default();
    b.bits(66, 8); // profile_idc: baseline
    // constraint_set0, 1 and 2: constrained baseline, as WebRTC's 42e0.
    b.bits(0xe0, 8);
    b.bits(u32::from(sps.level_idc), 8);
    b.ue(0); // seq_parameter_set_id
    b.ue(sps.log2_max_frame_num_minus4);
    b.ue(2); // pic_order_cnt_type: from frame_num, every picture a reference
    b.ue(1); // max_num_ref_frames
    b.bit(false); // gaps_in_frame_num_value_allowed_flag
    b.ue(sps.width_in_mbs.saturating_sub(1));
    b.ue(sps.height_in_mbs.saturating_sub(1));
    b.bit(true); // frame_mbs_only_flag
    b.bit(true); // direct_8x8_inference_flag
    let cropped = sps.crop_right != 0 || sps.crop_bottom != 0;
    b.bit(cropped);
    if cropped {
        b.ue(0);
        b.ue(sps.crop_right);
        b.ue(0);
        b.ue(sps.crop_bottom);
    }
    b.bit(true); // vui_parameters_present_flag
    b.bit(false); // aspect_ratio_info_present_flag
    b.bit(false); // overscan_info_present_flag
    b.bit(false); // video_signal_type_present_flag
    b.bit(false); // chroma_loc_info_present_flag
    b.bit(true); // timing_info_present_flag
    b.bits(1, 32); // num_units_in_tick
    b.bits(sps.fps.max(1) * 2, 32); // time_scale: two ticks a picture
    b.bit(false); // fixed_frame_rate_flag
    b.bit(false); // nal_hrd_parameters_present_flag
    b.bit(false); // vcl_hrd_parameters_present_flag
    b.bit(false); // pic_struct_present_flag
    b.bit(true); // bitstream_restriction_flag
    b.bit(true); // motion_vectors_over_pic_boundaries_flag
    b.ue(2); // max_bytes_per_pic_denom
    b.ue(1); // max_bits_per_mb_denom
    b.ue(16); // log2_max_mv_length_horizontal
    b.ue(16); // log2_max_mv_length_vertical
    // No reordering and one picture held: a decoder shows each at once.
    b.ue(0); // max_num_reorder_frames
    b.ue(1); // max_dec_frame_buffering
    b.nal(0x67)
}

/// The PPS, Annex B with its start code: CAVLC, one reference, QP 26,
/// the default deblocking.
pub fn pps() -> Vec<u8> {
    let mut b = Bits::default();
    b.ue(0); // pic_parameter_set_id
    b.ue(0); // seq_parameter_set_id
    b.bit(false); // entropy_coding_mode_flag: CAVLC
    b.bit(false); // bottom_field_pic_order_in_frame_present_flag
    b.ue(0); // num_slice_groups_minus1
    b.ue(0); // num_ref_idx_l0_default_active_minus1
    b.ue(0); // num_ref_idx_l1_default_active_minus1
    b.bit(false); // weighted_pred_flag
    b.bits(0, 2); // weighted_bipred_idc
    b.se(0); // pic_init_qp_minus26
    b.se(0); // pic_init_qs_minus26
    b.se(0); // chroma_qp_index_offset
    b.bit(false); // deblocking_filter_control_present_flag
    b.bit(false); // constrained_intra_pred_flag
    b.bit(false); // redundant_pic_cnt_present_flag
    b.nal(0x68)
}

/// A slice's NAL header and slice header, Annex B with its start code,
/// and its length in bits: it ends mid-byte, where the slice data the
/// driver writes begins. `slice_qp_delta` is 0: the driver's rate
/// control sets the QP.
pub fn slice_header(slice: &Slice) -> (Vec<u8>, u32) {
    let mut b = Bits::default();
    b.ue(0); // first_mb_in_slice
    b.ue(if slice.idr { 2 } else { 0 }); // slice_type: I or P
    b.ue(0); // pic_parameter_set_id
    let width = slice.log2_max_frame_num_minus4 + 4;
    b.bits(slice.frame_num & ((1 << width) - 1), width);
    if slice.idr {
        b.ue(slice.idr_pic_id);
    } else {
        b.bit(false); // num_ref_idx_active_override_flag
        b.bit(false); // ref_pic_list_modification_flag_l0
    }
    // dec_ref_pic_marking()
    if slice.idr {
        b.bit(false); // no_output_of_prior_pics_flag
        b.bit(false); // long_term_reference_flag
    } else {
        b.bit(false); // adaptive_ref_pic_marking_mode_flag: sliding window
    }
    b.se(0); // slice_qp_delta
    // nal_ref_idc 3: every picture is a reference.
    b.unit(if slice.idr { 0x65 } else { 0x61 })
}

/// Whether an access unit holds an IDR slice, where decoding can start.
pub fn is_keyframe(unit: &[u8]) -> bool {
    types(unit).contains(&5)
}

/// An Annex B stream cut into access units the way the app hands them
/// over (as `str0m` does): each ends with its slice, parameter sets
/// going with the slice after; each NAL unit behind a 4-byte start
/// code. For tests and the benchmarks.
pub fn access_units(stream: &[u8]) -> Vec<Vec<u8>> {
    let mut starts: Vec<usize> = stream
        .windows(3)
        .enumerate()
        .filter(|(_, w)| *w == [0, 0, 1])
        .map(|(i, _)| i + 3)
        .collect();
    starts.push(stream.len() + 3);
    let mut units = Vec::new();
    let mut unit = Vec::new();
    for pair in starts.windows(2) {
        // The next start code's leading zeros belong to it.
        let mut end = pair[1] - 3;
        while end > pair[0] && stream[end - 1] == 0 {
            end -= 1;
        }
        let nal = &stream[pair[0]..end];
        unit.extend_from_slice(&[0, 0, 0, 1]);
        unit.extend_from_slice(nal);
        if matches!(nal.first().map(|b| b & 0x1f), Some(1 | 5)) {
            units.push(std::mem::take(&mut unit));
        }
    }
    units
}

/// The NAL unit types in an Annex B access unit, in order.
pub fn types(unit: &[u8]) -> Vec<u8> {
    let mut types = Vec::new();
    let mut zeros = 0;
    for (i, &byte) in unit.iter().enumerate() {
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
    }
    types
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exp_golomb_codes() {
        let mut b = Bits::default();
        for value in [0, 1, 2, 3, 7] {
            b.ue(value);
        }
        b.se(-1);
        b.se(1);
        let bits: String = b.bytes.iter().map(|byte| format!("{byte:08b}")).collect();
        // 1 010 011 00100 0001000, then 011 and 010.
        assert!(bits.starts_with("1010011001000001000011010"), "{bits}");
    }

    #[test]
    fn start_codes_inside_are_escaped() {
        let mut b = Bits::default();
        b.bits(0, 16);
        b.bits(1, 8);
        b.bits(0, 16);
        b.bits(0, 8);
        let nal = b.nal(0x67);
        assert_eq!(nal, [0, 0, 0, 1, 0x67, 0, 0, 3, 1, 0, 0, 3, 0, 0x80]);
        assert_eq!(types(&nal), [7]);
    }

    #[test]
    fn slice_headers_end_mid_byte() {
        let (idr, bits) = slice_header(&Slice {
            idr: true,
            frame_num: 0,
            log2_max_frame_num_minus4: 4,
            idr_pic_id: 0,
        });
        // Start code and NAL header, then 1 011 1 00000000 1 0 0 1.
        assert_eq!(bits, 40 + 1 + 3 + 1 + 8 + 1 + 2 + 1);
        assert_eq!(
            idr,
            [0, 0, 0, 1, 0x65, 0b1011_1000, 0b0000_0100, 0b1000_0000]
        );
        let (p, bits) = slice_header(&Slice {
            idr: false,
            frame_num: 300,
            log2_max_frame_num_minus4: 4,
            idr_pic_id: 0,
        });
        // 1 1 1 00101100 0 0 0 1: frame_num wraps at 256.
        assert_eq!(bits, 40 + 3 + 8 + 4);
        assert_eq!(p, [0, 0, 0, 1, 0x61, 0b1110_0101, 0b1000_0010]);
    }

    /// The parameter sets and a slice header parse as the GPU decoder's
    /// parser reads them.
    #[test]
    fn headers_parse() {
        let sps_nal = sps(&Sps {
            level_idc: 40,
            width_in_mbs: 120,
            height_in_mbs: 68,
            log2_max_frame_num_minus4: 4,
            crop_right: 0,
            crop_bottom: 4,
            fps: 15,
        });
        assert_eq!(&sps_nal[4..8], [0x67, 66, 0xe0, 40]);
        let pps_nal = pps();
        assert_eq!(types(&[sps_nal.clone(), pps_nal.clone()].concat()), [7, 8]);
        #[cfg(target_os = "linux")]
        {
            use cros_codecs::codec::h264::parser::{Nalu, Parser};
            let mut parser = Parser::default();
            let mut cursor = std::io::Cursor::new(&sps_nal[..]);
            let nalu = Nalu::next(&mut cursor).expect("a NAL unit");
            let parsed = parser.parse_sps(&nalu).expect("an SPS").clone();
            assert_eq!(parsed.profile_idc, 66);
            assert!(parsed.constraint_set1_flag);
            assert_eq!(parsed.pic_order_cnt_type, 2);
            assert_eq!(
                (
                    parsed.pic_width_in_mbs_minus1,
                    parsed.pic_height_in_map_units_minus1
                ),
                (119, 67)
            );
            assert_eq!(parsed.visible_rectangle().max.y, 1080);
            assert_eq!(parsed.max_num_ref_frames, 1);
            assert_eq!(parsed.vui_parameters.max_dec_frame_buffering, 1);
            let mut cursor = std::io::Cursor::new(&pps_nal[..]);
            let nalu = Nalu::next(&mut cursor).expect("a NAL unit");
            let pps = parser.parse_pps(&nalu).expect("a PPS");
            assert!(!pps.entropy_coding_mode_flag);
            assert!(!pps.deblocking_filter_control_present_flag);
            // A P slice header, with a byte of slice data after it.
            let (mut slice, bits) = slice_header(&Slice {
                idr: false,
                frame_num: 7,
                log2_max_frame_num_minus4: 4,
                idr_pic_id: 0,
            });
            assert_eq!(bits % 8, 7);
            slice.push(0xff);
            let mut cursor = std::io::Cursor::new(&slice[..]);
            let nalu = Nalu::next(&mut cursor).expect("a NAL unit");
            let header = parser.parse_slice_header(nalu).expect("a slice").header;
            assert_eq!(header.frame_num, 7);
            assert!(header.slice_type.is_p());
            assert_eq!(header.slice_qp_delta, 0);
        }
    }
}
