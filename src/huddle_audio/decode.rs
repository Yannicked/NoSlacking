//! A video stream's H.264 turned into pictures (the `huddle-video`
//! feature): decoded in the helper process ([`super::helper`]), on the
//! GPU or in software, at no more pixels than the window shows; then
//! turned from I420 into the RGBA egui draws.
//!
//! No decoder runs in the app: decoders read strangers' network data,
//! and a panic in a release build aborts the whole app, where in the
//! helper it costs only the helper, which is started again. Without a
//! helper there is no video ([`Trouble::NoHelper`]).
//!
//! Each new start (a keyframe after waiting: the start, a loss, an
//! error) opens a fresh decoder in the helper, on the GPU when Settings
//! → Huddles allows it. The helper decodes in software whatever the GPU
//! cannot; a stream it moved to software, or whose helper failed, asks
//! for software from then on. When the helper fails, the stream waits
//! for a keyframe (the session asks for one) and starts again in the
//! helper started anew.

use egui::{Color32, ColorImage};

use super::bitstream;
use super::helper::{self, Helper, HelperTrouble, Lane, RemoteDecoder};

pub use super::helper::{Outcome, Picture};

/// Why a frame gave no picture.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum Trouble {
    /// Frames were lost, or the stream has just started: nothing decodes
    /// until a keyframe, which the sender should be asked for.
    #[error("waiting for a keyframe")]
    NeedKeyframe,
    /// The frame did not decode; the decoder starts over at the next
    /// keyframe.
    #[error("the frame did not decode: {0}")]
    Broken(String),
    /// The picture could not be turned into RGBA.
    #[error("the picture could not be converted: {0}")]
    Convert(String),
    /// The video helper is missing, of another version, or failed too
    /// often: no video until the app starts again.
    #[error("no video helper")]
    NoHelper,
}

/// One decoded picture, in I420: a full-size luma plane and two chroma
/// planes of half its width and height (rounded up).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Yuv {
    /// Its width in pixels.
    pub width: usize,
    /// Its height in pixels.
    pub height: usize,
    /// Luma, `width * height` bytes.
    pub y: Vec<u8>,
    /// Blue difference, a quarter as many.
    pub u: Vec<u8>,
    /// Red difference.
    pub v: Vec<u8>,
}

impl Yuv {
    /// The chroma planes' width and height.
    fn chroma(&self) -> (usize, usize) {
        (self.width.div_ceil(2), self.height.div_ceil(2))
    }

    /// Whether the planes are as long as the size says.
    fn whole(&self) -> bool {
        let (cw, ch) = self.chroma();
        self.width > 0
            && self.height > 0
            && self.y.len() == self.width * self.height
            && self.u.len() == cw * ch
            && self.v.len() == cw * ch
    }
}

/// An H.264 stream's decoder in the helper that recovers from loss: give
/// it every frame (one access unit, Annex B, as `str0m` hands them) in
/// order.
pub struct H264 {
    /// The helper; none when it is not installed.
    helper: Option<Helper>,
    /// This stream's decoder in the helper, from its last start.
    decoder: Option<RemoteDecoder>,
    /// Until a keyframe comes: at the start, after a loss or an error.
    waiting: bool,
    /// The GPU, or not, whatever the setting says (tests); none to
    /// follow Settings → Huddles at each start.
    gpu: Option<bool>,
    /// The helper moved this stream to software (the GPU cannot decode
    /// it, or failed on it), or failed while decoding it: software from
    /// here on.
    software_only: bool,
    /// The size the pictures are shown at; 0×0 until known.
    fit: (usize, usize),
    /// The size the helper was last told to shrink to.
    told: Option<(usize, usize)>,
}

impl std::fmt::Debug for H264 {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("H264")
            .field("waiting", &self.waiting)
            .field("software_only", &self.software_only)
            .finish_non_exhaustive()
    }
}

impl H264 {
    /// A decoder waiting for its first keyframe, in `lane`'s helper, on
    /// the GPU when Settings → Huddles allows it.
    pub fn new(lane: Lane) -> Self {
        Self::with_helper(helper::shared(lane), None)
    }

    /// A decoder in `helper` (none: no video), on the GPU or not as
    /// `gpu` says, or as the setting says if none.
    pub fn with_helper(helper: Option<Helper>, gpu: Option<bool>) -> Self {
        Self {
            helper,
            decoder: None,
            waiting: true,
            gpu,
            software_only: false,
            fit: (0, 0),
            told: None,
        }
    }

    /// Whether there is no helper to decode in, for good: the call window
    /// then says there is no video, rather than waiting for it.
    pub fn no_helper(&self) -> bool {
        self.helper.as_ref().is_none_or(Helper::given_up)
    }

    /// The size the pictures are shown at, in pixels: the helper shrinks
    /// them to it, so neither a copy back from the GPU nor the pipe
    /// carries more than is shown.
    pub fn set_fit(&mut self, width: usize, height: usize) {
        self.fit = (width, height);
    }

    /// Frames went missing before the next one: what follows cannot be
    /// decoded until a keyframe.
    pub fn lost(&mut self) {
        self.waiting = true;
    }

    /// Whether it waits for a keyframe.
    pub fn waiting(&self) -> bool {
        self.waiting
    }

    /// A decoder in the helper for the stream starting at keyframe
    /// `unit`.
    fn open(&self, unit: &[u8]) -> Result<RemoteDecoder, Trouble> {
        let Some(helper) = &self.helper else {
            return Err(Trouble::NoHelper);
        };
        let side = |n: u32| n.min(noslacking_video_ipc::MAX_SIDE);
        let (width, height) =
            bitstream::sps_of_frame(unit).map_or((0, 0), |sps| (side(sps.width), side(sps.height)));
        let gpu = !self.software_only && self.gpu.unwrap_or_else(helper::gpu);
        match helper.open_decoder(noslacking_video_ipc::Codec::H264, width, height, gpu) {
            Ok(decoder) => {
                log::debug!(
                    "video: {width}x{height} decodes in the helper{}",
                    if gpu {
                        ", on the GPU if it can"
                    } else {
                        ", in software"
                    }
                );
                Ok(decoder)
            }
            Err(_) if helper.given_up() => Err(Trouble::NoHelper),
            Err(lost) => Err(Trouble::Broken(lost.to_string())),
        }
    }

    /// Tells the helper the size to shrink to, if it changed.
    fn tell_fit(&mut self) -> Result<(), HelperTrouble> {
        let Some(decoder) = &mut self.decoder else {
            return Ok(());
        };
        if self.told == Some(self.fit) {
            return Ok(());
        }
        let side = |n: usize| {
            u32::try_from(n)
                .unwrap_or(u32::MAX)
                .min(noslacking_video_ipc::MAX_SIDE)
        };
        decoder.set_output_size(side(self.fit.0), side(self.fit.1))?;
        self.told = Some(self.fit);
        Ok(())
    }

    /// Decodes one frame: its picture if `show`, else the helper keeps
    /// it back for [`Self::fetch`] ([`Outcome::Kept`]); or none for a
    /// frame that carries only parameter sets. Every frame must be given,
    /// shown or not: the ones after it refer to it.
    pub fn decode(&mut self, unit: &[u8], show: bool) -> Result<Outcome, Trouble> {
        let keyframe = is_keyframe(unit);
        if self.waiting {
            if !keyframe {
                return Err(Trouble::NeedKeyframe);
            }
            // A decoder that failed once fails on: a new one for the new
            // start.
            self.decoder = None;
            self.told = None;
            self.decoder = Some(self.open(unit)?);
            self.waiting = false;
        }
        let decoded = match self.tell_fit() {
            Ok(()) => match &mut self.decoder {
                Some(decoder) => decoder.decode(unit, keyframe, show),
                None => Err(HelperTrouble::Lost("no decoder".into())),
            },
            Err(trouble) => Err(trouble),
        };
        self.checked(decoded)
    }

    /// The picture the helper kept back from the last frame, at the size
    /// shown now, if no frame since replaced it: the window can take one
    /// again. Nothing while the stream waits for a keyframe.
    pub fn fetch(&mut self) -> Result<Outcome, Trouble> {
        if self.waiting || self.decoder.is_none() {
            return Ok(Outcome::Nothing);
        }
        let fetched = match self.tell_fit() {
            Ok(()) => match &mut self.decoder {
                Some(decoder) => decoder.fetch(),
                None => Ok(Outcome::Nothing),
            },
            Err(trouble) => Err(trouble),
        };
        self.checked(fetched)
    }

    /// What the helper answered, checked, and what it means for the
    /// stream.
    fn checked(&mut self, decoded: Result<Outcome, HelperTrouble>) -> Result<Outcome, Trouble> {
        match decoded {
            Ok(Outcome::Picture(picture)) if picture.yuv.whole() => {
                if !picture.gpu && !self.software_only && self.gpu.unwrap_or_else(helper::gpu) {
                    // The helper moved it to software: not the GPU again
                    // at the next start either.
                    log::info!("video: the GPU cannot decode this stream; software in the helper");
                    self.software_only = true;
                }
                Ok(Outcome::Picture(picture))
            }
            Ok(Outcome::Picture(picture)) => {
                self.waiting = true;
                Err(Trouble::Broken(format!(
                    "planes do not match {}x{}",
                    picture.yuv.width, picture.yuv.height
                )))
            }
            Ok(other) => Ok(other),
            Err(HelperTrouble::NeedKeyframe) => {
                self.waiting = true;
                Err(Trouble::NeedKeyframe)
            }
            Err(HelperTrouble::Broken(why) | HelperTrouble::Unsupported(why)) => {
                self.waiting = true;
                Err(Trouble::Broken(why))
            }
            Err(HelperTrouble::Lost(why)) => {
                // It may have been this stream that ended it: software
                // in the next helper, which starts with the next
                // keyframe.
                log::info!("video: the helper failed ({why}); a keyframe is asked for");
                self.decoder = None;
                self.waiting = true;
                self.software_only = true;
                if self.no_helper() {
                    Err(Trouble::NoHelper)
                } else {
                    Err(Trouble::NeedKeyframe)
                }
            }
        }
    }
}

/// Whether a frame holds an IDR slice, where decoding can start.
pub fn is_keyframe(unit: &[u8]) -> bool {
    bitstream::nal_units(unit)
        .iter()
        .any(|nal| bitstream::nal_type(nal) == Some(5))
}

/// The picture as egui's opaque pixels. H.264 from WebRTC senders is
/// BT.601 in studio range unless its VUI says otherwise, which Chrome's
/// screen shares do not.
pub fn to_image(yuv: &Yuv) -> Result<ColorImage, Trouble> {
    if !yuv.whole() {
        return Err(Trouble::Convert(format!(
            "planes do not match {}x{}",
            yuv.width, yuv.height
        )));
    }
    let size =
        |n: usize| u32::try_from(n).map_err(|_| Trouble::Convert(format!("{n} is too large")));
    let (cw, _) = yuv.chroma();
    let image = yuv::YuvPlanarImage {
        y_plane: &yuv.y,
        y_stride: size(yuv.width)?,
        u_plane: &yuv.u,
        u_stride: size(cw)?,
        v_plane: &yuv.v,
        v_stride: size(cw)?,
        width: size(yuv.width)?,
        height: size(yuv.height)?,
    };
    // Opaque: premultiplied and straight alpha are the same, so the
    // converter writes egui's own pixels.
    let mut pixels = vec![Color32::BLACK; yuv.width * yuv.height];
    yuv::yuv420_to_rgba(
        &image,
        bytemuck::cast_slice_mut(&mut pixels),
        size(yuv.width * 4)?,
        yuv::YuvRange::Limited,
        yuv::YuvStandardMatrix::Bt601,
    )
    .map_err(|e| Trouble::Convert(e.to_string()))?;
    Ok(ColorImage::new([yuv.width, yuv.height], pixels))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::huddle_audio::helper::pretend::{Act, Pretend, picture, welcome};
    use crate::sync::lock;
    use noslacking_video_ipc::{FailKind, Reply, Request};
    use sha2::{Digest, Sha256};
    use std::sync::atomic::{AtomicU32, AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    const SCREEN: &[u8] = include_bytes!("fixtures/screen-1920x1080.h264");

    /// One frame through `decoder`, its picture asked for.
    fn shown(decoder: &mut H264, unit: &[u8]) -> Result<Option<Picture>, Trouble> {
        decoder.decode(unit, true).map(Outcome::picture)
    }
    const CAMERA: &[u8] = include_bytes!("fixtures/camera-480x480.h264");

    /// Decodes every frame through the helper (its own code, on a
    /// thread, in software: no GPU in tests), hashing the pictures as
    /// ffmpeg writes them (`-f rawvideo -pix_fmt yuv420p`).
    fn decode_all(stream: &[u8]) -> (usize, (usize, usize), String) {
        let mut decoder = H264::new(Lane::Share);
        let mut hash = Sha256::new();
        let mut count = 0;
        let mut size = (0, 0);
        for frame in bitstream::access_units(stream) {
            let picture = shown(&mut decoder, &frame)
                .expect("decodes")
                .expect("a picture");
            assert_eq!(picture.source, [picture.yuv.width, picture.yuv.height]);
            assert!(!picture.gpu);
            let yuv = picture.yuv;
            size = (yuv.width, yuv.height);
            hash.update(&yuv.y);
            hash.update(&yuv.u);
            hash.update(&yuv.v);
            count += 1;
        }
        let hex = crate::text::hex(&hash.finalize());
        (count, size, hex)
    }

    /// Both fixtures (OpenH264, constrained baseline, made with ffmpeg)
    /// come back from the helper exactly as ffmpeg's own decoder makes
    /// them.
    #[test]
    fn fixtures_decode_exactly_as_ffmpeg_does() {
        assert_eq!(
            decode_all(SCREEN),
            (
                36,
                (1920, 1080),
                "a2bd2fa8a81ad980725de116dbc10da22367e82186641f63c41bc914e5eb8812".into()
            )
        );
        assert_eq!(
            decode_all(CAMERA),
            (
                66,
                (480, 480),
                "e433e34c83ae538aa67adc4fae754ca4afc8892ea91546d6d55ffc97d699e3c2".into()
            )
        );
    }

    /// A frame decoded without its picture keeps it in the helper (its
    /// own code, on a thread): fetched, it is the picture showing the
    /// frame would have given; it is fetched once, and gone after the
    /// next frame.
    #[test]
    fn pictures_kept_back_are_fetched_as_they_would_have_been_shown() {
        let frames = bitstream::access_units(CAMERA);
        let mut every = H264::new(Lane::Cameras);
        let mut kept = H264::new(Lane::Cameras);
        every.set_fit(240, 180);
        kept.set_fit(240, 180);
        assert_eq!(kept.fetch(), Ok(Outcome::Nothing), "nothing to fetch yet");
        for (n, frame) in frames.iter().take(12).enumerate() {
            let expected = shown(&mut every, frame)
                .expect("decodes")
                .expect("a picture");
            assert_eq!(kept.decode(frame, false), Ok(Outcome::Kept));
            if n % 3 == 0 {
                let fetched = kept.fetch().expect("fetches").picture();
                assert_eq!(fetched, Some(expected), "frame {n}");
                assert_eq!(kept.fetch(), Ok(Outcome::Nothing), "once only");
            }
        }
        // Lost: nothing is fetched until the stream starts again.
        kept.lost();
        assert_eq!(kept.fetch(), Ok(Outcome::Nothing));
    }

    /// The helper's answers for a picture kept back or unchanged reach
    /// the stream as they are, and change nothing about it.
    #[test]
    fn kept_and_unchanged_pictures_leave_the_stream_as_it_was() {
        let pretend = Pretend::new(|request| match request {
            Request::Hello { .. } => Act::Reply(welcome()),
            Request::OpenDecoder { .. } => Act::Reply(Reply::Opened { id: 1 }),
            Request::Decode { show: false, .. } => Act::Reply(Reply::Kept),
            Request::Decode { .. } => Act::Reply(Reply::Unchanged),
            Request::Fetch { .. } => Act::Reply(picture(32, 32, 9)),
            _ => Act::Reply(Reply::Done),
        });
        let helper = Helper::with_timeouts(
            Arc::new(pretend),
            Duration::from_secs(5),
            Duration::from_millis(500),
        );
        let frames = bitstream::access_units(CAMERA);
        let mut decoder = H264::with_helper(Some(helper), Some(true));
        assert_eq!(decoder.decode(&frames[0], false), Ok(Outcome::Kept));
        assert_eq!(decoder.decode(&frames[1], true), Ok(Outcome::Unchanged));
        assert!(!decoder.waiting());
        let fetched = decoder.fetch().expect("fetches").picture();
        assert_eq!(fetched.map(|p| p.yuv.y[0]), Some(9));
    }

    #[test]
    fn nothing_decodes_before_a_keyframe_or_after_a_loss_until_the_next() {
        let frames = bitstream::access_units(CAMERA);
        let mut decoder = H264::new(Lane::Cameras);
        // Joined mid-stream: a P frame first.
        assert_eq!(shown(&mut decoder, &frames[3]), Err(Trouble::NeedKeyframe));
        assert!(
            shown(&mut decoder, &frames[0])
                .expect("a keyframe")
                .is_some()
        );
        assert!(shown(&mut decoder, &frames[1]).expect("next").is_some());
        // A loss: frames 2… are gone.
        decoder.lost();
        assert_eq!(shown(&mut decoder, &frames[5]), Err(Trouble::NeedKeyframe));
        // The second keyframe is frame 44 (a GOP of 44).
        assert!(is_keyframe(&frames[44]) && !is_keyframe(&frames[43]));
        assert!(
            shown(&mut decoder, &frames[44])
                .expect("recovers")
                .is_some()
        );
        assert!(
            shown(&mut decoder, &frames[45])
                .expect("and goes on")
                .is_some()
        );
    }

    /// Broken frames are errors in the app, never a panic, and the next
    /// keyframe recovers; the helper's own tests break many more.
    #[test]
    fn broken_frames_are_errors_and_the_next_keyframe_recovers() {
        let frames = bitstream::access_units(CAMERA);
        let mut decoder = H264::new(Lane::Cameras);
        assert!(shown(&mut decoder, &frames[0]).expect("decodes").is_some());
        let mut noticed = false;
        for n in 1..40 {
            let mut broken = frames[n].clone();
            let end = broken.len();
            for byte in &mut broken[8..end] {
                *byte ^= 0x5a;
            }
            if let Err(Trouble::Broken(_)) = shown(&mut decoder, &broken) {
                noticed = true;
                assert_eq!(
                    shown(&mut decoder, &frames[n + 1]),
                    Err(Trouble::NeedKeyframe)
                );
                break;
            }
        }
        assert!(noticed, "some of it must have been noticed");
        assert!(
            shown(&mut decoder, &frames[44])
                .expect("recovers")
                .is_some()
        );
        // Garbage of every length, and nothing at all.
        for n in [0, 1, 4, 5, 100] {
            let mut junk = vec![0u8, 0, 0, 1, 0x65];
            junk.extend((0..n).map(|i| u8::try_from(i * 37 % 256).unwrap_or(0)));
            let _ = shown(&mut decoder, &junk);
            let _ = shown(&mut decoder, &junk[..n.min(junk.len())]);
        }
        assert!(shown(&mut decoder, &frames[0]).expect("recovers").is_some());
    }

    /// A pretend helper: its decodes give grey pictures at the size
    /// shown, from the GPU if it was asked for, but its `n`th decode
    /// (counted over every launch) does what `failing` says. Every open
    /// is noted: whether it asked for the GPU.
    fn pretend_helper(
        failing: impl Fn(usize) -> Option<Act> + Send + Sync + 'static,
    ) -> (Helper, Arc<AtomicU32>, Arc<Mutex<Vec<bool>>>) {
        let decodes = AtomicUsize::new(0);
        let fit = Mutex::new((0, 0));
        let gpu = Mutex::new(false);
        let opens = Arc::new(Mutex::new(Vec::new()));
        let noted = Arc::clone(&opens);
        let pretend = Pretend::new(move |request| match request {
            Request::Hello { .. } => Act::Reply(welcome()),
            Request::OpenDecoder { hardware, .. } => {
                *lock(&gpu) = *hardware;
                lock(&noted).push(*hardware);
                Act::Reply(Reply::Opened { id: 1 })
            }
            Request::SetOutputSize { width, height, .. } => {
                *lock(&fit) = (*width, *height);
                Act::Reply(Reply::Done)
            }
            Request::Decode { .. } => {
                let n = decodes.fetch_add(1, Ordering::Relaxed);
                let (width, height) = noslacking_video_ipc::output_size((480, 480), *lock(&fit));
                let hardware = *lock(&gpu);
                failing(n).unwrap_or_else(|| {
                    let Reply::Picture(decoded) = picture(width, height, 200) else {
                        return Act::Crash;
                    };
                    Act::Reply(Reply::Picture(noslacking_video_ipc::Decoded {
                        source: (480, 480),
                        hardware,
                        ..decoded
                    }))
                })
            }
            _ => Act::Reply(Reply::Done),
        });
        let launches = Arc::clone(&pretend.launches);
        let helper = Helper::with_timeouts(
            Arc::new(pretend),
            Duration::from_secs(5),
            Duration::from_millis(500),
        );
        (helper, launches, opens)
    }

    fn opened(opens: &Mutex<Vec<bool>>) -> Vec<bool> {
        lock(opens).clone()
    }

    #[test]
    fn a_crashed_helper_starts_again_at_the_next_keyframe_in_software() {
        let frames = bitstream::access_units(CAMERA);
        // The helper's third decode crashes it.
        let (helper, launches, opens) = pretend_helper(|n| (n == 2).then_some(Act::Crash));
        let mut decoder = H264::with_helper(Some(helper), Some(true));
        let first = shown(&mut decoder, &frames[0])
            .expect("decodes")
            .expect("a picture");
        assert!(first.gpu);
        assert_eq!(first.yuv.y[0], 200, "the helper's picture");
        assert!(shown(&mut decoder, &frames[1]).expect("decodes").is_some());
        // Lost mid-stream: a keyframe is asked for (a PLI goes out).
        assert_eq!(shown(&mut decoder, &frames[2]), Err(Trouble::NeedKeyframe));
        assert_eq!(shown(&mut decoder, &frames[3]), Err(Trouble::NeedKeyframe));
        assert!(!decoder.no_helper(), "it starts again");
        // The keyframe: a new helper, asked for software this time.
        let picture = shown(&mut decoder, &frames[44])
            .expect("decodes")
            .expect("a picture");
        assert!(!picture.gpu);
        assert_eq!(launches.load(Ordering::Relaxed), 2);
        assert_eq!(opened(&opens), [true, false]);
        assert!(shown(&mut decoder, &frames[45]).expect("goes on").is_some());
    }

    /// The helper is told the size the pictures are shown at, once and
    /// again when it changes, and they come back at it, with the
    /// stream's own size beside them.
    #[test]
    fn pictures_come_at_the_size_shown() {
        let (helper, _, _) = pretend_helper(|_| None);
        let frames = bitstream::access_units(CAMERA);
        let mut decoder = H264::with_helper(Some(helper), Some(true));
        decoder.set_fit(240, 180);
        let picture = shown(&mut decoder, &frames[0])
            .expect("decodes")
            .expect("a picture");
        assert_eq!((picture.yuv.width, picture.yuv.height), (240, 240));
        assert_eq!(picture.source, [480, 480]);
        decoder.set_fit(160, 120);
        let picture = shown(&mut decoder, &frames[1])
            .expect("decodes")
            .expect("a picture");
        assert_eq!((picture.yuv.width, picture.yuv.height), (160, 160));
        decoder.set_fit(0, 0);
        let picture = shown(&mut decoder, &frames[2])
            .expect("decodes")
            .expect("a picture");
        assert_eq!(
            (picture.yuv.width, picture.yuv.height),
            (480, 480),
            "back to its own size"
        );
        assert!(to_image(&picture.yuv).is_ok());
    }

    /// A stream the helper moved to software asks for software at its
    /// next start; with the setting off, it asks for software at once.
    #[test]
    fn a_stream_moved_to_software_stays_there() {
        let frames = bitstream::access_units(CAMERA);
        // The helper answers its first decode from software, as when the
        // GPU cannot decode the stream.
        let software = Arc::new(AtomicU32::new(0));
        let first = Arc::clone(&software);
        let (helper, launches, opens) = pretend_helper(move |n| {
            (n == 0).then(|| {
                first.fetch_add(1, Ordering::Relaxed);
                let Reply::Picture(decoded) = picture(480, 480, 9) else {
                    return Act::Crash;
                };
                Act::Reply(Reply::Picture(noslacking_video_ipc::Decoded {
                    hardware: false,
                    ..decoded
                }))
            })
        });
        let mut decoder = H264::with_helper(Some(helper.clone()), Some(true));
        let picture = shown(&mut decoder, &frames[0])
            .expect("decodes")
            .expect("one");
        assert!(!picture.gpu);
        decoder.lost();
        assert!(shown(&mut decoder, &frames[44]).expect("decodes").is_some());
        assert_eq!(opened(&opens), [true, false], "not the GPU again");
        assert_eq!(launches.load(Ordering::Relaxed), 1);
        assert_eq!(software.load(Ordering::Relaxed), 1);
        let mut off = H264::with_helper(Some(helper), Some(false));
        assert!(shown(&mut off, &frames[0]).expect("decodes").is_some());
        assert_eq!(opened(&opens), [true, false, false]);
    }

    #[test]
    fn helper_failures_become_what_the_stream_does_next() {
        let (helper, launches, _) = pretend_helper(|n| {
            let (kind, detail) = match n {
                0 => (FailKind::Broken, "bad slice"),
                _ => (FailKind::NeedKeyframe, "joined late"),
            };
            Some(Act::Reply(Reply::Failed {
                kind,
                detail: detail.into(),
            }))
        });
        let frames = bitstream::access_units(CAMERA);
        let mut decoder = H264::with_helper(Some(helper), None);
        assert!(matches!(
            shown(&mut decoder, &frames[0]),
            Err(Trouble::Broken(_))
        ));
        assert!(decoder.waiting());
        assert_eq!(shown(&mut decoder, &frames[0]), Err(Trouble::NeedKeyframe));
        assert_eq!(
            launches.load(Ordering::Relaxed),
            1,
            "not the helper's fault"
        );
    }

    #[test]
    fn without_a_helper_there_is_no_video() {
        let frames = bitstream::access_units(CAMERA);
        let mut decoder = H264::with_helper(None, None);
        assert!(decoder.no_helper(), "said at once");
        assert_eq!(shown(&mut decoder, &frames[3]), Err(Trouble::NeedKeyframe));
        assert_eq!(shown(&mut decoder, &frames[0]), Err(Trouble::NoHelper));
        // One that crashes on every frame is given up after its restarts.
        let (helper, launches, _) = pretend_helper(|_| Some(Act::Crash));
        let mut decoder = H264::with_helper(Some(helper), None);
        let mut troubles = Vec::new();
        for _ in 0..=helper::MAX_RESTARTS {
            troubles.push(shown(&mut decoder, &frames[0]));
        }
        assert!(
            troubles[..troubles.len() - 1]
                .iter()
                .all(|t| *t == Err(Trouble::NeedKeyframe)),
            "{troubles:?}"
        );
        assert_eq!(troubles.last(), Some(&Err(Trouble::NoHelper)));
        assert!(decoder.no_helper());
        assert_eq!(shown(&mut decoder, &frames[0]), Err(Trouble::NoHelper));
        assert_eq!(launches.load(Ordering::Relaxed), helper::MAX_RESTARTS + 1);
    }

    #[test]
    fn studio_range_yuv_becomes_the_colours_it_means() {
        // 4×2: black, white, mid grey and a pure red; chroma per 2×2.
        let yuv = Yuv {
            width: 4,
            height: 2,
            y: vec![16, 16, 235, 235, 16, 16, 235, 235],
            u: vec![128, 128],
            v: vec![128, 128],
        };
        let image = to_image(&yuv).expect("converts");
        assert_eq!(image.size, [4, 2]);
        let close = |a: Color32, b: [u8; 3]| {
            let got = a.to_array();
            got[3] == 255 && got.iter().zip(b).all(|(&x, y)| x.abs_diff(y) <= 2)
        };
        assert!(close(image.pixels[0], [0, 0, 0]), "{:?}", image.pixels[0]);
        assert!(
            close(image.pixels[3], [255, 255, 255]),
            "{:?}",
            image.pixels[3]
        );
        // BT.601 red: Y 81, Cb 90, Cr 240.
        let red = Yuv {
            width: 2,
            height: 2,
            y: vec![81; 4],
            u: vec![90],
            v: vec![240],
        };
        let image = to_image(&red).expect("converts");
        assert!(close(image.pixels[0], [255, 0, 0]), "{:?}", image.pixels[0]);
        // Planes that do not match the size are refused, not read past.
        let short = Yuv {
            y: vec![0; 3],
            ..red
        };
        assert!(matches!(to_image(&short), Err(Trouble::Convert(_))));
    }
}
