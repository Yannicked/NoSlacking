//! The helper's side of the conversation: reads requests, hands them to
//! the back end, writes one reply for each, until its input closes.

use std::collections::HashMap;
use std::io::{Read, Write};

use noslacking_video_ipc::{self as ipc, FailKind, Reply, Request};

use crate::backend::{Backend, Decoder, Encoder};

/// The most decoders and encoders open at once: a huddle's share, its
/// camera tiles and your own camera are far fewer.
const MAX_OPEN: usize = 64;

/// Serves requests from `input` until it closes, replying on `output`.
///
/// The first request must be the hello; anything else first ends the
/// conversation. A message that does not decode gets a protocol failure
/// and the conversation goes on (its frame was whole); broken framing
/// ends it, since nothing after it can be trusted to line up.
pub fn serve(
    input: &mut impl Read,
    output: &mut impl Write,
    backend: &mut dyn Backend,
) -> Result<(), ipc::Error> {
    let mut server = Server {
        backend,
        decoders: HashMap::new(),
        encoders: HashMap::new(),
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

struct Server<'a> {
    backend: &'a mut dyn Backend,
    decoders: HashMap<u32, Box<dyn Decoder>>,
    encoders: HashMap<u32, Box<dyn Encoder>>,
    next_id: u32,
}

impl Server<'_> {
    fn id(&mut self) -> u32 {
        let id = self.next_id;
        self.next_id = self.next_id.wrapping_add(1).max(1);
        id
    }

    fn full(&self) -> bool {
        self.decoders.len() + self.encoders.len() >= MAX_OPEN
    }

    fn handle(&mut self, request: Request) -> Reply {
        match request {
            Request::Hello { .. } => failed(FailKind::Protocol, "hello twice"),
            Request::OpenDecoder {
                codec,
                width,
                height,
            } => {
                if self.full() {
                    return failed(FailKind::Unsupported, "too many open");
                }
                match self.backend.open_decoder(codec, width, height) {
                    Ok(decoder) => {
                        let id = self.id();
                        self.decoders.insert(id, decoder);
                        Reply::Opened { id }
                    }
                    Err(failure) => failed(failure.kind, &failure.detail),
                }
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
                if self.decoders.remove(&id).is_some() || self.encoders.remove(&id).is_some() {
                    Reply::Done
                } else {
                    failed(FailKind::UnknownId, "nothing open with that number")
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::Nothing;
    use crate::fake::Fake;
    use noslacking_video_ipc::{Codec, Planes};

    /// Runs `requests` through a server with `backend` and returns its
    /// replies, each with the sequence number it carried.
    fn talk(backend: &mut dyn Backend, requests: &[Vec<u8>]) -> (Vec<(u32, Reply)>, bool) {
        let mut input = Vec::new();
        for (seq, body) in requests.iter().enumerate() {
            ipc::write_frame(&mut input, 10 + seq as u32, body).expect("written");
        }
        let mut output = Vec::new();
        let ok = serve(&mut input.as_slice(), &mut output, backend).is_ok();
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
        assert!(matches!(&replies[3], Reply::Picture(p) if (p.width, p.height) == (64, 48)));
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

    #[test]
    fn without_hardware_everything_is_unsupported() {
        let (replies, ok) = talk(
            &mut Nothing::new("none: no libva"),
            &[
                hello(),
                Request::OpenDecoder {
                    codec: Codec::H264,
                    width: 1920,
                    height: 1080,
                }
                .encode(),
            ],
        );
        assert!(ok);
        assert!(matches!(
            &replies[0].1,
            Reply::Welcome { capabilities, backend, .. }
                if capabilities.is_empty() && backend == "none: no libva"
        ));
        assert_eq!(kind(&replies[1].1), Some(FailKind::Unsupported));
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
        assert!(serve(&mut input.as_slice(), &mut output, &mut Fake).is_err());
    }
}
