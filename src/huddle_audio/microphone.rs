//! The microphone, opened only while unmuted.
//!
//! The rule this module keeps: the device is not opened until the user
//! unmutes, and is closed (its stream dropped, its thread joined) when
//! they mute or leave. [`MicControl`] is that rule as a state machine over
//! any [`Microphone`], so tests can hold it to it with a pretend device.
//!
//! The real one, [`Cpal`], opens the default input device through cpal
//! (rodio's) on a thread of its own, which also runs the pipeline:
//! the device's samples to 48 kHz mono ([`ToMono48k`]), 10 ms at a time
//! through echo cancellation and the rest ([`Processor`]), joined into
//! 20 ms ([`Framer`]), encoded ([`Encoder`]), thinned by DTX ([`Dtx`]) and
//! handed to the session as [`Outgoing`] frames. The device's own callback
//! only copies samples out; nothing there waits.
//!
//! [`ToneSource`] is the probe's stand-in for a microphone: the same
//! encoder fed a quiet tone at the real-time pace.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use tokio::sync::mpsc;

use super::encoder::{Encoder, OpusRs};
use super::processing::{OUTPUT_GUESS_MS, Processor, RenderTap, ToMono48k};
use super::uplink::{Dtx, Framer, Outgoing, TWENTY_MS, Tone, level};

/// Why the microphone did not open.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MicError {
    /// There is no input device.
    NoDevice,
    /// The device would not open: in use, refused by the system's privacy
    /// settings, or a format it cannot give.
    Open(String),
}

impl std::fmt::Display for MicError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoDevice => f.write_str("no input device"),
            Self::Open(why) => write!(f, "the input device would not open: {why}"),
        }
    }
}

/// Something that can be opened to capture. What `open` returns holds the
/// device; dropping it closes it.
pub trait Microphone: Send + 'static {
    /// The open device.
    type Open: Send + 'static;
    /// Opens the device and starts capturing.
    fn open(&mut self) -> Result<Self::Open, MicError>;
}

/// The microphone as mute and unmute see it.
#[derive(Debug)]
pub struct MicControl<M: Microphone> {
    mic: M,
    open: Option<M::Open>,
}

impl<M: Microphone> MicControl<M> {
    /// Muted, with `mic` closed.
    pub fn new(mic: M) -> Self {
        Self { mic, open: None }
    }

    /// Whether the device is open.
    pub fn is_open(&self) -> bool {
        self.open.is_some()
    }

    /// Mutes (closing the device) or unmutes (opening it). A failed open
    /// leaves it muted.
    pub fn set_muted(&mut self, muted: bool) -> Result<(), MicError> {
        if muted {
            self.open = None;
        } else if self.open.is_none() {
            self.open = Some(self.mic.open()?);
        }
        Ok(())
    }
}

/// Where an open microphone's work goes.
#[derive(Clone, Debug)]
pub struct Wiring {
    /// The session's queue of frames to send.
    pub frames: mpsc::Sender<Outgoing>,
    /// What the huddle plays, for the echo canceller.
    pub render: RenderTap,
}

/// Turns 10 ms of 48 kHz mono into what is sent.
pub struct Pipeline {
    processor: Option<Processor>,
    framer: Framer,
    encoder: Box<dyn Encoder>,
    dtx: Option<Dtx>,
    /// Frames encoded, sent and left out, for the log.
    pub counts: PipelineCounts,
}

/// What a [`Pipeline`] did.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PipelineCounts {
    /// 20 ms frames encoded.
    pub encoded: u64,
    /// Of those, handed to the session.
    pub sent: u64,
    /// Left out by DTX.
    pub skipped: u64,
    /// Dropped because the session's queue was full.
    pub dropped: u64,
    /// Frames the encoder refused.
    pub failed: u64,
}

impl std::fmt::Debug for Pipeline {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Pipeline")
            .field("counts", &self.counts)
            .finish_non_exhaustive()
    }
}

/// How likely speech has to be for the detector to call it so.
const SPEECH: f32 = 0.5;
/// Loud enough to send whatever the detector thinks: -40 dBFS.
const LOUD: u8 = 40;

impl Pipeline {
    /// The microphone's: processing and DTX on.
    pub fn speech(processor: Processor, encoder: Box<dyn Encoder>) -> Self {
        Self {
            processor: Some(processor),
            framer: Framer::default(),
            encoder,
            dtx: Some(Dtx::default()),
            counts: PipelineCounts::default(),
        }
    }

    /// A test signal's: no processing, every frame sent.
    pub fn plain(encoder: Box<dyn Encoder>) -> Self {
        Self {
            processor: None,
            framer: Framer::default(),
            encoder,
            dtx: None,
            counts: PipelineCounts::default(),
        }
    }

    /// The processor, for its delay and statistics.
    pub fn processor_mut(&mut self) -> Option<&mut Processor> {
        self.processor.as_mut()
    }

    /// Takes 10 ms of the microphone after `rendered`, the far end played
    /// since the last call; returns a frame to send when one is due.
    pub fn push(&mut self, ten_ms: &mut [f32], rendered: &[Vec<f32>]) -> Option<Outgoing> {
        let speech = match &mut self.processor {
            Some(processor) => {
                for frame in rendered {
                    processor.render(frame);
                }
                let likely = processor.capture(ten_ms);
                likely >= SPEECH || level(ten_ms) <= LOUD
            }
            None => true,
        };
        let (frame, speech) = self.framer.push(ten_ms, speech)?;
        let payload = match self.encoder.encode(&frame) {
            Ok(payload) => payload,
            Err(error) => {
                self.counts.failed += 1;
                log::debug!("huddle microphone: {error}");
                return None;
            }
        };
        self.counts.encoded += 1;
        let gap = match &mut self.dtx {
            Some(dtx) => match dtx.decide(speech) {
                Some(gap) => gap,
                None => {
                    self.counts.skipped += 1;
                    return None;
                }
            },
            None => 0,
        };
        Some(Outgoing {
            payload,
            gap,
            level: level(&frame),
            voice: speech,
        })
    }

    /// Hands `frame` to the session without waiting; a full queue (the
    /// session stalled) drops it.
    pub fn send(&mut self, frames: &mpsc::Sender<Outgoing>, frame: Outgoing) -> bool {
        match frames.try_send(frame) {
            Ok(()) => {
                self.counts.sent += 1;
                true
            }
            Err(mpsc::error::TrySendError::Full(_)) => {
                self.counts.dropped += 1;
                true
            }
            Err(mpsc::error::TrySendError::Closed(_)) => false,
        }
    }
}

/// A thread that stops and is joined when dropped.
#[derive(Debug)]
pub struct Running {
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Drop for Running {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            thread.thread().unpark();
            let _ = thread.join();
        }
    }
}

/// Samples from the device's callback: interleaved, as f32, and how long
/// ago the first was captured.
struct Chunk {
    samples: Vec<f32>,
    latency: Option<Duration>,
}

/// The default input device, through cpal.
#[derive(Debug)]
pub struct Cpal {
    wiring: Wiring,
}

impl Cpal {
    /// The default input device, its frames going to `wiring`.
    pub fn new(wiring: Wiring) -> Self {
        Self { wiring }
    }
}

impl Microphone for Cpal {
    type Open = Running;

    fn open(&mut self) -> Result<Running, MicError> {
        let stop = Arc::new(AtomicBool::new(false));
        let (opened, result) = std::sync::mpsc::channel();
        let wiring = self.wiring.clone();
        let thread_stop = stop.clone();
        let thread = std::thread::Builder::new()
            .name("noslacking-huddle-mic".into())
            .spawn(move || capture(&wiring, &thread_stop, &opened))
            .map_err(|e| MicError::Open(format!("no thread: {e}")))?;
        let running = Running {
            stop,
            thread: Some(thread),
        };
        match result.recv() {
            Ok(Ok(())) => Ok(running),
            Ok(Err(error)) => Err(error),
            Err(_) => Err(MicError::Open("the microphone thread stopped".into())),
        }
    }
}

/// Builds the input stream on the default device in the format it
/// prefers, converting each callback's samples to f32.
fn open_stream(
    chunks: std::sync::mpsc::SyncSender<Chunk>,
) -> Result<(rodio::cpal::Stream, u32, u16), MicError> {
    use rodio::cpal::traits::{DeviceTrait as _, HostTrait as _, StreamTrait as _};
    use rodio::cpal::{InputCallbackInfo, SampleFormat, SizedSample};

    let host = rodio::cpal::default_host();
    let device = host.default_input_device().ok_or(MicError::NoDevice)?;
    let supported = device
        .default_input_config()
        .map_err(|e| MicError::Open(e.to_string()))?;
    // 48 kHz when the device offers it: no resampling then.
    let supported = device
        .supported_input_configs()
        .ok()
        .and_then(|mut configs| {
            configs.find(|c| {
                c.channels() == supported.channels()
                    && c.sample_format() == supported.sample_format()
                    && (c.min_sample_rate()..=c.max_sample_rate()).contains(&48_000)
            })
        })
        .map_or(supported.clone(), |c| c.with_sample_rate(48_000));
    let config = supported.config();
    let (rate, channels) = (config.sample_rate, config.channels);

    fn build<T: SizedSample + Send + 'static>(
        device: &rodio::cpal::Device,
        config: &rodio::cpal::StreamConfig,
        chunks: std::sync::mpsc::SyncSender<Chunk>,
        to_f32: fn(T) -> f32,
    ) -> Result<rodio::cpal::Stream, rodio::cpal::BuildStreamError> {
        device.build_input_stream(
            config,
            move |data: &[T], info: &InputCallbackInfo| {
                let stamp = info.timestamp();
                let latency = stamp.callback.duration_since(&stamp.capture);
                let samples = data.iter().map(|s| to_f32(*s)).collect();
                // Never wait on the device's thread: a full queue (the
                // pipeline stalled) loses this chunk.
                let _ = chunks.try_send(Chunk { samples, latency });
            },
            |error| log::warn!("huddle microphone: {error}"),
            None,
        )
    }

    let stream = match supported.sample_format() {
        SampleFormat::F32 => build::<f32>(&device, &config, chunks, |s| s),
        SampleFormat::I16 => build::<i16>(&device, &config, chunks, |s| f32::from(s) / 32_768.0),
        SampleFormat::U16 => build::<u16>(&device, &config, chunks, |s| {
            (f32::from(s) - 32_768.0) / 32_768.0
        }),
        SampleFormat::I32 => build::<i32>(&device, &config, chunks, |s| {
            (f64::from(s) / 2_147_483_648.0) as f32
        }),
        other => {
            return Err(MicError::Open(format!("sample format {other} not handled")));
        }
    }
    .map_err(|e| MicError::Open(e.to_string()))?;
    stream.play().map_err(|e| MicError::Open(e.to_string()))?;
    log::info!(
        "huddle microphone: open: {} Hz, {} channel(s), {}",
        rate,
        channels,
        supported.sample_format()
    );
    Ok((stream, rate, channels))
}

/// The microphone's thread: opens the device, says how that went, then
/// runs the pipeline until told to stop, and closes the device.
fn capture(
    wiring: &Wiring,
    stop: &AtomicBool,
    opened: &std::sync::mpsc::Sender<Result<(), MicError>>,
) {
    // A second of chunks, whatever the device's period.
    let (chunks, incoming) = std::sync::mpsc::sync_channel(100);
    let (stream, rate, channels) = match open_stream(chunks) {
        Ok(opened) => opened,
        Err(error) => {
            let _ = opened.send(Err(error));
            return;
        }
    };
    let mut convert = match ToMono48k::new(rate, channels) {
        Ok(convert) => convert,
        Err(why) => {
            let _ = opened.send(Err(MicError::Open(why)));
            return;
        }
    };
    let encoder = match OpusRs::speech() {
        Ok(encoder) => encoder,
        Err(why) => {
            let _ = opened.send(Err(MicError::Open(why)));
            return;
        }
    };
    let mut pipeline = Pipeline::speech(Processor::new(OUTPUT_GUESS_MS), Box::new(encoder));
    wiring.render.set_on(true);
    let _ = opened.send(Ok(()));

    let mut delay_ms = OUTPUT_GUESS_MS;
    let mut next_log = Instant::now() + Duration::from_secs(5);
    while !stop.load(Ordering::Relaxed) {
        let chunk = match incoming.recv_timeout(Duration::from_millis(100)) {
            Ok(chunk) => chunk,
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => continue,
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
        };
        if let Some(latency) = chunk.latency {
            let ms = i32::try_from(latency.as_millis()).unwrap_or(i32::MAX);
            let guess = ms.saturating_add(OUTPUT_GUESS_MS);
            if (guess - delay_ms).abs() > 5 {
                delay_ms = guess;
                if let Some(processor) = pipeline.processor_mut() {
                    processor.set_delay(delay_ms);
                }
            }
        }
        let mut closed = false;
        convert.push(&chunk.samples, |ten_ms| {
            let mut frame = ten_ms.to_vec();
            let rendered = wiring.render.take();
            if let Some(out) = pipeline.push(&mut frame, &rendered) {
                closed |= !pipeline.send(&wiring.frames, out);
            }
        });
        if closed {
            break;
        }
        if Instant::now() >= next_log {
            next_log += Duration::from_secs(5);
            let (erle, found) = pipeline
                .processor_mut()
                .map(|p| p.echo_stats())
                .unwrap_or_default();
            log::info!(
                "huddle microphone: {:?}; echo: delay guess {delay_ms} ms, found {found:?} ms, \
                 ERLE {erle:?} dB",
                pipeline.counts
            );
        }
    }
    wiring.render.set_on(false);
    drop(stream);
    log::info!("huddle microphone: closed; {:?}", pipeline.counts);
}

/// The probe's tone, encoded and sent as a microphone would be.
#[derive(Debug)]
pub struct ToneSource {
    _running: Running,
}

impl ToneSource {
    /// Starts sending [`Tone::probe`] to `frames`, 20 ms every 20 ms,
    /// until dropped.
    pub fn start(frames: mpsc::Sender<Outgoing>) -> Result<Self, String> {
        let mut pipeline = Pipeline::plain(Box::new(OpusRs::speech()?));
        let stop = Arc::new(AtomicBool::new(false));
        let thread_stop = stop.clone();
        let thread = std::thread::Builder::new()
            .name("noslacking-huddle-tone".into())
            .spawn(move || {
                let mut tone = Tone::probe();
                let mut due = Instant::now();
                while !thread_stop.load(Ordering::Relaxed) {
                    let mut frame = vec![0.0; TWENTY_MS];
                    tone.fill(&mut frame);
                    for half in frame.chunks_mut(TWENTY_MS / 2) {
                        if let Some(out) = pipeline.push(half, &[])
                            && !pipeline.send(&frames, out)
                        {
                            return;
                        }
                    }
                    due += Duration::from_millis(20);
                    std::thread::park_timeout(due.saturating_duration_since(Instant::now()));
                }
            })
            .map_err(|e| format!("no tone thread: {e}"))?;
        Ok(Self {
            _running: Running {
                stop,
                thread: Some(thread),
            },
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;

    /// A pretend device that counts how many are open.
    #[derive(Clone, Default)]
    struct Pretend {
        open: Arc<AtomicUsize>,
        opened: Arc<AtomicUsize>,
        refuse: Arc<AtomicBool>,
    }

    struct PretendOpen(Arc<AtomicUsize>);

    impl Drop for PretendOpen {
        fn drop(&mut self) {
            self.0.fetch_sub(1, Ordering::SeqCst);
        }
    }

    impl Microphone for Pretend {
        type Open = PretendOpen;

        fn open(&mut self) -> Result<PretendOpen, MicError> {
            if self.refuse.load(Ordering::SeqCst) {
                return Err(MicError::Open("denied".into()));
            }
            self.opened.fetch_add(1, Ordering::SeqCst);
            self.open.fetch_add(1, Ordering::SeqCst);
            Ok(PretendOpen(self.open.clone()))
        }
    }

    #[test]
    fn the_microphone_is_open_only_while_unmuted() {
        let device = Pretend::default();
        let mut control = MicControl::new(device.clone());
        // Joined muted: never opened.
        assert_eq!(device.opened.load(Ordering::SeqCst), 0);
        control.set_muted(true).expect("muted");
        assert_eq!(device.opened.load(Ordering::SeqCst), 0);
        assert!(!control.is_open());

        control.set_muted(false).expect("unmuted");
        assert_eq!(device.open.load(Ordering::SeqCst), 1);
        // Unmuting again opens nothing more.
        control.set_muted(false).expect("unmuted");
        assert_eq!(device.opened.load(Ordering::SeqCst), 1);

        control.set_muted(true).expect("muted");
        assert_eq!(device.open.load(Ordering::SeqCst), 0, "closed on mute");
        assert!(!control.is_open());

        control.set_muted(false).expect("unmuted");
        assert_eq!(device.open.load(Ordering::SeqCst), 1);
        drop(control);
        assert_eq!(device.open.load(Ordering::SeqCst), 0, "closed on leave");
        assert_eq!(device.opened.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn a_refused_microphone_stays_muted() {
        let device = Pretend::default();
        device.refuse.store(true, Ordering::SeqCst);
        let mut control = MicControl::new(device.clone());
        assert_eq!(
            control.set_muted(false),
            Err(MicError::Open("denied".into()))
        );
        assert!(!control.is_open());
        assert_eq!(device.open.load(Ordering::SeqCst), 0);
    }

    /// An encoder that says which frame it was given.
    struct Counting(u8);

    impl Encoder for Counting {
        fn encode(&mut self, frame: &[f32]) -> Result<Vec<u8>, String> {
            assert_eq!(frame.len(), TWENTY_MS);
            self.0 = self.0.wrapping_add(1);
            Ok(vec![0xF8, self.0])
        }
    }

    #[test]
    fn a_plain_pipeline_sends_every_twenty_ms() {
        let mut pipeline = Pipeline::plain(Box::new(Counting(0)));
        let mut out = Vec::new();
        for _ in 0..6 {
            let mut ten = vec![0.1; TWENTY_MS / 2];
            out.extend(pipeline.push(&mut ten, &[]));
        }
        assert_eq!(out.len(), 3);
        assert!(out.iter().all(|o| o.gap == 0 && o.voice && o.level == 20));
        assert_eq!(out[2].payload, vec![0xF8, 3]);
    }

    #[test]
    fn a_quiet_microphone_is_thinned_by_dtx() {
        let mut pipeline = Pipeline::speech(Processor::new(0), Box::new(Counting(0)));
        let mut sent = Vec::new();
        // Two seconds of digital silence.
        for _ in 0..200 {
            let mut ten = vec![0.0; TWENTY_MS / 2];
            sent.extend(pipeline.push(&mut ten, &[]));
        }
        assert_eq!(pipeline.counts.encoded, 100, "every frame is encoded");
        // The hangover, then one in twenty.
        assert!(sent.len() <= 16, "{} sent", sent.len());
        assert!(sent.iter().skip(10).all(|o| o.gap == 19 && !o.voice));
        // Something loud goes at once, saying how much was left out.
        let mut loud = vec![0.0; TWENTY_MS / 2];
        super::super::uplink::Tone::new(500.0, 0.5).fill(&mut loud);
        let mut next = None;
        for _ in 0..2 {
            let mut ten = loud.clone();
            next = next.or(pipeline.push(&mut ten, &[]));
        }
        let next = next.expect("sent");
        assert!(next.gap > 0 && next.voice);
    }

    #[tokio::test]
    async fn the_tone_source_sends_opus_in_real_time() {
        let (frames, mut incoming) = mpsc::channel(50);
        let source = ToneSource::start(frames).expect("started");
        let first = incoming.recv().await.expect("a frame");
        assert_eq!(
            super::super::jitter::opus_samples(&first.payload),
            Some(960)
        );
        assert_eq!(first.gap, 0);
        assert_eq!(first.level, 23);
        drop(source);
        // Its thread is gone: the queue closes once drained.
        while incoming.recv().await.is_some() {}
    }
}
