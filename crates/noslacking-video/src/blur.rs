//! A blurred background behind the person on camera (a first try, on with
//! `NOSLACKING_BLUR=1`).
//!
//! Google's MediaPipe selfie segmenter (Apache-2.0, see `models/README.md`)
//! says where the person is, run by tract on the processor: on every
//! third picture, its mask smoothed over time and kept for the
//! pictures between, since people move slowly against 30 pictures a
//! second. The background is each plane shrunk to a quarter and blurred
//! there, from the background's own pixels only (the mask weighs them), so
//! the person's colours do not bleed into a halo around them; the full
//! picture then keeps the person and takes the blurred plane elsewhere.
//!
//! It runs on the camera's reader thread, beside the pipeline's encoding.

use std::time::{Duration, Instant};

use noslacking_video_ipc::Planes;
use tract_onnx::prelude::*;

/// The model, as `tools/selfie-onnx/convert.py` made it.
const MODEL: &[u8] = include_bytes!("../models/selfie_segmenter_landscape.onnx");
/// Its input, in pixels.
const MODEL_W: usize = 256;
const MODEL_H: usize = 144;
/// The model runs on every this many pictures.
const MODEL_EVERY: u64 = 3;
/// How much of a new mask goes into the one kept, so its edges do not
/// flicker.
const MASK_NEW: f32 = 0.7;
/// The background is blurred at this fraction of a plane's size: cheaper,
/// and blurrier for the same work.
const SHRINK: usize = 4;
/// The box blur's radius on the shrunk luma, and on the shrunk chroma
/// (half its size); three passes come close to a Gaussian.
const RADIUS_LUMA: usize = 3;
const RADIUS_CHROMA: usize = 2;
/// How often the cost is told.
const REPORT_EVERY: Duration = Duration::from_secs(10);

/// Whether the background is wanted blurred: `NOSLACKING_BLUR` set to
/// anything but empty or `0`, for now.
pub fn wanted() -> bool {
    std::env::var_os("NOSLACKING_BLUR").is_some_and(|v| !v.is_empty() && v != "0")
}

/// The blur for one camera: the model, the mask kept between pictures,
/// and what it cost.
pub struct Blur {
    model: std::sync::Arc<TypedRunnableModel>,
    input: Vec<f32>,
    mask: Vec<f32>,
    pictures: u64,
    cost: Cost,
}

/// Time spent since the last report.
#[derive(Default)]
struct Cost {
    since: Option<Instant>,
    pictures: u32,
    masks: u32,
    model: Duration,
    rest: Duration,
}

impl Blur {
    /// Loads the model (about 50 ms).
    pub fn new() -> Result<Self, String> {
        let model = tract_onnx::onnx()
            .model_for_read(&mut &MODEL[..])
            .and_then(|m| m.into_optimized())
            .and_then(|m| m.into_runnable())
            .map_err(|e| format!("the segmentation model did not load: {e}"))?;
        Ok(Self {
            model,
            input: vec![0.0; MODEL_W * MODEL_H * 3],
            mask: Vec::new(),
            pictures: 0,
            cost: Cost::default(),
        })
    }

    /// Blurs `picture`'s background in place. A picture too small to
    /// shrink, or with planes the wrong size, is left as it is.
    pub fn apply(&mut self, picture: &mut Planes) {
        let (w, h) = (picture.width as usize, picture.height as usize);
        if w < SHRINK * 4
            || h < SHRINK * 4
            || picture.y.len() != w * h
            || picture.u.len() != (w / 2) * (h / 2)
            || picture.v.len() != picture.u.len()
        {
            return;
        }
        let start = Instant::now();
        if self.pictures.is_multiple_of(MODEL_EVERY) || self.mask.is_empty() {
            match self.segment(picture) {
                Ok(mask) => {
                    if self.mask.len() == mask.len() {
                        for (kept, new) in self.mask.iter_mut().zip(&mask) {
                            *kept += (new - *kept) * MASK_NEW;
                        }
                    } else {
                        self.mask = mask;
                    }
                    self.cost.masks += 1;
                }
                Err(error) => {
                    eprintln!("noslacking-video: blur: {error}");
                    if self.mask.is_empty() {
                        return;
                    }
                }
            }
        }
        self.pictures += 1;
        let segmented = Instant::now();
        // The mask at the picture's size, once: the chroma planes take
        // every other pixel of it.
        let person_full = scaled(&self.mask, MODEL_W, MODEL_H, w, h, person);
        let (cw, ch) = (w / 2, h / 2);
        blur_plane(
            &mut picture.y,
            w,
            h,
            &self.mask,
            &person_full,
            1,
            RADIUS_LUMA,
        );
        blur_plane(
            &mut picture.u,
            cw,
            ch,
            &self.mask,
            &person_full,
            2,
            RADIUS_CHROMA,
        );
        blur_plane(
            &mut picture.v,
            cw,
            ch,
            &self.mask,
            &person_full,
            2,
            RADIUS_CHROMA,
        );
        self.cost.model += segmented - start;
        self.cost.rest += segmented.elapsed();
        self.cost.pictures += 1;
        self.report();
    }

    /// The person's mask for `picture`, `MODEL_W` × `MODEL_H`, 1 on them.
    fn segment(&mut self, picture: &Planes) -> TractResult<Vec<f32>> {
        model_input(picture, &mut self.input);
        let x: Tensor =
            tract_ndarray::Array4::from_shape_vec((1, MODEL_H, MODEL_W, 3), self.input.clone())?
                .into();
        let out = self.model.run(tvec!(x.into_tvalue()))?;
        let mask = out[0].clone().into_tensor();
        let view = mask.try_as_plain_ram()?;
        Ok(view.as_slice::<f32>()?.to_vec())
    }

    fn report(&mut self) {
        let since = *self.cost.since.get_or_insert_with(Instant::now);
        if since.elapsed() < REPORT_EVERY || self.cost.pictures == 0 {
            return;
        }
        let ms = |d: Duration, n: u32| d.as_secs_f64() * 1e3 / f64::from(n.max(1));
        eprintln!(
            "noslacking-video: blur: {} pictures, {} masks in {:.0} s: model {:.1} ms a mask, blur and blend {:.1} ms a picture",
            self.cost.pictures,
            self.cost.masks,
            since.elapsed().as_secs_f64(),
            ms(self.cost.model, self.cost.masks),
            ms(self.cost.rest, self.cost.pictures),
        );
        self.cost = Cost::default();
    }
}

/// The model's input from an I420 picture: squeezed to `MODEL_W` ×
/// `MODEL_H`, each the nearest source pixel, as RGB in 0..1 (BT.601
/// studio range, as the helper's conversions).
fn model_input(picture: &Planes, out: &mut [f32]) {
    let (w, h) = (picture.width as usize, picture.height as usize);
    let cw = w / 2;
    for j in 0..MODEL_H {
        let sj = (j * h + h / 2) / MODEL_H;
        for i in 0..MODEL_W {
            let si = (i * w + w / 2) / MODEL_W;
            let y = f32::from(picture.y[sj * w + si]) - 16.0;
            let c = (sj / 2) * cw + si / 2;
            let u = f32::from(picture.u[c]) - 128.0;
            let v = f32::from(picture.v[c]) - 128.0;
            let p = (j * MODEL_W + i) * 3;
            out[p] = ((1.164 * y + 1.596 * v) / 255.0).clamp(0.0, 1.0);
            out[p + 1] = ((1.164 * y - 0.392 * u - 0.813 * v) / 255.0).clamp(0.0, 1.0);
            out[p + 2] = ((1.164 * y + 2.017 * u) / 255.0).clamp(0.0, 1.0);
        }
    }
}

/// Blurs one `w` × `h` plane behind the person: `mask` is the model's
/// (`MODEL_W` × `MODEL_H`), `person_full` how much each pixel of the
/// luma plane is the person, of which this plane takes every `step`th.
fn blur_plane(
    plane: &mut [u8],
    w: usize,
    h: usize,
    mask: &[f32],
    person_full: &[f32],
    step: usize,
    radius: usize,
) {
    let (sw, sh) = (w / SHRINK, h / SHRINK);
    let small = shrink(plane, w, h, SHRINK);
    // How much each shrunk pixel is background, to weigh the blur by.
    let weight = scaled(mask, MODEL_W, MODEL_H, sw, sh, |m| 1.0 - person(m));
    let background = background(&small, &weight, sw, sh, radius);
    let full_w = w * step;
    composite(plane, w, h, &background, sw, sh, |i, j| {
        person_full[j * step * full_w + i * step]
    });
}

/// The blurred background of a shrunk plane: each pixel the average of
/// the background around it, weighted by `weight` (1 background, 0
/// person), so the person does not bleed into it. Deep inside the person,
/// with no background near, it is the plane as it is: the person is kept
/// there anyway.
fn background(small: &[f32], weight: &[f32], w: usize, h: usize, radius: usize) -> Vec<f32> {
    let mut weighted: Vec<f32> = small.iter().zip(weight).map(|(p, k)| p * k).collect();
    let mut total = weight.to_vec();
    box_blur(&mut weighted, w, h, radius);
    box_blur(&mut total, w, h, radius);
    weighted
        .iter()
        .zip(&total)
        .zip(small)
        .map(|((s, k), p)| if *k > 1e-3 { s / k } else { *p })
        .collect()
}

/// The model's confidence made a little harder at the edges (from 0.3 to
/// 0.7), so the person does not look see-through.
fn person(confidence: f32) -> f32 {
    let m = ((confidence - 0.3) * 2.5).clamp(0.0, 1.0);
    m * m * (3.0 - 2.0 * m)
}

/// A plane shrunk by `k` each way, each pixel the average of its block.
fn shrink(src: &[u8], w: usize, h: usize, k: usize) -> Vec<f32> {
    let (sw, sh) = (w / k, h / k);
    let mut out = vec![0f32; sw * sh];
    let scale = 1.0 / (k * k) as f32;
    for j in 0..sh {
        for i in 0..sw {
            let mut sum = 0u32;
            for dj in 0..k {
                let at = (j * k + dj) * w + i * k;
                sum += src[at..at + k].iter().map(|&x| u32::from(x)).sum::<u32>();
            }
            out[j * sw + i] = sum as f32 * scale;
        }
    }
    out
}

/// Three box blurs of radius `r`, in place: close to a Gaussian. Running
/// sums, so a pixel costs the same whatever the radius, and the vertical
/// pass adds whole rows (in memory order) rather than walking columns.
fn box_blur(p: &mut [f32], w: usize, h: usize, r: usize) {
    let mut tmp = vec![0f32; p.len()];
    let mut sum = vec![0f32; w];
    for _ in 0..3 {
        for j in 0..h {
            let line = &p[j * w..(j + 1) * w];
            let out = &mut tmp[j * w..(j + 1) * w];
            let mut s: f32 = line[..=r.min(w - 1)].iter().sum();
            for (i, o) in out.iter_mut().enumerate() {
                let (a, b) = (i.saturating_sub(r), (i + r).min(w - 1));
                *o = s / (b - a + 1) as f32;
                // The next window gains b + 1 and, once it moves, loses i - r.
                if b + 1 < w {
                    s += line[b + 1];
                }
                if i >= r {
                    s -= line[i - r];
                }
            }
        }
        sum.fill(0.0);
        for row in tmp.chunks_exact(w).take(r.min(h - 1) + 1) {
            for (s, v) in sum.iter_mut().zip(row) {
                *s += v;
            }
        }
        for j in 0..h {
            let (a, b) = (j.saturating_sub(r), (j + r).min(h - 1));
            let n = 1.0 / (b - a + 1) as f32;
            for (o, s) in p[j * w..(j + 1) * w].iter_mut().zip(&sum) {
                *o = s * n;
            }
            if b + 1 < h {
                for (s, v) in sum.iter_mut().zip(&tmp[(b + 1) * w..(b + 2) * w]) {
                    *s += v;
                }
            }
            if j >= r {
                for (s, v) in sum.iter_mut().zip(&tmp[(j - r) * w..(j - r + 1) * w]) {
                    *s -= v;
                }
            }
        }
    }
}

/// For each of `w` output columns, the source column (of `sw`) to the
/// left of it and the right one's weight, for bilinear interpolation.
fn taps(w: usize, sw: usize) -> Vec<(usize, f32)> {
    (0..w)
        .map(|i| {
            let x = ((i as f32 + 0.5) * sw as f32 / w as f32 - 0.5).clamp(0.0, (sw - 1) as f32);
            let x0 = (x as usize).min(sw.saturating_sub(2));
            (x0, x - x0 as f32)
        })
        .collect()
}

/// Row `j` of `h` of a `sw` × `sh` source scaled bilinearly, into `out`
/// through `cols`; `tmp` holds the source row between.
#[allow(clippy::too_many_arguments)]
fn row(
    src: &[f32],
    sw: usize,
    sh: usize,
    j: usize,
    h: usize,
    cols: &[(usize, f32)],
    tmp: &mut [f32],
    out: &mut [f32],
) {
    let y = ((j as f32 + 0.5) * sh as f32 / h as f32 - 0.5).clamp(0.0, (sh - 1) as f32);
    let y0 = (y as usize).min(sh.saturating_sub(2));
    let fy = y - y0 as f32;
    let y1 = (y0 + 1).min(sh - 1);
    for x in 0..sw {
        let (a, b) = (src[y0 * sw + x], src[y1 * sw + x]);
        tmp[x] = a + (b - a) * fy;
    }
    for (o, &(x0, fx)) in out.iter_mut().zip(cols) {
        let x1 = (x0 + 1).min(sw - 1);
        *o = tmp[x0] + (tmp[x1] - tmp[x0]) * fx;
    }
}

/// A `sw` × `sh` source scaled bilinearly to `w` × `h`, each value passed
/// through `f`.
fn scaled(
    src: &[f32],
    sw: usize,
    sh: usize,
    w: usize,
    h: usize,
    f: impl Fn(f32) -> f32,
) -> Vec<f32> {
    let cols = taps(w, sw);
    let mut tmp = vec![0f32; sw];
    let mut out = vec![0f32; w * h];
    for (j, line) in out.chunks_exact_mut(w).enumerate() {
        row(src, sw, sh, j, h, &cols, &mut tmp, line);
        for v in line.iter_mut() {
            *v = f(*v);
        }
    }
    out
}

/// Keeps the person in `plane` (as much as `person(i, j)` says) and takes
/// the `bw` × `bh` blurred `background`, scaled up a row at a time,
/// elsewhere.
fn composite(
    plane: &mut [u8],
    w: usize,
    h: usize,
    background: &[f32],
    bw: usize,
    bh: usize,
    person: impl Fn(usize, usize) -> f32,
) {
    let bcols = taps(w, bw);
    let mut bt = vec![0f32; bw];
    let mut brow = vec![0f32; w];
    for j in 0..h {
        row(background, bw, bh, j, h, &bcols, &mut bt, &mut brow);
        for (i, (px, &bg)) in plane[j * w..(j + 1) * w].iter_mut().zip(&brow).enumerate() {
            let m = person(i, j);
            *px = (bg + (f32::from(*px) - bg) * m + 0.5).clamp(0.0, 255.0) as u8;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn picture(w: u32, h: u32, y: u8) -> Planes {
        let (w_, h_) = (w as usize, h as usize);
        Planes {
            width: w,
            height: h,
            y: vec![y; w_ * h_],
            u: vec![128; (w_ / 2) * (h_ / 2)],
            v: vec![128; (w_ / 2) * (h_ / 2)],
        }
    }

    #[test]
    fn the_model_loads_and_says_where_nobody_is() {
        let mut blur = Blur::new().expect("the built-in model loads");
        // A flat grey picture: nobody in it.
        let mask = blur.segment(&picture(640, 480, 120)).expect("runs");
        assert_eq!(mask.len(), MODEL_W * MODEL_H);
        assert!(mask.iter().all(|v| (0.0..=1.0).contains(v)));
        assert!(
            mask.iter().sum::<f32>() / (mask.len() as f32) < 0.2,
            "mostly background"
        );
    }

    #[test]
    fn a_flat_picture_stays_flat() {
        let mut blur = Blur::new().expect("loads");
        let mut p = picture(640, 480, 120);
        blur.apply(&mut p);
        assert!(p.y.iter().all(|&v| v.abs_diff(120) <= 1));
        assert!(p.u.iter().chain(&p.v).all(|&v| v.abs_diff(128) <= 1));
    }

    #[test]
    fn odd_and_tiny_pictures_are_left_alone() {
        let mut blur = Blur::new().expect("loads");
        let mut tiny = picture(8, 8, 50);
        blur.apply(&mut tiny);
        assert!(tiny.y.iter().all(|&v| v == 50));
        let mut wrong = picture(640, 480, 50);
        wrong.u.pop();
        blur.apply(&mut wrong);
        assert!(wrong.y.iter().all(|&v| v == 50));
    }

    #[test]
    fn the_running_sums_blur_as_a_box_does() {
        let (w, h, r) = (23, 17, 3);
        let src: Vec<f32> = (0..w * h).map(|i| ((i * 37) % 101) as f32).collect();
        let mut fast = src.clone();
        box_blur(&mut fast, w, h, r);
        // The same three passes, summed the slow way.
        let mut slow = src;
        let mut tmp = vec![0f32; slow.len()];
        for _ in 0..3 {
            for j in 0..h {
                for i in 0..w {
                    let (a, b) = (i.saturating_sub(r), (i + r).min(w - 1));
                    tmp[j * w + i] =
                        (a..=b).map(|x| slow[j * w + x]).sum::<f32>() / (b - a + 1) as f32;
                }
            }
            for j in 0..h {
                let (a, b) = (j.saturating_sub(r), (j + r).min(h - 1));
                for i in 0..w {
                    slow[j * w + i] =
                        (a..=b).map(|y| tmp[y * w + i]).sum::<f32>() / (b - a + 1) as f32;
                }
            }
        }
        for (f, s) in fast.iter().zip(&slow) {
            assert!((f - s).abs() < 1e-2, "{f} against {s}");
        }
    }

    #[test]
    fn the_person_does_not_bleed_into_the_background() {
        // A bright person (weight 0) in the middle of a dark background:
        // the background beside them stays dark.
        let (w, h) = (40, 30);
        let small: Vec<f32> = (0..w * h)
            .map(|i| {
                if (15..25).contains(&(i % w)) {
                    250.0
                } else {
                    10.0
                }
            })
            .collect();
        let weight: Vec<f32> = (0..w * h)
            .map(|i| {
                if (15..25).contains(&(i % w)) {
                    0.0
                } else {
                    1.0
                }
            })
            .collect();
        let bg = background(&small, &weight, w, h, 3);
        let beside = bg[15 * w + 13];
        assert!(beside < 20.0, "background beside the person is {beside}");
        // A plain blur would have brought the person's brightness in.
        let mut plain = small.clone();
        box_blur(&mut plain, w, h, 3);
        assert!(plain[15 * w + 13] > 60.0);
    }

    #[test]
    fn the_person_is_kept_and_the_rest_replaced() {
        let (w, h) = (64, 48);
        let mut plane = vec![200u8; w * h];
        let bg = vec![10f32; (w / SHRINK) * (h / SHRINK)];
        // The left half is the person.
        composite(&mut plane, w, h, &bg, w / SHRINK, h / SHRINK, |i, _| {
            if i < w / 2 { 1.0 } else { 0.0 }
        });
        assert_eq!(plane[h / 2 * w + 10], 200, "the person");
        assert_eq!(plane[h / 2 * w + w - 10], 10, "the background");
    }
}
