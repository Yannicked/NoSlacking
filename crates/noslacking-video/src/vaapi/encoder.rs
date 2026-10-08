//! H.264 encoding on the GPU through VA-API, set up as WebRTC's
//! single-layer H.264 and the app's software encoder are: constrained
//! baseline (profile 66 with constraint_set1, CAVLC, no B-frames), one
//! reference, one slice a picture (packetization-mode 1 splits it into
//! RTP packets), every picture out as soon as it is in. IDRs come when
//! asked and at least every [`IDR_EVERY_SECONDS`], each with the SPS and
//! PPS in front of it. The rate is kept by the driver (CBR where it has
//! it, else VBR), and a new rate takes effect at the next picture without
//! a keyframe.
//!
//! The headers are ours ([`crate::nal`]), handed to the driver packed
//! wherever it takes packed sequence and slice headers, as ffmpeg and
//! GStreamer do: Mesa's drivers need them (they write each slice's NAL
//! header from what they parse out of ours, and no SPS or PPS without
//! them). A driver that takes none writes its own from the parameter
//! buffers, and ours go in front of an IDR that came without any. Every
//! picture's NAL units are checked before it goes out.

use std::rc::Rc;

use noslacking_video_ipc::Planes;

use super::va::enc::{
    self, CodedBuffer, EncPicFields, EncPictureParameterBufferH264, EncSeqFields,
    EncSequenceParameterBufferH264, EncSliceParameterBufferH264, FrameRate, Hrd, Misc,
    PackedHeaderParameterBuffer, RateControl, RcFlags, VuiFields,
};
use super::va::{self, BufferType, Config, Context, Display, Parameter, PictureH264, Surfaces};
use crate::backend::{Encoded, Encoder, Failure};
use crate::nal;

/// A keyframe at least this often, in seconds, as the software encoder.
pub const IDR_EVERY_SECONDS: u32 = 4;
/// `log2_max_frame_num_minus4`: frame numbers count to 256 and wrap.
const LOG2_MAX_FRAME_NUM_MINUS4: u32 = 4;
/// The bit rates an encoder is opened or set to, in bit/s: what any
/// huddle sends lies well inside.
const BITRATE_RANGE: (u32, u32) = (50_000, 50_000_000);

/// What the driver offers for encoding constrained baseline H.264.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EncodeSupport {
    /// The entry point: low power where there is one.
    pub entrypoint: i32,
    /// `VA_RC_CBR` or `VA_RC_VBR`.
    pub rate_control: u32,
    /// The packed headers we give (`VA_ENC_PACKED_HEADER_*`): the
    /// sequence (SPS and PPS) and slice headers, or none.
    pub packed_headers: u32,
    /// The largest picture.
    pub max_size: (u32, u32),
}

impl EncodeSupport {
    /// Whether `display` encodes constrained baseline H.264, and how.
    pub fn query(display: &Display) -> Option<Self> {
        let profile = va::PROFILE_H264_CONSTRAINED_BASELINE;
        if !display.profiles().ok()?.contains(&profile) {
            return None;
        }
        let entrypoints = display.entrypoints(profile).ok()?;
        let entrypoint = [enc::ENTRYPOINT_ENC_SLICE_LP, enc::ENTRYPOINT_ENC_SLICE]
            .into_iter()
            .find(|e| entrypoints.contains(e))?;
        let values = display
            .attributes(
                profile,
                entrypoint,
                &[
                    enc::ATTRIB_RATE_CONTROL,
                    enc::ATTRIB_ENC_PACKED_HEADERS,
                    va::ATTRIB_MAX_PICTURE_WIDTH,
                    va::ATTRIB_MAX_PICTURE_HEIGHT,
                ],
            )
            .ok()?;
        let known = |i: usize| {
            values
                .get(i)
                .copied()
                .filter(|&v| v != va::ATTRIB_NOT_SUPPORTED)
        };
        let modes = known(0)?;
        let rate_control = choose_rate_control(modes)?;
        let packed_headers = choose_packed_headers(known(1).unwrap_or(0));
        let side = |i: usize, fallback: u32| {
            known(i)
                .filter(|&v| v > 0)
                .unwrap_or(fallback)
                .min(noslacking_video_ipc::MAX_SIDE)
        };
        Some(Self {
            entrypoint,
            rate_control,
            packed_headers,
            max_size: (side(2, 1920), side(3, 1088)),
        })
    }
}

/// CBR where the driver has it (what WebRTC's encoders do: a steady
/// rate the bandwidth estimate can trust), else VBR; none without
/// either.
pub fn choose_rate_control(modes: u32) -> Option<u32> {
    [enc::RC_CBR, enc::RC_VBR]
        .into_iter()
        .find(|&mode| modes & mode != 0)
}

/// Packed sequence and slice headers wherever the driver takes both
/// (`offered`, its `VAConfigAttribEncPackedHeaders`), else none.
pub fn choose_packed_headers(offered: u32) -> u32 {
    let wanted = enc::PACKED_HEADER_SEQUENCE | enc::PACKED_HEADER_SLICE;
    if offered & wanted == wanted {
        wanted
    } else {
        0
    }
}

/// The H.264 level for `size` at `fps`: the lowest of 3.1 (the
/// software encoder's), 3.2, 4.0 and 4.2 whose frame size and macroblock
/// rate cover it; none past 4.2.
pub fn level(size: (u32, u32), fps: u32) -> Option<u8> {
    let (across, down) = (
        u64::from(size.0.div_ceil(16)),
        u64::from(size.1.div_ceil(16)),
    );
    let mbs = across * down;
    let rate = mbs * u64::from(fps);
    // Table A-1: (level_idc, MaxFS, MaxMBPS).
    [
        (31, 3600, 108_000),
        (32, 5120, 216_000),
        (40, 8192, 245_760),
        (42, 8704, 522_240),
    ]
    .into_iter()
    .find(|&(_, max_fs, max_mbps)| {
        // A.3.1 (f): neither side past sqrt(8 × MaxFS) macroblocks.
        mbs <= max_fs
            && rate <= max_mbps
            && across * across <= 8 * max_fs
            && down * down <= 8 * max_fs
    })
    .map(|(idc, _, _)| idc)
}

/// The reference the next P picture predicts from.
#[derive(Clone, Copy, Debug)]
struct Reference {
    surface: u32,
    frame_num: u32,
    poc: i32,
}

/// One stream's encoder on the GPU. Fields drop in order: the coded
/// buffer before the context that made it.
pub struct VaapiEncoder {
    coded: CodedBuffer,
    context: Context,
    display: Rc<Display>,
    support: EncodeSupport,
    /// The pictures' size.
    size: (u32, u32),
    /// Rounded up to whole macroblocks: the surfaces' size.
    coded_size: (u32, u32),
    fps: u32,
    bitrate: u32,
    level: u8,
    /// The surface pictures are uploaded into.
    input: u32,
    /// The two reconstructed pictures: one the reference, one the next.
    recon: [u32; 2],
    reference: Option<Reference>,
    /// Pictures since the last IDR (that one 0).
    since_idr: u32,
    idr_pic_id: u16,
    /// The rate control parameters go with the next picture.
    send_rate: bool,
    /// The rate was changed since the last picture: the driver's rate
    /// control starts over at the new rate.
    rate_changed: bool,
}

impl VaapiEncoder {
    /// An encoder of `size` pictures at `fps` and `bitrate` bit/s.
    pub fn new(
        display: &Rc<Display>,
        support: EncodeSupport,
        size: (u32, u32),
        fps: u32,
        bitrate: u32,
    ) -> Result<Self, Failure> {
        let (width, height) = size;
        if width < 16 || height < 16 || width % 2 != 0 || height % 2 != 0 {
            return Err(Failure::unsupported(format!("{width}x{height}")));
        }
        if width > support.max_size.0 || height > support.max_size.1 {
            return Err(Failure::unsupported(format!(
                "{width}x{height} is larger than the driver encodes"
            )));
        }
        let fps = fps.clamp(1, 60);
        let level = level(size, fps).ok_or_else(|| {
            Failure::unsupported(format!("{width}x{height} at {fps} fps is past level 4.2"))
        })?;
        let coded_size = (width.next_multiple_of(16), height.next_multiple_of(16));
        let mut attributes = vec![(enc::ATTRIB_RATE_CONTROL, support.rate_control)];
        if support.packed_headers != 0 {
            attributes.push((enc::ATTRIB_ENC_PACKED_HEADERS, support.packed_headers));
        }
        let config = Config::with_attributes(
            display,
            va::PROFILE_H264_CONSTRAINED_BASELINE,
            support.entrypoint,
            &attributes,
        )
        .map_err(Failure::device)?;
        let surfaces =
            Surfaces::new(display, coded_size.0, coded_size.1, 3).map_err(Failure::device)?;
        let ids = surfaces.ids.clone();
        let context =
            Context::new(config, surfaces, coded_size.0, coded_size.1).map_err(Failure::device)?;
        // No picture's stream comes near its raw size; at least 256 KiB
        // for the smallest.
        let raw = coded_size.0 * coded_size.1 * 3 / 2;
        let coded = context
            .coded_buffer(raw.max(256 << 10))
            .map_err(Failure::device)?;
        Ok(Self {
            coded,
            context,
            display: Rc::clone(display),
            support,
            size,
            coded_size,
            fps,
            bitrate: bitrate.clamp(BITRATE_RANGE.0, BITRATE_RANGE.1),
            level,
            input: ids[0],
            recon: [ids[1], ids[2]],
            reference: None,
            since_idr: 0,
            idr_pic_id: 0,
            send_rate: true,
            rate_changed: false,
        })
    }

    fn idr_period(&self) -> u32 {
        self.fps * IDR_EVERY_SECONDS
    }

    fn sequence_parameters(&self) -> EncSequenceParameterBufferH264 {
        let mbs = (self.coded_size.0 / 16, self.coded_size.1 / 16);
        // Cropping is in chroma samples, 2 pixels each way.
        let crop = (
            (self.coded_size.0 - self.size.0) / 2,
            (self.coded_size.1 - self.size.1) / 2,
        );
        EncSequenceParameterBufferH264 {
            seq_parameter_set_id: 0,
            level_idc: self.level,
            pad0: 0,
            intra_period: self.idr_period(),
            intra_idr_period: self.idr_period(),
            ip_period: 1,
            bits_per_second: self.bitrate,
            max_num_ref_frames: 1,
            picture_width_in_mbs: u16::try_from(mbs.0).unwrap_or(u16::MAX),
            picture_height_in_mbs: u16::try_from(mbs.1).unwrap_or(u16::MAX),
            seq_fields: enc::enc_seq_fields(&EncSeqFields {
                chroma_format_idc: 1,
                frame_mbs_only_flag: true,
                direct_8x8_inference_flag: true,
                log2_max_frame_num_minus4: LOG2_MAX_FRAME_NUM_MINUS4,
                pic_order_cnt_type: 2,
                log2_max_pic_order_cnt_lsb_minus4: 0,
            }),
            bit_depth_luma_minus8: 0,
            bit_depth_chroma_minus8: 0,
            num_ref_frames_in_pic_order_cnt_cycle: 0,
            pad1: 0,
            offset_for_non_ref_pic: 0,
            offset_for_top_to_bottom_field: 0,
            offset_for_ref_frame: [0; 256],
            frame_cropping_flag: u8::from(crop != (0, 0)),
            pad2: [0; 3],
            frame_crop_left_offset: 0,
            frame_crop_right_offset: crop.0,
            frame_crop_top_offset: 0,
            frame_crop_bottom_offset: crop.1,
            vui_parameters_present_flag: 1,
            pad3: [0; 3],
            vui_fields: enc::vui_fields(&VuiFields {
                timing_info_present_flag: true,
                bitstream_restriction_flag: true,
                log2_max_mv_length_horizontal: 15,
                log2_max_mv_length_vertical: 15,
                fixed_frame_rate_flag: false,
                motion_vectors_over_pic_boundaries_flag: true,
            }),
            aspect_ratio_idc: 0,
            pad4: [0; 3],
            sar_width: 0,
            sar_height: 0,
            num_units_in_tick: 1,
            time_scale: self.fps * 2,
            reserved: [0; 4],
        }
    }

    fn rate_parameters(&self) -> (Misc<RateControl>, Misc<FrameRate>, Misc<Hrd>) {
        let rate = RateControl {
            bits_per_second: self.bitrate,
            // VBR aims at the rate asked for, which is also its most.
            target_percentage: 100,
            // The rate is kept over a second.
            window_size: 1000,
            initial_qp: 0,
            min_qp: 0,
            basic_unit_size: 0,
            rc_flags: enc::rc_flags(&RcFlags {
                reset: self.rate_changed,
                // A skipped picture would leave a gap the app does not
                // know about; padding wastes the link on a still screen.
                disable_frame_skip: true,
                disable_bit_stuffing: true,
            }),
            icq_quality_factor: 0,
            // A coarser picture rather than a burst over the link.
            max_qp: 51,
            quality_factor: 0,
            target_frame_size: 0,
            reserved: [0; 4],
        };
        // Half a second of buffer: an IDR may take several pictures'
        // worth, a P picture not much more than its share.
        let buffer = self.bitrate / 2;
        (
            Misc::rate_control(rate),
            Misc::frame_rate(FrameRate {
                framerate: self.fps,
                framerate_flags: 0,
                reserved: [0; 4],
            }),
            Misc::hrd(Hrd {
                initial_buffer_fullness: buffer / 4 * 3,
                buffer_size: buffer,
                reserved: [0; 4],
            }),
        )
    }

    /// The SPS and PPS, from the same values the driver is given.
    fn parameter_sets(&self) -> Vec<u8> {
        let crop = (
            (self.coded_size.0 - self.size.0) / 2,
            (self.coded_size.1 - self.size.1) / 2,
        );
        let sps = nal::Sps {
            level_idc: self.level,
            width_in_mbs: self.coded_size.0 / 16,
            height_in_mbs: self.coded_size.1 / 16,
            log2_max_frame_num_minus4: LOG2_MAX_FRAME_NUM_MINUS4,
            crop_right: crop.0,
            crop_bottom: crop.1,
            fps: self.fps,
        };
        [nal::sps(&sps), nal::pps()].concat()
    }
}

impl VaapiEncoder {
    /// The surface it encodes from, `coded_size()` large: a picture put
    /// there (video processing does, for a shared screen) is encoded by
    /// [`Self::encode_input`].
    pub fn input(&self) -> u32 {
        self.input
    }

    /// The pictures' size.
    pub fn size(&self) -> (u32, u32) {
        self.size
    }

    /// The input surface's size: the pictures', rounded up to whole
    /// macroblocks.
    pub fn coded_size(&self) -> (u32, u32) {
        self.coded_size
    }

    /// Writes `picture` (I420, of the encoder's size) into the input
    /// surface.
    pub fn write(&mut self, picture: &Planes) -> Result<(), Failure> {
        if (picture.width, picture.height) != self.size {
            return Err(Failure::broken(format!(
                "a {}x{} picture for a {}x{} encoder",
                picture.width, picture.height, self.size.0, self.size.1
            )));
        }
        self.display
            .write_i420(self.input, self.coded_size, picture)
            .map_err(Failure::device)
    }

    /// Encodes what the input surface holds, an IDR if `force_keyframe`
    /// (or one is due).
    pub fn encode_input(&mut self, force_keyframe: bool) -> Result<Encoded, Failure> {
        let idr = force_keyframe || self.reference.is_none() || self.since_idr >= self.idr_period();
        if idr {
            self.since_idr = 0;
            self.idr_pic_id = self.idr_pic_id.wrapping_add(1);
        }
        let frame_num = self.since_idr % (1 << (LOG2_MAX_FRAME_NUM_MINUS4 + 4));
        // Picture order type 2: twice the pictures since the IDR.
        let poc = i32::try_from(self.since_idr.wrapping_mul(2)).unwrap_or(0);
        let target = match self.reference {
            Some(reference) if !idr && reference.surface == self.recon[0] => self.recon[1],
            _ => self.recon[0],
        };
        let current = PictureH264 {
            picture_id: target,
            frame_idx: frame_num,
            flags: 0,
            top_field_order_cnt: poc,
            bottom_field_order_cnt: poc,
            reserved: [0; 4],
        };
        let reference = if idr { None } else { self.reference };
        let mut reference_frames = [PictureH264::invalid(); 16];
        let mut slice = EncSliceParameterBufferH264 {
            macroblock_address: 0,
            num_macroblocks: (self.coded_size.0 / 16) * (self.coded_size.1 / 16),
            slice_type: if idr { 2 } else { 0 },
            idr_pic_id: self.idr_pic_id,
            ..EncSliceParameterBufferH264::default()
        };
        if let Some(reference) = reference {
            let entry = PictureH264 {
                picture_id: reference.surface,
                frame_idx: reference.frame_num,
                flags: va::PICTURE_H264_SHORT_TERM_REFERENCE,
                top_field_order_cnt: reference.poc,
                bottom_field_order_cnt: reference.poc,
                reserved: [0; 4],
            };
            reference_frames[0] = entry;
            slice.ref_pic_list0[0] = entry;
        }
        let picture_parameters = EncPictureParameterBufferH264 {
            curr_pic: current,
            reference_frames,
            coded_buf: self.coded.id,
            pic_parameter_set_id: 0,
            seq_parameter_set_id: 0,
            last_picture: 0,
            pad0: 0,
            frame_num: u16::try_from(frame_num).unwrap_or(0),
            pic_init_qp: 26,
            num_ref_idx_l0_active_minus1: 0,
            num_ref_idx_l1_active_minus1: 0,
            chroma_qp_index_offset: 0,
            second_chroma_qp_index_offset: 0,
            pad1: 0,
            pic_fields: enc::enc_pic_fields(&EncPicFields {
                idr_pic_flag: idr,
                reference_pic_flag: 1,
                entropy_coding_mode_flag: false,
                deblocking_filter_control_present_flag: false,
            }),
            reserved: [0; 4],
        };
        let sequence = self.sequence_parameters();
        let (rate, frame_rate, hrd) = self.rate_parameters();
        let packing = self.support.packed_headers != 0;
        let header = |kind, bit_length| PackedHeaderParameterBuffer {
            kind,
            bit_length,
            has_emulation_bytes: 1,
            pad0: [0; 3],
            reserved: [0; 4],
        };
        let packed_sequence = (packing && idr).then(|| {
            let sets = self.parameter_sets();
            let bits = u32::try_from(sets.len() * 8).unwrap_or(0);
            (header(enc::PACKED_HEADER_TYPE_SEQUENCE, bits), sets)
        });
        let packed_slice = packing.then(|| {
            let (data, bits) = nal::slice_header(&nal::Slice {
                idr,
                frame_num,
                log2_max_frame_num_minus4: LOG2_MAX_FRAME_NUM_MINUS4,
                idr_pic_id: u32::from(self.idr_pic_id),
            });
            (header(enc::PACKED_HEADER_TYPE_SLICE, bits), data)
        });
        let mut buffers: Vec<(BufferType, &[u8])> = Vec::new();
        if idr {
            buffers.push((BufferType::EncSequenceParameter, sequence.bytes()));
        }
        if idr || self.send_rate {
            buffers.push((BufferType::EncMiscParameter, rate.bytes()));
            buffers.push((BufferType::EncMiscParameter, frame_rate.bytes()));
            buffers.push((BufferType::EncMiscParameter, hrd.bytes()));
        }
        buffers.push((BufferType::EncPictureParameter, picture_parameters.bytes()));
        if let Some((parameters, data)) = &packed_sequence {
            buffers.push((BufferType::EncPackedHeaderParameter, parameters.bytes()));
            buffers.push((BufferType::EncPackedHeaderData, data));
        }
        if let Some((parameters, data)) = &packed_slice {
            buffers.push((BufferType::EncPackedHeaderParameter, parameters.bytes()));
            buffers.push((BufferType::EncPackedHeaderData, data));
        }
        buffers.push((BufferType::EncSliceParameter, slice.bytes()));
        let rendered = self.context.render(self.input, &buffers);
        drop(buffers);
        if let Err(why) = rendered {
            // The reference may not have been written: start over.
            self.reference = None;
            return Err(Failure::device(why));
        }
        let data = match self.coded.read() {
            Ok(data) => data,
            Err(why) => {
                self.reference = None;
                return Err(Failure::device(why));
            }
        };
        let types = noslacking_video_ipc::h264::nal_types(&data);
        if !types.iter().any(|&t| t == 1 || t == 5) {
            self.reference = None;
            return Err(Failure::device(format!(
                "the driver gave no slice ({} bytes: {:02x?})",
                data.len(),
                &data[..data.len().min(48)]
            )));
        }
        let keyframe = types.contains(&5);
        if keyframe != idr || (idr && !types.starts_with(&[7, 8]) && !types.contains(&7)) {
            self.reference = None;
            return Err(Failure::device(format!(
                "the driver's picture is not what was asked (IDR {idr}, NAL units {types:?})"
            )));
        }
        let data = if idr && !types.contains(&7) {
            // The driver wrote no parameter sets: ours go in front.
            let mut whole = self.parameter_sets();
            whole.extend_from_slice(&data);
            whole
        } else {
            data
        };
        self.reference = Some(Reference {
            surface: target,
            frame_num,
            poc,
        });
        self.since_idr += 1;
        self.send_rate = false;
        self.rate_changed = false;
        Ok(Encoded { keyframe, data })
    }
}

impl Encoder for VaapiEncoder {
    fn encode(&mut self, picture: &Planes, force_keyframe: bool) -> Result<Encoded, Failure> {
        self.write(picture)?;
        self.encode_input(force_keyframe)
    }

    fn set_bitrate(&mut self, bitrate: u32) -> Result<(), Failure> {
        let bitrate = bitrate.clamp(BITRATE_RANGE.0, BITRATE_RANGE.1);
        if bitrate != self.bitrate {
            self.bitrate = bitrate;
            self.send_rate = true;
            self.rate_changed = true;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn levels_cover_the_size_and_rate() {
        assert_eq!(level((640, 480), 30), Some(31));
        assert_eq!(level((1280, 720), 30), Some(31));
        assert_eq!(level((1280, 720), 60), Some(32));
        assert_eq!(level((1920, 1080), 15), Some(40));
        assert_eq!(level((1920, 1080), 30), Some(40));
        assert_eq!(level((1920, 1080), 60), Some(42));
        assert_eq!(level((2048, 1088), 30), Some(42));
        // Past 4.2: too many macroblocks, or a side too long.
        assert_eq!(level((2560, 1440), 15), None);
        assert_eq!(level((8192, 16), 15), None);
    }

    /// The fixtures' pictures, decoded by the software decoder: what
    /// the encoder is given.
    pub(crate) fn source(stream: &[u8]) -> Vec<Planes> {
        let mut decoder = rusty_h264_decoder::Decoder::new();
        crate::h264::tests::frames(stream)
            .iter()
            .filter_map(|frame| decoder.decode(frame).expect("decodes"))
            .map(|p| Planes {
                width: u32::try_from(p.width).expect("small"),
                height: u32::try_from(p.height).expect("small"),
                y: p.y,
                u: p.u,
                v: p.v,
            })
            .collect()
    }

    /// Luma PSNR of `a` against `b`, in dB.
    pub(crate) fn psnr(a: &[u8], b: &[u8]) -> f64 {
        let mse = a
            .iter()
            .zip(b)
            .map(|(&x, &y)| (f64::from(x) - f64::from(y)).powi(2))
            .sum::<f64>()
            / a.len() as f64;
        if mse == 0.0 {
            99.0
        } else {
            10.0 * (255.0 * 255.0 / mse).log10()
        }
    }

    /// Encodes the camera and 1080p share fixtures on this machine's GPU
    /// and decodes the stream again with the software decoder: every
    /// picture comes back at its size and close to what went in; the
    /// first is an IDR with SPS and PPS, constrained baseline; a forced
    /// IDR comes when asked, and a new bit rate takes without one.
    /// `cargo test -p noslacking-video -- --ignored vaapi --nocapture`;
    /// `NOSLACKING_VIDEO_DUMP=dir` also writes the streams there.
    #[test]
    #[ignore = "needs a GPU with VA-API"]
    #[allow(clippy::print_stdout, reason = "the numbers are for the reader")]
    fn vaapi_encodes_what_the_software_decoder_reads_back() {
        let display = Display::open().expect("VA-API");
        let support = EncodeSupport::query(&display).expect("H.264 encoding");
        println!("{} {support:?}", display.vendor());
        for (name, stream, fps, bitrate) in [
            (
                "camera",
                &include_bytes!("../../../../src/huddle_audio/fixtures/camera-480x480.h264")[..],
                30,
                900_000,
            ),
            (
                "screen",
                &include_bytes!("../../../../src/huddle_audio/fixtures/screen-1920x1080.h264")[..],
                15,
                2_500_000,
            ),
        ] {
            let pictures = source(stream);
            let size = (pictures[0].width, pictures[0].height);
            let mut encoder =
                VaapiEncoder::new(&display, support, size, fps, bitrate).expect("an encoder");
            let mut decoder = rusty_h264_decoder::Decoder::new();
            let mut out = Vec::new();
            let mut worst = 99.0f64;
            let mut total = 0.0;
            let started = std::time::Instant::now();
            let mut keyframes = Vec::new();
            // Bytes in the first and second half, at the two rates; the
            // first second's IDRs left out.
            let mut halves = [0usize; 2];
            for (n, picture) in pictures.iter().enumerate() {
                if n == pictures.len() / 2 {
                    encoder.set_bitrate(bitrate / 2).expect("set");
                }
                let encoded = encoder.encode(picture, n == 10).expect("encodes");
                let types = noslacking_video_ipc::h264::nal_types(&encoded.data);
                if encoded.keyframe {
                    keyframes.push(n);
                    assert!(types.starts_with(&[7, 8]), "{name} {n}: {types:?}");
                    let at = encoded
                        .data
                        .windows(4)
                        .position(|w| w[..3] == [0, 0, 1] && w[3] & 0x1f == 7)
                        .expect("an SPS");
                    assert_eq!(encoded.data[at + 4], 66, "baseline");
                    assert_eq!(encoded.data[at + 5] & 0x40, 0x40, "constraint_set1");
                } else {
                    assert!(
                        !types.contains(&7) && types.contains(&1),
                        "{name} {n}: {types:?}"
                    );
                }
                out.extend_from_slice(&encoded.data);
                if n >= 12 {
                    halves[usize::from(n >= pictures.len() / 2)] += encoded.data.len();
                }
                let decoded = decoder
                    .decode(&encoded.data)
                    .expect("decodes")
                    .expect("a picture");
                assert_eq!(
                    (decoded.width, decoded.height),
                    (picture.width as usize, picture.height as usize)
                );
                let quality = psnr(&decoded.y, &picture.y);
                worst = worst.min(quality);
                total += quality;
            }
            let took = started.elapsed().as_secs_f64() * 1000.0;
            let seconds = pictures.len() as f64 / f64::from(fps);
            let half = pictures.len() / 2;
            let rate = |bytes: usize, pictures: usize| {
                bytes as f64 * 8.0 * f64::from(fps) / pictures as f64 / 1000.0
            };
            println!(
                "{name} {}x{}: {} pictures, {:.2} ms each with the software decoding, \
                 {:.0} kbit/s ({:.0} at {} then {:.0} at {}), luma PSNR mean {:.1} worst \
                 {worst:.1} dB, keyframes {keyframes:?}",
                size.0,
                size.1,
                pictures.len(),
                took / pictures.len() as f64,
                out.len() as f64 * 8.0 / seconds / 1000.0,
                rate(halves[0], half - 12),
                bitrate / 1000,
                rate(halves[1], pictures.len() - half),
                bitrate / 2000,
                total / pictures.len() as f64,
            );
            if let Some(dir) = std::env::var_os("NOSLACKING_VIDEO_DUMP") {
                std::fs::write(
                    std::path::Path::new(&dir).join(format!("{name}.h264")),
                    &out,
                )
                .expect("written");
            }
            assert_eq!(keyframes, [0, 10], "{name}");
            assert!(worst > 28.0, "{name}: {worst:.1} dB");
            assert!(
                rate(halves[1], pictures.len() - half) < f64::from(bitrate) / 1000.0 * 0.75,
                "{name}: the lower rate holds"
            );
        }
    }

    #[test]
    fn rate_control_prefers_cbr() {
        assert_eq!(choose_rate_control(0x416), Some(enc::RC_CBR));
        assert_eq!(choose_rate_control(enc::RC_VBR | 0x10), Some(enc::RC_VBR));
        assert_eq!(choose_rate_control(0x10), None, "constant QP only");
    }
}
