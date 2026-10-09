//! One stream decoded by VideoToolbox. The stream's SPS and PPS make a
//! format description and a session for it; each frame's slices go in
//! as AVCC and come out as an NV12 pixel buffer, read back as I420 when
//! its picture is wanted, cropped to what the SPS shows and shrunk on
//! the processor to cover the output box (by a whole step, as the app's
//! own software path shrinks).

use noslacking_video_ipc::h264::parse_sps;
use noslacking_video_ipc::{Decoded, MAX_SIDE};

use super::avcc;
use super::planes;
use super::vt::{Decompression, PixelBuffer, Status};
use crate::backend::{Decoder, Failure};
use crate::shrink;

/// A session and the parameter sets it was made for.
struct Session {
    decompression: Decompression,
    sps: Vec<u8>,
    pps: Vec<u8>,
    /// The picture shown, from the SPS: what each buffer is cropped to.
    shown: (u32, u32),
}

/// One stream's decoder on the GPU.
pub struct VtDecoder {
    session: Option<Session>,
    /// The last SPS and PPS seen, for the next session.
    sps: Option<Vec<u8>>,
    pps: Option<Vec<u8>>,
    /// The box pictures should cover; 0×0 for their own size.
    fit: (u32, u32),
    /// The last frame's picture and the size shown of it, until taken.
    last: Option<(PixelBuffer, (u32, u32))>,
}

impl std::fmt::Debug for VtDecoder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VtDecoder")
            .field("open", &self.session.is_some())
            .field("fit", &self.fit)
            .finish_non_exhaustive()
    }
}

impl VtDecoder {
    /// A decoder waiting for its stream's parameter sets and first IDR.
    pub fn new() -> Self {
        Self {
            session: None,
            sps: None,
            pps: None,
            fit: (0, 0),
            last: None,
        }
    }

    /// The session for the parameter sets seen last, made afresh (at an
    /// IDR) when they changed and the old one cannot take them.
    fn session_for(&mut self, idr: bool) -> Result<&mut Session, Failure> {
        let (Some(sps), Some(pps)) = (&self.sps, &self.pps) else {
            return Err(Failure::need_keyframe("no parameter sets yet"));
        };
        let same = self
            .session
            .as_ref()
            .is_some_and(|s| s.sps == *sps && s.pps == *pps);
        if !same {
            if !idr {
                return Err(Failure::need_keyframe(
                    "the parameter sets changed between IDRs",
                ));
            }
            let parsed = parse_sps(sps).ok_or_else(|| Failure::broken("an unreadable SPS"))?;
            let shown = (parsed.width, parsed.height);
            if shown.0 == 0 || shown.1 == 0 || shown.0 > MAX_SIDE || shown.1 > MAX_SIDE {
                return Err(Failure::unsupported(format!(
                    "{}x{} is not a picture size",
                    shown.0, shown.1
                )));
            }
            // The old session goes before the new is made. Streams
            // change their parameter sets rarely (a new size), so a
            // session is not kept across them.
            self.session = None;
            let decompression =
                Decompression::new(sps, pps, true).map_err(|status| match status {
                    // What the GPU does not decode, software does.
                    Status::NO_DECODER | Status::UNSUPPORTED_FORMAT => {
                        Failure::unsupported(format!("videotoolbox: {status}"))
                    }
                    _ => Failure::device(format!("videotoolbox: {status}")),
                })?;
            self.session = Some(Session {
                decompression,
                sps: sps.clone(),
                pps: pps.clone(),
                shown,
            });
        }
        self.session
            .as_mut()
            .ok_or_else(|| Failure::device("no session"))
    }
}

impl Default for VtDecoder {
    fn default() -> Self {
        Self::new()
    }
}

impl Decoder for VtDecoder {
    fn decode_frame(&mut self, frame: &[u8], _keyframe: bool) -> Result<bool, Failure> {
        self.last = None;
        let unit = avcc::split(frame);
        if let Some(sps) = unit.sps {
            self.sps = Some(sps.to_vec());
        }
        if let Some(pps) = unit.pps {
            self.pps = Some(pps.to_vec());
        }
        if unit.avcc.is_empty() {
            return Ok(false);
        }
        if self.session.is_none() && !unit.idr {
            return Err(Failure::need_keyframe("waiting for an IDR"));
        }
        let session = self.session_for(unit.idr)?;
        let shown = session.shown;
        match session.decompression.decode(&unit.avcc) {
            Ok(Some(buffer)) => {
                self.last = Some((buffer, shown));
                Ok(true)
            }
            Ok(None) => Err(Failure::broken("videotoolbox dropped the frame")),
            Err(Status::BAD_DATA) => Err(Failure::broken("videotoolbox: bad data")),
            Err(Status::INVALID_SESSION) => {
                // The session is gone (the GPU reset, the machine slept):
                // a new one, at the next IDR.
                self.session = None;
                Err(Failure::need_keyframe("videotoolbox: the session ended"))
            }
            Err(status) => Err(Failure::device(format!("videotoolbox: {status}"))),
        }
    }

    fn picture(&mut self) -> Result<Option<Decoded>, Failure> {
        let Some((buffer, shown)) = self.last.take() else {
            return Ok(None);
        };
        // The buffer may be the coded size (whole macroblocks) or the
        // shown one: its top-left part shown either way.
        let (width, height) = buffer.size();
        let side = |v: usize| u32::try_from(v).unwrap_or(0);
        let read = (shown.0.min(side(width)), shown.1.min(side(height)));
        let planes = buffer
            .read_nv12(read.0, read.1, planes::to_i420)
            .map_err(|status| Failure::device(format!("videotoolbox: {status}")))?
            .ok_or_else(|| {
                Failure::device(format!(
                    "videotoolbox gave an unreadable {buffer:?} for {}x{}",
                    shown.0, shown.1
                ))
            })?;
        let source = (planes.width, planes.height);
        Ok(Some(Decoded {
            planes: shrink::shrink(planes, self.fit),
            source,
            hardware: true,
        }))
    }

    fn set_output_size(&mut self, width: u32, height: u32) {
        self.fit = (width, height);
    }
}
