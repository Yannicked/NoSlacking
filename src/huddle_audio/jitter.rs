//! The jitter buffer: Opus frames in by RTP timestamp as the network
//! delivers them, out in order at the sound device's pace.
//!
//! It holds back a little (60 ms by default) before it starts, so packets
//! that come late or out of order still play in their place. A frame that
//! never came is played as a loss, which the decoder conceals; when the
//! buffer runs dry it waits to fill again, playing silence; when it grows
//! too long (the device's clock slower than the sender's, or a burst after
//! a stall) the oldest frames go, so the delay stays short.

use std::collections::BTreeMap;

/// Opus's RTP clock: 48 kHz whatever the audio's own rate.
pub const CLOCK: u32 = 48_000;
/// A 20 ms frame, Opus's usual one in WebRTC.
pub const FRAME: u32 = CLOCK / 50;

/// How many 48 kHz samples one Opus packet holds, from its TOC byte and
/// frame count (RFC 6716 §3.1). `None` for an empty or broken packet.
pub fn opus_samples(packet: &[u8]) -> Option<u32> {
    let toc = *packet.first()?;
    let config = toc >> 3;
    // Frame lengths in units of 2.5 ms.
    let per_frame = match config {
        0..=11 => [4, 8, 16, 24][usize::from(config & 3)],
        12..=15 => [4, 8][usize::from(config & 1)],
        _ => [1, 2, 4, 8][usize::from(config & 3)],
    };
    let frames = match toc & 3 {
        0 => 1,
        1 | 2 => 2,
        _ => u32::from(*packet.get(1)? & 0x3F),
    };
    let samples = per_frame * frames * (CLOCK / 400);
    // A packet holds at most 120 ms.
    (frames > 0 && samples <= CLOCK / 1000 * 120).then_some(samples)
}

/// What to play next.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Pull {
    /// This frame.
    Frame(Vec<u8>),
    /// The frame due was lost: conceal one frame's length.
    Lost,
    /// Nothing to play yet: silence while the buffer fills.
    Silence,
}

/// Counts for the log.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Counts {
    /// Frames taken in.
    pub pushed: u64,
    /// Frames played.
    pub played: u64,
    /// Frames that came after their turn, thrown away.
    pub late: u64,
    /// Frames thrown away to keep the delay short.
    pub trimmed: u64,
    /// Lost frames concealed.
    pub concealed: u64,
    /// Times the buffer ran dry while playing.
    pub underruns: u64,
}

/// The buffer.
#[derive(Debug)]
pub struct Jitter {
    /// Frames by their RTP timestamp, unwrapped to 64 bits.
    frames: BTreeMap<u64, Vec<u8>>,
    /// The last timestamp seen, raw and unwrapped.
    last: Option<(u32, u64)>,
    /// The timestamp due next, while playing.
    next: Option<u64>,
    /// How much to hold before playing, and the most to hold, in samples.
    target: u64,
    limit: u64,
    /// The length of the last frame, for concealing a lost one.
    frame: u64,
    counts: Counts,
}

impl Default for Jitter {
    fn default() -> Self {
        Self::new(60, 300)
    }
}

impl Jitter {
    /// A buffer that holds `target_ms` before it plays and at most
    /// `limit_ms`.
    pub fn new(target_ms: u32, limit_ms: u32) -> Self {
        let per_ms = u64::from(CLOCK / 1000);
        Self {
            frames: BTreeMap::new(),
            last: None,
            next: None,
            target: u64::from(target_ms) * per_ms,
            limit: u64::from(limit_ms.max(target_ms)) * per_ms,
            frame: u64::from(FRAME),
            counts: Counts::default(),
        }
    }

    /// What happened to the frames so far.
    pub fn counts(&self) -> Counts {
        self.counts
    }

    /// How much is held, in samples, from the first frame held to the end
    /// of the last.
    pub fn held(&self) -> u64 {
        match (self.frames.first_key_value(), self.frames.last_key_value()) {
            (Some((first, _)), Some((last, payload))) => {
                last - first + u64::from(opus_samples(payload).unwrap_or(FRAME))
            }
            _ => 0,
        }
    }

    /// The 32-bit timestamp as a 64-bit one that keeps counting past the
    /// wrap, either way from the last one seen.
    fn unwrap(&mut self, timestamp: u32) -> u64 {
        let unwrapped = match self.last {
            // Starts high enough that going back never goes below zero.
            None => u64::from(timestamp) + (1 << 32),
            Some((raw, base)) => {
                let step = i64::from(timestamp.wrapping_sub(raw) as i32);
                base.saturating_add_signed(step)
            }
        };
        if self.last.is_none_or(|(_, base)| unwrapped > base) {
            self.last = Some((timestamp, unwrapped));
        }
        unwrapped
    }

    /// Takes a frame in.
    pub fn push(&mut self, timestamp: u32, payload: Vec<u8>) {
        if payload.is_empty() {
            return;
        }
        let at = self.unwrap(timestamp);
        self.counts.pushed += 1;
        if self.next.is_some_and(|next| at < next) {
            self.counts.late += 1;
            return;
        }
        self.frames.insert(at, payload);
        // Too much held: drop the oldest until back at the target.
        if self.held() > self.limit {
            while self.held() > self.target && self.frames.len() > 1 {
                self.frames.pop_first();
                self.counts.trimmed += 1;
            }
            if self.next.is_some() {
                self.next = self.frames.first_key_value().map(|(at, _)| *at);
            }
        }
    }

    /// What to play next.
    pub fn pull(&mut self) -> Pull {
        let Some(next) = self.next else {
            if self.held() < self.target {
                return Pull::Silence;
            }
            let Some((at, payload)) = self.frames.pop_first() else {
                return Pull::Silence;
            };
            return self.play(at, payload);
        };
        let Some((&first, _)) = self.frames.first_key_value() else {
            // Dry: wait to fill up again.
            self.next = None;
            self.counts.underruns += 1;
            return Pull::Silence;
        };
        if first <= next {
            let Some((at, payload)) = self.frames.pop_first() else {
                return Pull::Silence;
            };
            return self.play(at, payload);
        }
        if first - next > self.limit {
            // A jump (the sender restarted its clock): carry on from there.
            let Some((at, payload)) = self.frames.pop_first() else {
                return Pull::Silence;
            };
            return self.play(at, payload);
        }
        self.next = Some(next + self.frame);
        self.counts.concealed += 1;
        Pull::Lost
    }

    fn play(&mut self, at: u64, payload: Vec<u8>) -> Pull {
        let samples = u64::from(opus_samples(&payload).unwrap_or(FRAME));
        self.frame = samples;
        self.next = Some(at + samples);
        self.counts.played += 1;
        Pull::Frame(payload)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A 20 ms CELT frame's TOC (config 31, one frame), then a body byte
    /// that names the frame, so tests can tell them apart.
    fn frame(n: u8) -> Vec<u8> {
        vec![0xF8, n]
    }

    fn played(pull: Pull) -> Option<u8> {
        match pull {
            Pull::Frame(payload) => payload.get(1).copied(),
            _ => None,
        }
    }

    #[test]
    fn opus_packets_say_how_long_they_are() {
        assert_eq!(opus_samples(&[0xF8, 0xFF, 0xFE]), Some(960), "20 ms CELT");
        assert_eq!(opus_samples(&[0x08]), Some(960), "20 ms SILK NB");
        assert_eq!(opus_samples(&[0x18]), Some(2880), "60 ms SILK NB");
        assert_eq!(opus_samples(&[0x78]), Some(960), "20 ms hybrid FB");
        assert_eq!(opus_samples(&[0xE0]), Some(120), "2.5 ms CELT");
        assert_eq!(opus_samples(&[0x09]), Some(1920), "two 20 ms frames");
        assert_eq!(opus_samples(&[0x0B, 0x03]), Some(2880), "three, coded");
        assert_eq!(opus_samples(&[0x0B, 0x07]), None, "140 ms is too long");
        assert_eq!(opus_samples(&[]), None);
        assert_eq!(opus_samples(&[0x0B]), None, "a count byte is missing");
    }

    #[test]
    fn frames_play_in_order_after_the_target() {
        let mut jitter = Jitter::new(60, 300);
        jitter.push(1000, frame(1));
        assert_eq!(jitter.pull(), Pull::Silence, "20 ms held, 60 wanted");
        // Out of order: 3 before 2.
        jitter.push(1000 + 2 * FRAME, frame(3));
        jitter.push(1000 + FRAME, frame(2));
        assert_eq!(played(jitter.pull()), Some(1));
        assert_eq!(played(jitter.pull()), Some(2));
        assert_eq!(played(jitter.pull()), Some(3));
        assert_eq!(jitter.pull(), Pull::Silence);
        assert_eq!(jitter.counts().underruns, 1);
    }

    #[test]
    fn a_missing_frame_is_concealed_and_a_late_one_dropped() {
        let mut jitter = Jitter::new(20, 300);
        jitter.push(0, frame(1));
        assert_eq!(played(jitter.pull()), Some(1));
        jitter.push(2 * FRAME, frame(3));
        assert_eq!(jitter.pull(), Pull::Lost);
        assert_eq!(played(jitter.pull()), Some(3));
        // Frame 2 shows up after its turn.
        jitter.push(FRAME, frame(2));
        assert_eq!(jitter.counts().late, 1);
        assert_eq!(jitter.counts().concealed, 1);
    }

    #[test]
    fn timestamps_wrap_around() {
        let mut jitter = Jitter::new(40, 300);
        let start = u32::MAX - FRAME + 1;
        jitter.push(start, frame(1));
        jitter.push(start.wrapping_add(FRAME), frame(2));
        jitter.push(start.wrapping_add(2 * FRAME), frame(3));
        assert_eq!(played(jitter.pull()), Some(1));
        assert_eq!(played(jitter.pull()), Some(2));
        assert_eq!(played(jitter.pull()), Some(3));
    }

    #[test]
    fn too_much_held_is_trimmed_to_the_target() {
        let mut jitter = Jitter::new(60, 200);
        for n in 0..12u8 {
            jitter.push(u32::from(n) * FRAME, frame(n));
        }
        assert!(jitter.held() <= 200 * 48, "{}", jitter.held());
        assert_eq!(jitter.counts().trimmed, 8);
        // What is left plays from the newest frames.
        assert_eq!(played(jitter.pull()), Some(8));
    }

    #[test]
    fn a_clock_jump_restarts_from_the_new_time() {
        let mut jitter = Jitter::new(20, 200);
        jitter.push(0, frame(1));
        assert_eq!(played(jitter.pull()), Some(1));
        jitter.push(10 * CLOCK, frame(2));
        assert_eq!(played(jitter.pull()), Some(2));
        assert_eq!(jitter.counts().concealed, 0);
    }
}
