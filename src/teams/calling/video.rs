//! The camera in a Teams call: the far end's on the `main-video` line,
//! decoded by the video helper into a [`Gallery`] tile, and ours from the
//! camera's encoder, both through the huddle's video system.
//!
//! A 1:1 call's video is only the SDP (`docs/research/teams-calls.md`
//! §D): the camera line is kept `sendrecv` for the whole call and our
//! camera sends only while it is on, so turning it on or off asks
//! nothing of the far end; it renegotiates when its own camera starts,
//! which the call answers. Like the far end's audio, its video names no
//! SSRC we could use: it is learnt from the first packet at the line's
//! H.264 payload type.
//!
//! Receiving needs `huddle-video` (the decoder) and sending
//! `huddle-camera` (the encoder); a build without either keeps the line
//! but neither shows nor sends a picture.

use std::collections::{HashSet, VecDeque};
use std::time::{Duration, Instant};

use str0m::media::{KeyframeRequestKind, MediaData, MediaKind, Mid};
use str0m::{Rtc, media::KeyframeRequest};
use tokio::sync::mpsc;

#[cfg(feature = "huddle-video")]
use crate::huddle_audio::gallery::{CameraDecoding, Gallery};

use super::media::MediaEvent;

/// The far end's camera's key in the [`Gallery`].
pub const FAR_CAMERA: &str = "far";

/// No picture from the far end for this long: its camera is off (the
/// native client stops sending without a renegotiation).
const FAR_STOPPED: Duration = Duration::from_secs(3);
/// The fewest seconds between two keyframe requests of ours.
const PLI_EVERY: Duration = Duration::from_secs(1);
/// How many of the far end's frames may wait in the decoder's queue;
/// the rest of a burst waits here. Retransmission makes bursts: frames
/// after a lost packet are held until its resend comes, then all handed
/// over at once, which would overflow the decoder's queue and cost a
/// keyframe the native client may be slow to send.
#[cfg(feature = "huddle-video")]
const FEED_AHEAD: usize = 6;
/// How many frames may wait here before they are given up on (about
/// five seconds of a phone's camera): the decoder is stuck, not behind.
const BACKLOG_MAX: usize = 75;
/// How often waiting frames are offered to the decoder again.
const FEED_EVERY: Duration = Duration::from_millis(10);
/// Keyframe requests (PLI) left unanswered before a full refresh (FIR)
/// is asked for instead.
const PLIS_BEFORE_FIR: u32 = 2;
/// How often the far end's video's numbers reach the log while it shows.
const REPORT_EVERY: Duration = Duration::from_secs(10);

/// Where a call's pictures go and come from.
#[derive(Default)]
pub struct Video {
    /// The far end's camera is shown here, under [`FAR_CAMERA`].
    #[cfg(feature = "huddle-video")]
    pub gallery: Option<Gallery>,
    /// Our camera's encoded pictures, while it is on.
    #[cfg(feature = "huddle-camera")]
    pub camera: Option<CameraFeed>,
}

impl std::fmt::Debug for Video {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Video").finish_non_exhaustive()
    }
}

/// Our camera, as the camera's task wires it.
#[cfg(feature = "huddle-camera")]
pub struct CameraFeed {
    /// Its encoded pictures.
    pub frames: mpsc::Receiver<crate::huddle_audio::camera_send::VideoFrame>,
    /// Whether it is on.
    pub on: tokio::sync::watch::Receiver<bool>,
    /// Asks the encoder for a keyframe or another bitrate.
    pub control: crate::huddle_audio::camera_send::SendControl,
}

/// What the camera side has for the session next.
pub(super) enum Input {
    #[cfg(feature = "huddle-camera")]
    Frame(crate::huddle_audio::camera_send::VideoFrame),
    #[cfg(feature = "huddle-camera")]
    On(bool),
    /// The camera's side is gone.
    #[cfg(feature = "huddle-camera")]
    Closed,
}

/// Whether a frame holds an IDR slice: a keyframe.
fn is_keyframe(unit: &[u8]) -> bool {
    use crate::huddle_audio::bitstream::{nal_type, nal_units};
    nal_units(unit).iter().any(|nal| nal_type(nal) == Some(5))
}

/// What the far end's video did over a while, for the log.
#[derive(Debug, Default)]
struct Report {
    since: Option<Instant>,
    pictures: u64,
    /// Pictures after a lost packet that was not sent again in time.
    gaps: u64,
    plis: u64,
    firs: u64,
    keyframes: u64,
    /// The most frames that waited here at once.
    held: usize,
}

/// The camera line of one call.
pub(super) struct CallVideo {
    mid: Mid,
    /// Our camera's SSRC, and its resends', which str0m must have
    /// whenever the codec has a retransmission payload type (it panics
    /// on a resend asked for without one).
    ssrc: u32,
    rtx_ssrc: Option<u32>,
    /// The line's H.264 payload type.
    pt: u8,
    /// Its retransmission's, if lost packets are asked for again.
    rtx: Option<u8>,
    /// The far end's video SSRCs str0m was told to expect.
    seen: HashSet<u32>,
    /// The far end's retransmission SSRCs seen, and the one paired with
    /// its video.
    seen_rtx: HashSet<u32>,
    paired_rtx: Option<u32>,
    /// What the far end's video did since last logged.
    report: Report,
    /// Whether the far end takes our camera, and sends its own.
    send: bool,
    receive: bool,
    tell: mpsc::UnboundedSender<MediaEvent>,
    /// When the far end's last picture came, and whether it is shown.
    last_picture: Option<Instant>,
    showing: bool,
    pictures_in: u64,
    /// A keyframe to ask for, and when the last was.
    want_pli: bool,
    last_pli: Option<Instant>,
    /// Keyframe requests sent since the far end's last keyframe.
    unanswered: u32,
    /// The far end's frames not yet handed to the decoder, and whether
    /// the next one handed over follows on from the last.
    backlog: VecDeque<Vec<u8>>,
    backlog_contiguous: bool,
    #[cfg(feature = "huddle-video")]
    decoding: Option<CameraDecoding>,
    #[cfg(feature = "huddle-video")]
    gallery: Option<Gallery>,
    #[cfg(feature = "huddle-camera")]
    camera: Option<CameraFeed>,
    /// Whether our camera is on, and whether its next picture must be a
    /// keyframe (the first since it went on).
    #[cfg(feature = "huddle-camera")]
    camera_on: bool,
    #[cfg(feature = "huddle-camera")]
    awaiting_keyframe: bool,
    #[cfg(feature = "huddle-camera")]
    pictures_out: u64,
}

impl CallVideo {
    /// The camera line `mid` with H.264 at `pt`, sending on `ssrc`;
    /// declared to `rtc` here.
    pub(super) fn new(
        rtc: &mut Rtc,
        mid: Mid,
        (pt, rtx): (u8, Option<u8>),
        ssrc: u32,
        video: Video,
        tell: mpsc::UnboundedSender<MediaEvent>,
    ) -> Self {
        let rtx_ssrc = rtx.map(|_| {
            loop {
                let candidate = rand::random::<u32>().max(1);
                if candidate != ssrc {
                    break candidate;
                }
            }
        });
        let mut api = rtc.direct_api();
        api.declare_media(mid, MediaKind::Video);
        api.declare_stream_tx(ssrc.into(), rtx_ssrc.map(Into::into), mid, None);
        #[cfg(feature = "huddle-video")]
        let decoding = video.gallery.clone().and_then(|gallery| {
            CameraDecoding::spawn(gallery)
                .map_err(|error| log::warn!("video: no decoding thread: {error}"))
                .ok()
        });
        #[cfg(feature = "huddle-camera")]
        let (camera, camera_on) = match video.camera {
            Some(mut feed) => {
                let on = *feed.on.borrow_and_update();
                (Some(feed), on)
            }
            None => (None, false),
        };
        #[cfg(not(any(feature = "huddle-video", feature = "huddle-camera")))]
        let _ = video;
        log::info!("video: camera line {mid}, H.264 at {pt}, our SSRC {ssrc}");
        Self {
            mid,
            ssrc,
            rtx_ssrc,
            pt,
            rtx,
            seen: HashSet::new(),
            seen_rtx: HashSet::new(),
            paired_rtx: None,
            report: Report::default(),
            send: true,
            receive: true,
            tell,
            last_picture: None,
            showing: false,
            pictures_in: 0,
            want_pli: false,
            last_pli: None,
            unanswered: 0,
            backlog: VecDeque::new(),
            backlog_contiguous: true,
            #[cfg(feature = "huddle-video")]
            decoding,
            #[cfg(feature = "huddle-video")]
            gallery: video.gallery,
            #[cfg(feature = "huddle-camera")]
            camera,
            #[cfg(feature = "huddle-camera")]
            camera_on,
            #[cfg(feature = "huddle-camera")]
            awaiting_keyframe: true,
            #[cfg(feature = "huddle-camera")]
            pictures_out: 0,
        }
    }

    /// Our camera's SSRC.
    pub(super) fn ssrc(&self) -> u32 {
        self.ssrc
    }

    /// The line's H.264 payload type.
    pub(super) fn pt(&self) -> u8 {
        self.pt
    }

    /// Its retransmission's payload type, if lost packets are asked for.
    pub(super) fn rtx(&self) -> Option<u8> {
        self.rtx
    }

    /// The SSRC our camera's resends go out on.
    pub(super) fn rtx_ssrc(&self) -> Option<u32> {
        self.rtx_ssrc
    }

    /// What the far end's latest description says of the line.
    pub(super) fn set_flows(&mut self, send: bool, receive: bool) {
        if send != self.send || receive != self.receive {
            log::info!(
                "video: {}sending, {}receiving",
                if send { "" } else { "not " },
                if receive { "" } else { "not " }
            );
        }
        #[cfg(feature = "huddle-camera")]
        if send && !self.send {
            self.awaiting_keyframe = true;
        }
        self.send = send;
        self.receive = receive;
        if !receive {
            self.far_stopped();
        }
    }

    /// Tells str0m to expect the far end's video on the SSRC `data`
    /// carries, if it is RTP at the line's payload type on an SSRC not
    /// seen before.
    pub(super) fn learn(&mut self, rtc: &mut Rtc, data: &[u8]) {
        if let Some(ssrc) = super::media::rtp_ssrc(data, self.pt)
            && self.seen.insert(ssrc)
        {
            log::info!("video: the far end's camera comes on SSRC {ssrc}");
            rtc.direct_api()
                .expect_stream_rx(ssrc.into(), None, self.mid, None);
            self.pair_rtx(rtc);
        }
        if let Some(rtx) = self.rtx
            && let Some(ssrc) = super::media::rtp_ssrc(data, rtx)
            && self.seen_rtx.insert(ssrc)
        {
            log::info!("video: the far end resends lost packets on SSRC {ssrc}");
            self.pair_rtx(rtc);
        }
    }

    /// Tells str0m which stream carries the resends of the far end's
    /// video, once both have come: one of each, as a 1:1 call has. The
    /// pair is set once; str0m takes it again if it changes.
    fn pair_rtx(&mut self, rtc: &mut Rtc) {
        if self.seen.len() != 1 || self.seen_rtx.len() != 1 {
            return;
        }
        let (Some(&video), Some(&rtx)) = (self.seen.iter().next(), self.seen_rtx.iter().next())
        else {
            return;
        };
        if self.paired_rtx == Some(rtx) {
            return;
        }
        self.paired_rtx = Some(rtx);
        rtc.direct_api()
            .expect_stream_rx(video.into(), Some(rtx.into()), self.mid, None);
    }

    /// Whether `mid` is this line's.
    pub(super) fn is(&self, mid: Mid) -> bool {
        mid == self.mid
    }

    /// One picture from the far end, as str0m put it together.
    pub(super) fn data(&mut self, data: &MediaData, now: Instant) {
        if !self.receive {
            return;
        }
        self.pictures_in += 1;
        if self.pictures_in == 1 {
            log::info!(
                "video: the far end's first picture, {} bytes",
                data.data.len()
            );
        }
        self.last_picture = Some(now);
        if !self.showing {
            self.showing = true;
            #[cfg(feature = "huddle-video")]
            if let Some(decoding) = &mut self.decoding {
                decoding.start(FAR_CAMERA);
            }
            let _ = self.tell.send(MediaEvent::FarVideo(true));
            self.want_pli = true;
        }
        self.report.pictures += 1;
        if !data.contiguous {
            self.want_pli = true;
            self.report.gaps += 1;
            self.backlog_contiguous = false;
        }
        if is_keyframe(&data.data) {
            self.report.keyframes += 1;
            if self.unanswered > 0 {
                log::info!(
                    "video: the far end sent a keyframe after {} requests",
                    self.unanswered
                );
            }
            self.unanswered = 0;
        }
        #[cfg(feature = "huddle-video")]
        if self
            .gallery
            .as_ref()
            .is_some_and(|g| g.take_keyframe_wish(FAR_CAMERA))
        {
            self.want_pli = true;
        }
        self.backlog.push_back(data.data.to_vec());
        if self.backlog.len() > BACKLOG_MAX {
            log::info!(
                "video: {} of the far end's frames waited for the decoder; dropped, a keyframe asked for",
                self.backlog.len()
            );
            self.backlog.clear();
            self.backlog_contiguous = false;
            self.want_pli = true;
        }
        self.report.held = self.report.held.max(self.backlog.len());
        self.feed();
    }

    /// Hands waiting frames to the decoder while its queue has room.
    fn feed(&mut self) {
        #[cfg(feature = "huddle-video")]
        if let Some(decoding) = &mut self.decoding {
            while !self.backlog.is_empty() && decoding.waiting(FAR_CAMERA) < FEED_AHEAD {
                let Some(unit) = self.backlog.pop_front() else {
                    break;
                };
                decoding.push(FAR_CAMERA, unit, self.backlog_contiguous);
                self.backlog_contiguous = true;
            }
            return;
        }
        // Nothing decodes in this build: nothing waits either.
        self.backlog.clear();
    }

    /// The far end wants a keyframe of our camera.
    pub(super) fn keyframe_request(&mut self, request: &KeyframeRequest) {
        #[cfg(feature = "huddle-camera")]
        if request.mid == self.mid
            && let Some(camera) = &self.camera
        {
            camera.control.want_keyframe();
        }
        #[cfg(not(feature = "huddle-camera"))]
        let _ = request;
    }

    /// What is due at `now`: a keyframe request, the far end's camera
    /// gone quiet.
    pub(super) fn on_time(&mut self, rtc: &mut Rtc, now: Instant) {
        if self.showing
            && self
                .last_picture
                .is_some_and(|at| now.saturating_duration_since(at) >= FAR_STOPPED)
        {
            log::info!("video: no picture from the far end for a while; its camera is off");
            self.far_stopped();
        }
        if !self.backlog.is_empty() {
            self.feed();
        }
        // A keyframe asked for twice in vain: a full refresh instead.
        let kind = if self.unanswered >= PLIS_BEFORE_FIR {
            KeyframeRequestKind::Fir
        } else {
            KeyframeRequestKind::Pli
        };
        if self.want_pli
            && self.showing
            && self.last_pli.is_none_or(|at| now >= at + PLI_EVERY)
            && let Some(mut writer) = rtc.writer(self.mid)
            // Fails until the stream's first packet came; tried again.
            && writer.request_keyframe(None, kind).is_ok()
        {
            self.want_pli = false;
            self.last_pli = Some(now);
            self.unanswered += 1;
            match kind {
                KeyframeRequestKind::Fir => self.report.firs += 1,
                _ => self.report.plis += 1,
            }
        }
        if self.showing && self.report.since.is_none_or(|at| now >= at + REPORT_EVERY) {
            if self.report.since.is_some() {
                log::info!(
                    "video: the far end's camera: {} pictures in {} s, {} keyframes, {} after \
                     a loss, keyframes asked for {} times (PLI) and {} (FIR), at most {} \
                     frames waited; resends {}",
                    self.report.pictures,
                    REPORT_EVERY.as_secs(),
                    self.report.keyframes,
                    self.report.gaps,
                    self.report.plis,
                    self.report.firs,
                    self.report.held,
                    if self.paired_rtx.is_some() {
                        "on"
                    } else {
                        "not seen"
                    },
                );
            }
            self.report = Report {
                since: Some(now),
                ..Report::default()
            };
        }
    }

    /// The next moment something is due.
    pub(super) fn deadline(&self) -> Option<Instant> {
        let quiet = self
            .last_picture
            .filter(|_| self.showing)
            .map(|at| at + FAR_STOPPED);
        let pli = (self.want_pli && self.showing)
            .then(|| self.last_pli.map_or_else(Instant::now, |at| at + PLI_EVERY));
        let report = self
            .report
            .since
            .filter(|_| self.showing)
            .map(|at| at + REPORT_EVERY);
        let feed = (!self.backlog.is_empty()).then(|| Instant::now() + FEED_EVERY);
        [quiet, pli, report, feed].into_iter().flatten().min()
    }

    fn far_stopped(&mut self) {
        if !self.showing {
            return;
        }
        self.showing = false;
        self.want_pli = false;
        self.backlog.clear();
        self.backlog_contiguous = true;
        self.unanswered = 0;
        #[cfg(feature = "huddle-video")]
        if let Some(decoding) = &mut self.decoding {
            decoding.stop(FAR_CAMERA);
        }
        let _ = self.tell.send(MediaEvent::FarVideo(false));
    }

    /// What the camera has next: never, without one.
    pub(super) async fn next(&mut self) -> Input {
        #[cfg(feature = "huddle-camera")]
        if let Some(camera) = &mut self.camera {
            return tokio::select! {
                frame = camera.frames.recv() => frame.map_or(Input::Closed, Input::Frame),
                changed = camera.on.changed() => match changed {
                    Ok(()) => Input::On(*camera.on.borrow_and_update()),
                    Err(_) => Input::Closed,
                },
            };
        }
        std::future::pending().await
    }

    /// Takes what [`Self::next`] had, sending a picture while `connected`.
    pub(super) fn input(&mut self, rtc: &mut Rtc, input: Input, connected: bool) {
        #[cfg(not(feature = "huddle-camera"))]
        let _ = (rtc, connected);
        match input {
            #[cfg(feature = "huddle-camera")]
            Input::Frame(frame) => self.send_frame(rtc, frame, connected),
            #[cfg(feature = "huddle-camera")]
            Input::On(on) => {
                log::info!("video: our camera is {}", if on { "on" } else { "off" });
                self.camera_on = on;
                if on {
                    self.awaiting_keyframe = true;
                    if let Some(camera) = &self.camera {
                        camera.control.want_keyframe();
                    }
                }
            }
            #[cfg(feature = "huddle-camera")]
            Input::Closed => {
                self.camera = None;
                self.camera_on = false;
            }
        }
    }

    /// Sends one picture of our camera, from a keyframe on.
    #[cfg(feature = "huddle-camera")]
    fn send_frame(
        &mut self,
        rtc: &mut Rtc,
        frame: crate::huddle_audio::camera_send::VideoFrame,
        connected: bool,
    ) {
        use str0m::media::{Frequency, MediaTime, Pt};

        if !connected || !self.send || !self.camera_on {
            return;
        }
        if self.awaiting_keyframe {
            if !frame.keyframe {
                if let Some(camera) = &self.camera {
                    camera.control.want_keyframe();
                }
                return;
            }
            self.awaiting_keyframe = false;
        }
        let Some(writer) = rtc.writer(self.mid) else {
            return;
        };
        let time = MediaTime::new(frame.time, Frequency::NINETY_KHZ);
        match writer.write(Pt::from(self.pt), frame.at, time, frame.data) {
            Ok(()) => {
                self.pictures_out += 1;
                if self.pictures_out == 1 {
                    log::info!("video: our camera's first picture sent");
                }
            }
            Err(error) => log::debug!("video: could not send a picture: {error}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_keyframe_is_a_frame_with_an_idr_slice() {
        // SPS, PPS and an IDR slice, as a keyframe comes.
        let keyframe = [
            0, 0, 0, 1, 0x67, 0x42, 0, 0, 0, 1, 0x68, 0xce, 0, 0, 0, 1, 0x65, 0x88,
        ];
        assert!(is_keyframe(&keyframe));
        // A slice of a picture that refers to others.
        assert!(!is_keyframe(&[0, 0, 0, 1, 0x41, 0x9a]));
        assert!(!is_keyframe(&[]));
    }
}
