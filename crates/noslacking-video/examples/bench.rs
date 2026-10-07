//! Measures decoding the 1080p and 480×480 fixtures at the sizes the
//! app shows them: in software in this process (rusty_h264, then a
//! whole-step shrink: what the app did itself before the helper took
//! all decoding), on the GPU in this process, and through the helper
//! over its pipe as the app uses it: on the GPU, scaled there or (with
//! `NOSLACKING_VIDEO_GPU_SCALE=0` in the helper's environment) shrunk on
//! its CPU, or in software.
//!
//! `cargo build --release -p noslacking-video --examples --bins`, then
//! `target/release/examples/bench all target/release/noslacking-video`
//! (or `software`, `hardware`, `helper`, `helper-cpu`,
//! `helper-software` or `pipe` alone, to time one under `time`).

#![allow(clippy::print_stdout, reason = "the measurements are for the reader")]

use std::io::{BufReader, BufWriter};
use std::process::{Command, Stdio};
use std::time::Instant;

use noslacking_video::backend::Decoder;
use noslacking_video::nal;
use noslacking_video::software::Software;
use noslacking_video_ipc::{self as ipc, Codec, Reply, Request};

const SCREEN: &[u8] = include_bytes!("../../../src/huddle_audio/fixtures/screen-1920x1080.h264");
const CAMERA: &[u8] = include_bytes!("../../../src/huddle_audio/fixtures/camera-480x480.h264");

/// What is measured: a stream, its size, and the box it is shown in
/// (0×0: at its own size).
type Case = (&'static str, &'static [u8], (u32, u32), (u32, u32));

const CASES: [Case; 5] = [
    ("1080p share, full size", SCREEN, (1920, 1080), (0, 0)),
    (
        "1080p share shown 960 wide",
        SCREEN,
        (1920, 1080),
        (960, 540),
    ),
    (
        "1080p share shown 640 wide",
        SCREEN,
        (1920, 1080),
        (640, 360),
    ),
    ("480x480 camera, full size", CAMERA, (480, 480), (0, 0)),
    (
        "480x480 camera in a 240 tile",
        CAMERA,
        (480, 480),
        (240, 180),
    ),
];
const ROUNDS: usize = 10;

/// The stream cut into frames, each ending with its slice.
fn frames(stream: &[u8]) -> Vec<Vec<u8>> {
    nal::access_units(stream)
}

/// CPU time (user and system) process `pid` has used so far, in
/// seconds; from /proc, so Linux only (none elsewhere).
fn cpu(pid: u32) -> Option<f64> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    // After the command name in parentheses: utime and stime are the
    // 12th and 13th fields, in clock ticks (100 a second on Linux).
    let rest = &stat[stat.rfind(')')? + 2..];
    let fields: Vec<&str> = rest.split(' ').collect();
    let ticks = fields.get(11)?.parse::<f64>().ok()? + fields.get(12)?.parse::<f64>().ok()?;
    Some(ticks / 100.0)
}

/// The CPU both this process and `child` have used so far.
fn cpu_now(child: Option<u32>) -> f64 {
    cpu(std::process::id()).unwrap_or(0.0) + child.and_then(cpu).unwrap_or(0.0)
}

fn report(
    name: &str,
    started: Instant,
    cpu_before: f64,
    child: Option<u32>,
    count: usize,
    size: (u32, u32),
) {
    let per = started.elapsed().as_secs_f64() * 1000.0 / count as f64;
    let cpu = (cpu_now(child) - cpu_before) * 1000.0 / count as f64;
    println!(
        "  {name}: {per:.2} ms a frame, {cpu:.2} ms of CPU a frame ({}x{})",
        size.0, size.1
    );
}

/// Software in this process: rusty_h264, then the whole-step shrink.
fn software() {
    // Decode on one thread, as a decoder thread does.
    println!("software in this process (rusty_h264 and a whole-step shrink)");
    for (name, stream, _, fit) in CASES {
        let frames = frames(stream);
        let (started, before) = (Instant::now(), cpu_now(None));
        let mut size = (0, 0);
        for _ in 0..ROUNDS {
            let mut decoder = Software::new();
            decoder.set_output_size(fit.0, fit.1);
            for frame in &frames {
                let decoded = decoder
                    .decode(frame, nal::is_keyframe(frame))
                    .ok()
                    .flatten()
                    .expect("decodes");
                size = (decoded.planes.width, decoded.planes.height);
            }
        }
        report(name, started, before, None, ROUNDS * frames.len(), size);
    }
}

fn in_process() {
    let mut backend = noslacking_video::choose_backend();
    println!("on the GPU in this process, {}", backend.name());
    for (name, stream, source, fit) in CASES {
        let frames = frames(stream);
        let Ok(mut decoder) = backend.open_decoder(Codec::H264, source.0, source.1) else {
            println!("  {name}: no decoder");
            continue;
        };
        decoder.set_output_size(fit.0, fit.1);
        let (started, before) = (Instant::now(), cpu_now(None));
        let mut size = (0, 0);
        for _ in 0..ROUNDS {
            for frame in &frames {
                let decoded = decoder
                    .decode(frame, false)
                    .ok()
                    .flatten()
                    .expect("decodes");
                size = (decoded.planes.width, decoded.planes.height);
            }
        }
        report(name, started, before, None, ROUNDS * frames.len(), size);
    }
}

fn through_the_helper(helper: &str, hardware: bool, gpu_scale: bool) {
    let mut child = Command::new(helper)
        .env(
            "NOSLACKING_VIDEO_GPU_SCALE",
            if gpu_scale { "1" } else { "0" },
        )
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
        ipc::write_frame(&mut input, seq, &request.encode()).expect("sent");
        let (_, reply) = ipc::read_reply(&mut output)
            .expect("read")
            .expect("a reply");
        reply
    };
    call(Request::Hello {
        version: ipc::VERSION,
    });
    println!(
        "through the helper, {}",
        match (hardware, gpu_scale) {
            (false, _) => "in software",
            (true, true) => "on the GPU, shrunk there",
            (true, false) => "on the GPU, shrunk on its CPU",
        }
    );
    for (name, stream, source, fit) in CASES {
        let frames = frames(stream);
        let Reply::Opened { id } = call(Request::OpenDecoder {
            codec: Codec::H264,
            width: source.0,
            height: source.1,
            hardware,
        }) else {
            println!("  {name}: no decoder");
            continue;
        };
        call(Request::SetOutputSize {
            id,
            width: fit.0,
            height: fit.1,
        });
        let (started, before) = (Instant::now(), cpu_now(pid));
        let mut size = (0, 0);
        for _ in 0..ROUNDS {
            for frame in &frames {
                let reply = call(Request::Decode {
                    id,
                    keyframe: nal::is_keyframe(frame),
                    data: frame.clone(),
                });
                let Reply::Picture(decoded) = reply else {
                    panic!("{reply:?}");
                };
                assert_eq!(decoded.hardware, hardware, "decoded where asked");
                size = (decoded.planes.width, decoded.planes.height);
            }
        }
        report(name, started, before, pid, ROUNDS * frames.len(), size);
        call(Request::Close { id });
    }
    drop(input);
    let _ = child.wait();
}

/// The pipe alone: 3 MB pictures (1080p I420) through a pipe between two
/// processes and back as framed messages, without a decoder.
fn pipe_alone() {
    let picture = vec![7u8; 1920 * 1080 * 3 / 2];
    let mut child = Command::new("cat")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("cat");
    let mut input = child.stdin.take().expect("stdin");
    let mut output = BufReader::new(child.stdout.take().expect("stdout"));
    let rounds = 200;
    let writer = std::thread::spawn(move || {
        for seq in 0..rounds {
            ipc::write_frame(&mut input, seq, &picture).expect("written");
        }
    });
    let started = Instant::now();
    for _ in 0..rounds {
        ipc::read_frame(&mut output)
            .expect("read")
            .expect("a frame");
    }
    let per = started.elapsed().as_secs_f64() * 1000.0 / f64::from(rounds);
    let _ = writer.join();
    let _ = child.wait();
    println!(
        "pipe alone: a 1080p I420 picture ({} bytes) in {per:.2} ms",
        1920 * 1080 * 3 / 2
    );
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
    if let Some(helper) = &helper {
        if matches!(what.as_str(), "all" | "helper") {
            through_the_helper(helper, true, true);
        }
        if matches!(what.as_str(), "all" | "helper-cpu") {
            through_the_helper(helper, true, false);
        }
        if matches!(what.as_str(), "all" | "helper-software") {
            through_the_helper(helper, false, true);
        }
    }
    if matches!(what.as_str(), "all" | "pipe") {
        pipe_alone();
    }
}
