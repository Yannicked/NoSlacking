//! Measures sending a camera, 640×480 at 30 pictures a second, from the
//! frame a webcam hands over (YUYV, made once from the test picture) to
//! the H.264 that goes out and the self-view the call bar shows, without
//! opening anyone's camera.
//!
//! - `pipeline`: the camera's pipeline in this process, one frame every
//!   1/30 s: the time from frame in (converted from YUYV as a V4L2
//!   camera's is) to access unit out, and this process's CPU a picture,
//!   on the GPU and in software, each with its self-view made. Also what
//!   the app itself did per picture before the camera moved into the
//!   helper (convert the frame to I420, and shrink, convert and mirror
//!   every picture for the self-view), for comparison; the encoding it
//!   then did, in software or through the helper, is in the research
//!   note (§6.8).
//! - `helper PATH`: the helper program sending its test camera (as YUYV
//!   frames) as the app drives it, for ten seconds each way: the app's
//!   side (this process: asking, reading the access units, turning each
//!   self-view into RGBA as the call bar does) and the helper's CPU, and
//!   how long from capture to access unit.
//!
//! `cargo build --release -p noslacking-video --examples --bins`, then
//! `target/release/examples/camera all target/release/noslacking-video`.
//! (`examples/encode.rs` measures the encoders alone, back to back.)

#![allow(clippy::print_stdout, reason = "the measurements are for the reader")]

use std::io::{BufReader, BufWriter};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use noslacking_video::capture::camera::convert;
use noslacking_video::pipeline::{Ask, Pipeline, Settings};
use noslacking_video::shrink;
use noslacking_video_ipc::{self as ipc, CameraChoice, Planes, Reply, Request};

/// Pictures each way in this process: ten seconds at 30 a second.
const PICTURES: usize = 300;
const FRAME: Duration = Duration::from_nanos(1_000_000_000 / 30);
/// The self-view's widest, as the app asks.
const PREVIEW: u32 = 320;
/// The rate, as §6.8 measured the encoders at.
const BITRATE: u32 = 900_000;

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

const CAMERA: &[u8] = include_bytes!("../../../src/huddle_audio/fixtures/camera-480x480.h264");

/// A real camera's pictures, as §6.8 measured the encoders with: the
/// 480×480 camera fixture decoded and stretched to 640×480 (nearest
/// pixel).
fn pictures() -> Vec<Planes> {
    let mut decoder = rusty_h264_decoder::Decoder::new();
    let stretch = |plane: &[u8], from: usize, to: usize, rows: usize| {
        let mut out = Vec::with_capacity(to * rows);
        for row in plane.chunks_exact(from).take(rows) {
            out.extend((0..to).map(|x| row[x * from / to]));
        }
        out
    };
    noslacking_video_ipc::h264::access_units(CAMERA)
        .iter()
        .filter_map(|frame| decoder.decode(frame).ok().flatten())
        .map(|p| Planes {
            width: 640,
            height: 480,
            y: stretch(&p.y, p.width, 640, p.height),
            u: stretch(&p.u, p.width / 2, 320, p.height / 2),
            v: stretch(&p.v, p.width / 2, 320, p.height / 2),
        })
        .collect()
}

/// The camera's frames: those pictures as YUYV, as a webcam hands them
/// over.
fn frames(pictures: &[Planes]) -> Vec<Vec<u8>> {
    pictures.iter().map(convert::to_yuyv).collect()
}

/// Pictures handed over before measuring: the encoder opens on the
/// first, which is a cost once, not a picture's.
const WARM_UP: usize = 15;

/// A self-view as the call bar shows it: RGBA, mirrored.
fn rgba(picture: &Planes) -> Vec<u8> {
    let image = yuv::YuvPlanarImage {
        y_plane: &picture.y,
        y_stride: picture.width,
        u_plane: &picture.u,
        u_stride: picture.width / 2,
        v_plane: &picture.v,
        v_stride: picture.width / 2,
        width: picture.width,
        height: picture.height,
    };
    let mut out = vec![0u8; picture.width as usize * picture.height as usize * 4];
    let _ = yuv::yuv420_to_rgba(
        &image,
        &mut out,
        picture.width * 4,
        yuv::YuvRange::Limited,
        yuv::YuvStandardMatrix::Bt601,
    );
    for row in out.chunks_exact_mut(picture.width as usize * 4) {
        row.as_chunks_mut::<4>().0.reverse();
    }
    out
}

/// Feeds `pipeline` a picture every 1/30 s (`picture(n)` makes it: from
/// YUYV as a V4L2 camera's is, or handed over as it is) and asks for its
/// access unit, as the app does; prints the cost, the first
/// [`WARM_UP`] pictures left out.
fn run(name: &str, mut pipeline: Pipeline, picture: impl Fn(usize) -> Planes) {
    let mut times = Vec::new();
    let (mut bytes, mut hardware, mut previews) = (0usize, 0, 0);
    let mut before = 0.0;
    let mut due = Instant::now();
    for n in 0..WARM_UP + PICTURES {
        std::thread::sleep(due.saturating_duration_since(Instant::now()));
        due += FRAME;
        if n == WARM_UP {
            before = cpu(std::process::id()).unwrap_or(0.0);
        }
        let started = Instant::now();
        pipeline.put_picture(picture(n), started);
        let (reply, answer) = std::sync::mpsc::channel();
        pipeline.ask(Ask::Next {
            force_keyframe: n % 120 == 0,
            repeat: false,
            wait: Duration::ZERO,
            reply,
        });
        let Ok(Ok(Some(encoded))) = answer.recv() else {
            println!("  {name}: picture {n} did not come");
            continue;
        };
        if n < WARM_UP {
            continue;
        }
        times.push(started.elapsed());
        bytes += encoded.data.len();
        hardware += usize::from(encoded.hardware);
        previews += usize::from(encoded.preview.is_some());
    }
    let cpu_ms = (cpu(std::process::id()).unwrap_or(0.0) - before) * 1000.0 / PICTURES as f64;
    let (mean, p95) = spread(&mut times);
    println!(
        "  {name}: {mean:.2} ms in to access unit out (p95 {p95:.2}), {cpu_ms:.2} ms of CPU a \
         picture ({:.1} % of a core at 30 a second), {:.0} kbit/s, {hardware} from the GPU, \
         {previews} self-views",
        cpu_ms * 30.0 / 10.0,
        bytes as f64 * 8.0 * 30.0 / PICTURES as f64 / 1000.0
    );
}

fn in_process() {
    let pictures = pictures();
    let frames = frames(&pictures);
    let from_yuyv =
        |n: usize| convert::from_yuyv(&frames[n % frames.len()], 640, 480, 1280).expect("whole");
    let as_is = |n: usize| pictures[n % pictures.len()].clone();
    println!(
        "the camera's pipeline in this process, 640x480 at 30 a second, {BITRATE} bit/s, the \
         camera fixture"
    );
    // What the app did for each picture before (on main): convert the
    // camera's frame to I420, and shrink, convert and mirror it for the
    // self-view; then encode it (in software, or as I420 through the pipe
    // to the helper's GPU). Its parts, each alone: the camera's frame to
    // I420, the self-view shrunk (the helper's now), made RGBA and
    // mirrored (still the app's).
    let mut parts = [Vec::new(), Vec::new(), Vec::new(), Vec::new()];
    let before = cpu(std::process::id()).unwrap_or(0.0);
    for n in 0..PICTURES {
        let started = Instant::now();
        let picture = from_yuyv(n);
        let converted = Instant::now();
        let small = shrink::preview(&picture, PREVIEW);
        let shrunk = Instant::now();
        std::hint::black_box(rgba(&small));
        parts[0].push(started.elapsed());
        parts[1].push(converted - started);
        parts[2].push(shrunk - converted);
        parts[3].push(shrunk.elapsed());
    }
    let cpu_ms = (cpu(std::process::id()).unwrap_or(0.0) - before) * 1000.0 / PICTURES as f64;
    let [all, yuyv, shrunk, shown] = parts.map(|mut times| spread(&mut times).0);
    println!(
        "  before, in the app and not encoding: {all:.2} ms, {cpu_ms:.2} ms of CPU a picture \
         (YUYV to I420 {yuyv:.2} ms, the self-view shrunk {shrunk:.2} ms, made RGBA and mirrored \
         {shown:.2} ms)"
    );
    let gpu = noslacking_video::choose_backend().capture_gpu();
    if gpu.is_some() {
        run(
            "on the GPU, from YUYV, with its self-view",
            Pipeline::new(Settings::camera(true, BITRATE, gpu.clone(), PREVIEW)),
            from_yuyv,
        );
        run(
            "on the GPU, I420 in, no self-view",
            Pipeline::new(Settings::camera(true, BITRATE, gpu, 0)),
            as_is,
        );
    } else {
        println!("  on the GPU: no GPU encoder here");
    }
    run(
        "in software, from YUYV, with its self-view",
        Pipeline::new(Settings::camera(false, BITRATE, None, PREVIEW)),
        from_yuyv,
    );
    run(
        "in software, I420 in, no self-view",
        Pipeline::new(Settings::camera(false, BITRATE, None, 0)),
        as_is,
    );
}

/// The helper program sending its test camera, driven as the app drives
/// it for ten seconds.
fn through_the_helper(helper: &str, hardware: bool) {
    let mut child = Command::new(helper)
        .env("NOSLACKING_VIDEO_TEST_CAMERA", "yuyv")
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
    let Reply::Started { id, .. } = call(Request::StartCamera {
        choice: CameraChoice::Test,
        hardware,
        bitrate: BITRATE,
        preview: PREVIEW,
    }) else {
        println!("  did not start");
        let _ = child.kill();
        let _ = child.wait();
        return;
    };
    // The first picture: the camera is made and the encoder opened.
    call(Request::NextFrame {
        id,
        force_keyframe: true,
        repeat: false,
        wait_ms: ipc::MAX_WAIT_MS,
    });
    let (mut times, mut got, mut on_gpu, mut previews) = (Vec::new(), 0usize, 0usize, 0usize);
    let (app, helper_cpu) = (
        cpu(std::process::id()).unwrap_or(0.0),
        cpu(pid).unwrap_or(0.0),
    );
    let started = Instant::now();
    // As the app's gate: a little under a frame's time after the last
    // picture's capture, ask for the next, which the helper waits for.
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
            let captured = Instant::now() - Duration::from_micros(u64::from(frame.age_us));
            due = captured + FRAME - Duration::from_millis(5);
            times.push(Duration::from_micros(u64::from(frame.age_us)));
            got += 1;
            on_gpu += usize::from(frame.hardware);
            if let Some(preview) = &frame.preview {
                // What the call bar does with it.
                std::hint::black_box(rgba(preview));
                previews += 1;
            }
        }
    }
    let seconds = started.elapsed().as_secs_f64();
    let app = (cpu(std::process::id()).unwrap_or(0.0) - app) / seconds * 100.0;
    let helper_cpu = (cpu(pid).unwrap_or(0.0) - helper_cpu) / seconds * 100.0;
    let (mean, p95) = spread(&mut times);
    println!(
        "  {}: {:.1} pictures a second ({on_gpu} of {got} from the GPU, {previews} self-views); \
         CPU: app {app:.1} %, helper {helper_cpu:.1} % of a core; capture to access unit \
         {mean:.2} ms (p95 {p95:.2})",
        if hardware { "GPU" } else { "software" },
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
        println!("through the helper, as the app drives it, 640x480 at 30 a second");
        through_the_helper(helper, true);
        through_the_helper(helper, false);
    }
}
