//! Measures the app's side of a decoded picture's way from the video
//! helper to egui (docs/research/huddle-video.md §6.12): the round trip
//! through the helper as the app makes it, the pipe alone at the
//! picture's size, turning I420 into RGBA, and handing the result to
//! egui as a texture (its bookkeeping; the upload itself happens in the
//! painter, measured in the demo's call window).
//!
//! `cargo build --release --features huddle-video --example picture_path
//! -p noslacking -p noslacking-video --bins`, then
//! `target/release/examples/picture_path target/release/noslacking-video`
//! (add `gpu` after the helper's path to decode on the GPU).

#![allow(clippy::print_stdout, reason = "the measurements are for the reader")]

use std::io::{BufReader, BufWriter};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use noslacking::huddle_audio::decode::{H264, Outcome};
use noslacking::huddle_audio::helper::{self, Helper, ProcessLauncher};
use noslacking_video_ipc::{self as ipc, Planes, Reply};

const SCREEN: &[u8] = include_bytes!("../src/huddle_audio/fixtures/screen-1920x1080.h264");
const CAMERA: &[u8] = include_bytes!("../src/huddle_audio/fixtures/camera-480x480.h264");

/// A stream and the box it is shown in (0×0: at its own size).
type Case = (&'static str, &'static [u8], (usize, usize));

const CASES: [Case; 4] = [
    ("1080p share, full size", SCREEN, (0, 0)),
    ("1080p share shown 960 wide", SCREEN, (960, 540)),
    ("480x480 camera, full size", CAMERA, (0, 0)),
    ("480x480 camera in a 240 tile", CAMERA, (240, 180)),
];

/// CPU time (user and system) of `what` (`self` or `thread-self`) so
/// far, in seconds, from /proc (Linux only; 0 elsewhere).
fn cpu(what: &str) -> f64 {
    let Ok(stat) = std::fs::read_to_string(format!("/proc/{what}/stat")) else {
        return 0.0;
    };
    let Some(close) = stat.rfind(')') else {
        return 0.0;
    };
    let fields: Vec<&str> = stat[close + 2..].split(' ').collect();
    let tick = |n: usize| fields.get(n).and_then(|f| f.parse::<f64>().ok());
    (tick(11).unwrap_or(0.0) + tick(12).unwrap_or(0.0)) / 100.0
}

/// CPU time of this process's children (the helper) so far, in seconds.
fn children_cpu() -> f64 {
    let Ok(tasks) = std::fs::read_dir("/proc/self/task") else {
        return 0.0;
    };
    tasks
        .flatten()
        .filter_map(|task| std::fs::read_to_string(task.path().join("children")).ok())
        .flat_map(|children| children.split_whitespace().map(cpu).collect::<Vec<_>>())
        .sum()
}

/// Milliseconds a picture.
fn per(total: Duration, count: usize) -> f64 {
    total.as_secs_f64() * 1000.0 / count.max(1) as f64
}

/// Every picture of `stream` decoded in the helper at the size `fit`
/// asks for, and the round trip timed: wall time a picture, and the
/// app's CPU a picture (reading the pipe, checking, the threads).
fn round_trip(
    helper: &Helper,
    gpu: bool,
    name: &str,
    stream: &[u8],
    fit: (usize, usize),
) -> Vec<Planes> {
    let frames = noslacking_video_ipc::h264::access_units(stream);
    let mut decoder = H264::with_helper(Some(helper.clone()), Some(gpu));
    decoder.set_fit(fit.0, fit.1);
    let mut pictures = Vec::new();
    let rounds = 10;
    // The decoder opens (the GPU's context made) before the clock starts;
    // each round starts again at the fixture's first frame, a keyframe.
    let _ = decoder.decode(&frames[0], true);
    for show in [true, false] {
        let (started, before, helper_before) = (Instant::now(), cpu("self"), children_cpu());
        for round in 0..rounds {
            for frame in &frames {
                if let Ok(Outcome::Picture(picture)) = decoder.decode(frame, show)
                    && round == 0
                {
                    pictures.push(picture.yuv);
                }
            }
        }
        let count = rounds * frames.len();
        let app_cpu = (cpu("self") - before) * 1000.0 / count as f64;
        let helper_cpu = (children_cpu() - helper_before) * 1000.0 / count as f64;
        let size = pictures.first().map_or((0, 0), |p| (p.width, p.height));
        println!(
            "  {name} ({}x{}){}: round trip {:.2} ms, app CPU {app_cpu:.2} ms, helper CPU \
             {helper_cpu:.2} ms a frame",
            size.0,
            size.1,
            if show { "" } else { ", decoded unseen" },
            per(started.elapsed(), count),
        );
    }
    pictures
}

/// `picture` framed as the helper's reply, through `cat` and read back as
/// the app reads replies: wall time a picture and the reading thread's
/// CPU a picture.
fn pipe_alone(picture: &Planes) {
    let reply = Reply::Picture(ipc::Decoded {
        planes: picture.clone(),
        source: (picture.width, picture.height),
        hardware: false,
    });
    let mut child = Command::new("cat")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("cat");
    let mut input = BufWriter::new(child.stdin.take().expect("stdin"));
    let output = child.stdout.take().expect("stdout");
    let bytes = picture.y.len() * 3 / 2;
    let rounds = (600_000_000 / bytes.max(1)).clamp(100, 4000);
    let writer = std::thread::spawn(move || {
        for seq in 0..rounds {
            ipc::write_reply(&mut input, u32::try_from(seq).unwrap_or(0), &reply).expect("written");
        }
    });
    let reader = std::thread::spawn(move || {
        // As the app's `video-helper-out` thread reads.
        let mut output = BufReader::with_capacity(1 << 16, output);
        let (started, before) = (Instant::now(), cpu("thread-self"));
        for _ in 0..rounds {
            ipc::read_reply(&mut output)
                .expect("read")
                .expect("a reply");
        }
        (started.elapsed(), cpu("thread-self") - before)
    });
    let (took, reader_cpu) = reader.join().expect("the reader");
    let _ = writer.join();
    let _ = child.wait();
    println!(
        "    pipe alone: {:.2} ms a picture, reading {:.2} ms of CPU ({bytes} bytes)",
        per(took, rounds),
        reader_cpu * 1000.0 / rounds as f64
    );
}

/// Turning `pictures` into RGBA as the app does, and the parts of it.
fn convert(pictures: &[Planes]) {
    let rounds = (200_000_000 / pictures[0].y.len().max(1)).clamp(3, 400);
    let count = rounds * pictures.len();
    let started = Instant::now();
    for _ in 0..rounds {
        for picture in pictures {
            std::hint::black_box(helper::to_image(picture, false).expect("converts"));
        }
    }
    let whole = per(started.elapsed(), count);
    // The fresh buffer alone (allocated and filled, page faults and all).
    let started = Instant::now();
    for _ in 0..rounds {
        for picture in pictures {
            let pixels = vec![egui::Color32::BLACK; (picture.width * picture.height) as usize];
            std::hint::black_box(pixels);
        }
    }
    let fresh = per(started.elapsed(), count);
    println!(
        "    to RGBA: {whole:.2} ms a picture as the app does it ({fresh:.2} ms of it a fresh \
         buffer)"
    );
}

/// Handing a picture to egui as the call window does: a texture set to
/// each new image, and the frame's texture changes taken as a painter
/// takes them (the upload itself needs a GL context).
fn hand_to_egui(pictures: &[Planes]) {
    let ctx = egui::Context::default();
    let images: Vec<Arc<egui::ColorImage>> = pictures
        .iter()
        .take(8)
        .map(|p| Arc::new(helper::to_image(p, false).expect("converts")))
        .collect();
    let mut texture = ctx.load_texture(
        "bench",
        Arc::clone(&images[0]),
        egui::TextureOptions::LINEAR,
    );
    let rounds = 2000;
    let started = Instant::now();
    for n in 0..rounds {
        texture.set(
            Arc::clone(&images[n % images.len()]),
            egui::TextureOptions::LINEAR,
        );
        drop(std::hint::black_box(ctx.tex_manager().write().take_delta()));
    }
    println!(
        "    egui's bookkeeping: {:.4} ms a picture",
        per(started.elapsed(), rounds)
    );
}

fn main() {
    let mut args = std::env::args().skip(1);
    let Some(program) = args.next() else {
        println!("usage: picture_path <noslacking-video> [gpu]");
        return;
    };
    let gpu = args.next().as_deref() == Some("gpu");
    let helper = Helper::new(Arc::new(ProcessLauncher::new(program.into())));
    println!(
        "through the helper, {}",
        if gpu { "on the GPU" } else { "in software" }
    );
    for (name, stream, fit) in CASES {
        let pictures = round_trip(&helper, gpu, name, stream, fit);
        if pictures.is_empty() {
            continue;
        }
        pipe_alone(&pictures[0]);
        convert(&pictures);
        hand_to_egui(&pictures);
    }
}
