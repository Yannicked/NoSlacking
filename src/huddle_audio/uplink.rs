//! What goes out on the audio track besides silence: 20 ms Opus frames
//! from the microphone (or the probe's tone), and the small pieces that
//! put them on the wire. Each is pure, so each is tested alone.
//!
//! - [`Framer`] joins two 10 ms frames, the unit echo cancellation works
//!   in, into the 20 ms the encoder takes.
//! - [`Dtx`] decides which frames are worth sending: speech, and a little
//!   after it; in a pause one frame in twenty, as libopus's own DTX does,
//!   so the far end's comfort noise and the NAT binding stay fresh.
//! - [`Outbound`] stamps each packet with its RTP time (48 kHz, 960 a
//!   frame, counting the frames left out) and marks the first after a
//!   gap, the start of a talkspurt (RFC 3551 §4.1).
//! - [`level`] measures a frame for the RFC 6464 audio-level header
//!   extension, which `str0m` writes when Chime's answer negotiates it.
//! - [`Tone`] is the probe's quiet 440 Hz.

use super::jitter::{CLOCK, FRAME};

/// Samples in 10 ms at 48 kHz: one frame of echo cancellation.
pub const TEN_MS: usize = (CLOCK / 100) as usize;
/// Samples in 20 ms at 48 kHz: one Opus frame.
pub const TWENTY_MS: usize = FRAME as usize;

/// One encoded 20 ms frame, on its way to the session.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Outgoing {
    /// The Opus packet.
    pub payload: Vec<u8>,
    /// How many 20 ms frames were left out just before this one (a pause
    /// under DTX); the RTP time skips them.
    pub gap: u32,
    /// The frame's level for RFC 6464: 0 is full scale, 127 silence, in
    /// -dBov.
    pub level: u8,
    /// Whether it holds speech, for the same extension's V bit.
    pub voice: bool,
}

/// Joins 10 ms frames into 20 ms ones.
#[derive(Debug, Default)]
pub struct Framer {
    half: Vec<f32>,
    /// Whether either half held speech.
    speech: bool,
}

impl Framer {
    /// Takes 10 ms; every second call hands back 20 ms and whether any of
    /// it was speech.
    pub fn push(&mut self, ten_ms: &[f32], speech: bool) -> Option<(Vec<f32>, bool)> {
        self.speech |= speech;
        self.half.extend_from_slice(ten_ms);
        if self.half.len() < TWENTY_MS {
            return None;
        }
        let frame: Vec<f32> = self.half.drain(..TWENTY_MS).collect();
        let speech = std::mem::take(&mut self.speech);
        Some((frame, speech))
    }
}

/// Frames of a pause still sent after speech: 200 ms, so a word's tail
/// and a short breath go out whole.
const HANGOVER: u32 = 10;
/// In a pause, one frame in this many is sent: every 400 ms.
const KEEPALIVE: u32 = 20;

/// Discontinuous transmission, decided outside the encoder: `opus-rs`
/// does not offer libopus's own.
#[derive(Debug, Default)]
pub struct Dtx {
    /// Frames since the last speech.
    quiet: u32,
    /// Frames left out since the last one sent.
    skipped: u32,
}

impl Dtx {
    /// Whether to send the next frame: `Some(gap)` with the frames left
    /// out before it, or `None` to leave it out.
    pub fn decide(&mut self, speech: bool) -> Option<u32> {
        self.quiet = if speech {
            0
        } else {
            self.quiet.saturating_add(1)
        };
        if self.quiet <= HANGOVER || self.skipped + 1 >= KEEPALIVE {
            return Some(std::mem::take(&mut self.skipped));
        }
        self.skipped += 1;
        None
    }
}

/// When a packet goes out, in RTP terms.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Stamp {
    /// Its RTP time, 48 kHz, not yet wrapped to 32 bits (`str0m` does).
    pub time: u64,
    /// The marker bit: the first packet after frames were left out.
    pub talkspurt: bool,
}

/// The audio track's RTP clock, shared by silence and the microphone so
/// switching between them never steps back.
#[derive(Debug, Default)]
pub struct Outbound {
    next: u64,
}

impl Outbound {
    /// The stamp for the next packet, `gap` frames after the last.
    pub fn stamp(&mut self, gap: u32) -> Stamp {
        let time = self.next + u64::from(gap) * u64::from(FRAME);
        self.next = time + u64::from(FRAME);
        Stamp {
            time,
            talkspurt: gap > 0,
        }
    }
}

/// A frame's level for RFC 6464, as WebRTC measures it: its RMS against
/// full scale, in -dB, 0 to 127.
pub fn level(samples: &[f32]) -> u8 {
    if samples.is_empty() {
        return 127;
    }
    let power = samples.iter().map(|s| s * s).sum::<f32>() / samples.len() as f32;
    if power <= 1e-13 {
        return 127;
    }
    let db = -10.0 * power.log10();
    db.round().clamp(0.0, 127.0) as u8
}

/// A sine for the probe: something Chime can hear without anyone
/// talking.
#[derive(Debug)]
pub struct Tone {
    phase: f32,
    step: f32,
    amplitude: f32,
}

impl Tone {
    /// `hz` at `amplitude` (of full scale), sampled at 48 kHz.
    pub fn new(hz: f32, amplitude: f32) -> Self {
        Self {
            phase: 0.0,
            step: std::f32::consts::TAU * hz / CLOCK as f32,
            amplitude,
        }
    }

    /// The probe's: 440 Hz at a tenth of full scale (-23 dBFS RMS),
    /// clearly there and not loud.
    pub fn probe() -> Self {
        Self::new(440.0, 0.1)
    }

    /// Fills `out` with what comes next.
    pub fn fill(&mut self, out: &mut [f32]) {
        for sample in out {
            *sample = self.amplitude * self.phase.sin();
            self.phase = (self.phase + self.step) % std::f32::consts::TAU;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn two_ten_ms_frames_make_one_of_twenty() {
        let mut framer = Framer::default();
        let first: Vec<f32> = (0..TEN_MS).map(|n| n as f32).collect();
        let second: Vec<f32> = (TEN_MS..2 * TEN_MS).map(|n| n as f32).collect();
        assert_eq!(framer.push(&first, false), None);
        let (frame, speech) = framer.push(&second, true).expect("20 ms");
        assert_eq!(frame.len(), TWENTY_MS);
        assert!(speech, "speech in either half counts");
        assert!(frame.iter().enumerate().all(|(n, s)| *s == n as f32));
        // The next pair starts afresh.
        assert_eq!(framer.push(&first, false), None);
        let (_, speech) = framer.push(&first, false).expect("20 ms");
        assert!(!speech);
    }

    #[test]
    fn rtp_time_counts_frames_and_marks_gaps() {
        let mut out = Outbound::default();
        assert_eq!(
            out.stamp(0),
            Stamp {
                time: 0,
                talkspurt: false
            }
        );
        assert_eq!(out.stamp(0).time, 960);
        // Three frames left out: the time skips them and the packet is
        // the start of a talkspurt.
        assert_eq!(
            out.stamp(3),
            Stamp {
                time: 960 * 5,
                talkspurt: true
            }
        );
        assert_eq!(
            out.stamp(0),
            Stamp {
                time: 960 * 6,
                talkspurt: false
            }
        );
    }

    #[test]
    fn dtx_sends_speech_a_tail_and_a_frame_now_and_then() {
        let mut dtx = Dtx::default();
        assert_eq!(dtx.decide(true), Some(0));
        // The hangover: ten quiet frames still go.
        for _ in 0..HANGOVER {
            assert_eq!(dtx.decide(false), Some(0));
        }
        // Then 19 left out and the 20th sent, saying so.
        let mut sent = Vec::new();
        for n in 0..2 * KEEPALIVE {
            if let Some(gap) = dtx.decide(false) {
                sent.push((n, gap));
            }
        }
        assert_eq!(
            sent,
            vec![
                (KEEPALIVE - 1, KEEPALIVE - 1),
                (2 * KEEPALIVE - 1, KEEPALIVE - 1)
            ]
        );
        // Speech again: sent at once, with the frames left out since.
        for _ in 0..5 {
            assert_eq!(dtx.decide(false), None);
        }
        assert_eq!(dtx.decide(true), Some(5));
        assert_eq!(dtx.decide(true), Some(0));
    }

    #[test]
    fn levels_are_minus_db_of_full_scale() {
        assert_eq!(level(&[0.0; 960]), 127);
        assert_eq!(level(&[]), 127);
        assert_eq!(level(&[1.0; 960]), 0);
        assert_eq!(level(&[0.1; 960]), 20);
        let mut sine = vec![0.0; 960];
        Tone::new(1000.0, 1.0).fill(&mut sine);
        // A full-scale sine's RMS is 3 dB under full scale.
        assert_eq!(level(&sine), 3);
    }

    #[test]
    fn the_tone_is_quiet_and_at_its_pitch() {
        let mut tone = Tone::probe();
        let mut second = vec![0.0; CLOCK as usize];
        // In two calls: the phase carries on between them.
        let (a, b) = second.split_at_mut(12_345);
        tone.fill(a);
        tone.fill(b);
        let crossings = second
            .windows(2)
            .filter(|w| w[0] < 0.0 && w[1] >= 0.0)
            .count();
        assert!((439..=441).contains(&crossings), "{crossings}");
        let peak = second.iter().fold(0.0f32, |m, s| m.max(s.abs()));
        assert!((0.099..=0.1).contains(&peak), "{peak}");
        assert_eq!(level(&second), 23);
    }
}
