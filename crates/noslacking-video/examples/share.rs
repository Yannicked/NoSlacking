//! Measures sharing a 1920×1080 screen at 15 pictures a second, from the
//! frame a capture hands over to the H.264 that goes out, without anyone
//! answering the portal's dialog: the screen is the test screen's moving
//! pictures, made once, handed over as PipeWire hands them (packed BGRx
//! in memory, or a dma-buf exported from a surface of another display).
//!
//! - `pipeline`: the share's pipeline in this process, one picture every
//!   1/15 s: the time from frame in to access unit out, and this
//!   process's CPU a picture, for a dma-buf to the GPU, packed RGB to the
//!   GPU, and packed RGB in software (shrunk to 720p). Also what the app
//!   itself did per picture before the helper captured (copy the frame,
//!   convert it to I420), for comparison.
//! - `helper PATH`: the helper program as the app drives it, for ten
//!   seconds each way: the app's side (this process: asking and reading
//!   the access units) and the helper's CPU, and how long each answer
//!   took.
//!
//! `cargo build --release -p noslacking-video --examples --bins`, then
//! `target/release/examples/share all target/release/noslacking-video`.

#![allow(clippy::print_stdout, reason = "the measurements are for the reader")]

use std::io::{BufReader, BufWriter};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use noslacking_video::capture::{Frame, Order, Packed, convert, pattern};
use noslacking_video::pipeline::{Ask, Pipeline, Settings};
use noslacking_video_ipc::{self as ipc, Reply, Request, ShareChoice};

/// Pictures each way in this process: ten seconds at 15 a second.
const PICTURES: usize = 150;
const FRAME: Duration = Duration::from_nanos(1_000_000_000 / 15);

/// CPU time (user and system) process `pid` has used so far, in seconds;
/// from /proc, so Linux only.
fn cpu(pid: u32) -> Option<f64> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let rest = &stat[stat.rfind(')')? + 2..];
    let fields: Vec<&str> = rest.split(' ').collect();
    let ticks = fields.get(11)?.parse::<f64>().ok()? + fields.get(12)?.parse::<f64>().ok()?;
    Some(ticks / 100.0)
}

/// Mean and 95th percentile of `times`, in milliseconds.
fn spread(times: &mut [Duration]) -> (f64, f64) {
    times.sort();
    let mean = times.iter().sum::<Duration>().as_secs_f64() * 1000.0 / times.len().max(1) as f64;
    let p95 = times
        .get(times.len() * 95 / 100)
        .map_or(0.0, |d| d.as_secs_f64() * 1000.0);
    (mean, p95)
}

/// Feeds `pipeline` a frame from `frame(n)` every 1/15 s and asks for the
/// picture, as the app does; prints the cost.
fn run(name: &str, mut pipeline: Pipeline, mut frame: impl FnMut(usize, &mut Pipeline)) {
    let mut times = Vec::new();
    let mut bytes = 0usize;
    let mut sizes = std::collections::BTreeSet::new();
    let mut hardware = 0;
    let before = cpu(std::process::id()).unwrap_or(0.0);
    let mut due = Instant::now();
    for n in 0..PICTURES {
        std::thread::sleep(due.saturating_duration_since(Instant::now()));
        due += FRAME;
        let started = Instant::now();
        frame(n, &mut pipeline);
        let (reply, answer) = std::sync::mpsc::channel();
        pipeline.ask(Ask::Next {
            force_keyframe: n % 60 == 0,
            repeat: false,
            wait: Duration::ZERO,
            reply,
        });
        let Ok(Ok(Some(encoded))) = answer.recv() else {
            println!("  {name}: picture {n} did not come");
            continue;
        };
        times.push(started.elapsed());
        bytes += encoded.data.len();
        sizes.insert((encoded.width, encoded.height));
        hardware += usize::from(encoded.hardware);
    }
    let cpu_ms = (cpu(std::process::id()).unwrap_or(0.0) - before) * 1000.0 / PICTURES as f64;
    let (mean, p95) = spread(&mut times);
    println!(
        "  {name}: {mean:.2} ms frame in to access unit out (p95 {p95:.2}), {cpu_ms:.2} ms of \
         CPU a picture ({:.1} % of a core at 15 a second), {:.0} kbit/s, {sizes:?}, {hardware} \
         from the GPU",
        cpu_ms * 15.0 / 10.0,
        bytes as f64 * 8.0 * 15.0 / PICTURES as f64 / 1000.0
    );
}

fn settings(hardware: bool) -> Settings {
    Settings::share(
        hardware,
        2_500_000,
        hardware
            .then(|| noslacking_video::choose_backend().capture_gpu())
            .flatten(),
    )
}

fn in_process() {
    let pictures = pattern::packed_loop();
    let packed = |n: usize| {
        let (width, height, data) = &pictures[n % pictures.len()];
        Packed {
            width: *width,
            height: *height,
            stride: *width as usize * 4,
            order: Order::Bgra,
            data,
        }
    };
    println!("the share's pipeline in this process, 1920x1080 at 15 a second");
    // What the app did for each picture before: copy PipeWire's buffer,
    // convert it to I420 (then 3 MB through the pipe to the helper).
    let mut times = Vec::new();
    let before = cpu(std::process::id()).unwrap_or(0.0);
    for n in 0..PICTURES {
        let started = Instant::now();
        let picture = packed(n);
        let copy = picture.data.to_vec();
        let converted = convert::to_i420(&Packed {
            data: &copy,
            ..picture
        });
        std::hint::black_box(converted);
        times.push(started.elapsed());
    }
    let cpu_ms = (cpu(std::process::id()).unwrap_or(0.0) - before) * 1000.0 / PICTURES as f64;
    let (mean, p95) = spread(&mut times);
    println!(
        "  before (in the app): copy and convert to I420, {mean:.2} ms (p95 {p95:.2}), \
         {cpu_ms:.2} ms of CPU a picture"
    );
    #[cfg(target_os = "linux")]
    dmabuf_in_process(&pictures);
    run(
        "packed BGRx to the GPU",
        Pipeline::new(settings(true)),
        |n, pipeline| {
            pipeline.put(&Frame::Packed(packed(n)), Instant::now());
        },
    );
    #[cfg(target_os = "linux")]
    {
        let on_cpu: noslacking_video::pipeline::GpuOpener = std::sync::Arc::new(|| {
            let mut gpu = noslacking_video::vaapi::capture::GpuCapture::open().ok()?;
            gpu.convert_on_gpu(false);
            Some(Box::new(gpu) as Box<dyn noslacking_video::pipeline::Gpu>)
        });
        let settings = Settings {
            gpu: Some(on_cpu),
            ..settings(true)
        };
        run(
            "packed BGRx converted on the CPU, to the GPU",
            Pipeline::new(settings),
            |n, pipeline| {
                pipeline.put(&Frame::Packed(packed(n)), Instant::now());
            },
        );
    }
    run(
        "packed BGRx in software",
        Pipeline::new(settings(false)),
        |n, pipeline| {
            pipeline.put(&Frame::Packed(packed(n)), Instant::now());
        },
    );
}

/// The pictures as dma-bufs of another display, as a compositor's.
#[cfg(target_os = "linux")]
fn dmabuf_in_process(pictures: &[(u32, u32, Vec<u8>)]) {
    use noslacking_video::capture::DmaBuf;
    use noslacking_video::vaapi::va::Display;
    use noslacking_video::vaapi::va::prime::Rgb;
    use std::os::fd::AsFd;
    let Ok(display) = Display::open() else {
        println!("  dma-buf: no VA-API here");
        return;
    };
    let mut frames = Vec::new();
    for (width, height, data) in pictures {
        let surfaces = display
            .rgb_surface(*width, *height, Rgb::BGRX, true)
            .expect("an RGB surface");
        display
            .write_packed(
                surfaces.ids[0],
                (*width, *height),
                Rgb::BGRX,
                data,
                *width as usize * 4,
            )
            .expect("written");
        let exported = display.export(surfaces.ids[0]).expect("exported");
        frames.push((surfaces, exported));
    }
    let modifier = frames.first().map_or(0, |(_, e)| e.modifier);
    run(
        &format!("dma-buf to the GPU (modifier {modifier:#x})"),
        Pipeline::new(settings(true)),
        |n, pipeline| {
            let (_, exported) = &frames[n % frames.len()];
            pipeline.put(
                &Frame::DmaBuf(DmaBuf {
                    fd: exported.fd.as_fd(),
                    width: exported.width,
                    height: exported.height,
                    offset: exported.offset,
                    stride: exported.pitch,
                    order: Order::Bgra,
                    alpha: false,
                    modifier: exported.modifier,
                }),
                Instant::now(),
            );
        },
    );
}

/// The helper program sharing its test screen as `frames` ("dmabuf" or
/// "packed"), driven as the app drives it for ten seconds.
fn through_the_helper(helper: &str, frames: &str, hardware: bool, rgb_on_gpu: bool) {
    let mut child = Command::new(helper)
        .env("NOSLACKING_VIDEO_TEST_FRAMES", frames)
        .env(
            "NOSLACKING_VIDEO_RGB_UPLOAD",
            if rgb_on_gpu { "1" } else { "0" },
        )
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("the helper starts");
    let pid = child.id();
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
    let Reply::Started { id, .. } = call(Request::StartShare {
        choice: ShareChoice::Test,
        hardware,
        bitrate: 2_500_000,
        restore: String::new(),
    }) else {
        println!("  {frames}: did not start");
        let _ = child.kill();
        let _ = child.wait();
        return;
    };
    // The first picture: the pictures are made and the encoder opened.
    call(Request::NextFrame {
        id,
        force_keyframe: true,
        repeat: true,
        wait_ms: ipc::MAX_WAIT_MS,
    });
    let (mut times, mut got, mut on_gpu) = (Vec::new(), 0usize, 0usize);
    let (app, helper_cpu) = (
        cpu(std::process::id()).unwrap_or(0.0),
        cpu(pid).unwrap_or(0.0),
    );
    let started = Instant::now();
    // As the app's gate: a little under a frame's time after the last
    // picture, ask for the next, which the helper waits for.
    let mut due = Instant::now();
    while started.elapsed() < Duration::from_secs(10) {
        std::thread::sleep(due.saturating_duration_since(Instant::now()));
        let reply = call(Request::NextFrame {
            id,
            force_keyframe: false,
            repeat: false,
            wait_ms: 50,
        });
        if let Reply::Frame(frame) = reply {
            // From when it was captured, as the app's gate.
            let captured = Instant::now() - Duration::from_micros(u64::from(frame.age_us));
            due = captured + FRAME - Duration::from_millis(5);
            // From capture to the access unit in the app's hands.
            times.push(Duration::from_micros(u64::from(frame.age_us)));
            got += 1;
            on_gpu += usize::from(frame.hardware);
        }
    }
    let seconds = started.elapsed().as_secs_f64();
    let app = (cpu(std::process::id()).unwrap_or(0.0) - app) / seconds * 100.0;
    let helper_cpu = (cpu(pid).unwrap_or(0.0) - helper_cpu) / seconds * 100.0;
    let (mean, p95) = spread(&mut times);
    println!(
        "  {frames}, {}: {:.1} pictures a second ({on_gpu} of {got} from the GPU); CPU: app \
         {app:.1} %, helper {helper_cpu:.1} % of a core; capture to access unit {mean:.2} ms \
         (p95 {p95:.2})",
        match (hardware, rgb_on_gpu) {
            (false, _) => "software",
            (true, true) => "GPU",
            (true, false) => "GPU, converted on the CPU",
        },
        got as f64 / seconds
    );
    call(Request::Close { id });
    drop(input);
    let _ = child.wait();
}

fn main() {
    let what = std::env::args().nth(1).unwrap_or_else(|| "all".into());
    let helper = std::env::args().nth(2);
    if matches!(what.as_str(), "all" | "pipeline") {
        in_process();
    }
    if let Some(helper) = &helper
        && matches!(what.as_str(), "all" | "helper")
    {
        println!("through the helper, as the app drives it, 1920x1080 at 15 a second");
        through_the_helper(helper, "dmabuf", true, true);
        through_the_helper(helper, "packed", true, true);
        through_the_helper(helper, "packed", true, false);
        through_the_helper(helper, "packed", false, false);
    }
}
