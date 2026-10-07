//! The part of libva's encoding interface the H.264 encoder uses: the
//! parameter structures of `va/va_enc_h264.h` and `va/va.h`, copied by
//! hand from libva 2.23 like the decoding ones (sizes and offsets checked
//! against clang's in the tests, padding written out), the coded buffer
//! the driver writes the stream into, and uploading a picture into a
//! surface.

use std::ffi::{c_int, c_uint, c_void};
use std::rc::Rc;

use noslacking_video_ipc::Planes;

use super::{
    Context, Display, FOURCC_NV12, INVALID_ID, Image, ImageFormat, ImageHandle, Nv12Layout,
    Parameter, PictureH264, nv12_layout, private,
};

/// `VAEntrypointEncSlice`: encoding on the GPU's shaders and fixed
/// function (what Mesa's drivers offer).
pub const ENTRYPOINT_ENC_SLICE: i32 = 6;
/// `VAEntrypointEncSliceLP`: the low-power, fixed-function encoder
/// (Intel's VDEnc); preferred where it exists.
pub const ENTRYPOINT_ENC_SLICE_LP: i32 = 8;
/// `VAConfigAttribRateControl`.
pub const ATTRIB_RATE_CONTROL: i32 = 5;
/// `VAConfigAttribEncPackedHeaders`.
pub const ATTRIB_ENC_PACKED_HEADERS: i32 = 10;
/// `VAConfigAttribEncMaxRefFrames`: list 0's references in the low 16
/// bits.
pub const ATTRIB_ENC_MAX_REF_FRAMES: i32 = 13;
/// `VA_RC_CBR`.
pub const RC_CBR: u32 = 0x2;
/// `VA_RC_VBR`.
pub const RC_VBR: u32 = 0x4;
/// `VA_ENC_PACKED_HEADER_SEQUENCE`.
pub const PACKED_HEADER_SEQUENCE: u32 = 0x1;
/// `VA_ENC_PACKED_HEADER_SLICE`.
pub const PACKED_HEADER_SLICE: u32 = 0x4;
/// `VAEncPackedHeaderSequence`: the type of packed parameter sets.
pub const PACKED_HEADER_TYPE_SEQUENCE: u32 = 1;
/// `VAEncPackedHeaderSlice`: the type of a packed slice header.
pub const PACKED_HEADER_TYPE_SLICE: u32 = 3;
/// `VAEncMiscParameterTypeFrameRate`.
const MISC_FRAME_RATE: u32 = 0;
/// `VAEncMiscParameterTypeRateControl`.
const MISC_RATE_CONTROL: u32 = 1;
/// `VAEncMiscParameterTypeHRD`.
const MISC_HRD: u32 = 5;
/// `VAEncCodedBufferType`.
const CODED_BUFFER: c_int = 21;
/// The most segments a coded buffer is read through: one a slice, and
/// we encode one slice a picture; a driver handing back a looping list
/// is stopped here.
const MAX_SEGMENTS: usize = 64;

/// `VAEncSequenceParameterBufferH264`. Bit fields are built with
/// [`enc_seq_fields`] and [`vui_fields`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(C)]
pub struct EncSequenceParameterBufferH264 {
    /// `seq_parameter_set_id`.
    pub seq_parameter_set_id: u8,
    /// `level_idc`.
    pub level_idc: u8,
    /// The C compiler's padding, written out.
    pub pad0: u16,
    /// `intra_period`: pictures from one I to the next.
    pub intra_period: u32,
    /// `intra_idr_period`: pictures from one IDR to the next.
    pub intra_idr_period: u32,
    /// `ip_period`: 1 for no B-frames.
    pub ip_period: u32,
    /// `bits_per_second`.
    pub bits_per_second: u32,
    /// `max_num_ref_frames`.
    pub max_num_ref_frames: u32,
    /// `picture_width_in_mbs`.
    pub picture_width_in_mbs: u16,
    /// `picture_height_in_mbs`.
    pub picture_height_in_mbs: u16,
    /// `seq_fields.value`.
    pub seq_fields: u32,
    /// `bit_depth_luma_minus8`.
    pub bit_depth_luma_minus8: u8,
    /// `bit_depth_chroma_minus8`.
    pub bit_depth_chroma_minus8: u8,
    /// `num_ref_frames_in_pic_order_cnt_cycle`.
    pub num_ref_frames_in_pic_order_cnt_cycle: u8,
    /// The C compiler's padding, written out.
    pub pad1: u8,
    /// `offset_for_non_ref_pic`.
    pub offset_for_non_ref_pic: i32,
    /// `offset_for_top_to_bottom_field`.
    pub offset_for_top_to_bottom_field: i32,
    /// `offset_for_ref_frame`.
    pub offset_for_ref_frame: [i32; 256],
    /// `frame_cropping_flag`.
    pub frame_cropping_flag: u8,
    /// The C compiler's padding, written out.
    pub pad2: [u8; 3],
    /// `frame_crop_left_offset`, in chroma samples.
    pub frame_crop_left_offset: u32,
    /// `frame_crop_right_offset`.
    pub frame_crop_right_offset: u32,
    /// `frame_crop_top_offset`.
    pub frame_crop_top_offset: u32,
    /// `frame_crop_bottom_offset`.
    pub frame_crop_bottom_offset: u32,
    /// `vui_parameters_present_flag`.
    pub vui_parameters_present_flag: u8,
    /// The C compiler's padding, written out.
    pub pad3: [u8; 3],
    /// `vui_fields.value`.
    pub vui_fields: u32,
    /// `aspect_ratio_idc`.
    pub aspect_ratio_idc: u8,
    /// The C compiler's padding, written out.
    pub pad4: [u8; 3],
    /// `sar_width`.
    pub sar_width: u32,
    /// `sar_height`.
    pub sar_height: u32,
    /// `num_units_in_tick`.
    pub num_units_in_tick: u32,
    /// `time_scale`.
    pub time_scale: u32,
    /// `va_reserved`.
    pub reserved: [u32; 4],
}

/// The fields of `VAEncSequenceParameterBufferH264.seq_fields`.
#[derive(Clone, Copy, Debug, Default)]
pub struct EncSeqFields {
    /// `chroma_format_idc` (2 bits).
    pub chroma_format_idc: u32,
    /// `frame_mbs_only_flag`.
    pub frame_mbs_only_flag: bool,
    /// `direct_8x8_inference_flag`.
    pub direct_8x8_inference_flag: bool,
    /// `log2_max_frame_num_minus4` (4 bits).
    pub log2_max_frame_num_minus4: u32,
    /// `pic_order_cnt_type` (2 bits).
    pub pic_order_cnt_type: u32,
    /// `log2_max_pic_order_cnt_lsb_minus4` (4 bits).
    pub log2_max_pic_order_cnt_lsb_minus4: u32,
}

/// `seq_fields.value` from its fields (the ones left out are 0: no
/// field coding, no scaling matrix).
pub fn enc_seq_fields(f: &EncSeqFields) -> u32 {
    (f.chroma_format_idc & 0x3)
        | u32::from(f.frame_mbs_only_flag) << 2
        | u32::from(f.direct_8x8_inference_flag) << 5
        | (f.log2_max_frame_num_minus4 & 0xf) << 6
        | (f.pic_order_cnt_type & 0x3) << 10
        | (f.log2_max_pic_order_cnt_lsb_minus4 & 0xf) << 12
}

/// The fields of `VAEncSequenceParameterBufferH264.vui_fields` used.
#[derive(Clone, Copy, Debug, Default)]
pub struct VuiFields {
    /// `timing_info_present_flag`.
    pub timing_info_present_flag: bool,
    /// `bitstream_restriction_flag`.
    pub bitstream_restriction_flag: bool,
    /// `log2_max_mv_length_horizontal` (5 bits).
    pub log2_max_mv_length_horizontal: u32,
    /// `log2_max_mv_length_vertical` (5 bits).
    pub log2_max_mv_length_vertical: u32,
    /// `fixed_frame_rate_flag`.
    pub fixed_frame_rate_flag: bool,
    /// `motion_vectors_over_pic_boundaries_flag`.
    pub motion_vectors_over_pic_boundaries_flag: bool,
}

/// `vui_fields.value` from its fields.
pub fn vui_fields(f: &VuiFields) -> u32 {
    u32::from(f.timing_info_present_flag) << 1
        | u32::from(f.bitstream_restriction_flag) << 2
        | (f.log2_max_mv_length_horizontal & 0x1f) << 3
        | (f.log2_max_mv_length_vertical & 0x1f) << 8
        | u32::from(f.fixed_frame_rate_flag) << 13
        | u32::from(f.motion_vectors_over_pic_boundaries_flag) << 15
}

/// `VAEncPictureParameterBufferH264`. `pic_fields` is built with
/// [`enc_pic_fields`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(C)]
pub struct EncPictureParameterBufferH264 {
    /// `CurrPic`: the reconstructed picture's surface.
    pub curr_pic: PictureH264,
    /// `ReferenceFrames`.
    pub reference_frames: [PictureH264; 16],
    /// `coded_buf`: where the stream goes.
    pub coded_buf: u32,
    /// `pic_parameter_set_id`.
    pub pic_parameter_set_id: u8,
    /// `seq_parameter_set_id`.
    pub seq_parameter_set_id: u8,
    /// `last_picture`.
    pub last_picture: u8,
    /// The C compiler's padding, written out.
    pub pad0: u8,
    /// `frame_num`.
    pub frame_num: u16,
    /// `pic_init_qp`.
    pub pic_init_qp: u8,
    /// `num_ref_idx_l0_active_minus1`.
    pub num_ref_idx_l0_active_minus1: u8,
    /// `num_ref_idx_l1_active_minus1`.
    pub num_ref_idx_l1_active_minus1: u8,
    /// `chroma_qp_index_offset`.
    pub chroma_qp_index_offset: i8,
    /// `second_chroma_qp_index_offset`.
    pub second_chroma_qp_index_offset: i8,
    /// The C compiler's padding, written out.
    pub pad1: u8,
    /// `pic_fields.value`.
    pub pic_fields: u32,
    /// `va_reserved`.
    pub reserved: [u32; 4],
}

/// The fields of `VAEncPictureParameterBufferH264.pic_fields` used.
#[derive(Clone, Copy, Debug, Default)]
pub struct EncPicFields {
    /// `idr_pic_flag`.
    pub idr_pic_flag: bool,
    /// `reference_pic_flag` (2 bits): 1 for a reference picture.
    pub reference_pic_flag: u32,
    /// `entropy_coding_mode_flag`: CABAC (never, for constrained
    /// baseline).
    pub entropy_coding_mode_flag: bool,
    /// `deblocking_filter_control_present_flag`.
    pub deblocking_filter_control_present_flag: bool,
}

/// `pic_fields.value` from its fields.
pub fn enc_pic_fields(f: &EncPicFields) -> u32 {
    u32::from(f.idr_pic_flag)
        | (f.reference_pic_flag & 0x3) << 1
        | u32::from(f.entropy_coding_mode_flag) << 3
        | u32::from(f.deblocking_filter_control_present_flag) << 9
}

/// `VAEncSliceParameterBufferH264`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(C)]
pub struct EncSliceParameterBufferH264 {
    /// `macroblock_address`: the slice's first macroblock.
    pub macroblock_address: u32,
    /// `num_macroblocks`.
    pub num_macroblocks: u32,
    /// `macroblock_info`: none.
    pub macroblock_info: u32,
    /// `slice_type`: 0 P, 2 I.
    pub slice_type: u8,
    /// `pic_parameter_set_id`.
    pub pic_parameter_set_id: u8,
    /// `idr_pic_id`.
    pub idr_pic_id: u16,
    /// `pic_order_cnt_lsb`.
    pub pic_order_cnt_lsb: u16,
    /// The C compiler's padding, written out.
    pub pad0: u16,
    /// `delta_pic_order_cnt_bottom`.
    pub delta_pic_order_cnt_bottom: i32,
    /// `delta_pic_order_cnt`.
    pub delta_pic_order_cnt: [i32; 2],
    /// `direct_spatial_mv_pred_flag`.
    pub direct_spatial_mv_pred_flag: u8,
    /// `num_ref_idx_active_override_flag`.
    pub num_ref_idx_active_override_flag: u8,
    /// `num_ref_idx_l0_active_minus1`.
    pub num_ref_idx_l0_active_minus1: u8,
    /// `num_ref_idx_l1_active_minus1`.
    pub num_ref_idx_l1_active_minus1: u8,
    /// `RefPicList0`.
    pub ref_pic_list0: [PictureH264; 32],
    /// `RefPicList1`.
    pub ref_pic_list1: [PictureH264; 32],
    /// `luma_log2_weight_denom`.
    pub luma_log2_weight_denom: u8,
    /// `chroma_log2_weight_denom`.
    pub chroma_log2_weight_denom: u8,
    /// `luma_weight_l0_flag`.
    pub luma_weight_l0_flag: u8,
    /// The C compiler's padding, written out.
    pub pad1: u8,
    /// `luma_weight_l0`.
    pub luma_weight_l0: [i16; 32],
    /// `luma_offset_l0`.
    pub luma_offset_l0: [i16; 32],
    /// `chroma_weight_l0_flag`.
    pub chroma_weight_l0_flag: u8,
    /// The C compiler's padding, written out.
    pub pad2: u8,
    /// `chroma_weight_l0`.
    pub chroma_weight_l0: [[i16; 2]; 32],
    /// `chroma_offset_l0`.
    pub chroma_offset_l0: [[i16; 2]; 32],
    /// `luma_weight_l1_flag`.
    pub luma_weight_l1_flag: u8,
    /// The C compiler's padding, written out.
    pub pad3: u8,
    /// `luma_weight_l1`.
    pub luma_weight_l1: [i16; 32],
    /// `luma_offset_l1`.
    pub luma_offset_l1: [i16; 32],
    /// `chroma_weight_l1_flag`.
    pub chroma_weight_l1_flag: u8,
    /// The C compiler's padding, written out.
    pub pad4: u8,
    /// `chroma_weight_l1`.
    pub chroma_weight_l1: [[i16; 2]; 32],
    /// `chroma_offset_l1`.
    pub chroma_offset_l1: [[i16; 2]; 32],
    /// `cabac_init_idc`.
    pub cabac_init_idc: u8,
    /// `slice_qp_delta`.
    pub slice_qp_delta: i8,
    /// `disable_deblocking_filter_idc`.
    pub disable_deblocking_filter_idc: u8,
    /// `slice_alpha_c0_offset_div2`.
    pub slice_alpha_c0_offset_div2: i8,
    /// `slice_beta_offset_div2`.
    pub slice_beta_offset_div2: i8,
    /// The C compiler's padding, written out.
    pub pad5: [u8; 1],
    /// `va_reserved`.
    pub reserved: [u32; 4],
}

impl Default for EncSliceParameterBufferH264 {
    fn default() -> Self {
        Self {
            macroblock_address: 0,
            num_macroblocks: 0,
            macroblock_info: INVALID_ID,
            slice_type: 0,
            pic_parameter_set_id: 0,
            idr_pic_id: 0,
            pic_order_cnt_lsb: 0,
            pad0: 0,
            delta_pic_order_cnt_bottom: 0,
            delta_pic_order_cnt: [0; 2],
            direct_spatial_mv_pred_flag: 0,
            num_ref_idx_active_override_flag: 0,
            num_ref_idx_l0_active_minus1: 0,
            num_ref_idx_l1_active_minus1: 0,
            ref_pic_list0: [PictureH264::invalid(); 32],
            ref_pic_list1: [PictureH264::invalid(); 32],
            luma_log2_weight_denom: 0,
            chroma_log2_weight_denom: 0,
            luma_weight_l0_flag: 0,
            pad1: 0,
            luma_weight_l0: [0; 32],
            luma_offset_l0: [0; 32],
            chroma_weight_l0_flag: 0,
            pad2: 0,
            chroma_weight_l0: [[0; 2]; 32],
            chroma_offset_l0: [[0; 2]; 32],
            luma_weight_l1_flag: 0,
            pad3: 0,
            luma_weight_l1: [0; 32],
            luma_offset_l1: [0; 32],
            chroma_weight_l1_flag: 0,
            pad4: 0,
            chroma_weight_l1: [[0; 2]; 32],
            chroma_offset_l1: [[0; 2]; 32],
            cabac_init_idc: 0,
            slice_qp_delta: 0,
            disable_deblocking_filter_idc: 0,
            slice_alpha_c0_offset_div2: 0,
            slice_beta_offset_div2: 0,
            pad5: [0; 1],
            reserved: [0; 4],
        }
    }
}

/// `VAEncMiscParameterRateControl`. `rc_flags` is built with
/// [`rc_flags`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[repr(C)]
pub struct RateControl {
    /// `bits_per_second`: the most, for VBR; the rate, for CBR.
    pub bits_per_second: u32,
    /// `target_percentage`: VBR's aim, as a percentage of the most.
    pub target_percentage: u32,
    /// `window_size`: the rate is kept over this many milliseconds.
    pub window_size: u32,
    /// `initial_qp`: 0 for the driver's choice.
    pub initial_qp: u32,
    /// `min_qp`.
    pub min_qp: u32,
    /// `basic_unit_size`.
    pub basic_unit_size: u32,
    /// `rc_flags.value`.
    pub rc_flags: u32,
    /// `ICQ_quality_factor`.
    pub icq_quality_factor: u32,
    /// `max_qp`.
    pub max_qp: u32,
    /// `quality_factor`.
    pub quality_factor: u32,
    /// `target_frame_size`.
    pub target_frame_size: u32,
    /// `va_reserved`.
    pub reserved: [u32; 4],
}

/// The fields of `VAEncMiscParameterRateControl.rc_flags` used.
#[derive(Clone, Copy, Debug, Default)]
pub struct RcFlags {
    /// `reset`: start the rate control over (a new rate).
    pub reset: bool,
    /// `disable_frame_skip`: never skip a picture to keep the rate.
    pub disable_frame_skip: bool,
    /// `disable_bit_stuffing`: never pad a picture to fill the rate.
    pub disable_bit_stuffing: bool,
}

/// `rc_flags.value` from its fields.
pub fn rc_flags(f: &RcFlags) -> u32 {
    u32::from(f.reset)
        | u32::from(f.disable_frame_skip) << 1
        | u32::from(f.disable_bit_stuffing) << 2
}

/// `VAEncMiscParameterFrameRate`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[repr(C)]
pub struct FrameRate {
    /// `framerate`: pictures a second (or numerator | denominator << 16).
    pub framerate: u32,
    /// `framerate_flags.value`.
    pub framerate_flags: u32,
    /// `va_reserved`.
    pub reserved: [u32; 4],
}

/// `VAEncMiscParameterHRD`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[repr(C)]
pub struct Hrd {
    /// `initial_buffer_fullness`, in bits.
    pub initial_buffer_fullness: u32,
    /// `buffer_size`, in bits.
    pub buffer_size: u32,
    /// `va_reserved`.
    pub reserved: [u32; 4],
}

/// `VAEncMiscParameterBuffer` with its payload: the type, then the
/// structure (every field a `u32`, so nothing between them).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(C)]
pub struct Misc<T> {
    /// `type`: `VAEncMiscParameterType*`.
    kind: u32,
    /// The payload.
    pub data: T,
}

impl Misc<RateControl> {
    /// A rate control parameter.
    pub fn rate_control(data: RateControl) -> Self {
        Self {
            kind: MISC_RATE_CONTROL,
            data,
        }
    }
}

impl Misc<FrameRate> {
    /// A frame rate parameter.
    pub fn frame_rate(data: FrameRate) -> Self {
        Self {
            kind: MISC_FRAME_RATE,
            data,
        }
    }
}

impl Misc<Hrd> {
    /// A coded picture buffer parameter.
    pub fn hrd(data: Hrd) -> Self {
        Self {
            kind: MISC_HRD,
            data,
        }
    }
}

/// `VAEncPackedHeaderParameterBuffer`: what kind of header the data
/// buffer after it holds, and how many bits.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[repr(C)]
pub struct PackedHeaderParameterBuffer {
    /// `type`: `VAEncPackedHeader*`.
    pub kind: u32,
    /// `bit_length`.
    pub bit_length: u32,
    /// `has_emulation_bytes`: the data has its emulation prevention
    /// bytes already.
    pub has_emulation_bytes: u8,
    /// The C compiler's padding, written out.
    pub pad0: [u8; 3],
    /// `va_reserved`.
    pub reserved: [u32; 4],
}

/// `VACodedBufferSegment`: read from the driver, never written.
#[derive(Clone, Copy, Debug)]
#[repr(C)]
struct CodedBufferSegment {
    size: u32,
    bit_offset: u32,
    status: u32,
    reserved0: u32,
    buf: *const c_void,
    next: *const c_void,
    reserved: [u32; 4],
}

impl private::Sealed for EncSequenceParameterBufferH264 {}
impl private::Sealed for EncPictureParameterBufferH264 {}
impl private::Sealed for EncSliceParameterBufferH264 {}
impl private::Sealed for Misc<RateControl> {}
impl private::Sealed for Misc<FrameRate> {}
impl private::Sealed for Misc<Hrd> {}
impl private::Sealed for PackedHeaderParameterBuffer {}
impl Parameter for EncSequenceParameterBufferH264 {}
impl Parameter for EncPictureParameterBufferH264 {}
impl Parameter for EncSliceParameterBufferH264 {}
impl Parameter for Misc<RateControl> {}
impl Parameter for Misc<FrameRate> {}
impl Parameter for Misc<Hrd> {}
impl Parameter for PackedHeaderParameterBuffer {}

/// The buffer an encoder's output goes into, kept for the encoder's
/// life (destroyed when dropped, before its context).
pub struct CodedBuffer {
    display: Rc<Display>,
    /// Its id, for the picture parameters' `coded_buf`.
    pub id: u32,
    /// Its size in bytes: no picture's stream is longer.
    size: usize,
}

impl Context {
    /// A coded buffer of `size` bytes for this encoding context.
    pub fn coded_buffer(&self, size: u32) -> Result<CodedBuffer, String> {
        let mut id = INVALID_ID;
        // SAFETY: a live context; no data to copy (null), so libva only
        // allocates; the out pointer is to a live local.
        let status = unsafe {
            (self.display.libva.functions.create_buffer)(
                self.display.raw,
                self.id,
                CODED_BUFFER,
                size,
                1,
                std::ptr::null_mut(),
                &mut id,
            )
        };
        self.display.check(status, "vaCreateBuffer (coded)")?;
        Ok(CodedBuffer {
            display: Rc::clone(&self.display),
            id,
            size: usize::try_from(size).map_err(|e| e.to_string())?,
        })
    }
}

impl CodedBuffer {
    /// What the driver wrote for the last picture, its segments joined:
    /// the picture's NAL units, Annex B. Read only after the picture's
    /// source surface was synced (`Context::render` does).
    pub fn read(&self) -> Result<Vec<u8>, String> {
        let functions = &self.display.libva.functions;
        let mut mapped: *mut c_void = std::ptr::null_mut();
        // SAFETY: a coded buffer of this display, its picture finished;
        // mapped for reading and unmapped below.
        let status = unsafe { (functions.map_buffer)(self.display.raw, self.id, &mut mapped) };
        self.display.check(status, "vaMapBuffer (coded)")?;
        let mut out = Vec::new();
        let mut segment = mapped.cast_const();
        let mut trouble = None;
        for _ in 0..MAX_SEGMENTS {
            if segment.is_null() {
                break;
            }
            // SAFETY: libva maps a coded buffer as a list of
            // `VACodedBufferSegment`s, the first at the mapped address
            // and each pointing to the next (null at the end); each is
            // read whole, as the C structure laid out above.
            let header = unsafe { std::ptr::read(segment.cast::<CodedBufferSegment>()) };
            let size = usize::try_from(header.size).unwrap_or(usize::MAX);
            if out.len().saturating_add(size) > self.size || header.bit_offset != 0 {
                trouble = Some(format!(
                    "a coded segment of {size} bytes at bit {} does not fit",
                    header.bit_offset
                ));
                break;
            }
            if size > 0 {
                if header.buf.is_null() {
                    trouble = Some("a coded segment with no data".to_owned());
                    break;
                }
                // SAFETY: the segment says `size` bytes lie at `buf`,
                // inside the mapped buffer (checked above to be no more
                // than the buffer holds), readable until unmapped.
                let bytes = unsafe { std::slice::from_raw_parts(header.buf.cast::<u8>(), size) };
                out.extend_from_slice(bytes);
            }
            segment = header.next;
        }
        // SAFETY: mapped above; nothing read from it after this.
        let status = unsafe { (functions.unmap_buffer)(self.display.raw, self.id) };
        self.display.check(status, "vaUnmapBuffer (coded)")?;
        match trouble {
            Some(why) => Err(why),
            None => Ok(out),
        }
    }
}

impl Drop for CodedBuffer {
    fn drop(&mut self) {
        // SAFETY: a buffer made on this display, not yet destroyed.
        unsafe { (self.display.libva.functions.destroy_buffer)(self.display.raw, self.id) };
    }
}

impl Display {
    /// Writes `picture` into the top left of `surface` (`surface_size`
    /// large, at least the picture's size) as NV12, repeating its last
    /// column and row into the rest so the encoder's padding costs no
    /// bits.
    pub fn write_i420(
        &self,
        surface: u32,
        surface_size: (u32, u32),
        picture: &Planes,
    ) -> Result<(), String> {
        picture.check().map_err(|e| e.to_string())?;
        if picture.width > surface_size.0 || picture.height > surface_size.1 {
            return Err("the picture is larger than the surface".into());
        }
        if !self.derive_failed.get() {
            match self.derived_image(surface) {
                Ok(image) => return self.fill_image(&image, surface_size, picture),
                // Not every driver derives (or derives NV12): put from now
                // on.
                Err(_) => self.derive_failed.set(true),
            }
        }
        let image = self.new_image(surface_size)?;
        self.fill_image(&image, surface_size, picture)?;
        let (width, height): (c_uint, c_uint) = surface_size;
        // SAFETY: an image and a surface of this display, both
        // `surface_size` large.
        let status = unsafe {
            (self.libva.functions.put_image)(
                self.raw,
                surface,
                image.image.image_id,
                0,
                0,
                width,
                height,
                0,
                0,
                width,
                height,
            )
        };
        self.check(status, "vaPutImage")
    }

    /// An NV12 image of `size`.
    fn new_image(&self, size: (u32, u32)) -> Result<ImageHandle<'_>, String> {
        let mut format = ImageFormat {
            fourcc: FOURCC_NV12,
            bits_per_pixel: 12,
            ..ImageFormat::default()
        };
        let int = |n: u32| c_int::try_from(n).map_err(|e| e.to_string());
        let mut image = Image::default();
        // SAFETY: an NV12 format description and an out pointer to live
        // locals.
        let status = unsafe {
            (self.libva.functions.create_image)(
                self.raw,
                &mut format,
                int(size.0)?,
                int(size.1)?,
                &mut image,
            )
        };
        self.check(status, "vaCreateImage")?;
        let handle = ImageHandle {
            display: self,
            image,
        };
        if handle.image.format.fourcc != FOURCC_NV12 || handle.image.num_planes != 2 {
            return Err("the image is not NV12".into());
        }
        Ok(handle)
    }

    /// Maps `handle`'s buffer and writes `picture` into it, padded to
    /// `size`.
    fn fill_image(
        &self,
        handle: &ImageHandle<'_>,
        size: (u32, u32),
        picture: &Planes,
    ) -> Result<(), String> {
        let image = &handle.image;
        let layout = nv12_layout(
            (u32::from(image.width), u32::from(image.height)),
            [image.pitches[0], image.pitches[1]],
            [image.offsets[0], image.offsets[1]],
            image.data_size,
            (0, 0, size.0, size.1),
        )?;
        let mut mapped: *mut c_void = std::ptr::null_mut();
        // SAFETY: the image's buffer, mapped for writing; unmapped below
        // before the image is destroyed.
        let status = unsafe { (self.libva.functions.map_buffer)(self.raw, image.buf, &mut mapped) };
        self.check(status, "vaMapBuffer")?;
        if mapped.is_null() {
            return Err("vaMapBuffer gave nothing".into());
        }
        let length = usize::try_from(image.data_size).map_err(|e| e.to_string())?;
        // SAFETY: libva mapped `data_size` bytes at `mapped`, writable
        // and ours alone until vaUnmapBuffer, after the last use of
        // `data`.
        let data = unsafe { std::slice::from_raw_parts_mut(mapped.cast::<u8>(), length) };
        fill_nv12(data, &layout, size, picture);
        // SAFETY: mapped above; `data` is not used past here.
        let status = unsafe { (self.libva.functions.unmap_buffer)(self.raw, image.buf) };
        self.check(status, "vaUnmapBuffer")
    }
}

/// Writes I420 `picture` into NV12 `data` (laid out as `layout` says,
/// already checked to hold `size`), its last column and row repeated out
/// to `size`. Rows are built whole and written once each: mapped video
/// memory is often write-combined, where one long write beats many short
/// ones.
pub fn fill_nv12(data: &mut [u8], layout: &Nv12Layout, size: (u32, u32), picture: &Planes) {
    let n = |v: u32| usize::try_from(v).unwrap_or(0);
    let (width, height) = (n(picture.width), n(picture.height));
    let (padded_width, padded_height) = (n(size.0), n(size.1));
    let mut line = vec![0u8; padded_width];
    for row in 0..padded_height {
        let from = row.min(height - 1) * width;
        line[..width].copy_from_slice(&picture.y[from..from + width]);
        let last = line[width - 1];
        line[width..].fill(last);
        let start = layout.luma_offset + row * layout.luma_pitch;
        data[start..start + padded_width].copy_from_slice(&line);
    }
    let (cw, ch) = (width.div_ceil(2), height.div_ceil(2));
    let padded_cw = padded_width.div_ceil(2);
    let mut line = vec![0u8; padded_cw * 2];
    for row in 0..padded_height.div_ceil(2) {
        let from = row.min(ch - 1) * cw;
        let (u, v) = (&picture.u[from..from + cw], &picture.v[from..from + cw]);
        for (i, pair) in line.as_chunks_mut::<2>().0.iter_mut().enumerate() {
            let at = i.min(cw - 1);
            *pair = [u[at], v[at]];
        }
        let start = layout.chroma_offset + row * layout.chroma_pitch;
        data[start..start + padded_cw * 2].copy_from_slice(&line);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::mem::{offset_of, size_of};

    /// Sizes and offsets as clang computed them from libva 2.23's
    /// va_enc_h264.h and va.h on x86_64.
    #[test]
    fn encoding_structures_match_libva() {
        type S = EncSequenceParameterBufferH264;
        assert_eq!(size_of::<S>(), 1132);
        assert_eq!(offset_of!(S, level_idc), 1);
        assert_eq!(offset_of!(S, intra_period), 4);
        assert_eq!(offset_of!(S, intra_idr_period), 8);
        assert_eq!(offset_of!(S, ip_period), 12);
        assert_eq!(offset_of!(S, bits_per_second), 16);
        assert_eq!(offset_of!(S, max_num_ref_frames), 20);
        assert_eq!(offset_of!(S, picture_width_in_mbs), 24);
        assert_eq!(offset_of!(S, picture_height_in_mbs), 26);
        assert_eq!(offset_of!(S, seq_fields), 28);
        assert_eq!(offset_of!(S, bit_depth_luma_minus8), 32);
        assert_eq!(offset_of!(S, bit_depth_chroma_minus8), 33);
        assert_eq!(offset_of!(S, num_ref_frames_in_pic_order_cnt_cycle), 34);
        assert_eq!(offset_of!(S, offset_for_non_ref_pic), 36);
        assert_eq!(offset_of!(S, offset_for_top_to_bottom_field), 40);
        assert_eq!(offset_of!(S, offset_for_ref_frame), 44);
        assert_eq!(offset_of!(S, frame_cropping_flag), 1068);
        assert_eq!(offset_of!(S, frame_crop_left_offset), 1072);
        assert_eq!(offset_of!(S, frame_crop_bottom_offset), 1084);
        assert_eq!(offset_of!(S, vui_parameters_present_flag), 1088);
        assert_eq!(offset_of!(S, vui_fields), 1092);
        assert_eq!(offset_of!(S, aspect_ratio_idc), 1096);
        assert_eq!(offset_of!(S, sar_width), 1100);
        assert_eq!(offset_of!(S, sar_height), 1104);
        assert_eq!(offset_of!(S, num_units_in_tick), 1108);
        assert_eq!(offset_of!(S, time_scale), 1112);
        assert_eq!(offset_of!(S, reserved), 1116);

        type P = EncPictureParameterBufferH264;
        assert_eq!(size_of::<P>(), 648);
        assert_eq!(offset_of!(P, reference_frames), 36);
        assert_eq!(offset_of!(P, coded_buf), 612);
        assert_eq!(offset_of!(P, pic_parameter_set_id), 616);
        assert_eq!(offset_of!(P, seq_parameter_set_id), 617);
        assert_eq!(offset_of!(P, last_picture), 618);
        assert_eq!(offset_of!(P, frame_num), 620);
        assert_eq!(offset_of!(P, pic_init_qp), 622);
        assert_eq!(offset_of!(P, num_ref_idx_l0_active_minus1), 623);
        assert_eq!(offset_of!(P, num_ref_idx_l1_active_minus1), 624);
        assert_eq!(offset_of!(P, chroma_qp_index_offset), 625);
        assert_eq!(offset_of!(P, second_chroma_qp_index_offset), 626);
        assert_eq!(offset_of!(P, pic_fields), 628);
        assert_eq!(offset_of!(P, reserved), 632);

        type L = EncSliceParameterBufferH264;
        assert_eq!(size_of::<L>(), 3140);
        assert_eq!(offset_of!(L, num_macroblocks), 4);
        assert_eq!(offset_of!(L, macroblock_info), 8);
        assert_eq!(offset_of!(L, slice_type), 12);
        assert_eq!(offset_of!(L, pic_parameter_set_id), 13);
        assert_eq!(offset_of!(L, idr_pic_id), 14);
        assert_eq!(offset_of!(L, pic_order_cnt_lsb), 16);
        assert_eq!(offset_of!(L, delta_pic_order_cnt_bottom), 20);
        assert_eq!(offset_of!(L, delta_pic_order_cnt), 24);
        assert_eq!(offset_of!(L, direct_spatial_mv_pred_flag), 32);
        assert_eq!(offset_of!(L, num_ref_idx_active_override_flag), 33);
        assert_eq!(offset_of!(L, num_ref_idx_l0_active_minus1), 34);
        assert_eq!(offset_of!(L, num_ref_idx_l1_active_minus1), 35);
        assert_eq!(offset_of!(L, ref_pic_list0), 36);
        assert_eq!(offset_of!(L, ref_pic_list1), 1188);
        assert_eq!(offset_of!(L, luma_log2_weight_denom), 2340);
        assert_eq!(offset_of!(L, chroma_log2_weight_denom), 2341);
        assert_eq!(offset_of!(L, luma_weight_l0_flag), 2342);
        assert_eq!(offset_of!(L, luma_weight_l0), 2344);
        assert_eq!(offset_of!(L, luma_offset_l0), 2408);
        assert_eq!(offset_of!(L, chroma_weight_l0_flag), 2472);
        assert_eq!(offset_of!(L, chroma_weight_l0), 2474);
        assert_eq!(offset_of!(L, chroma_offset_l0), 2602);
        assert_eq!(offset_of!(L, luma_weight_l1_flag), 2730);
        assert_eq!(offset_of!(L, luma_weight_l1), 2732);
        assert_eq!(offset_of!(L, luma_offset_l1), 2796);
        assert_eq!(offset_of!(L, chroma_weight_l1_flag), 2860);
        assert_eq!(offset_of!(L, chroma_weight_l1), 2862);
        assert_eq!(offset_of!(L, chroma_offset_l1), 2990);
        assert_eq!(offset_of!(L, cabac_init_idc), 3118);
        assert_eq!(offset_of!(L, slice_qp_delta), 3119);
        assert_eq!(offset_of!(L, disable_deblocking_filter_idc), 3120);
        assert_eq!(offset_of!(L, slice_alpha_c0_offset_div2), 3121);
        assert_eq!(offset_of!(L, slice_beta_offset_div2), 3122);
        assert_eq!(offset_of!(L, reserved), 3124);

        // VAEncMiscParameterBuffer is a 4-byte type and its payload.
        assert_eq!(size_of::<RateControl>(), 60);
        assert_eq!(offset_of!(RateControl, rc_flags), 24);
        assert_eq!(offset_of!(RateControl, icq_quality_factor), 28);
        assert_eq!(offset_of!(RateControl, max_qp), 32);
        assert_eq!(offset_of!(RateControl, quality_factor), 36);
        assert_eq!(offset_of!(RateControl, target_frame_size), 40);
        assert_eq!(offset_of!(RateControl, reserved), 44);
        assert_eq!(size_of::<Misc<RateControl>>(), 4 + 60);
        assert_eq!(offset_of!(Misc<RateControl>, data), 4);
        assert_eq!(size_of::<FrameRate>(), 24);
        assert_eq!(size_of::<Misc<FrameRate>>(), 4 + 24);
        assert_eq!(size_of::<Hrd>(), 24);
        assert_eq!(size_of::<Misc<Hrd>>(), 4 + 24);

        type H = PackedHeaderParameterBuffer;
        assert_eq!(size_of::<H>(), 28);
        assert_eq!(offset_of!(H, bit_length), 4);
        assert_eq!(offset_of!(H, has_emulation_bytes), 8);
        assert_eq!(offset_of!(H, reserved), 12);

        type C = CodedBufferSegment;
        assert_eq!(size_of::<C>(), 48);
        assert_eq!(offset_of!(C, bit_offset), 4);
        assert_eq!(offset_of!(C, status), 8);
        assert_eq!(offset_of!(C, buf), 16);
        assert_eq!(offset_of!(C, next), 24);
        assert_eq!(offset_of!(C, reserved), 32);
    }

    /// The bit fields, against what clang made of the same assignments
    /// to libva's C bit fields.
    #[test]
    fn encoding_bit_fields_pack_as_c_does() {
        let seq = EncSeqFields {
            chroma_format_idc: 1,
            frame_mbs_only_flag: true,
            direct_8x8_inference_flag: true,
            log2_max_frame_num_minus4: 0xf,
            pic_order_cnt_type: 2,
            log2_max_pic_order_cnt_lsb_minus4: 3,
        };
        // And delta_pic_order_always_zero_flag (bit 16) in C's 0x13be5,
        // which the encoder never sets.
        assert_eq!(enc_seq_fields(&seq) | 1 << 16, 0x13be5);
        let vui = VuiFields {
            timing_info_present_flag: true,
            bitstream_restriction_flag: true,
            log2_max_mv_length_horizontal: 15,
            log2_max_mv_length_vertical: 15,
            fixed_frame_rate_flag: true,
            motion_vectors_over_pic_boundaries_flag: true,
        };
        assert_eq!(vui_fields(&vui), 0xaf7e);
        let pic = EncPicFields {
            idr_pic_flag: true,
            reference_pic_flag: 1,
            entropy_coding_mode_flag: false,
            deblocking_filter_control_present_flag: true,
        };
        // C's 0x243 also has weighted_bipred_idc 2 (bits 5–6).
        assert_eq!(enc_pic_fields(&pic) | 2 << 5, 0x243);
        let rc = RcFlags {
            reset: true,
            disable_frame_skip: false,
            disable_bit_stuffing: true,
        };
        // C's 0x40185 also has temporal_id 3 and frame_tolerance_mode 1.
        assert_eq!(rc_flags(&rc) | 3 << 7 | 1 << 18, 0x40185);
    }

    #[test]
    fn pictures_are_written_as_nv12_with_their_edges_repeated() {
        // A 3×3 picture into a 4×4 surface, pitch 6.
        let picture = Planes {
            width: 3,
            height: 3,
            y: vec![1, 2, 3, 4, 5, 6, 7, 8, 9],
            u: vec![10, 11, 12, 13],
            v: vec![20, 21, 22, 23],
        };
        let mut data = vec![0xee; 6 * 4 + 6 * 2];
        let layout = nv12_layout((4, 4), [6, 6], [0, 24], 36, (0, 0, 4, 4)).expect("fits");
        fill_nv12(&mut data, &layout, (4, 4), &picture);
        assert_eq!(
            data,
            [
                1, 2, 3, 3, 0xee, 0xee, //
                4, 5, 6, 6, 0xee, 0xee, //
                7, 8, 9, 9, 0xee, 0xee, //
                7, 8, 9, 9, 0xee, 0xee, //
                10, 20, 11, 21, 0xee, 0xee, //
                12, 22, 13, 23, 0xee, 0xee,
            ]
        );
    }
}
