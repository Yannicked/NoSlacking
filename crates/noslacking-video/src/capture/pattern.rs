//! The test screen: colour bars sliding left, a white square bouncing
//! across, and the elapsed time as mm:ss.t in large digits with the
//! frame number under it, so a frozen or late picture shows. The probe
//! shares it to see that Slack shows our share, and the benchmarks
//! measure with it; never anyone's real screen.

use std::time::Duration;

use noslacking_video_ipc::Planes;

/// A black I420 picture of `width`×`height`, rounded down to even sizes.
pub fn black(width: u32, height: u32) -> Planes {
    let (width, height) = (width & !1, height & !1);
    let luma = width as usize * height as usize;
    Planes {
        width,
        height,
        y: vec![16; luma],
        u: vec![128; luma / 4],
        v: vec![128; luma / 4],
    }
}

/// Frame `n` at `elapsed`, `width`×`height`.
pub fn pattern(width: u32, height: u32, n: u64, elapsed: Duration) -> Planes {
    let mut out = black(width, height);
    let (w, h) = (out.width as usize, out.height as usize);
    if w == 0 || h == 0 {
        return out;
    }
    // BT.601 studio-range colour bars: white, yellow, cyan, green,
    // magenta, red, blue, black.
    const BARS: [(u8, u8, u8); 8] = [
        (235, 128, 128),
        (210, 16, 146),
        (170, 166, 16),
        (145, 54, 34),
        (106, 202, 222),
        (81, 90, 240),
        (41, 240, 110),
        (16, 128, 128),
    ];
    let shift = usize::try_from(n * 4).unwrap_or(0);
    let bar = |x: usize| BARS[((x + shift) * BARS.len() / w) % BARS.len()];
    for row in 0..h {
        for x in 0..w {
            out.y[row * w + x] = bar(x).0;
        }
    }
    for row in 0..h / 2 {
        for x in 0..w / 2 {
            let (_, u, v) = bar(x * 2);
            out.u[row * (w / 2) + x] = u;
            out.v[row * (w / 2) + x] = v;
        }
    }
    // The bouncing square.
    let side = (h / 6).max(4) & !1;
    let span = w.saturating_sub(side).max(1);
    let travel = usize::try_from(n * 8).unwrap_or(0) % (2 * span);
    let left = (if travel < span {
        travel
    } else {
        2 * span - travel
    }) & !1;
    let top = (h / 8) & !1;
    fill(&mut out, left, top, side, side, (235, 128, 128));
    // The clock, on a dark band across the lower half.
    let band_top = (h / 2) & !1;
    let band = (h / 3) & !1;
    fill(&mut out, 0, band_top, w, band, (16, 128, 128));
    let tenths = elapsed.as_millis() / 100;
    let clock = format!(
        "{:02}:{:02}.{}",
        (tenths / 600) % 100,
        (tenths / 10) % 60,
        tenths % 10
    );
    let digit = (band * 5 / 8).max(10);
    text(&mut out, &clock, w / 16, band_top + band / 8, digit);
    let frame = format!("{n}");
    text(
        &mut out,
        &frame,
        w / 16,
        band_top + band / 8 + digit + digit / 6,
        (digit / 3).max(6),
    );
    out
}

/// Paints a rectangle in one colour.
fn fill(
    out: &mut Planes,
    left: usize,
    top: usize,
    width: usize,
    height: usize,
    colour: (u8, u8, u8),
) {
    let (w, h) = (out.width as usize, out.height as usize);
    let right = (left + width).min(w);
    let bottom = (top + height).min(h);
    for row in top.min(h)..bottom {
        for x in left.min(w)..right {
            out.y[row * w + x] = colour.0;
        }
    }
    for row in top.min(h) / 2..bottom / 2 {
        for x in left.min(w) / 2..right / 2 {
            out.u[row * (w / 2) + x] = colour.1;
            out.v[row * (w / 2) + x] = colour.2;
        }
    }
}

/// Writes `text` (digits, `:` and `.`) in seven-segment strokes `size`
/// pixels high, from `left`, `top`.
fn text(out: &mut Planes, text: &str, left: usize, top: usize, size: usize) {
    // Segments a–g, as bits.
    const DIGITS: [u8; 10] = [
        0b011_1111, 0b000_0110, 0b101_1011, 0b100_1111, 0b110_0110, 0b110_1101, 0b111_1101,
        0b000_0111, 0b111_1111, 0b110_1111,
    ];
    let white = (235, 128, 128);
    let stroke = (size / 8).max(2);
    let width = size / 2;
    let mut x = left;
    for c in text.chars() {
        match c {
            ':' => {
                fill(out, x, top + size / 4, stroke, stroke, white);
                fill(out, x, top + size * 3 / 4, stroke, stroke, white);
                x += stroke * 3;
            }
            '.' => {
                fill(out, x, top + size - stroke, stroke, stroke, white);
                x += stroke * 3;
            }
            _ => {
                let Some(bits) = c.to_digit(10).and_then(|d| DIGITS.get(d as usize)) else {
                    continue;
                };
                let half = size / 2;
                let segments = [
                    (x, top, width, stroke),                        // a
                    (x + width - stroke, top, stroke, half),        // b
                    (x + width - stroke, top + half, stroke, half), // c
                    (x, top + size - stroke, width, stroke),        // d
                    (x, top + half, stroke, half),                  // e
                    (x, top, stroke, half),                         // f
                    (x, top + half - stroke / 2, width, stroke),    // g
                ];
                for (i, &(sx, sy, sw, sh)) in segments.iter().enumerate() {
                    if bits & (1 << i) != 0 {
                        fill(out, sx, sy, sw, sh, white);
                    }
                }
                x += width + stroke * 2;
            }
        }
    }
}

/// How many different pictures the packed and dma-buf test screens go
/// round: two seconds' worth, so the encoder always has motion to code.
pub const LOOP: u64 = 30;

/// [`pattern`] as BGRx in memory, as `pattern::pattern(i)` for `i` in
/// `0..LOOP` at 1920×1080: what PipeWire's shared memory hands over, made
/// once so the benchmark measures the share and not the drawing.
pub fn packed_loop() -> Vec<(u32, u32, Vec<u8>)> {
    (0..LOOP)
        .map(|n| {
            let picture = pattern(1920, 1080, n, Duration::from_millis(n * 1000 / 15));
            (1920, 1080, to_bgrx(&picture))
        })
        .collect()
}

/// An I420 picture as BGRx, BT.601 studio range.
pub fn to_bgrx(picture: &Planes) -> Vec<u8> {
    let mut out = vec![0u8; picture.width as usize * picture.height as usize * 4];
    let planar = yuv::YuvPlanarImage {
        y_plane: &picture.y,
        y_stride: picture.width,
        u_plane: &picture.u,
        u_stride: picture.width / 2,
        v_plane: &picture.v,
        v_stride: picture.width / 2,
        width: picture.width,
        height: picture.height,
    };
    // A failure leaves it black, which a benchmark would show.
    let _ = yuv::yuv420_to_bgra(
        &planar,
        &mut out,
        picture.width * 4,
        yuv::YuvRange::Limited,
        yuv::YuvStandardMatrix::Bt601,
    );
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_test_screen_moves_and_is_whole() {
        let a = pattern(1920, 1080, 0, Duration::ZERO);
        let b = pattern(1920, 1080, 1, Duration::from_millis(67));
        assert!(a.check().is_ok() && b.check().is_ok());
        assert_ne!(a.y, b.y, "the bars and the square move");
        let bgrx = to_bgrx(&a);
        assert_eq!(bgrx.len(), 1920 * 1080 * 4);
        assert!(bgrx.iter().any(|&b| b > 200), "not black");
    }
}
