//! Pictures in and out of a pixel buffer's planes. VideoToolbox decodes
//! into NV12 (luma, then blue and red interleaved at half size), the
//! hardware's own layout, and its encoder takes NV12; the pipe carries
//! I420. Each plane comes with its own stride, as a locked pixel buffer
//! lends it, and nothing is read or written past what the plane holds.
//! Plain code, so it is tested on every system.

use noslacking_video_ipc::{Planes, chroma_size};

/// An NV12 picture as a pixel buffer lends it, read only.
#[derive(Clone, Copy, Debug)]
pub struct Nv12<'a> {
    /// The width to read, in pixels.
    pub width: u32,
    /// The height to read, in pixels.
    pub height: u32,
    /// The luma plane.
    pub y: &'a [u8],
    /// Bytes from one luma row to the next.
    pub y_stride: usize,
    /// The interleaved chroma plane, half as tall.
    pub uv: &'a [u8],
    /// Bytes from one chroma row to the next.
    pub uv_stride: usize,
}

/// An NV12 picture as a pixel buffer lends it, to write into.
#[derive(Debug)]
pub struct Nv12Mut<'a> {
    /// Its width in pixels.
    pub width: u32,
    /// Its height in pixels.
    pub height: u32,
    /// The luma plane.
    pub y: &'a mut [u8],
    /// Bytes from one luma row to the next.
    pub y_stride: usize,
    /// The interleaved chroma plane, half as tall.
    pub uv: &'a mut [u8],
    /// Bytes from one chroma row to the next.
    pub uv_stride: usize,
}

/// Whether `len` bytes hold `rows` rows of `row` bytes each `stride`
/// apart.
fn holds(len: usize, stride: usize, row: usize, rows: usize) -> bool {
    rows == 0
        || stride >= row
            && stride
                .checked_mul(rows - 1)
                .and_then(|n| n.checked_add(row))
                .is_some_and(|n| len >= n)
}

/// The top-left `nv12.width`×`nv12.height` of an NV12 picture as I420;
/// none if its planes are shorter than that.
pub fn to_i420(nv12: &Nv12<'_>) -> Option<Planes> {
    let (width, height) = (nv12.width, nv12.height);
    let (cw, ch) = chroma_size(width, height);
    let (w, h) = (usize::try_from(width).ok()?, usize::try_from(height).ok()?);
    let (cw, ch) = (usize::try_from(cw).ok()?, usize::try_from(ch).ok()?);
    if !holds(nv12.y.len(), nv12.y_stride, w, h)
        || !holds(nv12.uv.len(), nv12.uv_stride, cw * 2, ch)
    {
        return None;
    }
    let mut y = Vec::with_capacity(w * h);
    for row in nv12.y.chunks(nv12.y_stride.max(1)).take(h) {
        y.extend_from_slice(&row[..w]);
    }
    let mut u = Vec::with_capacity(cw * ch);
    let mut v = Vec::with_capacity(cw * ch);
    for row in nv12.uv.chunks(nv12.uv_stride.max(1)).take(ch) {
        for pair in row[..cw * 2].as_chunks::<2>().0 {
            u.push(pair[0]);
            v.push(pair[1]);
        }
    }
    Some(Planes {
        width,
        height,
        y,
        u,
        v,
    })
}

/// Writes `picture` into an NV12 picture of the same size; false (and
/// nothing written) if the sizes differ or a plane is too short.
pub fn write_i420(picture: &Planes, out: &mut Nv12Mut<'_>) -> bool {
    if (picture.width, picture.height) != (out.width, out.height) || picture.check().is_err() {
        return false;
    }
    let (cw, ch) = chroma_size(picture.width, picture.height);
    let n = |v: u32| usize::try_from(v).unwrap_or(usize::MAX);
    let (w, h, cw, ch) = (n(picture.width), n(picture.height), n(cw), n(ch));
    if !holds(out.y.len(), out.y_stride, w, h) || !holds(out.uv.len(), out.uv_stride, cw * 2, ch) {
        return false;
    }
    for (to, from) in out
        .y
        .chunks_mut(out.y_stride.max(1))
        .zip(picture.y.chunks_exact(w))
    {
        to[..w].copy_from_slice(from);
    }
    let chroma = picture.u.chunks_exact(cw).zip(picture.v.chunks_exact(cw));
    for (to, (u, v)) in out.uv.chunks_mut(out.uv_stride.max(1)).zip(chroma) {
        for (pair, (&u, &v)) in to[..cw * 2]
            .as_chunks_mut::<2>()
            .0
            .iter_mut()
            .zip(u.iter().zip(v))
        {
            *pair = [u, v];
        }
    }
    true
}

/// Copies `rows` rows of `row` bytes from `from` (rows `from_stride`
/// apart) into `to` (rows `to_stride` apart): a packed picture into a
/// pixel buffer whose rows are padded differently. False (and nothing
/// copied) if either is too short.
pub fn copy_rows(
    from: &[u8],
    from_stride: usize,
    to: &mut [u8],
    to_stride: usize,
    row: usize,
    rows: usize,
) -> bool {
    if !holds(from.len(), from_stride, row, rows) || !holds(to.len(), to_stride, row, rows) {
        return false;
    }
    for (to, from) in to
        .chunks_mut(to_stride.max(1))
        .zip(from.chunks(from_stride.max(1)))
        .take(rows)
    {
        to[..row].copy_from_slice(&from[..row]);
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    fn picture(width: u32, height: u32) -> Planes {
        let (cw, ch) = chroma_size(width, height);
        let n = |v: u32| usize::try_from(v).expect("small");
        Planes {
            width,
            height,
            y: (0..n(width * height)).map(|i| (i % 251) as u8).collect(),
            u: (0..n(cw * ch)).map(|i| (i % 7) as u8 + 100).collect(),
            v: (0..n(cw * ch)).map(|i| (i % 5) as u8 + 200).collect(),
        }
    }

    /// I420 written into padded NV12 planes reads back as it was, and a
    /// smaller top-left part reads back as the same part of it.
    #[test]
    fn nv12_round_trips_through_padded_planes() {
        let original = picture(36, 20);
        let (y_stride, uv_stride) = (64, 48);
        let mut y = vec![0xee; y_stride * 20];
        let mut uv = vec![0xee; uv_stride * 10];
        let mut out = Nv12Mut {
            width: 36,
            height: 20,
            y: &mut y,
            y_stride,
            uv: &mut uv,
            uv_stride,
        };
        assert!(write_i420(&original, &mut out));
        assert_eq!(uv[..4], [100, 200, 101, 201]);
        assert_eq!(uv[36], 0xee, "the padding is left alone");
        let mut nv12 = Nv12 {
            width: 36,
            height: 20,
            y: &y,
            y_stride,
            uv: &uv,
            uv_stride,
        };
        assert_eq!(to_i420(&nv12), Some(original.clone()));
        // A coded 36x20 picture shown as 32x16.
        nv12.width = 32;
        nv12.height = 16;
        let part = to_i420(&nv12).expect("read");
        assert!(part.check().is_ok());
        assert_eq!(part.y[..32], original.y[..32]);
        assert_eq!(part.y[32..64], original.y[36..68]);
        assert_eq!(part.u[16..32], original.u[18..34]);
    }

    #[test]
    fn short_planes_are_refused_not_read_past() {
        let y = vec![0; 16 * 8];
        let uv = vec![0; 16 * 3];
        let nv12 = Nv12 {
            width: 16,
            height: 8,
            y: &y,
            y_stride: 16,
            uv: &uv,
            uv_stride: 16,
        };
        assert_eq!(to_i420(&nv12), None, "chroma is a row short");
        let mut y = vec![0; 16 * 8];
        let mut uv = vec![0; 16 * 4];
        let mut out = Nv12Mut {
            width: 16,
            height: 8,
            y: &mut y,
            y_stride: 16,
            uv: &mut uv,
            uv_stride: 16,
        };
        assert!(!write_i420(&picture(18, 8), &mut out), "another size");
        out.y_stride = 8;
        assert!(!write_i420(&picture(16, 8), &mut out), "a stride too small");
    }

    #[test]
    fn rows_copy_between_strides() {
        let from: Vec<u8> = (0..30).collect();
        let mut to = vec![0; 3 * 12];
        assert!(copy_rows(&from, 10, &mut to, 12, 8, 3));
        assert_eq!(to[..8], from[..8]);
        assert_eq!(to[8..12], [0; 4]);
        assert_eq!(to[12..20], from[10..18]);
        assert_eq!(to[24..32], from[20..28]);
        assert!(!copy_rows(&from, 10, &mut to, 12, 8, 4), "a row too many");
        assert!(!copy_rows(&from, 10, &mut to, 6, 8, 2), "rows overlap");
    }
}
