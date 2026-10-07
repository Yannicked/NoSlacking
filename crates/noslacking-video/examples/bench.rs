//! Measures hardware decoding of the 1080p and 480×480 fixtures: in
//! this process, and through the helper over its pipe as the app uses
//! it, so the difference is the pipe's cost.
//!
//! `cargo build --release -p noslacking-video --examples --bins`, then
//! `target/release/examples/bench all target/release/noslacking-video`
//! (or `software`, `hardware`, `helper` or `pipe` alone, to time one
//! under `time`).

#![allow(clippy::print_stdout, reason = "the measurements are for the reader")]

use std::io::{BufReader, BufWriter};
use std::process::{Command, Stdio};
use std::time::Instant;

use noslacking_video_ipc::{self as ipc, Codec, Reply, Request};

const STREAMS: [(&str, &[u8], (u32, u32)); 2] = [
    (
        "screen 1920x1080",
        include_bytes!("../../../src/huddle_audio/fixtures/screen-1920x1080.h264"),
        (1920, 1080),
    ),
    (
        "camera 480x480",
        include_bytes!("../../../src/huddle_audio/fixtures/camera-480x480.h264"),
        (480, 480),
    ),
];
const ROUNDS: usize = 10;

/// The stream cut into frames, each ending with its slice.
fn frames(stream: &[u8]) -> Vec<Vec<u8>> {
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
    frames
}

fn in_process() {
    let mut backend = noslacking_video::choose_backend();
    println!("in process, {}", backend.name());
    for (name, stream, (width, height)) in STREAMS {
        let frames = frames(stream);
        let Ok(mut decoder) = backend.open_decoder(Codec::H264, width, height) else {
            println!("  {name}: no decoder");
            continue;
        };
        let started = Instant::now();
        let mut slowest = 0f64;
        for _ in 0..ROUNDS {
            for frame in &frames {
                let one = Instant::now();
                let picture = decoder.decode(frame, false).ok().flatten();
                assert!(picture.is_some(), "decodes");
                slowest = slowest.max(one.elapsed().as_secs_f64() * 1000.0);
            }
        }
        let per = started.elapsed().as_secs_f64() * 1000.0 / (ROUNDS * frames.len()) as f64;
        println!("  {name}: {per:.2} ms a frame (slowest {slowest:.2} ms)");
    }
}

/// The app's own software decoder, for comparison.
fn software() {
    println!("software (rusty_h264), for comparison");
    for (name, stream, _) in STREAMS {
        let frames = frames(stream);
        let started = Instant::now();
        for _ in 0..ROUNDS {
            let mut decoder = rusty_h264_decoder::Decoder::new();
            for frame in &frames {
                let picture = decoder.decode(frame).ok().flatten();
                assert!(picture.is_some(), "decodes");
            }
        }
        let per = started.elapsed().as_secs_f64() * 1000.0 / (ROUNDS * frames.len()) as f64;
        println!("  {name}: {per:.2} ms a frame");
    }
}

fn through_the_helper(helper: &str) {
    let mut child = Command::new(helper)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("the helper starts");
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
    let welcome = call(Request::Hello {
        version: ipc::VERSION,
    });
    println!("through the helper, {welcome:?}");
    for (name, stream, (width, height)) in STREAMS {
        let frames = frames(stream);
        let Reply::Opened { id } = call(Request::OpenDecoder {
            codec: Codec::H264,
            width,
            height,
        }) else {
            println!("  {name}: no decoder");
            continue;
        };
        let started = Instant::now();
        for _ in 0..ROUNDS {
            for frame in &frames {
                let reply = call(Request::Decode {
                    id,
                    keyframe: false,
                    data: frame.clone(),
                });
                assert!(matches!(reply, Reply::Picture(_)), "{reply:?}");
            }
        }
        let per = started.elapsed().as_secs_f64() * 1000.0 / (ROUNDS * frames.len()) as f64;
        println!("  {name}: {per:.2} ms a frame, pipe included");
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
    if matches!(what.as_str(), "all" | "helper")
        && let Some(helper) = &helper
    {
        through_the_helper(helper);
    }
    if matches!(what.as_str(), "all" | "pipe") {
        pipe_alone();
    }
}
