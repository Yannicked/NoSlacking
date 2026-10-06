//! Opus out: 20 ms of 48 kHz mono in, one packet out.
//!
//! The codec is `opus-rs`, a pure Rust port of libopus 1.6 (BSD-3-Clause,
//! no C library to build or find). It sits behind [`Encoder`] so libopus
//! (the `opus` crate) can stand in should it ever sound wrong.
//!
//! Settings are a browser's for speech: VOIP (SILK and CELT together, up
//! to 20 kHz of audio), about 32 kbit/s, variable bitrate, tuned for 10 %
//! loss. `opus-rs` has no DTX of its own; [`super::uplink::Dtx`] leaves
//! quiet frames out instead.
//!
//! In-band FEC is off, where a browser has it on: with it, `opus-rs`
//! 0.1.34's VOIP packets at 48 kHz decode wrong whenever the sound is
//! voiced (a tone, a vowel), up to five times the energy put in, the same
//! in `opus-decoder` and in `opus-rs`'s own decoder, so the encoder's
//! low-bitrate copies are at fault. Noise came through; speech would not.
//! `encoded_speech_decodes_back_with_both_decoders` would catch it coming
//! back. Losses are concealed by the far end's decoder instead.
//!
//! Playing stays with `opus-decoder` ([`super::speaker`]): `opus-rs`'s
//! decoder conceals a lost frame, but it cannot decode the FEC copy a
//! packet carries of the one before, which `opus-decoder` can.

use super::jitter::CLOCK;
use super::uplink::TWENTY_MS;

/// The bitrate asked of the encoder.
pub const BITRATE: i32 = 32_000;
/// The loss the encoder plans for, in percent: SILK leans less on the
/// frames before, so a lost one hurts the next ones less.
const EXPECTED_LOSS: i32 = 10;
/// Room for one packet: RFC 6716's most is 1275 bytes.
const MOST: usize = 1276;

/// Something that turns 20 ms of speech into an Opus packet.
pub trait Encoder: Send {
    /// Encodes `frame`: 960 samples of 48 kHz mono, in -1..1.
    fn encode(&mut self, frame: &[f32]) -> Result<Vec<u8>, String>;
}

/// `opus-rs`'s encoder, set up for speech.
pub struct OpusRs {
    inner: opus_rs::OpusEncoder,
    out: Vec<u8>,
}

impl std::fmt::Debug for OpusRs {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OpusRs").finish_non_exhaustive()
    }
}

impl OpusRs {
    /// The speech encoder: mono, VOIP, [`BITRATE`], no FEC (see above).
    pub fn speech() -> Result<Self, String> {
        let rate = i32::try_from(CLOCK).map_err(|e| e.to_string())?;
        let mut inner = opus_rs::OpusEncoder::new(rate, 1, opus_rs::Application::Voip)
            .map_err(|e| format!("no Opus encoder: {e}"))?;
        inner.bitrate_bps = BITRATE;
        inner.use_cbr = false;
        inner.use_inband_fec = false;
        inner.packet_loss_perc = EXPECTED_LOSS;
        Ok(Self {
            inner,
            out: vec![0; MOST],
        })
    }
}

impl Encoder for OpusRs {
    fn encode(&mut self, frame: &[f32]) -> Result<Vec<u8>, String> {
        if frame.len() != TWENTY_MS {
            return Err(format!("{} samples, not {TWENTY_MS}", frame.len()));
        }
        let n = self
            .inner
            .encode(frame, TWENTY_MS, &mut self.out)
            .map_err(|e| format!("Opus: {e}"))?;
        Ok(self.out[..n].to_vec())
    }
}

#[cfg(test)]
mod tests {
    use super::super::jitter::opus_samples;
    use super::super::uplink::Tone;
    use super::*;

    /// The energy of `a` that `b` does not match, against `a`'s own, at
    /// the lag (up to `max_lag`) where they match best: the codec delays.
    fn residual(a: &[f32], b: &[f32], max_lag: usize) -> f32 {
        let energy: f32 = a.iter().map(|s| s * s).sum();
        (0..max_lag)
            .map(|lag| {
                a.iter()
                    .zip(&b[lag..])
                    .map(|(x, y)| (x - y) * (x - y))
                    .sum::<f32>()
                    / energy
            })
            .fold(f32::MAX, f32::min)
    }

    /// Encodes a second of a tone in 20 ms frames.
    fn encoded(hz: f32) -> (Vec<f32>, Vec<Vec<u8>>) {
        let mut encoder = OpusRs::speech().expect("an encoder");
        let mut tone = Tone::new(hz, 0.3);
        let mut pcm = Vec::new();
        let mut packets = Vec::new();
        for _ in 0..50 {
            let mut frame = vec![0.0; TWENTY_MS];
            tone.fill(&mut frame);
            packets.push(encoder.encode(&frame).expect("encoded"));
            pcm.extend(frame);
        }
        (pcm, packets)
    }

    #[test]
    fn packets_are_twenty_ms_and_near_the_bitrate() {
        let (_, packets) = encoded(440.0);
        assert!(packets.iter().all(|p| opus_samples(p) == Some(960)));
        let bytes: usize = packets.iter().map(Vec::len).sum();
        // One second: about 4000 bytes at 32 kbit/s; VBR on a tone is
        // well under, never far over.
        assert!((300..=6000).contains(&bytes), "{bytes} bytes");
        assert!(
            OpusRs::speech()
                .expect("an encoder")
                .encode(&[0.0; 10])
                .is_err()
        );
    }

    /// What `opus-rs` encodes, the decoder that plays huddles
    /// (`opus-decoder`) and `opus-rs`'s own both decode to the tone.
    #[test]
    fn encoded_speech_decodes_back_with_both_decoders() {
        let (pcm, packets) = encoded(440.0);

        let mut ours = opus_decoder::OpusDecoder::new(CLOCK, 1).expect("a decoder");
        let mut theirs = opus_rs::OpusDecoder::new(48_000, 1).expect("a decoder");
        let mut by_ours = Vec::new();
        let mut by_theirs = Vec::new();
        for packet in &packets {
            let mut out = vec![0.0; ours.max_frame_size_per_channel()];
            let n = ours.decode_float(packet, &mut out, false).expect("decoded");
            by_ours.extend_from_slice(&out[..n]);
            let mut out = vec![0.0; TWENTY_MS];
            let n = theirs.decode(packet, TWENTY_MS, &mut out).expect("decoded");
            by_theirs.extend_from_slice(&out[..n]);
        }
        assert_eq!(by_ours.len(), pcm.len());
        assert_eq!(by_theirs.len(), pcm.len());
        // Past the first 100 ms, where the encoder settles; the codec
        // delays by a few milliseconds.
        let settled = &pcm[4800..pcm.len() - 960];
        assert!(residual(settled, &by_ours[4800..], 960) < 0.1);
        assert!(residual(settled, &by_theirs[4800..], 960) < 0.1);
        // Both decoders agree with each other too.
        assert!(residual(&by_ours[4800..40_000], &by_theirs[4800..], 64) < 0.05);
    }
}
