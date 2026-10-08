//! The camera tiles being watched, between the threads that touch them
//! (the `huddle-video` feature): the counterpart of `screen` for many
//! small pictures instead of one large one.
//!
//! The media session hands each frame of a camera with a tile to
//! [`CameraDecoding`], one thread for every camera (a 480×480 camera
//! decodes in well under a millisecond, so nine of them at 22 frames a
//! second are about a tenth of one core), with a decoder per camera in
//! the video helper (its own process, `helper::Lane::Cameras`, beside the
//! share's). Each picture comes back shrunk to the tile's size and
//! becomes RGBA here, and the newest per camera waits in the [`Gallery`]
//! for the window to take. As for the share, a camera's picture the
//! window would not see (it has not taken that camera's last one, or the
//! window cannot be seen) is decoded but kept back in the helper, and
//! only the newest fetched once the window can take it: never a queue.
//! The window is woken only when it had taken everything, so at most
//! once a frame however many cameras play.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, PoisonError};
use std::time::{Duration, Instant};

#[cfg(any(test, feature = "demo"))]
use noslacking_video_ipc::h264;

use super::decode::{H264, Outcome, Trouble};
use super::helper::Lane;
use super::screen::Picture;

/// What a new picture calls to be drawn.
type Wake = Arc<dyn Fn() + Send + Sync>;

/// Frames of one camera waiting for the decoder at most: about a second.
/// More means it cannot keep up; that camera's are dropped and a
/// keyframe asked for.
const QUEUE: usize = 22;
/// How often the decoder's timings reach the log while it works.
const REPORT_EVERY: Duration = Duration::from_secs(10);

/// The newest picture of each camera with a tile, shared between the
/// decoder thread, which puts, and the interface, which takes; and the
/// tile size the interface tells the decoder back.
#[derive(Clone)]
pub struct Gallery {
    shared: Arc<Shared>,
}

struct Shared {
    newest: Mutex<BTreeMap<String, Picture>>,
    /// Whether the window showing the tiles can be seen.
    visible: AtomicBool,
    /// The cameras whose helper kept back a newer picture than the window
    /// has: fetched once the window can take it.
    kept: Mutex<BTreeSet<String>>,
    /// Has the decoder thread fetch a camera's kept picture.
    fetch: Mutex<Option<Fetch>>,
    /// A tile's size in pixels, width in the high half.
    fit: AtomicU64,
    /// The cameras whose decoder wants a keyframe: set by it, taken by
    /// the session.
    keyframes: Mutex<BTreeSet<String>>,
    /// There is no video helper to decode with.
    no_video: AtomicBool,
    /// Pictures put since the start, for tests and the demo.
    pictures: AtomicUsize,
    /// The demo's tint per camera, so one fixture looks like several
    /// people: added to the chroma planes.
    #[cfg(feature = "demo")]
    tints: Mutex<BTreeMap<String, [i16; 2]>>,
    /// Wakes whoever draws the pictures: the whole interface at first,
    /// the call window alone once it is open ([`Gallery::set_wake`]).
    wake: Mutex<Wake>,
}

impl std::fmt::Debug for Gallery {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Gallery")
            .field("fit", &self.fit())
            .field("pictures", &self.pictures())
            .finish_non_exhaustive()
    }
}

/// What has the decoder thread fetch a camera's kept picture.
type Fetch = Box<dyn Fn(&str) + Send + Sync>;

/// One gallery is only ever equal to itself (its clones).
impl PartialEq for Gallery {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.shared, &other.shared)
    }
}

/// Locks, going on with what a panicked holder left: the data is
/// pictures, never half-written.
fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

impl Gallery {
    /// An empty gallery; `wake` is called when a picture arrives and the
    /// interface had taken all the others.
    pub fn new(wake: impl Fn() + Send + Sync + 'static) -> Self {
        Self {
            shared: Arc::new(Shared {
                newest: Mutex::new(BTreeMap::new()),
                visible: AtomicBool::new(true),
                kept: Mutex::new(BTreeSet::new()),
                fetch: Mutex::new(None),
                fit: AtomicU64::new(0),
                keyframes: Mutex::new(BTreeSet::new()),
                no_video: AtomicBool::new(false),
                pictures: AtomicUsize::new(0),
                #[cfg(feature = "demo")]
                tints: Mutex::new(BTreeMap::new()),
                wake: Mutex::new(Arc::new(wake)),
            }),
        }
    }

    /// Has a new picture call `wake` from now on in place of what it
    /// called: the call window has new pictures repaint only itself, not
    /// the main window behind it.
    pub fn set_wake(&self, wake: impl Fn() + Send + Sync + 'static) {
        *lock(&self.shared.wake) = Arc::new(wake);
    }

    /// Calls the wake, not holding its lock while it runs.
    fn wake(&self) {
        let wake = Arc::clone(&lock(&self.shared.wake));
        wake();
    }

    /// Puts `key`'s newest picture in place of any not taken, waking the
    /// interface only if nothing else was waiting.
    pub fn put(&self, key: &str, picture: Picture) {
        let was_empty = {
            let mut newest = lock(&self.shared.newest);
            let was_empty = newest.is_empty();
            newest.insert(key.to_owned(), picture);
            was_empty
        };
        self.shared.pictures.fetch_add(1, Ordering::Relaxed);
        if was_empty {
            self.wake();
        }
    }

    /// The newest picture of each camera that has a new one. The helper
    /// is then asked for any of theirs it kept back meanwhile.
    pub fn take(&self) -> BTreeMap<String, Picture> {
        let taken = std::mem::take(&mut *lock(&self.shared.newest));
        for key in taken.keys() {
            self.fetch_kept(key);
        }
        taken
    }

    /// Forgets `key`'s picture not taken, and any kept back: its tile
    /// went, or its stream started again.
    pub fn clear(&self, key: &str) {
        lock(&self.shared.newest).remove(key);
        lock(&self.shared.kept).remove(key);
    }

    /// Whether `key`'s next picture should be sent: the window can be
    /// seen and has taken that camera's last one. Otherwise the helper
    /// keeps it back ([`Self::keep`]).
    pub fn wants_picture(&self, key: &str) -> bool {
        self.visible() && !lock(&self.shared.newest).contains_key(key)
    }

    /// The helper kept a picture of `key` back: it is fetched as soon as
    /// the window can take it, which may be now.
    pub fn keep(&self, key: &str) {
        lock(&self.shared.kept).insert(key.to_owned());
        // The window may have taken the last picture since it was asked.
        self.fetch_kept(key);
    }

    /// Asks the decoder thread for `key`'s kept picture if the window can
    /// take one; only one asker wins it.
    fn fetch_kept(&self, key: &str) {
        let won = self.wants_picture(key) && lock(&self.shared.kept).remove(key);
        if won && let Some(fetch) = lock(&self.shared.fetch).as_ref() {
            fetch(key);
        }
    }

    /// What has the decoder thread fetch a camera's kept picture.
    fn on_fetch(&self, fetch: impl Fn(&str) + Send + Sync + 'static) {
        *lock(&self.shared.fetch) = Some(Box::new(fetch));
    }

    /// Says whether the window showing the tiles can be seen; when it can
    /// again, each camera's newest picture kept meanwhile is fetched.
    pub fn set_visible(&self, visible: bool) {
        let was = self.shared.visible.swap(visible, Ordering::Relaxed);
        if visible && !was {
            let kept: Vec<String> = lock(&self.shared.kept).iter().cloned().collect();
            for key in kept {
                self.fetch_kept(&key);
            }
        }
    }

    /// Whether the window showing the tiles can be seen.
    pub fn visible(&self) -> bool {
        self.shared.visible.load(Ordering::Relaxed)
    }

    /// How many pictures have been put.
    pub fn pictures(&self) -> usize {
        self.shared.pictures.load(Ordering::Relaxed)
    }

    /// A tile's size, in pixels.
    pub fn set_fit(&self, width: usize, height: usize) {
        let pack = |n: usize| u64::from(u32::try_from(n).unwrap_or(u32::MAX));
        self.shared
            .fit
            .store((pack(width) << 32) | pack(height), Ordering::Relaxed);
    }

    /// A tile's size; 0×0 until the window has said.
    pub fn fit(&self) -> (usize, usize) {
        let fit = self.shared.fit.load(Ordering::Relaxed);
        let unpack = |n: u64| usize::try_from(n & 0xffff_ffff).unwrap_or(0);
        (unpack(fit >> 32), unpack(fit))
    }

    /// Says whether there is a video helper to decode with.
    pub fn set_no_video(&self, no_video: bool) {
        self.shared.no_video.store(no_video, Ordering::Relaxed);
    }

    /// Whether the cameras cannot be shown: the video helper is missing
    /// or failed too often. The tiles show faces instead.
    pub fn no_video(&self) -> bool {
        self.shared.no_video.load(Ordering::Relaxed)
    }

    /// `key`'s decoder needs a keyframe to go on.
    pub fn want_keyframe(&self, key: &str) {
        lock(&self.shared.keyframes).insert(key.to_owned());
    }

    /// Whether `key` wanted a keyframe since last asked.
    pub fn take_keyframe_wish(&self, key: &str) -> bool {
        lock(&self.shared.keyframes).remove(key)
    }

    /// The demo tints `key`'s pictures so one fixture plays several
    /// people: `[blue, red]` added to the chroma.
    #[cfg(feature = "demo")]
    pub fn tint(&self, key: &str, tint: [i16; 2]) {
        lock(&self.shared.tints).insert(key.to_owned(), tint);
    }

    #[cfg(feature = "demo")]
    fn tinted(&self, key: &str, yuv: &mut noslacking_video_ipc::Planes) {
        let Some([u, v]) = lock(&self.shared.tints).get(key).copied() else {
            return;
        };
        let shift = |plane: &mut Vec<u8>, by: i16| {
            for p in plane.iter_mut() {
                *p = u8::try_from((i16::from(*p) + by).clamp(16, 240)).unwrap_or(128);
            }
        };
        shift(&mut yuv.u, u);
        shift(&mut yuv.v, v);
    }
}

/// What the decoder thread is told.
enum Job {
    /// A camera starts (or switched layer): a fresh decoder for it,
    /// waiting for a keyframe.
    Start(String),
    /// A camera's next frame; `contiguous` false after frames were lost.
    Frame {
        key: String,
        unit: Vec<u8>,
        contiguous: bool,
    },
    /// A camera's tile went.
    Stop(String),
    /// The window can take the picture of this camera the helper kept
    /// back.
    Fetch(String),
}

#[derive(Default)]
struct Queue {
    jobs: VecDeque<Job>,
    closed: bool,
}

type Jobs = Arc<(Mutex<Queue>, Condvar)>;

/// The decoder thread for one session's camera tiles. Dropping it ends
/// the thread once it has finished the frame in hand.
pub struct CameraDecoding {
    jobs: Jobs,
    gallery: Gallery,
    /// Cameras whose frames were dropped since the last queued.
    gaps: BTreeSet<String>,
}

impl std::fmt::Debug for CameraDecoding {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CameraDecoding")
            .field("gallery", &self.gallery)
            .finish_non_exhaustive()
    }
}

impl CameraDecoding {
    /// Starts the thread, idle until a camera starts.
    pub fn spawn(gallery: Gallery) -> std::io::Result<Self> {
        let jobs: Jobs = Arc::default();
        let fetching = Arc::downgrade(&jobs);
        gallery.on_fetch(move |key| {
            if let Some(jobs) = fetching.upgrade() {
                let mut queue = lock(&jobs.0);
                if !queue
                    .jobs
                    .iter()
                    .any(|job| matches!(job, Job::Fetch(k) if k == key))
                {
                    queue.jobs.push_back(Job::Fetch(key.to_owned()));
                }
                drop(queue);
                jobs.1.notify_one();
            }
        });
        let (thread_jobs, thread_gallery) = (jobs.clone(), gallery.clone());
        std::thread::Builder::new()
            .name("huddle-cameras".into())
            .spawn(move || run(&thread_jobs, &thread_gallery))?;
        Ok(Self {
            jobs,
            gallery,
            gaps: BTreeSet::new(),
        })
    }

    fn queue(&self) -> std::sync::MutexGuard<'_, Queue> {
        lock(&self.jobs.0)
    }

    /// Drops `key`'s frames and fetches still waiting: they are stale.
    fn forget(queue: &mut Queue, key: &str) {
        queue.jobs.retain(|job| match job {
            Job::Frame { key: k, .. } | Job::Fetch(k) => k != key,
            Job::Start(_) | Job::Stop(_) => true,
        });
    }

    fn send(&self, job: Job) {
        self.queue().jobs.push_back(job);
        self.jobs.1.notify_one();
    }

    /// Camera `key` has a tile now, or its stream changed: frames from
    /// here on are its new stream's.
    pub fn start(&mut self, key: &str) {
        self.gaps.remove(key);
        let mut queue = self.queue();
        Self::forget(&mut queue, key);
        queue.jobs.push_back(Job::Start(key.to_owned()));
        drop(queue);
        self.jobs.1.notify_one();
    }

    /// How many of camera `key`'s frames wait to be decoded: a sender
    /// whose frames come in bursts holds the rest back while this is
    /// high, rather than overflow the queue (which costs a keyframe).
    pub fn waiting(&self, key: &str) -> usize {
        self.queue()
            .jobs
            .iter()
            .filter(|job| matches!(job, Job::Frame { key: k, .. } if k == key))
            .count()
    }

    /// The next frame of camera `key`; `contiguous` false when frames
    /// before it were lost.
    pub fn push(&mut self, key: &str, unit: Vec<u8>, contiguous: bool) {
        let mut queue = self.queue();
        let waiting = queue
            .jobs
            .iter()
            .filter(|job| matches!(job, Job::Frame { key: k, .. } if k == key))
            .count();
        if waiting >= QUEUE {
            // Behind by a second: start over at a keyframe.
            Self::forget(&mut queue, key);
            drop(queue);
            self.gaps.insert(key.to_owned());
            self.gallery.want_keyframe(key);
            log::info!(
                "video: the camera decoder fell behind; frames dropped, a keyframe asked for"
            );
            return;
        }
        drop(queue);
        let contiguous = contiguous && !self.gaps.remove(key);
        self.send(Job::Frame {
            key: key.to_owned(),
            unit,
            contiguous,
        });
    }

    /// Camera `key` has no tile any more: its decoder and picture go.
    pub fn stop(&mut self, key: &str) {
        self.gaps.remove(key);
        let mut queue = self.queue();
        Self::forget(&mut queue, key);
        queue.jobs.push_back(Job::Stop(key.to_owned()));
        drop(queue);
        self.jobs.1.notify_one();
    }
}

impl Drop for CameraDecoding {
    fn drop(&mut self) {
        self.queue().closed = true;
        self.jobs.1.notify_one();
    }
}

/// The next job, or none once the queue is closed.
fn next(jobs: &Jobs) -> Option<Job> {
    let mut queue = lock(&jobs.0);
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

/// What the decoder did since its timings were last logged.
#[derive(Default)]
struct Timings {
    since: Option<Instant>,
    pictures: u32,
    working: Duration,
    errors: u32,
    /// Frames whose picture the helper kept back, unseen.
    kept: u32,
    /// Pictures the same as the last.
    unchanged: u32,
    shown: [usize; 2],
}

impl Timings {
    fn log(&mut self, now: Instant, cameras: usize) {
        let Some(since) = self.since else {
            self.since = Some(now);
            return;
        };
        if now < since + REPORT_EVERY {
            return;
        }
        if self.pictures > 0 || self.errors > 0 || self.kept > 0 {
            let seconds = now.duration_since(since).as_secs_f64();
            log::info!(
                "video: {cameras} cameras: {} pictures in {seconds:.0} s, shown at {}x{}: {:.2} ms \
                 a picture through the helper and converting, busy {:.1} % of the time; {} kept \
                 back unseen, {} unchanged; {} frames did not decode",
                self.pictures,
                self.shown[0],
                self.shown[1],
                self.working.as_secs_f64() * 1000.0 / f64::from(self.pictures.max(1)),
                self.working.as_secs_f64() * 100.0 / seconds.max(0.001),
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

/// One frame of camera `key` through its decoder into the gallery, or,
/// with no frame, the picture the helper kept back.
fn decode_one(
    key: &str,
    decoder: &mut H264,
    frame: Option<(&[u8], bool)>,
    gallery: &Gallery,
    timings: &mut Timings,
) {
    let started = Instant::now();
    let (width, height) = gallery.fit();
    decoder.set_fit(width, height);
    let decoded = match frame {
        Some((unit, contiguous)) => {
            if !contiguous {
                decoder.lost();
            }
            decoder.decode(unit, gallery.wants_picture(key))
        }
        None => decoder.fetch(),
    };
    match decoded {
        Ok(Outcome::Picture(picture)) => {
            gallery.set_no_video(false);
            let source = picture.source;
            #[cfg_attr(not(feature = "demo"), allow(unused_mut))]
            let mut yuv = picture.yuv;
            #[cfg(feature = "demo")]
            gallery.tinted(key, &mut yuv);
            match super::helper::to_image(&yuv, false) {
                Ok(image) => {
                    timings.pictures += 1;
                    timings.shown = image.size;
                    timings.working += started.elapsed();
                    gallery.put(
                        key,
                        Picture {
                            image: Arc::new(image),
                            source,
                        },
                    );
                }
                Err(error) => {
                    timings.errors += 1;
                    log::debug!("video: {error}");
                }
            }
        }
        Ok(Outcome::Kept) => {
            timings.kept += 1;
            gallery.keep(key);
        }
        Ok(Outcome::Unchanged) => timings.unchanged += 1,
        Ok(Outcome::Nothing) => {}
        Err(Trouble::NeedKeyframe) => gallery.want_keyframe(key),
        Err(Trouble::NoHelper) => gallery.set_no_video(true),
        Err(error) => {
            timings.errors += 1;
            log::debug!("video: camera: {error}");
            gallery.want_keyframe(key);
        }
    }
}

/// The decoder thread: a decoder per camera, the newest picture each.
fn run(jobs: &Jobs, gallery: &Gallery) {
    let mut decoders: BTreeMap<String, H264> = BTreeMap::new();
    let mut timings = Timings::default();
    while let Some(job) = next(jobs) {
        match job {
            Job::Start(key) => {
                gallery.clear(&key);
                // A keyframe to start on, asked for by the session too.
                gallery.want_keyframe(&key);
                let fresh = H264::new(Lane::Cameras);
                gallery.set_no_video(fresh.no_helper());
                decoders.insert(key, fresh);
            }
            Job::Stop(key) => {
                decoders.remove(&key);
                gallery.clear(&key);
            }
            Job::Frame {
                key,
                unit,
                contiguous,
            } => {
                if let Some(decoder) = decoders.get_mut(&key) {
                    decode_one(
                        &key,
                        decoder,
                        Some((&unit, contiguous)),
                        gallery,
                        &mut timings,
                    );
                    timings.log(Instant::now(), decoders.len());
                }
            }
            Job::Fetch(key) => {
                if let Some(decoder) = decoders.get_mut(&key) {
                    decode_one(&key, decoder, None, gallery, &mut timings);
                }
            }
        }
    }
}

/// The demo's cameras: plays the 480×480 fixture over and over at its 22
/// frames a second for each key in `playing`, tinted as the gallery says,
/// through the real decoder thread.
#[cfg(feature = "demo")]
pub fn demo_feed(gallery: Gallery, playing: Arc<Mutex<Vec<String>>>) -> std::io::Result<()> {
    let stream = include_bytes!("fixtures/camera-480x480.h264");
    let frames = h264::access_units(stream);
    let mut decoding = CameraDecoding::spawn(gallery)?;
    std::thread::Builder::new()
        .name("huddle-cameras-demo".into())
        .spawn(move || {
            let mut started: Vec<String> = Vec::new();
            let mut n = 0usize;
            loop {
                let now = lock(&playing).clone();
                for key in started.iter().filter(|k| !now.contains(k)) {
                    decoding.stop(key);
                }
                for key in now.iter().filter(|k| !started.contains(k)) {
                    decoding.start(key);
                }
                if now != started {
                    // Each pass starts again at the first frame, a keyframe.
                    n = 0;
                }
                started = now;
                if let Some(frame) = frames.get(n % frames.len().max(1)) {
                    for key in &started {
                        decoding.push(key, frame.clone(), true);
                    }
                    n += 1;
                }
                std::thread::sleep(Duration::from_millis(45));
            }
        })?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn picture(width: usize) -> Picture {
        Picture {
            image: Arc::new(egui::ColorImage::filled([width, 1], egui::Color32::RED)),
            source: [width, 1],
        }
    }

    /// Each tile keeps only its newest picture; the window is woken once
    /// for any number of them until it takes them.
    #[test]
    fn the_newest_picture_wins_per_tile_and_wakes_once() {
        let woken = Arc::new(AtomicUsize::new(0));
        let counter = woken.clone();
        let gallery = Gallery::new(move || {
            counter.fetch_add(1, Ordering::Relaxed);
        });
        assert!(gallery.take().is_empty());
        gallery.put("ana", picture(1));
        gallery.put("bob", picture(2));
        gallery.put("ana", picture(3));
        assert_eq!(woken.load(Ordering::Relaxed), 1, "one wake for three");
        let taken = gallery.take();
        let sizes: Vec<(&str, usize)> = taken
            .iter()
            .map(|(k, p)| (k.as_str(), p.source[0]))
            .collect();
        assert_eq!(sizes, [("ana", 3), ("bob", 2)], "Ana's older one dropped");
        assert!(gallery.take().is_empty());
        gallery.put("bob", picture(4));
        assert_eq!(woken.load(Ordering::Relaxed), 2, "taken: the next wakes");
        gallery.clear("bob");
        assert!(gallery.take().is_empty());
        assert_eq!(gallery.pictures(), 4);
        gallery.set_fit(240, 180);
        assert_eq!(gallery.fit(), (240, 180));
        assert!(!gallery.take_keyframe_wish("ana"));
        gallery.want_keyframe("ana");
        assert!(!gallery.take_keyframe_wish("bob"));
        assert!(gallery.take_keyframe_wish("ana") && !gallery.take_keyframe_wish("ana"));
        assert!(!gallery.no_video());
        gallery.set_no_video(true);
        assert!(gallery.clone().no_video(), "shared with its clones");
        assert_eq!(gallery.clone(), gallery);
        assert_ne!(Gallery::new(|| {}), gallery);
    }

    /// Waits up to 20 s for `gallery` to have had `n` pictures put.
    fn wait_for(gallery: &Gallery, n: usize) {
        let until = Instant::now() + Duration::from_secs(20);
        while gallery.pictures() < n && Instant::now() < until {
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    /// What the helper (its own code on a thread) makes of `frames` at
    /// `fit`, the last picture as egui's pixels.
    fn reference(frames: &[Vec<u8>], fit: (usize, usize)) -> egui::ColorImage {
        let mut decoder = H264::new(Lane::Cameras);
        decoder.set_fit(fit.0, fit.1);
        let mut last = None;
        for frame in frames {
            if let Ok(Outcome::Picture(picture)) = decoder.decode(frame, true) {
                last = Some(picture);
            }
        }
        super::super::helper::to_image(&last.expect("pictures").yuv, false).expect("converts")
    }

    /// Two cameras on the one thread, each with its own decoder: one
    /// joined mid-stream waits for its keyframe without holding the other
    /// up; pictures come at the tile's size; a picture the window has not
    /// taken holds the next ones back in the helper until it does, and
    /// then only the newest comes; a stopped camera's frames go nowhere.
    #[test]
    fn one_thread_decodes_each_camera_on_its_own() {
        let frames = h264::access_units(include_bytes!("fixtures/camera-480x480.h264"));
        let gallery = Gallery::new(|| {});
        gallery.set_fit(160, 120);
        let mut decoding = CameraDecoding::spawn(gallery.clone()).expect("a thread");
        decoding.start("ana");
        decoding.start("bob");
        for n in 0..6 {
            decoding.push("ana", frames[n].clone(), true);
            // Bob joined late: P frames, then his keyframe at 44.
            decoding.push("bob", frames[40 + n].clone(), true);
        }
        // Ana's first and Bob's 44: the window has taken neither, so the
        // frames after them are decoded but kept back.
        wait_for(&gallery, 2);
        // The rest of the frames reach the thread.
        decoding.push("ana", frames[6].clone(), true);
        std::thread::sleep(Duration::from_millis(300));
        assert_eq!(
            gallery.pictures(),
            2,
            "nothing sent the window would not see"
        );
        assert!(gallery.take_keyframe_wish("bob"), "Bob asked for one");
        let taken = gallery.take();
        assert_eq!(taken.len(), 2);
        let ana = &taken["ana"];
        assert_eq!(ana.source, [480, 480]);
        assert_eq!(ana.image.size, [160, 160], "a third: still covers 160x120");
        assert_eq!(*ana.image, reference(&frames[..1], (160, 120)));
        // Taken: each camera's newest kept picture comes, no other.
        wait_for(&gallery, 4);
        std::thread::sleep(Duration::from_millis(100));
        assert_eq!(gallery.pictures(), 4);
        let taken = gallery.take();
        assert_eq!(*taken["ana"].image, reference(&frames[..7], (160, 120)));
        assert_eq!(
            *taken["bob"].image,
            reference(&frames[44..46], (160, 120)),
            "Bob's 45"
        );
        // Nothing kept now: taking brings nothing more.
        std::thread::sleep(Duration::from_millis(100));
        assert_eq!(gallery.pictures(), 4);
        // While the window cannot be seen, nothing is sent; when it can
        // again, the newest comes.
        gallery.set_visible(false);
        for frame in &frames[7..10] {
            decoding.push("ana", frame.clone(), true);
        }
        std::thread::sleep(Duration::from_millis(300));
        assert_eq!(gallery.pictures(), 4, "hidden: nothing sent");
        gallery.set_visible(true);
        wait_for(&gallery, 5);
        assert_eq!(
            *gallery.take()["ana"].image,
            reference(&frames[..10], (160, 120))
        );
        decoding.stop("ana");
        decoding.push("ana", frames[0].clone(), true);
        std::thread::sleep(Duration::from_millis(100));
        assert!(gallery.take().is_empty(), "stopped: nothing decodes");
        drop(decoding);
    }

    /// What 4 and 9 cameras cost the decoder thread: every frame of the
    /// 480×480 fixture for each, one frame at a time for all of them, at
    /// a 320×240 tile. Prints milliseconds a picture and the share of a
    /// core at 22 frames a second. Ignored: it measures, it does not
    /// check; run it in release.
    /// `cargo test --release --features huddle-video -- --ignored cameras_cost --nocapture`
    #[test]
    #[ignore = "a measurement, best in release"]
    #[allow(clippy::print_stdout, reason = "the measurement is for the reader")]
    fn cameras_cost() {
        let frames = h264::access_units(include_bytes!("fixtures/camera-480x480.h264"));
        for cameras in [4usize, 9] {
            let gallery = Gallery::new(|| {});
            gallery.set_fit(320, 240);
            let mut decoding = CameraDecoding::spawn(gallery.clone()).expect("a thread");
            let keys: Vec<String> = (0..cameras).map(|n| format!("camera-{n}")).collect();
            for key in &keys {
                decoding.start(key);
            }
            let started = Instant::now();
            for (n, frame) in frames.iter().enumerate() {
                for key in &keys {
                    decoding.push(key, frame.clone(), true);
                }
                let until = Instant::now() + Duration::from_secs(20);
                while gallery.pictures() < (n + 1) * cameras && Instant::now() < until {
                    std::thread::yield_now();
                }
                // As the window does: the next pictures are then sent.
                gallery.take();
            }
            let took = started.elapsed().as_secs_f64() * 1000.0;
            let pictures = gallery.pictures();
            assert_eq!(pictures, frames.len() * cameras);
            let per = took / pictures as f64;
            println!(
                "{cameras} cameras: {pictures} pictures in {took:.0} ms, {per:.2} ms a picture, \
                 {:.0} % of a core at 22 fps",
                per * 22.0 * cameras as f64 / 10.0
            );
        }
    }
}
