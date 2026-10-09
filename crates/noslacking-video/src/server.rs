//! The helper's side of the conversation: reads requests, hands them to
//! the back end (or a capture: a share or the camera), writes one reply
//! for each, until its input closes.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::time::Duration;

use noslacking_video_ipc::{self as ipc, FailKind, Reply, Request};

use crate::backend::{self, Backend, Decoder};
use crate::capture::camera::Cameras;
use crate::capture::{Screens, Trouble};
use crate::pipeline::{Capture, Settings};

/// The most decoders and captures open at once: a huddle's share, its
/// camera tiles and your own camera are far fewer.
const MAX_OPEN: usize = 64;

/// What captures the server starts: the screens a share is taken from,
/// and the cameras.
pub struct Sources<'a> {
    /// Where shares come from.
    pub screens: &'a mut dyn Screens,
    /// Where the camera comes from.
    pub cameras: &'a mut dyn Cameras,
}

/// Serves requests from `input` until it closes, replying on `output`.
///
/// The first request must be the hello; anything else first ends the
/// conversation. A message that does not decode gets a protocol failure
/// and the conversation goes on (its frame was whole); broken framing
/// ends it, since nothing after it can be trusted to line up.
///
/// Screen shares are captured from `sources.screens` and cameras from
/// `sources.cameras`; when the input closes, any capture still open is
/// stopped (a camera closed) before this returns.
pub fn serve(
    input: &mut impl Read,
    output: &mut impl Write,
    backend: &mut dyn Backend,
    sources: Sources<'_>,
) -> Result<(), ipc::Error> {
    let mut server = Server {
        backend,
        screens: sources.screens,
        cameras: sources.cameras,
        decoders: HashMap::new(),
        captures: HashMap::new(),
        next_id: 1,
        answering: None,
    };
    let Some((first, hello)) = ipc::read_request(input)? else {
        return Ok(());
    };
    match hello {
        Ok(Request::Hello { .. }) => {
            // The app compares versions; ours goes back either way.
            let welcome = Reply::Welcome {
                version: ipc::VERSION,
                backend: server.backend.name(),
                capabilities: server.backend.capabilities(),
            };
            ipc::write_frame(output, first, &welcome.encode())?;
        }
        _ => {
            let reply = failed(FailKind::Protocol, "the first message must be the hello");
            ipc::write_frame(output, first, &reply.encode())?;
            return Err(ipc::Error::BadValue("first message"));
        }
    }
    while let Some((seq, request)) = ipc::read_request(input)? {
        let reply = match request {
            Ok(request) => server.handle(request),
            Err(error) => failed(FailKind::Protocol, &error.to_string()),
        };
        // A decoded picture goes from its planes into the pipe.
        ipc::write_reply(output, seq, &reply)?;
        server.sent(reply);
    }
    Ok(())
}

fn failed(kind: FailKind, detail: &str) -> Reply {
    Reply::Failed {
        kind,
        detail: detail.to_owned(),
    }
}

/// The most captures open at once: the app makes a share and a camera;
/// its tests, sharing one helper, a few.
const MAX_CAPTURES: usize = 4;

fn problem(trouble: Trouble) -> Reply {
    Reply::Problem {
        problem: trouble.problem,
        detail: trouble.detail,
    }
}

/// One stream's decoder, and what of it the app has.
struct Stream {
    decoder: Box<dyn Decoder>,
    /// The last frame's picture was kept back (`Decode` without `show`):
    /// a `Fetch` reads it.
    kept: bool,
    /// The last picture sent: the same again is answered `Unchanged`
    /// rather than sent, converted and uploaded again (a still screen
    /// sent once a second, a share's keepalive).
    sent: Option<ipc::Decoded>,
}

impl Stream {
    /// The last frame's picture as a reply: the picture, `Unchanged` if
    /// it is the one last sent, or none.
    fn picture(&mut self) -> Reply {
        match self.decoder.picture() {
            Ok(Some(picture)) => match picture.check() {
                Ok(()) if self.sent.as_ref() == Some(&picture) => Reply::Unchanged,
                Ok(()) => Reply::Picture(picture),
                // Our own bug, but the app must not get it.
                Err(error) => failed(FailKind::Device, &error.to_string()),
            },
            Ok(None) => Reply::NoPicture,
            Err(failure) => failed(failure.kind, &failure.detail),
        }
    }
}

struct Server<'a> {
    backend: &'a mut dyn Backend,
    screens: &'a mut dyn Screens,
    cameras: &'a mut dyn Cameras,
    decoders: HashMap<u32, Stream>,
    captures: HashMap<u32, Capture>,
    next_id: u32,
    /// The decoder the reply being written is about.
    answering: Option<u32>,
}

impl Server<'_> {
    /// `reply` was written: a picture is remembered as its decoder's
    /// last sent (moved, not copied).
    fn sent(&mut self, reply: Reply) {
        let id = self.answering.take();
        if let Reply::Picture(picture) = reply
            && let Some(stream) = id.and_then(|id| self.decoders.get_mut(&id))
        {
            stream.sent = Some(picture);
        }
    }

    fn id(&mut self) -> u32 {
        let id = self.next_id;
        self.next_id = self.next_id.wrapping_add(1).max(1);
        id
    }

    fn full(&self) -> bool {
        self.decoders.len() + self.captures.len() >= MAX_OPEN || self.captures.len() >= MAX_CAPTURES
    }

    /// Keeps a capture that started, or says why it did not.
    fn started(&mut self, started: Result<(Capture, String), Trouble>) -> Reply {
        match started {
            Ok((capture, restore)) => {
                let id = self.id();
                self.captures.insert(id, capture);
                Reply::Started { id, restore }
            }
            Err(trouble) => problem(trouble),
        }
    }

    fn handle(&mut self, request: Request) -> Reply {
        match request {
            Request::Hello { .. } => failed(FailKind::Protocol, "hello twice"),
            Request::OpenDecoder {
                codec,
                width,
                height,
                hardware,
            } => {
                if self.decoders.len() + self.captures.len() >= MAX_OPEN {
                    return failed(FailKind::Unsupported, "too many open");
                }
                let decoder = backend::open_decoder(self.backend, codec, width, height, hardware);
                let id = self.id();
                self.decoders.insert(
                    id,
                    Stream {
                        decoder,
                        kept: false,
                        sent: None,
                    },
                );
                Reply::Opened { id }
            }
            Request::Decode {
                id,
                keyframe,
                show,
                data,
            } => {
                let Some(stream) = self.decoders.get_mut(&id) else {
                    return failed(FailKind::UnknownId, "no such decoder");
                };
                stream.kept = false;
                match stream.decoder.decode_frame(&data, keyframe) {
                    Ok(true) if show => {
                        self.answering = Some(id);
                        stream.picture()
                    }
                    Ok(true) => {
                        stream.kept = true;
                        Reply::Kept
                    }
                    Ok(false) => Reply::NoPicture,
                    Err(failure) => failed(failure.kind, &failure.detail),
                }
            }
            Request::Fetch { id } => {
                let Some(stream) = self.decoders.get_mut(&id) else {
                    return failed(FailKind::UnknownId, "no such decoder");
                };
                if !std::mem::take(&mut stream.kept) {
                    return Reply::NoPicture;
                }
                self.answering = Some(id);
                stream.picture()
            }
            Request::SetBitrate { id, bitrate } => match self.captures.get(&id) {
                Some(capture) => {
                    capture.set_bitrate(bitrate);
                    Reply::Done
                }
                None => failed(FailKind::UnknownId, "no such capture"),
            },
            Request::SetMaxSize { id, width, height } => match self.captures.get(&id) {
                Some(capture) => {
                    capture.set_max_size((width > 0 && height > 0).then_some((width, height)));
                    Reply::Done
                }
                None => failed(FailKind::UnknownId, "no such capture"),
            },
            Request::SetOutputSize { id, width, height } => {
                let Some(stream) = self.decoders.get_mut(&id) else {
                    return failed(FailKind::UnknownId, "no such decoder");
                };
                stream.decoder.set_output_size(width, height);
                Reply::Done
            }
            Request::Close { id } => {
                if self.decoders.remove(&id).is_some() || self.captures.remove(&id).is_some() {
                    Reply::Done
                } else {
                    failed(FailKind::UnknownId, "nothing open with that number")
                }
            }
            Request::ListSources => match self.screens.sources() {
                Ok((dialog, sources)) => Reply::Sources { dialog, sources },
                Err(trouble) => problem(trouble),
            },
            Request::StartShare {
                choice,
                hardware,
                bitrate,
                restore,
            } => {
                if self.full() {
                    return failed(FailKind::Unsupported, "too many open");
                }
                let settings = Settings::share(hardware, bitrate, self.backend.capture_gpu());
                let started = self.screens.start(&choice, settings, &restore);
                self.started(started)
            }
            Request::ListCameras => match self.cameras.list() {
                Ok(sources) => Reply::Sources {
                    dialog: false,
                    sources,
                },
                Err(trouble) => problem(trouble),
            },
            Request::StartCamera {
                choice,
                hardware,
                bitrate,
                preview,
            } => {
                if self.full() {
                    return failed(FailKind::Unsupported, "too many open");
                }
                let settings =
                    Settings::camera(hardware, bitrate, self.backend.capture_gpu(), preview);
                let started = self
                    .cameras
                    .start(&choice, settings)
                    .map(|capture| (capture, String::new()));
                self.started(started)
            }
            Request::NextFrame {
                id,
                force_keyframe,
                repeat,
                wait_ms,
            } => {
                let Some(capture) = self.captures.get(&id) else {
                    return failed(FailKind::UnknownId, "no such capture");
                };
                let wait = Duration::from_millis(u64::from(wait_ms.min(ipc::MAX_WAIT_MS)));
                match capture.next(force_keyframe, repeat, wait) {
                    Ok(Some(frame)) => Reply::Frame(frame),
                    Ok(None) => Reply::NoPicture,
                    Err(trouble) => problem(trouble),
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::{Failure, Nothing};
    use crate::fake::Fake;
    use noslacking_video_ipc::{CameraChoice, CaptureProblem, Codec, Planes, Source, SourceKind};

    const CAMERA: &[u8] = include_bytes!("../../../src/huddle_audio/fixtures/camera-480x480.h264");

    /// Runs `requests` through a server with `backend` and the pretend
    /// screens and cameras, and returns its replies, each with the
    /// sequence number it carried.
    fn talk(backend: &mut dyn Backend, requests: &[Vec<u8>]) -> (Vec<(u32, Reply)>, bool) {
        let cameras = vec![Source {
            id: "pretend:1".into(),
            name: "A pretend camera".into(),
            kind: SourceKind::Camera,
        }];
        talk_with(backend, cameras, requests)
    }

    fn talk_with(
        backend: &mut dyn Backend,
        cameras: Vec<Source>,
        requests: &[Vec<u8>],
    ) -> (Vec<(u32, Reply)>, bool) {
        let mut input = Vec::new();
        for (seq, body) in requests.iter().enumerate() {
            ipc::write_frame(&mut input, 10 + seq as u32, body).expect("written");
        }
        let mut output = Vec::new();
        let ok = serve(
            &mut input.as_slice(),
            &mut output,
            backend,
            Sources {
                screens: &mut crate::capture::Pretend::default(),
                cameras: &mut crate::capture::camera::Pretend {
                    cameras,
                    refuse: None,
                },
            },
        )
        .is_ok();
        let mut replies = Vec::new();
        let mut output = output.as_slice();
        while let Some(frame) = ipc::read_frame(&mut output).expect("whole frames") {
            replies.push((frame.seq, Reply::decode(&frame.body).expect("a reply")));
        }
        (replies, ok)
    }

    fn hello() -> Vec<u8> {
        Request::Hello {
            version: ipc::VERSION,
        }
        .encode()
    }

    fn kind(reply: &Reply) -> Option<FailKind> {
        match reply {
            Reply::Failed { kind, .. } => Some(*kind),
            _ => None,
        }
    }

    #[test]
    fn a_conversation_with_the_fake_back_end() {
        let open = Request::OpenDecoder {
            codec: Codec::H264,
            width: 64,
            height: 48,
            hardware: true,
        };
        let decode = |id, keyframe| {
            Request::Decode {
                id,
                keyframe,
                show: true,
                data: vec![0, 0, 0, 1, if keyframe { 0x65 } else { 0x41 }],
            }
            .encode()
        };
        let (replies, ok) = talk(
            &mut Fake,
            &[
                hello(),
                open.encode(),
                decode(1, false),
                decode(1, true),
                Request::SetOutputSize {
                    id: 1,
                    width: 32,
                    height: 24,
                }
                .encode(),
                decode(9, true),
                Request::Close { id: 1 }.encode(),
                Request::Close { id: 1 }.encode(),
                Request::SetBitrate {
                    id: 2,
                    bitrate: 500_000,
                }
                .encode(),
            ],
        );
        assert!(ok);
        let seqs: Vec<u32> = replies.iter().map(|(seq, _)| *seq).collect();
        assert_eq!(
            seqs,
            (10..19).collect::<Vec<_>>(),
            "one reply each, in order"
        );
        let replies: Vec<Reply> = replies.into_iter().map(|(_, reply)| reply).collect();
        let Reply::Welcome {
            version,
            backend,
            capabilities,
        } = &replies[0]
        else {
            panic!("a welcome first: {:?}", replies[0]);
        };
        assert_eq!((*version, backend.as_str()), (ipc::VERSION, "fake"));
        assert_eq!(capabilities.len(), 2);
        assert_eq!(replies[1], Reply::Opened { id: 1 });
        assert_eq!(kind(&replies[2]), Some(FailKind::NeedKeyframe));
        assert!(matches!(
            &replies[3],
            Reply::Picture(p) if (p.planes.width, p.planes.height) == (64, 48) && p.hardware
        ));
        assert_eq!(replies[4], Reply::Done, "the output size, set");
        assert_eq!(kind(&replies[5]), Some(FailKind::UnknownId));
        assert_eq!(replies[6], Reply::Done);
        assert_eq!(
            kind(&replies[7]),
            Some(FailKind::UnknownId),
            "closed already"
        );
        assert_eq!(
            kind(&replies[8]),
            Some(FailKind::UnknownId),
            "no capture to retune"
        );
    }

    /// Pictures the app will not show are kept back and fetched when it
    /// can: only the newest, never a queue; a picture the same as the
    /// last sent is answered `Unchanged`.
    #[test]
    fn pictures_not_shown_are_kept_and_fetched_and_the_same_is_not_sent_again() {
        let open = Request::OpenDecoder {
            codec: Codec::H264,
            width: 64,
            height: 48,
            hardware: true,
        };
        // A frame that changes the fake's picture, or (`same`) one that
        // does not.
        let decode = |keyframe, show, same: bool| {
            let mut data = vec![0, 0, 0, 1, if keyframe { 0x65 } else { 0x41 }];
            if same {
                data = vec![0, 0, 0, 1, 0x41, 0xff];
            }
            Request::Decode {
                id: 1,
                keyframe,
                show,
                data,
            }
            .encode()
        };
        let fetch = Request::Fetch { id: 1 }.encode();
        let (replies, ok) = talk(
            &mut Fake,
            &[
                hello(),
                open.encode(),
                // Nothing kept yet.
                fetch.clone(),
                decode(true, true, false),
                // Two frames the app will not show: only the newest waits.
                decode(false, false, false),
                decode(false, false, false),
                fetch.clone(),
                // Fetched already.
                fetch.clone(),
                // The same picture again, shown or fetched: not sent.
                decode(false, true, true),
                decode(false, false, true),
                fetch.clone(),
                // Kept, then a frame shown: the kept one is gone.
                decode(false, false, false),
                decode(false, true, false),
                fetch.clone(),
                // Kept at one size and fetched at another: shrunk as it
                // is fetched.
                decode(false, false, false),
                Request::SetOutputSize {
                    id: 1,
                    width: 32,
                    height: 24,
                }
                .encode(),
                fetch.clone(),
                Request::Fetch { id: 9 }.encode(),
            ],
        );
        assert!(ok);
        let replies: Vec<Reply> = replies.into_iter().map(|(_, reply)| reply).collect();
        let shade = |reply: &Reply| match reply {
            Reply::Picture(p) => Some((p.planes.width, p.planes.y[0])),
            _ => None,
        };
        assert_eq!(replies[2], Reply::NoPicture);
        assert_eq!(shade(&replies[3]), Some((64, 1)));
        assert_eq!(replies[4], Reply::Kept);
        assert_eq!(replies[5], Reply::Kept);
        assert_eq!(shade(&replies[6]), Some((64, 3)), "the newest only");
        assert_eq!(replies[7], Reply::NoPicture);
        assert_eq!(replies[8], Reply::Unchanged);
        assert_eq!(replies[9], Reply::Kept);
        assert_eq!(replies[10], Reply::Unchanged);
        assert_eq!(replies[11], Reply::Kept);
        assert_eq!(shade(&replies[12]), Some((64, 5)));
        assert_eq!(replies[13], Reply::NoPicture);
        assert_eq!(replies[14], Reply::Kept);
        assert_eq!(replies[15], Reply::Done);
        assert_eq!(shade(&replies[16]), Some((32, 6)));
        assert_eq!(kind(&replies[17]), Some(FailKind::UnknownId));
    }

    /// The camera through the server: listed, started (the test camera
    /// whatever is chosen), its frames pulled with their self-views, its
    /// rate changed, closed; a camera that is not there says so.
    #[test]
    fn a_camera_is_listed_started_pulled_and_closed() {
        let start = |choice| {
            Request::StartCamera {
                choice,
                hardware: false,
                bitrate: 600_000,
                preview: 320,
            }
            .encode()
        };
        let next = Request::NextFrame {
            id: 1,
            force_keyframe: false,
            repeat: false,
            wait_ms: ipc::MAX_WAIT_MS,
        }
        .encode();
        let (replies, ok) = talk(
            &mut Nothing::new("none: a test"),
            &[
                hello(),
                Request::ListCameras.encode(),
                start(CameraChoice::First),
                next.clone(),
                next,
                Request::SetBitrate {
                    id: 1,
                    bitrate: 250_000,
                }
                .encode(),
                Request::SetMaxSize {
                    id: 1,
                    width: 320,
                    height: 180,
                }
                .encode(),
                Request::SetMaxSize {
                    id: 9,
                    width: 320,
                    height: 180,
                }
                .encode(),
                Request::Close { id: 1 }.encode(),
                start(CameraChoice::Device("pretend:7".into())),
            ],
        );
        assert!(ok);
        let replies: Vec<Reply> = replies.into_iter().map(|(_, reply)| reply).collect();
        assert!(matches!(
            &replies[1],
            Reply::Sources { dialog: false, sources } if sources.len() == 1
                && sources[0].kind == SourceKind::Camera
        ));
        assert_eq!(
            replies[2],
            Reply::Started {
                id: 1,
                restore: String::new()
            }
        );
        for (n, reply) in replies[3..5].iter().enumerate() {
            let Reply::Frame(frame) = reply else {
                panic!("a frame: {reply:?}");
            };
            assert_eq!(frame.keyframe, n == 0);
            assert_eq!((frame.width, frame.height), (640, 480));
            assert!(!frame.hardware);
            let preview = frame.preview.as_ref().expect("a self-view");
            assert_eq!((preview.width, preview.height), (320, 240));
        }
        assert_eq!(replies[5], Reply::Done);
        assert_eq!(replies[6], Reply::Done, "a box for the camera");
        assert!(
            matches!(
                &replies[7],
                Reply::Failed {
                    kind: FailKind::UnknownId,
                    ..
                }
            ),
            "a box for no capture: {:?}",
            replies[7]
        );
        assert_eq!(replies[8], Reply::Done);
        assert!(matches!(
            replies[9],
            Reply::Problem {
                problem: CaptureProblem::Gone,
                ..
            }
        ));
        // No camera at all.
        let (replies, _) = talk_with(
            &mut Fake,
            Vec::new(),
            &[
                hello(),
                Request::ListCameras.encode(),
                start(CameraChoice::First),
            ],
        );
        assert!(matches!(&replies[1].1, Reply::Sources { sources, .. } if sources.is_empty()));
        assert!(matches!(
            replies[2].1,
            Reply::Problem {
                problem: CaptureProblem::Unavailable,
                ..
            }
        ));
    }

    /// A self-view asked wider than allowed is a bad message, and the
    /// conversation goes on.
    #[test]
    fn a_camera_asked_for_too_wide_a_self_view_is_refused() {
        let mut wide = Request::StartCamera {
            choice: CameraChoice::Test,
            hardware: false,
            bitrate: 1,
            preview: 0,
        }
        .encode();
        let last = wide.len() - 4;
        wide[last..].copy_from_slice(&(ipc::MAX_PREVIEW_SIDE + 1).to_le_bytes());
        let (replies, ok) = talk(&mut Fake, &[hello(), wide, Request::ListCameras.encode()]);
        assert!(ok);
        assert_eq!(kind(&replies[1].1), Some(FailKind::Protocol));
        assert!(matches!(replies[2].1, Reply::Sources { .. }));
    }

    /// Decodes `frames` (each with whether it is a keyframe) through a
    /// server on `backend`, asking for the GPU or not, at a 240×180
    /// box: the replies after the hello, the open and the box.
    fn decode_through(
        backend: &mut dyn Backend,
        hardware: bool,
        frames: &[(&[u8], bool)],
    ) -> Vec<Reply> {
        let mut requests = vec![
            hello(),
            Request::OpenDecoder {
                codec: Codec::H264,
                width: 480,
                height: 480,
                hardware,
            }
            .encode(),
            Request::SetOutputSize {
                id: 1,
                width: 240,
                height: 180,
            }
            .encode(),
        ];
        for (frame, keyframe) in frames {
            requests.push(
                Request::Decode {
                    id: 1,
                    keyframe: *keyframe,
                    show: true,
                    data: frame.to_vec(),
                }
                .encode(),
            );
        }
        let (replies, ok) = talk(backend, &requests);
        assert!(ok);
        assert_eq!(replies[1].1, Reply::Opened { id: 1 });
        assert_eq!(replies[2].1, Reply::Done);
        replies
            .into_iter()
            .skip(3)
            .map(|(_, reply)| reply)
            .collect()
    }

    /// Whether `reply` is a software picture of the camera, halved.
    fn software_picture(reply: &Reply) -> bool {
        matches!(
            reply,
            Reply::Picture(p) if !p.hardware
                && p.source == (480, 480)
                && (p.planes.width, p.planes.height) == (240, 240)
        )
    }

    #[test]
    fn without_hardware_streams_decode_in_software() {
        let frames = noslacking_video_ipc::h264::access_units(CAMERA);
        let (replies, ok) = talk(&mut Nothing::new("none: no libva"), &[hello()]);
        assert!(ok);
        assert!(matches!(
            &replies[0].1,
            Reply::Welcome { capabilities, backend, .. }
                if capabilities.is_empty() && backend == "none: no libva"
        ));
        let replies = decode_through(
            &mut Nothing::new("none: no libva"),
            true,
            &[(&frames[3], false), (&frames[0], true), (&frames[1], false)],
        );
        assert_eq!(kind(&replies[0]), Some(FailKind::NeedKeyframe));
        assert!(software_picture(&replies[1]), "{:?}", replies[1]);
        assert!(software_picture(&replies[2]), "{:?}", replies[2]);
        // With the GPU there but not wanted: software too.
        let replies = decode_through(&mut Fake, false, &[(&frames[0], true)]);
        assert!(software_picture(&replies[0]), "{:?}", replies[0]);
    }

    /// A back end whose GPU decoder fails as `fail` says for each frame
    /// (counted from 0), and else gives a grey picture.
    struct Scripted(fn(usize) -> Option<Failure>);

    struct ScriptedDecoder {
        fail: fn(usize) -> Option<Failure>,
        frames: usize,
    }

    impl Backend for Scripted {
        fn name(&self) -> String {
            "scripted".into()
        }

        fn capabilities(&self) -> Vec<ipc::Capability> {
            Vec::new()
        }

        fn open_decoder(
            &mut self,
            _: Codec,
            _: u32,
            _: u32,
        ) -> Result<Box<dyn Decoder>, crate::backend::Failure> {
            Ok(Box::new(ScriptedDecoder {
                fail: self.0,
                frames: 0,
            }))
        }
    }

    impl Decoder for ScriptedDecoder {
        fn decode_frame(&mut self, _: &[u8], _: bool) -> Result<bool, crate::backend::Failure> {
            let n = self.frames;
            self.frames += 1;
            if let Some(failure) = (self.fail)(n) {
                return Err(failure);
            }
            Ok(true)
        }

        fn picture(&mut self) -> Result<Option<ipc::Decoded>, crate::backend::Failure> {
            // Each frame's picture differs from the last.
            let shade = u8::try_from(self.frames % 200).unwrap_or(0);
            Ok(Some(ipc::Decoded {
                planes: Planes {
                    width: 2,
                    height: 2,
                    y: vec![shade; 4],
                    u: vec![128],
                    v: vec![128],
                },
                source: (480, 480),
                hardware: true,
            }))
        }

        fn set_output_size(&mut self, _: u32, _: u32) {}
    }

    #[test]
    fn software_takes_over_where_the_gpu_fails() {
        let frames = noslacking_video_ipc::h264::access_units(CAMERA);
        let gpu = |reply: &Reply| matches!(reply, Reply::Picture(p) if p.hardware);
        // The GPU cannot decode the stream: software decodes the
        // keyframe in hand, and what follows.
        let replies = decode_through(
            &mut Scripted(|_| Some(Failure::unsupported("B slices"))),
            true,
            &[(&frames[0], true), (&frames[1], false)],
        );
        assert!(software_picture(&replies[0]), "{:?}", replies[0]);
        assert!(software_picture(&replies[1]), "{:?}", replies[1]);
        // The device fails between keyframes: a keyframe is asked for,
        // and software starts on it.
        let replies = decode_through(
            &mut Scripted(|n| (n == 1).then(|| Failure::device("hung"))),
            true,
            &[
                (&frames[0], true),
                (&frames[1], false),
                (&frames[2], false),
                (&frames[44], true),
                (&frames[45], false),
            ],
        );
        assert!(gpu(&replies[0]));
        assert_eq!(kind(&replies[1]), Some(FailKind::NeedKeyframe));
        assert_eq!(kind(&replies[2]), Some(FailKind::NeedKeyframe));
        assert!(software_picture(&replies[3]), "{:?}", replies[3]);
        assert!(software_picture(&replies[4]), "{:?}", replies[4]);
        // A frame broken between keyframes stays on the GPU; one broken
        // on a keyframe goes to software.
        let replies = decode_through(
            &mut Scripted(|n| matches!(n, 1 | 3).then(|| Failure::broken("bad slice"))),
            true,
            &[
                (&frames[0], true),
                (&frames[1], false),
                (&frames[2], false),
                (&frames[44], true),
            ],
        );
        assert!(gpu(&replies[0]));
        assert_eq!(kind(&replies[1]), Some(FailKind::Broken));
        assert!(gpu(&replies[2]));
        assert!(software_picture(&replies[3]), "{:?}", replies[3]);
    }

    #[test]
    fn bad_messages_get_a_protocol_failure_and_the_server_goes_on() {
        let (replies, ok) = talk(&mut Fake, &[hello(), vec![42], vec![2, 1], hello()]);
        assert!(ok);
        assert_eq!(replies.len(), 4);
        for (_, reply) in &replies[1..] {
            assert_eq!(kind(reply), Some(FailKind::Protocol));
        }
    }

    #[test]
    fn a_conversation_must_start_with_the_hello() {
        let (replies, ok) = talk(&mut Fake, &[Request::Close { id: 1 }.encode(), hello()]);
        assert!(!ok);
        assert_eq!(replies.len(), 1, "nothing after the bad start");
        assert_eq!(kind(&replies[0].1), Some(FailKind::Protocol));
        // No input at all is a clean end.
        let (replies, ok) = talk(&mut Fake, &[]);
        assert!(ok && replies.is_empty());
    }

    #[test]
    fn broken_framing_ends_the_conversation() {
        let mut input = Vec::new();
        ipc::write_frame(&mut input, 1, &hello()).expect("written");
        input.extend_from_slice(&[200, 0, 0]);
        let mut output = Vec::new();
        assert!(
            serve(
                &mut input.as_slice(),
                &mut output,
                &mut Fake,
                Sources {
                    screens: &mut crate::capture::Pretend::default(),
                    cameras: &mut crate::capture::camera::Pretend::default(),
                }
            )
            .is_err()
        );
    }
}
