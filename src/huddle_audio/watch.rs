//! Video inside a live session: it reads what Chime's signaling says of
//! video (INDEX, PAUSE, RESUME, BITRATES, DATA_MESSAGE,
//! REMOTE_VIDEO_UPDATE), receives the streams wanted on `recvonly`
//! m-lines added by renegotiation, counts what arrives on each and asks
//! for keyframes. What is wanted: while the call window is open, the
//! share it shows and the cameras its tiles show (with `huddle-video`,
//! whose frames go on to the decoder threads, `screen` and `gallery`),
//! and for the probe or `--video N` up to N streams, which can also be
//! dumped. The choices are [`super::video`]'s and [`super::cameras`]'s;
//! this is the part that touches `str0m`.
//!
//! A probe run or `--video` logs everything at info level; an app session
//! that only watches shares logs what changes and keeps the rest at debug
//! level.

use std::collections::{BTreeMap, HashMap};
use std::fs::File;
use std::io::BufWriter;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use str0m::Rtc;
use str0m::change::SdpPendingOffer;
use str0m::media::{Direction, KeyframeRequestKind, MediaData, MediaKind, Mid};

use super::bitstream::{Dump, DumpKind};
use super::cameras::{self, Camera, Debounce, Pauses, Wish};
use super::chime::{self, FrameType};
#[cfg(feature = "huddle-video")]
use super::gallery::{CameraDecoding, Gallery};
#[cfg(feature = "huddle-video")]
use super::screen::{Decoding, Screen};
use super::sdp::{self, Mids};
use super::video::{
    self, Index, Noted, Options, Plan, Resubscribe, Share, Slots, StreamStats, Summary,
};

/// Before the first re-SUBSCRIBE, audio gets this long to settle, so its
/// flow before and during can be compared.
const SETTLE: Duration = Duration::from_secs(2);
/// At most one re-SUBSCRIBE this often, however busy INDEX is.
const RESUBSCRIBE_EVERY: Duration = Duration::from_secs(3);
/// A re-SUBSCRIBE with no answer this long is given up.
pub const ANSWER_WAIT: Duration = Duration::from_secs(10);
/// Audio is counted this long past each answer.
const AUDIO_WINDOW: Duration = Duration::from_secs(2);
/// At most one keyframe request a second per stream.
const PLI_EVERY: Duration = Duration::from_secs(1);
/// While a keyframe request waits for its stream to start, it is tried
/// again this often.
const PLI_RETRY: Duration = Duration::from_millis(250);
/// INDEX changes are logged at most this often.
const INDEX_EVERY: Duration = Duration::from_secs(2);
/// BITRATES is summed up at most this often.
const BITRATES_EVERY: Duration = Duration::from_secs(20);
/// Each stream's counts, this often.
const STATS_EVERY: Duration = Duration::from_secs(5);

/// A new offer to send with SUBSCRIBE.
pub struct Reoffer {
    /// `str0m`'s offer, as it wrote it.
    pub offer: String,
    /// To accept the answer with.
    pub pending: SdpPendingOffer,
    /// SUBSCRIBE's `receive_stream_ids` for it.
    pub receive_stream_ids: Vec<u32>,
}

/// A re-SUBSCRIBE on its way.
struct InFlight {
    plan: Plan,
    /// The mids of the m-lines it adds, in order.
    added: Vec<Mid>,
    sent: Instant,
    audit: Resubscribe,
    audio_at_send: u64,
    answered: Option<Instant>,
}

/// One receiving m-line.
struct Slot {
    mid: Mid,
    /// What arrives on it for the stream it receives now.
    stats: Option<StreamStats>,
    want_pli: bool,
    last_pli: Option<Instant>,
}

/// What the app hands a session to watch shares and cameras with.
pub struct Viewer {
    /// Told who shares their screen, whenever that changes.
    pub shares: tokio::sync::watch::Sender<Vec<Share>>,
    /// Told who has a camera on, and which have a tile, whenever that
    /// changes.
    pub cameras: tokio::sync::watch::Sender<Vec<Camera>>,
    /// What the call window wants: whether it is open, the share it
    /// shows, room for how many tiles of what size.
    pub wish: tokio::sync::watch::Receiver<Wish>,
    /// Where the watched share's pictures go.
    #[cfg(feature = "huddle-video")]
    pub screen: Screen,
    /// Where the camera tiles' pictures go.
    #[cfg(feature = "huddle-video")]
    pub gallery: Gallery,
}

impl std::fmt::Debug for Viewer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Viewer").finish_non_exhaustive()
    }
}

/// The session's side of the [`Viewer`].
struct Viewing {
    shares: tokio::sync::watch::Sender<Vec<Share>>,
    cameras: tokio::sync::watch::Sender<Vec<Camera>>,
    #[cfg(feature = "huddle-video")]
    screen: Screen,
    #[cfg(feature = "huddle-video")]
    gallery: Gallery,
    /// The decoder thread, from the first share watched.
    #[cfg(feature = "huddle-video")]
    decoding: Option<Decoding>,
    /// The cameras' decoder thread, from the first tile.
    #[cfg(feature = "huddle-video")]
    camera_decoding: Option<CameraDecoding>,
    /// The share stream whose frames are being decoded.
    decoding_stream: Option<u32>,
    /// The cameras whose frames are being decoded, and their streams.
    decoding_cameras: BTreeMap<String, u32>,
}

/// What a session follows of video.
pub struct Watch {
    options: Options,
    /// Whether this is the probe or `--video`, which log everything.
    diagnostic: bool,
    me: String,
    /// What the call window wants.
    wish: Wish,
    /// When each attendee was last heard speaking, for the tiles.
    spoke: HashMap<String, Instant>,
    /// The streams paused at their source.
    pauses: Pauses,
    /// The cameras with a tile, in tile order, as last asked for.
    shown: Vec<String>,
    /// And the stream asked for each.
    shown_streams: BTreeMap<String, u32>,
    /// Holds a new choice back until it stands still.
    debounce: Debounce,
    viewing: Option<Viewing>,
    /// Our first video m-line: the send line, slot 0, always inactive.
    send_line: Option<Mid>,
    /// The latest INDEX and the last one logged.
    index: Index,
    logged: Option<Index>,
    logged_at: Option<Instant>,
    slots: Slots,
    lines: Vec<Slot>,
    in_flight: Option<InFlight>,
    /// Re-SUBSCRIBEs whose audio is still being counted.
    counting: Vec<InFlight>,
    last_resubscribe: Option<Instant>,
    /// SSRC to stream, from the last SUBSCRIBE_ACK.
    tracks: BTreeMap<u32, u32>,
    /// Each stream's dump; `None` once it could not be opened, or for a
    /// codec not dumped.
    dumps: BTreeMap<u32, Option<OpenDump>>,
    bitrates_at: Option<Instant>,
    next_stats: Option<Instant>,
    stray: u64,
    summary: Summary,
}

impl std::fmt::Debug for Watch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Watch")
            .field("options", &self.options)
            .field("slots", &self.slots)
            .finish_non_exhaustive()
    }
}

impl Watch {
    /// Follows video for attendee `me` as `options` ask (the probe's, or
    /// `--video`'s, which log everything), and for a `viewer` if there is
    /// one. Only the viewer's half of `wish` stays with the session.
    pub fn new(options: Option<Options>, viewer: Option<Viewer>, me: &str) -> Self {
        let viewing = viewer.map(|viewer| Viewing {
            shares: viewer.shares,
            cameras: viewer.cameras,
            #[cfg(feature = "huddle-video")]
            screen: viewer.screen,
            #[cfg(feature = "huddle-video")]
            gallery: viewer.gallery,
            #[cfg(feature = "huddle-video")]
            decoding: None,
            #[cfg(feature = "huddle-video")]
            camera_decoding: None,
            decoding_stream: None,
            decoding_cameras: BTreeMap::new(),
        });
        Self {
            diagnostic: options.is_some(),
            options: options.unwrap_or_default(),
            me: me.to_owned(),
            wish: Wish::closed(),
            spoke: HashMap::new(),
            pauses: Pauses::default(),
            shown: Vec::new(),
            shown_streams: BTreeMap::new(),
            debounce: Debounce::default(),
            viewing,
            send_line: None,
            index: Index::default(),
            logged: None,
            logged_at: None,
            slots: Slots::default(),
            lines: Vec::new(),
            in_flight: None,
            counting: Vec::new(),
            last_resubscribe: None,
            tracks: BTreeMap::new(),
            dumps: BTreeMap::new(),
            bitrates_at: None,
            next_stats: None,
            stray: 0,
            summary: Summary::default(),
        }
    }

    /// Whether the offer leaves VP8 out: when asked, and always when
    /// shares are watched, as only H.264 is decoded.
    pub fn h264_only(&self) -> bool {
        self.options.h264_only || (cfg!(feature = "huddle-video") && self.viewing.is_some())
    }

    /// How loudly the details are logged: all of it for the probe and
    /// `--video`, only at debug level otherwise.
    fn level(&self) -> log::Level {
        if self.diagnostic {
            log::Level::Info
        } else {
            log::Level::Debug
        }
    }

    /// The share the call window shows, while it is open.
    fn watched(&self) -> Option<&str> {
        self.wish
            .share
            .as_deref()
            .filter(|_| self.wish.open && self.viewing.is_some())
    }

    /// The cameras on now.
    fn feeds(&self) -> Vec<cameras::Feed> {
        cameras::feeds(&self.index, &self.me, &self.pauses)
    }

    /// The camera tiles to have at `now`, in order, and the stream to
    /// receive for each; none while the window is closed.
    fn tiles(&self, now: Instant) -> Vec<(String, u32)> {
        if !self.wish.open || self.viewing.is_none() {
            return Vec::new();
        }
        let feeds = self.feeds();
        cameras::pick(&feeds, &self.shown, &self.spoke, self.wish.tiles, now)
            .into_iter()
            .filter_map(|key| {
                let feed = feeds.iter().find(|f| f.key == key)?;
                let stream = cameras::layer(feed, self.wish.tile)?;
                Some((key, stream))
            })
            .collect()
    }

    /// The tiles and every stream to receive at `now`.
    fn wanted(&self, now: Instant) -> (Vec<(String, u32)>, Vec<u32>) {
        let tiles = self.tiles(now);
        let streams: Vec<u32> = tiles.iter().map(|&(_, stream)| stream).collect();
        let wanted = video::wanted(
            &self.index,
            &self.me,
            self.options.streams,
            self.watched(),
            &streams,
        );
        (tiles, wanted)
    }

    /// Takes the call window's new wish. Closed, decoding stops at once
    /// and the tiles are forgotten; the streams go at the next
    /// re-SUBSCRIBE.
    pub fn set_wish(&mut self, wish: Wish) {
        if self.wish == wish {
            return;
        }
        if (self.wish.open, &self.wish.share) != (wish.open, &wish.share) {
            log::info!(
                "video: {}",
                match (wish.open, wish.share.as_deref()) {
                    (false, _) => "the call window closed".to_owned(),
                    (true, None) =>
                        format!("the call window is open, room for {} tiles", wish.tiles),
                    (true, Some(key)) => format!(
                        "watching the share of {}, room for {} tiles",
                        video::short(key),
                        wish.tiles
                    ),
                }
            );
        }
        if !wish.open {
            self.shown.clear();
            self.shown_streams.clear();
        }
        self.wish = wish;
        self.follow();
        self.tell_cameras();
    }

    /// Someone (by attendee id) is heard speaking at `now`.
    pub fn spoke(&mut self, attendee: &str, now: Instant) {
        if let Some(at) = self.spoke.get_mut(attendee) {
            *at = now;
        } else {
            self.spoke.insert(attendee.to_owned(), now);
        }
    }

    /// The tiles asked for are these now.
    fn commit_tiles(&mut self, tiles: Vec<(String, u32)>) {
        let keys: Vec<String> = tiles.iter().map(|(key, _)| key.clone()).collect();
        let streams: BTreeMap<String, u32> = tiles.into_iter().collect();
        if keys == self.shown && streams == self.shown_streams {
            return;
        }
        if keys != self.shown {
            log::info!(
                "video: camera tiles for {}",
                if keys.is_empty() {
                    "nobody".to_owned()
                } else {
                    keys.iter()
                        .map(|k| video::short(k))
                        .collect::<Vec<_>>()
                        .join(", ")
                }
            );
        }
        self.shown = keys;
        self.shown_streams = streams;
        self.follow();
        self.tell_cameras();
    }

    /// Asks for a keyframe on the slots receiving `streams`.
    fn want_pli(&mut self, streams: &[u32]) {
        for (slot, stream) in self.slots.streams().iter().enumerate() {
            if streams.contains(stream)
                && let Some(line) = self.lines.get_mut(slot)
            {
                line.want_pli = true;
            }
        }
    }

    /// Starts or stops decoding as the watched share's stream and the
    /// tiles' are received or not.
    fn follow(&mut self) {
        let receiving = self.slots.receiving();
        let target = self
            .watched()
            .and_then(|key| video::share_stream(&self.index, &self.me, key))
            .filter(|stream| receiving.contains(stream));
        let tiles: BTreeMap<String, u32> = if self.wish.open {
            self.shown_streams
                .iter()
                .filter(|(_, stream)| receiving.contains(stream))
                .map(|(key, &stream)| (key.clone(), stream))
                .collect()
        } else {
            BTreeMap::new()
        };
        let Some(viewing) = &mut self.viewing else {
            return;
        };
        if viewing.decoding_stream != target {
            viewing.decoding_stream = target;
            #[cfg(feature = "huddle-video")]
            {
                if target.is_some() && viewing.decoding.is_none() {
                    match Decoding::spawn(viewing.screen.clone()) {
                        Ok(decoding) => viewing.decoding = Some(decoding),
                        Err(error) => log::warn!("video: no decoder thread: {error}"),
                    }
                }
                if let Some(decoding) = &mut viewing.decoding {
                    if target.is_some() {
                        decoding.start();
                    } else {
                        decoding.stop();
                    }
                }
            }
            match target {
                Some(stream) => log::info!("video: decoding stream {stream}"),
                None => log::info!("video: decoding stopped"),
            }
        }
        if viewing.decoding_cameras == tiles {
            return;
        }
        #[cfg(feature = "huddle-video")]
        {
            if !tiles.is_empty() && viewing.camera_decoding.is_none() {
                match CameraDecoding::spawn(viewing.gallery.clone()) {
                    Ok(decoding) => viewing.camera_decoding = Some(decoding),
                    Err(error) => log::warn!("video: no camera decoder thread: {error}"),
                }
            }
            if let Some(decoding) = &mut viewing.camera_decoding {
                for (key, stream) in &viewing.decoding_cameras {
                    if tiles.get(key) != Some(stream) {
                        decoding.stop(key);
                    }
                }
                for (key, stream) in &tiles {
                    if viewing.decoding_cameras.get(key) != Some(stream) {
                        decoding.start(key);
                    }
                }
            }
        }
        log::info!(
            "video: decoding {} cameras (streams {:?})",
            tiles.len(),
            tiles.values().collect::<Vec<_>>()
        );
        viewing.decoding_cameras = tiles;
    }

    /// Tells the viewer who has a camera on and who has a tile, if that
    /// changed.
    fn tell_cameras(&self) {
        let Some(viewing) = &self.viewing else {
            return;
        };
        let tiles: &[String] = if self.wish.open { &self.shown } else { &[] };
        let now = cameras::cameras(&self.feeds(), tiles);
        viewing.cameras.send_if_modified(|cameras| {
            let changed = *cameras != now;
            if changed {
                *cameras = now;
            }
            changed
        });
    }

    /// Tells the viewer who shares, if that changed.
    fn tell_shares(&self) {
        let Some(viewing) = &self.viewing else {
            return;
        };
        let now = video::shares(&self.index, &self.me);
        viewing.shares.send_if_modified(|shares| {
            let changed = *shares != now;
            if changed {
                *shares = now;
            }
            changed
        });
    }

    /// The first offer was made, its video m-line `send_line`.
    pub fn offered(&mut self, send_line: Mid) {
        self.send_line = Some(send_line);
    }

    /// Reads what a signaling frame says of video.
    pub fn frame(&mut self, frame: &chime::Frame, now: Instant) {
        let kind = FrameType::try_from(frame.r#type).ok();
        if let Some(index) = &frame.index {
            self.index = Index::of(index);
            if self.index.sources.iter().any(|s| s.video && s.is_share()) {
                self.summary.saw_share = true;
            }
            self.summary.codecs.clone_from(&self.index.codecs);
            self.log_index(now);
            self.tell_shares();
            let resumed = self.pauses.index(&self.index);
            self.want_pli(&resumed);
            self.tell_cameras();
        }
        if let Some(pause) = &frame.pause {
            let resume = kind == Some(FrameType::Resume);
            log::log!(
                self.level(),
                "video: {}",
                video::pause_line(if resume { "RESUME" } else { "PAUSE" }, pause)
            );
            if resume {
                // A keyframe at once, so the picture comes back quickly.
                let resumed = self
                    .pauses
                    .resume(&pause.stream_ids, &pause.group_ids, &self.index);
                self.want_pli(&resumed);
            } else {
                self.pauses
                    .pause(&pause.stream_ids, &pause.group_ids, &self.index);
            }
            self.tell_cameras();
        }
        if let Some(bitrates) = &frame.bitrates
            && self.bitrates_at.is_none_or(|at| now >= at + BITRATES_EVERY)
        {
            self.bitrates_at = Some(now);
            log::log!(self.level(), "video: {}", video::bitrates_line(bitrates));
        }
        if let Some(data) = &frame.data_message {
            for line in video::data_message_lines(data) {
                log::log!(self.level(), "video: {line}");
            }
        }
        if let Some(update) = &frame.remote_video_update {
            log::log!(
                self.level(),
                "video: {}",
                video::remote_video_update_line(update)
            );
        }
        if let Some(ack) = &frame.suback {
            self.tracks = video::track_streams(&ack.tracks);
            if !ack.tracks.is_empty() {
                let tracks: Vec<String> = ack
                    .tracks
                    .iter()
                    .map(|t| {
                        format!(
                            "ssrc {} = stream {} ({:?})",
                            t.ssrc.unwrap_or_default(),
                            t.stream_id.unwrap_or_default(),
                            t.track_label.as_deref().unwrap_or("")
                        )
                    })
                    .collect();
                log::log!(
                    self.level(),
                    "video: SUBSCRIBE_ACK tracks: {}",
                    tracks.join("; ")
                );
            }
            for a in &ack.allocations {
                log::log!(
                    self.level(),
                    "video: SUBSCRIBE_ACK allocation: stream {} group {} label {:?}",
                    a.stream_id.unwrap_or_default(),
                    a.group_id.unwrap_or_default(),
                    a.track_label.as_deref().unwrap_or("")
                );
            }
        }
    }

    /// Logs INDEX when it changed in a way that matters (not each new
    /// average bitrate), at most every two seconds.
    fn log_index(&mut self, now: Instant) {
        let shape = shape(&self.index);
        if self.logged.as_ref() == Some(&shape) {
            return;
        }
        if self.logged_at.is_some_and(|at| now < at + INDEX_EVERY) {
            // Logged by `tick` once the time has come.
            return;
        }
        self.logged = Some(shape);
        self.logged_at = Some(now);
        self.summary.indexes += 1;
        let level = self.level();
        for (n, line) in self.index.lines().iter().enumerate() {
            // The heading always; each source only when asked.
            if n == 0 {
                log::info!("video: {line}");
            } else {
                log::log!(level, "video: {line}");
            }
        }
    }

    /// A new offer and SUBSCRIBE, if the streams wanted changed and one
    /// is due: `dtls_up` is when the media connection came up.
    pub fn reoffer(
        &mut self,
        rtc: &mut Rtc,
        now: Instant,
        dtls_up: Instant,
        audio_frames: u64,
    ) -> Option<Reoffer> {
        if self.in_flight.is_some() {
            return None;
        }
        let (tiles, wanted) = self.wanted(now);
        let plan = self.slots.plan(&wanted);
        if !plan.changes() {
            self.commit_tiles(tiles);
            return None;
        }
        // Noted even while its turn has not come, so a choice that stood
        // still meanwhile goes as soon as it has.
        let settled = self.debounce.settled(&wanted, now);
        if !settled
            || now < dtls_up + SETTLE
            || self
                .last_resubscribe
                .is_some_and(|at| now < at + RESUBSCRIBE_EVERY)
        {
            return None;
        }
        let mut api = rtc.sdp_api();
        for &slot in &plan.free {
            if let Some(line) = self.lines.get(slot) {
                api.set_direction(line.mid, Direction::Inactive);
            }
        }
        for &(slot, _) in &plan.reuse {
            if let Some(line) = self.lines.get(slot) {
                api.set_direction(line.mid, Direction::RecvOnly);
            }
        }
        let added: Vec<Mid> = plan
            .add
            .iter()
            .map(|_| api.add_media(MediaKind::Video, Direction::RecvOnly, None, None, None))
            .collect();
        let Some((offer, pending)) = api.apply() else {
            log::warn!("video: the change {plan:?} made no new offer");
            self.last_resubscribe = Some(now);
            return None;
        };
        let after = self.slots.after(&plan);
        let receive_stream_ids = after.receive_stream_ids();
        let n = u32::try_from(self.summary.resubscribes.len() + self.counting.len() + 1)
            .unwrap_or(u32::MAX);
        log::info!(
            "video: re-SUBSCRIBE #{n}: wanted {wanted:?}; freeing slots {:?}, reusing {:?}, \
             adding {:?}; receive_stream_ids {receive_stream_ids:?}",
            plan.free,
            plan.reuse,
            plan.add
        );
        self.last_resubscribe = Some(now);
        self.commit_tiles(tiles);
        self.in_flight = Some(InFlight {
            plan,
            added,
            sent: now,
            audit: Resubscribe {
                n,
                stream_ids: receive_stream_ids.clone(),
                ..Resubscribe::default()
            },
            audio_at_send: audio_frames,
            answered: None,
        });
        Some(Reoffer {
            offer: offer.to_sdp_string(),
            pending,
            receive_stream_ids,
        })
    }

    /// When the re-SUBSCRIBE on its way was sent, if one is.
    pub fn in_flight_since(&self) -> Option<Instant> {
        self.in_flight
            .as_ref()
            .filter(|f| f.answered.is_none())
            .map(|f| f.sent)
    }

    /// No answer came: the m-lines stay as they were.
    pub fn abandon(&mut self) {
        if let Some(flight) = self.in_flight.take() {
            log::warn!("video: {}", flight.audit.line());
            self.summary.resubscribes.push(flight.audit);
        }
    }

    /// The answer to a re-SUBSCRIBE was taken (`answer` as Chime wrote
    /// it, `mids` our offer's): the slots now are what was asked.
    pub fn answered(&mut self, answer: &str, mids: &Mids, now: Instant) {
        let Some(mut flight) = self.in_flight.take() else {
            return;
        };
        let after = self.slots.after(&flight.plan);
        // The m-lines that stop or change stream: what they counted is
        // done.
        for &slot in flight
            .plan
            .free
            .iter()
            .chain(flight.plan.reuse.iter().map(|(slot, _)| slot))
        {
            if let Some(line) = self.lines.get_mut(slot)
                && let Some(stats) = line.stats.take()
            {
                self.summary.streams.push(stats);
            }
        }
        for mid in flight.added.drain(..) {
            self.lines.push(Slot {
                mid,
                stats: None,
                want_pli: false,
                last_pli: None,
            });
        }
        let media = sdp::media_lines(answer);
        let level = self.level();
        for (slot, &stream) in after.streams().iter().enumerate() {
            let Some(line) = self.lines.get_mut(slot) else {
                continue;
            };
            let chime_mid = mids
                .position(&line.mid.to_string())
                .map_or_else(|| "?".to_owned(), |n| n.to_string());
            let answered = media.iter().find(|m| m.mid == chime_mid);
            let ssrcs: Vec<String> = answered
                .map(|m| {
                    m.ssrcs
                        .iter()
                        .map(|ssrc| match self.tracks.get(ssrc) {
                            Some(s) => format!("{ssrc} (stream {s} by tracks)"),
                            None if m.rtx.contains(ssrc) => format!("{ssrc} (RTX)"),
                            None => format!("{ssrc} (not in tracks)"),
                        })
                        .collect()
                })
                .unwrap_or_default();
            log::log!(
                level,
                "video: slot {} = mid {chime_mid}: stream {stream}, answered {}, ssrcs [{}]",
                slot + 1,
                answered.map_or("nothing", |m| m.direction.as_str()),
                ssrcs.join(", ")
            );
            if stream != 0 && line.stats.is_none() {
                let source = self.index.sources.iter().find(|s| s.stream_id == stream);
                line.stats = Some(StreamStats {
                    stream_id: stream,
                    attendee: source.map_or_else(|| "?".into(), |s| video::short(&s.attendee_id)),
                    user: source.and_then(|s| s.user.clone()),
                    share: source.is_some_and(video::Source::is_share),
                    mid: chime_mid,
                    ..StreamStats::default()
                });
                // Ask for a keyframe as soon as the stream has started.
                line.want_pli = true;
            }
        }
        self.slots = after;
        self.follow();
        let answered_ms = u64::try_from(now.duration_since(flight.sent).as_millis()).unwrap_or(0);
        flight.audit.answered_ms = Some(answered_ms);
        flight.answered = Some(now);
        log::info!("video: {}", flight.audit.line());
        self.counting.push(flight);
    }

    /// A frame on a video m-line.
    pub fn media(&mut self, rtc: &mut Rtc, data: &MediaData, now: Instant) {
        let Some(slot) = self.lines.iter().position(|l| l.mid == data.mid) else {
            self.stray += 1;
            if self.stray == 1 {
                log::info!(
                    "video: a frame on mid {} (our send line or no slot): ignored",
                    data.mid
                );
            }
            return;
        };
        let codec = data.params.spec().codec.to_string();
        let format = data.params.spec().format;
        let pt = *data.pt;
        let ssrc = rtc
            .direct_api()
            .stream_rx_by_mid(data.mid, data.rid)
            .map(|s| *s.ssrc());
        let line = &mut self.lines[slot];
        let Some(stats) = &mut line.stats else {
            self.stray += 1;
            return;
        };
        let noted = stats.frame(&codec, pt, &data.data, data.is_keyframe(), data.contiguous);
        // What arrived first is worth the log; the rest only when asked.
        let level = if self.diagnostic {
            log::Level::Info
        } else {
            log::Level::Debug
        };
        for note in noted {
            match note {
                Noted::First => log::info!(
                    "video: stream {} on mid {}: first frame: pt {pt} {codec} (profile-level-id \
                     {}, packetization-mode {}), {} bytes, keyframe {}, ssrc {} ({})",
                    stats.stream_id,
                    stats.mid,
                    format
                        .profile_level_id
                        .map_or_else(|| "-".to_owned(), |p| format!("{p:06x}")),
                    format
                        .packetization_mode
                        .map_or_else(|| "-".to_owned(), |p| p.to_string()),
                    data.data.len(),
                    data.is_keyframe(),
                    ssrc.map_or_else(|| "?".to_owned(), |s| s.to_string()),
                    ssrc.and_then(|s| self.tracks.get(&s)).map_or_else(
                        || "not in SUBSCRIBE_ACK tracks".to_owned(),
                        |s| format!("stream {s} by tracks")
                    ),
                ),
                Noted::FirstKeyframe(said) => log::info!(
                    "video: stream {} on mid {}: first keyframe (frame {}): {said}",
                    stats.stream_id,
                    stats.mid,
                    stats.frames
                ),
                Noted::Sps(said) => log::log!(
                    level,
                    "video: stream {} on mid {}: {said}",
                    stats.stream_id,
                    stats.mid
                ),
            }
        }
        if !data.contiguous {
            line.want_pli = true;
        }
        let stream = stats.stream_id;
        #[cfg(feature = "huddle-video")]
        if let Some(viewing) = &mut self.viewing {
            if viewing.decoding_stream == Some(stream)
                && let Some(decoding) = &mut viewing.decoding
            {
                decoding.push(data.data.to_vec(), data.contiguous);
                if viewing.screen.take_keyframe_wish() {
                    line.want_pli = true;
                }
            } else if let Some((key, _)) =
                viewing.decoding_cameras.iter().find(|&(_, &s)| s == stream)
                && let Some(decoding) = &mut viewing.camera_decoding
            {
                decoding.push(key, data.data.to_vec(), data.contiguous);
                if viewing.gallery.take_keyframe_wish(key) {
                    line.want_pli = true;
                }
            }
        }
        let attendee = stats.attendee.clone();
        if let Some(dir) = &self.options.dump {
            dump(
                &mut self.dumps,
                dir,
                stream,
                &attendee,
                &codec,
                &data.data,
                data.time.numer(),
            );
        }
        self.request_keyframes(rtc, now);
    }

    /// Asks for the keyframes wanted and due.
    fn request_keyframes(&mut self, rtc: &mut Rtc, now: Instant) {
        let level = self.level();
        for line in &mut self.lines {
            if !line.want_pli || line.last_pli.is_some_and(|at| now < at + PLI_EVERY) {
                continue;
            }
            let Some(mut writer) = rtc.writer(line.mid) else {
                continue;
            };
            // Fails until the stream's first packet came; tried again.
            if writer
                .request_keyframe(None, KeyframeRequestKind::Pli)
                .is_ok()
            {
                line.want_pli = false;
                line.last_pli = Some(now);
                if let Some(stats) = &mut line.stats {
                    stats.plis += 1;
                    log::log!(
                        level,
                        "video: stream {} on mid {}: PLI sent ({} so far)",
                        stats.stream_id,
                        stats.mid,
                        stats.plis
                    );
                }
            }
        }
    }

    /// What is due now: keyframe requests, audio counts, INDEX and
    /// stream logs.
    pub fn tick(&mut self, rtc: Option<&mut Rtc>, now: Instant, audio_frames: u64) {
        if let Some(rtc) = rtc {
            self.request_keyframes(rtc, now);
        }
        let (done, counting): (Vec<_>, Vec<_>) = std::mem::take(&mut self.counting)
            .into_iter()
            .partition(|f| f.answered.is_some_and(|at| now >= at + AUDIO_WINDOW));
        self.counting = counting;
        for mut flight in done {
            flight.audit.audio_frames = Some(audio_frames.saturating_sub(flight.audio_at_send));
            flight.audit.window_ms =
                u64::try_from(now.duration_since(flight.sent).as_millis()).unwrap_or(0);
            log::info!("video: {}", flight.audit.line());
            self.summary.resubscribes.push(flight.audit);
        }
        if self.logged.as_ref() != Some(&shape(&self.index)) && !self.index.sources.is_empty() {
            self.log_index(now);
        }
        if self.next_stats.is_none_or(|at| now >= at) {
            self.next_stats = Some(now + STATS_EVERY);
            for stats in self.lines.iter().filter_map(|l| l.stats.as_ref()) {
                log::log!(self.level(), "video: {}", stats.line());
            }
        }
    }

    /// The next moment [`Self::tick`] has something to do.
    pub fn due(&self, now: Instant) -> Option<Instant> {
        let mut due: Vec<Instant> = Vec::new();
        if self.lines.iter().any(|l| l.want_pli) {
            due.push(now + PLI_RETRY);
        }
        due.extend(
            self.counting
                .iter()
                .filter_map(|f| f.answered.map(|at| at + AUDIO_WINDOW)),
        );
        if self.in_flight.is_none() {
            let (_, wanted) = self.wanted(now);
            if self.slots.plan(&wanted).changes() {
                // A change of streams waits for its turn, and until it
                // stands still.
                due.push(
                    self.last_resubscribe
                        .map_or(now + SETTLE, |at| at + RESUBSCRIBE_EVERY)
                        .max(self.debounce.ready_at(&wanted, now))
                        .max(now + Duration::from_millis(100)),
                );
            }
        }
        if let Some(at) = self.logged_at
            && self.logged.as_ref() != Some(&shape(&self.index))
        {
            due.push(at + INDEX_EVERY);
        }
        if !self.lines.is_empty()
            && let Some(at) = self.next_stats
        {
            due.push(at);
        }
        due.into_iter().min()
    }

    /// Closes the dumps and gives what was seen.
    pub fn finish(mut self) -> Summary {
        for (stream, (path, dump)) in std::mem::take(&mut self.dumps)
            .into_iter()
            .filter_map(|(stream, open)| Some((stream, open?)))
        {
            let frames = dump.frames();
            match dump.finish() {
                Ok(_) => log::info!(
                    "video: dumped {frames} frames of stream {stream} to {}",
                    path.display()
                ),
                Err(error) => log::warn!("video: dump {}: {error}", path.display()),
            }
        }
        for flight in self.counting.drain(..).chain(self.in_flight.take()) {
            self.summary.resubscribes.push(flight.audit);
        }
        self.summary
            .streams
            .extend(self.lines.into_iter().filter_map(|l| l.stats));
        if self.stray > 0 {
            log::info!("video: {} frames came on no slot", self.stray);
        }
        self.summary
    }
}

/// INDEX without what changes all the time (bitrates, frame rates), so a
/// change in it, a source come or gone, resized or paused, is worth a log
/// line.
fn shape(index: &Index) -> Index {
    let mut shape = index.clone();
    for source in &mut shape.sources {
        source.avg_bps = 0;
        source.max_kbps = 0;
        source.fps = 0;
    }
    shape
}

/// A dump being written, and where.
type OpenDump = (PathBuf, Dump<BufWriter<File>>);

/// Writes a frame to its stream's dump, opening it on the first; H.264
/// as Annex B, VP8 as IVF, anything else not at all.
fn dump(
    dumps: &mut BTreeMap<u32, Option<OpenDump>>,
    dir: &std::path::Path,
    stream: u32,
    attendee: &str,
    codec: &str,
    frame: &[u8],
    time: u64,
) {
    let open = dumps.entry(stream).or_insert_with(|| {
        let (kind, extension) = match codec {
            "H264" => (DumpKind::AnnexB, "h264"),
            "VP8" => (DumpKind::Ivf, "ivf"),
            other => {
                log::info!("video: stream {stream} is {other}, which is not dumped");
                return None;
            }
        };
        let path = dir.join(format!("{stream}-{attendee}.{extension}"));
        match File::create(&path).and_then(|file| Dump::new(BufWriter::new(file), kind)) {
            Ok(dump) => {
                log::info!("video: dumping stream {stream} to {}", path.display());
                Some((path, dump))
            }
            Err(error) => {
                log::warn!("video: cannot dump to {}: {error}", path.display());
                None
            }
        }
    });
    let Some((path, dump)) = open else {
        return;
    };
    if dump.full() {
        return;
    }
    if let Err(error) = dump.write(frame, time) {
        log::warn!("video: dump {}: {error}", path.display());
    }
    if dump.full() {
        log::info!(
            "video: dump of stream {stream} has its {} frames",
            dump.frames()
        );
    }
}
