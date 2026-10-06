//! Cleaning up the microphone before it is encoded: WebRTC's audio
//! processing, in `sonora`'s pure Rust port (M145).
//!
//! In order, as a browser does: a high-pass filter (rumble and DC), AEC3
//! echo cancellation against what the huddle plays, noise suppression and
//! AGC2's adaptive digital gain with its limiter. Everything runs on 10 ms
//! frames at 48 kHz mono.
//!
//! The far end, the "render" signal, is tapped in [`super::speaker`] as
//! it is decoded, just before the sound device takes it ([`RenderTap`]).
//! Echo cancellation must see each render frame before the capture frames
//! that carry its echo; the microphone's thread hands it every render
//! frame that came since, then the capture frame. AEC3 finds the actual
//! delay between the two itself (its delay estimator, up to about half a
//! second); the stream delay given it is only the starting guess: the
//! input device's latency, which cpal reports, plus [`OUTPUT_GUESS_MS`].
//!
//! Also here: [`ToMono48k`], which brings the device's samples to 48 kHz
//! mono (WebRTC's sinc resampler, from `sonora-common-audio`), and the
//! voice detector DTX uses (AGC2's RNN VAD).

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use sonora::config::{
    AdaptiveDigital, EchoCanceller, GainController2, HighPassFilter, NoiseSuppression,
    NoiseSuppressionLevel,
};
use sonora::{AudioProcessing, Config, StreamConfig};
use sonora_agc2::vad_wrapper::VoiceActivityDetectorWrapper;
use sonora_common_audio::push_sinc_resampler::PushSincResampler;

use super::jitter::CLOCK;
use super::uplink::TEN_MS;

/// What the output side adds to the echo path, as a first guess: rodio's
/// mixer and the device's buffer. AEC3 measures the real delay.
pub const OUTPUT_GUESS_MS: i32 = 40;
/// Render frames held for the microphone, at most: half a second. Older
/// ones are of no use to the echo canceller.
const RENDER_HELD: usize = 50;

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// The tap's state.
#[derive(Debug, Default)]
struct Tap {
    /// Whether a microphone is open to use what is tapped.
    on: bool,
    /// Mono samples short of a whole frame.
    partial: Vec<f32>,
    frames: VecDeque<Vec<f32>>,
}

/// What the huddle plays, in 10 ms frames, for the echo canceller. Shared
/// by the speaker, which pushes, and the microphone, which takes. While no
/// microphone is open it keeps nothing.
#[derive(Clone, Debug, Default)]
pub struct RenderTap {
    inner: Arc<Mutex<Tap>>,
}

impl RenderTap {
    /// Starts or stops keeping what is played.
    pub fn set_on(&self, on: bool) {
        let mut tap = lock(&self.inner);
        tap.on = on;
        if !on {
            tap.partial.clear();
            tap.frames.clear();
        }
    }

    /// Whether a microphone is taking frames.
    pub fn is_on(&self) -> bool {
        lock(&self.inner).on
    }

    /// Takes `samples` as played: 48 kHz, `channels` interleaved.
    pub fn push(&self, samples: &[f32], channels: usize) {
        let mut tap = lock(&self.inner);
        if !tap.on || channels == 0 {
            return;
        }
        let gain = 1.0 / channels as f32;
        for frame in samples.chunks_exact(channels) {
            let mono = frame.iter().sum::<f32>() * gain;
            tap.partial.push(mono);
            if tap.partial.len() == TEN_MS {
                let whole = std::mem::take(&mut tap.partial);
                tap.frames.push_back(whole);
                if tap.frames.len() > RENDER_HELD {
                    tap.frames.pop_front();
                }
            }
        }
    }

    /// Every whole frame played since the last call, oldest first.
    pub fn take(&self) -> Vec<Vec<f32>> {
        lock(&self.inner).frames.drain(..).collect()
    }
}

/// The device's samples, interleaved at its rate, to 10 ms frames of
/// 48 kHz mono.
pub struct ToMono48k {
    channels: usize,
    /// Mono samples per 10 ms at the device's rate.
    chunk: usize,
    /// None at 48 kHz.
    resampler: Option<PushSincResampler>,
    pending: Vec<f32>,
    out: Vec<f32>,
}

impl std::fmt::Debug for ToMono48k {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ToMono48k")
            .field("channels", &self.channels)
            .field("chunk", &self.chunk)
            .finish_non_exhaustive()
    }
}

impl ToMono48k {
    /// For a device at `rate` with `channels`. The rate must hold a whole
    /// number of samples in 10 ms, as every common one does.
    pub fn new(rate: u32, channels: u16) -> Result<Self, String> {
        if rate == 0 || !rate.is_multiple_of(100) || channels == 0 {
            return Err(format!("{rate} Hz × {channels} cannot be resampled"));
        }
        let chunk = (rate / 100) as usize;
        let resampler = (rate != CLOCK).then(|| PushSincResampler::new(chunk, TEN_MS));
        Ok(Self {
            channels: usize::from(channels),
            chunk,
            resampler,
            pending: Vec::with_capacity(chunk),
            out: vec![0.0; TEN_MS],
        })
    }

    /// Takes interleaved samples; calls `each` with every whole 10 ms
    /// frame of 48 kHz mono they complete.
    pub fn push(&mut self, interleaved: &[f32], mut each: impl FnMut(&[f32])) {
        let gain = 1.0 / self.channels as f32;
        for frame in interleaved.chunks_exact(self.channels) {
            self.pending.push(frame.iter().sum::<f32>() * gain);
            if self.pending.len() < self.chunk {
                continue;
            }
            match &mut self.resampler {
                Some(resampler) => {
                    resampler.resample(&self.pending, &mut self.out);
                    each(&self.out);
                }
                None => each(&self.pending),
            }
            self.pending.clear();
        }
    }
}

/// The processing for one open microphone.
pub struct Processor {
    apm: AudioProcessing,
    vad: VoiceActivityDetectorWrapper,
    scratch: Vec<f32>,
    scaled: Vec<f32>,
}

impl std::fmt::Debug for Processor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Processor").finish_non_exhaustive()
    }
}

/// The pipeline's settings: everything a browser turns on for a call.
fn config() -> Config {
    Config {
        high_pass_filter: Some(HighPassFilter::default()),
        echo_canceller: Some(EchoCanceller::default()),
        noise_suppression: Some(NoiseSuppression {
            level: NoiseSuppressionLevel::High,
            ..NoiseSuppression::default()
        }),
        gain_controller2: Some(GainController2 {
            // The system's microphone volume is the user's; only the
            // digital gain adapts.
            input_volume_controller: false,
            adaptive_digital: Some(AdaptiveDigital::default()),
            ..GainController2::default()
        }),
        ..Config::default()
    }
}

impl Processor {
    /// A fresh pipeline, starting from a delay of `delay_ms` between what
    /// is played and its echo.
    pub fn new(delay_ms: i32) -> Self {
        let stream = StreamConfig::new(CLOCK, 1);
        let mut apm = AudioProcessing::builder()
            .config(config())
            .capture_config(stream)
            .render_config(stream)
            .build();
        // Clamped to 0..=500; out of range is still processed.
        let _ = apm.set_stream_delay_ms(delay_ms);
        let rate = i32::try_from(CLOCK).unwrap_or(48_000);
        Self {
            apm,
            vad: VoiceActivityDetectorWrapper::new(sonora_simd::detect_backend(), rate),
            scratch: vec![0.0; TEN_MS],
            scaled: vec![0.0; TEN_MS],
        }
    }

    /// Updates the delay guess, as the device reports its latency.
    pub fn set_delay(&mut self, delay_ms: i32) {
        let _ = self.apm.set_stream_delay_ms(delay_ms);
    }

    /// Takes 10 ms of what was played.
    pub fn render(&mut self, frame: &[f32]) {
        if frame.len() != TEN_MS {
            return;
        }
        let _ = self
            .apm
            .process_render_f32(&[frame], &mut [&mut self.scratch[..]]);
    }

    /// Cleans 10 ms of the microphone in place; returns how likely it is
    /// speech, 0 to 1.
    pub fn capture(&mut self, frame: &mut [f32]) -> f32 {
        if frame.len() != TEN_MS {
            return 0.0;
        }
        if let Err(error) = self
            .apm
            .process_capture_f32(&[&*frame], &mut [&mut self.scratch[..]])
        {
            log::debug!("huddle microphone: processing: {error}");
            return 0.0;
        }
        frame.copy_from_slice(&self.scratch);
        // The detector works in 16-bit units, as AGC2 feeds it.
        for (scaled, sample) in self.scaled.iter_mut().zip(frame.iter()) {
            *scaled = sample * 32_768.0;
        }
        self.vad.analyze(&self.scaled)
    }

    /// Echo return loss enhancement and the delay AEC3 found, for the log.
    pub fn echo_stats(&self) -> (Option<f64>, Option<i32>) {
        let stats = self.apm.statistics();
        (stats.echo_return_loss_enhancement, stats.delay_ms)
    }
}

#[cfg(test)]
mod tests {
    use super::super::uplink::{Tone, level};
    use super::*;

    #[test]
    fn a_44_1_khz_stereo_device_comes_out_48_khz_mono() {
        let mut convert = ToMono48k::new(44_100, 2).expect("a converter");
        // One second of a 1 kHz tone, left and right the same, fed in
        // uneven pieces as a device does.
        let rate = 44_100.0;
        let input: Vec<f32> = (0..44_100)
            .flat_map(|n| {
                let s = 0.5 * (std::f32::consts::TAU * 1000.0 * n as f32 / rate).sin();
                [s, s]
            })
            .collect();
        let mut out = Vec::new();
        for piece in input.chunks(2 * 317) {
            convert.push(piece, |frame| {
                assert_eq!(frame.len(), TEN_MS);
                out.extend_from_slice(frame);
            });
        }
        assert_eq!(out.len(), 48_000, "100 frames of 10 ms");
        // The pitch holds: 1000 rising zero crossings in a second, give
        // or take the resampler's start.
        let crossings = out[4800..]
            .windows(2)
            .filter(|w| w[0] < 0.0 && w[1] >= 0.0)
            .count();
        assert!((898..=902).contains(&crossings), "{crossings}");
        // And the level: a 0.5 sine is 9 dB under full scale.
        assert_eq!(level(&out[4800..]), 9);
    }

    #[test]
    fn a_48_khz_device_is_only_framed_and_mixed_down() {
        let mut convert = ToMono48k::new(48_000, 2).expect("a converter");
        let input: Vec<f32> = (0..TEN_MS * 2).flat_map(|_| [0.2, 0.4]).collect();
        let mut frames = 0;
        convert.push(&input, |frame| {
            frames += 1;
            assert!(frame.iter().all(|s| (s - 0.3).abs() < 1e-6));
        });
        assert_eq!(frames, 2);
        assert!(ToMono48k::new(22_050, 1).is_err());
        assert!(ToMono48k::new(48_000, 0).is_err());
    }

    #[test]
    fn the_tap_keeps_frames_only_while_a_microphone_is_open() {
        let tap = RenderTap::default();
        tap.push(&[0.5; TEN_MS * 2], 2);
        assert!(tap.take().is_empty(), "off: nothing kept");
        tap.set_on(true);
        // Stereo in, mono frames out, across calls.
        tap.push(&[0.5; TEN_MS], 2);
        assert!(tap.take().is_empty(), "half a frame so far");
        tap.push(&[0.5; TEN_MS * 3], 2);
        let frames = tap.take();
        assert_eq!(frames.len(), 2);
        assert!(frames[0].iter().all(|s| (s - 0.5).abs() < 1e-6));
        // Never more than half a second.
        tap.push(&vec![0.1; TEN_MS * 2 * 80], 2);
        assert_eq!(tap.take().len(), RENDER_HELD);
        tap.set_on(false);
        tap.push(&[0.5; TEN_MS * 2], 2);
        assert!(tap.take().is_empty());
    }

    /// A deterministic noise, for speech-like energy in the near end.
    fn noise(seed: &mut u32) -> f32 {
        *seed ^= *seed << 13;
        *seed ^= *seed >> 17;
        *seed ^= *seed << 5;
        (*seed as f32 / u32::MAX as f32) * 2.0 - 1.0
    }

    /// The far end plays a tone; the microphone hears it 30 ms later at
    /// half the level, plus a quieter voice of its own (bursts of noise).
    /// Once AEC3 has converged, much less of the tone is left.
    #[test]
    fn echo_of_the_far_end_is_cancelled() {
        let delay = 30 * TEN_MS / 10;
        let mut processor = Processor::new(30);
        let mut tone = Tone::new(300.0, 0.3);
        let mut played: Vec<f32> = vec![0.0; delay];
        let mut seed = 0x1234_5678;
        let (mut echo_in, mut echo_out) = (0.0f32, 0.0f32);
        let seconds = 6;
        for n in 0..seconds * 100 {
            let mut far = vec![0.0; TEN_MS];
            tone.fill(&mut far);
            // A changing far end, as speech is: the tone pauses now and
            // then.
            if (n / 37) % 4 == 3 {
                far.iter_mut().for_each(|s| *s *= 0.1);
            }
            played.extend_from_slice(&far);
            processor.render(&far);
            // What the microphone picks up now: the echo of what played
            // `delay` samples ago.
            let echo: Vec<f32> = played.drain(..TEN_MS).map(|s| 0.5 * s).collect();
            // The near end talks in the first half only; the second half
            // is echo alone, where the measuring happens.
            let talking = n < seconds * 50 && (n / 25) % 2 == 0;
            let mut mic: Vec<f32> = echo
                .iter()
                .map(|e| {
                    let voice = if talking {
                        0.05 * noise(&mut seed)
                    } else {
                        0.0
                    };
                    e + voice
                })
                .collect();
            processor.capture(&mut mic);
            if n >= seconds * 75 {
                echo_in += echo.iter().map(|s| s * s).sum::<f32>();
                echo_out += mic.iter().map(|s| s * s).sum::<f32>();
            }
        }
        // About 54 dB here; noise suppression and gain control alone,
        // without AEC3, take off about 6.
        let reduction_db = 10.0 * (echo_in / echo_out.max(1e-12)).log10();
        assert!(reduction_db > 20.0, "only {reduction_db:.1} dB less echo");
        let (_, found) = processor.echo_stats();
        log::debug!("AEC3 found a delay of {found:?} ms");
    }

    #[test]
    fn silence_is_not_speech() {
        let mut processor = Processor::new(0);
        let mut quiet = 0.0f32;
        for _ in 0..100 {
            let mut frame = vec![0.0; TEN_MS];
            quiet = quiet.max(processor.capture(&mut frame));
        }
        assert!(quiet < 0.5, "{quiet}");
    }
}
