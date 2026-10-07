//! The built helper, as the app starts it: over its standard input and
//! output, with a back end named in its environment so no GPU is
//! touched.

use std::io::{BufReader, Write};
use std::process::{Command, Stdio};

use noslacking_video_ipc::{self as ipc, Codec, Reply, Request};

fn helper(backend: &str) -> std::process::Child {
    Command::new(env!("CARGO_BIN_EXE_noslacking-video"))
        .env("NOSLACKING_VIDEO_BACKEND", backend)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("the helper starts")
}

#[test]
fn the_helper_answers_over_its_pipes_and_ends_when_they_close() {
    let mut child = helper("fake");
    let mut input = child.stdin.take().expect("stdin");
    let mut output = BufReader::new(child.stdout.take().expect("stdout"));
    let mut call = |seq: u32, request: Request| {
        ipc::write_frame(&mut input, seq, &request.encode()).expect("sent");
        let (got, reply) = ipc::read_reply(&mut output)
            .expect("read")
            .expect("a reply");
        assert_eq!(got, seq);
        reply
    };
    let welcome = call(
        1,
        Request::Hello {
            version: ipc::VERSION,
        },
    );
    assert!(
        matches!(welcome, Reply::Welcome { version: ipc::VERSION, ref backend, .. } if backend == "fake")
    );
    let Reply::Opened { id } = call(
        2,
        Request::OpenDecoder {
            codec: Codec::H264,
            width: 320,
            height: 180,
            hardware: true,
        },
    ) else {
        panic!("a decoder");
    };
    let picture = call(
        3,
        Request::Decode {
            id,
            keyframe: true,
            data: vec![0, 0, 0, 1, 0x65, 0x88],
        },
    );
    assert!(
        matches!(picture, Reply::Picture(p) if (p.planes.width, p.planes.height) == (320, 180))
    );
    // Shown at half size: the pictures come at half size.
    assert_eq!(
        call(
            4,
            Request::SetOutputSize {
                id,
                width: 160,
                height: 90,
            },
        ),
        Reply::Done
    );
    let picture = call(
        5,
        Request::Decode {
            id,
            keyframe: false,
            data: vec![0, 0, 0, 1, 0x41, 0x88],
        },
    );
    assert!(matches!(
        picture,
        Reply::Picture(p) if (p.planes.width, p.planes.height) == (160, 90) && p.source == (320, 180)
    ));
    // Closing its input ends it, cleanly.
    input.flush().expect("flushed");
    drop(input);
    let status = child.wait().expect("it ends");
    assert!(status.success());
}

#[test]
fn without_hardware_the_welcome_lists_nothing_and_software_decodes() {
    let mut child = helper("none");
    let mut input = child.stdin.take().expect("stdin");
    let mut output = BufReader::new(child.stdout.take().expect("stdout"));
    let mut call = |seq: u32, request: Request| {
        ipc::write_request(&mut input, seq, &request).expect("sent");
        let (got, reply) = ipc::read_reply(&mut output)
            .expect("read")
            .expect("a reply");
        assert_eq!(got, seq);
        reply
    };
    let welcome = call(
        1,
        Request::Hello {
            version: ipc::VERSION,
        },
    );
    assert!(matches!(welcome, Reply::Welcome { capabilities, .. } if capabilities.is_empty()));
    let Reply::Opened { id } = call(
        2,
        Request::OpenDecoder {
            codec: Codec::H264,
            width: 480,
            height: 480,
            hardware: true,
        },
    ) else {
        panic!("a decoder");
    };
    let frames = noslacking_video::nal::access_units(include_bytes!(
        "../../../src/huddle_audio/fixtures/camera-480x480.h264"
    ));
    for (seq, frame) in (3..).zip(&frames[..3]) {
        let picture = call(
            seq,
            Request::Decode {
                id,
                keyframe: noslacking_video::nal::is_keyframe(frame),
                data: frame.clone(),
            },
        );
        assert!(
            matches!(&picture, Reply::Picture(p) if !p.hardware && p.source == (480, 480)),
            "{picture:?}"
        );
    }
    drop(input);
    assert!(child.wait().expect("it ends").success());
}

#[test]
fn the_helper_says_its_version_and_probes() {
    let output = Command::new(env!("CARGO_BIN_EXE_noslacking-video"))
        .arg("--version")
        .output()
        .expect("runs");
    assert!(output.status.success());
    assert!(String::from_utf8_lossy(&output.stdout).contains("protocol 3"));
    let output = Command::new(env!("CARGO_BIN_EXE_noslacking-video"))
        .arg("--probe")
        .env("NOSLACKING_VIDEO_BACKEND", "none")
        .output()
        .expect("runs");
    assert!(String::from_utf8_lossy(&output.stdout).contains("no hardware video"));
    let output = Command::new(env!("CARGO_BIN_EXE_noslacking-video"))
        .arg("--bogus")
        .output()
        .expect("runs");
    assert!(!output.status.success());
}
