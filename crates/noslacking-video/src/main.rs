//! `noslacking-video`: NoSlacking's video helper, which decodes every
//! huddle video stream the app shows (on the GPU when it can). The app
//! starts it and talks to it over standard input and output; see the
//! library's documentation. `noslacking-video --probe` prints what this
//! system's hardware can do.

use std::io::{BufReader, BufWriter};
use std::process::ExitCode;

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
    eprintln!(
        "noslacking-video: screens are captured through {}",
        screens.name()
    );
    let mut input = BufReader::new(std::io::stdin().lock());
    let mut output = BufWriter::with_capacity(1 << 16, std::io::stdout().lock());
    match noslacking_video::server::serve(&mut input, &mut output, backend.as_mut(), &mut screens) {
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
        println!("no hardware video: this helper decodes in software");
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
        if backend.share_gpu().is_some() {
            "where it imports them"
        } else {
            "no (no GPU encoder)"
        }
    );
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
