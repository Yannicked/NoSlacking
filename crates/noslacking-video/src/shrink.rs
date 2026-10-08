//! Shrinking a picture on the processor, for back ends (or drivers)
//! that cannot scale on the GPU: so the pipe still carries a picture no
//! larger than the app shows. By a whole step, each pixel the average of
//! the block it stands for, as the app's own software path does.

use noslacking_video_ipc::{Planes, chroma_size};

/// By how much to shrink a `source`-sized picture to still cover `fit`
/// both ways: the largest whole number, 1 for no box (0) or a larger one.
pub fn step(source: (u32, u32), fit: (u32, u32)) -> u32 {
    if fit.0 == 0 || fit.1 == 0 {
        return 1;
    }
    (source.0 / fit.0).min(source.1 / fit.1).max(1)
}

/// `planes` shrunk to cover `fit` by a whole step (even-sized, so the
/// chroma halves exactly); as they are if no step fits.
pub fn shrink(planes: Planes, fit: (u32, u32)) -> Planes {
    let by = step((planes.width, planes.height), fit);
    if by <= 1 {
        return planes;
    }
    by_step(&planes, by)
}

/// A camera's self-view: `planes` shrunk by the smallest whole step that
/// makes it at most `width` wide; a copy if it is already.
pub fn preview(planes: &Planes, width: u32) -> Planes {
    let by = planes.width.div_ceil(width.max(1)).max(1);
    if by <= 1 {
        return planes.clone();
    }
    by_step(planes, by)
}

/// `planes` averaged down by `by` (more than 1) each way, even-sized.
fn by_step(planes: &Planes, by: u32) -> Planes {
    let n = |v: u32| usize::try_from(v).unwrap_or(0);
    let width = ((planes.width / by) & !1).max(2);
    let height = ((planes.height / by) & !1).max(2);
    let (cw, ch) = chroma_size(planes.width, planes.height);
    let by = n(by);
    Planes {
        width,
        height,
        y: average(
            &planes.y,
            n(planes.width),
            n(planes.height),
            n(width),
            n(height),
            by,
        ),
        u: average(&planes.u, n(cw), n(ch), n(width / 2), n(height / 2), by),
        v: average(&planes.v, n(cw), n(ch), n(width / 2), n(height / 2), by),
    }
}

/// A plane of `width`×`height` averaged down by `by` to `out_w`×`out_h`;
/// blocks past the edge are cut short.
fn average(
    plane: &[u8],
    width: usize,
    height: usize,
    out_w: usize,
    out_h: usize,
    by: usize,
) -> Vec<u8> {
    if by == 2 && out_w * 2 <= width && out_h * 2 <= height {
        return halve(plane, width, out_w, out_h);
    }
    let mut out = Vec::with_capacity(out_w * out_h);
    let mut sums = vec![0u32; out_w];
    for row in 0..out_h {
        sums.fill(0);
        let top = row * by;
        let rows = by.min(height.saturating_sub(top));
        for line in plane.chunks_exact(width.max(1)).skip(top).take(rows) {
            for (sum, block) in sums.iter_mut().zip(line.chunks(by)) {
                *sum += block.iter().map(|&p| u32::from(p)).sum::<u32>();
            }
        }
        for (col, sum) in sums.iter().enumerate() {
            let cols = by.min(width.saturating_sub(col * by));
            let count = u32::try_from((rows * cols).max(1)).unwrap_or(u32::MAX);
            out.push(u8::try_from((sum + count / 2) / count).unwrap_or(u8::MAX));
        }
    }
    out
}

/// [`average`] by 2 where every block is whole: each pixel the rounded
/// mean of its 2×2 block, in one pass over each pair of rows. The common
/// case (a 640-wide camera's 320-wide self-view, a 1080p share shown at
/// half size), and about five times faster than the general loop.
fn halve(plane: &[u8], width: usize, out_w: usize, out_h: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(out_w * out_h);
    for rows in plane.chunks_exact(2 * width).take(out_h) {
        let (top, bottom) = rows.split_at(width);
        let pairs = top[..out_w * 2]
            .as_chunks::<2>()
            .0
            .iter()
            .zip(bottom[..out_w * 2].as_chunks::<2>().0);
        out.extend(pairs.map(|(a, b)| {
            let sum = u16::from(a[0]) + u16::from(a[1]) + u16::from(b[0]) + u16::from(b[1]);
            u8::try_from((sum + 2) / 4).unwrap_or(u8::MAX)
        }));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The 2× case gives what the general loop gives, odd sizes too.
    #[test]
    fn halving_is_the_general_average() {
        for (width, height) in [(8, 4), (9, 5), (640, 480), (321, 181)] {
            let plane: Vec<u8> = (0..width * height).map(|i| (i * 37 % 251) as u8).collect();
            let (out_w, out_h) = ((width / 2) & !1, (height / 2) & !1);
            let mut general = Vec::new();
            let mut sums = vec![0u32; out_w];
            for row in 0..out_h {
                sums.fill(0);
                for line in plane.chunks_exact(width).skip(row * 2).take(2) {
                    for (sum, block) in sums.iter_mut().zip(line.chunks(2)) {
                        *sum += block.iter().map(|&p| u32::from(p)).sum::<u32>();
                    }
                }
                general.extend(sums.iter().map(|s| ((s + 2) / 4) as u8));
            }
            assert_eq!(
                halve(&plane, width, out_w, out_h),
                general,
                "{width}x{height}"
            );
        }
    }

    fn picture(width: u32, height: u32) -> Planes {
        let (cw, ch) = chroma_size(width, height);
        let n = |v: u32| usize::try_from(v).expect("small");
        Planes {
            width,
            height,
            y: (0..n(width * height)).map(|i| (i % 200) as u8).collect(),
            u: vec![90; n(cw * ch)],
            v: vec![240; n(cw * ch)],
        }
    }

    #[test]
    fn pictures_shrink_by_whole_steps_and_stay_whole() {
        assert_eq!(step((1920, 1080), (0, 0)), 1);
        assert_eq!(step((1920, 1080), (960, 540)), 2);
        assert_eq!(step((1920, 1080), (640, 200)), 3);
        assert_eq!(
            step((1920, 1080), (300, 1000)),
            1,
            "the taller side decides"
        );
        let half = shrink(picture(1920, 1080), (960, 540));
        assert_eq!((half.width, half.height), (960, 540));
        assert!(half.check().is_ok());
        assert_eq!(half.u[0], 90);
        assert_eq!(half.v[0], 240);
        // Luma 0,1 over 1920,1921 (mod 200: 120,121): average 61.
        assert_eq!(half.y[0], 61);
        let third = shrink(picture(480, 480), (160, 120));
        assert_eq!((third.width, third.height), (160, 160));
        assert!(third.check().is_ok());
        // Odd sizes come out even and whole.
        let odd = shrink(picture(321, 181), (100, 50));
        assert_eq!((odd.width, odd.height), (106, 60));
        assert!(odd.check().is_ok());
        // Nothing to do: as it was.
        let same = shrink(picture(64, 48), (64, 48));
        assert_eq!((same.width, same.height), (64, 48));
    }

    #[test]
    fn a_self_view_is_at_most_as_wide_as_asked() {
        for ((w, h), max, want) in [
            ((640, 480), 320, (320, 240)),
            ((480, 480), 320, (240, 240)),
            ((640, 360), 320, (320, 180)),
            ((1280, 720), 320, (320, 180)),
            ((320, 240), 320, (320, 240)),
            ((200, 100), 320, (200, 100)),
            ((642, 482), 320, (214, 160)),
        ] {
            let small = preview(&picture(w, h), max);
            assert_eq!((small.width, small.height), want, "{w}x{h}");
            assert!(small.check().is_ok());
            assert!(small.width <= max);
        }
    }
}
