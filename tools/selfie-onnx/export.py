"""Exports MediaPipe's selfie segmenter from TFLite to the video helper's
own small format (`crates/noslacking-video/src/segment.rs` reads it), so
the helper runs it with no model runtime at all.

The format, little-endian:

    b"NSSEG1", u32 input, u32 output         (tensor ids)
    u32 tensors, then each: u16 h, w, c       (activations, batch 1, NHWC)
    u32 ops, then each: u8 kind and its fields

    kind 1 conv:       u32 in, out; u8 kh, kw, stride, pad_top, pad_left,
                       act; u16 cin, cout; f32 weights [kh][kw][cin][cout],
                       f32 bias [cout]
    kind 2 depthwise:  u32 in, out; u8 kh, kw, stride, pad_top, pad_left,
                       act; u16 c; f32 weights [kh][kw][c], f32 bias [c]
    kind 3 add, 4 mul: u32 a, b, out; u8 act  (mul: b may be 1x1xC)
    kind 5 relu, 6 hard swish, 7 sigmoid, 8 mean over h and w,
    9 bilinear resize (half-pixel centres, to the output's size):
                       u32 in, out
    kind 10 transposed conv, kernel = stride, no padding:
                       u32 in, out; u8 k; u16 cin, cout;
                       f32 weights [kh][kw][cin][cout], f32 bias [cout]

    act: 0 none, 1 relu, 3 relu6 (TFLite's numbers), 100 hard swish (ours:
    a RELU or HARD_SWISH layer right after a convolution, reading only it,
    is folded into the convolution, saving a pass over memory).

    uv venv && uv pip install tflite numpy
    python export.py selfie_segmenter_landscape.tflite selfie_segmenter_landscape.nsseg
"""
import struct
import sys

import numpy as np
import tflite

src, dst = sys.argv[1], sys.argv[2]
buf = open(src, "rb").read()
m = tflite.Model.GetRootAsModel(buf, 0)
g = m.Subgraphs(0)
OPS = {v: k for k, v in tflite.BuiltinOperator.__dict__.items() if not k.startswith("_")}
NP = {0: np.float32, 1: np.float16, 2: np.int32}

consts = {}
for i in range(g.TensorsLength()):
    t = g.Tensors(i)
    b = m.Buffers(t.Buffer())
    if b.DataLength():
        consts[i] = np.frombuffer(b.DataAsNumpy().tobytes(), dtype=NP[t.Type()]).reshape(t.ShapeAsNumpy())

ids = {}  # tflite tensor -> our activation id


def act_id(i):
    if i not in ids:
        ids[i] = len(ids)
    return ids[i]


def shape(i):
    s = [int(x) for x in g.Tensors(i).ShapeAsNumpy()]
    assert len(s) == 4 and s[0] == 1, s
    return s[1:]


def same_pad(n, k, s):
    out = -(-n // s)
    total = max(0, (out - 1) * s + k - n)
    return total // 2


# Which layers read each tensor, to fold an activation into the
# convolution before it only when nothing else reads the convolution.
readers = {}
for k in range(g.OperatorsLength()):
    op = g.Operators(k)
    for j in range(op.InputsLength()):
        readers[op.Inputs(j)] = readers.get(op.Inputs(j), 0) + 1

ops = []  # [kind, tflite output, record]


def add(kind, out, record):
    ops.append([kind, out, bytearray(record)])


for k in range(g.OperatorsLength()):
    op = g.Operators(k)
    oc = m.OperatorCodes(op.OpcodeIndex())
    code = max(oc.BuiltinCode(), oc.DeprecatedBuiltinCode())
    kind = OPS.get(code) if code != 32 else oc.CustomCode().decode()
    ins = [op.Inputs(j) for j in range(op.InputsLength())]
    out = op.Outputs(0)
    opt = op.BuiltinOptions()
    if kind == "DEQUANTIZE":
        consts[out] = consts[ins[0]].astype(np.float32)
        continue
    w = lambda i: consts[i].astype(np.float32)
    if kind == "CONV_2D":
        p = tflite.Conv2DOptions(); p.Init(opt.Bytes, opt.Pos)
        assert p.DilationHFactor() == 1 and p.DilationWFactor() == 1 and p.StrideH() == p.StrideW()
        wt = w(ins[1])  # [cout, kh, kw, cin]
        cout, kh, kw, cin = wt.shape
        h, wd, _ = shape(ins[0])
        pt, pl = (same_pad(h, kh, p.StrideH()), same_pad(wd, kw, p.StrideW())) if p.Padding() == 0 else (0, 0)
        bias = w(ins[2]) if len(ins) > 2 and ins[2] >= 0 else np.zeros(cout, np.float32)
        add(1, out, struct.pack("<BIIBBBBBBHH", 1, act_id(ins[0]), act_id(out), kh, kw, p.StrideH(), pt, pl,
                               p.FusedActivationFunction(), cin, cout)
                   + wt.transpose(1, 2, 3, 0).astype("<f4").tobytes() + bias.astype("<f4").tobytes())
    elif kind == "DEPTHWISE_CONV_2D":
        p = tflite.DepthwiseConv2DOptions(); p.Init(opt.Bytes, opt.Pos)
        assert p.DepthMultiplier() == 1 and p.StrideH() == p.StrideW()
        wt = w(ins[1])  # [1, kh, kw, c]
        _, kh, kw, c = wt.shape
        h, wd, _ = shape(ins[0])
        pt, pl = (same_pad(h, kh, p.StrideH()), same_pad(wd, kw, p.StrideW())) if p.Padding() == 0 else (0, 0)
        bias = w(ins[2]) if len(ins) > 2 and ins[2] >= 0 else np.zeros(c, np.float32)
        add(2, out, struct.pack("<BIIBBBBBBH", 2, act_id(ins[0]), act_id(out), kh, kw, p.StrideH(), pt, pl,
                               p.FusedActivationFunction(), c)
                   + wt[0].astype("<f4").tobytes() + bias.astype("<f4").tobytes())
    elif kind in ("ADD", "MUL"):
        p = (tflite.AddOptions if kind == "ADD" else tflite.MulOptions)(); p.Init(opt.Bytes, opt.Pos)
        a, b = ins
        assert a not in consts and b not in consts
        if shape(a) != shape(b) and shape(a)[:2] == [1, 1]:
            a, b = b, a  # the full activation first, the per-channel one second
        add(0, out, struct.pack("<BIIIB", 3 if kind == "ADD" else 4, act_id(a), act_id(b), act_id(out),
                               p.FusedActivationFunction()))
    elif kind in ("RELU", "HARD_SWISH", "LOGISTIC", "MEAN", "RESIZE_BILINEAR"):
        if kind == "MEAN":
            p = tflite.ReducerOptions(); p.Init(opt.Bytes, opt.Pos)
            assert p.KeepDims() and sorted(int(x) for x in consts[ins[1]].reshape(-1)) == [1, 2]
        if kind == "RESIZE_BILINEAR":
            p = tflite.ResizeBilinearOptions(); p.Init(opt.Bytes, opt.Pos)
            assert p.HalfPixelCenters() and not p.AlignCorners()
        prev = ops[-1] if ops else None
        if (kind in ("RELU", "HARD_SWISH") and prev is not None and prev[0] in (1, 2)
                and prev[1] == ins[0] and readers.get(ins[0], 0) == 1 and prev[2][14] == 0):
            # Folded into the convolution before: its act byte, its output.
            prev[2][14] = 1 if kind == "RELU" else 100
            prev[2][5:9] = struct.pack("<I", act_id(out))
            prev[1] = out
            continue
        k_ = {"RELU": 5, "HARD_SWISH": 6, "LOGISTIC": 7, "MEAN": 8, "RESIZE_BILINEAR": 9}[kind]
        add(0, out, struct.pack("<BII", k_, act_id(ins[0]), act_id(out)))
    elif code == 32 and kind == "Convolution2DTransposeBias":
        padding, sw, sh = struct.unpack("<iii", op.CustomOptionsAsNumpy().tobytes()[:12])
        wt = w(ins[1])  # [cout, kh, kw, cin]
        cout, kh, kw, cin = wt.shape
        assert kh == kw == sh == sw, "kernel equal to the stride: no overlap, no padding"
        add(0, out, struct.pack("<BIIBHH", 10, act_id(ins[0]), act_id(out), kh, cin, cout)
                   + wt.transpose(1, 2, 3, 0).astype("<f4").tobytes() + w(ins[2]).astype("<f4").tobytes())
    else:
        raise SystemExit(f"unsupported op {kind}")

inp, outp = g.Inputs(0), g.Outputs(0)
shapes = [None] * len(ids)
for t, i in ids.items():
    shapes[i] = shape(t)
blob = b"NSSEG1" + struct.pack("<III", ids[inp], ids[outp], len(shapes))
blob += b"".join(struct.pack("<HHH", *s) for s in shapes)
blob += struct.pack("<I", len(ops)) + b"".join(bytes(r) for _, _, r in ops)
open(dst, "wb").write(blob)
print("wrote", dst, len(ops), "ops", len(shapes), "tensors", len(blob) // 1024, "KB")
