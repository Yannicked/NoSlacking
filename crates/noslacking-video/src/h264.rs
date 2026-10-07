//! What a stateless H.264 decoder needs worked out before the hardware
//! sees a picture: the parameter sets and slice headers parsed, the
//! picture's order count, the decoded picture buffer's reference
//! marking, and each slice's reference list. VA-API wants all of it
//! (and so do Vulkan Video and V4L2's stateless decoders, which can use
//! this as it is).
//!
//! Parsing is ChromeOS's (`cros-codecs`, its parser only). The rest
//! follows the H.264 specification (clauses 8.2.1, 8.2.4 and 8.2.5) for
//! what Slack sends: progressive frames of I and P slices, as in
//! constrained baseline, with any picture order count type, long-term
//! references and memory management operations. B slices, fields and
//! gaps in `frame_num` are refused, and the app decodes those streams in
//! software. Pictures are given out in decoding order, as soon as they
//! are decoded: with no B slices that is also their display order.

use std::borrow::Cow;
use std::io::Cursor;
use std::rc::Rc;

use cros_codecs::codec::h264::parser::{
    MaxLongTermFrameIdx, Nalu, NaluType, Parser, Pps, RefPicListModification, RefPicMarking,
    SliceHeader, SliceType, Sps,
};

use crate::backend::Failure;

/// A picture in the decoded picture buffer, kept for reference.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Reference {
    /// The back end's handle for it (a VA surface).
    pub surface: u32,
    /// Its `frame_num`.
    pub frame_num: u32,
    /// `FrameNumWrap`, which for frames is also its `PicNum`.
    pub pic_num: i32,
    /// `LongTermFrameIdx` (and `LongTermPicNum`) when used for long-term
    /// reference; none for short-term.
    pub long_term: Option<u32>,
    /// Its top field's order count.
    pub top_poc: i32,
    /// Its bottom field's order count.
    pub bottom_poc: i32,
}

/// One slice of the picture, ready for the hardware.
#[derive(Debug)]
pub struct SliceToDecode<'a> {
    /// The parsed header.
    pub header: SliceHeader,
    /// The whole NAL unit as sent (header byte included, emulation
    /// prevention bytes still in), which is what VA-API takes.
    pub nal: &'a [u8],
    /// `RefPicList0` after its modifications: `num_ref_idx_l0_active`
    /// entries, none where the list has no picture. Empty for I slices.
    pub ref_list0: Vec<Option<Reference>>,
}

/// A picture to decode: everything the hardware is told about it.
#[derive(Debug)]
pub struct Picture<'a> {
    /// Its sequence parameter set.
    pub sps: Rc<Sps>,
    /// Its picture parameter set.
    pub pps: Rc<Pps>,
    /// Whether it is an IDR picture.
    pub idr: bool,
    /// Its NAL units' `nal_ref_idc`; 0 for a picture nothing refers to.
    pub nal_ref_idc: u8,
    /// Its `frame_num`.
    pub frame_num: u32,
    /// Its top field's order count.
    pub top_poc: i32,
    /// Its bottom field's order count.
    pub bottom_poc: i32,
    /// Every picture in the buffer used for reference, short-term first.
    pub references: Vec<Reference>,
    /// Its slices in order.
    pub slices: Vec<SliceToDecode<'a>>,
    marking: RefPicMarking,
    pic_order_cnt_msb: i32,
    pic_order_cnt_lsb: i32,
    frame_num_offset: u32,
}

impl Picture<'_> {
    /// The picture's coded size, in whole macroblocks.
    pub fn coded_size(&self) -> (u32, u32) {
        (self.sps.width(), self.sps.height())
    }

    /// The part of it to show: left, top, width, height.
    pub fn visible(&self) -> Result<(u32, u32, u32, u32), Failure> {
        visible(&self.sps)
    }
}

/// The part of a coded picture to show (its cropping): left, top, width,
/// height, checked to lie inside it.
pub fn visible(sps: &Sps) -> Result<(u32, u32, u32, u32), Failure> {
    let (width, height) = (sps.width(), sps.height());
    if !sps.frame_cropping_flag {
        return Ok((0, 0, width, height));
    }
    // 4:2:0, frames: crop units are two pixels each way.
    let (left, right) = (
        sps.frame_crop_left_offset.saturating_mul(2),
        sps.frame_crop_right_offset.saturating_mul(2),
    );
    let (top, bottom) = (
        sps.frame_crop_top_offset.saturating_mul(2),
        sps.frame_crop_bottom_offset.saturating_mul(2),
    );
    let shown_width = width
        .checked_sub(left.saturating_add(right))
        .filter(|&w| w > 0);
    let shown_height = height
        .checked_sub(top.saturating_add(bottom))
        .filter(|&h| h > 0);
    match (shown_width, shown_height) {
        (Some(w), Some(h)) => Ok((left, top, w, h)),
        _ => Err(Failure::broken("the cropping is larger than the picture")),
    }
}

/// What is remembered of the previous reference picture (for the order
/// count of type 0 and for `frame_num` gaps).
#[derive(Clone, Copy, Debug, Default)]
struct PreviousReference {
    frame_num: u32,
    pic_order_cnt_msb: i32,
    pic_order_cnt_lsb: i32,
    top_poc: i32,
    had_mmco5: bool,
}

/// What is remembered of the previous picture (for order counts of type
/// 1 and 2).
#[derive(Clone, Copy, Debug, Default)]
struct Previous {
    frame_num: u32,
    frame_num_offset: u32,
    had_mmco5: bool,
}

/// One stream's parsing and reference bookkeeping.
#[derive(Default)]
pub struct FrontEnd {
    parser: Parser,
    references: Vec<Reference>,
    previous_reference: PreviousReference,
    previous: Previous,
    /// `MaxLongTermFrameIdx`: none means "no long-term frame indices".
    max_long_term_frame_idx: Option<u32>,
    /// Whether an IDR has been decoded since the start or the last reset.
    started: bool,
}

impl std::fmt::Debug for FrontEnd {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FrontEnd")
            .field("references", &self.references)
            .field("started", &self.started)
            .finish_non_exhaustive()
    }
}

/// The NAL units of one frame, as the parser sees them.
fn nal_units(frame: &[u8]) -> Vec<Nalu<'_>> {
    let mut cursor = Cursor::new(frame);
    let mut units = Vec::new();
    // Each call finds the next start code; it fails when there is none.
    while let Ok(nalu) = Nalu::next(&mut cursor) {
        units.push(nalu);
    }
    units
}

impl FrontEnd {
    /// A front end waiting for its first IDR.
    pub fn new() -> Self {
        Self::default()
    }

    /// The surfaces the buffer holds: the back end must not decode into
    /// these.
    pub fn surfaces_in_use(&self) -> impl Iterator<Item = u32> + '_ {
        self.references.iter().map(|r| r.surface)
    }

    /// Forgets every reference: after a failure, nothing decodes until
    /// the next IDR.
    pub fn reset(&mut self) {
        self.references.clear();
        self.started = false;
    }

    /// Parses `frame` (one access unit) and works out its picture: none
    /// for a frame of parameter sets only.
    pub fn begin<'a>(&mut self, frame: &'a [u8]) -> Result<Option<Picture<'a>>, Failure> {
        let mut slices: Vec<SliceToDecode<'a>> = Vec::new();
        let mut first: Option<(u8, bool)> = None;
        for nalu in nal_units(frame) {
            match nalu.header.type_ {
                NaluType::Sps => {
                    self.parser
                        .parse_sps(&nalu)
                        .map_err(|e| Failure::broken(format!("SPS: {e}")))?;
                }
                NaluType::Pps => {
                    self.parser
                        .parse_pps(&nalu)
                        .map_err(|e| Failure::broken(format!("PPS: {e}")))?;
                }
                NaluType::Slice | NaluType::SliceIdr => {
                    let (ref_idc, idr) = (nalu.header.ref_idc, nalu.header.idr_pic_flag);
                    let data = unit_bytes(&nalu)
                        .ok_or_else(|| Failure::broken("a NAL unit outside its frame"))?;
                    let started = self.started;
                    let slice = self.parser.parse_slice_header(nalu).map_err(|e| {
                        // Joined mid-stream, the parameter sets it names
                        // have not come yet: wait for the keyframe.
                        if started {
                            Failure::broken(format!("slice header: {e}"))
                        } else {
                            Failure::need_keyframe(format!("slice header: {e}"))
                        }
                    })?;
                    if slice.header.first_mb_in_slice == 0 && first.is_some() {
                        return Err(Failure::broken("two pictures in one frame"));
                    }
                    if first.is_none() {
                        if slice.header.first_mb_in_slice != 0 {
                            return Err(Failure::need_keyframe(
                                "the picture's first slice is missing",
                            ));
                        }
                        first = Some((ref_idc, idr));
                    }
                    slices.push(SliceToDecode {
                        header: slice.header,
                        nal: data,
                        ref_list0: Vec::new(),
                    });
                }
                NaluType::SliceDpa | NaluType::SliceDpb | NaluType::SliceDpc => {
                    return Err(Failure::unsupported("data partitioning"));
                }
                // SEI, delimiters, filler and the rest carry nothing the
                // decoder needs.
                _ => {}
            }
        }
        let Some((nal_ref_idc, idr)) = first else {
            return Ok(None);
        };
        if !idr && !self.started {
            return Err(Failure::need_keyframe("no IDR yet"));
        }
        let header = &slices[0].header;
        let pps = Rc::clone(
            self.parser
                .get_pps(header.pic_parameter_set_id)
                .ok_or_else(|| Failure::broken("no such PPS"))?,
        );
        let sps = Rc::clone(&pps.sps);
        check_supported(&sps, &pps, &slices)?;
        let frame_num = u32::from(header.frame_num);
        let max_frame_num = sps.max_frame_num();
        if frame_num >= max_frame_num {
            return Err(Failure::broken("frame_num out of range"));
        }
        if idr && frame_num != 0 {
            return Err(Failure::broken("an IDR with a frame_num"));
        }
        if !idr {
            let previous = self.previous_reference.frame_num;
            if frame_num != previous && frame_num != (previous + 1) % max_frame_num {
                return Err(Failure::need_keyframe(format!(
                    "frame_num jumps from {previous} to {frame_num}: frames are missing"
                )));
            }
        }
        let mut picture = Picture {
            sps: Rc::clone(&sps),
            pps,
            idr,
            nal_ref_idc,
            frame_num,
            top_poc: 0,
            bottom_poc: 0,
            references: Vec::new(),
            slices: Vec::new(),
            marking: header.dec_ref_pic_marking.clone(),
            pic_order_cnt_msb: 0,
            pic_order_cnt_lsb: i32::from(header.pic_order_cnt_lsb),
            frame_num_offset: 0,
        };
        self.order_count(&mut picture, header)?;
        if idr {
            // The IDR empties the buffer before it decodes.
            self.references.clear();
        }
        // FrameNumWrap for each short-term reference (8.2.4.1).
        for reference in &mut self.references {
            if reference.long_term.is_none() {
                let wrap = if reference.frame_num > frame_num {
                    i64::from(reference.frame_num) - i64::from(max_frame_num)
                } else {
                    i64::from(reference.frame_num)
                };
                reference.pic_num = i32::try_from(wrap).unwrap_or(i32::MIN);
            }
        }
        let initial = self.initial_list();
        for slice in &mut slices {
            if matches!(slice.header.slice_type, SliceType::P) {
                slice.ref_list0 = modified_list(
                    &initial,
                    &self.references,
                    &slice.header,
                    frame_num,
                    max_frame_num,
                )?;
                if slice.ref_list0.first().is_none_or(Option::is_none) {
                    return Err(Failure::need_keyframe("a P slice with nothing to refer to"));
                }
            }
        }
        picture.references = self.references.clone();
        picture.slices = slices;
        Ok(Some(picture))
    }

    /// The picture decoded into `surface`: marks references (8.2.5) and
    /// remembers what the next picture's order count needs.
    pub fn finish(&mut self, picture: &Picture<'_>, surface: u32) -> Result<(), Failure> {
        self.started = true;
        let mut current = Reference {
            surface,
            frame_num: picture.frame_num,
            pic_num: i32::try_from(picture.frame_num).unwrap_or(i32::MAX),
            long_term: None,
            top_poc: picture.top_poc,
            bottom_poc: picture.bottom_poc,
        };
        let mut mmco5 = false;
        if picture.nal_ref_idc != 0 {
            if picture.idr {
                self.references.clear();
                if picture.marking.long_term_reference_flag {
                    current.long_term = Some(0);
                    self.max_long_term_frame_idx = Some(0);
                } else {
                    self.max_long_term_frame_idx = None;
                }
            } else if picture.marking.adaptive_ref_pic_marking_mode_flag {
                mmco5 = self.memory_management(picture, &mut current)?;
            } else {
                self.sliding_window(&picture.sps)?;
            }
            let limit = usize::from(picture.sps.max_num_ref_frames).max(1);
            if self.references.len() >= limit {
                return Err(Failure::broken("more references than the stream allows"));
            }
        }
        if mmco5 {
            // 8.2.1: after operation 5 the picture counts as frame 0 at
            // order 0.
            let lowest = current.top_poc.min(current.bottom_poc);
            current.top_poc -= lowest;
            current.bottom_poc -= lowest;
            current.frame_num = 0;
            current.pic_num = 0;
        }
        if picture.nal_ref_idc != 0 {
            self.references.push(current);
            self.previous_reference = PreviousReference {
                frame_num: current.frame_num,
                pic_order_cnt_msb: if mmco5 { 0 } else { picture.pic_order_cnt_msb },
                pic_order_cnt_lsb: if mmco5 {
                    current.top_poc
                } else {
                    picture.pic_order_cnt_lsb
                },
                top_poc: current.top_poc,
                had_mmco5: mmco5,
            };
        }
        self.previous = Previous {
            frame_num: if mmco5 { 0 } else { picture.frame_num },
            frame_num_offset: if mmco5 { 0 } else { picture.frame_num_offset },
            had_mmco5: mmco5,
        };
        Ok(())
    }

    /// The picture's order count (8.2.1).
    fn order_count(&self, picture: &mut Picture<'_>, header: &SliceHeader) -> Result<(), Failure> {
        let sps = &picture.sps;
        let max_frame_num = i64::from(sps.max_frame_num());
        let frame_num = i64::from(picture.frame_num);
        let clamp = |n: i64| i32::try_from(n).map_err(|_| Failure::broken("order count overflows"));
        let frame_num_offset = || -> i64 {
            let previous_offset = if self.previous.had_mmco5 {
                0
            } else {
                i64::from(self.previous.frame_num_offset)
            };
            if picture.idr {
                0
            } else if i64::from(self.previous.frame_num) > frame_num {
                previous_offset + max_frame_num
            } else {
                previous_offset
            }
        };
        let (top, bottom) = match sps.pic_order_cnt_type {
            0 => {
                let (previous_msb, previous_lsb) = if picture.idr {
                    (0, 0)
                } else if self.previous_reference.had_mmco5 {
                    (0, self.previous_reference.top_poc)
                } else {
                    (
                        self.previous_reference.pic_order_cnt_msb,
                        self.previous_reference.pic_order_cnt_lsb,
                    )
                };
                let max_lsb = 1i64 << (u32::from(sps.log2_max_pic_order_cnt_lsb_minus4) + 4);
                let lsb = i64::from(header.pic_order_cnt_lsb);
                let (previous_msb, previous_lsb) =
                    (i64::from(previous_msb), i64::from(previous_lsb));
                let msb = if lsb < previous_lsb && previous_lsb - lsb >= max_lsb / 2 {
                    previous_msb + max_lsb
                } else if lsb > previous_lsb && lsb - previous_lsb > max_lsb / 2 {
                    previous_msb - max_lsb
                } else {
                    previous_msb
                };
                picture.pic_order_cnt_msb = clamp(msb)?;
                let top = msb + lsb;
                (top, top + i64::from(header.delta_pic_order_cnt_bottom))
            }
            1 => {
                let offset = frame_num_offset();
                picture.frame_num_offset = u32::try_from(offset).unwrap_or(0);
                let cycle = i64::from(sps.num_ref_frames_in_pic_order_cnt_cycle);
                let mut absolute = if cycle != 0 { offset + frame_num } else { 0 };
                if picture.nal_ref_idc == 0 && absolute > 0 {
                    absolute -= 1;
                }
                let mut expected = 0i64;
                if absolute > 0 {
                    let cycles = (absolute - 1) / cycle;
                    let in_cycle = usize::try_from((absolute - 1) % cycle).unwrap_or(0);
                    expected = cycles * i64::from(sps.expected_delta_per_pic_order_cnt_cycle);
                    for offset in sps.offset_for_ref_frame.iter().take(in_cycle + 1) {
                        expected += i64::from(*offset);
                    }
                }
                if picture.nal_ref_idc == 0 {
                    expected += i64::from(sps.offset_for_non_ref_pic);
                }
                let top = expected + i64::from(header.delta_pic_order_cnt[0]);
                let bottom = top
                    + i64::from(sps.offset_for_top_to_bottom_field)
                    + i64::from(header.delta_pic_order_cnt[1]);
                (top, bottom)
            }
            2 => {
                let offset = frame_num_offset();
                picture.frame_num_offset = u32::try_from(offset).unwrap_or(0);
                let count = if picture.idr {
                    0
                } else if picture.nal_ref_idc == 0 {
                    2 * (offset + frame_num) - 1
                } else {
                    2 * (offset + frame_num)
                };
                (count, count)
            }
            _ => return Err(Failure::broken("unknown picture order count type")),
        };
        picture.top_poc = clamp(top)?;
        picture.bottom_poc = clamp(bottom)?;
        Ok(())
    }

    /// `RefPicList0` before modification, for P slices of frames
    /// (8.2.4.2.1): short-term references by descending `PicNum`, then
    /// long-term ones by ascending `LongTermPicNum`.
    fn initial_list(&self) -> Vec<Reference> {
        let mut short: Vec<Reference> = self
            .references
            .iter()
            .filter(|r| r.long_term.is_none())
            .copied()
            .collect();
        short.sort_by_key(|r| std::cmp::Reverse(r.pic_num));
        let mut long: Vec<Reference> = self
            .references
            .iter()
            .filter(|r| r.long_term.is_some())
            .copied()
            .collect();
        long.sort_by_key(|r| r.long_term);
        short.extend(long);
        short
    }

    /// Sliding window marking (8.2.5.3): the oldest short-term reference
    /// goes when the buffer is full.
    fn sliding_window(&mut self, sps: &Sps) -> Result<(), Failure> {
        let limit = usize::from(sps.max_num_ref_frames).max(1);
        while self.references.len() >= limit {
            let oldest = self
                .references
                .iter()
                .enumerate()
                .filter(|(_, r)| r.long_term.is_none())
                .min_by_key(|(_, r)| r.pic_num)
                .map(|(i, _)| i)
                .ok_or_else(|| Failure::broken("the buffer is full of long-term references"))?;
            self.references.remove(oldest);
        }
        Ok(())
    }

    /// Adaptive marking (8.2.5.4); whether operation 5 was among them.
    fn memory_management(
        &mut self,
        picture: &Picture<'_>,
        current: &mut Reference,
    ) -> Result<bool, Failure> {
        let current_pic_num = i64::from(picture.frame_num);
        let short_term_with = |references: &[Reference], difference_minus1: u32| {
            let pic_num = current_pic_num - (i64::from(difference_minus1) + 1);
            references
                .iter()
                .position(|r| r.long_term.is_none() && i64::from(r.pic_num) == pic_num)
        };
        let mut mmco5 = false;
        for operation in &picture.marking.inner {
            match operation.memory_management_control_operation {
                0 => break,
                1 => {
                    if let Some(i) =
                        short_term_with(&self.references, operation.difference_of_pic_nums_minus1)
                    {
                        self.references.remove(i);
                    }
                }
                2 => {
                    self.references
                        .retain(|r| r.long_term != Some(operation.long_term_pic_num));
                }
                3 => {
                    let index = operation.long_term_frame_idx;
                    let Some(i) =
                        short_term_with(&self.references, operation.difference_of_pic_nums_minus1)
                    else {
                        continue;
                    };
                    let surface = self.references[i].surface;
                    self.references
                        .retain(|r| r.long_term != Some(index) || r.surface == surface);
                    if let Some(reference) =
                        self.references.iter_mut().find(|r| r.surface == surface)
                    {
                        reference.long_term = Some(index);
                    }
                }
                4 => {
                    let max = match operation.max_long_term_frame_idx {
                        MaxLongTermFrameIdx::NoLongTermFrameIndices => None,
                        MaxLongTermFrameIdx::Idx(i) => Some(i),
                    };
                    self.max_long_term_frame_idx = max;
                    self.references.retain(|r| match (r.long_term, max) {
                        (None, _) => true,
                        (Some(_), None) => false,
                        (Some(index), Some(max)) => index <= max,
                    });
                }
                5 => {
                    self.references.clear();
                    self.max_long_term_frame_idx = None;
                    mmco5 = true;
                }
                6 => {
                    let index = operation.long_term_frame_idx;
                    self.references.retain(|r| r.long_term != Some(index));
                    current.long_term = Some(index);
                }
                _ => return Err(Failure::broken("unknown memory management operation")),
            }
        }
        if let (Some(index), max) = (current.long_term, self.max_long_term_frame_idx)
            && max.is_none_or(|max| index > max)
        {
            return Err(Failure::broken("a long-term index over the maximum"));
        }
        Ok(mmco5)
    }
}

/// What the parser accepts but this decoder does not: the app decodes
/// such a stream in software.
fn check_supported(sps: &Sps, pps: &Pps, slices: &[SliceToDecode<'_>]) -> Result<(), Failure> {
    if !sps.frame_mbs_only_flag {
        return Err(Failure::unsupported("interlaced video"));
    }
    if sps.chroma_format_idc != 1
        || sps.bit_depth_luma_minus8 != 0
        || sps.bit_depth_chroma_minus8 != 0
    {
        return Err(Failure::unsupported("not 8-bit 4:2:0"));
    }
    if pps.num_slice_groups_minus1 > 0 {
        return Err(Failure::unsupported("slice groups"));
    }
    for slice in slices {
        if !matches!(slice.header.slice_type, SliceType::P | SliceType::I) {
            return Err(Failure::unsupported(format!(
                "{:?} slices",
                slice.header.slice_type
            )));
        }
        if slice.header.field_pic_flag {
            return Err(Failure::unsupported("field pictures"));
        }
    }
    Ok(())
}

/// A P slice's `RefPicList0` after its modifications (8.2.4.3), cut to
/// `num_ref_idx_l0_active` entries.
fn modified_list(
    initial: &[Reference],
    references: &[Reference],
    header: &SliceHeader,
    current_frame_num: u32,
    max_frame_num: u32,
) -> Result<Vec<Option<Reference>>, Failure> {
    let active = usize::from(header.num_ref_idx_l0_active_minus1) + 1;
    let mut list: Vec<Option<Reference>> = initial.iter().copied().map(Some).collect();
    list.resize(active, None);
    if !header.ref_pic_list_modification_flag_l0 {
        return Ok(list);
    }
    let max_pic_num = i64::from(max_frame_num);
    let current_pic_num = i64::from(current_frame_num);
    let mut predicted = current_pic_num;
    for (index, modification) in header.ref_pic_list_modification_l0.iter().enumerate() {
        let RefPicListModification {
            modification_of_pic_nums_idc: idc,
            abs_diff_pic_num_minus1,
            long_term_pic_num,
            ..
        } = *modification;
        let wanted = match idc {
            0 | 1 => {
                let difference = i64::from(abs_diff_pic_num_minus1) + 1;
                let no_wrap = if idc == 0 {
                    let n = predicted - difference;
                    if n < 0 { n + max_pic_num } else { n }
                } else {
                    let n = predicted + difference;
                    if n >= max_pic_num { n - max_pic_num } else { n }
                };
                predicted = no_wrap;
                let pic_num = if no_wrap > current_pic_num {
                    no_wrap - max_pic_num
                } else {
                    no_wrap
                };
                references
                    .iter()
                    .find(|r| r.long_term.is_none() && i64::from(r.pic_num) == pic_num)
                    .copied()
                    .ok_or_else(|| Failure::need_keyframe("a reference that is not there"))?
            }
            2 => references
                .iter()
                .find(|r| r.long_term == Some(long_term_pic_num))
                .copied()
                .ok_or_else(|| Failure::need_keyframe("a long-term reference that is not there"))?,
            3 => break,
            _ => return Err(Failure::broken("unknown list modification")),
        };
        if index >= active {
            return Err(Failure::broken("more list modifications than entries"));
        }
        // Put it at `index`, then drop its other copy further on.
        list.insert(index, Some(wanted));
        let mut kept = index + 1;
        for position in index + 1..list.len() {
            let same = list[position].is_some_and(|r| {
                r.long_term == wanted.long_term
                    && (wanted.long_term.is_some() || r.pic_num == wanted.pic_num)
            });
            if !same {
                list[kept] = list[position];
                kept += 1;
            }
        }
        list.truncate(active);
    }
    Ok(list)
}

/// A NAL unit's own bytes (header byte on, start code off), borrowed
/// from the frame it was found in.
fn unit_bytes<'a>(nalu: &Nalu<'a>) -> Option<&'a [u8]> {
    match &nalu.data {
        Cow::Borrowed(data) => {
            let data: &'a [u8] = data;
            data.get(nalu.offset..nalu.offset.checked_add(nalu.size)?)
        }
        Cow::Owned(_) => None,
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    const SCREEN: &[u8] =
        include_bytes!("../../../src/huddle_audio/fixtures/screen-1920x1080.h264");
    const CAMERA: &[u8] = include_bytes!("../../../src/huddle_audio/fixtures/camera-480x480.h264");
    const PATTERN: &[u8] =
        include_bytes!("../../../src/huddle_audio/fixtures/test-pattern-320x180.h264");

    /// The stream cut into frames the way the app hands them over: each
    /// ends with its slice, parameter sets going with the slice after.
    pub(crate) fn frames(stream: &[u8]) -> Vec<Vec<u8>> {
        let mut frames = Vec::new();
        let mut frame = Vec::new();
        for nalu in nal_units(stream) {
            frame.extend_from_slice(&[0, 0, 0, 1]);
            let data = unit_bytes(&nalu).expect("borrowed");
            frame.extend_from_slice(data);
            if matches!(nalu.header.type_, NaluType::Slice | NaluType::SliceIdr) {
                frames.push(std::mem::take(&mut frame));
            }
        }
        frames
    }

    /// Runs a whole stream through the front end as if each picture were
    /// decoded into its own surface, checking what the hardware would be
    /// told.
    fn walk(stream: &[u8]) -> (usize, (u32, u32), (u32, u32, u32, u32)) {
        let mut front = FrontEnd::new();
        let mut count = 0;
        let mut size = (0, 0);
        let mut shown = (0, 0, 0, 0);
        let mut last_poc = i32::MIN;
        for (n, frame) in frames(stream).iter().enumerate() {
            let picture = front.begin(frame).expect("parses").expect("a picture");
            size = picture.coded_size();
            shown = picture.visible().expect("crop inside");
            assert!(
                picture.top_poc > last_poc || picture.idr,
                "frame {n} goes forward"
            );
            last_poc = picture.top_poc;
            for slice in &picture.slices {
                assert_eq!(slice.nal[0] & 0x1f, if picture.idr { 5 } else { 1 });
                if matches!(slice.header.slice_type, SliceType::P) {
                    // The previous frame is the first choice.
                    let first = slice.ref_list0[0].expect("a reference");
                    assert_eq!(first.surface as usize, n - 1, "frame {n}");
                }
            }
            let surface = u32::try_from(n).expect("small");
            front.finish(&picture, surface).expect("marks");
            assert!(front.references.len() <= usize::from(picture.sps.max_num_ref_frames).max(1));
            count += 1;
        }
        (count, size, shown)
    }

    #[test]
    fn the_fixtures_walk_through_with_one_picture_a_frame() {
        assert_eq!(walk(SCREEN), (36, (1920, 1088), (0, 0, 1920, 1080)));
        assert_eq!(walk(CAMERA), (66, (480, 480), (0, 0, 480, 480)));
        let (count, _, shown) = walk(PATTERN);
        assert!(count > 0);
        assert_eq!((shown.2, shown.3), (320, 180));
    }

    #[test]
    fn nothing_decodes_before_an_idr_or_after_missing_frames() {
        let frames = frames(CAMERA);
        let mut front = FrontEnd::new();
        let error = front.begin(&frames[3]).expect_err("a P frame first");
        assert_eq!(error.kind, noslacking_video_ipc::FailKind::NeedKeyframe);
        let picture = front
            .begin(&frames[0])
            .expect("the IDR")
            .expect("a picture");
        front.finish(&picture, 0).expect("marks");
        let picture = front.begin(&frames[1]).expect("next").expect("a picture");
        front.finish(&picture, 1).expect("marks");
        // Frames 2 and 3 lost.
        let error = front.begin(&frames[4]).expect_err("a gap");
        assert_eq!(error.kind, noslacking_video_ipc::FailKind::NeedKeyframe);
        // A reset waits for the next IDR, frame 44.
        front.reset();
        assert!(front.begin(&frames[45]).is_err());
        assert!(front.begin(&frames[44]).expect("the IDR").is_some());
    }

    #[test]
    fn garbage_is_an_error_or_nothing_never_a_panic() {
        let frames = frames(CAMERA);
        let mut front = FrontEnd::new();
        for junk in [
            &[][..],
            &[0, 0, 1],
            &[0, 0, 0, 1, 0x65],
            &[0, 0, 0, 1, 0x67, 0xff, 0xff, 0xff],
            &[0, 0, 1, 0x68, 0x00],
            &[0, 0, 1, 0x41, 0x9a, 0x00, 0x11],
        ] {
            let _ = front.begin(junk);
        }
        // Cut frames.
        for frame in frames.iter().take(3) {
            for cut in [5, 10, 20, frame.len() / 2] {
                let mut front = FrontEnd::new();
                let _ = front.begin(&frame[..cut.min(frame.len())]);
            }
        }
        let mut front = FrontEnd::new();
        assert!(front.begin(&frames[0]).expect("recovers").is_some());
    }

    #[test]
    fn list_modifications_move_a_reference_to_the_front() {
        let reference = |surface: u32, pic_num: i32| Reference {
            surface,
            frame_num: u32::try_from(pic_num).expect("positive"),
            pic_num,
            long_term: None,
            top_poc: pic_num * 2,
            bottom_poc: pic_num * 2,
        };
        let references = [reference(0, 3), reference(1, 4), reference(2, 5)];
        let initial = vec![references[2], references[1], references[0]];
        let header = SliceHeader {
            num_ref_idx_l0_active_minus1: 2,
            ref_pic_list_modification_flag_l0: true,
            ref_pic_list_modification_l0: vec![
                // CurrPicNum 6, minus 3: PicNum 3 first.
                RefPicListModification {
                    modification_of_pic_nums_idc: 0,
                    abs_diff_pic_num_minus1: 2,
                    ..Default::default()
                },
                RefPicListModification {
                    modification_of_pic_nums_idc: 3,
                    ..Default::default()
                },
            ],
            ..Default::default()
        };
        let list = modified_list(&initial, &references, &header, 6, 16).expect("modifies");
        let surfaces: Vec<Option<u32>> = list.iter().map(|r| r.map(|r| r.surface)).collect();
        assert_eq!(surfaces, [Some(0), Some(2), Some(1)]);
        // A modification naming a picture not in the buffer.
        let header = SliceHeader {
            ref_pic_list_modification_l0: vec![RefPicListModification {
                modification_of_pic_nums_idc: 0,
                abs_diff_pic_num_minus1: 0,
                ..Default::default()
            }],
            ..header
        };
        assert!(modified_list(&initial, &references[..1], &header, 6, 16).is_err());
        // Without modifications, the list is padded to its length.
        let plain = SliceHeader {
            num_ref_idx_l0_active_minus1: 4,
            ..Default::default()
        };
        let list = modified_list(&initial, &references, &plain, 6, 16).expect("as is");
        assert_eq!(list.len(), 5);
        assert!(list[3].is_none() && list[4].is_none());
    }
}
