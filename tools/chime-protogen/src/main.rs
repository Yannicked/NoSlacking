//! Generates the Rust for Chime's SignalingProtocol.proto without protoc:
//! `chime-protogen <the .proto> <out dir>` writes `<out dir>/_.rs`.

fn main() {
    let mut args = std::env::args().skip(1);
    let (Some(proto), Some(out)) = (args.next(), args.next()) else {
        eprintln!("usage: chime-protogen <SignalingProtocol.proto> <out dir>");
        std::process::exit(2);
    };
    let folder = std::path::Path::new(&proto)
        .parent()
        .map_or_else(|| ".".into(), std::path::Path::to_path_buf);
    let files = match protox::compile([&proto], [folder]) {
        Ok(files) => files,
        Err(error) => {
            eprintln!("{proto}: {error}");
            std::process::exit(1);
        }
    };
    let generated = prost_build::Config::new()
        .out_dir(&out)
        // These hold secrets (a TURN password, a join token); the app
        // writes their Debug by hand, redacted.
        .skip_debug([".SdkTurnCredentials", ".SdkMeetingSessionCredentials"])
        .compile_fds(files);
    if let Err(error) = generated {
        eprintln!("{error}");
        std::process::exit(1);
    }
}
