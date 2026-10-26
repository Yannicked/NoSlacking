//! The jitter buffer: Opus frames in by RTP timestamp as the network
//! delivers them, out in order at the sound device's pace.
//!
//! It holds back a little (60 ms by default) before it starts, so packets
//! that come late or out of order still play in their place. A frame that
//! never came is played as a loss, which the decoder conceals, unless the
//! frame after it is here: that one carries a low-bitrate copy of it
//! (in-band FEC), which the decoder plays instead ([`Pull::Recover`]).
//! It never conceals more than [`DRY_CONCEALED`] in a row, as a guess
//! stretched further turns to buzzing noise. A longer gap (the sender pausing, or a stall
//! that ran the buffer dry) goes silent after that, and the sound after
//! it fades in.
//!
//! How much it holds follows the network, as WebRTC's NetEQ does: it
//! times each frame against the sound device's clock, and a frame later
//! than the fastest raises the target by that much at once, forgotten
//! over a minute or so. A network that stalls now and then is given room
//! for the next stall; a calm one keeps the delay short.
//!
//! The burst that follows a stall is kept, not cut: everything said is
//! heard, a little late. The delay then comes back down in the pauses:
//! a gap between frames (the sender sends nothing while its speaker is
//! quiet) is shortened while more is held than the target, which nobody
//! misses, as no frame is passed over. Only when far too much is held ([`Jitter::new`]'s limit: the device's clock slower than
//! the sender's for long, or a stall of seconds) do the oldest frames go
//! at once, with one concealed frame across the cut. Each of those would
//! otherwise be a step in the sound: a pop.

use std::collections::BTreeMap;

/// Opus's RTP clock: 48 kHz whatever the audio's own rate.
pub const CLOCK: u32 = 48_000;
/// A 20 ms frame, Opus's usual one in WebRTC.
pub const FRAME: u32 = CLOCK / 50;
/// How many frames are concealed when the buffer runs dry before it goes
/// silent: 60 ms, enough to bridge a short stall.
pub const DRY_CONCEALED: u32 = 3;
/// The silence played per pull while the buffer fills: 10 ms.
pub const SILENCE: u32 = CLOCK / 100;
/// How fast a late frame is forgotten: one sample of headroom for every
/// this many played, so 280 ms of it lasts a little over a minute.
const FORGET: u64 = 256;
/// How far over the target the buffer may be before a pause is
/// shortened: two frames, so it does not chase the network's ripples.
const SLACK: u64 = 2 * FRAME as u64;

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
    /// The frame due was lost but the next one is here: play the copy of
    /// the lost one it carries (in-band FEC), one frame's length.
    Recover(Vec<u8>),
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
    /// Frames' worth of pauses cut short to bring the delay back down.
    pub skipped: u64,
    /// Lost frames concealed.
    pub concealed: u64,
    /// Lost frames played from the next one's copy (in-band FEC).
    pub recovered: u64,
    /// Times the buffer ran dry while playing.
    pub underruns: u64,
    /// How much the buffer means to hold now, in milliseconds.
    pub target_ms: u64,
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
    /// The least to hold before playing, and the most to hold, in
    /// samples.
    base: u64,
    limit: u64,
    /// Held on top of `base`, in samples: how late frames came lately,
    /// the latest at once, forgotten slowly.
    headroom: u64,
    /// Samples played (silence too): the sound device's clock.
    now: u64,
    /// The least delay seen, `now` less a frame's timestamp: a frame on
    /// time. It creeps up a little every pull, so a sender's clock
    /// slower than the device's is not taken for frames ever later.
    fastest: Option<i128>,
    /// The length of the last frame, for concealing a lost one.
    frame: u64,
    /// Frames concealed since the buffer ran dry.
    dry: u32,
    /// Frames were cut while playing: one is concealed across the cut.
    bridge: bool,
    /// The next frame follows silence (see [`Jitter::take_resumed`]).
    resumed: bool,
    counts: Counts,
}

impl Default for Jitter {
    fn default() -> Self {
        Self::new(60, 1000)
    }
}

impl Jitter {
    /// A buffer that holds at least `target_ms` before it plays, more
    /// while the network is uneven (up to half of `limit_ms`), and at
    /// most `limit_ms`.
    pub fn new(target_ms: u32, limit_ms: u32) -> Self {
        let per_ms = u64::from(CLOCK / 1000);
        Self {
            frames: BTreeMap::new(),
            last: None,
            next: None,
            base: u64::from(target_ms) * per_ms,
            limit: u64::from(limit_ms.max(target_ms)) * per_ms,
            headroom: 0,
            now: 0,
            fastest: None,
            frame: u64::from(FRAME),
            dry: 0,
            bridge: false,
            resumed: true,
            counts: Counts::default(),
        }
    }

    /// What happened to the frames so far.
    pub fn counts(&self) -> Counts {
        Counts {
            target_ms: self.target() / u64::from(CLOCK / 1000),
            ..self.counts
        }
    }

    /// How much to hold, in samples: the base and the headroom, the two
    /// together no more than half the limit.
    pub fn target(&self) -> u64 {
        (self.base + self.headroom)
            .min(self.limit / 2)
            .max(self.base)
    }

    /// Whether the frame just pulled is the first after silence, so the
    /// speaker fades it in; asking clears it.
    pub fn take_resumed(&mut self) -> bool {
        std::mem::take(&mut self.resumed)
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
        self.time(at);
        if let Some(next) = self.next.filter(|next| at < *next) {
            // Run dry, the buffer conceals ahead of frames that are only
            // slow; one of those plays from its own time, a little later
            // than due, rather than being thrown away for every frame
            // after it to arrive just as late and be concealed too.
            let concealed = u64::from(self.dry) * self.frame;
            if self.frames.is_empty() && at >= next.saturating_sub(concealed) {
                self.next = Some(at);
                self.dry = 0;
            } else {
                self.counts.late += 1;
                return;
            }
        }
        self.frames.insert(at, payload);
        // Far too much held: drop the oldest until back at the target.
        if self.held() > self.limit {
            let target = self.target();
            while self.held() > target && self.frames.len() > 1 {
                self.frames.pop_first();
                self.counts.trimmed += 1;
            }
            if self.next.is_some() {
                self.next = self.frames.first_key_value().map(|(at, _)| *at);
                self.bridge = true;
            }
        }
    }

    /// Times a frame due at `at` against the device's clock: later than
    /// the fastest frame, it raises the headroom by that much.
    fn time(&mut self, at: u64) {
        let delay = i128::from(self.now) - i128::from(at);
        match self.fastest {
            Some(fastest) if delay >= fastest => {
                let late = u64::try_from(delay - fastest).unwrap_or(u64::MAX);
                if late > self.limit {
                    // The sender's clock went back: time from here.
                    self.fastest = Some(delay);
                } else {
                    self.headroom = self.headroom.max(late);
                }
            }
            _ => self.fastest = Some(delay),
        }
    }

    /// What to play next.
    pub fn pull(&mut self) -> Pull {
        let pull = self.choose();
        let played = match &pull {
            Pull::Frame(payload) => u64::from(opus_samples(payload).unwrap_or(FRAME)),
            Pull::Lost | Pull::Recover(_) => self.frame,
            Pull::Silence => u64::from(SILENCE),
        };
        self.now += played;
        self.headroom = self.headroom.saturating_sub(played / FORGET);
        if let Some(fastest) = &mut self.fastest {
            *fastest += 1;
        }
        pull
    }

    fn choose(&mut self) -> Pull {
        let Some(next) = self.next else {
            if self.held() < self.target() {
                return Pull::Silence;
            }
            let Some((at, payload)) = self.frames.pop_first() else {
                return Pull::Silence;
            };
            return self.play(at, payload);
        };
        if std::mem::take(&mut self.bridge) {
            self.counts.concealed += 1;
            return Pull::Lost;
        }
        let Some((&first, _)) = self.frames.first_key_value() else {
            if self.dry < DRY_CONCEALED {
                self.dry += 1;
                self.next = Some(next + self.frame);
                self.counts.concealed += 1;
                return Pull::Lost;
            }
            // Dry for good: wait to fill up again.
            self.next = None;
            self.dry = 0;
            self.resumed = true;
            self.counts.underruns += 1;
            return Pull::Silence;
        };
        if first <= next {
            let Some((at, payload)) = self.frames.pop_first() else {
                return Pull::Silence;
            };
            return self.play(at, payload);
        }
        let gap = first - next;
        if gap > self.limit {
            // A jump (the sender restarted its clock): carry on from there.
            let Some((at, payload)) = self.frames.pop_first() else {
                return Pull::Silence;
            };
            return self.play(at, payload);
        }
        // A pause, not a lost frame or so, with more ahead than wanted:
        // cut it short, as far as the excess goes.
        let over = (self.held() + gap).saturating_sub(self.target());
        if gap >= 2 * self.frame && over > SLACK {
            let cut = gap.min(over);
            self.next = Some(next + cut);
            self.counts.skipped += cut / self.frame.max(1);
            self.resumed = true;
            return self.choose();
        }
        // One frame lost, and the one after it here: it carries a copy.
        // Not once gone silent: then the sender paused, and sent none.
        if gap == self.frame
            && self.dry < DRY_CONCEALED
            && let Some(after) = self.frames.get(&first)
        {
            self.dry = 0;
            self.next = Some(first);
            self.counts.recovered += 1;
            return Pull::Recover(after.clone());
        }
        if self.dry < DRY_CONCEALED {
            self.dry += 1;
            self.next = Some(next + self.frame);
            self.counts.concealed += 1;
            return Pull::Lost;
        }
        // Concealed as long as it sounds right: silence to the next frame,
        // which fades in.
        self.next = Some((next + u64::from(SILENCE)).min(first));
        self.resumed = true;
        Pull::Silence
    }

    fn play(&mut self, at: u64, payload: Vec<u8>) -> Pull {
        let samples = u64::from(opus_samples(&payload).unwrap_or(FRAME));
        self.frame = samples;
        self.dry = 0;
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

    /// A 20 ms CELT frame `len` bytes long, named `n`.
    fn sized(n: u8, len: usize) -> Vec<u8> {
        let mut payload = frame(n);
        payload.resize(len, 0);
        payload
    }

    #[test]
    fn a_stall_raises_the_target_and_the_burst_after_it_is_kept() {
        let mut jitter = Jitter::new(60, 1000);
        // Frames every 20 ms, but 50 to 63 held up and then all at once
        // with 64: a 280 ms stall, as Teams' relay does now and then.
        let arrives = |n: u64| if (50..64).contains(&n) { 64 } else { n };
        let mut sent = 0u64;
        let mut heard = Vec::new();
        while jitter.now < 200 * u64::from(FRAME) {
            while arrives(sent) * u64::from(FRAME) <= jitter.now {
                let n = u8::try_from(sent).expect("under 256 frames");
                jitter.push(u32::from(n) * FRAME, sized(n, 40));
                sent += 1;
            }
            heard.extend(played(jitter.pull()));
        }
        let counts = jitter.counts();
        assert_eq!((counts.trimmed, counts.late, counts.skipped), (0, 0, 0));
        assert!(heard.windows(2).all(|w| w[1] == w[0] + 1), "{heard:?}");
        assert!((40..=64).all(|n| heard.contains(&n)), "the stall heard");
        assert!(counts.target_ms >= 300, "{counts:?}");
        // So the next one like it is ridden out without a sound missing.
        let underruns = counts.underruns;
        let concealed = counts.concealed;
        let arrives = |n: u64| if (150..164).contains(&n) { 164 } else { n };
        while jitter.now < 400 * u64::from(FRAME) {
            while arrives(sent) * u64::from(FRAME) <= jitter.now {
                let n = u8::try_from(sent % 256).expect("modulo 256");
                jitter.push((sent * u64::from(FRAME)) as u32, sized(n, 40));
                sent += 1;
            }
            jitter.pull();
        }
        assert_eq!(jitter.counts().underruns, underruns);
        assert_eq!(jitter.counts().concealed, concealed);
    }

    #[test]
    fn the_delay_comes_back_down_in_the_pauses() {
        let mut jitter = Jitter::new(20, 1000);
        jitter.push(0, frame(0));
        assert_eq!(played(jitter.pull()), Some(0));
        // 140 ms of speech come at once, on time: 120 more than the
        // target. Speech is never cut.
        for n in 1..8 {
            jitter.push(u32::from(n) * FRAME, frame(n));
        }
        for n in 1..4 {
            assert_eq!(played(jitter.pull()), Some(n));
        }
        // Then a pause of 200 ms before frame 18: it is cut short, so
        // frame 18 follows 7 sooner than its time, and fades in.
        jitter.push(18 * FRAME, frame(18));
        for n in 4..8 {
            assert_eq!(played(jitter.pull()), Some(n));
        }
        assert_eq!(played(jitter.pull()), Some(18));
        assert!(jitter.take_resumed());
        assert!(jitter.counts().skipped >= 2, "{:?}", jitter.counts());
        assert_eq!(jitter.counts().concealed, 0);
    }

    #[test]
    fn a_long_gap_is_concealed_a_little_then_silent() {
        // A deep buffer: the sender pauses 300 ms, and what follows is
        // already here, held as long as wanted, no longer.
        let mut jitter = Jitter::new(300, 1000);
        jitter.push(0, frame(0));
        jitter.push(16 * FRAME, frame(16));
        assert_eq!(played(jitter.pull()), Some(0));
        let mut pulls = Vec::new();
        loop {
            let pull = jitter.pull();
            if let Some(n) = played(pull.clone()) {
                assert_eq!(n, 16);
                break;
            }
            pulls.push(pull);
        }
        let lost = pulls.iter().filter(|p| **p == Pull::Lost).count();
        assert_eq!(lost, DRY_CONCEALED as usize, "{pulls:?}");
        assert!(pulls.iter().skip(lost).all(|p| *p == Pull::Silence));
        assert!(jitter.take_resumed(), "the sound after fades in");
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
        // Out of order: 3 before 2. That is 2 late by a frame, so it
        // wants a frame more before it plays.
        jitter.push(1000 + 2 * FRAME, frame(3));
        jitter.push(1000 + FRAME, frame(2));
        assert_eq!(jitter.pull(), Pull::Silence, "60 ms held, 80 wanted");
        jitter.push(1000 + 3 * FRAME, frame(4));
        for n in 1..=4 {
            assert_eq!(played(jitter.pull()), Some(n));
        }
        // Dry: concealed a while, then silence.
        for _ in 0..DRY_CONCEALED {
            assert_eq!(jitter.pull(), Pull::Lost);
        }
        assert_eq!(jitter.pull(), Pull::Silence);
        assert_eq!(jitter.counts().underruns, 1);
    }

    #[test]
    fn a_short_stall_is_bridged_and_the_sound_after_silence_fades_in() {
        let mut jitter = Jitter::new(20, 300);
        jitter.push(0, frame(1));
        assert_eq!(played(jitter.pull()), Some(1));
        assert!(jitter.take_resumed(), "the first frame follows silence");
        // 20 ms with nothing, then the rest comes after all.
        assert_eq!(jitter.pull(), Pull::Lost);
        jitter.push(2 * FRAME, frame(3));
        assert_eq!(played(jitter.pull()), Some(3));
        assert!(!jitter.take_resumed(), "bridged, never silent");
        assert_eq!(jitter.counts().underruns, 0);
        // Silent, then sound again: that frame fades in.
        for _ in 0..=DRY_CONCEALED {
            jitter.pull();
        }
        jitter.push(20 * FRAME, frame(4));
        assert_eq!(played(jitter.pull()), Some(4));
        assert!(jitter.take_resumed());
    }

    #[test]
    fn a_slow_frame_concealed_past_still_plays_and_so_do_those_after() {
        let mut jitter = Jitter::new(20, 300);
        jitter.push(0, frame(1));
        assert_eq!(played(jitter.pull()), Some(1));
        // Frame 2 is slow: its turn is concealed, then it and the rest come.
        assert_eq!(jitter.pull(), Pull::Lost);
        assert_eq!(jitter.pull(), Pull::Lost);
        jitter.push(FRAME, frame(2));
        assert_eq!(played(jitter.pull()), Some(2));
        jitter.push(2 * FRAME, frame(3));
        assert_eq!(played(jitter.pull()), Some(3));
        assert_eq!(jitter.counts().late, 0);
        // One older than what was concealed is late still.
        jitter.push(0, frame(9));
        assert_eq!(jitter.counts().late, 1);
    }

    #[test]
    fn a_cut_while_playing_is_bridged_by_one_concealed_frame() {
        let mut jitter = Jitter::new(40, 200);
        jitter.push(0, frame(0));
        jitter.push(FRAME, frame(1));
        assert_eq!(played(jitter.pull()), Some(0));
        // A burst: past the limit, so the oldest go.
        for n in 2..14u8 {
            jitter.push(u32::from(n) * FRAME, frame(n));
        }
        assert!(jitter.counts().trimmed > 0);
        assert_eq!(jitter.pull(), Pull::Lost, "across the cut");
        assert!(played(jitter.pull()).is_some_and(|n| n > 1));
    }

    #[test]
    fn a_missing_frame_is_recovered_from_the_next_and_a_late_one_dropped() {
        let mut jitter = Jitter::new(20, 300);
        jitter.push(0, frame(1));
        assert_eq!(played(jitter.pull()), Some(1));
        jitter.push(2 * FRAME, frame(3));
        // Frame 3 carries a copy of 2, then plays itself.
        assert_eq!(jitter.pull(), Pull::Recover(frame(3)));
        assert_eq!(played(jitter.pull()), Some(3));
        // Frame 2 shows up after its turn.
        jitter.push(FRAME, frame(2));
        assert_eq!(jitter.counts().late, 1);
        assert_eq!(jitter.counts().recovered, 1);
        assert_eq!(jitter.counts().concealed, 0);
    }

    #[test]
    fn of_two_missing_frames_the_first_is_concealed_the_second_recovered() {
        let mut jitter = Jitter::new(20, 300);
        jitter.push(0, frame(1));
        assert_eq!(played(jitter.pull()), Some(1));
        jitter.push(3 * FRAME, frame(4));
        assert_eq!(jitter.pull(), Pull::Lost, "frame 2: no copy of it here");
        assert_eq!(jitter.pull(), Pull::Recover(frame(4)), "frame 3");
        assert_eq!(played(jitter.pull()), Some(4));
        assert_eq!(
            (jitter.counts().concealed, jitter.counts().recovered),
            (1, 1)
        );
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
