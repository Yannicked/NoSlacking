//! H.264 between the pipe's Annex B (a start code before each NAL unit,
//! the parameter sets in band) and the AVCC form VideoToolbox speaks
//! (each NAL unit behind its length, big-endian; the SPS and PPS kept
//! apart, in the stream's format description). Plain code, so it is
//! tested on every system.

use noslacking_video_ipc::h264::{IDR, PPS, SLICE, SPS, START, nal_type, nal_units};

/// A NAL unit's type: supplemental enhancement information, kept with
/// the slices it describes.
const SEI: u8 = 6;

/// One Annex B frame taken apart for a decoder that wants AVCC.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AccessUnit<'a> {
    /// The last SPS in it, without its start code.
    pub sps: Option<&'a [u8]>,
    /// The last PPS in it, without its start code.
    pub pps: Option<&'a [u8]>,
    /// Whether it holds an IDR slice.
    pub idr: bool,
    /// Its slices (and their SEI) each behind a four-byte length; empty
    /// for a frame of parameter sets only. Delimiters, filler and the
    /// like are left out: the decoder needs none of them.
    pub avcc: Vec<u8>,
}

/// `frame` (Annex B) taken apart: its parameter sets, and the rest as
/// AVCC with four-byte lengths. A NAL unit too long for a length (4 GiB)
/// cannot come over the pipe.
pub fn split(frame: &[u8]) -> AccessUnit<'_> {
    let mut unit = AccessUnit {
        sps: None,
        pps: None,
        idr: false,
        avcc: Vec::with_capacity(frame.len() + 16),
    };
    for nal in nal_units(frame) {
        match nal_type(nal) {
            Some(SPS) => unit.sps = Some(nal),
            Some(PPS) => unit.pps = Some(nal),
            Some(kind @ (SLICE..=IDR | SEI)) => {
                unit.idr |= kind == IDR;
                let Ok(length) = u32::try_from(nal.len()) else {
                    continue;
                };
                unit.avcc.extend_from_slice(&length.to_be_bytes());
                unit.avcc.extend_from_slice(nal);
            }
            _ => {}
        }
    }
    // SEI alone shows nothing: no picture to decode.
    if !has_slice(&unit.avcc) {
        unit.avcc.clear();
    }
    unit
}

/// Whether an AVCC buffer of four-byte lengths holds a slice.
fn has_slice(avcc: &[u8]) -> bool {
    nals(avcc, 4).is_some_and(|nals| {
        nals.iter()
            .any(|nal| matches!(nal_type(nal), Some(SLICE..=IDR)))
    })
}

/// The NAL units of an AVCC buffer whose lengths take `length_size`
/// bytes (1, 2 or 4); none if a length runs past the end.
fn nals(avcc: &[u8], length_size: usize) -> Option<Vec<&[u8]>> {
    if !matches!(length_size, 1 | 2 | 4) {
        return None;
    }
    let mut out = Vec::new();
    let mut rest = avcc;
    while !rest.is_empty() {
        let (length, after) = rest.split_at_checked(length_size)?;
        let length = length
            .iter()
            .fold(0usize, |n, &byte| n << 8 | usize::from(byte));
        let (nal, after) = after.split_at_checked(length)?;
        if !nal.is_empty() {
            out.push(nal);
        }
        rest = after;
    }
    Some(out)
}

/// An encoder's AVCC output (lengths of `length_size` bytes) as Annex B
/// with four-byte start codes, `parameter_sets` (SPS and PPS, without
/// start codes) in front when it holds an IDR without them, as every
/// IDR sent must; none if a length runs past the end.
pub fn to_annex_b(avcc: &[u8], length_size: usize, parameter_sets: &[Vec<u8>]) -> Option<Vec<u8>> {
    let nals = nals(avcc, length_size)?;
    let idr = nals.iter().any(|nal| nal_type(nal) == Some(IDR));
    let has_sps = nals.iter().any(|nal| nal_type(nal) == Some(SPS));
    let mut out = Vec::with_capacity(avcc.len() + 64);
    if idr && !has_sps {
        for set in parameter_sets {
            out.extend_from_slice(&START);
            out.extend_from_slice(set);
        }
    }
    for nal in nals {
        out.extend_from_slice(&START);
        out.extend_from_slice(nal);
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use noslacking_video_ipc::h264::{access_units, is_keyframe, nal_types};

    const CAMERA: &[u8] =
        include_bytes!("../../../../src/huddle_audio/fixtures/camera-480x480.h264");

    #[test]
    fn annex_b_splits_into_parameter_sets_and_avcc() {
        let frame = [
            0, 0, 0, 1, 0x09, 0xf0, // an access unit delimiter
            0, 0, 0, 1, 0x67, 0x42, 0xc0, 0x0c, // SPS
            0, 0, 1, 0x68, 0xce, 0x3c, 0x80, // PPS
            0, 0, 0, 1, 0x06, 0x05, 0x01, // SEI
            0, 0, 1, 0x65, 0x88, 0x80, // IDR slice
        ];
        let unit = split(&frame);
        assert_eq!(unit.sps, Some(&[0x67, 0x42, 0xc0, 0x0c][..]));
        assert_eq!(unit.pps, Some(&[0x68, 0xce, 0x3c, 0x80][..]));
        assert!(unit.idr);
        assert_eq!(
            unit.avcc,
            [0, 0, 0, 3, 0x06, 0x05, 0x01, 0, 0, 0, 3, 0x65, 0x88, 0x80]
        );
        // Parameter sets alone, or with SEI, give nothing to decode.
        let sets = split(&frame[6..21]);
        assert!(sets.sps.is_some() && sets.pps.is_some() && sets.avcc.is_empty());
        assert!(split(&frame[21..28]).avcc.is_empty());
        assert!(!split(&[0, 0, 1, 0x41, 0x9a]).idr);
    }

    /// Every frame of a real stream, taken to AVCC and back with its
    /// parameter sets, is the frame it was.
    #[test]
    fn a_stream_round_trips_through_avcc() {
        let frames = access_units(CAMERA);
        assert!(frames.len() > 5);
        let mut sets = Vec::new();
        for frame in &frames {
            let unit = split(frame);
            if let (Some(sps), Some(pps)) = (unit.sps, unit.pps) {
                sets = vec![sps.to_vec(), pps.to_vec()];
            }
            let back = to_annex_b(&unit.avcc, 4, &sets).expect("whole");
            assert_eq!(&back, frame);
            assert_eq!(is_keyframe(&back), unit.idr);
        }
        let first = split(&frames[0]);
        let sps = noslacking_video_ipc::h264::parse_sps(first.sps.expect("an SPS"));
        assert_eq!(sps.map(|s| (s.width, s.height)), Some((480, 480)));
    }

    #[test]
    fn an_encoders_output_gets_its_parameter_sets_and_start_codes() {
        let sets = vec![vec![0x67, 1, 2], vec![0x68, 3]];
        // An IDR with two-byte lengths, as an encoder may write them.
        let idr = [0, 2, 0x65, 0xaa, 0, 1, 0x65];
        let annex_b = to_annex_b(&idr, 2, &sets).expect("whole");
        assert_eq!(nal_types(&annex_b), [7, 8, 5, 5]);
        assert_eq!(
            annex_b,
            [
                0, 0, 0, 1, 0x67, 1, 2, 0, 0, 0, 1, 0x68, 3, 0, 0, 0, 1, 0x65, 0xaa, 0, 0, 0, 1,
                0x65
            ]
        );
        // A P frame goes without them.
        let p = to_annex_b(&[0, 0, 0, 2, 0x41, 0x9a], 4, &sets).expect("whole");
        assert_eq!(p, [0, 0, 0, 1, 0x41, 0x9a]);
        // An IDR that brings its own is left as it is.
        let own = [0, 0, 0, 1, 0x67, 0, 0, 0, 1, 0x65];
        assert_eq!(
            nal_types(&to_annex_b(&own, 4, &sets).expect("whole")),
            [7, 5]
        );
        // Lengths past the end, and lengths of no size, are refused.
        assert_eq!(to_annex_b(&[0, 0, 0, 9, 0x41], 4, &sets), None);
        assert_eq!(to_annex_b(&[0, 0, 0], 4, &sets), None);
        assert_eq!(to_annex_b(&[1, 0x41], 3, &sets), None);
    }
}
