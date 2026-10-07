//! The part of libva the decoder uses, behind a safe interface. This is
//! the only module of the helper with `unsafe` code.
//!
//! libva is opened at run time (`libva.so.2` and `libva-drm.so.2`, the
//! sonames of libva 2.x, the API since 2017), never linked, so the
//! helper builds without libva's headers and runs on systems without it.
//! The declarations below are copied from libva 2.23's `va/va.h` by
//! hand; only what is used is declared, and every structure's size and
//! field offsets are checked against what a C compiler made of the
//! header (the tests at the bottom). The structures passed into libva
//! have their padding written out as fields, so their bytes are all
//! initialized.

use std::ffi::{CStr, c_char, c_int, c_uint, c_void};
use std::fs::File;
use std::os::fd::AsRawFd;
use std::rc::Rc;

use libloading::Library;

/// `VAStatus`: 0 is success.
type Status = c_int;
/// `VADisplay`.
type RawDisplay = *mut c_void;

/// `VAProfileH264Main`.
pub const PROFILE_H264_MAIN: i32 = 6;
/// `VAProfileH264High`.
pub const PROFILE_H264_HIGH: i32 = 7;
/// `VAProfileH264ConstrainedBaseline`.
pub const PROFILE_H264_CONSTRAINED_BASELINE: i32 = 13;
/// `VAEntrypointVLD`: decoding.
pub const ENTRYPOINT_VLD: i32 = 1;
/// `VAConfigAttribRTFormat`.
const ATTRIB_RT_FORMAT: i32 = 0;
/// `VAConfigAttribMaxPictureWidth`.
pub const ATTRIB_MAX_PICTURE_WIDTH: i32 = 18;
/// `VAConfigAttribMaxPictureHeight`.
pub const ATTRIB_MAX_PICTURE_HEIGHT: i32 = 19;
/// `VA_ATTRIB_NOT_SUPPORTED`.
pub const ATTRIB_NOT_SUPPORTED: u32 = 0x8000_0000;
/// `VA_RT_FORMAT_YUV420`.
pub const RT_FORMAT_YUV420: u32 = 1;
/// `VA_PROGRESSIVE`.
const PROGRESSIVE: c_int = 1;
/// `VA_INVALID_ID` (and `VA_INVALID_SURFACE`).
pub const INVALID_ID: u32 = 0xffff_ffff;
/// `VA_FOURCC_NV12`.
const FOURCC_NV12: u32 = u32::from_le_bytes(*b"NV12");
/// `VA_PICTURE_H264_INVALID`.
pub const PICTURE_H264_INVALID: u32 = 0x01;
/// `VA_PICTURE_H264_SHORT_TERM_REFERENCE`.
pub const PICTURE_H264_SHORT_TERM_REFERENCE: u32 = 0x08;
/// `VA_PICTURE_H264_LONG_TERM_REFERENCE`.
pub const PICTURE_H264_LONG_TERM_REFERENCE: u32 = 0x10;

/// A picture's luma and two chroma planes, I420.
pub type I420 = (Vec<u8>, Vec<u8>, Vec<u8>);

/// `VABufferType` values used.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(i32)]
pub enum BufferType {
    /// `VAPictureParameterBufferType`.
    PictureParameter = 0,
    /// `VAIQMatrixBufferType`.
    IqMatrix = 1,
    /// `VASliceParameterBufferType`.
    SliceParameter = 4,
    /// `VASliceDataBufferType`.
    SliceData = 5,
}

/// `VAPictureH264`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[repr(C)]
pub struct PictureH264 {
    /// `picture_id`: the surface.
    pub picture_id: u32,
    /// `frame_idx`: `frame_num`, or `LongTermFrameIdx`.
    pub frame_idx: u32,
    /// `flags`: `VA_PICTURE_H264_*`.
    pub flags: u32,
    /// `TopFieldOrderCnt`.
    pub top_field_order_cnt: i32,
    /// `BottomFieldOrderCnt`.
    pub bottom_field_order_cnt: i32,
    /// `va_reserved`.
    pub reserved: [u32; 4],
}

impl PictureH264 {
    /// An empty slot.
    pub fn invalid() -> Self {
        Self {
            picture_id: INVALID_ID,
            flags: PICTURE_H264_INVALID,
            ..Self::default()
        }
    }
}

/// `VAPictureParameterBufferH264`. The bit fields `seq_fields` and
/// `pic_fields` are built with [`seq_fields`] and [`pic_fields`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(C)]
pub struct PictureParameterBufferH264 {
    /// `CurrPic`.
    pub curr_pic: PictureH264,
    /// `ReferenceFrames`: the buffer's references, then invalid slots.
    pub reference_frames: [PictureH264; 16],
    /// `picture_width_in_mbs_minus1`.
    pub picture_width_in_mbs_minus1: u16,
    /// `picture_height_in_mbs_minus1`.
    pub picture_height_in_mbs_minus1: u16,
    /// `bit_depth_luma_minus8`.
    pub bit_depth_luma_minus8: u8,
    /// `bit_depth_chroma_minus8`.
    pub bit_depth_chroma_minus8: u8,
    /// `num_ref_frames`.
    pub num_ref_frames: u8,
    /// The C compiler's padding, written out.
    pub pad0: u8,
    /// `seq_fields.value`.
    pub seq_fields: u32,
    /// `num_slice_groups_minus1` (deprecated; slice groups unsupported).
    pub num_slice_groups_minus1: u8,
    /// `slice_group_map_type` (deprecated).
    pub slice_group_map_type: u8,
    /// `slice_group_change_rate_minus1` (deprecated).
    pub slice_group_change_rate_minus1: u16,
    /// `pic_init_qp_minus26`.
    pub pic_init_qp_minus26: i8,
    /// `pic_init_qs_minus26`.
    pub pic_init_qs_minus26: i8,
    /// `chroma_qp_index_offset`.
    pub chroma_qp_index_offset: i8,
    /// `second_chroma_qp_index_offset`.
    pub second_chroma_qp_index_offset: i8,
    /// `pic_fields.value`.
    pub pic_fields: u32,
    /// `frame_num`.
    pub frame_num: u16,
    /// The C compiler's padding, written out.
    pub pad1: u16,
    /// `va_reserved`.
    pub reserved: [u32; 8],
}

/// The fields of `VAPictureParameterBufferH264.seq_fields`, in order.
#[derive(Clone, Copy, Debug, Default)]
pub struct SeqFields {
    /// `chroma_format_idc` (2 bits).
    pub chroma_format_idc: u32,
    /// `residual_colour_transform_flag`.
    pub separate_colour_plane_flag: bool,
    /// `gaps_in_frame_num_value_allowed_flag`.
    pub gaps_in_frame_num_value_allowed_flag: bool,
    /// `frame_mbs_only_flag`.
    pub frame_mbs_only_flag: bool,
    /// `mb_adaptive_frame_field_flag`.
    pub mb_adaptive_frame_field_flag: bool,
    /// `direct_8x8_inference_flag`.
    pub direct_8x8_inference_flag: bool,
    /// `MinLumaBiPredSize8x8`.
    pub min_luma_bi_pred_size8x8: bool,
    /// `log2_max_frame_num_minus4` (4 bits).
    pub log2_max_frame_num_minus4: u32,
    /// `pic_order_cnt_type` (2 bits).
    pub pic_order_cnt_type: u32,
    /// `log2_max_pic_order_cnt_lsb_minus4` (4 bits).
    pub log2_max_pic_order_cnt_lsb_minus4: u32,
    /// `delta_pic_order_always_zero_flag`.
    pub delta_pic_order_always_zero_flag: bool,
}

/// `seq_fields.value` from its fields: C bit fields of a `uint32_t`, the
/// first in the lowest bits.
pub fn seq_fields(f: &SeqFields) -> u32 {
    (f.chroma_format_idc & 0x3)
        | u32::from(f.separate_colour_plane_flag) << 2
        | u32::from(f.gaps_in_frame_num_value_allowed_flag) << 3
        | u32::from(f.frame_mbs_only_flag) << 4
        | u32::from(f.mb_adaptive_frame_field_flag) << 5
        | u32::from(f.direct_8x8_inference_flag) << 6
        | u32::from(f.min_luma_bi_pred_size8x8) << 7
        | (f.log2_max_frame_num_minus4 & 0xf) << 8
        | (f.pic_order_cnt_type & 0x3) << 12
        | (f.log2_max_pic_order_cnt_lsb_minus4 & 0xf) << 14
        | u32::from(f.delta_pic_order_always_zero_flag) << 18
}

/// The fields of `VAPictureParameterBufferH264.pic_fields`, in order.
#[derive(Clone, Copy, Debug, Default)]
pub struct PicFields {
    /// `entropy_coding_mode_flag`.
    pub entropy_coding_mode_flag: bool,
    /// `weighted_pred_flag`.
    pub weighted_pred_flag: bool,
    /// `weighted_bipred_idc` (2 bits).
    pub weighted_bipred_idc: u32,
    /// `transform_8x8_mode_flag`.
    pub transform_8x8_mode_flag: bool,
    /// `field_pic_flag`.
    pub field_pic_flag: bool,
    /// `constrained_intra_pred_flag`.
    pub constrained_intra_pred_flag: bool,
    /// `pic_order_present_flag`.
    pub pic_order_present_flag: bool,
    /// `deblocking_filter_control_present_flag`.
    pub deblocking_filter_control_present_flag: bool,
    /// `redundant_pic_cnt_present_flag`.
    pub redundant_pic_cnt_present_flag: bool,
    /// `reference_pic_flag`: `nal_ref_idc != 0`.
    pub reference_pic_flag: bool,
}

/// `pic_fields.value` from its fields.
pub fn pic_fields(f: &PicFields) -> u32 {
    u32::from(f.entropy_coding_mode_flag)
        | u32::from(f.weighted_pred_flag) << 1
        | (f.weighted_bipred_idc & 0x3) << 2
        | u32::from(f.transform_8x8_mode_flag) << 4
        | u32::from(f.field_pic_flag) << 5
        | u32::from(f.constrained_intra_pred_flag) << 6
        | u32::from(f.pic_order_present_flag) << 7
        | u32::from(f.deblocking_filter_control_present_flag) << 8
        | u32::from(f.redundant_pic_cnt_present_flag) << 9
        | u32::from(f.reference_pic_flag) << 10
}

/// `VAIQMatrixBufferH264`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(C)]
pub struct IqMatrixBufferH264 {
    /// `ScalingList4x4`, raster order.
    pub scaling_list4x4: [[u8; 16]; 6],
    /// `ScalingList8x8`, raster order.
    pub scaling_list8x8: [[u8; 64]; 2],
    /// `va_reserved`.
    pub reserved: [u32; 4],
}

/// `VASliceParameterBufferH264`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(C)]
pub struct SliceParameterBufferH264 {
    /// `slice_data_size`.
    pub slice_data_size: u32,
    /// `slice_data_offset`.
    pub slice_data_offset: u32,
    /// `slice_data_flag`: `VA_SLICE_DATA_FLAG_ALL` (0).
    pub slice_data_flag: u32,
    /// `slice_data_bit_offset`.
    pub slice_data_bit_offset: u16,
    /// `first_mb_in_slice`.
    pub first_mb_in_slice: u16,
    /// `slice_type`.
    pub slice_type: u8,
    /// `direct_spatial_mv_pred_flag`.
    pub direct_spatial_mv_pred_flag: u8,
    /// `num_ref_idx_l0_active_minus1`.
    pub num_ref_idx_l0_active_minus1: u8,
    /// `num_ref_idx_l1_active_minus1`.
    pub num_ref_idx_l1_active_minus1: u8,
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
    pub pad0: [u8; 3],
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
    /// Padding.
    pub pad1: u8,
    /// `luma_weight_l0`.
    pub luma_weight_l0: [i16; 32],
    /// `luma_offset_l0`.
    pub luma_offset_l0: [i16; 32],
    /// `chroma_weight_l0_flag`.
    pub chroma_weight_l0_flag: u8,
    /// Padding.
    pub pad2: u8,
    /// `chroma_weight_l0`.
    pub chroma_weight_l0: [[i16; 2]; 32],
    /// `chroma_offset_l0`.
    pub chroma_offset_l0: [[i16; 2]; 32],
    /// `luma_weight_l1_flag`.
    pub luma_weight_l1_flag: u8,
    /// Padding.
    pub pad3: u8,
    /// `luma_weight_l1`.
    pub luma_weight_l1: [i16; 32],
    /// `luma_offset_l1`.
    pub luma_offset_l1: [i16; 32],
    /// `chroma_weight_l1_flag`.
    pub chroma_weight_l1_flag: u8,
    /// Padding.
    pub pad4: u8,
    /// `chroma_weight_l1`.
    pub chroma_weight_l1: [[i16; 2]; 32],
    /// `chroma_offset_l1`.
    pub chroma_offset_l1: [[i16; 2]; 32],
    /// Padding.
    pub pad5: u16,
    /// `va_reserved`.
    pub reserved: [u32; 4],
}

impl Default for SliceParameterBufferH264 {
    fn default() -> Self {
        Self {
            slice_data_size: 0,
            slice_data_offset: 0,
            slice_data_flag: 0,
            slice_data_bit_offset: 0,
            first_mb_in_slice: 0,
            slice_type: 0,
            direct_spatial_mv_pred_flag: 0,
            num_ref_idx_l0_active_minus1: 0,
            num_ref_idx_l1_active_minus1: 0,
            cabac_init_idc: 0,
            slice_qp_delta: 0,
            disable_deblocking_filter_idc: 0,
            slice_alpha_c0_offset_div2: 0,
            slice_beta_offset_div2: 0,
            pad0: [0; 3],
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
            pad5: 0,
            reserved: [0; 4],
        }
    }
}

/// `VAConfigAttrib`.
#[derive(Clone, Copy, Debug, Default)]
#[repr(C)]
struct ConfigAttrib {
    kind: c_int,
    value: u32,
}

/// `VAImageFormat`.
#[derive(Clone, Copy, Debug, Default)]
#[repr(C)]
struct ImageFormat {
    fourcc: u32,
    byte_order: u32,
    bits_per_pixel: u32,
    depth: u32,
    red_mask: u32,
    green_mask: u32,
    blue_mask: u32,
    alpha_mask: u32,
    reserved: [u32; 4],
}

/// `VAImage`.
#[derive(Clone, Copy, Debug, Default)]
#[repr(C)]
struct Image {
    image_id: u32,
    format: ImageFormat,
    buf: u32,
    width: u16,
    height: u16,
    data_size: u32,
    num_planes: u32,
    pitches: [u32; 3],
    offsets: [u32; 3],
    num_palette_entries: i32,
    entry_bytes: i32,
    component_order: [i8; 4],
    reserved: [u32; 4],
}

/// A parameter structure libva takes as bytes: `repr(C)` with no
/// padding the compiler fills in (it is written out as fields), so all
/// of its bytes are initialized.
pub trait Parameter: Copy + private::Sealed {
    /// Its bytes, as libva reads them.
    fn bytes(&self) -> &[u8] {
        // SAFETY: implemented only for the `repr(C)` structures above,
        // whose padding is spelled out as fields (their sizes are checked
        // in the tests to equal the C structures', which they could not
        // if the compiler had added any), so every byte of `self` is
        // initialized; the slice lives as long as the borrow of `self`.
        unsafe {
            std::slice::from_raw_parts(
                (self as *const Self).cast::<u8>(),
                std::mem::size_of::<Self>(),
            )
        }
    }
}

mod private {
    pub trait Sealed {}
    impl Sealed for super::PictureParameterBufferH264 {}
    impl Sealed for super::IqMatrixBufferH264 {}
    impl Sealed for super::SliceParameterBufferH264 {}
}

impl Parameter for PictureParameterBufferH264 {}
impl Parameter for IqMatrixBufferH264 {}
impl Parameter for SliceParameterBufferH264 {}

/// libva's functions, looked up once.
struct Functions {
    get_display_drm: unsafe extern "C" fn(c_int) -> RawDisplay,
    initialize: unsafe extern "C" fn(RawDisplay, *mut c_int, *mut c_int) -> Status,
    terminate: unsafe extern "C" fn(RawDisplay) -> Status,
    error_str: unsafe extern "C" fn(Status) -> *const c_char,
    query_vendor_string: unsafe extern "C" fn(RawDisplay) -> *const c_char,
    max_num_profiles: unsafe extern "C" fn(RawDisplay) -> c_int,
    query_config_profiles: unsafe extern "C" fn(RawDisplay, *mut c_int, *mut c_int) -> Status,
    max_num_entrypoints: unsafe extern "C" fn(RawDisplay) -> c_int,
    query_config_entrypoints:
        unsafe extern "C" fn(RawDisplay, c_int, *mut c_int, *mut c_int) -> Status,
    get_config_attributes:
        unsafe extern "C" fn(RawDisplay, c_int, c_int, *mut ConfigAttrib, c_int) -> Status,
    create_config: unsafe extern "C" fn(
        RawDisplay,
        c_int,
        c_int,
        *mut ConfigAttrib,
        c_int,
        *mut u32,
    ) -> Status,
    destroy_config: unsafe extern "C" fn(RawDisplay, u32) -> Status,
    create_surfaces: unsafe extern "C" fn(
        RawDisplay,
        c_uint,
        c_uint,
        c_uint,
        *mut u32,
        c_uint,
        *mut c_void,
        c_uint,
    ) -> Status,
    destroy_surfaces: unsafe extern "C" fn(RawDisplay, *mut u32, c_int) -> Status,
    create_context: unsafe extern "C" fn(
        RawDisplay,
        u32,
        c_int,
        c_int,
        c_int,
        *mut u32,
        c_int,
        *mut u32,
    ) -> Status,
    destroy_context: unsafe extern "C" fn(RawDisplay, u32) -> Status,
    create_buffer: unsafe extern "C" fn(
        RawDisplay,
        u32,
        c_int,
        c_uint,
        c_uint,
        *mut c_void,
        *mut u32,
    ) -> Status,
    destroy_buffer: unsafe extern "C" fn(RawDisplay, u32) -> Status,
    begin_picture: unsafe extern "C" fn(RawDisplay, u32, u32) -> Status,
    render_picture: unsafe extern "C" fn(RawDisplay, u32, *mut u32, c_int) -> Status,
    end_picture: unsafe extern "C" fn(RawDisplay, u32) -> Status,
    sync_surface: unsafe extern "C" fn(RawDisplay, u32) -> Status,
    derive_image: unsafe extern "C" fn(RawDisplay, u32, *mut Image) -> Status,
    create_image:
        unsafe extern "C" fn(RawDisplay, *mut ImageFormat, c_int, c_int, *mut Image) -> Status,
    get_image: unsafe extern "C" fn(RawDisplay, u32, c_int, c_int, c_uint, c_uint, u32) -> Status,
    destroy_image: unsafe extern "C" fn(RawDisplay, u32) -> Status,
    map_buffer: unsafe extern "C" fn(RawDisplay, u32, *mut *mut c_void) -> Status,
    unmap_buffer: unsafe extern "C" fn(RawDisplay, u32) -> Status,
}

/// The loaded libraries and their functions. The function pointers stay
/// valid as long as the libraries do, which live in the same value.
struct Libva {
    functions: Functions,
    _libva: Library,
    _libva_drm: Library,
}

impl Libva {
    fn load() -> Result<Self, String> {
        // SAFETY: loading libva runs its initializers, which only set up
        // its own state; the names are libva 2's sonames.
        let libva = unsafe { Library::new("libva.so.2") }.map_err(|e| format!("libva: {e}"))?;
        // SAFETY: as above.
        let libva_drm =
            unsafe { Library::new("libva-drm.so.2") }.map_err(|e| format!("libva-drm: {e}"))?;
        macro_rules! symbol {
            ($library:expr, $name:literal) => {
                // SAFETY: the type each field is declared with is the
                // function's C signature from va.h (and va_drm.h); the
                // pointer is copied out of the `Symbol` and kept next to
                // the `Library` it came from, which outlives it.
                *unsafe { $library.get(concat!($name, "\0").as_bytes()) }
                    .map_err(|e| format!("{}: {e}", $name))?
            };
        }
        let functions = Functions {
            get_display_drm: symbol!(libva_drm, "vaGetDisplayDRM"),
            initialize: symbol!(libva, "vaInitialize"),
            terminate: symbol!(libva, "vaTerminate"),
            error_str: symbol!(libva, "vaErrorStr"),
            query_vendor_string: symbol!(libva, "vaQueryVendorString"),
            max_num_profiles: symbol!(libva, "vaMaxNumProfiles"),
            query_config_profiles: symbol!(libva, "vaQueryConfigProfiles"),
            max_num_entrypoints: symbol!(libva, "vaMaxNumEntrypoints"),
            query_config_entrypoints: symbol!(libva, "vaQueryConfigEntrypoints"),
            get_config_attributes: symbol!(libva, "vaGetConfigAttributes"),
            create_config: symbol!(libva, "vaCreateConfig"),
            destroy_config: symbol!(libva, "vaDestroyConfig"),
            create_surfaces: symbol!(libva, "vaCreateSurfaces"),
            destroy_surfaces: symbol!(libva, "vaDestroySurfaces"),
            create_context: symbol!(libva, "vaCreateContext"),
            destroy_context: symbol!(libva, "vaDestroyContext"),
            create_buffer: symbol!(libva, "vaCreateBuffer"),
            destroy_buffer: symbol!(libva, "vaDestroyBuffer"),
            begin_picture: symbol!(libva, "vaBeginPicture"),
            render_picture: symbol!(libva, "vaRenderPicture"),
            end_picture: symbol!(libva, "vaEndPicture"),
            sync_surface: symbol!(libva, "vaSyncSurface"),
            derive_image: symbol!(libva, "vaDeriveImage"),
            create_image: symbol!(libva, "vaCreateImage"),
            get_image: symbol!(libva, "vaGetImage"),
            destroy_image: symbol!(libva, "vaDestroyImage"),
            map_buffer: symbol!(libva, "vaMapBuffer"),
            unmap_buffer: symbol!(libva, "vaUnmapBuffer"),
        };
        Ok(Self {
            functions,
            _libva: libva,
            _libva_drm: libva_drm,
        })
    }
}

/// An initialized VA display on one DRM render node.
pub struct Display {
    libva: Libva,
    raw: RawDisplay,
    /// The render node; libva uses the descriptor until terminated.
    _device: File,
    /// The node's path, for the log.
    pub path: String,
    /// Whether reading a surface by deriving an image failed once: then
    /// every read copies through `vaGetImage`.
    derive_failed: std::cell::Cell<bool>,
}

impl std::fmt::Debug for Display {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Display")
            .field("path", &self.path)
            .finish_non_exhaustive()
    }
}

impl Drop for Display {
    fn drop(&mut self) {
        // SAFETY: `raw` was initialized by vaInitialize and everything
        // made on it (configs, surfaces, contexts) holds an `Rc` of this
        // display, so all of it is gone by now.
        unsafe { (self.libva.functions.terminate)(self.raw) };
    }
}

/// The text libva gives for a status.
fn describe(libva: &Libva, status: Status) -> String {
    // SAFETY: vaErrorStr returns a pointer to a static string for any
    // status value.
    let text = unsafe { (libva.functions.error_str)(status) };
    if text.is_null() {
        return format!("VA status {status}");
    }
    // SAFETY: non-null and static, as above; NUL-terminated by libva.
    let text = unsafe { CStr::from_ptr(text) };
    format!("{} ({status})", text.to_string_lossy())
}

impl Display {
    /// Loads libva and opens the first render node with a driver
    /// (`/dev/dri/renderD128` and on).
    pub fn open() -> Result<Rc<Self>, String> {
        let libva = Libva::load()?;
        let mut why = String::from("no render node");
        let mut libva = Some(libva);
        for node in 128..192 {
            let path = format!("/dev/dri/renderD{node}");
            let Ok(device) = File::options().read(true).write(true).open(&path) else {
                continue;
            };
            let Some(loaded) = libva.take() else { break };
            match Self::initialize(loaded, device, path) {
                Ok(display) => return Ok(Rc::new(display)),
                Err(failed) => {
                    let (loaded, reason) = *failed;
                    why = reason;
                    libva = Some(loaded);
                }
            }
        }
        Err(why)
    }

    fn initialize(libva: Libva, device: File, path: String) -> Result<Self, Box<(Libva, String)>> {
        // SAFETY: the descriptor is an open render node, kept open in the
        // display for as long as libva uses it.
        let raw = unsafe { (libva.functions.get_display_drm)(device.as_raw_fd()) };
        if raw.is_null() {
            return Err(Box::new((libva, format!("{path}: no VA display"))));
        }
        let (mut major, mut minor) = (0, 0);
        // SAFETY: a display from vaGetDisplayDRM; the out pointers are to
        // live locals.
        let status = unsafe { (libva.functions.initialize)(raw, &mut major, &mut minor) };
        if status != 0 {
            let why = format!("{path}: {}", describe(&libva, status));
            // SAFETY: terminating a display whose initialization failed
            // frees what vaGetDisplayDRM allocated.
            unsafe { (libva.functions.terminate)(raw) };
            return Err(Box::new((libva, why)));
        }
        Ok(Self {
            libva,
            raw,
            _device: device,
            path,
            derive_failed: std::cell::Cell::new(false),
        })
    }

    fn check(&self, status: Status, what: &str) -> Result<(), String> {
        if status == 0 {
            Ok(())
        } else {
            Err(format!("{what}: {}", describe(&self.libva, status)))
        }
    }

    /// The driver's name and version.
    pub fn vendor(&self) -> String {
        // SAFETY: an initialized display; the string is libva's, static
        // for the display's life, and copied out at once.
        let text = unsafe { (self.libva.functions.query_vendor_string)(self.raw) };
        if text.is_null() {
            return "unknown driver".to_owned();
        }
        // SAFETY: non-null, NUL-terminated by libva.
        unsafe { CStr::from_ptr(text) }
            .to_string_lossy()
            .into_owned()
    }

    /// The profiles the driver knows.
    pub fn profiles(&self) -> Result<Vec<i32>, String> {
        // SAFETY: an initialized display.
        let max = unsafe { (self.libva.functions.max_num_profiles)(self.raw) };
        let mut list = vec![0; usize::try_from(max).unwrap_or(0).max(1)];
        let mut count = 0;
        // SAFETY: the list holds vaMaxNumProfiles entries, as libva asks.
        let status = unsafe {
            (self.libva.functions.query_config_profiles)(self.raw, list.as_mut_ptr(), &mut count)
        };
        self.check(status, "vaQueryConfigProfiles")?;
        list.truncate(usize::try_from(count).unwrap_or(0));
        Ok(list)
    }

    /// The entry points the driver has for `profile`.
    pub fn entrypoints(&self, profile: i32) -> Result<Vec<i32>, String> {
        // SAFETY: an initialized display.
        let max = unsafe { (self.libva.functions.max_num_entrypoints)(self.raw) };
        let mut list = vec![0; usize::try_from(max).unwrap_or(0).max(1)];
        let mut count = 0;
        // SAFETY: the list holds vaMaxNumEntrypoints entries.
        let status = unsafe {
            (self.libva.functions.query_config_entrypoints)(
                self.raw,
                profile,
                list.as_mut_ptr(),
                &mut count,
            )
        };
        self.check(status, "vaQueryConfigEntrypoints")?;
        list.truncate(usize::try_from(count).unwrap_or(0));
        Ok(list)
    }

    /// The values of config attributes `kinds` for `profile` and
    /// `entrypoint`; [`ATTRIB_NOT_SUPPORTED`] for those the driver does
    /// not know.
    pub fn attributes(
        &self,
        profile: i32,
        entrypoint: i32,
        kinds: &[i32],
    ) -> Result<Vec<u32>, String> {
        let mut list: Vec<ConfigAttrib> = kinds
            .iter()
            .map(|&kind| ConfigAttrib { kind, value: 0 })
            .collect();
        let count = c_int::try_from(list.len()).map_err(|e| e.to_string())?;
        // SAFETY: `list` holds `count` attributes for libva to fill.
        let status = unsafe {
            (self.libva.functions.get_config_attributes)(
                self.raw,
                profile,
                entrypoint,
                list.as_mut_ptr(),
                count,
            )
        };
        self.check(status, "vaGetConfigAttributes")?;
        Ok(list.iter().map(|a| a.value).collect())
    }
}

/// A decoding configuration.
pub struct Config {
    display: Rc<Display>,
    id: u32,
}

impl Config {
    /// A configuration for decoding `profile` into 4:2:0 surfaces.
    pub fn new(display: &Rc<Display>, profile: i32) -> Result<Self, String> {
        let mut attribute = ConfigAttrib {
            kind: ATTRIB_RT_FORMAT,
            value: RT_FORMAT_YUV420,
        };
        let mut id = INVALID_ID;
        // SAFETY: one attribute, and an out pointer to a live local.
        let status = unsafe {
            (display.libva.functions.create_config)(
                display.raw,
                profile,
                ENTRYPOINT_VLD,
                &mut attribute,
                1,
                &mut id,
            )
        };
        display.check(status, "vaCreateConfig")?;
        Ok(Self {
            display: Rc::clone(display),
            id,
        })
    }
}

impl Drop for Config {
    fn drop(&mut self) {
        // SAFETY: a config made on this display and not yet destroyed; a
        // context made from it owns it, and is destroyed first.
        unsafe { (self.display.libva.functions.destroy_config)(self.display.raw, self.id) };
    }
}

/// A set of 4:2:0 surfaces to decode into.
pub struct Surfaces {
    display: Rc<Display>,
    /// Their ids.
    pub ids: Vec<u32>,
}

impl Surfaces {
    /// `count` surfaces of `width`×`height`.
    pub fn new(
        display: &Rc<Display>,
        width: u32,
        height: u32,
        count: usize,
    ) -> Result<Self, String> {
        let mut ids = vec![INVALID_ID; count];
        let n = c_uint::try_from(count).map_err(|e| e.to_string())?;
        // SAFETY: `ids` has room for `count` surface ids; no attributes.
        let status = unsafe {
            (display.libva.functions.create_surfaces)(
                display.raw,
                RT_FORMAT_YUV420,
                width,
                height,
                ids.as_mut_ptr(),
                n,
                std::ptr::null_mut(),
                0,
            )
        };
        display.check(status, "vaCreateSurfaces")?;
        Ok(Self {
            display: Rc::clone(display),
            ids,
        })
    }
}

impl Drop for Surfaces {
    fn drop(&mut self) {
        let count = c_int::try_from(self.ids.len()).unwrap_or(0);
        // SAFETY: surfaces made on this display; the context that renders
        // into them owns them, and is destroyed first.
        unsafe {
            (self.display.libva.functions.destroy_surfaces)(
                self.display.raw,
                self.ids.as_mut_ptr(),
                count,
            )
        };
    }
}

/// A decoding context, with the config and surfaces it was made over
/// (destroyed after it, as fields drop after `drop`).
pub struct Context {
    display: Rc<Display>,
    id: u32,
    _config: Config,
    /// The surfaces it decodes into.
    pub surfaces: Surfaces,
}

impl Context {
    /// A context decoding `width`×`height` pictures into `surfaces`.
    pub fn new(
        config: Config,
        surfaces: Surfaces,
        width: u32,
        height: u32,
    ) -> Result<Self, String> {
        let display = Rc::clone(&config.display);
        let mut targets = surfaces.ids.clone();
        let mut id = INVALID_ID;
        let int = |n: u32| c_int::try_from(n).map_err(|e| e.to_string());
        let count = c_int::try_from(targets.len()).map_err(|e| e.to_string())?;
        // SAFETY: a live config and surfaces of this display; the out
        // pointer is to a live local.
        let status = unsafe {
            (display.libva.functions.create_context)(
                display.raw,
                config.id,
                int(width)?,
                int(height)?,
                PROGRESSIVE,
                targets.as_mut_ptr(),
                count,
                &mut id,
            )
        };
        display.check(status, "vaCreateContext")?;
        Ok(Self {
            display,
            id,
            _config: config,
            surfaces,
        })
    }

    fn buffer(&self, kind: BufferType, data: &[u8]) -> Result<Buffer<'_>, String> {
        let size = c_uint::try_from(data.len()).map_err(|e| e.to_string())?;
        let mut id = INVALID_ID;
        // SAFETY: libva copies `size` bytes from `data` into the new
        // buffer (it does not write through the pointer, whatever its C
        // type says); the out pointer is to a live local.
        let status = unsafe {
            (self.display.libva.functions.create_buffer)(
                self.display.raw,
                self.id,
                kind as c_int,
                size,
                1,
                data.as_ptr().cast_mut().cast(),
                &mut id,
            )
        };
        self.display.check(status, "vaCreateBuffer")?;
        Ok(Buffer {
            display: &self.display,
            id,
        })
    }

    /// Decodes one picture into surface `target` from `buffers` (its
    /// picture parameters, matrix, and each slice's parameters and data),
    /// and waits until it is done.
    pub fn decode(&self, target: u32, buffers: &[(BufferType, &[u8])]) -> Result<(), String> {
        let made: Vec<Buffer<'_>> = buffers
            .iter()
            .map(|(kind, data)| self.buffer(*kind, data))
            .collect::<Result<_, _>>()?;
        let mut ids: Vec<u32> = made.iter().map(|b| b.id).collect();
        let count = c_int::try_from(ids.len()).map_err(|e| e.to_string())?;
        let functions = &self.display.libva.functions;
        // SAFETY: a live context and one of its surfaces.
        let status = unsafe { (functions.begin_picture)(self.display.raw, self.id, target) };
        self.display.check(status, "vaBeginPicture")?;
        // SAFETY: buffers of this context, alive until `made` drops.
        let rendered = unsafe {
            (functions.render_picture)(self.display.raw, self.id, ids.as_mut_ptr(), count)
        };
        // SAFETY: a picture was begun; it is ended even if rendering
        // failed, so the context is ready for the next.
        let ended = unsafe { (functions.end_picture)(self.display.raw, self.id) };
        self.display.check(rendered, "vaRenderPicture")?;
        self.display.check(ended, "vaEndPicture")?;
        // SAFETY: a surface of this display.
        let synced = unsafe { (functions.sync_surface)(self.display.raw, target) };
        self.display.check(synced, "vaSyncSurface")
    }
}

impl Drop for Context {
    fn drop(&mut self) {
        // SAFETY: a context made on this display, not yet destroyed.
        unsafe { (self.display.libva.functions.destroy_context)(self.display.raw, self.id) };
    }
}

/// A parameter or data buffer, destroyed when dropped (libva 2 no
/// longer frees them after rendering).
struct Buffer<'a> {
    display: &'a Display,
    id: u32,
}

impl Drop for Buffer<'_> {
    fn drop(&mut self) {
        // SAFETY: a buffer made on this display, not yet destroyed.
        unsafe { (self.display.libva.functions.destroy_buffer)(self.display.raw, self.id) };
    }
}

/// A VA image, destroyed when dropped.
struct ImageHandle<'a> {
    display: &'a Display,
    image: Image,
}

impl Drop for ImageHandle<'_> {
    fn drop(&mut self) {
        // SAFETY: an image made on this display, not yet destroyed.
        unsafe {
            (self.display.libva.functions.destroy_image)(self.display.raw, self.image.image_id)
        };
    }
}

/// Where the planes of an NV12 image lie in its buffer, checked to fit.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Nv12Layout {
    /// Bytes from one luma row to the next.
    pub luma_pitch: usize,
    /// Where the luma plane starts.
    pub luma_offset: usize,
    /// Bytes from one chroma row to the next.
    pub chroma_pitch: usize,
    /// Where the interleaved chroma plane starts.
    pub chroma_offset: usize,
}

/// Checks that an NV12 image of `image_size` with `pitches` and
/// `offsets` in a buffer of `data_size` bytes holds the rectangle
/// `crop` (left, top, width, height): nothing is read from a mapped
/// buffer before this holds.
pub fn nv12_layout(
    image_size: (u32, u32),
    pitches: [u32; 2],
    offsets: [u32; 2],
    data_size: u32,
    crop: (u32, u32, u32, u32),
) -> Result<Nv12Layout, String> {
    let n = |v: u32| usize::try_from(v).map_err(|e| e.to_string());
    let (left, top, width, height) = (n(crop.0)?, n(crop.1)?, n(crop.2)?, n(crop.3)?);
    let (image_width, image_height) = (n(image_size.0)?, n(image_size.1)?);
    let right = left.checked_add(width).ok_or("crop overflows")?;
    let bottom = top.checked_add(height).ok_or("crop overflows")?;
    if width == 0 || height == 0 || right > image_width || bottom > image_height {
        return Err(format!(
            "{width}x{height} at {left},{top} is not inside the {image_width}x{image_height} image"
        ));
    }
    let layout = Nv12Layout {
        luma_pitch: n(pitches[0])?,
        luma_offset: n(offsets[0])?,
        chroma_pitch: n(pitches[1])?,
        chroma_offset: n(offsets[1])?,
    };
    if layout.luma_pitch < image_width || layout.chroma_pitch < image_width.div_ceil(2) * 2 {
        return Err("pitches narrower than the image".into());
    }
    // The last byte read of each plane, past which nothing is touched.
    let luma_end = layout
        .luma_pitch
        .checked_mul(bottom - 1)
        .and_then(|v| v.checked_add(layout.luma_offset))
        .and_then(|v| v.checked_add(right));
    let chroma_end = layout
        .chroma_pitch
        .checked_mul(bottom.div_ceil(2) - 1)
        .and_then(|v| v.checked_add(layout.chroma_offset))
        .and_then(|v| v.checked_add(right.div_ceil(2) * 2));
    let size = n(data_size)?;
    match (luma_end, chroma_end) {
        (Some(luma), Some(chroma)) if luma <= size && chroma <= size => Ok(layout),
        _ => Err("the planes run past the image's buffer".into()),
    }
}

impl Display {
    /// Reads `crop` (left, top, width, height; left and top even) of
    /// decoded surface `surface` (`surface_size` large) as I420 planes.
    pub fn read_i420(
        &self,
        surface: u32,
        surface_size: (u32, u32),
        crop: (u32, u32, u32, u32),
    ) -> Result<I420, String> {
        let image = if self.derive_failed.get() {
            self.copied_image(surface, surface_size)?
        } else {
            match self.derived_image(surface) {
                Ok(image) => image,
                Err(_) => {
                    // Not every driver derives (or derives NV12): copy
                    // from now on.
                    self.derive_failed.set(true);
                    self.copied_image(surface, surface_size)?
                }
            }
        };
        self.read_image(&image, crop)
    }

    fn derived_image(&self, surface: u32) -> Result<ImageHandle<'_>, String> {
        let mut image = Image::default();
        // SAFETY: a synced surface of this display; the out pointer is to
        // a live local.
        let status = unsafe { (self.libva.functions.derive_image)(self.raw, surface, &mut image) };
        self.check(status, "vaDeriveImage")?;
        let handle = ImageHandle {
            display: self,
            image,
        };
        if handle.image.format.fourcc != FOURCC_NV12 || handle.image.num_planes != 2 {
            return Err("the derived image is not NV12".into());
        }
        Ok(handle)
    }

    fn copied_image(&self, surface: u32, size: (u32, u32)) -> Result<ImageHandle<'_>, String> {
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
        // SAFETY: a synced surface and an image of the same size, both of
        // this display.
        let status = unsafe {
            (self.libva.functions.get_image)(
                self.raw,
                surface,
                0,
                0,
                size.0,
                size.1,
                handle.image.image_id,
            )
        };
        self.check(status, "vaGetImage")?;
        if handle.image.format.fourcc != FOURCC_NV12 || handle.image.num_planes != 2 {
            return Err("the image is not NV12".into());
        }
        Ok(handle)
    }

    fn read_image(
        &self,
        handle: &ImageHandle<'_>,
        crop: (u32, u32, u32, u32),
    ) -> Result<I420, String> {
        let image = &handle.image;
        let layout = nv12_layout(
            (u32::from(image.width), u32::from(image.height)),
            [image.pitches[0], image.pitches[1]],
            [image.offsets[0], image.offsets[1]],
            image.data_size,
            crop,
        )?;
        let mut mapped: *mut c_void = std::ptr::null_mut();
        // SAFETY: the image's buffer, mapped for reading; unmapped below
        // before the image is destroyed.
        let status = unsafe { (self.libva.functions.map_buffer)(self.raw, image.buf, &mut mapped) };
        self.check(status, "vaMapBuffer")?;
        if mapped.is_null() {
            return Err("vaMapBuffer gave nothing".into());
        }
        let size = usize::try_from(image.data_size).map_err(|e| e.to_string())?;
        // SAFETY: libva mapped `data_size` bytes at `mapped`, readable
        // until vaUnmapBuffer, which comes after the last use of `data`.
        let data = unsafe { std::slice::from_raw_parts(mapped.cast::<u8>(), size) };
        let planes = copy_nv12(data, &layout, crop);
        // SAFETY: mapped above; `data` is not used past here.
        let status = unsafe { (self.libva.functions.unmap_buffer)(self.raw, image.buf) };
        self.check(status, "vaUnmapBuffer")?;
        Ok(planes)
    }
}

/// The `crop` of an NV12 picture in `data` (laid out as `layout` says,
/// already checked to hold it) as I420 planes. Rows are copied whole
/// first: mapped video memory is often uncached, where one long read
/// beats many short ones.
pub fn copy_nv12(data: &[u8], layout: &Nv12Layout, crop: (u32, u32, u32, u32)) -> I420 {
    let n = |v: u32| usize::try_from(v).unwrap_or(0);
    let (left, top, width, height) = (n(crop.0), n(crop.1), n(crop.2), n(crop.3));
    let (cw, ch) = (width.div_ceil(2), height.div_ceil(2));
    let mut y = Vec::with_capacity(width * height);
    for row in top..top + height {
        let start = layout.luma_offset + row * layout.luma_pitch + left;
        y.extend_from_slice(&data[start..start + width]);
    }
    let mut u = Vec::with_capacity(cw * ch);
    let mut v = Vec::with_capacity(cw * ch);
    let mut line = vec![0u8; cw * 2];
    for row in top / 2..top / 2 + ch {
        let start = layout.chroma_offset + row * layout.chroma_pitch + (left / 2) * 2;
        line.copy_from_slice(&data[start..start + cw * 2]);
        for pair in line.as_chunks::<2>().0 {
            u.push(pair[0]);
            v.push(pair[1]);
        }
    }
    (y, u, v)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::mem::{offset_of, size_of};

    /// Sizes and offsets as clang computed them from libva 2.23's va.h
    /// on x86_64 (the same on every 64-bit Linux: no pointers or longs).
    #[test]
    fn structures_match_libva() {
        assert_eq!(size_of::<PictureH264>(), 36);
        assert_eq!(size_of::<PictureParameterBufferH264>(), 672);
        assert_eq!(offset_of!(PictureParameterBufferH264, reference_frames), 36);
        assert_eq!(
            offset_of!(PictureParameterBufferH264, picture_width_in_mbs_minus1),
            612
        );
        assert_eq!(offset_of!(PictureParameterBufferH264, num_ref_frames), 618);
        assert_eq!(offset_of!(PictureParameterBufferH264, seq_fields), 620);
        assert_eq!(
            offset_of!(PictureParameterBufferH264, num_slice_groups_minus1),
            624
        );
        assert_eq!(
            offset_of!(PictureParameterBufferH264, pic_init_qp_minus26),
            628
        );
        assert_eq!(offset_of!(PictureParameterBufferH264, pic_fields), 632);
        assert_eq!(offset_of!(PictureParameterBufferH264, frame_num), 636);
        assert_eq!(offset_of!(PictureParameterBufferH264, reserved), 640);
        assert_eq!(size_of::<IqMatrixBufferH264>(), 240);
        assert_eq!(size_of::<SliceParameterBufferH264>(), 3128);
        assert_eq!(
            offset_of!(SliceParameterBufferH264, slice_data_bit_offset),
            12
        );
        assert_eq!(offset_of!(SliceParameterBufferH264, slice_type), 16);
        assert_eq!(
            offset_of!(SliceParameterBufferH264, slice_beta_offset_div2),
            24
        );
        assert_eq!(offset_of!(SliceParameterBufferH264, ref_pic_list0), 28);
        assert_eq!(offset_of!(SliceParameterBufferH264, ref_pic_list1), 1180);
        assert_eq!(
            offset_of!(SliceParameterBufferH264, luma_log2_weight_denom),
            2332
        );
        assert_eq!(
            offset_of!(SliceParameterBufferH264, luma_weight_l0_flag),
            2334
        );
        assert_eq!(offset_of!(SliceParameterBufferH264, luma_weight_l0), 2336);
        assert_eq!(offset_of!(SliceParameterBufferH264, luma_offset_l0), 2400);
        assert_eq!(
            offset_of!(SliceParameterBufferH264, chroma_weight_l0_flag),
            2464
        );
        assert_eq!(offset_of!(SliceParameterBufferH264, chroma_weight_l0), 2466);
        assert_eq!(offset_of!(SliceParameterBufferH264, chroma_offset_l0), 2594);
        assert_eq!(
            offset_of!(SliceParameterBufferH264, luma_weight_l1_flag),
            2722
        );
        assert_eq!(offset_of!(SliceParameterBufferH264, luma_weight_l1), 2724);
        assert_eq!(offset_of!(SliceParameterBufferH264, luma_offset_l1), 2788);
        assert_eq!(
            offset_of!(SliceParameterBufferH264, chroma_weight_l1_flag),
            2852
        );
        assert_eq!(offset_of!(SliceParameterBufferH264, chroma_weight_l1), 2854);
        assert_eq!(offset_of!(SliceParameterBufferH264, chroma_offset_l1), 2982);
        assert_eq!(offset_of!(SliceParameterBufferH264, reserved), 3112);
        assert_eq!(size_of::<Image>(), 120);
        assert_eq!(size_of::<ImageFormat>(), 48);
        assert_eq!(offset_of!(Image, buf), 52);
        assert_eq!(offset_of!(Image, width), 56);
        assert_eq!(offset_of!(Image, data_size), 60);
        assert_eq!(offset_of!(Image, num_planes), 64);
        assert_eq!(offset_of!(Image, pitches), 68);
        assert_eq!(offset_of!(Image, offsets), 80);
        assert_eq!(offset_of!(Image, component_order), 100);
        assert_eq!(size_of::<ConfigAttrib>(), 8);
    }

    #[test]
    fn bit_fields_pack_lowest_first() {
        let seq = SeqFields {
            chroma_format_idc: 1,
            frame_mbs_only_flag: true,
            direct_8x8_inference_flag: true,
            log2_max_frame_num_minus4: 0xf,
            pic_order_cnt_type: 2,
            log2_max_pic_order_cnt_lsb_minus4: 0x3,
            delta_pic_order_always_zero_flag: true,
            ..SeqFields::default()
        };
        assert_eq!(
            seq_fields(&seq),
            1 | 1 << 4 | 1 << 6 | 0xf << 8 | 2 << 12 | 3 << 14 | 1 << 18
        );
        let pic = PicFields {
            weighted_bipred_idc: 2,
            deblocking_filter_control_present_flag: true,
            reference_pic_flag: true,
            ..PicFields::default()
        };
        assert_eq!(pic_fields(&pic), 2 << 2 | 1 << 8 | 1 << 10);
    }

    #[test]
    fn nv12_layouts_are_checked_before_reading() {
        let ok = nv12_layout((64, 32), [64, 64], [0, 64 * 32], 64 * 48, (0, 0, 64, 32));
        assert!(ok.is_ok());
        // A buffer too small for the chroma plane.
        assert!(nv12_layout((64, 32), [64, 64], [0, 64 * 32], 64 * 40, (0, 0, 64, 32)).is_err());
        // A pitch narrower than the picture.
        assert!(nv12_layout((64, 32), [32, 64], [0, 2048], 64 * 48, (0, 0, 64, 32)).is_err());
        // A crop outside the image.
        assert!(nv12_layout((64, 32), [64, 64], [0, 2048], 64 * 48, (8, 0, 64, 32)).is_err());
        // Offsets that overflow.
        assert!(
            nv12_layout(
                (64, 32),
                [64, 64],
                [u32::MAX, 2048],
                u32::MAX,
                (0, 0, 64, 32)
            )
            .is_err()
        );
    }

    #[test]
    fn nv12_becomes_i420_cropped() {
        // 4x4 picture, pitch 6: luma rows 0..4 then chroma rows 0..2.
        let mut data = Vec::new();
        for row in 0..4u8 {
            data.extend_from_slice(&[row * 10, row * 10 + 1, row * 10 + 2, row * 10 + 3, 99, 99]);
        }
        data.extend_from_slice(&[1, 2, 3, 4, 99, 99, 5, 6, 7, 8, 99, 99]);
        let layout = nv12_layout((4, 4), [6, 6], [0, 24], 36, (0, 0, 4, 2)).expect("fits");
        let (y, u, v) = copy_nv12(&data, &layout, (0, 0, 4, 2));
        assert_eq!(y, [0, 1, 2, 3, 10, 11, 12, 13]);
        assert_eq!(u, [1, 3]);
        assert_eq!(v, [2, 4]);
        let layout = nv12_layout((4, 4), [6, 6], [0, 24], 36, (2, 2, 2, 2)).expect("fits");
        let (y, u, v) = copy_nv12(&data, &layout, (2, 2, 2, 2));
        assert_eq!(y, [22, 23, 32, 33]);
        assert_eq!((u, v), (vec![7], vec![8]));
    }
}
