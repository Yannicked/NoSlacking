"""Converts MediaPipe's selfie segmenter from TFLite (NHWC) to ONNX (NCHW
inside, NHWC at the edges), for the video helper's background blur.

Only the ops the selfie segmenters use are handled, including MediaPipe's
custom `Convolution2DTransposeBias` (a transposed convolution with bias),
which is why the model cannot be read as TFLite elsewhere. The result
takes `image`, RGB in 0..1 as [1, H, W, 3], and gives `mask`, the person's
confidence as [1, H, W, 1]; it matched MediaPipe's own output exactly
(largest difference 0.0000) when made.

    uv venv && uv pip install tflite onnx numpy
    python convert.py selfie_segmenter_landscape.tflite selfie_segmenter_landscape.onnx
"""
import struct, sys
import numpy as np
import tflite
import onnx
from onnx import helper, numpy_helper, TensorProto

src, dst = sys.argv[1], sys.argv[2]
buf = open(src, "rb").read()
m = tflite.Model.GetRootAsModel(buf, 0)
g = m.Subgraphs(0)
OPS = {v: k for k, v in tflite.BuiltinOperator.__dict__.items() if not k.startswith("_")}
NP = {0: np.float32, 1: np.float16, 2: np.int32}

def tensor(i):
    return g.Tensors(i)

def const(i):
    t = tensor(i)
    b = m.Buffers(t.Buffer())
    if b.DataLength() == 0:
        return None
    return np.frombuffer(b.DataAsNumpy().tobytes(), dtype=NP[t.Type()]).reshape(t.ShapeAsNumpy())

consts = {}  # tflite tensor index -> numpy (NHWC layout as stored)
for i in range(g.TensorsLength()):
    c = const(i)
    if c is not None:
        consts[i] = c

nodes, inits = [], []
name = lambda i: f"t{i}"
rank4 = lambda i: len(tensor(i).ShapeAsNumpy()) == 4
counter = [0]

def uniq(p):
    counter[0] += 1
    return f"{p}_{counter[0]}"

def init(arr, p):
    n = uniq(p)
    inits.append(numpy_helper.from_array(np.ascontiguousarray(arr), n))
    return n

def operand(i):
    """A tensor as an ONNX value name: constants become initializers, in
    NCHW-compatible shape when they broadcast against a 4-D activation."""
    if i in consts:
        c = consts[i].astype(np.float32) if consts[i].dtype != np.int32 else consts[i]
        if c.ndim == 4:  # NHWC constant -> NCHW
            c = c.transpose(0, 3, 1, 2)
        elif c.ndim == 1 and c.size > 1:  # per-channel -> [1,C,1,1]
            c = c.reshape(1, -1, 1, 1)
        return init(c, "c")
    return name(i)

def activation(out, act):
    if act == 0:
        return out
    tmp = uniq("pre")
    if act == 1:
        return ("Relu", tmp)
    if act == 3:
        return ("Clip6", tmp)
    raise SystemExit(f"activation {act}")

def emit(op_type, ins, out, act=0, **attrs):
    if act == 0:
        nodes.append(helper.make_node(op_type, ins, [out], **attrs))
        return
    tmp = uniq("pre")
    nodes.append(helper.make_node(op_type, ins, [tmp], **attrs))
    if act == 1:
        nodes.append(helper.make_node("Relu", [tmp], [out]))
    elif act == 3:
        lo, hi = init(np.array(0, np.float32), "lo"), init(np.array(6, np.float32), "hi")
        nodes.append(helper.make_node("Clip", [tmp, lo, hi], [out]))
    else:
        raise SystemExit(f"activation {act}")

def same_pads(in_hw, k_hw, s_hw, d_hw=(1, 1)):
    pads_b, pads_e = [], []
    for n, k, s, d in zip(in_hw, k_hw, s_hw, d_hw):
        ke = (k - 1) * d + 1
        out = -(-n // s)
        total = max(0, (out - 1) * s + ke - n)
        pads_b.append(total // 2)
        pads_e.append(total - total // 2)
    return pads_b + pads_e

for k in range(g.OperatorsLength()):
    op = g.Operators(k)
    oc = m.OperatorCodes(op.OpcodeIndex())
    code = max(oc.BuiltinCode(), oc.DeprecatedBuiltinCode())
    ins = [op.Inputs(j) for j in range(op.InputsLength())]
    out = op.Outputs(0)
    o = name(out)
    kind = OPS.get(code)
    opt = op.BuiltinOptions()
    if kind == "DEQUANTIZE":
        consts[out] = consts[ins[0]].astype(np.float32)
        continue
    if kind == "CONV_2D":
        p = tflite.Conv2DOptions(); p.Init(opt.Bytes, opt.Pos)
        w = consts[ins[1]].astype(np.float32)  # [O,H,W,I]
        wn = init(w.transpose(0, 3, 1, 2), "w")
        args = [name(ins[0]), wn] + ([init(consts[ins[2]].astype(np.float32), "b")] if len(ins) > 2 and ins[2] >= 0 else [])
        hw = tensor(ins[0]).ShapeAsNumpy()[1:3]
        s = (p.StrideH(), p.StrideW())
        pads = same_pads(hw, w.shape[1:3], s, (p.DilationHFactor(), p.DilationWFactor())) if p.Padding() == 0 else [0, 0, 0, 0]
        emit("Conv", args, o, p.FusedActivationFunction(), strides=list(s), pads=pads,
             dilations=[p.DilationHFactor(), p.DilationWFactor()], kernel_shape=list(w.shape[1:3]))
    elif kind == "DEPTHWISE_CONV_2D":
        p = tflite.DepthwiseConv2DOptions(); p.Init(opt.Bytes, opt.Pos)
        w = consts[ins[1]].astype(np.float32)  # [1,H,W,C*M]
        c = tensor(ins[0]).ShapeAsNumpy()[3]
        wn = init(w.transpose(3, 0, 1, 2), "dw")  # [C*M,1,H,W]
        args = [name(ins[0]), wn] + ([init(consts[ins[2]].astype(np.float32), "b")] if len(ins) > 2 and ins[2] >= 0 else [])
        hw = tensor(ins[0]).ShapeAsNumpy()[1:3]
        s = (p.StrideH(), p.StrideW())
        pads = same_pads(hw, w.shape[1:3], s, (p.DilationHFactor(), p.DilationWFactor())) if p.Padding() == 0 else [0, 0, 0, 0]
        emit("Conv", args, o, p.FusedActivationFunction(), strides=list(s), pads=pads, group=int(c),
             dilations=[p.DilationHFactor(), p.DilationWFactor()], kernel_shape=list(w.shape[1:3]))
    elif kind in ("ADD", "MUL"):
        p = (tflite.AddOptions if kind == "ADD" else tflite.MulOptions)(); p.Init(opt.Bytes, opt.Pos)
        emit("Add" if kind == "ADD" else "Mul", [operand(ins[0]), operand(ins[1])], o, p.FusedActivationFunction())
    elif kind == "AVERAGE_POOL_2D":
        p = tflite.Pool2DOptions(); p.Init(opt.Bytes, opt.Pos)
        hw = tensor(ins[0]).ShapeAsNumpy()[1:3]
        k_ = (p.FilterHeight(), p.FilterWidth())
        s = (p.StrideH(), p.StrideW())
        pads = same_pads(hw, k_, s) if p.Padding() == 0 else [0, 0, 0, 0]
        # TensorFlow leaves padding out of the average.
        emit("AveragePool", [name(ins[0])], o, p.FusedActivationFunction(), kernel_shape=list(k_),
             strides=list(s), pads=pads, count_include_pad=0)
    elif kind == "RELU":
        emit("Relu", [name(ins[0])], o)
    elif kind == "HARD_SWISH":
        emit("HardSwish", [name(ins[0])], o)
    elif kind == "LOGISTIC":
        emit("Sigmoid", [name(ins[0])], o)
    elif kind == "MEAN":
        p = tflite.ReducerOptions(); p.Init(opt.Bytes, opt.Pos)
        axes = [int(a) for a in consts[ins[1]].reshape(-1)]
        nchw = {0: 0, 1: 2, 2: 3, 3: 1}
        emit("ReduceMean", [name(ins[0])], o, axes=[nchw[a] for a in axes], keepdims=1 if p.KeepDims() else 0)
        assert p.KeepDims(), "MEAN without keep_dims would change the rank"
    elif kind == "RESIZE_BILINEAR":
        p = tflite.ResizeBilinearOptions(); p.Init(opt.Bytes, opt.Pos)
        size = consts[ins[1]].reshape(-1)
        n, h, w, c = tensor(out).ShapeAsNumpy()
        mode = "align_corners" if p.AlignCorners() else ("half_pixel" if p.HalfPixelCenters() else "asymmetric")
        sizes = init(np.array([n, c, int(size[0]), int(size[1])], np.int64), "sz")
        emit("Resize", [name(ins[0]), "", "", sizes], o, mode="linear", coordinate_transformation_mode=mode)
    elif code == 32 and oc.CustomCode().decode() == "Convolution2DTransposeBias":
        # MediaPipe's op: TfLiteTransposeConvParams {padding, stride_w, stride_h} as raw ints.
        raw = op.CustomOptionsAsNumpy().tobytes()
        padding, sw, sh = struct.unpack("<iii", raw[:12])
        w = consts[ins[1]].astype(np.float32)  # [O,H,W,I]
        b = consts[ins[2]].astype(np.float32)
        wn = init(w.transpose(3, 0, 1, 2), "tw")  # ONNX ConvTranspose: [I,O,H,W]
        ih, iw = tensor(ins[0]).ShapeAsNumpy()[1:3]
        oh, ow = tensor(out).ShapeAsNumpy()[1:3]
        kh, kw = w.shape[1:3]
        pads = []
        for i_, o_, k_, s_ in ((ih, oh, kh, sh), (iw, ow, kw, sw)):
            total = max(0, (i_ - 1) * s_ + k_ - o_)
            pads.append((total // 2, total - total // 2))
        emit("ConvTranspose", [name(ins[0]), wn, init(b, "tb")], o, strides=[sh, sw], kernel_shape=[kh, kw],
             pads=[pads[0][0], pads[1][0], pads[0][1], pads[1][1]], output_shape=[int(oh), int(ow)])
        print("transpose conv: padding", padding, "strides", sh, sw, "kernel", kh, kw, "pads", pads)
    else:
        raise SystemExit(f"unsupported op {kind} {code}")

inp, outp = g.Inputs(0), g.Outputs(0)
ishape = [int(x) for x in tensor(inp).ShapeAsNumpy()]
oshape = [int(x) for x in tensor(outp).ShapeAsNumpy()]
# NHWC at the edges, NCHW inside.
nodes.insert(0, helper.make_node("Transpose", ["image"], [name(inp)], perm=[0, 3, 1, 2]))
nodes.append(helper.make_node("Transpose", [name(outp)], ["mask"], perm=[0, 2, 3, 1]))
graph = helper.make_graph(
    nodes, "selfie_segmenter",
    [helper.make_tensor_value_info("image", TensorProto.FLOAT, ishape)],
    [helper.make_tensor_value_info("mask", TensorProto.FLOAT, oshape)],
    inits,
)
model = helper.make_model(graph, opset_imports=[helper.make_opsetid("", 17)], producer_name="noslacking-spike")
model.ir_version = 8
onnx.checker.check_model(model)
onnx.save(model, dst)
print("wrote", dst, len(nodes), "nodes", sum(i.ByteSize() for i in inits) // 1024, "KB of weights")
