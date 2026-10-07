//! The VA-API back end (Linux): H.264 decoding and encoding on Intel,
//! AMD and other GPUs whose driver implements VA-API (Mesa's for AMD and
//! others, intel-media-driver for Intel). Decoding goes through libva's
//! stateless interface: [`crate::h264`] works out what each picture
//! needs; this fills in libva's parameter buffers, decodes into a surface
//! and reads the picture back as I420. Encoding is [`encoder`]'s.

pub mod encoder;
#[allow(unsafe_code)]
pub mod va;

use std::rc::Rc;

use cros_codecs::codec::h264::parser::{Pps, SliceType, Sps};
use noslacking_video_ipc::{
    Capability, Codec, Decoded, Direction, FailKind, MAX_SIDE, Planes, output_size,
};

use crate::backend::{Backend, Decoder, Encoder, Failure};
use crate::h264::{FrontEnd, Picture, Reference};
use crate::shrink;
use va::{
    BufferType, Config, Context, Display, IqMatrixBufferH264, Parameter, PicFields, PictureH264,
    PictureParameterBufferH264, Scaler, SeqFields, SliceParameterBufferH264, Surfaces,
};

/// The VA-API back end: a display with an H.264 decoder, an encoder, or
/// both.
pub struct Vaapi {
    display: Rc<Display>,
    /// The H.264 profiles the driver decodes, by preference for a
    /// constrained baseline stream.
    profiles: Vec<i32>,
    max_size: (u32, u32),
    /// How the driver encodes constrained baseline, if it does.
    encode: Option<encoder::EncodeSupport>,
}

impl std::fmt::Debug for Vaapi {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Vaapi")
            .field("display", &self.display)
            .field("profiles", &self.profiles)
            .field("encode", &self.encode)
            .finish_non_exhaustive()
    }
}

impl Vaapi {
    /// Opens libva on the first render node; why not, if its driver
    /// neither decodes nor encodes H.264.
    pub fn open() -> Result<Self, String> {
        let display = Display::open()?;
        let known = display.profiles()?;
        let mut profiles = Vec::new();
        for profile in [
            va::PROFILE_H264_CONSTRAINED_BASELINE,
            va::PROFILE_H264_MAIN,
            va::PROFILE_H264_HIGH,
        ] {
            if known.contains(&profile)
                && display
                    .entrypoints(profile)
                    .is_ok_and(|e| e.contains(&va::ENTRYPOINT_VLD))
            {
                profiles.push(profile);
            }
        }
        let encode = encoder::EncodeSupport::query(&display);
        if profiles.is_empty() && encode.is_none() {
            return Err(format!(
                "{} ({}) neither decodes nor encodes H.264",
                display.vendor(),
                display.path
            ));
        }
        let limits = match profiles.first() {
            Some(&first) => display
                .attributes(
                    first,
                    va::ENTRYPOINT_VLD,
                    &[va::ATTRIB_MAX_PICTURE_WIDTH, va::ATTRIB_MAX_PICTURE_HEIGHT],
                )
                .unwrap_or_default(),
            None => Vec::new(),
        };
        let limit = |i: usize| {
            limits
                .get(i)
                .copied()
                .filter(|&v| v != va::ATTRIB_NOT_SUPPORTED && v > 0)
                .unwrap_or(MAX_SIDE)
                .min(MAX_SIDE)
        };
        Ok(Self {
            display,
            profiles,
            max_size: (limit(0), limit(1)),
            encode,
        })
    }
}

impl Backend for Vaapi {
    fn name(&self) -> String {
        format!("vaapi: {} ({})", self.display.vendor(), self.display.path)
    }

    fn capabilities(&self) -> Vec<Capability> {
        let mut capabilities = Vec::new();
        if !self.profiles.is_empty() {
            capabilities.push(Capability {
                codec: Codec::H264,
                direction: Direction::Decode,
                max_width: self.max_size.0,
                max_height: self.max_size.1,
            });
        }
        if let Some(support) = self.encode {
            // Constrained baseline (what Direction::Encode means for
            // H.264), up to what the driver takes and level 4.2 holds:
            // 2048×1088 landscape (encoder::level).
            capabilities.push(Capability {
                codec: Codec::H264,
                direction: Direction::Encode,
                max_width: support.max_size.0.min(2048),
                max_height: support.max_size.1.min(1088),
            });
        }
        capabilities
    }

    fn open_encoder(
        &mut self,
        codec: Codec,
        width: u32,
        height: u32,
        fps: u32,
        bitrate: u32,
    ) -> Result<Box<dyn Encoder>, Failure> {
        let Codec::H264 = codec;
        let Some(support) = self.encode else {
            return Err(Failure::unsupported("the driver does not encode H.264"));
        };
        let encoder =
            encoder::VaapiEncoder::new(&self.display, support, (width, height), fps, bitrate)?;
        Ok(Box::new(encoder))
    }

    fn open_decoder(
        &mut self,
        codec: Codec,
        width: u32,
        height: u32,
    ) -> Result<Box<dyn Decoder>, Failure> {
        let Codec::H264 = codec;
        if self.profiles.is_empty() {
            return Err(Failure::unsupported("the driver does not decode H.264"));
        }
        if width > self.max_size.0 || height > self.max_size.1 {
            return Err(Failure::unsupported(format!(
                "{width}x{height} is too large"
            )));
        }
        Ok(Box::new(VaapiDecoder {
            display: Rc::clone(&self.display),
            profiles: self.profiles.clone(),
            max_size: self.max_size,
            front: FrontEnd::new(),
            session: None,
            fit: (0, 0),
            scaler: None,
            // NOSLACKING_VIDEO_GPU_SCALE=0 shrinks on the CPU instead,
            // to compare the two (examples/bench.rs).
            no_scaler: std::env::var_os("NOSLACKING_VIDEO_GPU_SCALE").is_some_and(|v| v == "0"),
        }))
    }
}

/// What a context was made for: when a new SPS changes any of it, the
/// context is made afresh (at an IDR).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Shape {
    profile: i32,
    coded: (u32, u32),
    surfaces: usize,
}

struct Session {
    shape: Shape,
    context: Context,
}

/// One stream's decoder.
struct VaapiDecoder {
    display: Rc<Display>,
    profiles: Vec<i32>,
    max_size: (u32, u32),
    front: FrontEnd,
    session: Option<Session>,
    /// The box pictures should cover; 0×0 for their own size.
    fit: (u32, u32),
    /// Scaling on the GPU, made on first need.
    scaler: Option<Scaler>,
    /// The driver cannot scale: shrink on the CPU.
    no_scaler: bool,
}

impl VaapiDecoder {
    /// The profile to decode `sps` with: the most constrained the driver
    /// has that covers the stream.
    fn profile(&self, sps: &Sps) -> Result<i32, Failure> {
        let wanted: &[i32] = match sps.profile_idc {
            // Baseline (Slack's constrained baseline among it): slice
            // groups and redundant pictures are refused before here, and
            // what is left decodes as Main.
            66 => &[
                va::PROFILE_H264_CONSTRAINED_BASELINE,
                va::PROFILE_H264_MAIN,
                va::PROFILE_H264_HIGH,
            ],
            77 => &[va::PROFILE_H264_MAIN, va::PROFILE_H264_HIGH],
            100 => &[va::PROFILE_H264_HIGH],
            other => return Err(Failure::unsupported(format!("profile_idc {other}"))),
        };
        wanted
            .iter()
            .copied()
            .find(|p| self.profiles.contains(p))
            .ok_or_else(|| Failure::unsupported(format!("profile_idc {}", sps.profile_idc)))
    }

    /// A context fit for `picture`, made afresh if its shape changed.
    fn session_for(&mut self, picture: &Picture<'_>) -> Result<&Session, Failure> {
        let coded = picture.coded_size();
        if coded.0 > self.max_size.0 || coded.1 > self.max_size.1 {
            return Err(Failure::unsupported(format!(
                "{}x{} is larger than the driver decodes",
                coded.0, coded.1
            )));
        }
        let shape = Shape {
            profile: self.profile(&picture.sps)?,
            coded,
            // Every reference, the picture being decoded, and one spare.
            surfaces: usize::from(picture.sps.max_num_ref_frames).max(1) + 2,
        };
        if self.session.as_ref().is_some_and(|s| s.shape == shape) {
            return self
                .session
                .as_ref()
                .ok_or_else(|| Failure::device("no context"));
        }
        if !picture.idr {
            return Err(Failure::need_keyframe(
                "the stream changed shape between IDRs",
            ));
        }
        // The old context and surfaces go before the new are made.
        self.session = None;
        let config = Config::new(&self.display, shape.profile).map_err(Failure::device)?;
        let surfaces = Surfaces::new(&self.display, coded.0, coded.1, shape.surfaces)
            .map_err(Failure::device)?;
        let context = Context::new(config, surfaces, coded.0, coded.1).map_err(Failure::device)?;
        Ok(self.session.insert(Session { shape, context }))
    }

    fn decode_picture(&mut self, picture: &Picture<'_>) -> Result<(u32, Planes), Failure> {
        let crop = picture.visible()?;
        let in_use: Vec<u32> = self.front.surfaces_in_use().collect();
        let display = Rc::clone(&self.display);
        let session = self.session_for(picture)?;
        let target = session
            .context
            .surfaces
            .ids
            .iter()
            .copied()
            .find(|id| !in_use.contains(id))
            .ok_or_else(|| Failure::broken("no free surface"))?;
        let picture_parameters = picture_parameters(picture, target);
        let matrix = iq_matrix(&picture.sps, &picture.pps);
        let slices: Vec<SliceParameterBufferH264> = picture
            .slices
            .iter()
            .map(|slice| slice_parameters(picture, slice))
            .collect::<Result<_, _>>()?;
        let mut buffers: Vec<(BufferType, &[u8])> = vec![
            (BufferType::PictureParameter, picture_parameters.bytes()),
            (BufferType::IqMatrix, matrix.bytes()),
        ];
        for (parameters, slice) in slices.iter().zip(&picture.slices) {
            buffers.push((BufferType::SliceParameter, parameters.bytes()));
            buffers.push((BufferType::SliceData, slice.nal));
        }
        session
            .context
            .render(target, &buffers)
            .map_err(Failure::broken)?;
        let coded = session.shape.coded;
        let shown = (crop.2, crop.3);
        let size = output_size(shown, self.fit);
        if size != shown
            && let Some(planes) = self.scaled(target, crop, size)
        {
            return Ok((target, planes));
        }
        let (y, u, v) = display
            .read_i420(target, coded, crop)
            .map_err(Failure::device)?;
        let planes = Planes {
            width: crop.2,
            height: crop.3,
            y,
            u,
            v,
        };
        // No scaling on the GPU: shrunk here, so the pipe still carries
        // no more than is shown.
        Ok((target, shrink::shrink(planes, self.fit)))
    }

    /// `crop` of decoded surface `target` scaled to `size` on the GPU
    /// and read back; none if the driver cannot, which is then not
    /// tried again for this stream.
    fn scaled(
        &mut self,
        target: u32,
        crop: (u32, u32, u32, u32),
        size: (u32, u32),
    ) -> Option<Planes> {
        if self.scaler.is_none() && !self.no_scaler {
            match Scaler::new(&self.display) {
                Ok(scaler) => self.scaler = Some(scaler),
                Err(why) => {
                    eprintln!("noslacking-video: no scaling on the GPU ({why}): on the CPU");
                    self.no_scaler = true;
                }
            }
        }
        let scaler = self.scaler.as_mut()?;
        let result = scaler.scale(target, crop, size).and_then(|surface| {
            self.display
                .read_i420(surface, size, (0, 0, size.0, size.1))
        });
        match result {
            Ok((y, u, v)) => Some(Planes {
                width: size.0,
                height: size.1,
                y,
                u,
                v,
            }),
            Err(why) => {
                eprintln!("noslacking-video: scaling on the GPU failed ({why}): on the CPU");
                self.scaler = None;
                self.no_scaler = true;
                None
            }
        }
    }
}

impl Decoder for VaapiDecoder {
    fn set_output_size(&mut self, width: u32, height: u32) {
        self.fit = (width, height);
    }

    fn decode(&mut self, frame: &[u8], _keyframe: bool) -> Result<Option<Decoded>, Failure> {
        let picture = match self.front.begin(frame) {
            Ok(Some(picture)) => picture,
            Ok(None) => return Ok(None),
            Err(failure) => {
                if failure.kind != FailKind::Unsupported {
                    self.front.reset();
                }
                return Err(failure);
            }
        };
        match self.decode_picture(&picture) {
            Ok((surface, planes)) => {
                if let Err(failure) = self.front.finish(&picture, surface) {
                    self.front.reset();
                    return Err(failure);
                }
                let (_, _, width, height) = picture.visible()?;
                Ok(Some(Decoded {
                    planes,
                    source: (width, height),
                    hardware: true,
                }))
            }
            Err(failure) => {
                self.front.reset();
                Err(failure)
            }
        }
    }
}

/// A picture as `VAPictureH264` describes it.
fn va_picture(reference: &Reference) -> PictureH264 {
    let (flags, frame_idx) = match reference.long_term {
        Some(index) => (va::PICTURE_H264_LONG_TERM_REFERENCE, index),
        None => (va::PICTURE_H264_SHORT_TERM_REFERENCE, reference.frame_num),
    };
    PictureH264 {
        picture_id: reference.surface,
        frame_idx,
        flags,
        top_field_order_cnt: reference.top_poc,
        bottom_field_order_cnt: reference.bottom_poc,
        reserved: [0; 4],
    }
}

/// `VAPictureParameterBufferH264` for `picture` decoding into `target`.
fn picture_parameters(picture: &Picture<'_>, target: u32) -> PictureParameterBufferH264 {
    let (sps, pps) = (&picture.sps, &picture.pps);
    let mut reference_frames = [PictureH264::invalid(); 16];
    for (slot, reference) in reference_frames.iter_mut().zip(&picture.references) {
        *slot = va_picture(reference);
    }
    PictureParameterBufferH264 {
        curr_pic: PictureH264 {
            picture_id: target,
            frame_idx: picture.frame_num,
            flags: 0,
            top_field_order_cnt: picture.top_poc,
            bottom_field_order_cnt: picture.bottom_poc,
            reserved: [0; 4],
        },
        reference_frames,
        picture_width_in_mbs_minus1: sps.pic_width_in_mbs_minus1,
        // Frames only (fields are refused), so map units are macroblocks.
        picture_height_in_mbs_minus1: sps.pic_height_in_map_units_minus1,
        bit_depth_luma_minus8: sps.bit_depth_luma_minus8,
        bit_depth_chroma_minus8: sps.bit_depth_chroma_minus8,
        num_ref_frames: sps.max_num_ref_frames,
        pad0: 0,
        seq_fields: va::seq_fields(&SeqFields {
            chroma_format_idc: u32::from(sps.chroma_format_idc),
            separate_colour_plane_flag: sps.separate_colour_plane_flag,
            gaps_in_frame_num_value_allowed_flag: sps.gaps_in_frame_num_value_allowed_flag,
            frame_mbs_only_flag: sps.frame_mbs_only_flag,
            mb_adaptive_frame_field_flag: sps.mb_adaptive_frame_field_flag,
            direct_8x8_inference_flag: sps.direct_8x8_inference_flag,
            // A.3.3.2: from level 3.1 on.
            min_luma_bi_pred_size8x8: sps.level_idc as u8 >= 31,
            log2_max_frame_num_minus4: u32::from(sps.log2_max_frame_num_minus4),
            pic_order_cnt_type: u32::from(sps.pic_order_cnt_type),
            log2_max_pic_order_cnt_lsb_minus4: u32::from(sps.log2_max_pic_order_cnt_lsb_minus4),
            delta_pic_order_always_zero_flag: sps.delta_pic_order_always_zero_flag,
        }),
        num_slice_groups_minus1: 0,
        slice_group_map_type: 0,
        slice_group_change_rate_minus1: 0,
        pic_init_qp_minus26: pps.pic_init_qp_minus26,
        pic_init_qs_minus26: pps.pic_init_qs_minus26,
        chroma_qp_index_offset: pps.chroma_qp_index_offset,
        second_chroma_qp_index_offset: pps.second_chroma_qp_index_offset,
        pic_fields: va::pic_fields(&PicFields {
            entropy_coding_mode_flag: pps.entropy_coding_mode_flag,
            weighted_pred_flag: pps.weighted_pred_flag,
            weighted_bipred_idc: u32::from(pps.weighted_bipred_idc),
            transform_8x8_mode_flag: pps.transform_8x8_mode_flag,
            field_pic_flag: false,
            constrained_intra_pred_flag: pps.constrained_intra_pred_flag,
            pic_order_present_flag: pps.bottom_field_pic_order_in_frame_present_flag,
            deblocking_filter_control_present_flag: pps.deblocking_filter_control_present_flag,
            redundant_pic_cnt_present_flag: pps.redundant_pic_cnt_present_flag,
            reference_pic_flag: picture.nal_ref_idc != 0,
        }),
        frame_num: u16::try_from(picture.frame_num).unwrap_or(0),
        pad1: 0,
        reserved: [0; 8],
    }
}

/// Zigzag scan position to raster position, 4×4.
const ZIGZAG_4X4: [usize; 16] = [0, 1, 4, 8, 5, 2, 3, 6, 9, 12, 13, 10, 7, 11, 14, 15];
/// Zigzag scan position to raster position, 8×8.
const ZIGZAG_8X8: [usize; 64] = [
    0, 1, 8, 16, 9, 2, 3, 10, 17, 24, 32, 25, 18, 11, 4, 5, 12, 19, 26, 33, 40, 48, 41, 34, 27, 20,
    13, 6, 7, 14, 21, 28, 35, 42, 49, 56, 57, 50, 43, 36, 29, 22, 15, 23, 30, 37, 44, 51, 58, 59,
    52, 45, 38, 31, 39, 46, 53, 60, 61, 54, 47, 55, 62, 63,
];

/// The scaling lists, in raster order as libva wants: flat (16) unless
/// the stream has its own (High profile only; Slack sends none).
fn iq_matrix(sps: &Sps, pps: &Pps) -> IqMatrixBufferH264 {
    let mut matrix = IqMatrixBufferH264 {
        scaling_list4x4: [[16; 16]; 6],
        scaling_list8x8: [[16; 64]; 2],
        reserved: [0; 4],
    };
    if !sps.seq_scaling_matrix_present_flag && !pps.pic_scaling_matrix_present_flag {
        return matrix;
    }
    for (list, zigzag) in matrix
        .scaling_list4x4
        .iter_mut()
        .zip(&pps.scaling_lists_4x4)
    {
        for (i, &value) in zigzag.iter().enumerate() {
            list[ZIGZAG_4X4[i]] = value;
        }
    }
    for (list, zigzag) in matrix
        .scaling_list8x8
        .iter_mut()
        .zip(&pps.scaling_lists_8x8)
    {
        for (i, &value) in zigzag.iter().enumerate() {
            list[ZIGZAG_8X8[i]] = value;
        }
    }
    matrix
}

/// `VASliceParameterBufferH264` for one slice.
fn slice_parameters(
    picture: &Picture<'_>,
    slice: &crate::h264::SliceToDecode<'_>,
) -> Result<SliceParameterBufferH264, Failure> {
    let header = &slice.header;
    let mut parameters = SliceParameterBufferH264 {
        slice_data_size: u32::try_from(slice.nal.len())
            .map_err(|_| Failure::broken("a slice too large"))?,
        slice_data_bit_offset: u16::try_from(header.header_bit_size)
            .map_err(|_| Failure::broken("a slice header too long"))?,
        first_mb_in_slice: u16::try_from(header.first_mb_in_slice)
            .map_err(|_| Failure::broken("first_mb_in_slice too large"))?,
        slice_type: header.slice_type as u8,
        direct_spatial_mv_pred_flag: u8::from(header.direct_spatial_mv_pred_flag),
        num_ref_idx_l0_active_minus1: header.num_ref_idx_l0_active_minus1,
        num_ref_idx_l1_active_minus1: header.num_ref_idx_l1_active_minus1,
        cabac_init_idc: header.cabac_init_idc,
        slice_qp_delta: header.slice_qp_delta,
        disable_deblocking_filter_idc: header.disable_deblocking_filter_idc,
        slice_alpha_c0_offset_div2: header.slice_alpha_c0_offset_div2,
        slice_beta_offset_div2: header.slice_beta_offset_div2,
        luma_log2_weight_denom: header.pred_weight_table.luma_log2_weight_denom,
        chroma_log2_weight_denom: header.pred_weight_table.chroma_log2_weight_denom,
        ..SliceParameterBufferH264::default()
    };
    if matches!(header.slice_type, SliceType::P) {
        for (slot, entry) in parameters.ref_pic_list0.iter_mut().zip(&slice.ref_list0) {
            if let Some(reference) = entry {
                *slot = va_picture(reference);
            }
        }
        if picture.pps.weighted_pred_flag {
            let table = &header.pred_weight_table;
            parameters.luma_weight_l0_flag = 1;
            parameters.chroma_weight_l0_flag = 1;
            let active = usize::from(header.num_ref_idx_l0_active_minus1) + 1;
            for i in 0..active.min(32) {
                parameters.luma_weight_l0[i] = table.luma_weight_l0[i];
                parameters.luma_offset_l0[i] = i16::from(table.luma_offset_l0[i]);
                for j in 0..2 {
                    parameters.chroma_weight_l0[i][j] = table.chroma_weight_l0[i][j];
                    parameters.chroma_offset_l0[i][j] = i16::from(table.chroma_offset_l0[i][j]);
                }
            }
        }
    }
    Ok(parameters)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Decodes both fixtures on this machine's GPU and compares the
    /// pictures with ffmpeg's (the hashes the software decoder's tests
    /// check too). Needs a VA-API driver with H.264: ignored by default.
    /// `cargo test -p noslacking-video -- --ignored vaapi --nocapture`
    #[test]
    #[ignore = "needs a GPU with VA-API"]
    #[allow(clippy::print_stdout, reason = "the timings are for the reader")]
    fn vaapi_decodes_the_fixtures_as_ffmpeg_does() {
        use sha2::{Digest, Sha256};
        let mut backend = Vaapi::open().expect("VA-API with H.264");
        println!("{}", backend.name());
        for (stream, size, expected) in [
            (
                &include_bytes!("../../../../src/huddle_audio/fixtures/screen-1920x1080.h264")[..],
                (1920, 1080),
                "a2bd2fa8a81ad980725de116dbc10da22367e82186641f63c41bc914e5eb8812",
            ),
            (
                &include_bytes!("../../../../src/huddle_audio/fixtures/camera-480x480.h264")[..],
                (480, 480),
                "e433e34c83ae538aa67adc4fae754ca4afc8892ea91546d6d55ffc97d699e3c2",
            ),
        ] {
            let mut decoder = backend
                .open_decoder(Codec::H264, size.0, size.1)
                .expect("a decoder");
            let mut hash = Sha256::new();
            let started = std::time::Instant::now();
            let frames = crate::h264::tests::frames(stream);
            for frame in &frames {
                let decoded = decoder
                    .decode(frame, false)
                    .expect("decodes")
                    .expect("a picture");
                assert!(decoded.hardware);
                assert_eq!(decoded.source, size);
                let picture = decoded.planes;
                assert_eq!((picture.width, picture.height), size);
                hash.update(&picture.y);
                hash.update(&picture.u);
                hash.update(&picture.v);
            }
            let took = started.elapsed().as_secs_f64() * 1000.0;
            println!(
                "{}x{}: {} frames, {:.2} ms a frame",
                size.0,
                size.1,
                frames.len(),
                took / frames.len() as f64
            );
            let hex: String = hash.finalize().iter().map(|b| format!("{b:02x}")).collect();
            assert_eq!(hex, expected, "{}x{}", size.0, size.1);
        }
    }

    /// Decodes the 1080p share scaled to 960×540 on the GPU (and the
    /// cameras to 240×240) and compares each picture with the software
    /// decoder's (bit-exact with ffmpeg) shrunk on the CPU: scaling
    /// filters differ, so within a small mean difference.
    /// `cargo test -p noslacking-video -- --ignored vaapi --nocapture`
    #[test]
    #[ignore = "needs a GPU with VA-API"]
    #[allow(clippy::print_stdout, reason = "the timings are for the reader")]
    fn vaapi_scales_on_the_gpu_close_to_a_software_shrink() {
        let mut backend = Vaapi::open().expect("VA-API with H.264");
        for (stream, size, fit) in [
            (
                &include_bytes!("../../../../src/huddle_audio/fixtures/screen-1920x1080.h264")[..],
                (1920, 1080),
                (960, 540),
            ),
            (
                &include_bytes!("../../../../src/huddle_audio/fixtures/camera-480x480.h264")[..],
                (480, 480),
                (240, 240),
            ),
        ] {
            let mut decoder = backend
                .open_decoder(Codec::H264, size.0, size.1)
                .expect("a decoder");
            decoder.set_output_size(fit.0, fit.1);
            let mut software = rusty_h264_decoder::Decoder::new();
            let frames = crate::h264::tests::frames(stream);
            let started = std::time::Instant::now();
            let mut worst = 0f64;
            for frame in &frames {
                let decoded = decoder
                    .decode(frame, false)
                    .expect("decodes")
                    .expect("a picture");
                assert_eq!(decoded.source, size);
                let picture = decoded.planes;
                assert_eq!((picture.width, picture.height), fit);
                assert!(picture.check().is_ok());
                let reference = software.decode(frame).expect("decodes").expect("a picture");
                let reference = shrink::shrink(
                    Planes {
                        width: u32::try_from(reference.width).expect("small"),
                        height: u32::try_from(reference.height).expect("small"),
                        y: reference.y,
                        u: reference.u,
                        v: reference.v,
                    },
                    fit,
                );
                for (ours, theirs) in [
                    (&picture.y, &reference.y),
                    (&picture.u, &reference.u),
                    (&picture.v, &reference.v),
                ] {
                    let total: u64 = ours
                        .iter()
                        .zip(theirs.iter())
                        .map(|(a, b)| u64::from(a.abs_diff(*b)))
                        .sum();
                    worst = worst.max(total as f64 / ours.len() as f64);
                }
            }
            let took = started.elapsed().as_secs_f64() * 1000.0;
            println!(
                "{}x{} shown at {}x{}: {:.2} ms a frame with the software reference, \
                 worst mean difference {worst:.2}",
                size.0,
                size.1,
                fit.0,
                fit.1,
                took / frames.len() as f64
            );
            assert!(worst < 4.0, "{worst}");
        }
    }
}
