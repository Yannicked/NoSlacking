//! Measures encoding a camera at 640×480 and 30 pictures a second and a
//! share at 1920×1080 and 15: in software as the app does (rusty_h264's
//! encoder, set up as `src/huddle_audio/video_encoder.rs` sets it up), on
//! the GPU in this process, and through the helper over its pipe as the
//! app uses it. For each: time and CPU a picture (this process and the
//! helper's), the rate it came out at, and its luma PSNR against what
//! went in (decoded again by rusty_h264's decoder, outside the timing).
//!
//! `cargo build --release -p noslacking-video --examples --bins`, then
//! `target/release/examples/encode all target/release/noslacking-video`
//! (or `software`, `hardware` or `helper` alone).

#![allow(clippy::print_stdout, reason = "the measurements are for the reader")]

use std::io::{BufReader, BufWriter};
use std::process::{Command, Stdio};
use std::time::Instant;

use noslacking_video_ipc::{self as ipc, Codec, Planes, Reply, Request};

const SCREEN: &[u8] = include_bytes!("../../../src/huddle_audio/fixtures/screen-1920x1080.h264");
const CAMERA: &[u8] = include_bytes!("../../../src/huddle_audio/fixtures/camera-480x480.h264");
/// Each sequence this many times over, one encoder running on.
const ROUNDS: usize = 5;

/// A sequence to encode: its name, pictures, rate and bit rate.
struct Case {
    name: &'static str,
    pictures: Vec<Planes>,
    fps: u32,
    bitrate: u32,
}

/// The stream's pictures, decoded.
fn decode(stream: &[u8]) -> Vec<Planes> {
    // Cut into access units, each ending with its slice.
    let mut frames = Vec::new();
    let mut frame: Vec<u8> = Vec::new();
    let mut starts: Vec<usize> = stream
        .windows(3)
        .enumerate()
        .filter(|(_, w)| *w == [0, 0, 1])
        .map(|(i, _)| i + 3)
        .collect();
    starts.push(stream.len() + 3);
    for pair in starts.windows(2) {
        let mut end = pair[1] - 3;
        while end > pair[0] && stream[end - 1] == 0 {
            end -= 1;
        }
        let nal = &stream[pair[0]..end];
        frame.extend_from_slice(&[0, 0, 0, 1]);
        frame.extend_from_slice(nal);
        if matches!(nal.first().map(|b| b & 0x1f), Some(1 | 5)) {
            frames.push(std::mem::take(&mut frame));
        }
    }
    let mut decoder = rusty_h264_decoder::Decoder::new();
    frames
        .iter()
        .filter_map(|frame| decoder.decode(frame).ok().flatten())
        .map(|p| Planes {
            width: u32::try_from(p.width).unwrap_or(0),
            height: u32::try_from(p.height).unwrap_or(0),
            y: p.y,
            u: p.u,
            v: p.v,
        })
        .collect()
}

/// `picture` stretched to `width` wide (nearest pixel): the 480×480
/// camera as a 640×480 one.
fn widen(picture: &Planes, width: u32) -> Planes {
    let stretch = |plane: &[u8], from: usize, to: usize, rows: usize| {
        let mut out = Vec::with_capacity(to * rows);
        for row in plane.chunks_exact(from).take(rows) {
            out.extend((0..to).map(|x| row[x * from / to]));
        }
        out
    };
    let (w, h) = (picture.width as usize, picture.height as usize);
    let to = width as usize;
    Planes {
        width,
        height: picture.height,
        y: stretch(&picture.y, w, to, h),
        u: stretch(&picture.u, w / 2, to / 2, h / 2),
        v: stretch(&picture.v, w / 2, to / 2, h / 2),
    }
}

fn cases() -> Vec<Case> {
    let camera = decode(CAMERA).iter().map(|p| widen(p, 640)).collect();
    vec![
        Case {
            name: "640x480 camera at 30 fps, 900 kbit/s",
            pictures: camera,
            fps: 30,
            bitrate: 900_000,
        },
        Case {
            name: "1920x1080 share at 15 fps, 2500 kbit/s",
            pictures: decode(SCREEN),
            fps: 15,
            bitrate: 2_500_000,
        },
    ]
}

/// CPU time (user and system) process `pid` has used so far, in seconds;
/// from /proc, so Linux only.
fn cpu(pid: u32) -> Option<f64> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let rest = &stat[stat.rfind(')')? + 2..];
    let fields: Vec<&str> = rest.split(' ').collect();
    let ticks = fields.get(11)?.parse::<f64>().ok()? + fields.get(12)?.parse::<f64>().ok()?;
    Some(ticks / 100.0)
}

fn cpu_now(child: Option<u32>) -> f64 {
    cpu(std::process::id()).unwrap_or(0.0) + child.and_then(cpu).unwrap_or(0.0)
}

fn psnr(a: &[u8], b: &[u8]) -> f64 {
    let mse = a
        .iter()
        .zip(b)
        .map(|(&x, &y)| (f64::from(x) - f64::from(y)).powi(2))
        .sum::<f64>()
        / a.len() as f64;
    if mse == 0.0 {
        99.0
    } else {
        10.0 * (255.0 * 255.0 / mse).log10()
    }
}

/// Runs `encode` over the case, ROUNDS times, and prints what it cost.
fn measure(
    case: &Case,
    child: Option<u32>,
    mut encode: impl FnMut(&Planes, bool) -> (Vec<u8>, bool),
) {
    let mut stream = Vec::new();
    let mut keyframes = 0;
    let count = ROUNDS * case.pictures.len();
    let (started, before) = (Instant::now(), cpu_now(child));
    for _ in 0..ROUNDS {
        for picture in &case.pictures {
            let (data, keyframe) = encode(picture, false);
            keyframes += usize::from(keyframe);
            stream.push(data);
        }
    }
    let per = started.elapsed().as_secs_f64() * 1000.0 / count as f64;
    let cpu = (cpu_now(child) - before) * 1000.0 / count as f64;
    let bytes: usize = stream.iter().map(Vec::len).sum();
    let kbps = bytes as f64 * 8.0 * f64::from(case.fps) / count as f64 / 1000.0;
    let mut decoder = rusty_h264_decoder::Decoder::new();
    let mut quality = 0.0;
    for (data, picture) in stream.iter().zip(case.pictures.iter().cycle()) {
        let decoded = decoder.decode(data).ok().flatten().expect("decodes");
        quality += psnr(&decoded.y, &picture.y);
    }
    println!(
        "  {}: {per:.2} ms a picture, {cpu:.2} ms of CPU, {kbps:.0} kbit/s, luma PSNR {:.1} dB, \
         {keyframes} keyframes",
        case.name,
        quality / count as f64
    );
}

fn software() {
    use rusty_h264_common::YuvPlanes;
    use rusty_h264_encoder::{Encoder, EncoderConfig, Preset};
    println!("software (rusty_h264's encoder, set up as the app)");
    for case in cases() {
        let (w, h) = (case.pictures[0].width, case.pictures[0].height);
        let mut config = EncoderConfig::baseline(w as usize, h as usize);
        config.preset = Preset::Fast;
        config.gop_size = case.fps * 4;
        config.min_keyint = 1;
        config.framerate = case.fps as f32;
        config.bitrate = case.bitrate;
        config.qp = 30;
        config.level_idc = if w * h > 1280 * 720 { 40 } else { 31 };
        let mut encoder = Encoder::new(config).expect("an encoder");
        measure(&case, None, |picture, _| {
            let planes =
                YuvPlanes::tight(w as usize, h as usize, &picture.y, &picture.u, &picture.v)
                    .expect("whole planes");
            let data = encoder.encode_planes(&planes).expect("encodes");
            let keyframe = data
                .windows(4)
                .any(|w| w[..3] == [0, 0, 1] && w[3] & 0x1f == 5);
            (data, keyframe)
        });
    }
}

fn in_process() {
    let mut backend = noslacking_video::choose_backend();
    println!("on the GPU in this process, {}", backend.name());
    for case in cases() {
        let (w, h) = (case.pictures[0].width, case.pictures[0].height);
        let Ok(mut encoder) = backend.open_encoder(Codec::H264, w, h, case.fps, case.bitrate)
        else {
            println!("  {}: no encoder", case.name);
            continue;
        };
        measure(&case, None, |picture, force| {
            let encoded = encoder.encode(picture, force).expect("encodes");
            (encoded.data, encoded.keyframe)
        });
    }
}

fn through_the_helper(helper: &str) {
    let mut child = Command::new(helper)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("the helper starts");
    let pid = Some(child.id());
    let mut input = BufWriter::new(child.stdin.take().expect("stdin"));
    let mut output = BufReader::new(child.stdout.take().expect("stdout"));
    let mut seq = 0;
    let mut call = |request: Request| -> Reply {
        seq += 1;
        ipc::write_request(&mut input, seq, &request).expect("sent");
        let (_, reply) = ipc::read_reply(&mut output)
            .expect("read")
            .expect("a reply");
        reply
    };
    call(Request::Hello {
        version: ipc::VERSION,
    });
    println!("through the helper");
    for case in cases() {
        let (w, h) = (case.pictures[0].width, case.pictures[0].height);
        let Reply::Opened { id } = call(Request::OpenEncoder {
            codec: Codec::H264,
            width: w,
            height: h,
            fps: case.fps,
            bitrate: case.bitrate,
        }) else {
            println!("  {}: no encoder", case.name);
            continue;
        };
        measure(&case, pid, |picture, force| {
            // The app hands over its own copy, as it does each picture.
            let reply = call(Request::Encode {
                id,
                force_keyframe: force,
                picture: picture.clone(),
            });
            let Reply::Encoded { keyframe, data } = reply else {
                panic!("{reply:?}");
            };
            (data, keyframe)
        });
        call(Request::Close { id });
    }
    drop(input);
    let _ = child.wait();
}

fn main() {
    let what = std::env::args().nth(1).unwrap_or_else(|| "all".into());
    let helper = std::env::args().nth(2);
    if matches!(what.as_str(), "all" | "software") {
        software();
    }
    if matches!(what.as_str(), "all" | "hardware") {
        in_process();
    }
    if let Some(helper) = &helper
        && matches!(what.as_str(), "all" | "helper")
    {
        through_the_helper(helper);
    }
}
