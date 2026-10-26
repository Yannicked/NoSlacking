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
//! In-band FEC is on, as a browser has it: each packet carries a
//! low-bitrate copy of the one before, which the far end's decoder (libopus
//! in Chime and Teams) plays when that one is lost. `opus-rs` before 0.1.36
//! got the copies wrong, so voiced sound decoded at up to five times the
//! energy put in; `encoded_speech_decodes_back` would catch that coming
//! back.
//!
//! `opus-rs` decodes what we hear too ([`super::speaker`]), and plays the
//! far end's FEC copies the same way.

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
    /// The speech encoder: mono, VOIP, [`BITRATE`], in-band FEC (see above).
    pub fn speech() -> Result<Self, String> {
        let rate = i32::try_from(CLOCK).map_err(|e| e.to_string())?;
        let mut inner = opus_rs::OpusEncoder::new(rate, 1, opus_rs::Application::Voip)
            .map_err(|e| format!("no Opus encoder: {e}"))?;
        inner.bitrate_bps = BITRATE;
        inner.use_cbr = false;
        inner.use_inband_fec = true;
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
    fn packets_carry_a_copy_of_the_one_before() {
        let (_, with) = encoded(440.0);
        let mut encoder = OpusRs::speech().expect("an encoder");
        encoder.inner.use_inband_fec = false;
        let mut tone = Tone::new(440.0, 0.3);
        let mut without = 0;
        for _ in 0..50 {
            let mut frame = vec![0.0; TWENTY_MS];
            tone.fill(&mut frame);
            without += encoder.encode(&frame).expect("encoded").len();
        }
        let with: usize = with.iter().map(Vec::len).sum();
        assert!(
            with > without + 50 * 3,
            "{with} bytes with FEC, {without} without"
        );
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

    /// What `opus-rs` encodes decodes back to the tone.
    #[test]
    fn encoded_speech_decodes_back() {
        let (pcm, packets) = encoded(440.0);
        let mut decoder = opus_rs::OpusDecoder::new(48_000, 1).expect("a decoder");
        let mut decoded = Vec::new();
        for packet in &packets {
            let mut out = vec![0.0; TWENTY_MS];
            let n = decoder
                .decode(packet, TWENTY_MS, &mut out)
                .expect("decoded");
            decoded.extend_from_slice(&out[..n]);
        }
        assert_eq!(decoded.len(), pcm.len());
        // Past the first 100 ms, where the encoder settles; the codec
        // delays by a few milliseconds.
        let settled = &pcm[4800..pcm.len() - 960];
        assert!(residual(settled, &decoded[4800..], 960) < 0.1);
    }
}
