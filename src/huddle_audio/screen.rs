//! The screen share being watched, between the threads that touch it
//! (the `huddle-video` feature).
//!
//! The media session (on the tokio runtime) hands each frame of the
//! watched share to a [`Decoding`], whose own thread has it decoded by
//! the video helper (its own process, `helper::Lane::Share`) at the size
//! the call window shows and turns it into egui's pixels. The newest
//! picture waits in the [`Screen`] for the window to take, and the
//! window is woken only when it has taken the last one, so at most once a
//! frame. Pictures nobody will see are not sent: while the window has not
//! taken the last picture, or cannot be seen (minimised, covered), every
//! frame is still decoded but the helper keeps its picture back, and only
//! the newest is fetched once the window can take it (never a queue).
//! Neither the runtime nor the interface ever decodes. Without a helper
//! the screen says there is no video ([`Screen::no_video`]).

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, PoisonError};
use std::time::{Duration, Instant};

use egui::ColorImage;

use super::decode::{self, H264, Outcome, Trouble};
use super::helper::Lane;

/// What a new picture calls to be drawn.
type Wake = Arc<dyn Fn() + Send + Sync>;

/// Frames waiting for the decoder at most: about three seconds of a
/// share. More means it cannot keep up; they are dropped and a keyframe
/// asked for.
const QUEUE: usize = 36;
/// How often the decoder's timings reach the log while it works.
const REPORT_EVERY: Duration = Duration::from_secs(10);

/// One picture of the share, ready to draw.
#[derive(Clone, Debug)]
pub struct Picture {
    /// Its pixels, at most about twice the window's size each way.
    pub image: Arc<ColorImage>,
    /// The share's own size, before shrinking.
    pub source: [usize; 2],
}

/// The newest picture of the watched share, shared between the decoder
/// thread, which puts, and the interface, which takes; and what the
/// interface tells the decoder back (the size it shows the share at).
#[derive(Clone)]
pub struct Screen {
    shared: Arc<Shared>,
}

struct Shared {
    newest: Mutex<Option<Picture>>,
    /// Whether the window showing the share can be seen.
    visible: AtomicBool,
    /// The helper kept back a newer picture than the window has: fetched
    /// once the window can take it.
    kept: AtomicBool,
    /// Has the decoder thread fetch the kept picture.
    fetch: Mutex<Option<Box<dyn Fn() + Send + Sync>>>,
    /// The window's size for the share in pixels, width in the high half.
    fit: AtomicU64,
    /// The decoder wants a keyframe: set by it, taken by the session.
    keyframe: AtomicBool,
    /// There is no video helper to decode with.
    no_video: AtomicBool,
    /// Pictures put since the start, for the window to tell one from the
    /// next and for tests.
    pictures: AtomicUsize,
    /// Wakes whoever draws the pictures: the whole interface at first,
    /// the call window alone once it is open ([`Screen::set_wake`]).
    wake: Mutex<Wake>,
}

impl std::fmt::Debug for Screen {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Screen")
            .field("fit", &self.fit())
            .field("pictures", &self.pictures())
            .finish_non_exhaustive()
    }
}

/// One screen is only ever equal to itself (its clones).
impl PartialEq for Screen {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.shared, &other.shared)
    }
}

impl Screen {
    /// An empty screen; `wake` is called when a picture arrives that the
    /// interface has not been woken for yet.
    pub fn new(wake: impl Fn() + Send + Sync + 'static) -> Self {
        Self {
            shared: Arc::new(Shared {
                newest: Mutex::new(None),
                visible: AtomicBool::new(true),
                kept: AtomicBool::new(false),
                fetch: Mutex::new(None),
                fit: AtomicU64::new(0),
                keyframe: AtomicBool::new(false),
                no_video: AtomicBool::new(false),
                pictures: AtomicUsize::new(0),
                wake: Mutex::new(Arc::new(wake)),
            }),
        }
    }

    fn newest(&self) -> std::sync::MutexGuard<'_, Option<Picture>> {
        self.shared
            .newest
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    /// Has a new picture call `wake` from now on in place of what it
    /// called: the call window has new pictures repaint only itself, not
    /// the main window behind it.
    pub fn set_wake(&self, wake: impl Fn() + Send + Sync + 'static) {
        *self
            .shared
            .wake
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Arc::new(wake);
    }

    /// Calls the wake, not holding its lock while it runs.
    fn wake(&self) {
        let wake = Arc::clone(
            &self
                .shared
                .wake
                .lock()
                .unwrap_or_else(PoisonError::into_inner),
        );
        wake();
    }

    /// Puts the newest picture in place of any the interface has not
    /// taken, waking it only if it had taken the last one.
    pub fn put(&self, picture: Picture) {
        let was_empty = self.newest().replace(picture).is_none();
        self.shared.pictures.fetch_add(1, Ordering::Relaxed);
        if was_empty {
            self.wake();
        }
    }

    /// The newest picture, if one came since the last taken. The helper
    /// is then asked for any picture it kept back meanwhile.
    pub fn take(&self) -> Option<Picture> {
        let picture = self.newest().take();
        if picture.is_some() {
            self.fetch_kept();
        }
        picture
    }

    /// Forgets a picture not taken, and any kept back: the share changed
    /// or stopped.
    pub fn clear(&self) {
        self.newest().take();
        self.shared.kept.store(false, Ordering::Relaxed);
    }

    /// Whether the next frame's picture should be sent: the window can be
    /// seen and has taken the last one. Otherwise the helper keeps it
    /// back ([`Self::keep`]).
    pub fn wants_picture(&self) -> bool {
        self.visible() && self.newest().is_none()
    }

    /// The helper kept a picture back: it is fetched as soon as the
    /// window can take it, which may be now.
    pub fn keep(&self) {
        self.shared.kept.store(true, Ordering::Relaxed);
        // The window may have taken the last picture since it was asked.
        self.fetch_kept();
    }

    /// Asks the decoder thread for the kept picture if the window can take
    /// one; only one asker wins it.
    fn fetch_kept(&self) {
        if self.wants_picture() && self.shared.kept.swap(false, Ordering::Relaxed) {
            let fetch = self
                .shared
                .fetch
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            if let Some(fetch) = fetch.as_ref() {
                fetch();
            }
        }
    }

    /// What has the decoder thread fetch a kept picture.
    fn on_fetch(&self, fetch: impl Fn() + Send + Sync + 'static) {
        *self
            .shared
            .fetch
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Some(Box::new(fetch));
    }

    /// Says whether the window showing the share can be seen; when it can
    /// again, the newest picture kept meanwhile is fetched.
    pub fn set_visible(&self, visible: bool) {
        let was = self.shared.visible.swap(visible, Ordering::Relaxed);
        if visible && !was {
            self.fetch_kept();
        }
    }

    /// Whether the window showing the share can be seen.
    pub fn visible(&self) -> bool {
        self.shared.visible.load(Ordering::Relaxed)
    }

    /// How many pictures have been put.
    pub fn pictures(&self) -> usize {
        self.shared.pictures.load(Ordering::Relaxed)
    }

    /// The size, in pixels, the window shows the share at.
    pub fn set_fit(&self, width: usize, height: usize) {
        let pack = |n: usize| u64::from(u32::try_from(n).unwrap_or(u32::MAX));
        self.shared
            .fit
            .store((pack(width) << 32) | pack(height), Ordering::Relaxed);
    }

    /// The size the window shows the share at; 0×0 until it has said.
    pub fn fit(&self) -> (usize, usize) {
        let fit = self.shared.fit.load(Ordering::Relaxed);
        let unpack = |n: u64| usize::try_from(n & 0xffff_ffff).unwrap_or(0);
        (unpack(fit >> 32), unpack(fit))
    }

    /// The decoder needs a keyframe to go on.
    pub fn want_keyframe(&self) {
        self.shared.keyframe.store(true, Ordering::Relaxed);
    }

    /// Whether a keyframe was wanted since last asked.
    pub fn take_keyframe_wish(&self) -> bool {
        self.shared.keyframe.swap(false, Ordering::Relaxed)
    }

    /// Says whether there is a video helper to decode with.
    pub fn set_no_video(&self, no_video: bool) {
        self.shared.no_video.store(no_video, Ordering::Relaxed);
    }

    /// Whether the share cannot be shown: the video helper is missing or
    /// failed too often. The window says so instead of waiting.
    pub fn no_video(&self) -> bool {
        self.shared.no_video.load(Ordering::Relaxed)
    }
}

/// What the decoder thread is told.
enum Job {
    /// A new stream starts: a fresh decoder, waiting for a keyframe.
    Start,
    /// The next frame; `contiguous` false after frames were lost.
    Frame { unit: Vec<u8>, contiguous: bool },
    /// The window can take the picture the helper kept back.
    Fetch,
    /// Nothing is watched any more.
    Stop,
}

#[derive(Default)]
struct Queue {
    jobs: VecDeque<Job>,
    closed: bool,
}

type Jobs = Arc<(Mutex<Queue>, Condvar)>;

/// The decoder thread for one session's watched share. Dropping it ends
/// the thread once it has finished the frame in hand.
pub struct Decoding {
    jobs: Jobs,
    screen: Screen,
    /// Frames were dropped since the last one queued.
    gap: bool,
}

impl std::fmt::Debug for Decoding {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Decoding")
            .field("screen", &self.screen)
            .finish_non_exhaustive()
    }
}

impl Decoding {
    /// Starts the thread, idle until [`Self::start`].
    pub fn spawn(screen: Screen) -> std::io::Result<Self> {
        let jobs: Jobs = Arc::default();
        let fetching = Arc::downgrade(&jobs);
        screen.on_fetch(move || {
            if let Some(jobs) = fetching.upgrade() {
                let mut queue = jobs.0.lock().unwrap_or_else(PoisonError::into_inner);
                if !queue.jobs.iter().any(|job| matches!(job, Job::Fetch)) {
                    queue.jobs.push_back(Job::Fetch);
                }
                drop(queue);
                jobs.1.notify_one();
            }
        });
        let (thread_jobs, thread_screen) = (jobs.clone(), screen.clone());
        std::thread::Builder::new()
            .name("huddle-video".into())
            .spawn(move || run(&thread_jobs, &thread_screen))?;
        Ok(Self {
            jobs,
            screen,
            gap: false,
        })
    }

    fn queue(&self) -> std::sync::MutexGuard<'_, Queue> {
        self.jobs.0.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Hands the thread `job`, dropping whatever frames still wait if
    /// `fresh` (a new start or a stop makes them stale).
    fn send(&self, job: Job, fresh: bool) {
        let mut queue = self.queue();
        if fresh {
            queue.jobs.clear();
        }
        queue.jobs.push_back(job);
        drop(queue);
        self.jobs.1.notify_one();
    }

    /// A new stream is watched: frames from here on are its.
    pub fn start(&mut self) {
        self.gap = false;
        self.send(Job::Start, true);
    }

    /// How many frames wait to be decoded: a sender whose frames come in
    /// bursts holds the rest back while this is high, rather than
    /// overflow the queue (which costs a keyframe).
    pub fn waiting(&self) -> usize {
        self.queue()
            .jobs
            .iter()
            .filter(|job| matches!(job, Job::Frame { .. }))
            .count()
    }

    /// The next frame of the watched stream; `contiguous` false when
    /// frames before it were lost.
    pub fn push(&mut self, unit: Vec<u8>, contiguous: bool) {
        let full = self.queue().jobs.len() >= QUEUE;
        if full {
            // Behind by seconds: start over at a keyframe.
            let mut queue = self.queue();
            queue.jobs.retain(|job| !matches!(job, Job::Frame { .. }));
            drop(queue);
            self.gap = true;
            self.screen.want_keyframe();
            log::info!("video: the decoder fell behind; frames dropped, a keyframe asked for");
            return;
        }
        let contiguous = contiguous && !std::mem::take(&mut self.gap);
        self.send(Job::Frame { unit, contiguous }, false);
    }

    /// Nothing is watched: the thread idles and the picture goes.
    pub fn stop(&mut self) {
        self.send(Job::Stop, true);
    }
}

impl Drop for Decoding {
    fn drop(&mut self) {
        self.queue().closed = true;
        self.jobs.1.notify_one();
    }
}

/// The next job, or none once the queue is closed.
fn next(jobs: &Jobs) -> Option<Job> {
    let mut queue = jobs.0.lock().unwrap_or_else(PoisonError::into_inner);
    loop {
        if queue.closed {
            return None;
        }
        if let Some(job) = queue.jobs.pop_front() {
            return Some(job);
        }
        queue = jobs.1.wait(queue).unwrap_or_else(PoisonError::into_inner);
    }
}

/// How many of the first frames' outcomes are logged.
const FIRST_TOLD: u32 = 5;

/// What the decoder did since its timings were last logged.
#[derive(Default)]
struct Timings {
    /// How many outcomes were logged one by one.
    told: u32,
    since: Option<Instant>,
    pictures: u32,
    decoding: Duration,
    slowest: Duration,
    converting: Duration,
    errors: u32,
    /// Frames whose picture the helper kept back, unseen.
    kept: u32,
    /// Pictures the same as the last.
    unchanged: u32,
    size: [usize; 2],
    shown: [usize; 2],
    /// Whether the helper decoded the last picture on the GPU.
    gpu: bool,
}

impl Timings {
    fn log(&mut self, now: Instant) {
        let Some(since) = self.since else {
            self.since = Some(now);
            return;
        };
        if now < since + REPORT_EVERY {
            return;
        }
        if self.pictures > 0 || self.errors > 0 || self.kept > 0 {
            let per =
                |total: Duration| total.as_secs_f64() * 1000.0 / f64::from(self.pictures.max(1));
            log::info!(
                "video: decoded {} pictures in {:.0} s ({}x{} shown at {}x{}) in the helper {}: \
                 {:.1} ms a picture (slowest {:.1} ms), {:.1} ms converting; {} kept back unseen, \
                 {} unchanged; {} frames did not decode",
                self.pictures,
                now.duration_since(since).as_secs_f64(),
                self.size[0],
                self.size[1],
                self.shown[0],
                self.shown[1],
                if self.gpu {
                    "on the GPU"
                } else {
                    "in software"
                },
                per(self.decoding),
                self.slowest.as_secs_f64() * 1000.0,
                per(self.converting),
                self.kept,
                self.unchanged,
                self.errors
            );
        }
        *self = Self {
            since: Some(now),
            ..Self::default()
        };
    }
}

/// What a frame or a fetch gave, into the screen: a picture as egui's
/// pixels, a kept one noted, a keyframe asked for when one is needed.
fn show(
    screen: &Screen,
    decoded: Result<decode::Outcome, Trouble>,
    started: Instant,
    timings: &mut Timings,
) {
    // What the first frames come to, which the timings leave out while
    // nothing decodes.
    if timings.told < FIRST_TOLD {
        timings.told += 1;
        let what = match &decoded {
            Ok(decode::Outcome::Picture(_)) => "a picture".to_owned(),
            Ok(decode::Outcome::Kept) => "kept back".to_owned(),
            Ok(decode::Outcome::Unchanged) => "unchanged".to_owned(),
            Ok(decode::Outcome::Nothing) => "nothing".to_owned(),
            Err(trouble) => format!("{trouble}"),
        };
        log::debug!("video: the share's decoder: {what}");
    }
    let took = started.elapsed();
    match decoded {
        Ok(Outcome::Picture(picture)) => {
            screen.set_no_video(false);
            timings.decoding += took;
            timings.slowest = timings.slowest.max(took);
            timings.gpu = picture.gpu;
            let started = Instant::now();
            let source = picture.source;
            match decode::to_image(&picture.yuv) {
                Ok(image) => {
                    timings.pictures += 1;
                    timings.size = source;
                    timings.shown = image.size;
                    timings.converting += started.elapsed();
                    screen.put(Picture {
                        image: Arc::new(image),
                        source,
                    });
                }
                Err(error) => {
                    timings.errors += 1;
                    log::debug!("video: {error}");
                }
            }
        }
        Ok(Outcome::Kept) => {
            timings.kept += 1;
            screen.keep();
        }
        Ok(Outcome::Unchanged) => timings.unchanged += 1,
        Ok(Outcome::Nothing) => {}
        Err(Trouble::NeedKeyframe) => screen.want_keyframe(),
        Err(Trouble::NoHelper) => screen.set_no_video(true),
        Err(error) => {
            timings.errors += 1;
            log::debug!("video: {error}");
            screen.want_keyframe();
        }
    }
    timings.log(Instant::now());
}

/// The decoder thread: decodes what comes, puts the newest picture.
fn run(jobs: &Jobs, screen: &Screen) {
    let mut decoder: Option<H264> = None;
    let mut timings = Timings::default();
    while let Some(job) = next(jobs) {
        match job {
            Job::Start => {
                log::info!("video: the share's decoder starts over");
                let fresh = H264::new(Lane::Share);
                screen.set_no_video(fresh.no_helper());
                decoder = Some(fresh);
                screen.clear();
                // A keyframe to start on, asked for by the session too.
                screen.want_keyframe();
            }
            Job::Stop => {
                decoder = None;
                screen.clear();
            }
            Job::Frame { unit, contiguous } => {
                let Some(decoder) = &mut decoder else {
                    continue;
                };
                if !contiguous {
                    decoder.lost();
                }
                let started = Instant::now();
                decoder.set_fit(screen.fit().0, screen.fit().1);
                let keyframe = decode::is_keyframe(&unit);
                let decoded = decoder.decode(&unit, screen.wants_picture());
                if keyframe {
                    log::debug!(
                        "video: the share's decoder on a keyframe ({} bytes, NAL units {:?}): {}",
                        unit.len(),
                        super::bitstream::nal_types(&unit),
                        match &decoded {
                            Ok(decode::Outcome::Picture(_)) => "a picture".to_owned(),
                            Ok(decode::Outcome::Kept) => "kept back".to_owned(),
                            Ok(decode::Outcome::Unchanged) => "unchanged".to_owned(),
                            Ok(decode::Outcome::Nothing) => "nothing".to_owned(),
                            Err(trouble) => format!("{trouble}"),
                        }
                    );
                }
                show(screen, decoded, started, &mut timings);
            }
            Job::Fetch => {
                let Some(decoder) = &mut decoder else {
                    continue;
                };
                let started = Instant::now();
                decoder.set_fit(screen.fit().0, screen.fit().1);
                let fetched = decoder.fetch();
                show(screen, fetched, started, &mut timings);
            }
        }
    }
}

/// The demo's share: plays the 1080p fixture over and over at its 12
/// frames a second into `screen` while `watched` is set.
#[cfg(feature = "demo")]
pub fn demo_feed(screen: Screen, watched: Arc<AtomicBool>) -> std::io::Result<()> {
    let stream = include_bytes!("fixtures/screen-1920x1080.h264");
    let mut frames: Vec<Vec<u8>> = Vec::new();
    let mut frame = Vec::new();
    for nal in super::bitstream::nal_units(stream) {
        frame.extend_from_slice(&[0, 0, 0, 1]);
        frame.extend_from_slice(nal);
        if matches!(super::bitstream::nal_type(nal), Some(1 | 5)) {
            frames.push(std::mem::take(&mut frame));
        }
    }
    let mut decoding = Decoding::spawn(screen)?;
    std::thread::Builder::new()
        .name("huddle-video-demo".into())
        .spawn(move || {
            let mut playing = false;
            let mut n = 0;
            loop {
                let wanted = watched.load(Ordering::Relaxed);
                if wanted && !playing {
                    decoding.start();
                    n = 0;
                } else if !wanted && playing {
                    decoding.stop();
                }
                playing = wanted;
                if playing && let Some(frame) = frames.get(n % frames.len().max(1)) {
                    // Each pass starts again at the first frame, a keyframe.
                    decoding.push(frame.clone(), true);
                    n += 1;
                }
                std::thread::sleep(Duration::from_millis(83));
            }
        })?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn picture(width: usize) -> Picture {
        Picture {
            image: Arc::new(ColorImage::filled([width, 1], egui::Color32::RED)),
            source: [width, 1],
        }
    }

    #[test]
    fn the_newest_picture_wins_and_wakes_once() {
        let woken = Arc::new(AtomicUsize::new(0));
        let counter = woken.clone();
        let screen = Screen::new(move || {
            counter.fetch_add(1, Ordering::Relaxed);
        });
        assert!(screen.take().is_none());
        screen.put(picture(1));
        screen.put(picture(2));
        screen.put(picture(3));
        // Three put, one wake: the interface had not taken the first.
        assert_eq!(woken.load(Ordering::Relaxed), 1);
        assert_eq!(screen.pictures(), 3);
        let newest = screen.take().expect("a picture");
        assert_eq!(newest.source, [3, 1], "older ones dropped, not queued");
        assert!(screen.take().is_none());
        // Taken: the next wakes again.
        screen.put(picture(4));
        assert_eq!(woken.load(Ordering::Relaxed), 2);
        screen.clear();
        assert!(screen.take().is_none());
        // The window's size goes the other way.
        assert_eq!(screen.fit(), (0, 0));
        screen.set_fit(1280, 720);
        assert_eq!(screen.fit(), (1280, 720));
        assert!(!screen.take_keyframe_wish());
        screen.want_keyframe();
        assert!(screen.take_keyframe_wish() && !screen.take_keyframe_wish());
        assert!(!screen.no_video());
        screen.set_no_video(true);
        assert!(screen.clone().no_video(), "shared with its clones");
        assert_eq!(screen.clone(), screen);
        assert_ne!(Screen::new(|| {}), screen);
    }

    /// The thread has the camera fixture decoded (by the helper's own
    /// code, on a thread in tests) into pictures of the size the window
    /// asks, asks for a keyframe when it must, and goes idle on stop.
    #[test]
    fn the_decoder_thread_fills_the_screen() {
        let stream = include_bytes!("fixtures/camera-480x480.h264");
        let mut frames: Vec<Vec<u8>> = Vec::new();
        let mut frame = Vec::new();
        for nal in super::super::bitstream::nal_units(stream) {
            frame.extend_from_slice(&[0, 0, 0, 1]);
            frame.extend_from_slice(nal);
            if matches!(super::super::bitstream::nal_type(nal), Some(1 | 5)) {
                frames.push(std::mem::take(&mut frame));
            }
        }
        let screen = Screen::new(|| {});
        screen.set_fit(200, 200);
        let mut decoding = Decoding::spawn(screen.clone()).expect("a thread");
        decoding.start();
        // Joined mid-stream: nothing until the keyframe at 44.
        for frame in &frames[40..50] {
            decoding.push(frame.clone(), true);
        }
        let wait_for = |n: usize| {
            let until = Instant::now() + Duration::from_secs(20);
            while screen.pictures() < n && Instant::now() < until {
                std::thread::sleep(Duration::from_millis(5));
            }
        };
        wait_for(1);
        // Frames 45 to 49 are decoded, but the window has not taken 44:
        // their pictures stay in the helper.
        std::thread::sleep(Duration::from_millis(300));
        assert_eq!(screen.pictures(), 1, "frame 44 alone");
        assert!(screen.take_keyframe_wish(), "it asked for one");
        let picture = screen.take().expect("the first");
        assert_eq!(picture.source, [480, 480]);
        assert_eq!(picture.image.size, [240, 240], "halved: still covers 200");
        // Taken: the newest kept back (49) comes, and only it.
        wait_for(2);
        std::thread::sleep(Duration::from_millis(100));
        assert_eq!(screen.pictures(), 2);
        let mut reference = H264::new(Lane::Share);
        reference.set_fit(200, 200);
        let mut last = None;
        for frame in &frames[44..50] {
            if let Ok(Outcome::Picture(picture)) = reference.decode(frame, true) {
                last = Some(picture);
            }
        }
        let last = decode::to_image(&last.expect("pictures").yuv).expect("converts");
        assert_eq!(*screen.take().expect("the newest").image, last);
        // Hidden: decoded, nothing sent; seen again: the newest comes.
        screen.set_visible(false);
        for frame in &frames[50..53] {
            decoding.push(frame.clone(), true);
        }
        std::thread::sleep(Duration::from_millis(300));
        assert_eq!(screen.pictures(), 2, "hidden: nothing sent");
        assert!(screen.take().is_none());
        screen.set_visible(true);
        wait_for(3);
        assert_eq!(screen.take().expect("the newest").image.size, [240, 240]);
        decoding.stop();
        decoding.push(frames[44].clone(), true);
        std::thread::sleep(Duration::from_millis(100));
        assert!(screen.take().is_none(), "stopped: nothing decodes");
        drop(decoding);
    }
}
