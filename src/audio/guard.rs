//! Keeping a decoder's panic off the sound device's thread.
//!
//! cpal runs a stream's callback, and with it every rodio source that
//! plays, on a thread of its own. A panic there ends that thread: the
//! sound stops for good while everything else carries on as if it played.
//! Worse, on Linux, dropping the stream afterwards panics too: cpal 0.17's
//! ALSA host wakes its thread by writing to a pipe whose read end the
//! thread owned, and asserts that the write worked (`host/alsa/mod.rs`,
//! `TriggerSender::wakeup`; cpal 0.18 keeps the read end in the stream,
//! but rodio 0.22 is on 0.17). That second panic lands on whichever
//! thread lets go of the device: leaving a huddle, or a voice clip ending.
//!
//! So decoding is wrapped in [`Health::catch`], and a device whose thread
//! is gone anyway (something else on it panicked) is leaked by [`let_go`]
//! rather than dropped. Release builds abort on any panic, which no catch
//! can stop; this is for debug builds, tests, and keeping a decoder's
//! slip a lost sound instead of a stuck one there.

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

/// What happened to the sources on one device's thread.
#[derive(Debug, Default)]
pub struct Health {
    /// Panics caught while decoding.
    caught: AtomicU64,
    /// Set when a source was dropped by a panic unwinding its thread:
    /// the device's thread is gone.
    gone: AtomicBool,
}

impl Health {
    /// How many panics were caught.
    pub fn caught(&self) -> u64 {
        self.caught.load(Ordering::Relaxed)
    }

    /// Whether the device's thread was ended by a panic.
    pub fn thread_gone(&self) -> bool {
        self.gone.load(Ordering::Relaxed)
    }

    /// Runs `work`, turning a panic in it into `None` and counting it.
    /// What `work` touched may be half-changed then; the caller starts it
    /// afresh or stops using it.
    pub fn catch<T>(&self, work: impl FnOnce() -> T) -> Option<T> {
        match catch_unwind(AssertUnwindSafe(work)) {
            Ok(value) => Some(value),
            Err(_) => {
                self.caught.fetch_add(1, Ordering::Relaxed);
                None
            }
        }
    }

    /// Notes that the device's thread is gone if this is called while a
    /// panic unwinds: from the `Drop` of something that thread owns.
    pub fn dropped(&self) {
        if std::thread::panicking() {
            self.gone.store(true, Ordering::Relaxed);
        }
    }
}

/// Lets go of a device: drops it, or, when its callback's thread is gone
/// (ended by a panic), leaks it, since dropping it then panics in cpal
/// (see the module's notes). Leaking holds the device open until the app
/// exits; it only happens after a bug has already stopped the sound.
pub fn let_go<D>(device: D, thread_gone: bool) {
    if thread_gone {
        log::warn!("the sound device's thread panicked; leaving the device open");
        std::mem::forget(device);
    } else {
        drop(device);
    }
}

/// A rodio source that ends, instead of taking the device's thread with
/// it, if it panics.
pub struct Guarded<S> {
    inner: Option<S>,
    health: Arc<Health>,
    channels: rodio::ChannelCount,
    rate: rodio::SampleRate,
}

impl<S: rodio::Source> Guarded<S> {
    /// Guards `inner`, counting what happens in `health`.
    pub fn new(inner: S, health: Arc<Health>) -> Self {
        Self {
            channels: inner.channels(),
            rate: inner.sample_rate(),
            inner: Some(inner),
            health,
        }
    }
}

impl<S: rodio::Source> Iterator for Guarded<S> {
    type Item = rodio::Sample;

    fn next(&mut self) -> Option<rodio::Sample> {
        let inner = self.inner.as_mut()?;
        match self.health.catch(|| inner.next()) {
            Some(sample) => sample,
            None => {
                // Its state is suspect now: the sound ends here.
                self.inner = None;
                None
            }
        }
    }
}

impl<S: rodio::Source> rodio::Source for Guarded<S> {
    fn current_span_len(&self) -> Option<usize> {
        match &self.inner {
            Some(inner) => inner.current_span_len(),
            None => Some(0),
        }
    }

    fn channels(&self) -> rodio::ChannelCount {
        self.inner.as_ref().map_or(self.channels, |s| s.channels())
    }

    fn sample_rate(&self) -> rodio::SampleRate {
        self.inner.as_ref().map_or(self.rate, |s| s.sample_rate())
    }

    fn total_duration(&self) -> Option<std::time::Duration> {
        self.inner.as_ref().and_then(|s| s.total_duration())
    }

    fn try_seek(&mut self, position: std::time::Duration) -> Result<(), rodio::source::SeekError> {
        match &mut self.inner {
            Some(inner) => {
                let health = self.health.clone();
                match health.catch(|| inner.try_seek(position)) {
                    Some(result) => result,
                    None => {
                        self.inner = None;
                        Ok(())
                    }
                }
            }
            None => Ok(()),
        }
    }
}

impl<S> Drop for Guarded<S> {
    fn drop(&mut self) {
        self.health.dropped();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rodio::Source as _;

    /// A source that panics on its `n`th sample.
    struct Faulty {
        left: usize,
    }

    impl Iterator for Faulty {
        type Item = rodio::Sample;

        fn next(&mut self) -> Option<rodio::Sample> {
            assert!(self.left > 0, "a decoder's bug");
            self.left -= 1;
            Some(0.5)
        }
    }

    impl rodio::Source for Faulty {
        fn current_span_len(&self) -> Option<usize> {
            None
        }
        fn channels(&self) -> rodio::ChannelCount {
            rodio::ChannelCount::MIN
        }
        fn sample_rate(&self) -> rodio::SampleRate {
            rodio::SampleRate::MIN
        }
        fn total_duration(&self) -> Option<std::time::Duration> {
            None
        }
    }

    #[test]
    fn a_panicking_source_ends_instead() {
        let health = Arc::new(Health::default());
        let mut guarded = Guarded::new(Faulty { left: 3 }, health.clone());
        assert_eq!(guarded.by_ref().count(), 3);
        assert_eq!(guarded.next(), None, "it stays ended");
        assert_eq!(health.caught(), 1);
        assert_eq!(guarded.channels(), rodio::ChannelCount::MIN);
        drop(guarded);
        assert!(!health.thread_gone(), "an ordinary drop");
    }

    #[test]
    fn a_source_dropped_by_a_panic_marks_its_thread_gone() {
        let health = Arc::new(Health::default());
        let guarded = Guarded::new(Faulty { left: 1 }, health.clone());
        // As on cpal's thread: the callback owns the source, and a panic
        // outside the source unwinds through it.
        let thread = std::thread::spawn(move || {
            let _owned = guarded;
            panic!("the mixer's bug");
        });
        assert!(thread.join().is_err());
        assert!(health.thread_gone());
        assert_eq!(health.caught(), 0);
    }

    /// Stands in for a cpal stream: dropping it after its thread is gone
    /// is what panics.
    struct Device(Arc<AtomicBool>);

    impl Drop for Device {
        fn drop(&mut self) {
            self.0.store(true, Ordering::Relaxed);
        }
    }

    #[test]
    fn a_device_whose_thread_is_gone_is_not_dropped() {
        let healthy = Health::default();
        let dropped = Arc::new(AtomicBool::new(false));
        let_go(Device(dropped.clone()), healthy.thread_gone());
        assert!(dropped.load(Ordering::Relaxed));

        let broken = Health::default();
        broken.gone.store(true, Ordering::Relaxed);
        let dropped = Arc::new(AtomicBool::new(false));
        let_go(Device(dropped.clone()), broken.thread_gone());
        assert!(!dropped.load(Ordering::Relaxed));
    }
}
