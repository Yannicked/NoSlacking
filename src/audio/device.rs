//! The thread that plays: it decodes with symphonia (through rodio) and
//! writes to the system's sound device through cpal.
//!
//! The device is opened when a sound starts and let go of when it stops
//! or ends, so an idle app holds no audio stream open. The thread itself
//! starts with the first sound and stops with the app.

use std::io::Cursor;
use std::sync::mpsc;
use std::time::Duration;

use super::{Bytes, Order, Report, TICK, Why};
use crate::backend::Waker;

/// The interface's end of the playing thread.
pub struct Device {
    waker: Waker,
    /// Orders for the thread, once it has started.
    orders: Option<mpsc::Sender<Order>>,
    reports: (mpsc::Sender<Report>, mpsc::Receiver<Report>),
}

impl std::fmt::Debug for Device {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Device")
            .field("started", &self.orders.is_some())
            .finish_non_exhaustive()
    }
}

impl Device {
    /// A device whose reports wake the window through `waker`.
    pub fn new(waker: Waker) -> Self {
        Self {
            waker,
            orders: None,
            reports: mpsc::channel(),
        }
    }

    /// Passes `order` to the thread, starting it first if need be. A
    /// thread that will not start is a sound that fails, as without a
    /// device.
    pub fn send(&mut self, order: Order) {
        if self.orders.is_none() {
            if matches!(order, Order::Stop) {
                return;
            }
            let (orders, inbox) = mpsc::channel();
            let reports = self.reports.0.clone();
            let waker = self.waker.clone();
            let spawned = std::thread::Builder::new()
                .name("noslacking-audio".into())
                .spawn(move || run(&inbox, &Out { reports, waker }));
            match spawned {
                Ok(_) => self.orders = Some(orders),
                Err(error) => {
                    log::warn!("could not start the audio thread: {error}");
                    if let Order::Load { id, .. } = order {
                        let _ = self.reports.0.send(Report::Failed {
                            id,
                            why: Why::NoDevice,
                        });
                    }
                    return;
                }
            }
        }
        if let Some(orders) = &self.orders
            && orders.send(order).is_err()
        {
            log::warn!("the audio thread has stopped");
            self.orders = None;
        }
    }

    /// The thread's next report, if it has one.
    pub fn try_recv(&self) -> Option<Report> {
        self.reports.1.try_recv().ok()
    }
}

/// Where the thread's reports go.
struct Out {
    reports: mpsc::Sender<Report>,
    waker: Waker,
}

impl Out {
    fn send(&self, report: Report) {
        let _ = self.reports.send(report);
        self.waker.wake();
    }
}

/// The sound in hand on the thread.
struct Loaded {
    id: u64,
    bytes: Bytes,
    ext: String,
    mimetype: String,
    /// The device and what plays on it, while it plays or is paused;
    /// let go of once it ends.
    output: Option<(rodio::MixerDeviceSink, rodio::Player)>,
}

/// Takes orders until the app goes, telling how far a playing sound got
/// every [`TICK`].
fn run(inbox: &mpsc::Receiver<Order>, out: &Out) {
    let mut loaded: Option<Loaded> = None;
    loop {
        let playing = loaded
            .as_ref()
            .and_then(|l| l.output.as_ref())
            .is_some_and(|(_, player)| !player.is_paused());
        let order = if playing {
            match inbox.recv_timeout(TICK) {
                Ok(order) => Some(order),
                Err(mpsc::RecvTimeoutError::Timeout) => None,
                Err(mpsc::RecvTimeoutError::Disconnected) => return,
            }
        } else {
            match inbox.recv() {
                Ok(order) => Some(order),
                Err(_) => return,
            }
        };
        match order {
            None => tick(&mut loaded, out),
            Some(Order::Load {
                id,
                bytes,
                ext,
                mimetype,
            }) => {
                loaded = None;
                let mut sound = Loaded {
                    id,
                    bytes,
                    ext,
                    mimetype,
                    output: None,
                };
                match sound.open() {
                    Ok(duration) => {
                        out.send(Report::Loaded { id, duration });
                        loaded = Some(sound);
                    }
                    Err(why) => out.send(Report::Failed { id, why }),
                }
            }
            Some(Order::Play) => {
                if let Some(sound) = &mut loaded {
                    if sound.output.is_none()
                        && let Err(why) = sound.open()
                    {
                        out.send(Report::Failed { id: sound.id, why });
                        loaded = None;
                        continue;
                    }
                    if let Some((_, player)) = &sound.output {
                        player.play();
                    }
                }
            }
            Some(Order::Pause) => {
                if let Some((_, player)) = loaded.as_ref().and_then(|l| l.output.as_ref()) {
                    player.pause();
                }
            }
            Some(Order::Seek(position)) => {
                if let Some(sound) = &mut loaded {
                    sound.seek(position, out);
                }
            }
            Some(Order::Stop) => loaded = None,
        }
    }
}

/// Says how far the playing sound got, or that it ended.
fn tick(loaded: &mut Option<Loaded>, out: &Out) {
    let Some(sound) = loaded else { return };
    let Some((_, player)) = &sound.output else {
        return;
    };
    if player.empty() {
        // Let go of the device; the bytes stay for playing it again.
        sound.output = None;
        out.send(Report::Ended { id: sound.id });
    } else {
        out.send(Report::Position {
            id: sound.id,
            position: player.get_pos(),
        });
    }
}

impl Loaded {
    /// Decodes the sound and starts it on the default device, from the
    /// start. Returns how long it lasts, if the decoder knows.
    fn open(&mut self) -> Result<Option<Duration>, Why> {
        use rodio::Source as _;
        let source = decode(&self.bytes, &self.ext, &self.mimetype).map_err(|error| {
            log::warn!("could not decode a sound ({}): {error}", self.ext);
            Why::Unreadable
        })?;
        let duration = source.total_duration();
        let mut sink = rodio::DeviceSinkBuilder::open_default_sink().map_err(|error| {
            log::warn!("no sound device: {error}");
            Why::NoDevice
        })?;
        sink.log_on_drop(false);
        let player = rodio::Player::connect_new(sink.mixer());
        player.append(source);
        self.output = Some((sink, player));
        Ok(duration)
    }

    /// Moves to `position`, opening the sound again (paused) if it had
    /// ended, so the next play starts there.
    fn seek(&mut self, position: Duration, out: &Out) {
        if self.output.is_none() {
            if let Err(why) = self.open() {
                out.send(Report::Failed { id: self.id, why });
                return;
            }
            if let Some((_, player)) = &self.output {
                player.pause();
            }
        }
        if let Some((_, player)) = &self.output {
            if let Err(error) = player.try_seek(position) {
                log::warn!("could not seek: {error}");
            }
            out.send(Report::Position {
                id: self.id,
                position: player.get_pos(),
            });
        }
    }
}

/// A decoder for `bytes`, guided by its extension and type.
pub(super) fn decode(
    bytes: &Bytes,
    ext: &str,
    mimetype: &str,
) -> Result<rodio::Decoder<Cursor<Bytes>>, rodio::decoder::DecoderError> {
    let len = bytes.0.len() as u64;
    let mut builder = rodio::Decoder::builder()
        .with_data(Cursor::new(bytes.clone()))
        .with_byte_len(len)
        .with_seekable(true);
    if !ext.is_empty() {
        builder = builder.with_hint(ext);
    }
    if !mimetype.is_empty() {
        builder = builder.with_mime_type(mimetype);
    }
    builder.build()
}

#[cfg(test)]
mod tests {
    use super::*;
    use rodio::Source as _;

    /// Decodes `bytes` fully and returns how long it says it lasts and
    /// how many samples came out.
    fn decoded(bytes: &[u8], ext: &str, mimetype: &str) -> (Option<Duration>, usize) {
        let source = decode(&Bytes::from(bytes.to_vec()), ext, mimetype).expect("decodes");
        let duration = source.total_duration();
        (duration, source.count())
    }

    fn about_half_a_second(duration: Option<Duration>) {
        let ms = duration.expect("a known length").as_millis();
        assert!((400..=700).contains(&ms), "{ms} ms");
    }

    #[test]
    fn a_generated_wav_decodes() {
        let bytes = super::super::wav(8000, &super::super::beeps(8000, 0.5));
        let (duration, samples) = decoded(&bytes, "wav", "audio/wav");
        about_half_a_second(duration);
        assert_eq!(samples, 4000);
    }

    #[test]
    fn every_supported_container_decodes() {
        for (bytes, ext, mimetype) in [
            (&include_bytes!("fixtures/tone.m4a")[..], "m4a", "audio/mp4"),
            (
                &include_bytes!("fixtures/tone.mp3")[..],
                "mp3",
                "audio/mpeg",
            ),
            (&include_bytes!("fixtures/tone.ogg")[..], "ogg", "audio/ogg"),
            (
                &include_bytes!("fixtures/tone.flac")[..],
                "flac",
                "audio/flac",
            ),
        ] {
            let source = decode(&Bytes::from(bytes.to_vec()), ext, mimetype)
                .unwrap_or_else(|error| panic!("{ext}: {error}"));
            let rate = source.sample_rate().get();
            let samples = source.count();
            // Half a second of a tone, give or take an encoder's padding.
            let seconds = samples as f32 / rate as f32;
            assert!((0.4..=0.8).contains(&seconds), "{ext}: {seconds} s");
        }
    }

    #[test]
    fn garbage_does_not_decode() {
        let bytes = Bytes::from(b"definitely not a sound file".to_vec());
        assert!(decode(&bytes, "m4a", "audio/mp4").is_err());
        assert!(decode(&Bytes::from(Vec::new()), "", "").is_err());
    }
}
