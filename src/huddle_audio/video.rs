//! What a huddle's video looks like to us: the sources Chime's INDEX
//! lists, who shares a screen, which streams to receive (the share the
//! call window watches, and for the probe or `--video N` whatever it
//! picks), how the receiving m-lines line up with SUBSCRIBE's
//! `receive_stream_ids`, whose stream an SSRC is, and what arrives on
//! each, counted (decoding is `decode`'s, with `huddle-video`).
//!
//! The rules are the JS SDK's (see docs/research/huddle-video.md §2):
//! a source whose attendee id ends in `#content` is a screen share; a
//! group is one sender's video, its streams the simulcast layers, and the
//! one with the highest `max_bitrate_kbps` is taken
//! (`AllHighestVideoBandwidthPolicy`); our own sources never are.
//! "Subscription index 0 is reserved for transmitting camera": the first
//! video m-line is our inactive send line, and each later one receives
//! the stream at its position in `receive_stream_ids`, 0 when inactive
//! (`DefaultTransceiverController`). All of it is plain data, so it is
//! tested without a socket.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::PathBuf;

use super::bitstream::{self, Sps, Vp8Header};
use super::chime::proto;
use super::roster::slack_user;

/// The video options the app's own huddles use, from `--video` and its
/// companions at start-up; unset (no video at all) unless asked, so a
/// normal run is untouched.
static FOR_APP: std::sync::OnceLock<Options> = std::sync::OnceLock::new();

/// Makes the app's huddles watch video as `options` say, as the probe
/// does. Only the first call counts.
pub fn set_for_app(options: Options) {
    let _ = FOR_APP.set(options);
}

/// The video options for the app's huddles, if `--video` and its
/// companions asked for any.
pub fn for_app() -> Option<Options> {
    FOR_APP.get().cloned()
}

/// What the probe, or the app when asked, wants of video.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Options {
    /// How many video streams to receive; 0 only logs what Chime says.
    pub streams: usize,
    /// Offer H.264 only on the video m-lines (no VP8).
    pub h264_only: bool,
    /// Where to write each stream's first frames, if anywhere.
    pub dump: Option<PathBuf>,
}

/// A source in INDEX: one stream a sender offers.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Source {
    /// Chime's stream id, what `receive_stream_ids` names.
    pub stream_id: u32,
    /// The sender's group: one per camera or share, its streams the
    /// simulcast layers.
    pub group_id: u32,
    /// The sender's attendee id; `…#content` for a screen share.
    pub attendee_id: String,
    /// The Slack user (`U…`) from the external user id, when it has one.
    pub user: Option<String>,
    /// Whether INDEX says it is video (`media_type` 2).
    pub video: bool,
    /// Its width in pixels, as announced.
    pub width: u32,
    /// Its height.
    pub height: u32,
    /// Frames a second, as announced.
    pub fps: u32,
    /// The most it sends, in kbit/s.
    pub max_kbps: u32,
    /// What it sends on average, in bit/s.
    pub avg_bps: u32,
    /// The track label.
    pub label: String,
}

impl Source {
    /// Reads one INDEX descriptor.
    pub fn of(d: &proto::SdkStreamDescriptor) -> Self {
        Self {
            stream_id: d.stream_id.unwrap_or_default(),
            group_id: d.group_id.unwrap_or_default(),
            attendee_id: d.attendee_id.clone().unwrap_or_default(),
            user: d
                .external_user_id
                .as_deref()
                .and_then(slack_user)
                .map(str::to_owned),
            video: d.media_type == Some(proto::SdkStreamMediaType::Video as i32),
            width: d.width.unwrap_or_default(),
            height: d.height.unwrap_or_default(),
            fps: d.framerate.unwrap_or_default(),
            max_kbps: d.max_bitrate_kbps.unwrap_or_default(),
            avg_bps: d.avg_bitrate_bps.unwrap_or_default(),
            label: d.track_label.clone().unwrap_or_default(),
        }
    }

    /// Whether it is a screen share: Chime's content share joins as
    /// `<attendee>#content`.
    pub fn is_share(&self) -> bool {
        self.attendee_id.ends_with("#content")
    }

    /// Whether it is ours (our attendee, or our own share).
    pub fn is_ours(&self, me: &str) -> bool {
        self.attendee_id
            .strip_suffix("#content")
            .unwrap_or(&self.attendee_id)
            == me
    }

    /// One line for the log.
    pub fn line(&self) -> String {
        format!(
            "stream {} group {} attendee {}{} user {} {} {}x{} {} fps max {} kbps avg {} kbps label {:?}",
            self.stream_id,
            self.group_id,
            short(&self.attendee_id),
            if self.is_share() {
                " (#content: a share)"
            } else {
                ""
            },
            self.user.as_deref().unwrap_or("?"),
            if self.video { "video" } else { "audio/other" },
            self.width,
            self.height,
            self.fps,
            self.max_kbps,
            self.avg_bps / 1000,
            self.label
        )
    }
}

/// An attendee id, shortened for the log and for file names: its first
/// eight characters, and `-content` for a share.
pub fn short(attendee_id: &str) -> String {
    let base = attendee_id.strip_suffix("#content").unwrap_or(attendee_id);
    let mut out: String = base
        .chars()
        .filter(char::is_ascii_alphanumeric)
        .take(8)
        .collect();
    if attendee_id.ends_with("#content") {
        out.push_str("-content");
    }
    out
}

/// A codec in INDEX's `supported_receive_codec_intersection`, by name.
pub fn codec_name(code: i32) -> String {
    proto::SdkVideoCodecCapability::try_from(code)
        .map_or_else(|_| format!("codec {code}"), |c| c.as_str_name().to_owned())
}

/// What one INDEX said.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Index {
    /// The sources, in Chime's order.
    pub sources: Vec<Source>,
    /// Streams whose sender paused them.
    pub paused: Vec<u32>,
    /// Chime's head count.
    pub participants: Option<u32>,
    /// The codecs every receiver can take, which senders pick from.
    pub codecs: Vec<String>,
    /// Whether Chime says the meeting's video is at capacity.
    pub at_capacity: bool,
}

impl Index {
    /// Reads an INDEX frame's body.
    pub fn of(index: &proto::SdkIndexFrame) -> Self {
        Self {
            sources: index.sources.iter().map(Source::of).collect(),
            paused: index.paused_at_source_ids.clone(),
            participants: index.num_participants,
            codecs: index
                .supported_receive_codec_intersection
                .iter()
                .map(|&c| codec_name(c))
                .collect(),
            at_capacity: index.at_capacity.unwrap_or_default(),
        }
    }

    /// The lines the log gets for it: a heading, then one per source.
    pub fn lines(&self) -> Vec<String> {
        let video = self.sources.iter().filter(|s| s.video).count();
        let shares = self
            .sources
            .iter()
            .filter(|s| s.video && s.is_share())
            .count();
        let mut lines = vec![format!(
            "INDEX: {} sources ({video} video, {shares} shares), {} participants, paused {:?}, \
             at capacity {}, codec intersection [{}]",
            self.sources.len(),
            self.participants
                .map_or_else(|| "?".to_owned(), |n| n.to_string()),
            self.paused,
            self.at_capacity,
            self.codecs.join(", ")
        )];
        lines.extend(self.sources.iter().map(|s| format!("  {}", s.line())));
        lines
    }
}

/// The streams to receive, at most `n`: screen shares first, then
/// cameras, one stream per group (its highest `max_bitrate_kbps`, the
/// larger picture on a tie), never ours, in group order within each.
pub fn choose(index: &Index, me: &str, n: usize) -> Vec<u32> {
    let mut best: BTreeMap<u32, &Source> = BTreeMap::new();
    for source in index.sources.iter().filter(|s| s.video && !s.is_ours(me)) {
        let better = |old: &Source| {
            (
                source.max_kbps,
                source.width * source.height,
                u32::MAX - source.stream_id,
            ) > (
                old.max_kbps,
                old.width * old.height,
                u32::MAX - old.stream_id,
            )
        };
        match best.get(&source.group_id) {
            Some(old) if !better(old) => {}
            _ => {
                best.insert(source.group_id, source);
            }
        }
    }
    let mut chosen: Vec<&Source> = best.into_values().collect();
    // Stable: group order is kept within shares and within cameras.
    chosen.sort_by_key(|s| !s.is_share());
    chosen.into_iter().take(n).map(|s| s.stream_id).collect()
}

/// Someone sharing their screen, as the call bar and the call window
/// list them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Share {
    /// Which share: its attendee id (`…#content`), steady for as long as
    /// it lasts, while its stream ids may change.
    pub key: String,
    /// The Slack user sharing, when Chime's external id names one.
    pub user: Option<String>,
}

/// The screens shared now, others' only, one per sharer, in INDEX's
/// order.
pub fn shares(index: &Index, me: &str) -> Vec<Share> {
    let mut shares: Vec<Share> = Vec::new();
    for source in index
        .sources
        .iter()
        .filter(|s| s.video && s.is_share() && !s.is_ours(me))
    {
        if !shares.iter().any(|s| s.key == source.attendee_id) {
            shares.push(Share {
                key: source.attendee_id.clone(),
                user: source.user.clone(),
            });
        }
    }
    shares
}

/// The stream to receive for the share `key`: its best, as [`choose`]
/// picks for a group; none once it has stopped.
pub fn share_stream(index: &Index, me: &str, key: &str) -> Option<u32> {
    index
        .sources
        .iter()
        .filter(|s| s.video && s.is_share() && !s.is_ours(me) && s.attendee_id == key)
        .max_by_key(|s| (s.max_kbps, s.width * s.height, u32::MAX - s.stream_id))
        .map(|s| s.stream_id)
}

/// The streams to receive: the share the call window shows, if it is
/// still going, and with `--video N` also what [`choose`] picks. Nothing
/// is received that is not watched.
pub fn wanted(index: &Index, me: &str, diagnostic: usize, watched: Option<&str>) -> Vec<u32> {
    let mut wanted = choose(index, me, diagnostic);
    if let Some(stream) = watched.and_then(|key| share_stream(index, me, key))
        && !wanted.contains(&stream)
    {
        wanted.insert(0, stream);
    }
    wanted
}

/// The receiving video m-lines, after the first: what each receives,
/// 0 when inactive. Slot `i` here is video m-line `i + 1`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Slots {
    streams: Vec<u32>,
}

/// What a change of streams does to the m-lines.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Plan {
    /// Slots that stop receiving: set `inactive`.
    pub free: Vec<usize>,
    /// Inactive slots that receive again, and what: set `recvonly`.
    pub reuse: Vec<(usize, u32)>,
    /// New `recvonly` m-lines, in order, and what each receives.
    pub add: Vec<u32>,
}

impl Plan {
    /// Whether the SDP changes at all.
    pub fn changes(&self) -> bool {
        !(self.free.is_empty() && self.reuse.is_empty() && self.add.is_empty())
    }
}

impl Slots {
    /// What each slot receives, 0 when inactive.
    pub fn streams(&self) -> &[u32] {
        &self.streams
    }

    /// The streams received now.
    pub fn receiving(&self) -> Vec<u32> {
        self.streams.iter().copied().filter(|&s| s != 0).collect()
    }

    /// How to go from what is received to `wanted`. A stream still wanted
    /// keeps its slot. One no longer wanted frees its slot. New ones take
    /// slots that were already inactive (as the JS SDK reuses inactive
    /// transceivers), then new m-lines. A slot freed in this same change
    /// is not reused in it: its direction would not change, and `str0m`
    /// then makes no offer to carry the new stream id.
    pub fn plan(&self, wanted: &[u32]) -> Plan {
        let mut plan = Plan::default();
        for (slot, &stream) in self.streams.iter().enumerate() {
            if stream != 0 && !wanted.contains(&stream) {
                plan.free.push(slot);
            }
        }
        let mut idle = self
            .streams
            .iter()
            .enumerate()
            .filter(|&(_, &s)| s == 0)
            .map(|(slot, _)| slot);
        for &stream in wanted {
            if stream == 0 || self.streams.contains(&stream) {
                continue;
            }
            match idle.next() {
                Some(slot) => plan.reuse.push((slot, stream)),
                None => plan.add.push(stream),
            }
        }
        plan
    }

    /// The slots once `plan` is done.
    pub fn after(&self, plan: &Plan) -> Self {
        let mut streams = self.streams.clone();
        for &slot in &plan.free {
            if let Some(s) = streams.get_mut(slot) {
                *s = 0;
            }
        }
        for &(slot, stream) in &plan.reuse {
            if let Some(s) = streams.get_mut(slot) {
                *s = stream;
            }
        }
        streams.extend(&plan.add);
        Self { streams }
    }

    /// SUBSCRIBE's `receive_stream_ids`: one per video m-line in order,
    /// 0 first for our send line and for every inactive slot.
    pub fn receive_stream_ids(&self) -> Vec<u32> {
        std::iter::once(0)
            .chain(self.streams.iter().copied())
            .collect()
    }
}

/// SUBSCRIBE_ACK's `tracks`: whose stream each SSRC carries.
pub fn track_streams(tracks: &[proto::SdkTrackMapping]) -> BTreeMap<u32, u32> {
    tracks
        .iter()
        .filter_map(|t| Some((t.ssrc?, t.stream_id?)))
        .collect()
}

/// PAUSE or RESUME, for the log.
pub fn pause_line(kind: &str, pause: &proto::SdkPauseResumeFrame) -> String {
    format!(
        "{kind}: streams {:?} groups {:?}",
        pause.stream_ids, pause.group_ids
    )
}

/// BITRATES, summed up for the log.
pub fn bitrates_line(frame: &proto::SdkBitrateFrame) -> String {
    let mut line = format!("BITRATES: {} streams:", frame.bitrates.len());
    for b in &frame.bitrates {
        let _ = write!(
            line,
            " {}={}kbps",
            b.source_stream_id.unwrap_or_default(),
            b.avg_bitrate_bps.unwrap_or_default() / 1000
        );
    }
    if let Some(out) = frame.server_available_outgoing_bitrate {
        let _ = write!(line, "; server can send us {} kbps", out / 1000);
    }
    line
}

/// DATA_MESSAGE, for the log: topics, sizes and senders, never what the
/// messages say.
pub fn data_message_lines(frame: &proto::SdkDataMessageFrame) -> Vec<String> {
    frame
        .messages
        .iter()
        .map(|m| {
            format!(
                "DATA_MESSAGE: topic {:?}, {} bytes, lifetime {} ms, from {} ({})",
                m.topic.as_deref().unwrap_or(""),
                m.data.as_ref().map_or(0, Vec::len),
                m.lifetime_ms.unwrap_or_default(),
                m.sender_attendee_id
                    .as_deref()
                    .map_or_else(|| "?".into(), short),
                m.sender_external_user_id
                    .as_deref()
                    .and_then(slack_user)
                    .unwrap_or("?")
            )
        })
        .collect()
}

/// REMOTE_VIDEO_UPDATE, for the log.
pub fn remote_video_update_line(frame: &proto::SdkRemoteVideoUpdateFrame) -> String {
    let added: Vec<String> = frame
        .added_or_updated_video_subscriptions
        .iter()
        .map(|s| {
            format!(
                "mid {} stream {} group {} attendee {}",
                s.mid,
                s.stream_id.unwrap_or_default(),
                s.group_id.unwrap_or_default(),
                s.attendee_id.as_deref().map_or_else(|| "?".into(), short)
            )
        })
        .collect();
    format!(
        "REMOTE_VIDEO_UPDATE: added or updated [{}], removed mids {:?}",
        added.join("; "),
        frame.removed_video_subscription_mids
    )
}

/// What arrives on one video m-line for one stream.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct StreamStats {
    /// The stream asked for.
    pub stream_id: u32,
    /// Its sender, shortened.
    pub attendee: String,
    /// Its Slack user, if known.
    pub user: Option<String>,
    /// Whether it is a screen share.
    pub share: bool,
    /// The m-line it came on, as Chime numbers it.
    pub mid: String,
    /// The codec by name and payload type, from the first frame.
    pub codec: Option<(String, u8)>,
    /// Frames received.
    pub frames: u64,
    /// Of which keyframes.
    pub keyframes: u64,
    /// Frames after a gap (`contiguous == false`).
    pub gaps: u64,
    /// Bytes received.
    pub bytes: u64,
    /// Keyframe requests (PLI) sent.
    pub plis: u64,
    /// The first SPS seen, for H.264.
    pub sps: Option<Sps>,
    /// H.264 keyframes that came before any SPS.
    pub keyframes_without_sps: u64,
    /// The first keyframe's header, for VP8.
    pub vp8: Option<Vp8Header>,
}

/// What the log should hear of a frame beyond the counts.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Noted {
    /// The first frame: its codec.
    First,
    /// The first keyframe, and what its header said.
    FirstKeyframe(String),
    /// The first SPS, when it was not in the first frame.
    Sps(String),
}

impl StreamStats {
    /// Counts one frame. `codec` is `str0m`'s name for it ("H264", "VP8").
    pub fn frame(
        &mut self,
        codec: &str,
        pt: u8,
        data: &[u8],
        keyframe: bool,
        contiguous: bool,
    ) -> Vec<Noted> {
        let mut noted = Vec::new();
        if self.codec.is_none() {
            self.codec = Some((codec.to_owned(), pt));
            noted.push(Noted::First);
        }
        self.frames += 1;
        self.bytes += data.len() as u64;
        if !contiguous {
            self.gaps += 1;
        }
        if keyframe {
            self.keyframes += 1;
            if self.keyframes == 1 {
                let said = match codec {
                    "H264" => bitstream::sps_of_frame(data)
                        .map_or_else(|| "no SPS in it".to_owned(), |s| format!("SPS {s}")),
                    "VP8" => {
                        self.vp8 = bitstream::vp8_header(data);
                        self.vp8.map_or_else(
                            || "no VP8 keyframe header".to_owned(),
                            |h| {
                                format!(
                                    "VP8 keyframe header {} bytes, {}x{}, first partition {} bytes",
                                    h.header_bytes,
                                    h.size.map_or(0, |s| s.0),
                                    h.size.map_or(0, |s| s.1),
                                    h.first_partition
                                )
                            },
                        )
                    }
                    other => format!("{other}: not read"),
                };
                noted.push(Noted::FirstKeyframe(said));
            }
        }
        // The SPS may come apart from the first keyframe (or that
        // keyframe's first packet may have been missed): look until found.
        if codec == "H264" && self.sps.is_none() {
            self.sps = bitstream::sps_of_frame(data);
            match self.sps {
                Some(sps) if self.frames > 1 => noted.push(Noted::Sps(format!(
                    "SPS {sps}, first seen on frame {} ({} keyframes before it without one)",
                    self.frames, self.keyframes_without_sps
                ))),
                Some(_) => {}
                None if keyframe => self.keyframes_without_sps += 1,
                None => {}
            }
        }
        noted
    }

    /// The picture's size, from the first keyframe.
    pub fn resolution(&self) -> Option<(u32, u32)> {
        self.sps.map(|s| (s.width, s.height)).or_else(|| {
            self.vp8
                .and_then(|h| h.size)
                .map(|(w, h)| (u32::from(w), u32::from(h)))
        })
    }

    /// One line for the log and the summary.
    pub fn line(&self) -> String {
        let profile = self.sps.map_or_else(String::new, |s| {
            format!(" {} level {}", s.profile(), s.level_idc)
        });
        let without = match self.keyframes_without_sps {
            0 => String::new(),
            n => format!(", {n} keyframes before any SPS"),
        };
        format!(
            "stream {} ({}{}, user {}) on mid {}: {} frames, {} keyframes, {} gaps, {} PLIs sent, \
             {} kB, codec {}, {}{profile}{without}",
            self.stream_id,
            self.attendee,
            if self.share {
                ", a share"
            } else {
                ", a camera"
            },
            self.user.as_deref().unwrap_or("?"),
            self.mid,
            self.frames,
            self.keyframes,
            self.gaps,
            self.plis,
            self.bytes / 1000,
            self.codec
                .as_ref()
                .map_or_else(|| "none yet".to_owned(), |(c, pt)| format!("{c} (pt {pt})")),
            self.resolution()
                .map_or_else(|| "size unknown".to_owned(), |(w, h)| format!("{w}x{h}"))
        )
    }
}

/// How audio fared through one re-SUBSCRIBE.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Resubscribe {
    /// Which, from 1.
    pub n: u32,
    /// What it asked for (`receive_stream_ids`).
    pub stream_ids: Vec<u32>,
    /// Milliseconds from SUBSCRIBE to its answer, if one came.
    pub answered_ms: Option<u64>,
    /// Audio frames from SUBSCRIBE until two seconds after the answer.
    pub audio_frames: Option<u64>,
    /// How long that window was, in milliseconds.
    pub window_ms: u64,
}

impl Resubscribe {
    /// One line for the log and the summary.
    pub fn line(&self) -> String {
        match (self.answered_ms, self.audio_frames) {
            (Some(answered), Some(frames)) => format!(
                "re-SUBSCRIBE #{} {:?}: answered after {answered} ms; {frames} audio frames in \
                 the {} ms from SUBSCRIBE to 2 s after the answer ({})",
                self.n,
                self.stream_ids,
                self.window_ms,
                if frames > 0 {
                    "audio kept flowing"
                } else {
                    "NO AUDIO"
                }
            ),
            (Some(answered), None) => format!(
                "re-SUBSCRIBE #{} {:?}: answered after {answered} ms; audio still being counted",
                self.n, self.stream_ids
            ),
            _ => format!("re-SUBSCRIBE #{} {:?}: no answer", self.n, self.stream_ids),
        }
    }
}

/// What the probe saw of video, for its last lines.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Summary {
    /// Every stream received, in the order they started.
    pub streams: Vec<StreamStats>,
    /// Every re-SUBSCRIBE and how audio fared.
    pub resubscribes: Vec<Resubscribe>,
    /// The INDEXes that changed something, counted.
    pub indexes: u32,
    /// The last codec intersection Chime gave.
    pub codecs: Vec<String>,
    /// Whether a `#content` source was ever listed.
    pub saw_share: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn source(stream: u32, group: u32, attendee: &str, kbps: u32) -> proto::SdkStreamDescriptor {
        proto::SdkStreamDescriptor {
            stream_id: Some(stream),
            group_id: Some(group),
            attendee_id: Some(attendee.into()),
            external_user_id: Some(format!("T1-R1-U{group}")),
            media_type: Some(proto::SdkStreamMediaType::Video as i32),
            max_bitrate_kbps: Some(kbps),
            width: Some(640),
            height: Some(360),
            framerate: Some(15),
            ..Default::default()
        }
    }

    fn index(sources: Vec<proto::SdkStreamDescriptor>) -> Index {
        Index::of(&proto::SdkIndexFrame {
            sources,
            num_participants: Some(4),
            supported_receive_codec_intersection: vec![1, 3, 99],
            ..Default::default()
        })
    }

    #[test]
    fn index_sources_read_with_shares_and_users() {
        let mut share = source(9, 5, "attendee-b#content", 1500);
        share.track_label = Some("content".into());
        let mut audio = source(1, 1, "attendee-c", 40);
        audio.media_type = Some(proto::SdkStreamMediaType::Audio as i32);
        let index = index(vec![source(3, 2, "attendee-a", 600), share, audio]);
        assert_eq!(
            index.codecs,
            ["VP8", "H264_CONSTRAINED_BASELINE_PROFILE", "codec 99"]
        );
        let share = &index.sources[1];
        assert!(share.is_share() && share.video);
        assert_eq!(share.user.as_deref(), Some("U5"));
        assert!(!index.sources[2].video);
        let lines = index.lines();
        assert_eq!(lines.len(), 4);
        assert!(
            lines[0].starts_with("INDEX: 3 sources (2 video, 1 shares), 4 participants"),
            "{}",
            lines[0]
        );
        assert!(
            lines[2].contains("stream 9 group 5 attendee attendee-content (#content: a share) user U5 video 640x360 15 fps max 1500 kbps"),
            "{}",
            lines[2]
        );
        assert_eq!(short("abcdef12-3456#content"), "abcdef12-content");
    }

    #[test]
    fn choosing_takes_shares_first_one_per_group_never_ours_at_most_n() {
        let index = index(vec![
            // A camera with two simulcast layers: the higher wins.
            source(1, 1, "alice", 300),
            source(2, 1, "alice", 1200),
            source(3, 2, "bob", 600),
            // Our own camera and share are never taken.
            source(4, 3, "me", 900),
            source(5, 4, "me#content", 1500),
            // Bob's share comes first though its group is later.
            source(6, 6, "bob#content", 1000),
            source(7, 7, "carol", 500),
        ]);
        assert_eq!(choose(&index, "me", 10), [6, 2, 3, 7]);
        assert_eq!(choose(&index, "me", 2), [6, 2]);
        assert!(choose(&index, "me", 0).is_empty());
        assert!(choose(&Index::default(), "me", 4).is_empty());
    }

    /// What is received follows the call window and INDEX: nothing until
    /// a share is watched, its stream while it lasts, the other share's
    /// when the window switches, nothing once it closes or the share ends.
    #[test]
    fn only_the_watched_share_is_received() {
        let mut ana_share = source(6, 6, "ana#content", 1000);
        ana_share.external_user_id = Some("T1-R1-UANA".into());
        // A lower layer of Ana's share, and Bob's share.
        let ana_low = source(8, 6, "ana#content", 300);
        let bob_share = source(9, 7, "bob#content", 800);
        let cameras = vec![source(2, 1, "alice", 1200), source(4, 3, "me", 900)];
        let none = index(cameras.clone());
        let one = index([cameras.clone(), vec![ana_share.clone(), ana_low.clone()]].concat());
        let two = index([cameras.clone(), vec![ana_share, ana_low, bob_share]].concat());
        let mine = index([cameras, vec![source(5, 4, "me#content", 1500)]].concat());

        // Who shares: one entry per sharer, never us.
        assert!(shares(&none, "me").is_empty());
        assert_eq!(
            shares(&one, "me"),
            [Share {
                key: "ana#content".into(),
                user: Some("UANA".into())
            }]
        );
        assert_eq!(
            shares(&two, "me")
                .iter()
                .map(|s| s.key.as_str())
                .collect::<Vec<_>>(),
            ["ana#content", "bob#content"]
        );
        assert!(
            shares(&mine, "me").is_empty(),
            "our own share is not listed"
        );

        // Nobody watching: nothing, whoever shares; cameras never.
        assert!(wanted(&one, "me", 0, None).is_empty());
        assert!(wanted(&two, "me", 0, None).is_empty());
        // Watching Ana: her best layer.
        assert_eq!(wanted(&one, "me", 0, Some("ana#content")), [6]);
        assert_eq!(wanted(&two, "me", 0, Some("ana#content")), [6]);
        // Switched to Bob: his alone.
        assert_eq!(wanted(&two, "me", 0, Some("bob#content")), [9]);
        // Ana stopped sharing while watched: nothing.
        assert!(wanted(&none, "me", 0, Some("ana#content")).is_empty());
        // Our own share is never received, even if asked for.
        assert!(wanted(&mine, "me", 0, Some("me#content")).is_empty());
        // `--video N` still adds what it picks, the watched share first.
        assert_eq!(wanted(&two, "me", 1, Some("bob#content")), [9, 6]);
        assert_eq!(wanted(&two, "me", 3, Some("bob#content")), [6, 9, 2]);

        // And the slots follow: open, switch, close.
        let slots = Slots::default();
        let watching = slots.after(&slots.plan(&wanted(&two, "me", 0, Some("ana#content"))));
        assert_eq!(watching.receive_stream_ids(), [0, 6]);
        let plan = watching.plan(&wanted(&two, "me", 0, Some("bob#content")));
        assert_eq!((plan.free.clone(), plan.add.clone()), (vec![0], vec![9]));
        let switched = watching.after(&plan);
        let closed = switched.plan(&wanted(&two, "me", 0, None));
        assert_eq!(closed.free, [1]);
        assert!(switched.after(&closed).receiving().is_empty());
    }

    #[test]
    fn slots_line_up_with_receive_stream_ids_across_resubscribes() {
        let none = Slots::default();
        assert_eq!(none.receive_stream_ids(), [0], "only our send line");

        // First: two streams, two new m-lines.
        let plan = none.plan(&[6, 2]);
        assert_eq!(plan.add, [6, 2]);
        assert!(plan.changes());
        let one = none.after(&plan);
        assert_eq!(one.receive_stream_ids(), [0, 6, 2]);

        // The same again changes nothing.
        assert!(!one.plan(&[6, 2]).changes());

        // 6 goes: its slot turns inactive, 2 keeps its place.
        let plan = one.plan(&[2]);
        assert_eq!((plan.free.as_slice(), plan.add.len()), (&[0][..], 0));
        let two = one.after(&plan);
        assert_eq!(two.receive_stream_ids(), [0, 0, 2]);

        // A new stream takes the inactive slot rather than a new m-line.
        let plan = two.plan(&[2, 7]);
        assert_eq!(plan.reuse, [(0, 7)]);
        assert!(plan.add.is_empty());
        let three = two.after(&plan);
        assert_eq!(three.receive_stream_ids(), [0, 7, 2]);
        assert_eq!(three.receiving(), [7, 2]);

        // Swapping one stream for another in the same change: the freed
        // slot is not reused, so the SDP changes and carries the new id.
        let plan = three.plan(&[2, 8]);
        assert_eq!(plan.free, [0]);
        assert_eq!(plan.add, [8]);
        let four = three.after(&plan);
        assert_eq!(four.receive_stream_ids(), [0, 0, 2, 8]);
        // And the next one takes it.
        let five = four.after(&four.plan(&[2, 8, 9]));
        assert_eq!(five.receive_stream_ids(), [0, 9, 2, 8]);
    }

    #[test]
    fn tracks_map_ssrcs_to_streams() {
        let tracks = vec![
            proto::SdkTrackMapping {
                stream_id: Some(6),
                ssrc: Some(1111),
                track_label: Some("v".into()),
            },
            proto::SdkTrackMapping {
                stream_id: Some(2),
                ssrc: Some(2222),
                track_label: None,
            },
            // Half a mapping says nothing.
            proto::SdkTrackMapping {
                stream_id: None,
                ssrc: Some(3333),
                track_label: None,
            },
        ];
        let map = track_streams(&tracks);
        assert_eq!(map.get(&1111), Some(&6));
        assert_eq!(map.get(&2222), Some(&2));
        assert_eq!(map.len(), 2);
    }

    #[test]
    fn frames_are_counted_and_the_first_keyframe_read() {
        let mut stats = StreamStats {
            stream_id: 6,
            attendee: "bob-content".into(),
            share: true,
            mid: "2".into(),
            ..Default::default()
        };
        let stream = include_bytes!("fixtures/test-pattern-320x180.h264");
        let noted = stats.frame("H264", 108, stream, true, true);
        assert_eq!(noted[0], Noted::First);
        assert!(
            matches!(&noted[1], Noted::FirstKeyframe(s) if s.contains("constrained baseline") && s.contains("320x180")),
            "{noted:?}"
        );
        assert!(
            stats
                .frame("H264", 108, &[0, 0, 1, 0x41], false, false)
                .is_empty()
        );
        assert!(
            stats.frame("H264", 108, stream, true, true).is_empty(),
            "only the first"
        );
        assert_eq!((stats.frames, stats.keyframes, stats.gaps), (3, 2, 1));
        assert_eq!(stats.resolution(), Some((320, 180)));
        let line = stats.line();
        assert!(
            line.starts_with(
                "stream 6 (bob-content, a share, user ?) on mid 2: 3 frames, 2 keyframes, 1 gaps"
            ),
            "{line}"
        );
        assert!(
            line.ends_with("codec H264 (pt 108), 320x180 constrained baseline level 12"),
            "{line}"
        );

        // A keyframe whose SPS was missed, then one with it.
        let mut late = StreamStats::default();
        let idr_only = [0, 0, 0, 1, 0x65, 0x88, 0x80];
        assert!(matches!(
            &late.frame("H264", 108, &idr_only, true, true)[1],
            Noted::FirstKeyframe(s) if s == "no SPS in it"
        ));
        let noted = late.frame("H264", 108, stream, true, true);
        assert!(
            matches!(&noted[..], [Noted::Sps(s)] if s.contains("first seen on frame 2 (1 keyframes before it without one)")),
            "{noted:?}"
        );
        assert_eq!(late.resolution(), Some((320, 180)));
        assert!(
            late.line()
                .ends_with("320x180 constrained baseline level 12, 1 keyframes before any SPS"),
            "{}",
            late.line()
        );

        let mut vp8 = StreamStats::default();
        let noted = vp8.frame(
            "VP8",
            96,
            &[0x70, 0x51, 0x00, 0x9d, 0x01, 0x2a, 0x40, 0x01, 0xb4, 0x00],
            true,
            true,
        );
        assert!(
            matches!(&noted[1], Noted::FirstKeyframe(s) if s.starts_with("VP8 keyframe header 10 bytes, 320x180")),
            "{noted:?}"
        );
    }

    #[test]
    fn signaling_extras_log_sizes_not_contents() {
        let lines = data_message_lines(&proto::SdkDataMessageFrame {
            messages: vec![proto::SdkDataMessagePayload {
                topic: Some("reactions".into()),
                data: Some(b"secret words".to_vec()),
                lifetime_ms: Some(300),
                sender_attendee_id: Some("abcdef1234".into()),
                sender_external_user_id: Some("T1-R1-U77".into()),
                ingest_time_ns: None,
            }],
        });
        assert_eq!(
            lines,
            ["DATA_MESSAGE: topic \"reactions\", 12 bytes, lifetime 300 ms, from abcdef12 (U77)"]
        );
        assert!(!lines[0].contains("secret"));
        let line = bitrates_line(&proto::SdkBitrateFrame {
            bitrates: vec![proto::SdkBitrate {
                source_stream_id: Some(6),
                avg_bitrate_bps: Some(850_000),
            }],
            server_available_outgoing_bitrate: Some(2_000_000),
        });
        assert_eq!(
            line,
            "BITRATES: 1 streams: 6=850kbps; server can send us 2000 kbps"
        );
        assert_eq!(
            pause_line(
                "PAUSE",
                &proto::SdkPauseResumeFrame {
                    stream_ids: vec![6],
                    group_ids: vec![]
                }
            ),
            "PAUSE: streams [6] groups []"
        );
    }

    #[test]
    fn resubscribe_lines_say_whether_audio_flowed() {
        let mut r = Resubscribe {
            n: 1,
            stream_ids: vec![0, 6],
            answered_ms: Some(120),
            audio_frames: Some(105),
            window_ms: 2120,
        };
        assert!(r.line().contains("105 audio frames") && r.line().contains("audio kept flowing"));
        r.audio_frames = Some(0);
        assert!(r.line().contains("NO AUDIO"));
        r.answered_ms = None;
        assert!(r.line().ends_with("no answer"));
    }
}
