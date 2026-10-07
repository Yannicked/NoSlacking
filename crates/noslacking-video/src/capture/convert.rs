//! Captured pictures made ready for the software encoder: packed RGB to
//! I420 (BT.601 studio range, as WebRTC senders send), and shrunk to the
//! size sent keeping their shape. The GPU does the same itself (video
//! processing) without these.

use noslacking_video_ipc::Planes;

use super::{Order, Packed};

/// The size a `width`×`height` picture is sent at: shrunk, keeping its
/// shape, to fit `max`, never enlarged, and even both ways. `None` for a
/// picture too small to send (under 16 pixels a side).
pub fn fit(width: u32, height: u32, max: (u32, u32)) -> Option<(u32, u32)> {
    // An odd size loses its last row or column first, as the conversions
    // do.
    let (width, height) = (u64::from(width & !1), u64::from(height & !1));
    if width < 16 || height < 16 {
        return None;
    }
    let (max_w, max_h) = (u64::from(max.0.max(16)), u64::from(max.1.max(16)));
    // The smaller of the two scales, as a fraction, so no float rounds
    // a side past the limit.
    let (w, h) = if width * max_h > height * max_w {
        // Wider than the box: its width decides.
        let w = width.min(max_w);
        (w, height * w / width)
    } else {
        let h = height.min(max_h);
        (width * h / height, h)
    };
    let (w, h) = (u32::try_from(w & !1).ok()?, u32::try_from(h & !1).ok()?);
    (w >= 16 && h >= 16).then_some((w, h))
}

/// A packed picture as I420 at its own size (an odd size loses its last
/// row or column). `None` for a broken one.
pub fn to_i420(packed: &Packed<'_>) -> Option<Planes> {
    if !packed.whole() {
        return None;
    }
    let (w, h) = (packed.width & !1, packed.height & !1);
    if w == 0 || h == 0 {
        return None;
    }
    let (width, height) = (w as usize, h as usize);
    // The converter reads whole rows of `stride`: pad a last row that the
    // producer cut short after its pixels.
    let full = packed.stride.checked_mul(height)?;
    let data: std::borrow::Cow<'_, [u8]> = if packed.data.len() >= full {
        std::borrow::Cow::Borrowed(&packed.data[..full])
    } else {
        let mut padded = packed.data.to_vec();
        padded.resize(full, 0);
        std::borrow::Cow::Owned(padded)
    };
    let mut y = vec![0u8; width * height];
    let mut u = vec![0u8; width / 2 * (height / 2)];
    let mut v = vec![0u8; width / 2 * (height / 2)];
    let mut planar = yuv::YuvPlanarImageMut {
        y_plane: yuv::BufferStoreMut::Borrowed(&mut y),
        y_stride: w,
        u_plane: yuv::BufferStoreMut::Borrowed(&mut u),
        u_stride: w / 2,
        v_plane: yuv::BufferStoreMut::Borrowed(&mut v),
        v_stride: w / 2,
        width: w,
        height: h,
    };
    let stride = u32::try_from(packed.stride).ok()?;
    let convert = match packed.order {
        Order::Bgra => yuv::bgra_to_yuv420,
        Order::Rgba => yuv::rgba_to_yuv420,
    };
    convert(
        &mut planar,
        &data,
        stride,
        yuv::YuvRange::Limited,
        yuv::YuvStandardMatrix::Bt601,
        yuv::YuvConversionMode::Balanced,
    )
    .ok()?;
    drop(planar);
    Some(Planes {
        width: w,
        height: h,
        y,
        u,
        v,
    })
}

/// `picture` shrunk to `width`×`height` (even, no larger than it), each
/// pixel the average of the block of the source it covers. The picture
/// itself when it already has that size.
pub fn scale(picture: Planes, width: u32, height: u32) -> Planes {
    let (width, height) = (
        (width & !1).clamp(2, picture.width.max(2)),
        (height & !1).clamp(2, picture.height.max(2)),
    );
    if (width, height) == (picture.width, picture.height)
        || picture.check().is_err()
        || !picture.width.is_multiple_of(2)
        || !picture.height.is_multiple_of(2)
    {
        return picture;
    }
    let n = |v: u32| v as usize;
    let (cw, ch) = (n(picture.width / 2), n(picture.height / 2));
    Planes {
        width,
        height,
        y: area(
            &picture.y,
            n(picture.width),
            n(picture.height),
            n(width),
            n(height),
        ),
        u: area(&picture.u, cw, ch, n(width / 2), n(height / 2)),
        v: area(&picture.v, cw, ch, n(width / 2), n(height / 2)),
    }
}

/// One plane averaged down: each output pixel covers the source pixels
/// from its left/top edge to the next one's, at least one.
fn area(plane: &[u8], width: usize, height: usize, out_w: usize, out_h: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(out_w * out_h);
    // The source columns each output column starts at, once.
    let columns: Vec<(usize, usize)> = (0..out_w)
        .map(|x| {
            let from = x * width / out_w;
            let to = ((x + 1) * width / out_w).max(from + 1).min(width);
            (from, to)
        })
        .collect();
    let mut sums = vec![0u32; out_w];
    for row in 0..out_h {
        let top = row * height / out_h;
        let bottom = ((row + 1) * height / out_h).max(top + 1).min(height);
        sums.fill(0);
        for line in plane.chunks_exact(width).skip(top).take(bottom - top) {
            for (sum, &(from, to)) in sums.iter_mut().zip(&columns) {
                *sum += line[from..to].iter().map(|&p| u32::from(p)).sum::<u32>();
            }
        }
        let rows = u32::try_from(bottom - top).unwrap_or(1);
        for (sum, &(from, to)) in sums.iter().zip(&columns) {
            let count = (rows * u32::try_from(to - from).unwrap_or(1)).max(1);
            out.push(u8::try_from((sum + count / 2) / count).unwrap_or(u8::MAX));
        }
    }
    out
}

/// A picture as the software encoder takes it: I420, shrunk to fit
/// `max`. `None` for one too small or broken.
pub fn for_software(packed: &Packed<'_>, max: (u32, u32)) -> Option<Planes> {
    let (w, h) = fit(packed.width, packed.height, max)?;
    Some(scale(to_i420(packed)?, w, h))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn packed(width: u32, height: u32, stride: usize, data: &[u8]) -> Packed<'_> {
        Packed {
            width,
            height,
            stride,
            order: Order::Bgra,
            data,
        }
    }

    /// Larger screens shrink to fit keeping their shape; smaller ones are
    /// sent as they are.
    #[test]
    fn screens_are_shrunk_keeping_their_shape() {
        for ((w, h), max, want) in [
            ((3840, 2160), (1920, 1080), (1920, 1080)),
            ((2560, 1440), (1920, 1080), (1920, 1080)),
            ((2560, 1600), (1920, 1080), (1728, 1080)),
            ((3440, 1440), (1920, 1080), (1920, 802)),
            ((1080, 1920), (1920, 1080), (606, 1080)),
            ((1920, 1080), (1920, 1080), (1920, 1080)),
            ((1366, 768), (1920, 1080), (1366, 768)),
            ((1365, 767), (1920, 1080), (1364, 766)),
            ((1920, 1080), (1280, 720), (1280, 720)),
            ((2560, 1440), (1280, 720), (1280, 720)),
        ] {
            assert_eq!(fit(w, h, max), Some(want), "{w}x{h} in {max:?}");
        }
        assert_eq!(fit(8, 8, (1920, 1080)), None);
        let data = vec![128u8; 2560 * 4 * 1440];
        let sent =
            for_software(&packed(2560, 1440, 2560 * 4, &data), (1280, 720)).expect("a picture");
        assert_eq!((sent.width, sent.height), (1280, 720));
        assert!(sent.check().is_ok());
        // Broken: nothing.
        assert!(for_software(&packed(64, 64, 256, &data[..100]), (1280, 720)).is_none());
    }

    #[test]
    fn colours_come_through_in_either_byte_order() {
        let mut bgra = vec![0u8; 32 * 32 * 4];
        let mut rgba = bgra.clone();
        for pixel in bgra.as_chunks_mut::<4>().0 {
            pixel.copy_from_slice(&[0, 0, 255, 255]);
        }
        for pixel in rgba.as_chunks_mut::<4>().0 {
            pixel.copy_from_slice(&[255, 0, 0, 255]);
        }
        let a = to_i420(&packed(32, 32, 128, &bgra)).expect("converted");
        let b = to_i420(&Packed {
            order: Order::Rgba,
            ..packed(32, 32, 128, &rgba)
        })
        .expect("converted");
        assert_eq!(a, b);
        // BT.601 studio-range red: Y about 81, V about 240.
        assert!((70..95).contains(&a.y[0]), "{}", a.y[0]);
        assert!(a.v[0] > 220, "{}", a.v[0]);
    }

    #[test]
    fn a_short_last_row_and_padding_are_handled() {
        // Rows of 40 bytes for 8 pixels; the last row stops at its pixels.
        let mut data = vec![200u8; 40 * 15 + 32];
        let picture = to_i420(&packed(8, 16, 40, &data)).expect("converted");
        assert_eq!((picture.width, picture.height), (8, 16));
        data.truncate(40 * 15 + 31);
        assert!(
            to_i420(&packed(8, 16, 40, &data)).is_none(),
            "one byte short"
        );
    }

    #[test]
    fn scaling_averages_and_keeps_the_size_asked() {
        let picture = Planes {
            width: 4,
            height: 2,
            y: vec![0, 100, 200, 50, 0, 100, 200, 50],
            u: vec![10, 30],
            v: vec![40, 60],
        };
        let small = scale(picture.clone(), 2, 2);
        assert_eq!((small.width, small.height), (2, 2));
        assert_eq!(small.y, vec![50, 125, 50, 125]);
        assert_eq!(small.u, vec![20]);
        // Asked for its own size, or larger: the same.
        assert_eq!(scale(picture.clone(), 4, 2), picture);
        assert_eq!(scale(picture.clone(), 8, 8), picture);
    }
}
