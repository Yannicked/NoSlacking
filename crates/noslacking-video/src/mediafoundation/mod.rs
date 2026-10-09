//! The Media Foundation back end (Windows): H.264 decoded by Windows'
//! own decoder on the GPU through DXVA, and encoded by the GPU vendor's
//! encoder (NVIDIA's, AMD's, Intel's, Qualcomm's), whichever the system
//! lists first.
//!
//! Decoding (`decoder`) feeds Annex B frames to Microsoft's H.264
//! decoder transform with a Direct3D 11 device behind it, in low-latency
//! mode, takes NV12 pictures in GPU memory, and reads only the ones the
//! app wants back as I420, shrunk on the processor to the output box. It
//! is used only where the GPU decodes: Media Foundation's own software
//! path is not wanted when rusty_h264 is there behind it. Encoding
//! (`encoder`) drives a hardware encoder transform, most of which work
//! asynchronously (they say when they want a picture and when one is
//! encoded), set up as the VA-API encoder is: constrained baseline,
//! no B-frames, CBR, low latency, IDRs when asked and every
//! [`IDR_EVERY_SECONDS`].
//!
//! Built and checked by cross-compiling, not yet run on a Windows
//! machine's GPU. What is here outside `cfg(windows)` is the arithmetic
//! around the API (sizes, planes, NAL units, error codes), tested on
//! every system.

#[cfg(windows)]
#[allow(unsafe_code)]
pub mod decoder;
#[cfg(windows)]
#[allow(unsafe_code)]
pub mod encoder;
#[cfg(windows)]
#[allow(unsafe_code)]
pub mod mf;

use noslacking_video_ipc::{FailKind, Planes, chroma_size, h264};

use crate::backend::{Encoded, Failure};

/// A keyframe at least this often, in seconds, as the other encoders.
pub const IDR_EVERY_SECONDS: u32 = 4;
/// The bit rates an encoder is opened or set to, in bit/s, as VA-API's.
pub const BITRATE_RANGE: (u32, u32) = (50_000, 50_000_000);
/// The largest picture encoded: what level 4.2 holds landscape, as the
/// VA-API encoder offers (a share is 1080p at most anyway).
pub const ENCODE_MAX: (u32, u32) = (2048, 1088);
/// The sizes the decoder is asked about, largest first; the first the
/// GPU takes is what it decodes up to.
pub const DECODE_SIZES: [(u32, u32); 3] = [(4096, 2304), (2560, 1600), (1920, 1088)];

/// The HRESULTs the back end tells apart, as `i32`s (what
/// `windows_core::HRESULT` holds), so the mapping is tested everywhere.
pub mod hresult {
    // Written unsigned, as Windows' headers do; the cast keeps the bits.
    /// The transform wants another frame before it gives a picture.
    pub const NEED_MORE_INPUT: i32 = 0xC00D_6D72_u32 as i32;
    /// The output's format changed: it must be set again.
    pub const STREAM_CHANGE: i32 = 0xC00D_6D61_u32 as i32;
    /// The transform takes no input until its output is taken.
    pub const NOT_ACCEPTING: i32 = 0xC00D_36B5_u32 as i32;
    /// A format it cannot take.
    pub const INVALID_MEDIA_TYPE: i32 = 0xC00D_36B4_u32 as i32;
    /// A format was not set.
    pub const TYPE_NOT_SET: i32 = 0xC00D_6D60_u32 as i32;
    /// The Direct3D device is not one it works with.
    pub const UNSUPPORTED_D3D_TYPE: i32 = 0xC00D_6D76_u32 as i32;
    /// A hardware transform did not start.
    pub const HW_FAILED_START: i32 = 0xC00D_3704_u32 as i32;
    /// The GPU was removed (a driver update, a crash, an eGPU unplugged).
    pub const DEVICE_REMOVED: i32 = 0x887A_0005_u32 as i32;
    /// The GPU hung.
    pub const DEVICE_HUNG: i32 = 0x887A_0006_u32 as i32;
    /// The GPU was reset.
    pub const DEVICE_RESET: i32 = 0x887A_0007_u32 as i32;
    /// Out of memory.
    pub const OUT_OF_MEMORY: i32 = 0x8007_000E_u32 as i32;
    /// Not implemented.
    pub const NOT_IMPLEMENTED: i32 = 0x8000_4001_u32 as i32;
}

/// What a failed call's HRESULT means for the app: the GPU gone or out of
/// memory is the device's failure (software from then on), a format or
/// device it will not take is unsupported, anything else a broken frame.
pub fn failure_kind(code: i32) -> FailKind {
    match code {
        hresult::DEVICE_REMOVED
        | hresult::DEVICE_HUNG
        | hresult::DEVICE_RESET
        | hresult::OUT_OF_MEMORY
        | hresult::HW_FAILED_START => FailKind::Device,
        hresult::INVALID_MEDIA_TYPE
        | hresult::TYPE_NOT_SET
        | hresult::UNSUPPORTED_D3D_TYPE
        | hresult::NOT_IMPLEMENTED => FailKind::Unsupported,
        _ => FailKind::Broken,
    }
}

/// Two 32-bit numbers in one attribute, as Media Foundation keeps a
/// frame's size (width high) and rate (numerator high).
pub fn pack(high: u32, low: u32) -> u64 {
    (u64::from(high) << 32) | u64::from(low)
}

/// The two numbers [`pack`] put together.
pub fn unpack(value: u64) -> (u32, u32) {
    // The halves, each exactly 32 bits.
    ((value >> 32) as u32, (value & 0xFFFF_FFFF) as u32)
}

/// What part of a decoded `frame`-sized picture is shown: the display
/// aperture the decoder gives, `(x, y, width, height)`, if it lies inside
/// the frame and has a size; else the whole frame.
pub fn visible(frame: (u32, u32), aperture: Option<(i32, i32, i32, i32)>) -> (u32, u32, u32, u32) {
    let whole = (0, 0, frame.0, frame.1);
    let Some((x, y, w, h)) = aperture else {
        return whole;
    };
    let (Ok(x), Ok(y), Ok(w), Ok(h)) = (
        u32::try_from(x),
        u32::try_from(y),
        u32::try_from(w),
        u32::try_from(h),
    ) else {
        return whole;
    };
    let inside = w > 0
        && h > 0
        && x.checked_add(w).is_some_and(|r| r <= frame.0)
        && y.checked_add(h).is_some_and(|b| b <= frame.1);
    if inside { (x, y, w, h) } else { whole }
}

/// The bytes an NV12 picture of `width`×`height` takes with rows of
/// `pitch` bytes: the luma's rows, then half as many of interleaved
/// chroma.
pub fn nv12_len(pitch: usize, height: u32) -> Option<usize> {
    let rows = usize::try_from(height).ok()?;
    pitch.checked_mul(rows.checked_add(rows.div_ceil(2))?)
}

/// `crop` (`x, y, width, height`, `x` and `y` even) of an NV12 picture
/// whose luma has `rows` rows of `pitch` bytes, the chroma right after
/// it, as I420. None if `data` is too short for it.
pub fn nv12_to_i420(
    data: &[u8],
    pitch: usize,
    rows: u32,
    crop: (u32, u32, u32, u32),
) -> Option<Planes> {
    let n = |v: u32| usize::try_from(v).ok();
    let (x, y, width, height) = (n(crop.0)?, n(crop.1)?, n(crop.2)?, n(crop.3)?);
    let rows = n(rows)?;
    let (cw, ch) = chroma_size(crop.2, crop.3);
    let (cw, ch) = (n(cw)?, n(ch)?);
    let chroma_at = pitch.checked_mul(rows)?;
    if width == 0
        || height == 0
        || !x.is_multiple_of(2)
        || !y.is_multiple_of(2)
        || x.checked_add(width)? > pitch
        || y.checked_add(height)? > rows
    {
        return None;
    }
    let last_chroma_row = chroma_at.checked_add((y / 2 + ch - 1).checked_mul(pitch)?)?;
    if data.len() < last_chroma_row.checked_add(x + cw * 2)? {
        return None;
    }
    let mut luma = Vec::with_capacity(width * height);
    for row in data[y * pitch..].chunks(pitch).take(height) {
        luma.extend_from_slice(row.get(x..x + width)?);
    }
    let mut u = Vec::with_capacity(cw * ch);
    let mut v = Vec::with_capacity(cw * ch);
    for row in data[chroma_at + (y / 2) * pitch..].chunks(pitch).take(ch) {
        for [first, second] in row.get(x..x + cw * 2)?.as_chunks::<2>().0 {
            u.push(*first);
            v.push(*second);
        }
    }
    Some(Planes {
        width: crop.2,
        height: crop.3,
        y: luma,
        u,
        v,
    })
}

/// `picture` (even-sized) as NV12 with rows of its own width, into `out`
/// ([`nv12_len`] of it long); whether it fit.
pub fn i420_to_nv12(picture: &Planes, out: &mut [u8]) -> bool {
    let (Ok(width), Ok(height)) = (
        usize::try_from(picture.width),
        usize::try_from(picture.height),
    ) else {
        return false;
    };
    let (cw, _) = chroma_size(picture.width, picture.height);
    let cw = usize::try_from(cw).unwrap_or(0);
    let luma = width * height;
    if !picture.width.is_multiple_of(2)
        || !picture.height.is_multiple_of(2)
        || picture.y.len() < luma
        || picture.u.len() < luma / 4
        || picture.v.len() < luma / 4
        || nv12_len(width, picture.height) != Some(out.len())
    {
        return false;
    }
    let (y, chroma) = out.split_at_mut(luma);
    y.copy_from_slice(&picture.y[..luma]);
    for ((pair, u), v) in chroma
        .as_chunks_mut::<2>()
        .0
        .iter_mut()
        .zip(&picture.u[..cw * (height / 2)])
        .zip(&picture.v[..cw * (height / 2)])
    {
        *pair = [*u, *v];
    }
    true
}

/// The SPS and PPS NAL units of `data` (Annex B), each with its start
/// code; empty if it has none.
pub fn parameter_sets(data: &[u8]) -> Vec<u8> {
    let mut sets = Vec::new();
    for unit in h264::nal_units(data) {
        if matches!(h264::nal_type(unit), Some(h264::SPS | h264::PPS)) {
            sets.extend_from_slice(&h264::START);
            sets.extend_from_slice(unit);
        }
    }
    sets
}

/// An encoder's output for one picture made ready to send: its NAL units
/// with four-byte start codes, access unit delimiters left out (the app
/// packetizes slices and parameter sets only), and the parameter sets
/// in front of an IDR that came without them (from `sets`, the last ones
/// seen, which a new pair replaces). Fails if it holds no slice, or an
/// IDR comes with no parameter sets known.
pub fn finish_frame(data: &[u8], sets: &mut Vec<u8>) -> Result<Encoded, Failure> {
    const DELIMITER: u8 = 9;
    let units = h264::nal_units(data);
    let types: Vec<u8> = units.iter().filter_map(|u| h264::nal_type(u)).collect();
    if !types.iter().any(|&t| t == h264::SLICE || t == h264::IDR) {
        return Err(Failure::device(format!(
            "the encoder gave no slice (NAL units {types:?})"
        )));
    }
    let keyframe = types.contains(&h264::IDR);
    let fresh = parameter_sets(data);
    if !fresh.is_empty() {
        *sets = fresh;
    }
    let mut out = Vec::with_capacity(data.len() + sets.len() + 8);
    if keyframe && !types.contains(&h264::SPS) {
        if sets.is_empty() {
            return Err(Failure::device("an IDR without parameter sets"));
        }
        out.extend_from_slice(sets);
    }
    for unit in units {
        if h264::nal_type(unit) != Some(DELIMITER) {
            out.extend_from_slice(&h264::START);
            out.extend_from_slice(unit);
        }
    }
    Ok(Encoded {
        keyframe,
        data: out,
    })
}

/// Whether `frame` (Annex B) has a slice in it, so a picture should come
/// of it, and whether that slice is an IDR's.
pub fn slices(frame: &[u8]) -> (bool, bool) {
    let types = h264::nal_types(frame);
    (
        types.iter().any(|&t| t == h264::SLICE || t == h264::IDR),
        types.contains(&h264::IDR),
    )
}

/// The frame count after which a keyframe is due: [`IDR_EVERY_SECONDS`]
/// at `fps`.
pub fn idr_interval(fps: u32) -> u32 {
    fps.max(1).saturating_mul(IDR_EVERY_SECONDS)
}

#[cfg(windows)]
pub use decoder::MediaFoundation;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes_and_rates_pack_high_then_low() {
        assert_eq!(pack(1920, 1080), 0x0000_0780_0000_0438);
        assert_eq!(unpack(pack(1920, 1080)), (1920, 1080));
        assert_eq!(unpack(pack(u32::MAX, 0)), (u32::MAX, 0));
    }

    #[test]
    fn errors_map_to_what_the_app_does_next() {
        assert_eq!(failure_kind(hresult::DEVICE_REMOVED), FailKind::Device);
        assert_eq!(failure_kind(hresult::OUT_OF_MEMORY), FailKind::Device);
        assert_eq!(
            failure_kind(hresult::INVALID_MEDIA_TYPE),
            FailKind::Unsupported
        );
        // E_FAIL, what a decoder says of a frame it could not read.
        assert_eq!(failure_kind(0x8000_4005_u32 as i32), FailKind::Broken);
    }

    #[test]
    fn the_aperture_crops_when_it_fits() {
        assert_eq!(
            visible((1920, 1088), Some((0, 0, 1920, 1080))),
            (0, 0, 1920, 1080)
        );
        assert_eq!(visible((1920, 1088), None), (0, 0, 1920, 1088));
        // Past the frame, or empty, or negative: the whole frame.
        for aperture in [(0, 0, 1921, 1080), (0, 0, 0, 1080), (-2, 0, 100, 100)] {
            assert_eq!(visible((1920, 1088), Some(aperture)), (0, 0, 1920, 1088));
        }
    }

    /// An NV12 picture of `width`×`rows` with rows of `pitch`: luma
    /// counting up along each row, chroma U = row, V = column.
    fn nv12(width: usize, rows: usize, pitch: usize) -> Vec<u8> {
        let mut data = vec![0xEE; pitch * (rows + rows / 2)];
        for r in 0..rows {
            for c in 0..width {
                data[r * pitch + c] = (r * 16 + c) as u8;
            }
        }
        for r in 0..rows / 2 {
            for c in 0..width / 2 {
                data[pitch * rows + r * pitch + c * 2] = r as u8;
                data[pitch * rows + r * pitch + c * 2 + 1] = 100 + c as u8;
            }
        }
        data
    }

    #[test]
    fn nv12_is_cropped_and_split_into_planes() {
        let data = nv12(8, 6, 16);
        let planes = nv12_to_i420(&data, 16, 6, (2, 2, 4, 2)).expect("inside");
        assert_eq!((planes.width, planes.height), (4, 2));
        assert_eq!(planes.y, [34, 35, 36, 37, 50, 51, 52, 53]);
        assert_eq!(planes.u, [1, 1]);
        assert_eq!(planes.v, [101, 102]);
        let whole = nv12_to_i420(&data, 16, 6, (0, 0, 8, 6)).expect("whole");
        assert_eq!(whole.u.len(), 12);
        assert!(whole.check().is_ok());
    }

    #[test]
    fn nv12_outside_its_buffer_is_refused() {
        let data = nv12(8, 6, 16);
        assert!(nv12_to_i420(&data, 16, 6, (0, 0, 8, 8)).is_none());
        assert!(nv12_to_i420(&data, 16, 6, (1, 0, 4, 4)).is_none());
        assert!(nv12_to_i420(&data[..100], 16, 6, (0, 0, 8, 6)).is_none());
        assert!(nv12_to_i420(&data, 16, 6, (0, 0, 0, 6)).is_none());
    }

    #[test]
    fn i420_goes_to_nv12_and_back() {
        let picture = Planes {
            width: 4,
            height: 2,
            y: (0..8).collect(),
            u: vec![10, 11],
            v: vec![20, 21],
        };
        let mut out = vec![0; nv12_len(4, 2).expect("small")];
        assert!(i420_to_nv12(&picture, &mut out));
        assert_eq!(out, [0, 1, 2, 3, 4, 5, 6, 7, 10, 20, 11, 21]);
        assert_eq!(
            nv12_to_i420(&out, 4, 2, (0, 0, 4, 2)),
            Some(picture.clone())
        );
        let mut short = vec![0; 11];
        assert!(!i420_to_nv12(&picture, &mut short));
    }

    /// `units` (header byte, then a payload byte) as Annex B.
    fn stream(units: &[u8]) -> Vec<u8> {
        units.iter().flat_map(|&t| [0, 0, 0, 1, t, 0xAA]).collect()
    }

    #[test]
    fn idrs_go_out_with_parameter_sets_and_without_delimiters() {
        let mut sets = Vec::new();
        // An IDR with its SPS and PPS (and an AUD, as Intel's encoder
        // writes): kept, the AUD dropped.
        let first = finish_frame(&stream(&[0x09, 0x67, 0x68, 0x65]), &mut sets).expect("an IDR");
        assert!(first.keyframe);
        assert_eq!(first.data, stream(&[0x67, 0x68, 0x65]));
        assert_eq!(sets, stream(&[0x67, 0x68]));
        // A later IDR without them gets the last ones in front.
        let later = finish_frame(&stream(&[0x65]), &mut sets).expect("an IDR");
        assert_eq!(later.data, stream(&[0x67, 0x68, 0x65]));
        let other = finish_frame(&stream(&[0x41]), &mut sets).expect("a P frame");
        assert!(!other.keyframe);
        assert_eq!(other.data, stream(&[0x41]));
    }

    #[test]
    fn frames_without_slices_or_sets_are_refused() {
        let mut sets = Vec::new();
        assert!(finish_frame(&stream(&[0x67, 0x68]), &mut sets).is_err());
        assert!(finish_frame(&stream(&[0x65]), &mut sets).is_err());
    }

    #[test]
    fn slices_are_told_from_parameter_sets() {
        assert_eq!(slices(&stream(&[0x67, 0x68])), (false, false));
        assert_eq!(slices(&stream(&[0x67, 0x68, 0x65])), (true, true));
        assert_eq!(slices(&stream(&[0x41])), (true, false));
    }

    #[test]
    fn keyframes_are_due_every_four_seconds() {
        assert_eq!(idr_interval(30), 120);
        assert_eq!(idr_interval(0), 4);
    }
}
