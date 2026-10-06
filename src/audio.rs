//! Voice clips and sound files played in the app.
//!
//! The card asks with a [`Request`]; [`Playback`] decides what that means
//! and answers with [`Effect`]s for the app to carry out: fetch the sound
//! (the worker does, with the workspace's own client, into memory), hand
//! it to the [`Device`] (a thread of its own that decodes and plays it),
//! or open it in the system's player when it can't play here. The device
//! answers with [`Report`]s: how far it got, that it ended, or why it
//! could not play.
//!
//! One sound plays at a time: starting another stops the first. Signing
//! out of a workspace stops its sound.
//!
//! Playing needs the `audio` feature (rodio, with symphonia's decoders).
//! Without it every sound opens in the system's player, as before.

use std::sync::Arc;
use std::time::Duration;

use crate::failure::{Failure, Problem};
use crate::model::{File, Media};

#[cfg(feature = "audio")]
mod device;
#[cfg(feature = "audio")]
pub use device::Device;

/// The most a sound may weigh to be played here. It is held in memory
/// while it plays; anything larger opens in the system's player. A
/// minute of a voice clip is well under a megabyte.
pub const MAX_BYTES: u64 = 50 * 1024 * 1024;

/// How often the device says how far it got while a sound plays.
pub const TICK: Duration = Duration::from_millis(100);

/// A sound's bytes, fetched whole. Shared, so the device can start it
/// again after it ends without another fetch; printed by length only, so
/// a log of events never fills with them.
#[derive(Clone)]
pub struct Bytes(pub Arc<[u8]>);

impl std::fmt::Debug for Bytes {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Bytes({} bytes)", self.0.len())
    }
}

impl AsRef<[u8]> for Bytes {
    fn as_ref(&self) -> &[u8] {
        &self.0
    }
}

impl From<Vec<u8>> for Bytes {
    fn from(bytes: Vec<u8>) -> Self {
        Self(bytes.into())
    }
}

/// Which sound plays: its workspace and Slack's id for the file.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Key {
    pub team: String,
    pub file: String,
}

/// A sound a card asks to play, with what deciding where it plays needs.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Track {
    /// Slack's id for the file.
    pub file: String,
    /// What plays: Slack's smaller copy when it made one (see
    /// [`File::player`]).
    pub url: String,
    /// The name of what plays, whose extension says how it is stored.
    pub name: String,
    pub mimetype: String,
    /// The file's size, as Slack gave it.
    pub size: u64,
    /// How long it lasts, as Slack measured it, until the decoder knows.
    pub duration_ms: Option<u64>,
}

impl Track {
    /// The sound in `file`, if it is one and has something to play.
    pub fn of(file: &File) -> Option<Self> {
        if file.media() != Some(Media::Audio) {
            return None;
        }
        let (url, name) = file.player()?;
        // Slack's MP4 copy of an old clip is AAC whatever the original was.
        let mimetype = if file.aac.as_deref() == Some(url.as_str()) {
            "audio/mp4".to_owned()
        } else {
            file.mimetype.clone()
        };
        Some(Self {
            file: file.id.clone(),
            url,
            name,
            mimetype,
            size: file.size,
            duration_ms: file.duration_ms,
        })
    }

    /// The extension of what plays, lowercased, for the decoder's guess.
    pub fn extension(&self) -> String {
        self.name
            .rsplit_once('.')
            .map(|(_, ext)| ext.to_ascii_lowercase())
            .unwrap_or_default()
    }
}

/// What a card asks of the player.
#[derive(Clone, Debug, PartialEq)]
pub enum Request {
    /// Plays the sound, or pauses it when it is playing, or carries on.
    Toggle(Track),
    /// Moves to `fraction` (0 to 1) of the way through, starting the
    /// sound there if another was playing.
    Seek { track: Track, fraction: f32 },
    /// Stops whatever plays.
    Stop,
}

/// Why a sound plays in the system's player instead, or stopped.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Why {
    /// Stored in a way no decoder here reads, Opus above all.
    NoDecoder,
    /// Larger than [`MAX_BYTES`].
    TooLarge,
    /// Built without the `audio` feature.
    NotBuilt,
    /// There is no sound device, or it would not open.
    NoDevice,
    /// The decoder could not make sense of it.
    Unreadable,
    /// It could not be fetched; the worker's problem says why.
    Unfetched,
}

impl Why {
    /// The reason in a sentence for a toast, or nothing when there is
    /// nothing to say: a build without sound opens every sound in the
    /// system's player, and a failed fetch has its own toast.
    pub fn message(self) -> Option<String> {
        use crate::i18n::t;
        let text = match self {
            Self::NoDecoder => t("This kind of sound can't play in the app."),
            Self::TooLarge => t("This sound is too large to play in the app."),
            Self::NoDevice => t("There is no sound device to play on."),
            Self::Unreadable => t("This sound could not be played in the app."),
            Self::NotBuilt | Self::Unfetched => return None,
        };
        Some(text.into_owned())
    }
}

/// Where a sound plays.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Route {
    Here,
    /// In the system's player, for this reason.
    Elsewhere(Why),
}

/// Where `track` plays: here, unless this build has no sound, it is too
/// large to hold, or it is stored in a way the decoders don't read.
pub fn route(track: &Track) -> Route {
    if !cfg!(feature = "audio") {
        Route::Elsewhere(Why::NotBuilt)
    } else if track.size > MAX_BYTES {
        Route::Elsewhere(Why::TooLarge)
    } else if !decodable(&track.extension(), &track.mimetype) {
        Route::Elsewhere(Why::NoDecoder)
    } else {
        Route::Here
    }
}

/// Whether a sound with extension `ext` and type `mimetype` may be one
/// the decoders read: AAC or ALAC in MP4, MP3, Vorbis in Ogg, FLAC and
/// WAV. Opus (in Ogg or WebM) has no decoder; an `.ogg` may still hold
/// Opus, which only decoding finds out.
pub fn decodable(ext: &str, mimetype: &str) -> bool {
    const EXTENSIONS: &[&str] = &[
        "m4a", "mp4", "aac", "mp3", "ogg", "oga", "flac", "wav", "wave",
    ];
    const TYPES: &[&str] = &[
        "audio/mp4",
        "audio/x-m4a",
        "audio/m4a",
        "audio/aac",
        "audio/mpeg",
        "audio/mp3",
        "audio/ogg",
        "audio/vorbis",
        "audio/flac",
        "audio/x-flac",
        "audio/wav",
        "audio/x-wav",
        "audio/wave",
        "audio/vnd.wave",
    ];
    let mimetype = mimetype.to_ascii_lowercase();
    // "audio/ogg; codecs=opus" names its codec.
    if ext.eq_ignore_ascii_case("opus") || mimetype.contains("opus") || mimetype.contains("webm") {
        return false;
    }
    let essence = mimetype.split(';').next().unwrap_or("").trim();
    EXTENSIONS.iter().any(|e| e.eq_ignore_ascii_case(ext)) || TYPES.contains(&essence)
}

/// What a sound is doing, for its card.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Phase {
    /// Being fetched or decoded.
    Loading,
    Playing,
    Paused,
    /// It could not play here, for this reason.
    Failed(Why),
}

/// The sound that is playing (or loading, paused, failed), as the cards
/// draw it.
#[derive(Clone, Debug, PartialEq)]
pub struct Now {
    pub key: Key,
    pub phase: Phase,
    pub position: Duration,
    /// How long it lasts: the decoder's measure, else Slack's.
    pub duration: Option<Duration>,
}

impl Now {
    /// How far through it is, from 0 to 1; 0 while its length is unknown.
    pub fn fraction(&self) -> f32 {
        self.duration.map_or(0.0, |d| fraction(self.position, d))
    }

    /// Whether it is playing or about to, so the button offers pause.
    pub fn busy(&self) -> bool {
        matches!(self.phase, Phase::Loading | Phase::Playing)
    }
}

/// An order for the [`Device`].
#[derive(Clone, Debug)]
pub enum Order {
    /// Decodes `bytes` and plays them from the start, as sound `id`.
    Load {
        id: u64,
        bytes: Bytes,
        /// The extension and type, to help the decoder guess the format.
        ext: String,
        mimetype: String,
    },
    Play,
    Pause,
    Seek(Duration),
    /// Stops and lets go of the sound and the sound device.
    Stop,
}

/// What the [`Device`] says about sound `id`.
#[derive(Clone, Debug, PartialEq)]
pub enum Report {
    /// It decoded and started, lasting `duration` if the decoder knows.
    Loaded { id: u64, duration: Option<Duration> },
    /// It is `position` in.
    Position { id: u64, position: Duration },
    /// It played to its end.
    Ended { id: u64 },
    /// It could not play.
    Failed { id: u64, why: Why },
}

impl Report {
    fn id(&self) -> u64 {
        match self {
            Self::Loaded { id, .. }
            | Self::Position { id, .. }
            | Self::Ended { id }
            | Self::Failed { id, .. } => *id,
        }
    }
}

/// What the app does for [`Playback`].
#[derive(Clone, Debug)]
pub enum Effect {
    /// Fetches the sound for request `id` with `team`'s client.
    Fetch {
        team: String,
        id: u64,
        url: String,
        name: String,
    },
    /// Tells the device.
    Device(Order),
    /// Opens the sound in the system's player instead.
    Open {
        team: String,
        url: String,
        name: String,
    },
    /// Says why it plays elsewhere (see [`Why::message`]).
    Tell(Why),
    /// Says what stopped the fetch.
    Problem(Problem),
}

/// The sound in hand: what was asked, how far it got, and the request id
/// that tells its answers from those of a sound asked for before it.
#[derive(Clone, Debug)]
struct Current {
    key: Key,
    track: Track,
    id: u64,
    phase: Phase,
    position: Duration,
    duration: Option<Duration>,
    /// Where to start once loaded, when the waveform was clicked to start.
    start: Option<f32>,
}

/// The player's state: at most one sound, and what to do as requests and
/// reports come in. Pure, so it is tested without a sound device.
#[derive(Debug, Default)]
pub struct Playback {
    current: Option<Current>,
    next_id: u64,
}

impl Playback {
    /// What the cards show: the sound in hand, if any.
    pub fn now(&self) -> Option<Now> {
        self.current.as_ref().map(|c| Now {
            key: c.key.clone(),
            phase: c.phase,
            position: c.position,
            duration: c.duration,
        })
    }

    /// What to do when a card in `team` asks for `request`.
    pub fn request(&mut self, team: &str, request: Request) -> Vec<Effect> {
        match request {
            Request::Toggle(track) => {
                let key = key(team, &track);
                match self.current.as_mut().filter(|c| c.key == key) {
                    Some(c) if c.phase == Phase::Playing => {
                        c.phase = Phase::Paused;
                        vec![Effect::Device(Order::Pause)]
                    }
                    Some(c) if c.phase == Phase::Paused => {
                        c.phase = Phase::Playing;
                        vec![Effect::Device(Order::Play)]
                    }
                    // A second click while it loads changes its mind.
                    Some(c) if c.phase == Phase::Loading => self.stop(),
                    _ => self.start(key, track, None),
                }
            }
            Request::Seek { track, fraction } => {
                let key = key(team, &track);
                let fraction = fraction.clamp(0.0, 1.0);
                match self.current.as_mut().filter(|c| c.key == key) {
                    Some(c) if matches!(c.phase, Phase::Playing | Phase::Paused) => {
                        let Some(duration) = c.duration else {
                            return Vec::new();
                        };
                        c.position = position_at(fraction, duration);
                        vec![Effect::Device(Order::Seek(c.position))]
                    }
                    Some(c) if c.phase == Phase::Loading => {
                        c.start = Some(fraction);
                        Vec::new()
                    }
                    _ => self.start(key, track, Some(fraction)),
                }
            }
            Request::Stop => self.stop(),
        }
    }

    /// Starts `track` in place of whatever was in hand, here or in the
    /// system's player.
    fn start(&mut self, key: Key, track: Track, start: Option<f32>) -> Vec<Effect> {
        let mut effects = self.stop();
        match route(&track) {
            Route::Elsewhere(why) => {
                effects.extend(elsewhere(&key.team, &track, why));
            }
            Route::Here => {
                self.next_id += 1;
                let id = self.next_id;
                effects.push(Effect::Fetch {
                    team: key.team.clone(),
                    id,
                    url: track.url.clone(),
                    name: track.name.clone(),
                });
                self.current = Some(Current {
                    duration: track.duration_ms.map(Duration::from_millis),
                    key,
                    track,
                    id,
                    phase: Phase::Loading,
                    position: Duration::ZERO,
                    start,
                });
            }
        }
        effects
    }

    /// Stops whatever is in hand.
    fn stop(&mut self) -> Vec<Effect> {
        match self.current.take() {
            Some(_) => vec![Effect::Device(Order::Stop)],
            None => Vec::new(),
        }
    }

    /// The sound in hand, when `id` is its request: answers for one asked
    /// for before are stale.
    fn current(&mut self, id: u64) -> Option<&mut Current> {
        self.current.as_mut().filter(|c| c.id == id)
    }

    /// What to do once the worker fetched sound `id`, or could not.
    pub fn fetched(&mut self, id: u64, result: Result<Bytes, Problem>) -> Vec<Effect> {
        let Some(c) = self.current(id).filter(|c| c.phase == Phase::Loading) else {
            return Vec::new();
        };
        match result {
            Ok(bytes) => vec![Effect::Device(Order::Load {
                id,
                bytes,
                ext: c.track.extension(),
                mimetype: c.track.mimetype.clone(),
            })],
            // The system's player has a larger limit.
            Err(problem) if problem.failure == Failure::TooLarge => {
                c.phase = Phase::Failed(Why::TooLarge);
                elsewhere(&c.key.team, &c.track, Why::TooLarge)
            }
            Err(problem) => {
                c.phase = Phase::Failed(Why::Unfetched);
                vec![Effect::Problem(problem)]
            }
        }
    }

    /// What to do when the device says something.
    pub fn report(&mut self, report: Report) -> Vec<Effect> {
        let Some(c) = self.current(report.id()) else {
            return Vec::new();
        };
        match report {
            Report::Loaded { duration, .. } => {
                c.duration = duration.or(c.duration);
                c.phase = Phase::Playing;
                c.position = Duration::ZERO;
                match (c.start.take(), c.duration) {
                    (Some(fraction), Some(duration)) if fraction > 0.0 => {
                        c.position = position_at(fraction, duration);
                        vec![Effect::Device(Order::Seek(c.position))]
                    }
                    _ => Vec::new(),
                }
            }
            Report::Position { position, .. } => {
                if matches!(c.phase, Phase::Playing | Phase::Paused) {
                    c.position = c.duration.map_or(position, |d| position.min(d));
                }
                Vec::new()
            }
            // Ready to play again from the start.
            Report::Ended { .. } => {
                c.phase = Phase::Paused;
                c.position = Duration::ZERO;
                Vec::new()
            }
            Report::Failed { why, .. } => {
                c.phase = Phase::Failed(why);
                elsewhere(&c.key.team, &c.track, why)
            }
        }
    }

    /// Stops `team`'s sound when you sign out of it.
    pub fn signed_out(&mut self, team: &str) -> Vec<Effect> {
        if self.current.as_ref().is_some_and(|c| c.key.team == team) {
            self.stop()
        } else {
            Vec::new()
        }
    }
}

fn key(team: &str, track: &Track) -> Key {
    Key {
        team: team.to_owned(),
        file: track.file.clone(),
    }
}

/// Opening `track` in the system's player, saying why first.
fn elsewhere(team: &str, track: &Track, why: Why) -> Vec<Effect> {
    vec![
        Effect::Tell(why),
        Effect::Open {
            team: team.to_owned(),
            url: track.url.clone(),
            name: track.name.clone(),
        },
    ]
}

/// How far `position` is through `duration`, from 0 to 1.
pub fn fraction(position: Duration, duration: Duration) -> f32 {
    if duration.is_zero() {
        return 0.0;
    }
    (position.as_secs_f64() / duration.as_secs_f64()).clamp(0.0, 1.0) as f32
}

/// The point `fraction` of the way through `duration`.
pub fn position_at(fraction: f32, duration: Duration) -> Duration {
    duration.mul_f64(f64::from(fraction.clamp(0.0, 1.0)))
}

/// How far along a bar from `left` to `right` the pointer at `x` is,
/// from 0 to 1: where a click on the waveform seeks to.
pub fn fraction_at(x: f32, left: f32, right: f32) -> f32 {
    if right <= left {
        return 0.0;
    }
    ((x - left) / (right - left)).clamp(0.0, 1.0)
}

/// How many of `bars` waveform bars are drawn as played, `fraction` of
/// the way through: none at the start, all at the end.
pub fn played_bars(bars: usize, fraction: f32) -> usize {
    ((bars as f32 * fraction.clamp(0.0, 1.0)).round() as usize).min(bars)
}

/// "0:05 / 0:14": how far in and how long, or only how far when the
/// length is unknown.
pub fn time_text(position: Duration, duration: Option<Duration>) -> String {
    let ms = |d: Duration| u64::try_from(d.as_millis()).unwrap_or(u64::MAX);
    let at = crate::model::duration_text(ms(position));
    match duration {
        Some(duration) => format!("{at} / {}", crate::model::duration_text(ms(duration))),
        None => at,
    }
}

/// Where the cards find what plays: [`publish`]ed by the app each frame.
fn now_id() -> egui::Id {
    egui::Id::new("noslacking-audio-now")
}

/// Hands the cards what plays this frame.
pub fn publish(ctx: &egui::Context, now: Option<Now>) {
    ctx.data_mut(|d| d.insert_temp(now_id(), now));
}

/// What plays, if it is `file` in `team`.
pub fn now_for(ctx: &egui::Context, team: &str, file: &str) -> Option<Now> {
    ctx.data(|d| d.get_temp::<Option<Now>>(now_id()))
        .flatten()
        .filter(|now| now.key.team == team && now.key.file == file)
}

/// The player without a sound stack: nothing ever plays here, so it
/// takes no orders and has nothing to say.
#[cfg(not(feature = "audio"))]
#[derive(Debug, Default)]
pub struct Device;

#[cfg(not(feature = "audio"))]
impl Device {
    /// A device that plays nothing.
    pub fn new(_waker: crate::backend::Waker) -> Self {
        Self
    }

    /// Ignores `order`: [`route`] sends every sound elsewhere.
    pub fn send(&mut self, _order: Order) {}

    /// Never says anything.
    pub fn try_recv(&self) -> Option<Report> {
        None
    }
}

/// A WAV of 16-bit mono samples at `rate` per second, for the demo's
/// voice clip and the tests.
#[cfg(any(test, feature = "demo"))]
pub fn wav(rate: u32, samples: &[i16]) -> Vec<u8> {
    let data = u32::try_from(samples.len() * 2).unwrap_or(u32::MAX);
    let mut out = Vec::with_capacity(44 + samples.len() * 2);
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&(36 + data).to_le_bytes());
    out.extend_from_slice(b"WAVEfmt ");
    out.extend_from_slice(&16u32.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes()); // PCM
    out.extend_from_slice(&1u16.to_le_bytes()); // mono
    out.extend_from_slice(&rate.to_le_bytes());
    out.extend_from_slice(&(rate * 2).to_le_bytes());
    out.extend_from_slice(&2u16.to_le_bytes());
    out.extend_from_slice(&16u16.to_le_bytes());
    out.extend_from_slice(b"data");
    out.extend_from_slice(&data.to_le_bytes());
    for sample in samples {
        out.extend_from_slice(&sample.to_le_bytes());
    }
    out
}

/// `seconds` of soft beeps at `rate`: a short tone every half second,
/// rising and falling, faded in and out so it never clicks.
#[cfg(any(test, feature = "demo"))]
pub fn beeps(rate: u32, seconds: f32) -> Vec<i16> {
    let count = (rate as f32 * seconds) as usize;
    let beat = rate as usize / 2;
    let tone = beat * 3 / 5;
    let notes = [440.0f32, 554.37, 659.25, 554.37];
    (0..count)
        .map(|n| {
            let (index, at) = (n / beat, n % beat);
            if at >= tone {
                return 0;
            }
            let pitch = notes.get(index % notes.len()).copied().unwrap_or(440.0);
            let t = n as f32 / rate as f32;
            let edge = (at.min(tone - at) as f32 / (rate as f32 * 0.02)).min(1.0);
            let wave = (t * pitch * std::f32::consts::TAU).sin();
            (wave * edge * 9000.0) as i16
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn clip() -> Track {
        Track {
            file: "F24".into(),
            url: "https://files.slack.com/files-pri/T-F24/clip.m4a".into(),
            name: "clip.m4a".into(),
            mimetype: "audio/mp4".into(),
            size: 171_020,
            duration_ms: Some(14_000),
        }
    }

    fn fetch_id(effects: &[Effect]) -> Option<u64> {
        effects.iter().find_map(|e| match e {
            Effect::Fetch { id, .. } => Some(*id),
            _ => None,
        })
    }

    fn opens(effects: &[Effect]) -> bool {
        effects.iter().any(|e| matches!(e, Effect::Open { .. }))
    }

    #[test]
    fn opus_and_webm_go_to_the_system_player() {
        assert!(decodable("m4a", "audio/mp4"));
        assert!(decodable("mp3", ""));
        assert!(decodable("", "audio/mpeg"));
        assert!(decodable("ogg", "audio/ogg"));
        assert!(decodable("WAV", "audio/x-wav"));
        assert!(decodable("flac", "audio/flac"));
        assert!(!decodable("opus", "audio/ogg"));
        assert!(!decodable("ogg", "audio/ogg; codecs=opus"));
        assert!(!decodable("webm", "audio/webm"));
        assert!(!decodable("", "audio/x-unknown"));
        assert!(!decodable("aiff", "audio/aiff"));
    }

    #[test]
    fn where_a_sound_plays() {
        let here = if cfg!(feature = "audio") {
            Route::Here
        } else {
            Route::Elsewhere(Why::NotBuilt)
        };
        assert_eq!(route(&clip()), here);
        if !cfg!(feature = "audio") {
            return;
        }
        let opus = Track {
            name: "memo.opus".into(),
            mimetype: "audio/ogg".into(),
            ..clip()
        };
        assert_eq!(route(&opus), Route::Elsewhere(Why::NoDecoder));
        let large = Track {
            size: MAX_BYTES + 1,
            ..clip()
        };
        assert_eq!(route(&large), Route::Elsewhere(Why::TooLarge));
        let limit = Track {
            size: MAX_BYTES,
            ..clip()
        };
        assert_eq!(route(&limit), Route::Here);
    }

    #[test]
    fn a_voice_clip_plays_slacks_aac_copy() {
        let file = File {
            id: "F1".into(),
            name: "Audio clip.webm".into(),
            mimetype: "audio/webm".into(),
            url_private: Some("https://files.slack.com/files-pri/T-F1/clip.webm".into()),
            aac: Some("https://files.slack.com/files-tmb/T-F1-x/clip_audio.mp4".into()),
            duration_ms: Some(5_000),
            ..File::default()
        };
        let track = Track::of(&file).expect("a sound with a copy");
        assert_eq!(track.name, "Audio clip.m4a");
        assert_eq!(track.mimetype, "audio/mp4");
        assert!(decodable(&track.extension(), &track.mimetype));
        // A video is not a sound.
        let video = File {
            mimetype: "video/mp4".into(),
            ..file
        };
        assert_eq!(Track::of(&video), None);
    }

    #[test]
    fn position_and_seek_maths() {
        let d = Duration::from_secs(14);
        assert_eq!(fraction(Duration::from_secs(7), d), 0.5);
        assert_eq!(fraction(Duration::from_secs(20), d), 1.0);
        assert_eq!(fraction(Duration::from_secs(1), Duration::ZERO), 0.0);
        assert_eq!(position_at(0.25, d), Duration::from_millis(3500));
        assert_eq!(position_at(-1.0, d), Duration::ZERO);
        assert_eq!(position_at(2.0, d), d);
        assert_eq!(fraction_at(150.0, 100.0, 200.0), 0.5);
        assert_eq!(fraction_at(50.0, 100.0, 200.0), 0.0);
        assert_eq!(fraction_at(250.0, 100.0, 200.0), 1.0);
        assert_eq!(fraction_at(150.0, 200.0, 200.0), 0.0);
        assert_eq!(
            time_text(Duration::from_millis(5_400), Some(d)),
            "0:05 / 0:14"
        );
        assert_eq!(time_text(Duration::from_secs(65), None), "1:05");
    }

    #[test]
    fn the_waveform_splits_at_the_position() {
        assert_eq!(played_bars(50, 0.0), 0);
        assert_eq!(played_bars(50, 1.0), 50);
        assert_eq!(played_bars(50, 0.5), 25);
        assert_eq!(played_bars(50, 0.011), 1);
        assert_eq!(played_bars(50, 1.5), 50);
        assert_eq!(played_bars(50, -0.5), 0);
        assert_eq!(played_bars(0, 0.7), 0);
    }

    #[test]
    fn play_pause_and_resume() {
        if !cfg!(feature = "audio") {
            return;
        }
        let mut playback = Playback::default();
        let effects = playback.request("T1", Request::Toggle(clip()));
        let id = fetch_id(&effects).expect("fetched first");
        assert_eq!(playback.now().map(|n| n.phase), Some(Phase::Loading));
        let effects = playback.fetched(id, Ok(Bytes::from(vec![1, 2, 3])));
        assert!(matches!(
            effects.as_slice(),
            [Effect::Device(Order::Load { ext, .. })] if ext == "m4a"
        ));
        playback.report(Report::Loaded {
            id,
            duration: Some(Duration::from_secs(10)),
        });
        let now = playback.now().expect("in hand");
        assert_eq!(now.phase, Phase::Playing);
        // The decoder's length wins over Slack's.
        assert_eq!(now.duration, Some(Duration::from_secs(10)));
        playback.report(Report::Position {
            id,
            position: Duration::from_secs(4),
        });
        assert_eq!(playback.now().map(|n| n.fraction()), Some(0.4));
        let effects = playback.request("T1", Request::Toggle(clip()));
        assert!(matches!(effects.as_slice(), [Effect::Device(Order::Pause)]));
        let effects = playback.request("T1", Request::Toggle(clip()));
        assert!(matches!(effects.as_slice(), [Effect::Device(Order::Play)]));
        // Seeking while it plays.
        let effects = playback.request(
            "T1",
            Request::Seek {
                track: clip(),
                fraction: 0.5,
            },
        );
        assert!(matches!(
            effects.as_slice(),
            [Effect::Device(Order::Seek(at))] if *at == Duration::from_secs(5)
        ));
        // At the end it waits at the start, and plays again from there.
        playback.report(Report::Ended { id });
        let now = playback.now().expect("still in hand");
        assert_eq!((now.phase, now.position), (Phase::Paused, Duration::ZERO));
        let effects = playback.request("T1", Request::Toggle(clip()));
        assert!(matches!(effects.as_slice(), [Effect::Device(Order::Play)]));
    }

    #[test]
    fn one_sound_at_a_time_and_stale_answers_are_ignored() {
        if !cfg!(feature = "audio") {
            return;
        }
        let mut playback = Playback::default();
        let first = fetch_id(&playback.request("T1", Request::Toggle(clip()))).expect("first");
        let other = Track {
            file: "F25".into(),
            ..clip()
        };
        let effects = playback.request("T1", Request::Toggle(other));
        assert!(matches!(effects.first(), Some(Effect::Device(Order::Stop))));
        let second = fetch_id(&effects).expect("second");
        assert_ne!(first, second);
        // The first sound's answers arrive late and change nothing.
        assert!(playback.fetched(first, Ok(Bytes::from(vec![0]))).is_empty());
        assert!(
            playback
                .report(Report::Failed {
                    id: first,
                    why: Why::Unreadable
                })
                .is_empty()
        );
        assert_eq!(playback.now().map(|n| n.key.file), Some("F25".into()));
        // A second click while it loads stops it.
        let effects = playback.request(
            "T1",
            Request::Toggle(Track {
                file: "F25".into(),
                ..clip()
            }),
        );
        assert!(matches!(effects.as_slice(), [Effect::Device(Order::Stop)]));
        assert_eq!(playback.now(), None);
    }

    #[test]
    fn clicking_the_waveform_starts_there() {
        if !cfg!(feature = "audio") {
            return;
        }
        let mut playback = Playback::default();
        let effects = playback.request(
            "T1",
            Request::Seek {
                track: clip(),
                fraction: 0.5,
            },
        );
        let id = fetch_id(&effects).expect("fetched");
        playback.fetched(id, Ok(Bytes::from(vec![0])));
        let effects = playback.report(Report::Loaded { id, duration: None });
        // Slack's length (14 s) places it until the decoder knows.
        assert!(matches!(
            effects.as_slice(),
            [Effect::Device(Order::Seek(at))] if *at == Duration::from_secs(7)
        ));
    }

    #[test]
    fn failures_fall_back_to_the_system_player() {
        if !cfg!(feature = "audio") {
            return;
        }
        // No sound device: say so and open it elsewhere, keeping the
        // failure on the card.
        let mut playback = Playback::default();
        let id = fetch_id(&playback.request("T1", Request::Toggle(clip()))).expect("fetched");
        playback.fetched(id, Ok(Bytes::from(vec![0])));
        let effects = playback.report(Report::Failed {
            id,
            why: Why::NoDevice,
        });
        assert!(matches!(effects.first(), Some(Effect::Tell(Why::NoDevice))));
        assert!(opens(&effects));
        assert_eq!(
            playback.now().map(|n| n.phase),
            Some(Phase::Failed(Why::NoDevice))
        );
        // Clicking again tries again.
        assert!(fetch_id(&playback.request("T1", Request::Toggle(clip()))).is_some());

        // Opus never tries.
        let mut playback = Playback::default();
        let opus = Track {
            name: "memo.opus".into(),
            ..clip()
        };
        let effects = playback.request("T1", Request::Toggle(opus));
        assert!(fetch_id(&effects).is_none());
        assert!(matches!(
            effects.first(),
            Some(Effect::Tell(Why::NoDecoder))
        ));
        assert!(opens(&effects));
        assert_eq!(playback.now(), None);

        // Larger than Slack said: the fetch stops at the cap.
        let mut playback = Playback::default();
        let id = fetch_id(&playback.request("T1", Request::Toggle(clip()))).expect("fetched");
        let too_large = Problem::new(
            crate::failure::Doing::Download {
                name: "clip.m4a".into(),
            },
            Failure::TooLarge,
        );
        let effects = playback.fetched(id, Err(too_large));
        assert!(opens(&effects));

        // Any other fetch failure is only said.
        let mut playback = Playback::default();
        let id = fetch_id(&playback.request("T1", Request::Toggle(clip()))).expect("fetched");
        let offline = Problem::new(
            crate::failure::Doing::Download {
                name: "clip.m4a".into(),
            },
            Failure::NotSignedIn,
        );
        let effects = playback.fetched(id, Err(offline));
        assert!(matches!(effects.as_slice(), [Effect::Problem(_)]));
    }

    #[test]
    fn signing_out_stops_that_workspaces_sound() {
        if !cfg!(feature = "audio") {
            return;
        }
        let mut playback = Playback::default();
        playback.request("T1", Request::Toggle(clip()));
        assert!(playback.signed_out("T2").is_empty());
        assert!(playback.now().is_some());
        let effects = playback.signed_out("T1");
        assert!(matches!(effects.as_slice(), [Effect::Device(Order::Stop)]));
        assert_eq!(playback.now(), None);
    }

    #[test]
    fn without_sound_everything_opens_in_the_system_player() {
        if cfg!(feature = "audio") {
            return;
        }
        let mut playback = Playback::default();
        let effects = playback.request("T1", Request::Toggle(clip()));
        assert!(matches!(effects.first(), Some(Effect::Tell(Why::NotBuilt))));
        assert!(opens(&effects));
        assert_eq!(Why::NotBuilt.message(), None);
    }

    #[test]
    fn a_generated_wav_is_well_formed() {
        let samples = beeps(8000, 1.0);
        assert_eq!(samples.len(), 8000);
        let bytes = wav(8000, &samples);
        assert_eq!(bytes.len(), 44 + 16_000);
        assert_eq!(bytes.get(..4), Some(&b"RIFF"[..]));
        assert_eq!(bytes.get(8..12), Some(&b"WAVE"[..]));
        // Faded in: the first sample is silent; it sounds within a beep.
        assert_eq!(samples.first(), Some(&0));
        assert!(samples.iter().take(2000).any(|s| s.abs() > 4000));
    }
}
