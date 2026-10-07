//! The helper's side of the conversation: reads requests, hands them to
//! the back end (or a share's capture), writes one reply for each, until
//! its input closes.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::time::Duration;

use noslacking_video_ipc::{self as ipc, FailKind, Reply, Request};

use crate::backend::{self, Backend, Decoder, Encoder};
use crate::capture::{Screens, Trouble};
use crate::share::{Settings, Share};

/// The most decoders and encoders open at once: a huddle's share, its
/// camera tiles and your own camera are far fewer.
const MAX_OPEN: usize = 64;

/// Serves requests from `input` until it closes, replying on `output`.
///
/// The first request must be the hello; anything else first ends the
/// conversation. A message that does not decode gets a protocol failure
/// and the conversation goes on (its frame was whole); broken framing
/// ends it, since nothing after it can be trusted to line up.
///
/// Screen shares are captured from `screens`; when the input closes, any
/// share still open is stopped before this returns.
pub fn serve(
    input: &mut impl Read,
    output: &mut impl Write,
    backend: &mut dyn Backend,
    screens: &mut dyn Screens,
) -> Result<(), ipc::Error> {
    let mut server = Server {
        backend,
        screens,
        decoders: HashMap::new(),
        encoders: HashMap::new(),
        shares: HashMap::new(),
        next_id: 1,
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
    // Pictures to encode are read straight into their planes.
    while let Some((seq, request)) = ipc::read_request(input)? {
        let reply = match request {
            Ok(request) => server.handle(request),
            Err(error) => failed(FailKind::Protocol, &error.to_string()),
        };
        ipc::write_reply(output, seq, &reply)?;
    }
    Ok(())
}

fn failed(kind: FailKind, detail: &str) -> Reply {
    Reply::Failed {
        kind,
        detail: detail.to_owned(),
    }
}

/// The most screen shares open at once: the app makes one; its tests,
/// sharing one helper, a few.
const MAX_SHARES: usize = 4;

fn share_problem(trouble: Trouble) -> Reply {
    Reply::ShareProblem {
        problem: trouble.problem,
        detail: trouble.detail,
    }
}

struct Server<'a> {
    backend: &'a mut dyn Backend,
    screens: &'a mut dyn Screens,
    decoders: HashMap<u32, Box<dyn Decoder>>,
    encoders: HashMap<u32, Box<dyn Encoder>>,
    shares: HashMap<u32, Share>,
    next_id: u32,
}

impl Server<'_> {
    fn id(&mut self) -> u32 {
        let id = self.next_id;
        self.next_id = self.next_id.wrapping_add(1).max(1);
        id
    }

    fn full(&self) -> bool {
        self.decoders.len() + self.encoders.len() + self.shares.len() >= MAX_OPEN
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
                if self.full() {
                    return failed(FailKind::Unsupported, "too many open");
                }
                let decoder = backend::open_decoder(self.backend, codec, width, height, hardware);
                let id = self.id();
                self.decoders.insert(id, decoder);
                Reply::Opened { id }
            }
            Request::Decode { id, keyframe, data } => {
                let Some(decoder) = self.decoders.get_mut(&id) else {
                    return failed(FailKind::UnknownId, "no such decoder");
                };
                match decoder.decode(&data, keyframe) {
                    Ok(Some(picture)) => match picture.check() {
                        Ok(()) => Reply::Picture(picture),
                        // Our own bug, but the app must not get it.
                        Err(error) => failed(FailKind::Device, &error.to_string()),
                    },
                    Ok(None) => Reply::NoPicture,
                    Err(failure) => failed(failure.kind, &failure.detail),
                }
            }
            Request::OpenEncoder {
                codec,
                width,
                height,
                fps,
                bitrate,
            } => {
                if self.full() {
                    return failed(FailKind::Unsupported, "too many open");
                }
                match self
                    .backend
                    .open_encoder(codec, width, height, fps, bitrate)
                {
                    Ok(encoder) => {
                        let id = self.id();
                        self.encoders.insert(id, encoder);
                        Reply::Opened { id }
                    }
                    Err(failure) => failed(failure.kind, &failure.detail),
                }
            }
            Request::Encode {
                id,
                force_keyframe,
                picture,
            } => {
                let Some(encoder) = self.encoders.get_mut(&id) else {
                    return failed(FailKind::UnknownId, "no such encoder");
                };
                match encoder.encode(&picture, force_keyframe) {
                    Ok(encoded) => Reply::Encoded {
                        keyframe: encoded.keyframe,
                        data: encoded.data,
                    },
                    Err(failure) => failed(failure.kind, &failure.detail),
                }
            }
            Request::SetBitrate { id, bitrate } => {
                if let Some(share) = self.shares.get(&id) {
                    share.set_bitrate(bitrate);
                    return Reply::Done;
                }
                let Some(encoder) = self.encoders.get_mut(&id) else {
                    return failed(FailKind::UnknownId, "no such encoder");
                };
                match encoder.set_bitrate(bitrate) {
                    Ok(()) => Reply::Done,
                    Err(failure) => failed(failure.kind, &failure.detail),
                }
            }
            Request::SetOutputSize { id, width, height } => {
                let Some(decoder) = self.decoders.get_mut(&id) else {
                    return failed(FailKind::UnknownId, "no such decoder");
                };
                decoder.set_output_size(width, height);
                Reply::Done
            }
            Request::Close { id } => {
                if self.decoders.remove(&id).is_some()
                    || self.encoders.remove(&id).is_some()
                    || self.shares.remove(&id).is_some()
                {
                    Reply::Done
                } else {
                    failed(FailKind::UnknownId, "nothing open with that number")
                }
            }
            Request::ListSources => match self.screens.sources() {
                Ok((dialog, sources)) => Reply::Sources { dialog, sources },
                Err(trouble) => share_problem(trouble),
            },
            Request::StartShare {
                choice,
                hardware,
                bitrate,
                restore,
            } => {
                if self.full() || self.shares.len() >= MAX_SHARES {
                    return failed(FailKind::Unsupported, "too many open");
                }
                let settings = Settings {
                    hardware,
                    bitrate,
                    gpu: self.backend.share_gpu(),
                };
                match self.screens.start(&choice, settings, &restore) {
                    Ok((share, restore)) => {
                        let id = self.id();
                        self.shares.insert(id, share);
                        Reply::ShareStarted { id, restore }
                    }
                    Err(trouble) => share_problem(trouble),
                }
            }
            Request::NextShareFrame {
                id,
                force_keyframe,
                repeat,
                wait_ms,
            } => {
                let Some(share) = self.shares.get(&id) else {
                    return failed(FailKind::UnknownId, "no such share");
                };
                let wait = Duration::from_millis(u64::from(wait_ms.min(ipc::MAX_SHARE_WAIT_MS)));
                match share.next(force_keyframe, repeat, wait) {
                    Ok(Some(frame)) => Reply::ShareFrame(frame),
                    Ok(None) => Reply::NoPicture,
                    Err(trouble) => share_problem(trouble),
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
    use noslacking_video_ipc::{Codec, Planes};

    const CAMERA: &[u8] = include_bytes!("../../../src/huddle_audio/fixtures/camera-480x480.h264");

    /// Runs `requests` through a server with `backend` and returns its
    /// replies, each with the sequence number it carried.
    fn talk(backend: &mut dyn Backend, requests: &[Vec<u8>]) -> (Vec<(u32, Reply)>, bool) {
        let mut input = Vec::new();
        for (seq, body) in requests.iter().enumerate() {
            ipc::write_frame(&mut input, 10 + seq as u32, body).expect("written");
        }
        let mut output = Vec::new();
        let ok = serve(
            &mut input.as_slice(),
            &mut output,
            backend,
            &mut crate::capture::Pretend::default(),
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
                Request::OpenEncoder {
                    codec: Codec::H264,
                    width: 4,
                    height: 2,
                    fps: 30,
                    bitrate: 1_000_000,
                }
                .encode(),
                Request::Encode {
                    id: 2,
                    force_keyframe: false,
                    picture: Planes {
                        width: 4,
                        height: 2,
                        y: vec![0; 8],
                        u: vec![0; 2],
                        v: vec![0; 2],
                    },
                }
                .encode(),
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
            (10..21).collect::<Vec<_>>(),
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
        assert_eq!(replies[8], Reply::Opened { id: 2 });
        assert!(matches!(&replies[9], Reply::Encoded { keyframe: true, .. }));
        assert_eq!(replies[10], Reply::Done);
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
        let frames = crate::nal::access_units(CAMERA);
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
        fn decode(
            &mut self,
            _: &[u8],
            _: bool,
        ) -> Result<Option<ipc::Decoded>, crate::backend::Failure> {
            let n = self.frames;
            self.frames += 1;
            if let Some(failure) = (self.fail)(n) {
                return Err(failure);
            }
            Ok(Some(ipc::Decoded {
                planes: Planes {
                    width: 2,
                    height: 2,
                    y: vec![200; 4],
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
        let frames = crate::nal::access_units(CAMERA);
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
                &mut crate::capture::Pretend::default()
            )
            .is_err()
        );
    }
}
