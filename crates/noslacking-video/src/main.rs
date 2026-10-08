//! `noslacking-video`: NoSlacking's video helper, which does everything
//! in a huddle that touches pixels: it decodes every video stream the
//! app shows (on the GPU when it can), and captures and encodes the
//! screen the user shares and their camera. The app starts it and talks
//! to it over standard input and output; see the library's
//! documentation. `noslacking-video --probe` prints what this system's
//! hardware can do and the cameras there are (without opening any).

use std::io::{BufReader, BufWriter};
use std::process::ExitCode;

use noslacking_video::capture::camera::Cameras as _;

fn main() -> ExitCode {
    let argument = std::env::args().nth(1);
    match argument.as_deref() {
        Some("--probe") => probe(),
        Some("--version") => version(),
        Some(other) => {
            eprintln!("noslacking-video: unknown argument {other:?} (try --probe)");
            ExitCode::from(2)
        }
        None => serve(),
    }
}

fn serve() -> ExitCode {
    #[cfg(target_os = "linux")]
    noslacking_video::pipe::enlarge_stdout();
    let mut backend = noslacking_video::choose_backend();
    eprintln!("noslacking-video: back end {}", backend.name());
    let mut screens = noslacking_video::capture::System::new();
    let mut cameras = noslacking_video::capture::camera::System;
    eprintln!(
        "noslacking-video: screens are captured through {}, cameras through {}",
        screens.name(),
        cameras.name()
    );
    let mut input = BufReader::new(std::io::stdin().lock());
    let mut output = BufWriter::with_capacity(1 << 16, std::io::stdout().lock());
    let sources = noslacking_video::server::Sources {
        screens: &mut screens,
        cameras: &mut cameras,
    };
    match noslacking_video::server::serve(&mut input, &mut output, backend.as_mut(), sources) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("noslacking-video: {error}");
            ExitCode::FAILURE
        }
    }
}

#[allow(clippy::print_stdout, reason = "the probe's answer is for the reader")]
fn probe() -> ExitCode {
    let backend = noslacking_video::choose_backend();
    println!("back end: {}", backend.name());
    let capabilities = backend.capabilities();
    if capabilities.is_empty() {
        println!("no hardware video: this helper decodes and encodes in software");
    }
    for capability in capabilities {
        println!(
            "{:?} {:?} up to {}x{}",
            capability.codec, capability.direction, capability.max_width, capability.max_height
        );
    }
    println!(
        "screen sharing: through {}; dma-bufs to the GPU: {}",
        noslacking_video::capture::System::new().name(),
        if backend.capture_gpu().is_some() {
            "where it imports them"
        } else {
            "no (no GPU encoder)"
        }
    );
    let mut cameras = noslacking_video::capture::camera::System;
    match cameras.list() {
        Ok(list) if list.is_empty() => println!("cameras (through {}): none", cameras.name()),
        Ok(list) => {
            println!("cameras (through {}):", cameras.name());
            for camera in list {
                println!("  {} ({})", camera.name, camera.id);
            }
        }
        Err(trouble) => println!("cameras (through {}): {trouble}", cameras.name()),
    }
    ExitCode::SUCCESS
}

#[allow(clippy::print_stdout, reason = "the version is for the reader")]
fn version() -> ExitCode {
    println!(
        "noslacking-video {} (protocol {})",
        env!("CARGO_PKG_VERSION"),
        noslacking_video_ipc::VERSION
    );
    ExitCode::SUCCESS
}
