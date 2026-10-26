//! A capture of what a call's speaker is given and plays, for finding
//! faults in the sound offline: opt in by naming a directory in
//! `NOSLACKING_AUDIO_CAPTURE`. Off otherwise, and never on by itself,
//! since it holds what was said.
//!
//! Three files go in the directory, written on a thread of their own so
//! the sound device's thread never waits on the disk:
//!
//! - `packets.bin`: each Opus packet as it came, as microseconds since
//!   the capture began (u64), its RTP timestamp (u32) and its length
//!   (u16), little-endian, then its bytes;
//! - `pulls.txt`: what the jitter buffer gave for each pull, by the
//!   samples per channel played before it: `frame <bytes>`, `lost` or
//!   `silence`;
//! - `played.f32`: the sound as handed to the device, 48 kHz stereo,
//!   interleaved little-endian `f32`.

use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::Instant;

use super::jitter::Pull;

/// The variable naming the directory to capture into.
pub const VARIABLE: &str = "NOSLACKING_AUDIO_CAPTURE";

/// One thing to write.
enum Item {
    Packet {
        at: Instant,
        timestamp: u32,
        payload: Vec<u8>,
    },
    Pull(String),
    Played(Vec<f32>),
}

/// The capture's end the speaker writes to.
#[derive(Debug)]
pub struct Capture {
    items: mpsc::Sender<Item>,
}

impl Capture {
    /// A capture into the directory `NOSLACKING_AUDIO_CAPTURE` names, if
    /// it names one that can be written to.
    pub fn from_env() -> Option<Self> {
        let dir = PathBuf::from(std::env::var_os(VARIABLE)?);
        match Self::start(&dir) {
            Ok(capture) => {
                log::info!(
                    "huddle audio: capturing the far end's sound in {}",
                    dir.display()
                );
                Some(capture)
            }
            Err(error) => {
                log::warn!("huddle audio: no capture in {}: {error}", dir.display());
                None
            }
        }
    }

    fn start(dir: &Path) -> std::io::Result<Self> {
        std::fs::create_dir_all(dir)?;
        let open = |name: &str| File::create(dir.join(name)).map(BufWriter::new);
        let mut files = Files {
            packets: open("packets.bin")?,
            pulls: open("pulls.txt")?,
            played: open("played.f32")?,
            start: Instant::now(),
            samples: 0,
        };
        let (items, received) = mpsc::channel();
        std::thread::Builder::new()
            .name("audio capture".into())
            .spawn(move || {
                for item in received {
                    if let Err(error) = files.write(item) {
                        log::warn!("huddle audio: the capture stopped: {error}");
                        return;
                    }
                }
                let _ = files.flush();
            })?;
        Ok(Self { items })
    }

    /// A packet as it came.
    pub fn packet(&self, timestamp: u32, payload: &[u8]) {
        let _ = self.items.send(Item::Packet {
            at: Instant::now(),
            timestamp,
            payload: payload.to_vec(),
        });
    }

    /// What the jitter buffer gave.
    pub fn pull(&self, pull: &Pull) {
        let text = match pull {
            Pull::Frame(payload) => format!("frame {}", payload.len()),
            Pull::Lost => "lost".to_owned(),
            Pull::Recover(next) => format!("recover {}", next.len()),
            Pull::Silence => "silence".to_owned(),
        };
        let _ = self.items.send(Item::Pull(text));
    }

    /// Sound as handed to the device, interleaved stereo.
    pub fn played(&self, samples: &[f32]) {
        let _ = self.items.send(Item::Played(samples.to_vec()));
    }
}

/// The files, on the capture's thread.
struct Files {
    packets: BufWriter<File>,
    pulls: BufWriter<File>,
    played: BufWriter<File>,
    start: Instant,
    /// Samples per channel played so far.
    samples: u64,
}

impl Files {
    fn write(&mut self, item: Item) -> std::io::Result<()> {
        match item {
            Item::Packet {
                at,
                timestamp,
                payload,
            } => {
                let micros =
                    u64::try_from(at.duration_since(self.start).as_micros()).unwrap_or(u64::MAX);
                let length = u16::try_from(payload.len()).unwrap_or(u16::MAX);
                self.packets.write_all(&micros.to_le_bytes())?;
                self.packets.write_all(&timestamp.to_le_bytes())?;
                self.packets.write_all(&length.to_le_bytes())?;
                self.packets.write_all(&payload[..usize::from(length)])?;
            }
            Item::Pull(text) => writeln!(self.pulls, "{} {text}", self.samples)?,
            Item::Played(samples) => {
                self.samples += (samples.len() / 2) as u64;
                for sample in samples {
                    self.played.write_all(&sample.to_le_bytes())?;
                }
            }
        }
        Ok(())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.packets.flush()?;
        self.pulls.flush()?;
        self.played.flush()
    }
}
