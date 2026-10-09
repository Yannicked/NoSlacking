//! Just enough of H.264's Annex B byte stream for both sides of the
//! pipe: NAL units found by their start codes, their types, and frames
//! (access units) as `str0m` hands them over. The app reads them to tell
//! a keyframe from the rest and to check what the helper encoded; the
//! helper, to cut its fixtures into frames. Nothing here decodes a
//! picture.

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

#[cfg(test)]
mod tests {
    use super::*;

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
