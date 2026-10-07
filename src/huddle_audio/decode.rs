//! A screen share's H.264 turned into pictures (the `huddle-video`
//! feature): the decoder, made safe to feed what the network brings,
//! and the step from its I420 planes to the RGBA egui draws, at no more
//! pixels than the window shows.
//!
//! The decoder is `rusty_h264`, pure Rust; why that one is in
//! docs/research/huddle-video.md (Stage 1). It never panics on a broken
//! frame, it returns an error; but once it has, it refuses every frame
//! after, even the next keyframe. So [`H264`] starts it afresh on each
//! keyframe after a loss or an error, and until that keyframe comes it
//! decodes nothing and says a keyframe is wanted.
//!
//! Each new start (a keyframe after waiting) first tries the GPU through
//! the helper process ([`super::hardware`]) when the helper is there,
//! hardware decoding is on and the helper decodes the stream's size.
//! When the helper fails, the stream goes on in software at once if the
//! frame in hand is a keyframe, and else asks for one.

use std::borrow::Cow;

use egui::{Color32, ColorImage};

use super::bitstream;
use super::hardware::{self, Helper, HwDecoder, HwTrouble};

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

/// An H.264 stream's decoder that recovers from loss: give it every
/// frame (one access unit, Annex B, as `str0m` hands them) in order.
pub struct H264 {
    decoder: rusty_h264_decoder::Decoder,
    /// Until a keyframe comes: at the start, after a loss or an error.
    waiting: bool,
    /// The helper to try the GPU through; none for software only.
    helper: Option<Helper>,
    /// This stream's decoder in the helper, while it decodes there.
    hardware: Option<HwDecoder>,
    /// The GPU cannot decode this stream (it said so, or failed on a
    /// keyframe): software from here on.
    software_only: bool,
    /// Whether Settings → Huddles turning the GPU off stops it at the
    /// next start (a decoder made by [`H264::new`]).
    follow_setting: bool,
    /// The size the pictures are shown at; 0×0 until known.
    fit: (usize, usize),
    /// The size the helper was last told to shrink to.
    told: Option<(usize, usize)>,
}

impl std::fmt::Debug for H264 {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("H264")
            .field("waiting", &self.waiting)
            .field("hardware", &self.hardware.is_some())
            .finish_non_exhaustive()
    }
}

impl Default for H264 {
    fn default() -> Self {
        Self::new()
    }
}

impl H264 {
    /// A decoder waiting for its first keyframe, which uses the GPU
    /// when it can (Settings → Huddles).
    pub fn new() -> Self {
        let helper = if hardware::enabled() {
            hardware::shared()
        } else {
            None
        };
        Self {
            follow_setting: true,
            ..Self::with_helper(helper)
        }
    }

    /// A decoder that tries the GPU through `helper` whatever the
    /// setting says, or only software with none.
    pub fn with_helper(helper: Option<Helper>) -> Self {
        Self {
            follow_setting: false,
            decoder: rusty_h264_decoder::Decoder::new(),
            waiting: true,
            helper,
            hardware: None,
            software_only: false,
            fit: (0, 0),
            told: None,
        }
    }

    /// Whether the stream decodes on the GPU at the moment.
    pub fn on_hardware(&self) -> bool {
        self.hardware.is_some()
    }

    /// A decoder in the helper for the stream starting at keyframe
    /// `unit`, if the helper decodes it.
    fn open_hardware(&mut self, unit: &[u8]) -> Option<HwDecoder> {
        if self.software_only || (self.follow_setting && !hardware::enabled()) {
            return None;
        }
        let helper = self.helper.as_ref()?;
        let sps = bitstream::sps_of_frame(unit)?;
        if !helper.decodes(noslacking_video_ipc::Codec::H264, sps.width, sps.height) {
            return None;
        }
        match helper.open_decoder(noslacking_video_ipc::Codec::H264, sps.width, sps.height) {
            Ok(decoder) => {
                log::debug!("video: {}x{} decodes on the GPU", sps.width, sps.height);
                Some(decoder)
            }
            Err(lost) => {
                log::debug!("video: no GPU decoder ({lost}): software");
                None
            }
        }
    }

    /// The size the pictures are shown at, in pixels. On the GPU they
    /// are shrunk to it there, so neither the copy back from the GPU nor
    /// the pipe carries more than is shown; in software the caller
    /// shrinks them ([`shrink`]), which then finds nothing to do for the
    /// GPU's.
    pub fn set_fit(&mut self, width: usize, height: usize) {
        self.fit = (width, height);
    }

    /// Tells the helper the size to shrink to, if it changed.
    fn tell_fit(&mut self) -> Result<(), HwTrouble> {
        let Some(hardware) = &mut self.hardware else {
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
        hardware.set_output_size(side(self.fit.0), side(self.fit.1))?;
        self.told = Some(self.fit);
        Ok(())
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

    /// Decodes one frame: a picture, or none for a frame that carries
    /// only parameter sets.
    pub fn decode(&mut self, unit: &[u8]) -> Result<Option<Yuv>, Trouble> {
        let keyframe = is_keyframe(unit);
        if self.waiting {
            if !keyframe {
                return Err(Trouble::NeedKeyframe);
            }
            // A decoder that failed once fails on: a new one for the new
            // start, on the GPU if it can.
            self.hardware = None;
            self.told = None;
            self.hardware = self.open_hardware(unit);
            if self.hardware.is_none() {
                self.decoder = rusty_h264_decoder::Decoder::new();
            }
            self.waiting = false;
        }
        let told = self.tell_fit();
        if let Some(hardware) = &mut self.hardware {
            let decoded = match told {
                Ok(()) => hardware.decode(unit, keyframe),
                Err(trouble) => Err(trouble),
            };
            let trouble = match decoded {
                Ok(Some(yuv)) if yuv.whole() => return Ok(Some(yuv)),
                Ok(Some(yuv)) => {
                    HwTrouble::Lost(format!("planes do not match {}x{}", yuv.width, yuv.height))
                }
                Ok(None) => return Ok(None),
                Err(trouble) => trouble,
            };
            let unsupported = matches!(trouble, HwTrouble::Unsupported(_));
            match trouble {
                HwTrouble::NeedKeyframe => {
                    self.waiting = true;
                    return Err(Trouble::NeedKeyframe);
                }
                HwTrouble::Broken(why) => {
                    self.waiting = true;
                    if keyframe {
                        // The GPU fails where a decoder should start:
                        // software decodes this stream from here on.
                        log::info!("video: the GPU failed on a keyframe ({why}): software");
                        self.software_only = true;
                    }
                    return Err(Trouble::Broken(why));
                }
                HwTrouble::Unsupported(why) | HwTrouble::Lost(why) => {
                    if unsupported {
                        self.software_only = true;
                    }
                    log::info!("video: the GPU decoder is gone ({why}): software");
                    self.hardware = None;
                    self.decoder = rusty_h264_decoder::Decoder::new();
                    if !keyframe {
                        // Software needs a keyframe to start on.
                        self.waiting = true;
                        return Err(Trouble::NeedKeyframe);
                    }
                    // A keyframe in hand: software takes over with it.
                }
            }
        }
        match self.decoder.decode(unit) {
            Ok(Some(frame)) => {
                let yuv = Yuv {
                    width: frame.width,
                    height: frame.height,
                    y: frame.y,
                    u: frame.u,
                    v: frame.v,
                };
                if yuv.whole() {
                    Ok(Some(yuv))
                } else {
                    self.waiting = true;
                    Err(Trouble::Broken(format!(
                        "planes do not match {}x{}",
                        yuv.width, yuv.height
                    )))
                }
            }
            Ok(None) => Ok(None),
            Err(error) => {
                self.waiting = true;
                Err(Trouble::Broken(error.to_string()))
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

/// By how much to shrink a `source`-sized picture shown at `fit`: the
/// largest whole number that still leaves it at least as large as `fit`
/// both ways, so text stays sharp and no more than about twice the
/// pixels shown each way are converted and uploaded. 1 when the window
/// is as large as the picture, or its size is not known yet (0).
pub fn reduction(source: (usize, usize), fit: (usize, usize)) -> usize {
    if fit.0 == 0 || fit.1 == 0 {
        return 1;
    }
    (source.0 / fit.0).min(source.1 / fit.1).max(1)
}

/// The picture shrunk `by` times each way, each pixel the average of the
/// block it stands for; even-sized, so its chroma halves exactly. The
/// picture itself when `by` is 1.
pub fn shrink(yuv: &Yuv, by: usize) -> Cow<'_, Yuv> {
    if by <= 1 {
        return Cow::Borrowed(yuv);
    }
    let width = ((yuv.width / by) & !1).max(2);
    let height = ((yuv.height / by) & !1).max(2);
    let (cw, ch) = yuv.chroma();
    Cow::Owned(Yuv {
        width,
        height,
        y: average(&yuv.y, yuv.width, yuv.height, width, height, by),
        u: average(&yuv.u, cw, ch, width / 2, height / 2, by),
        v: average(&yuv.v, cw, ch, width / 2, height / 2, by),
    })
}

/// A plane of `width`×`height` averaged down by `by` to `out_w`×`out_h`;
/// blocks past the edge are cut short.
fn average(
    plane: &[u8],
    width: usize,
    height: usize,
    out_w: usize,
    out_h: usize,
    by: usize,
) -> Vec<u8> {
    let mut out = Vec::with_capacity(out_w * out_h);
    let mut sums = vec![0u32; out_w];
    for row in 0..out_h {
        sums.fill(0);
        let top = row * by;
        let rows = by.min(height.saturating_sub(top));
        for line in plane.chunks_exact(width).skip(top).take(rows) {
            for (sum, block) in sums.iter_mut().zip(line.chunks(by)) {
                *sum += block.iter().map(|&p| u32::from(p)).sum::<u32>();
            }
        }
        for (col, sum) in sums.iter().enumerate() {
            let cols = by.min(width.saturating_sub(col * by));
            let count = u32::try_from((rows * cols).max(1)).unwrap_or(u32::MAX);
            out.push(u8::try_from((sum + count / 2) / count).unwrap_or(u8::MAX));
        }
    }
    out
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
    use sha2::{Digest, Sha256};

    const SCREEN: &[u8] = include_bytes!("fixtures/screen-1920x1080.h264");
    const CAMERA: &[u8] = include_bytes!("fixtures/camera-480x480.h264");

    /// A stream split into frames as `str0m` hands them: each ends after
    /// its slice, the parameter sets going with the slice they precede.
    fn frames(stream: &[u8]) -> Vec<Vec<u8>> {
        let mut frames = Vec::new();
        let mut frame = Vec::new();
        for nal in bitstream::nal_units(stream) {
            frame.extend_from_slice(&[0, 0, 0, 1]);
            frame.extend_from_slice(nal);
            if matches!(bitstream::nal_type(nal), Some(1 | 5)) {
                frames.push(std::mem::take(&mut frame));
            }
        }
        frames
    }

    /// Decodes every frame, hashing the pictures as ffmpeg writes them
    /// (`-f rawvideo -pix_fmt yuv420p`).
    fn decode_all(stream: &[u8]) -> (usize, (usize, usize), String) {
        let mut decoder = H264::new();
        let mut hash = Sha256::new();
        let mut count = 0;
        let mut size = (0, 0);
        for frame in frames(stream) {
            let yuv = decoder.decode(&frame).expect("decodes").expect("a picture");
            size = (yuv.width, yuv.height);
            hash.update(&yuv.y);
            hash.update(&yuv.u);
            hash.update(&yuv.v);
            count += 1;
        }
        let hex: String = hash.finalize().iter().map(|b| format!("{b:02x}")).collect();
        (count, size, hex)
    }

    /// Both fixtures (OpenH264, constrained baseline, made with ffmpeg)
    /// decode to exactly what ffmpeg's own decoder makes of them.
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

    #[test]
    fn nothing_decodes_before_a_keyframe_or_after_a_loss_until_the_next() {
        let frames = frames(CAMERA);
        let mut decoder = H264::new();
        // Joined mid-stream: a P frame first.
        assert_eq!(decoder.decode(&frames[3]), Err(Trouble::NeedKeyframe));
        assert!(decoder.decode(&frames[0]).expect("a keyframe").is_some());
        assert!(decoder.decode(&frames[1]).expect("next").is_some());
        // A loss: frames 2… are gone.
        decoder.lost();
        assert_eq!(decoder.decode(&frames[5]), Err(Trouble::NeedKeyframe));
        // The second keyframe is frame 44 (a GOP of 44).
        assert!(is_keyframe(&frames[44]) && !is_keyframe(&frames[43]));
        assert!(decoder.decode(&frames[44]).expect("recovers").is_some());
        assert!(decoder.decode(&frames[45]).expect("and goes on").is_some());
    }

    /// Broken frames are errors, never a panic (a panic would end the
    /// app: release builds abort), and the next keyframe recovers.
    #[test]
    fn broken_frames_are_errors_and_the_next_keyframe_recovers() {
        let frames = frames(CAMERA);
        let mut seed = 0x9e37_79b9_7f4a_7c15_u64;
        let mut random = move |n: usize| {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            usize::try_from(seed % (n.max(1) as u64)).unwrap_or(0)
        };
        let mut broken = 0;
        for round in 0..30 {
            let mut decoder = H264::new();
            for (i, frame) in frames.iter().enumerate() {
                let mut frame = frame.clone();
                if i % 7 == round % 7 {
                    match round % 3 {
                        0 => frame.truncate(random(frame.len())),
                        1 => {
                            for _ in 0..8 {
                                let at = random(frame.len());
                                frame[at] ^= 1 << random(8);
                            }
                        }
                        _ => {
                            let at = random(frame.len());
                            for byte in &mut frame[at..] {
                                *byte = u8::try_from(random(256)).unwrap_or(0);
                            }
                        }
                    }
                }
                if let Err(Trouble::Broken(_)) = decoder.decode(&frame) {
                    broken += 1;
                }
            }
        }
        assert!(broken > 0, "some of it must have been noticed");
        // Garbage of every length, and nothing at all.
        let mut decoder = H264::new();
        for n in [0, 1, 4, 5, 100] {
            let mut junk = vec![0u8, 0, 0, 1, 0x65];
            junk.extend((0..n).map(|i| u8::try_from(i * 37 % 256).unwrap_or(0)));
            let _ = decoder.decode(&junk);
            let _ = decoder.decode(&junk[..n.min(junk.len())]);
        }
        // And a clean keyframe afterwards decodes.
        let mut decoder = H264::new();
        let _ = decoder.decode(&[0, 0, 0, 1, 0x65, 0xff, 0xff]);
        assert!(decoder.decode(&frames[0]).expect("recovers").is_some());
    }

    /// A pretend helper: decodes to grey pictures, but its `n`th decode
    /// (counted over every launch) does `then`.
    fn pretend_helper(
        failing: impl Fn(usize) -> Option<super::super::hardware::pretend::Act> + Send + Sync + 'static,
    ) -> (Helper, std::sync::Arc<std::sync::atomic::AtomicU32>) {
        use super::super::hardware::pretend::{Act, Pretend, picture, welcome};
        use noslacking_video_ipc::{Reply, Request};
        let decodes = std::sync::atomic::AtomicUsize::new(0);
        let fit = std::sync::Mutex::new((0, 0));
        let pretend = Pretend::new(move |request| match request {
            Request::Hello { .. } => Act::Reply(welcome()),
            Request::OpenDecoder { .. } => Act::Reply(Reply::Opened { id: 1 }),
            Request::SetOutputSize { width, height, .. } => {
                *fit.lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner) = (*width, *height);
                Act::Reply(Reply::Done)
            }
            Request::Decode { .. } => {
                let n = decodes.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                let (width, height) = noslacking_video_ipc::output_size(
                    (480, 480),
                    *fit.lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner),
                );
                failing(n).unwrap_or_else(|| Act::Reply(picture(width, height, 200)))
            }
            _ => Act::Reply(Reply::Done),
        });
        let launches = std::sync::Arc::clone(&pretend.launches);
        let helper = Helper::with_timeouts(
            std::sync::Arc::new(pretend),
            std::time::Duration::from_secs(5),
            std::time::Duration::from_millis(500),
        );
        (helper, launches)
    }

    #[test]
    fn the_gpu_decodes_when_the_helper_can_and_a_crash_on_a_keyframe_goes_on_in_software() {
        use super::super::hardware::pretend::Act;
        let frames = frames(CAMERA);
        // The helper's third decode crashes it; the restarted helper's
        // first (the 44th frame, a keyframe) does too.
        let (helper, launches) = pretend_helper(|n| matches!(n, 2 | 3).then_some(Act::Crash));
        let mut decoder = H264::with_helper(Some(helper));
        let first = decoder
            .decode(&frames[0])
            .expect("decodes")
            .expect("a picture");
        assert!(decoder.on_hardware());
        assert_eq!(first.y[0], 200, "the helper's picture");
        assert!(decoder.decode(&frames[1]).expect("decodes").is_some());
        // Lost mid-stream: a keyframe is asked for (a PLI goes out).
        assert_eq!(decoder.decode(&frames[2]), Err(Trouble::NeedKeyframe));
        assert!(!decoder.on_hardware());
        assert_eq!(decoder.decode(&frames[3]), Err(Trouble::NeedKeyframe));
        // The keyframe: the helper starts again, crashes on it, and
        // software decodes it at once.
        let picture = decoder
            .decode(&frames[44])
            .expect("decodes")
            .expect("a picture");
        assert_eq!(launches.load(std::sync::atomic::Ordering::Relaxed), 2);
        assert!(!decoder.on_hardware());
        assert_ne!(
            picture.y[0..16],
            [200; 16],
            "the real picture, from software"
        );
        assert!(
            decoder
                .decode(&frames[45])
                .expect("software goes on")
                .is_some()
        );
    }

    /// The helper is told the size the pictures are shown at, once and
    /// again when it changes, and they come back at it: the caller's
    /// shrink then has nothing left to do.
    #[test]
    fn pictures_from_the_gpu_come_at_the_size_shown() {
        let (helper, _) = pretend_helper(|_| None);
        let frames = frames(CAMERA);
        let mut decoder = H264::with_helper(Some(helper));
        decoder.set_fit(240, 180);
        let picture = decoder
            .decode(&frames[0])
            .expect("decodes")
            .expect("a picture");
        assert_eq!((picture.width, picture.height), (240, 240));
        assert_eq!(
            reduction((240, 240), (240, 180)),
            1,
            "nothing left to shrink"
        );
        decoder.set_fit(160, 120);
        let picture = decoder
            .decode(&frames[1])
            .expect("decodes")
            .expect("a picture");
        assert_eq!((picture.width, picture.height), (160, 160));
        decoder.set_fit(0, 0);
        let picture = decoder
            .decode(&frames[2])
            .expect("decodes")
            .expect("a picture");
        assert_eq!(
            (picture.width, picture.height),
            (480, 480),
            "back to its own size"
        );
        assert!(to_image(&picture).is_ok());
    }

    #[test]
    fn a_stream_the_gpu_cannot_decode_stays_in_software() {
        use super::super::hardware::pretend::Act;
        use noslacking_video_ipc::{FailKind, Reply};
        let (helper, launches) = pretend_helper(|_| {
            Some(Act::Reply(Reply::Failed {
                kind: FailKind::Unsupported,
                detail: "B slices".into(),
            }))
        });
        let frames = frames(CAMERA);
        let mut decoder = H264::with_helper(Some(helper));
        assert!(decoder.decode(&frames[0]).expect("software").is_some());
        assert!(!decoder.on_hardware());
        decoder.lost();
        assert!(decoder.decode(&frames[44]).expect("software").is_some());
        assert!(!decoder.on_hardware(), "not tried again for this stream");
        assert_eq!(launches.load(std::sync::atomic::Ordering::Relaxed), 1);
    }

    #[test]
    fn without_a_helper_or_with_hardware_off_it_is_software() {
        let frames = frames(CAMERA);
        let mut decoder = H264::with_helper(None);
        assert!(decoder.decode(&frames[0]).expect("decodes").is_some());
        assert!(!decoder.on_hardware());
        // H264::new() in tests never finds the real helper.
        assert!(!H264::new().on_hardware());
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

    #[test]
    fn pictures_shrink_by_whole_steps_to_cover_the_window() {
        assert_eq!(reduction((1920, 1080), (0, 0)), 1);
        assert_eq!(reduction((1920, 1080), (1920, 1080)), 1);
        assert_eq!(reduction((1920, 1080), (1400, 800)), 1);
        assert_eq!(reduction((1920, 1080), (900, 500)), 2);
        assert_eq!(reduction((1920, 1080), (640, 200)), 3);
        assert_eq!(
            reduction((1920, 1080), (300, 1000)),
            1,
            "the taller side decides"
        );
        let yuv = Yuv {
            width: 6,
            height: 4,
            y: vec![
                0, 10, 20, 30, 40, 50, //
                10, 20, 30, 40, 50, 60, //
                100, 100, 100, 100, 100, 100, //
                0, 0, 0, 0, 255, 255,
            ],
            u: vec![10, 20, 30, 40, 50, 60],
            v: vec![200; 6],
        };
        assert!(matches!(shrink(&yuv, 1), Cow::Borrowed(_)));
        let half = shrink(&yuv, 2);
        assert_eq!((half.width, half.height), (2, 2));
        assert_eq!(half.y, [10, 30, 50, 50]);
        assert_eq!(half.u, [30]);
        assert_eq!(half.v, [200]);
        assert!(to_image(&half).is_ok());
    }
}
