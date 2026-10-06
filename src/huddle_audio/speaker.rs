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
//! fallback if it ever decodes wrong.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use super::jitter::{CLOCK, Counts, FRAME, Jitter, Pull};

/// Channels played: Chime's Opus is stereo-capable; mono comes out on
/// both.
const CHANNELS: u16 = 2;
/// Silence played while the buffer fills, per pull: 10 ms.
const SILENCE: usize = (CLOCK / 100) as usize * CHANNELS as usize;

/// The frames in hand, shared between the network and the device.
#[derive(Debug, Default)]
struct Shared {
    jitter: Mutex<Jitter>,
    stop: AtomicBool,
    /// Frames the decoder could not read.
    broken: Mutex<u64>,
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
        }
    }
}

/// Decodes and hands out samples as the device asks for them.
pub struct Decoded {
    shared: Arc<Shared>,
    decoder: opus_decoder::OpusDecoder,
    /// Interleaved samples decoded and not yet played.
    samples: Vec<f32>,
    at: usize,
}

impl Decoded {
    fn new(shared: Arc<Shared>) -> Result<Self, String> {
        let decoder = opus_decoder::OpusDecoder::new(CLOCK, usize::from(CHANNELS))
            .map_err(|e| format!("no Opus decoder: {e}"))?;
        Ok(Self {
            shared,
            decoder,
            samples: Vec::new(),
            at: 0,
        })
    }

    /// Decodes what comes next into `self.samples`.
    fn refill(&mut self) {
        let pull = lock(&self.shared.jitter).pull();
        let room = self.decoder.max_frame_size_per_channel() * usize::from(CHANNELS);
        self.samples.resize(room, 0.0);
        self.at = 0;
        let decoded = match &pull {
            Pull::Frame(payload) => self.decoder.decode_float(payload, &mut self.samples, false),
            Pull::Lost => self.decoder.decode_float(&[], &mut self.samples, false),
            Pull::Silence => {
                self.samples.clear();
                self.samples.resize(SILENCE, 0.0);
                return;
            }
        };
        match decoded {
            Ok(per_channel) if per_channel > 0 => {
                self.samples.truncate(per_channel * usize::from(CHANNELS));
            }
            // A frame that does not decode, or concealment with nothing
            // before it: a frame's worth of silence keeps the time.
            Ok(_) | Err(_) => {
                if matches!(pull, Pull::Frame(_)) {
                    *lock(&self.shared.broken) += 1;
                }
                self.samples.clear();
                self.samples
                    .resize(FRAME as usize * usize::from(CHANNELS), 0.0);
            }
        }
    }
}

impl Iterator for Decoded {
    type Item = f32;

    fn next(&mut self) -> Option<f32> {
        if self.at >= self.samples.len() {
            if self.shared.stop.load(Ordering::Relaxed) {
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

/// The playing end: the device thread. Dropping it stops the sound.
pub struct Speaker {
    shared: Arc<Shared>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl std::fmt::Debug for Speaker {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Speaker").finish_non_exhaustive()
    }
}

impl Speaker {
    /// Opens the default output device on a thread of its own and starts
    /// playing (silence, until frames come). Returns the speaker and its
    /// feed, or why no device would open.
    pub fn open() -> Result<(Self, Feed), String> {
        let shared = Arc::new(Shared::default());
        let source = Decoded::new(shared.clone())?;
        let (opened, result) = std::sync::mpsc::channel();
        let thread_shared = shared.clone();
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
                while !thread_shared.stop.load(Ordering::Relaxed) {
                    std::thread::park_timeout(std::time::Duration::from_millis(200));
                }
                player.stop();
            })
            .map_err(|e| format!("no audio thread: {e}"))?;
        result
            .recv()
            .map_err(|_| "the audio thread stopped".to_owned())??;
        let feed = Feed {
            shared: shared.clone(),
        };
        Ok((
            Self {
                shared,
                thread: Some(thread),
            },
            feed,
        ))
    }
}

impl Drop for Speaker {
    fn drop(&mut self) {
        self.shared.stop.store(true, Ordering::Relaxed);
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

    #[test]
    fn frames_decode_to_twenty_milliseconds_each() {
        let shared = Arc::new(Shared::default());
        let feed = Feed {
            shared: shared.clone(),
        };
        let mut decoded = Decoded::new(shared).expect("a decoder");
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
        let shared = Arc::new(Shared::default());
        let mut decoded = Decoded::new(shared.clone()).expect("a decoder");
        assert_eq!(decoded.by_ref().take(SILENCE * 3).count(), SILENCE * 3);
        shared.stop.store(true, Ordering::Relaxed);
        // What was decoded plays out, then it ends.
        assert!(decoded.by_ref().count() < SILENCE);
    }

    #[test]
    fn a_broken_frame_keeps_the_time() {
        let shared = Arc::new(Shared::default());
        let feed = Feed {
            shared: shared.clone(),
        };
        let mut decoded = Decoded::new(shared).expect("a decoder");
        // A 20 ms TOC whose padding runs past the end: the buffer takes
        // it, the decoder not.
        for n in 0..3 {
            feed.push(n * FRAME, &[0xFB, 0x41, 0xFF]);
        }
        let samples: Vec<f32> = decoded.by_ref().take(3 * 960 * 2).collect();
        assert_eq!(samples.len(), 3 * 960 * 2);
        assert!(feed.played().broken > 0);
    }
}
