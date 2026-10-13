//! The person segmentation model, run by our own code: MediaPipe's selfie
//! segmenter (a MobileNetV3-style network) needs only a dozen kinds of
//! layer, and a general model runtime ran them ten times slower than
//! they can go (tract's depthwise convolutions and resizes). Exported from
//! Google's TFLite file by `tools/selfie-onnx/export.py`, whose docstring
//! describes the format; the layers come in the order they run.
//!
//! Tensors are batch 1, NHWC: a pixel's channels sit side by side, so
//! every kernel works across channels eight at a time (`wide`'s `f32x8`:
//! SIMD on every processor, without unsafe code here).

use wide::f32x8;

/// A tensor's size: height, width, channels.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Shape {
    h: usize,
    w: usize,
    c: usize,
}

impl Shape {
    fn len(self) -> usize {
        self.h * self.w * self.c
    }
}

/// An activation fused into a layer: TFLite's numbering, and hard swish
/// (100), which the exporter folds into the convolution before it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Act {
    None,
    Relu,
    Relu6,
    HardSwish,
}

impl Act {
    fn from(code: u8) -> Result<Self, String> {
        match code {
            0 => Ok(Self::None),
            1 => Ok(Self::Relu),
            3 => Ok(Self::Relu6),
            100 => Ok(Self::HardSwish),
            other => Err(format!("activation {other}")),
        }
    }

    fn apply(self, v: f32x8) -> f32x8 {
        match self {
            Self::None => v,
            Self::Relu => v.max(f32x8::ZERO),
            Self::Relu6 => v.max(f32x8::ZERO).min(f32x8::splat(6.0)),
            Self::HardSwish => hard_swish(v),
        }
    }

    fn apply1(self, v: f32) -> f32 {
        match self {
            Self::None => v,
            Self::Relu => v.max(0.0),
            Self::Relu6 => v.clamp(0.0, 6.0),
            Self::HardSwish => v * (v + 3.0).clamp(0.0, 6.0) / 6.0,
        }
    }
}

/// `x * relu6(x + 3) / 6`, MobileNetV3's activation.
fn hard_swish(v: f32x8) -> f32x8 {
    v * (v + f32x8::splat(3.0))
        .max(f32x8::ZERO)
        .min(f32x8::splat(6.0))
        * f32x8::splat(1.0 / 6.0)
}

/// A convolution's geometry.
#[derive(Clone, Copy, Debug)]
struct Window {
    kh: usize,
    kw: usize,
    stride: usize,
    pad_top: usize,
    pad_left: usize,
}

/// One layer: which tensors it reads and writes, and its weights.
#[derive(Debug)]
enum Op {
    /// Weights `[kh][kw][cin][cout]`.
    Conv {
        input: usize,
        output: usize,
        window: Window,
        act: Act,
        weights: Vec<f32>,
        bias: Vec<f32>,
    },
    /// Weights `[kh][kw][c]`.
    Depthwise {
        input: usize,
        output: usize,
        window: Window,
        act: Act,
        weights: Vec<f32>,
        bias: Vec<f32>,
    },
    Add {
        a: usize,
        b: usize,
        output: usize,
        act: Act,
    },
    /// `b` the same size as `a`, or one value per channel.
    Mul {
        a: usize,
        b: usize,
        output: usize,
        act: Act,
    },
    Relu {
        input: usize,
        output: usize,
    },
    HardSwish {
        input: usize,
        output: usize,
    },
    Sigmoid {
        input: usize,
        output: usize,
    },
    /// The average of each channel over the whole picture.
    Mean {
        input: usize,
        output: usize,
    },
    /// Bilinear, half-pixel centres, to the output's size.
    Resize {
        input: usize,
        output: usize,
    },
    /// Kernel the size of the stride (no overlap, no padding); weights
    /// `[k][k][cin][cout]`.
    Transposed {
        input: usize,
        output: usize,
        k: usize,
        weights: Vec<f32>,
        bias: Vec<f32>,
    },
}

/// The model, ready to run, with room for every tensor it makes.
#[derive(Debug)]
pub struct Segmenter {
    shapes: Vec<Shape>,
    ops: Vec<Op>,
    buffers: Vec<Vec<f32>>,
    input: usize,
    output: usize,
}

/// Reads the exported format.
struct Reader<'a> {
    bytes: &'a [u8],
}

impl Reader<'_> {
    fn take(&mut self, n: usize) -> Result<&[u8], String> {
        if self.bytes.len() < n {
            return Err("the model ends early".into());
        }
        let (head, rest) = self.bytes.split_at(n);
        self.bytes = rest;
        Ok(head)
    }

    fn u8(&mut self) -> Result<u8, String> {
        Ok(self.take(1)?[0])
    }

    fn u16(&mut self) -> Result<usize, String> {
        let b = self.take(2)?;
        Ok(usize::from(u16::from_le_bytes([b[0], b[1]])))
    }

    fn u32(&mut self) -> Result<usize, String> {
        let b = self.take(4)?;
        usize::try_from(u32::from_le_bytes([b[0], b[1], b[2], b[3]])).map_err(|e| e.to_string())
    }

    fn floats(&mut self, n: usize) -> Result<Vec<f32>, String> {
        let b = self.take(n.checked_mul(4).ok_or("too many weights")?)?;
        Ok(b.as_chunks::<4>()
            .0
            .iter()
            .map(|c| f32::from_le_bytes(*c))
            .collect())
    }

    fn window(&mut self) -> Result<Window, String> {
        Ok(Window {
            kh: usize::from(self.u8()?),
            kw: usize::from(self.u8()?),
            stride: usize::from(self.u8()?),
            pad_top: usize::from(self.u8()?),
            pad_left: usize::from(self.u8()?),
        })
    }
}

impl Segmenter {
    /// Reads a model exported by `tools/selfie-onnx/export.py`, checking
    /// every layer's sizes against its tensors so running it cannot go out
    /// of bounds.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, String> {
        let mut r = Reader { bytes };
        if r.take(6)? != b"NSSEG1" {
            return Err("not a segmentation model".into());
        }
        let (input, output, count) = (r.u32()?, r.u32()?, r.u32()?);
        let mut shapes = Vec::with_capacity(count);
        for _ in 0..count {
            shapes.push(Shape {
                h: r.u16()?,
                w: r.u16()?,
                c: r.u16()?,
            });
        }
        let shape = |i: usize| {
            shapes
                .get(i)
                .copied()
                .ok_or_else(|| format!("no tensor {i}"))
        };
        let mut ops = Vec::new();
        for _ in 0..r.u32()? {
            let op = match r.u8()? {
                1 => {
                    let (input, output, window) = (r.u32()?, r.u32()?, r.window()?);
                    let act = Act::from(r.u8()?)?;
                    let (cin, cout) = (r.u16()?, r.u16()?);
                    let weights = r.floats(window.kh * window.kw * cin * cout)?;
                    let bias = r.floats(cout)?;
                    let (i, o) = (shape(input)?, shape(output)?);
                    if i.c != cin || o.c != cout || !fits(i, o, window) {
                        return Err(format!("a convolution does not fit {i:?} to {o:?}"));
                    }
                    Op::Conv {
                        input,
                        output,
                        window,
                        act,
                        weights,
                        bias,
                    }
                }
                2 => {
                    let (input, output, window) = (r.u32()?, r.u32()?, r.window()?);
                    let act = Act::from(r.u8()?)?;
                    let c = r.u16()?;
                    let weights = r.floats(window.kh * window.kw * c)?;
                    let bias = r.floats(c)?;
                    let (i, o) = (shape(input)?, shape(output)?);
                    if i.c != c || o.c != c || !fits(i, o, window) {
                        return Err(format!(
                            "a depthwise convolution does not fit {i:?} to {o:?}"
                        ));
                    }
                    Op::Depthwise {
                        input,
                        output,
                        window,
                        act,
                        weights,
                        bias,
                    }
                }
                kind @ (3 | 4) => {
                    let (a, b, output) = (r.u32()?, r.u32()?, r.u32()?);
                    let act = Act::from(r.u8()?)?;
                    let (sa, sb, so) = (shape(a)?, shape(b)?, shape(output)?);
                    let per_channel = kind == 4 && sb.h == 1 && sb.w == 1 && sb.c == sa.c;
                    if sa != so || (sb != sa && !per_channel) {
                        return Err(format!("{sa:?} and {sb:?} do not combine"));
                    }
                    if kind == 3 {
                        Op::Add { a, b, output, act }
                    } else {
                        Op::Mul { a, b, output, act }
                    }
                }
                kind @ 5..=9 => {
                    let (input, output) = (r.u32()?, r.u32()?);
                    let (i, o) = (shape(input)?, shape(output)?);
                    let fine = match kind {
                        8 => o.h == 1 && o.w == 1 && o.c == i.c,
                        9 => o.c == i.c,
                        _ => i == o,
                    };
                    if !fine {
                        return Err(format!("layer {kind} does not fit {i:?} to {o:?}"));
                    }
                    match kind {
                        5 => Op::Relu { input, output },
                        6 => Op::HardSwish { input, output },
                        7 => Op::Sigmoid { input, output },
                        8 => Op::Mean { input, output },
                        _ => Op::Resize { input, output },
                    }
                }
                10 => {
                    let (input, output) = (r.u32()?, r.u32()?);
                    let k = usize::from(r.u8()?);
                    let (cin, cout) = (r.u16()?, r.u16()?);
                    let weights = r.floats(k * k * cin * cout)?;
                    let bias = r.floats(cout)?;
                    let (i, o) = (shape(input)?, shape(output)?);
                    if i.c != cin || o.c != cout || o.h != i.h * k || o.w != i.w * k {
                        return Err(format!(
                            "a transposed convolution does not fit {i:?} to {o:?}"
                        ));
                    }
                    Op::Transposed {
                        input,
                        output,
                        k,
                        weights,
                        bias,
                    }
                }
                other => return Err(format!("unknown layer {other}")),
            };
            ops.push(op);
        }
        shape(input)?;
        shape(output)?;
        let buffers = shapes.iter().map(|s| vec![0.0; s.len()]).collect();
        Ok(Self {
            shapes,
            ops,
            buffers,
            input,
            output,
        })
    }

    /// The picture the model takes: width, height (RGB, 0 to 1).
    pub fn input_size(&self) -> (usize, usize) {
        let s = self.shapes[self.input];
        (s.w, s.h)
    }

    /// Runs the model on `image` (`input_size`, RGB in 0..1, row by row)
    /// and answers its output: here how much each pixel is the person.
    pub fn run(&mut self, image: &[f32]) -> &[f32] {
        let into = &mut self.buffers[self.input];
        let n = into.len().min(image.len());
        into[..n].copy_from_slice(&image[..n]);
        for op in &self.ops {
            let output = match op {
                Op::Conv { output, .. }
                | Op::Depthwise { output, .. }
                | Op::Add { output, .. }
                | Op::Mul { output, .. }
                | Op::Relu { output, .. }
                | Op::HardSwish { output, .. }
                | Op::Sigmoid { output, .. }
                | Op::Mean { output, .. }
                | Op::Resize { output, .. }
                | Op::Transposed { output, .. } => *output,
            };
            // The output taken out while the inputs are read: a layer never
            // writes a tensor it reads.
            let mut out = std::mem::take(&mut self.buffers[output]);
            let so = self.shapes[output];
            let (buffers, shapes) = (&self.buffers, &self.shapes);
            match op {
                Op::Conv {
                    input,
                    window,
                    act,
                    weights,
                    bias,
                    ..
                } => {
                    conv(
                        &buffers[*input],
                        shapes[*input],
                        &mut out,
                        so,
                        *window,
                        *act,
                        weights,
                        bias,
                    );
                }
                Op::Depthwise {
                    input,
                    window,
                    act,
                    weights,
                    bias,
                    ..
                } => {
                    depthwise(
                        &buffers[*input],
                        shapes[*input],
                        &mut out,
                        so,
                        *window,
                        *act,
                        weights,
                        bias,
                    );
                }
                Op::Add { a, b, act, .. } => {
                    each2(
                        &buffers[*a],
                        &buffers[*b],
                        &mut out,
                        |x, y| act.apply(x + y),
                        |x, y| act.apply1(x + y),
                    );
                }
                Op::Mul { a, b, act, .. } => {
                    if shapes[*b].len() == so.len() {
                        each2(
                            &buffers[*a],
                            &buffers[*b],
                            &mut out,
                            |x, y| act.apply(x * y),
                            |x, y| act.apply1(x * y),
                        );
                    } else {
                        let scale = &buffers[*b];
                        for (o, x) in out
                            .chunks_exact_mut(so.c)
                            .zip(buffers[*a].chunks_exact(so.c))
                        {
                            each2(
                                x,
                                scale,
                                o,
                                |x, y| act.apply(x * y),
                                |x, y| act.apply1(x * y),
                            );
                        }
                    }
                }
                Op::Relu { input, .. } => {
                    each(
                        &buffers[*input],
                        &mut out,
                        |x| x.max(f32x8::ZERO),
                        |x| x.max(0.0),
                    );
                }
                Op::HardSwish { input, .. } => {
                    each(&buffers[*input], &mut out, hard_swish, |x| {
                        Act::HardSwish.apply1(x)
                    });
                }
                Op::Sigmoid { input, .. } => {
                    for (o, &x) in out.iter_mut().zip(&buffers[*input]) {
                        *o = 1.0 / (1.0 + (-x).exp());
                    }
                }
                Op::Mean { input, .. } => mean(&buffers[*input], shapes[*input], &mut out),
                Op::Resize { input, .. } => resize(&buffers[*input], shapes[*input], &mut out, so),
                Op::Transposed {
                    input,
                    k,
                    weights,
                    bias,
                    ..
                } => {
                    transposed(
                        &buffers[*input],
                        shapes[*input],
                        &mut out,
                        so,
                        *k,
                        weights,
                        bias,
                    );
                }
            }
            self.buffers[output] = out;
        }
        &self.buffers[self.output]
    }
}

/// Whether a convolution with `w` makes `o` from `i` without reading
/// outside it more than its padding says.
fn fits(i: Shape, o: Shape, w: Window) -> bool {
    w.stride > 0
        && w.kh > 0
        && w.kw > 0
        && o.h > 0
        && o.w > 0
        && w.pad_top < w.kh
        && w.pad_left < w.kw
        && (o.h - 1) * w.stride + w.kh <= i.h + 2 * w.kh
        && (o.w - 1) * w.stride + w.kw <= i.w + 2 * w.kw
}

fn load(s: &[f32]) -> f32x8 {
    let mut a = [0f32; 8];
    a.copy_from_slice(&s[..8]);
    f32x8::from(a)
}

fn store(s: &mut [f32], v: f32x8) {
    s[..8].copy_from_slice(&v.to_array());
}

/// `out = f(x)`, eight at a time.
#[inline(never)]
fn each(x: &[f32], out: &mut [f32], f: impl Fn(f32x8) -> f32x8, f1: impl Fn(f32) -> f32) {
    let n = out.len().min(x.len()) / 8 * 8;
    for (o, x) in out[..n]
        .as_chunks_mut::<8>()
        .0
        .iter_mut()
        .zip(x[..n].as_chunks::<8>().0)
    {
        *o = f(f32x8::from(*x)).to_array();
    }
    for (o, &x) in out[n..].iter_mut().zip(&x[n..]) {
        *o = f1(x);
    }
}

/// `out = f(x, y)`, eight at a time.
#[inline(never)]
fn each2(
    x: &[f32],
    y: &[f32],
    out: &mut [f32],
    f: impl Fn(f32x8, f32x8) -> f32x8,
    f1: impl Fn(f32, f32) -> f32,
) {
    let len = out.len().min(x.len()).min(y.len());
    let n = len / 8 * 8;
    for ((o, x), y) in out[..n]
        .as_chunks_mut::<8>()
        .0
        .iter_mut()
        .zip(x[..n].as_chunks::<8>().0)
        .zip(y[..n].as_chunks::<8>().0)
    {
        *o = f(f32x8::from(*x), f32x8::from(*y)).to_array();
    }
    for ((o, &x), &y) in out[n..len].iter_mut().zip(&x[n..len]).zip(&y[n..len]) {
        *o = f1(x, y);
    }
}

/// A convolution, NHWC, weights `[kh][kw][cin][cout]`. 1×1 ones (most of
/// them) go four pixels at a time, so each weight loaded serves four.
#[allow(clippy::too_many_arguments)]
#[inline(never)]
fn conv(
    x: &[f32],
    si: Shape,
    out: &mut [f32],
    so: Shape,
    w: Window,
    act: Act,
    weights: &[f32],
    bias: &[f32],
) {
    let (cin, cout) = (si.c, so.c);
    let vec = cout / 8 * 8;
    if w.kh == 1 && w.kw == 1 && w.stride == 1 && si.h == so.h && si.w == so.w {
        pointwise(x, so.h * so.w, cin, out, cout, act, weights, bias);
        return;
    }
    // Up to 32 output channels in registers across every tap (the input
    // layer: 3 channels in, 16 out).
    if cout % 8 == 0 && cout <= 32 {
        let n = cout / 8;
        for oy in 0..so.h {
            for ox in 0..so.w {
                let mut acc = [f32x8::ZERO; 4];
                for (a, b) in acc.iter_mut().zip(bias.as_chunks::<8>().0) {
                    *a = f32x8::from(*b);
                }
                for ky in 0..w.kh {
                    let Some(iy) = (oy * w.stride + ky)
                        .checked_sub(w.pad_top)
                        .filter(|&y| y < si.h)
                    else {
                        continue;
                    };
                    for kx in 0..w.kw {
                        let Some(ix) = (ox * w.stride + kx)
                            .checked_sub(w.pad_left)
                            .filter(|&x| x < si.w)
                        else {
                            continue;
                        };
                        let px = &x[(iy * si.w + ix) * cin..(iy * si.w + ix + 1) * cin];
                        let wk = &weights
                            [(ky * w.kw + kx) * cin * cout..(ky * w.kw + kx + 1) * cin * cout];
                        for (&v, row) in px.iter().zip(wk.chunks_exact(cout)) {
                            let s = f32x8::splat(v);
                            for (a, wv) in acc[..n].iter_mut().zip(row.as_chunks::<8>().0) {
                                *a = s.mul_add(f32x8::from(*wv), *a);
                            }
                        }
                    }
                }
                let o = &mut out[(oy * so.w + ox) * cout..(oy * so.w + ox + 1) * cout];
                for (o, a) in o.as_chunks_mut::<8>().0.iter_mut().zip(&acc[..n]) {
                    *o = act.apply(*a).to_array();
                }
            }
        }
        return;
    }
    for oy in 0..so.h {
        for ox in 0..so.w {
            let o = &mut out[(oy * so.w + ox) * cout..(oy * so.w + ox + 1) * cout];
            o.copy_from_slice(bias);
            for ky in 0..w.kh {
                let Some(iy) = (oy * w.stride + ky)
                    .checked_sub(w.pad_top)
                    .filter(|&y| y < si.h)
                else {
                    continue;
                };
                for kx in 0..w.kw {
                    let Some(ix) = (ox * w.stride + kx)
                        .checked_sub(w.pad_left)
                        .filter(|&x| x < si.w)
                    else {
                        continue;
                    };
                    let px = &x[(iy * si.w + ix) * cin..(iy * si.w + ix + 1) * cin];
                    let wk = &weights[(ky * w.kw + kx) * cin * cout..];
                    for (ci, &v) in px.iter().enumerate() {
                        let wr = &wk[ci * cout..(ci + 1) * cout];
                        let s = f32x8::splat(v);
                        for co in (0..vec).step_by(8) {
                            let sum = s.mul_add(load(&wr[co..]), load(&o[co..]));
                            store(&mut o[co..], sum);
                        }
                        for co in vec..cout {
                            o[co] += v * wr[co];
                        }
                    }
                }
            }
            for co in (0..vec).step_by(8) {
                let v = act.apply(load(&o[co..]));
                store(&mut o[co..], v);
            }
            for v in &mut o[vec..] {
                *v = act.apply1(*v);
            }
        }
    }
}

/// A 1×1 convolution over `pixels` pixels, weights `[cin][cout]`: eight
/// pixels at a time, their inputs laid out channel by channel first so
/// each weight vector loaded serves all eight.
#[inline(never)]
#[allow(clippy::too_many_arguments)]
fn pointwise(
    x: &[f32],
    pixels: usize,
    cin: usize,
    out: &mut [f32],
    cout: usize,
    act: Act,
    weights: &[f32],
    bias: &[f32],
) {
    const TILE: usize = 4;
    let vec = cout / 8 * 8;
    // The tile's inputs, `[cin][TILE]`.
    let mut tile = vec![0f32; cin * TILE];
    let mut p = 0;
    while p + TILE <= pixels {
        let block = &x[p * cin..(p + TILE) * cin];
        for (q, px) in block.chunks_exact(cin).enumerate() {
            for (ci, &v) in px.iter().enumerate() {
                tile[ci * TILE + q] = v;
            }
        }
        let columns = tile.as_chunks::<TILE>().0;
        for co in (0..vec).step_by(8) {
            let b = load(&bias[co..]);
            let mut acc = [b; TILE];
            for (row, xs) in weights.chunks_exact(cout).zip(columns) {
                let wv = load(&row[co..]);
                for (a, &v) in acc.iter_mut().zip(xs) {
                    *a = f32x8::splat(v).mul_add(wv, *a);
                }
            }
            for (q, a) in acc.iter().enumerate() {
                store(&mut out[(p + q) * cout + co..], act.apply(*a));
            }
        }
        for co in vec..cout {
            for q in 0..TILE {
                let s: f32 = columns
                    .iter()
                    .zip(weights.chunks_exact(cout))
                    .map(|(xs, row)| xs[q] * row[co])
                    .sum();
                out[(p + q) * cout + co] = act.apply1(s + bias[co]);
            }
        }
        p += TILE;
    }
    for p in p..pixels {
        let px = &x[p * cin..(p + 1) * cin];
        for co in 0..cout {
            let s: f32 = px
                .iter()
                .zip(weights.chunks_exact(cout))
                .map(|(v, row)| v * row[co])
                .sum();
            out[p * cout + co] = act.apply1(s + bias[co]);
        }
    }
}

/// A depthwise convolution, NHWC, weights `[kh][kw][c]`: each channel on
/// its own, eight channels at a time; the taps outside the picture are
/// left out per row and column, not tested per pixel.
#[allow(clippy::too_many_arguments)]
#[inline(never)]
fn depthwise(
    x: &[f32],
    si: Shape,
    out: &mut [f32],
    so: Shape,
    w: Window,
    act: Act,
    weights: &[f32],
    bias: &[f32],
) {
    let c = so.c;
    let vec = c / 8 * 8;
    for oy in 0..so.h {
        let top = oy * w.stride;
        let ky0 = w.pad_top.saturating_sub(top);
        let ky1 = w.kh.min((si.h + w.pad_top).saturating_sub(top));
        for ox in 0..so.w {
            let left = ox * w.stride;
            let kx0 = w.pad_left.saturating_sub(left);
            let kx1 = w.kw.min((si.w + w.pad_left).saturating_sub(left));
            let o = &mut out[(oy * so.w + ox) * c..(oy * so.w + ox + 1) * c];
            for ch in (0..vec).step_by(8) {
                let mut acc = load(&bias[ch..]);
                for ky in ky0..ky1 {
                    let row = (top + ky - w.pad_top) * si.w;
                    for kx in kx0..kx1 {
                        let at = (row + left + kx - w.pad_left) * c + ch;
                        acc = load(&x[at..])
                            .mul_add(load(&weights[(ky * w.kw + kx) * c + ch..]), acc);
                    }
                }
                store(&mut o[ch..], act.apply(acc));
            }
            for ch in vec..c {
                let mut acc = bias[ch];
                for ky in ky0..ky1 {
                    let row = (top + ky - w.pad_top) * si.w;
                    for kx in kx0..kx1 {
                        acc += x[(row + left + kx - w.pad_left) * c + ch]
                            * weights[(ky * w.kw + kx) * c + ch];
                    }
                }
                o[ch] = act.apply1(acc);
            }
        }
    }
}

/// Each channel's average over the picture.
#[inline(never)]
fn mean(x: &[f32], si: Shape, out: &mut [f32]) {
    let c = si.c;
    let vec = c / 8 * 8;
    out.fill(0.0);
    for px in x.chunks_exact(c) {
        for ch in (0..vec).step_by(8) {
            let v = load(&out[ch..]) + load(&px[ch..]);
            store(&mut out[ch..], v);
        }
        for ch in vec..c {
            out[ch] += px[ch];
        }
    }
    let n = 1.0 / (si.h * si.w) as f32;
    for v in out.iter_mut() {
        *v *= n;
    }
}

/// Bilinear resize with half-pixel centres, as TFLite's: a source
/// coordinate clamped below at 0 and its upper neighbour at the edge.
#[inline(never)]
fn resize(x: &[f32], si: Shape, out: &mut [f32], so: Shape) {
    let c = si.c;
    let vec = c / 8 * 8;
    let taps = |o: usize, n_in: usize, n_out: usize| {
        let at = (o as f32 + 0.5) * n_in as f32 / n_out as f32 - 0.5;
        let floor = at.floor();
        let lo = floor.max(0.0) as usize;
        let hi = (at.ceil().max(0.0) as usize).min(n_in - 1);
        (lo.min(n_in - 1), hi, at - floor)
    };
    let cols: Vec<_> = (0..so.w).map(|ox| taps(ox, si.w, so.w)).collect();
    for oy in 0..so.h {
        let (y0, y1, fy) = taps(oy, si.h, so.h);
        let (r0, r1) = (&x[y0 * si.w * c..], &x[y1 * si.w * c..]);
        for (ox, &(x0, x1, fx)) in cols.iter().enumerate() {
            let o = &mut out[(oy * so.w + ox) * c..(oy * so.w + ox + 1) * c];
            let (vfx, vfy) = (f32x8::splat(fx), f32x8::splat(fy));
            for ch in (0..vec).step_by(8) {
                let (a, b) = (load(&r0[x0 * c + ch..]), load(&r0[x1 * c + ch..]));
                let (d, e) = (load(&r1[x0 * c + ch..]), load(&r1[x1 * c + ch..]));
                let top = (b - a).mul_add(vfx, a);
                let bottom = (e - d).mul_add(vfx, d);
                store(&mut o[ch..], (bottom - top).mul_add(vfy, top));
            }
            for ch in vec..c {
                let top = r0[x0 * c + ch] + (r0[x1 * c + ch] - r0[x0 * c + ch]) * fx;
                let bottom = r1[x0 * c + ch] + (r1[x1 * c + ch] - r1[x0 * c + ch]) * fx;
                o[ch] = top + (bottom - top) * fy;
            }
        }
    }
}

/// A transposed convolution whose kernel is its stride: each input pixel
/// makes its own `k` × `k` block. With one output channel (the mask) a
/// dot product over the input channels, eight at a time.
#[inline(never)]
fn transposed(
    x: &[f32],
    si: Shape,
    out: &mut [f32],
    so: Shape,
    k: usize,
    weights: &[f32],
    bias: &[f32],
) {
    let (cin, cout) = (si.c, so.c);
    let vec = cin / 8 * 8;
    for iy in 0..si.h {
        for ix in 0..si.w {
            let px = &x[(iy * si.w + ix) * cin..(iy * si.w + ix + 1) * cin];
            for ky in 0..k {
                for kx in 0..k {
                    let o = ((iy * k + ky) * so.w + ix * k + kx) * cout;
                    let wk = &weights[(ky * k + kx) * cin * cout..];
                    for co in 0..cout {
                        let mut acc = f32x8::ZERO;
                        if cout == 1 {
                            for ci in (0..vec).step_by(8) {
                                acc = load(&px[ci..]).mul_add(load(&wk[ci..]), acc);
                            }
                        }
                        let first = if cout == 1 { vec } else { 0 };
                        let mut s = acc.reduce_add() + bias[co];
                        for ci in first..cin {
                            s += px[ci] * wk[ci * cout + co];
                        }
                        out[o + co] = s;
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_model_finds_the_person_as_mediapipe_does() {
        // A NASA astronaut portrait (public domain) at the model's size,
        // and MediaPipe's own mask for it (as bytes, 0 to 255).
        let rgb = include_bytes!("../models/fixtures/portrait_256x144.rgb");
        let expected = include_bytes!("../models/fixtures/portrait_256x144.mask");
        let mut model =
            Segmenter::from_bytes(include_bytes!("../models/selfie_segmenter_landscape.nsseg"))
                .expect("the built-in model reads");
        assert_eq!(model.input_size(), (256, 144));
        let x: Vec<f32> = rgb.iter().map(|&b| f32::from(b) / 255.0).collect();
        let mask = model.run(&x);
        assert_eq!(mask.len(), expected.len());
        let worst = mask
            .iter()
            .zip(expected)
            .map(|(m, &e)| (m * 255.0 - f32::from(e)).abs())
            .fold(0.0, f32::max);
        assert!(worst <= 1.0, "off by {worst}/255");
        let person = expected.iter().filter(|&&e| e > 127).count();
        assert!(
            (7_000..14_000).contains(&person),
            "the person covers {person} pixels"
        );
    }

    #[test]
    fn layers_that_do_not_fit_are_refused() {
        assert!(Segmenter::from_bytes(b"NOTSEG").is_err());
        assert!(Segmenter::from_bytes(b"NSSEG1").is_err(), "ends early");
        // One tensor 2x2x8, a 1x1 convolution from it to a missing one.
        let mut m = b"NSSEG1".to_vec();
        for v in [0u32, 0, 1] {
            m.extend(v.to_le_bytes());
        }
        for v in [2u16, 2, 8] {
            m.extend(v.to_le_bytes());
        }
        m.extend(1u32.to_le_bytes());
        m.push(1);
        m.extend(0u32.to_le_bytes());
        m.extend(5u32.to_le_bytes());
        m.extend([1, 1, 1, 0, 0, 0]);
        m.extend(8u16.to_le_bytes());
        m.extend(8u16.to_le_bytes());
        m.extend(vec![0u8; (64 + 8) * 4]);
        assert!(
            Segmenter::from_bytes(&m).is_err(),
            "tensor 5 does not exist"
        );
    }

    #[test]
    fn a_depthwise_convolution_matches_the_plain_sum() {
        let (si, so) = (Shape { h: 5, w: 6, c: 11 }, Shape { h: 3, w: 3, c: 11 });
        let w = Window {
            kh: 3,
            kw: 3,
            stride: 2,
            pad_top: 1,
            pad_left: 0,
        };
        let x: Vec<f32> = (0..si.len()).map(|i| ((i * 7) % 13) as f32 - 6.0).collect();
        let weights: Vec<f32> = (0..9 * 11).map(|i| ((i * 5) % 9) as f32 * 0.1).collect();
        let bias: Vec<f32> = (0..11).map(|i| i as f32).collect();
        let mut out = vec![0.0; so.len()];
        depthwise(&x, si, &mut out, so, w, Act::None, &weights, &bias);
        for oy in 0..so.h {
            for ox in 0..so.w {
                for ch in 0..11 {
                    let mut s = bias[ch];
                    for ky in 0..3 {
                        for kx in 0..3 {
                            let (iy, ix) = ((oy * 2 + ky) as isize - 1, (ox * 2 + kx) as isize);
                            if (0..5).contains(&iy) && (0..6).contains(&ix) {
                                s += x[((iy as usize) * 6 + ix as usize) * 11 + ch]
                                    * weights[(ky * 3 + kx) * 11 + ch];
                            }
                        }
                    }
                    let got = out[(oy * so.w + ox) * 11 + ch];
                    assert!((got - s).abs() < 1e-4, "{got} against {s}");
                }
            }
        }
    }

    #[test]
    fn a_pointwise_convolution_matches_the_plain_sum() {
        let (si, so) = (Shape { h: 3, w: 3, c: 5 }, Shape { h: 3, w: 3, c: 12 });
        let w = Window {
            kh: 1,
            kw: 1,
            stride: 1,
            pad_top: 0,
            pad_left: 0,
        };
        let x: Vec<f32> = (0..si.len()).map(|i| ((i * 3) % 7) as f32 - 3.0).collect();
        let weights: Vec<f32> = (0..5 * 12).map(|i| ((i * 11) % 5) as f32 * 0.25).collect();
        let bias = vec![0.5; 12];
        let mut out = vec![0.0; so.len()];
        conv(&x, si, &mut out, so, w, Act::Relu, &weights, &bias);
        for p in 0..9 {
            for co in 0..12 {
                let s: f32 = (0..5)
                    .map(|ci| x[p * 5 + ci] * weights[ci * 12 + co])
                    .sum::<f32>()
                    + 0.5;
                assert!((out[p * 12 + co] - s.max(0.0)).abs() < 1e-4);
            }
        }
    }
}
