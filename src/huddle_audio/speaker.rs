//! Playing what the huddle sends: Opus frames through the [`Jitter`]
//! buffer, decoded on the sound device's own thread and played on the
//! default output device.
//!
//! A sibling of [`crate::audio`]'s device thread: the same rodio and cpal,
//! but a stream that never ends instead of a file. The level is left as
//! decoded, so the system's volume is the one that counts.
//!
//! Decoding is `opus-decoder`: pure Rust, no unsafe code, no C library to
//! find on Linux, macOS or Windows. It passes the RFC 8251 test vectors
//! by its own account; it is young, and libopus (the `opus` crate) is the
//! fallback if it ever decodes wrong. `opus-rs`, which encodes what we
//! send, decodes too, but not well enough to play with: it has no CELT
//! loss concealment, refuses a mono packet in a stereo decoder, has no
//! FEC, and reads the network's bytes with unchecked indexing.
//!
//! A panic in the decoder is caught ([`crate::audio::guard`]): that frame
//! is lost and a fresh decoder takes the next. The session checks
//! [`Speaker::stopped`] now and then, so a device that stops asking for
//! sound (its thread gone, or stuck) is opened again on the same
//! [`Feed`] instead of the huddle playing to nobody.
//!
//! What is decoded is also handed to a [`RenderTap`] just before the
//! device takes it: the far end the echo canceller needs while the
//! microphone is open ([`super::processing`]).

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use super::jitter::{CLOCK, Counts, FRAME, Jitter, Pull, opus_samples};
use super::processing::RenderTap;
use crate::audio::guard::{self, Health};

/// Channels played: Chime's Opus is stereo-capable; mono comes out on
/// both.
const CHANNELS: u16 = 2;
/// Silence played while the buffer fills, per pull: 10 ms.
const SILENCE: usize = (CLOCK / 100) as usize * CHANNELS as usize;
/// How long a device may go without asking for sound before it counts as
/// stopped. It asks every few milliseconds while it plays.
const STALL: Duration = Duration::from_secs(2);

/// The frames in hand, shared between the network and the device; it
/// outlives a device that is opened again.
#[derive(Debug, Default)]
struct Shared {
    jitter: Mutex<Jitter>,
    /// Frames the decoder could not read.
    broken: Mutex<u64>,
    /// Times the decoder panicked and was started afresh.
    restarts: AtomicU64,
}

/// One open device's state, shared with its thread and its callback.
#[derive(Debug, Default)]
struct Device {
    stop: AtomicBool,
    /// Times the callback asked for more sound.
    pulls: AtomicU64,
    /// Whether the callback's thread was ended by a panic.
    health: Health,
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// What the speaker played so far.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Played {
    /// The jitter buffer's counts.
    pub jitter: Counts,
    /// Frames the decoder refused.
    pub broken: u64,
    /// Times the decoder panicked and was replaced.
    pub restarts: u64,
}

/// The network's end: hand it frames as they come.
#[derive(Clone, Debug)]
pub struct Feed {
    shared: Arc<Shared>,
}

impl Feed {
    /// One Opus frame with its RTP timestamp.
    pub fn push(&self, timestamp: u32, payload: &[u8]) {
        lock(&self.shared.jitter).push(timestamp, payload.to_vec());
    }

    /// What was played so far.
    pub fn played(&self) -> Played {
        Played {
            jitter: lock(&self.shared.jitter).counts(),
            broken: *lock(&self.shared.broken),
            restarts: self.shared.restarts.load(Ordering::Relaxed),
        }
    }
}

/// What decodes the huddle's frames: opus-decoder, or a stand-in in
/// tests.
trait Opus: Send {
    /// The most samples per channel one packet decodes to.
    fn room(&self) -> usize;
    /// Decodes `packet` (empty: conceals a lost one) into interleaved
    /// `out`; how many samples per channel came out, or `None` if it
    /// would not decode.
    fn decode(&mut self, packet: &[u8], out: &mut [f32]) -> Option<usize>;
}

impl Opus for opus_decoder::OpusDecoder {
    fn room(&self) -> usize {
        self.max_frame_size_per_channel()
    }

    fn decode(&mut self, packet: &[u8], out: &mut [f32]) -> Option<usize> {
        self.decode_float(packet, out, false).ok()
    }
}

/// Makes a decoder: the first, and a fresh one after a panic.
type MakeOpus = fn() -> Result<Box<dyn Opus>, String>;

fn opus_decoder() -> Result<Box<dyn Opus>, String> {
    let decoder = opus_decoder::OpusDecoder::new(CLOCK, usize::from(CHANNELS))
        .map_err(|e| format!("no Opus decoder: {e}"))?;
    Ok(Box::new(decoder))
}

/// Decodes and hands out samples as the device asks for them.
pub struct Decoded {
    shared: Arc<Shared>,
    device: Arc<Device>,
    decoder: Box<dyn Opus>,
    make: MakeOpus,
    /// Interleaved samples decoded and not yet played.
    samples: Vec<f32>,
    at: usize,
    /// Where what is played goes for echo cancellation.
    tap: Option<RenderTap>,
}

impl Decoded {
    fn new(
        shared: Arc<Shared>,
        device: Arc<Device>,
        tap: Option<RenderTap>,
        make: MakeOpus,
    ) -> Result<Self, String> {
        Ok(Self {
            shared,
            device,
            decoder: make()?,
            make,
            samples: Vec::new(),
            at: 0,
            tap,
        })
    }

    /// Decodes what comes next into `self.samples`, and shows it to the
    /// tap.
    fn refill(&mut self) {
        self.device.pulls.fetch_add(1, Ordering::Relaxed);
        self.decode();
        if let Some(tap) = &self.tap {
            tap.push(&self.samples, usize::from(CHANNELS));
        }
    }

    /// Decodes what comes next into `self.samples`.
    fn decode(&mut self) {
        let pull = lock(&self.shared.jitter).pull();
        let room = self.decoder.room() * usize::from(CHANNELS);
        self.samples.resize(room, 0.0);
        self.at = 0;
        let payload: &[u8] = match &pull {
            Pull::Frame(payload) => payload,
            Pull::Lost => &[],
            Pull::Silence => {
                self.samples.clear();
                self.samples.resize(SILENCE, 0.0);
                return;
            }
        };
        let (decoder, samples) = (&mut self.decoder, &mut self.samples);
        let decoded = match self
            .device
            .health
            .catch(|| decoder.decode(payload, samples))
        {
            Some(decoded) => decoded,
            None => {
                // Its state is suspect: a fresh one for the next frame.
                // Losing that state costs a click; keeping it could cost
                // the rest of the huddle.
                log::warn!("huddle audio: the Opus decoder panicked; starting it afresh");
                self.shared.restarts.fetch_add(1, Ordering::Relaxed);
                if let Ok(fresh) = (self.make)() {
                    self.decoder = fresh;
                }
                None
            }
        };
        match decoded {
            Some(per_channel) if per_channel > 0 => {
                self.samples.truncate(per_channel * usize::from(CHANNELS));
            }
            // A frame that does not decode, or concealment with nothing
            // before it: its length in silence keeps the time.
            Some(_) | None => {
                let mut length = FRAME;
                if let Pull::Frame(payload) = &pull {
                    *lock(&self.shared.broken) += 1;
                    length = opus_samples(payload).unwrap_or(FRAME);
                }
                self.samples.clear();
                self.samples
                    .resize(length as usize * usize::from(CHANNELS), 0.0);
            }
        }
    }
}

impl Drop for Decoded {
    fn drop(&mut self) {
        // Dropped by a panic unwinding the device's thread: its stream
        // must not be dropped (see `guard::let_go`).
        self.device.health.dropped();
    }
}

impl Iterator for Decoded {
    type Item = f32;

    fn next(&mut self) -> Option<f32> {
        if self.at >= self.samples.len() {
            if self.device.stop.load(Ordering::Relaxed) {
                return None;
            }
            self.refill();
        }
        let sample = self.samples.get(self.at).copied();
        self.at += 1;
        sample
    }
}

impl rodio::Source for Decoded {
    fn current_span_len(&self) -> Option<usize> {
        // The rate and channels never change.
        None
    }

    fn channels(&self) -> rodio::ChannelCount {
        rodio::ChannelCount::MIN.saturating_add(CHANNELS - 1)
    }

    fn sample_rate(&self) -> rodio::SampleRate {
        rodio::SampleRate::MIN.saturating_add(CLOCK - 1)
    }

    fn total_duration(&self) -> Option<std::time::Duration> {
        None
    }
}

/// Notices a device that has stopped asking for sound.
#[derive(Debug, Default)]
struct Watchdog {
    /// The pulls last seen, and when they were first seen at that count.
    seen: Option<(u64, Instant)>,
}

impl Watchdog {
    /// Whether `pulls` has stayed where it is for [`STALL`] as of `now`.
    fn stalled(&mut self, pulls: u64, now: Instant) -> bool {
        match self.seen {
            Some((seen, since)) if seen == pulls => now.saturating_duration_since(since) >= STALL,
            _ => {
                self.seen = Some((pulls, now));
                false
            }
        }
    }
}

/// The playing end: the device thread. Dropping it stops the sound.
pub struct Speaker {
    device: Arc<Device>,
    thread: Option<std::thread::JoinHandle<()>>,
    watchdog: Watchdog,
}

impl std::fmt::Debug for Speaker {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Speaker").finish_non_exhaustive()
    }
}

impl Speaker {
    /// Opens the default output device on a thread of its own and starts
    /// playing (silence, until frames come), showing what it plays to
    /// `tap`. Returns the speaker and its feed, or why no device would
    /// open.
    pub fn open(tap: Option<RenderTap>) -> Result<(Self, Feed), String> {
        let shared = Arc::new(Shared::default());
        let speaker = Self::start(shared.clone(), tap)?;
        Ok((speaker, Feed { shared }))
    }

    /// Opens the default output device again for `feed`, after the last
    /// one [`stopped`](Self::stopped). Let go of the old one first.
    pub fn reopen(feed: &Feed, tap: Option<RenderTap>) -> Result<Self, String> {
        Self::start(feed.shared.clone(), tap)
    }

    fn start(shared: Arc<Shared>, tap: Option<RenderTap>) -> Result<Self, String> {
        let device = Arc::new(Device::default());
        let source = Decoded::new(shared, device.clone(), tap, opus_decoder)?;
        let (opened, result) = std::sync::mpsc::channel();
        let thread_device = device.clone();
        let thread = std::thread::Builder::new()
            .name("noslacking-huddle-audio".into())
            .spawn(move || {
                let mut sink = match rodio::DeviceSinkBuilder::open_default_sink() {
                    Ok(sink) => sink,
                    Err(error) => {
                        let _ = opened.send(Err(format!("no sound device: {error}")));
                        return;
                    }
                };
                sink.log_on_drop(false);
                let player = rodio::Player::connect_new(sink.mixer());
                player.append(source);
                let _ = opened.send(Ok(()));
                // Holds the device until told to stop; the source ends
                // itself then too.
                while !thread_device.stop.load(Ordering::Relaxed) {
                    std::thread::park_timeout(std::time::Duration::from_millis(200));
                }
                player.stop();
                guard::let_go(sink, thread_device.health.thread_gone());
            })
            .map_err(|e| format!("no audio thread: {e}"))?;
        result
            .recv()
            .map_err(|_| "the audio thread stopped".to_owned())??;
        Ok(Self {
            device,
            thread: Some(thread),
            watchdog: Watchdog::default(),
        })
    }

    /// Whether the device has stopped playing, as of `now`: its callback's
    /// thread died, the thread holding it ended, or it has not asked for
    /// sound in a while. Asked now and then; the first ask only starts
    /// the clock.
    pub fn stopped(&mut self, now: Instant) -> bool {
        self.device.health.thread_gone()
            || self.thread.as_ref().is_none_or(|t| t.is_finished())
            || self
                .watchdog
                .stalled(self.device.pulls.load(Ordering::Relaxed), now)
    }
}

impl Drop for Speaker {
    fn drop(&mut self) {
        self.device.stop.store(true, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            thread.thread().unpark();
            let _ = thread.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Opus's shortest silence: a 20 ms CELT frame with nothing in it.
    const SILENT_FRAME: [u8; 3] = [0xF8, 0xFF, 0xFE];

    /// A 20 ms CELT frame (found by feeding opus-decoder noise) whose
    /// band has 16 short blocks: the one that overflowed opus-decoder's
    /// u8 collapse mask on the device's thread in debug builds.
    const SIXTEEN_BLOCKS: [u8; 55] = [
        0xf8, 0x75, 0xd5, 0x48, 0x6a, 0x8c, 0xcf, 0xb8, 0x7d, 0xb1, 0xd2, 0x3b, 0x43, 0x7d, 0x6b,
        0x6b, 0x17, 0xc1, 0x14, 0xfe, 0x7d, 0xa5, 0xae, 0x93, 0x56, 0x58, 0xc4, 0x69, 0xd1, 0x30,
        0xda, 0x3f, 0x75, 0xab, 0x8e, 0xab, 0x1c, 0x2c, 0x0e, 0xf2, 0xe7, 0xe0, 0x6b, 0xf5, 0x08,
        0x84, 0x7d, 0x51, 0x67, 0xf9, 0x16, 0x30, 0x90, 0x1d, 0x29,
    ];

    /// A packet the stand-in decoder panics on.
    const POISON: [u8; 2] = [0xF8, 0xDE];

    /// Decodes every packet to 20 ms of a constant, panicking on
    /// [`POISON`] as opus-decoder did on [`SIXTEEN_BLOCKS`].
    struct Brittle;

    impl Opus for Brittle {
        fn room(&self) -> usize {
            5760
        }

        fn decode(&mut self, packet: &[u8], out: &mut [f32]) -> Option<usize> {
            assert!(packet != POISON, "attempt to shift left with overflow");
            out.iter_mut().take(960 * 2).for_each(|s| *s = 0.25);
            Some(960)
        }
    }

    fn brittle() -> Result<Box<dyn Opus>, String> {
        Ok(Box::new(Brittle))
    }

    /// A feed and the source a device would play from it.
    fn decoded(tap: Option<RenderTap>, make: MakeOpus) -> (Feed, Arc<Device>, Decoded) {
        let shared = Arc::new(Shared::default());
        let device = Arc::new(Device::default());
        let decoded = Decoded::new(shared.clone(), device.clone(), tap, make).expect("a decoder");
        (Feed { shared }, device, decoded)
    }

    #[test]
    fn a_frame_with_sixteen_short_blocks_decodes() {
        // Cargo.toml turns opus-decoder's overflow checks off for this;
        // with them on, this panics in src/celt/vq.rs.
        let mut decoder = opus_decoder::OpusDecoder::new(CLOCK, 2).expect("a decoder");
        let mut out = vec![0.0; decoder.max_frame_size_per_channel() * 2];
        let decoded = decoder.decode_float(&SIXTEEN_BLOCKS, &mut out, false);
        assert_eq!(decoded.ok(), Some(960));
    }

    #[test]
    fn frames_decode_to_twenty_milliseconds_each() {
        let (feed, _, mut decoded) = decoded(None, opus_decoder);
        for n in 0..4 {
            feed.push(n * FRAME, &SILENT_FRAME);
        }
        // 60 ms are held before playing; then frames come out whole.
        let samples: Vec<f32> = decoded.by_ref().take(4 * 960 * 2).collect();
        assert_eq!(samples.len(), 4 * 960 * 2);
        assert!(samples.iter().all(|s| s.abs() < 1e-3));
        let played = feed.played();
        assert_eq!(played.jitter.played, 4);
        assert_eq!(played.broken, 0);
    }

    #[test]
    fn nothing_to_play_is_silence_not_the_end() {
        let (_, device, mut decoded) = decoded(None, opus_decoder);
        assert_eq!(decoded.by_ref().take(SILENCE * 3).count(), SILENCE * 3);
        device.stop.store(true, Ordering::Relaxed);
        // What was decoded plays out, then it ends.
        assert!(decoded.by_ref().count() < SILENCE);
    }

    #[test]
    fn a_broken_frame_keeps_the_time() {
        let (feed, _, mut decoded) = decoded(None, opus_decoder);
        // A 20 ms TOC whose padding runs past the end: the buffer takes
        // it, the decoder not.
        for n in 0..3 {
            feed.push(n * FRAME, &[0xFB, 0x41, 0xFF]);
        }
        let samples: Vec<f32> = decoded.by_ref().take(3 * 960 * 2).collect();
        assert_eq!(samples.len(), 3 * 960 * 2);
        assert!(feed.played().broken > 0);
    }

    #[test]
    fn what_plays_is_tapped_for_the_echo_canceller() {
        let tap = RenderTap::default();
        tap.set_on(true);
        let (feed, _, mut decoded) = decoded(Some(tap.clone()), opus_decoder);
        for n in 0..4 {
            feed.push(n * FRAME, &SILENT_FRAME);
        }
        // 80 ms of frames, then 60 ms of silence as the buffer runs dry:
        // fourteen 10 ms frames of mono, silence included.
        let played = decoded.by_ref().take(SILENCE * 6 + 4 * 960 * 2).count();
        assert_eq!(played, SILENCE * 6 + 4 * 960 * 2);
        assert_eq!(tap.take().len(), 14);
    }

    #[test]
    fn a_decoder_panic_loses_one_frame_and_plays_on() {
        let (feed, device, mut decoded) = decoded(None, brittle);
        feed.push(0, &[0xF8, 1]);
        feed.push(FRAME, &POISON);
        feed.push(2 * FRAME, &[0xF8, 3]);
        feed.push(3 * FRAME, &[0xF8, 4]);
        let samples: Vec<f32> = decoded.by_ref().take(4 * 960 * 2).collect();
        // The poisoned frame is 20 ms of silence; the rest play.
        assert_eq!(samples.len(), 4 * 960 * 2);
        assert!(samples[..960 * 2].iter().all(|s| *s == 0.25));
        assert!(samples[960 * 2..2 * 960 * 2].iter().all(|s| *s == 0.0));
        assert!(samples[2 * 960 * 2..].iter().all(|s| *s == 0.25));
        let played = feed.played();
        assert_eq!(played.jitter.played, 4);
        assert_eq!((played.broken, played.restarts), (1, 1));
        assert_eq!(device.health.caught(), 1);
        drop(decoded);
        assert!(!device.health.thread_gone());
    }

    #[test]
    fn a_hybrid_frame_claiming_too_much_redundancy_plays() {
        // Two 76-byte hybrid frames claiming more redundancy than they
        // hold: opus-decoder 0.1.1 sliced out of range on it; the patched
        // copy (vendor/opus-decoder) drops the redundancy as libopus does.
        const BAD_HYBRID: [u8; 153] = [
            0x69, 0xaf, 0x0e, 0xe6, 0x85, 0x96, 0x57, 0x1b, 0xb5, 0x8c, 0xc1, 0xa2, 0x21, 0x35,
            0xa5, 0x94, 0x3d, 0x0d, 0x19, 0xd1, 0xc8, 0xe7, 0x7c, 0xd0, 0x63, 0x03, 0xbd, 0xc4,
            0x31, 0xdf, 0x4e, 0x76, 0x64, 0xf8, 0x27, 0x93, 0xe6, 0xc1, 0x2e, 0x44, 0x06, 0x6c,
            0x2e, 0xa8, 0xdf, 0xa2, 0x5d, 0x8f, 0xb0, 0xe1, 0xa8, 0x8f, 0xce, 0x1f, 0xd7, 0x8a,
            0x47, 0xaf, 0x68, 0xf8, 0x71, 0x37, 0xf5, 0x9e, 0x65, 0xa3, 0x2a, 0x18, 0x28, 0x26,
            0x82, 0xe1, 0x88, 0xa7, 0xb8, 0x27, 0xad, 0x60, 0xfa, 0x63, 0x9d, 0x18, 0x42, 0xb4,
            0xb7, 0x92, 0xe3, 0x60, 0x34, 0x5e, 0x40, 0x7e, 0x7c, 0xee, 0x8b, 0x98, 0x8f, 0x1c,
            0xde, 0x63, 0xad, 0x44, 0xce, 0x75, 0x0b, 0x2f, 0x15, 0xf7, 0xbe, 0x4f, 0x2d, 0xa3,
            0x9e, 0x57, 0xbb, 0xa8, 0xdd, 0xbc, 0xf9, 0x0a, 0x3a, 0x14, 0xc2, 0x73, 0xf7, 0x34,
            0x98, 0xbb, 0x54, 0x28, 0xc4, 0xdb, 0xfe, 0x4d, 0x7f, 0x97, 0x0a, 0x58, 0x92, 0x0b,
            0x47, 0x59, 0x37, 0x15, 0x85, 0x30, 0xe0, 0xb1, 0x78, 0xd8, 0x9f, 0x26, 0x72,
        ];
        let (feed, device, mut decoded) = decoded(None, opus_decoder);
        feed.push(0, &BAD_HYBRID);
        feed.push(2 * FRAME, &SILENT_FRAME);
        feed.push(3 * FRAME, &SILENT_FRAME);
        // The odd packet's 40 ms, then the two frames.
        let samples = decoded.by_ref().take(4 * 960 * 2).count();
        assert_eq!(samples, 4 * 960 * 2);
        let played = feed.played();
        assert_eq!(played.jitter.played, 3);
        assert_eq!(played.broken, 0);
        assert_eq!(played.restarts, 0);
        assert!(!device.health.thread_gone());
    }

    #[test]
    fn a_source_dropped_by_a_panic_marks_the_device_gone() {
        let (_, device, decoded) = decoded(None, brittle);
        // As on cpal's thread: the callback owns the source, and a panic
        // outside it unwinds through it.
        let thread = std::thread::spawn(move || {
            let _owned = decoded;
            panic!("rodio's bug");
        });
        assert!(thread.join().is_err());
        assert!(device.health.thread_gone());
    }

    #[test]
    fn a_device_that_stops_asking_counts_as_stopped() {
        let mut watchdog = Watchdog::default();
        let start = Instant::now();
        let at = |ms: u64| start + Duration::from_millis(ms);
        assert!(
            !watchdog.stalled(0, at(0)),
            "the first look starts the clock"
        );
        assert!(!watchdog.stalled(0, at(1_500)));
        assert!(!watchdog.stalled(70, at(1_900)), "it asked");
        assert!(!watchdog.stalled(70, at(3_800)));
        assert!(watchdog.stalled(70, at(3_900)), "two seconds without");
        assert!(!watchdog.stalled(71, at(4_000)), "and back");
    }
}
