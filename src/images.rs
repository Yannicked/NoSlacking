//! Avatars, custom emoji, workspace icons and file previews: fetched once,
//! kept on disk, decoded by egui on demand.
//!
//! Public images (avatars, emoji) are plain `https://` URIs. Files need the
//! workspace's token, so their URI names the team: `nsauth:T0123:https://…`
//! (see [`authed`]). The token never appears in the URI itself.
//!
//! Bytes are held in memory up to a budget and then evicted oldest first;
//! egui keeps the decoded texture, and asks again (served from disk) only
//! if it forgets it. The disk cache is trimmed to [`DISK_BYTES`] at start,
//! least recently used first.
//!
//! A failed fetch is tried again after a wait that doubles each time, so a
//! dropped connection does not leave a broken image for the whole session.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant, SystemTime};

use egui::load::{Bytes, BytesLoadResult, BytesLoader, BytesPoll, LoadError};
use sha1::{Digest as _, Sha1};

use crate::slack;

/// Bytes held in memory at most.
const HELD_BYTES: usize = 96 * 1024 * 1024;
/// The disk cache is trimmed to this at start, least recently used first.
pub const DISK_BYTES: u64 = 512 * 1024 * 1024;
/// The largest single image fetched.
const MAX_IMAGE_BYTES: usize = 24 * 1024 * 1024;
const PREFIX: &str = "nsauth:";
/// The widest or tallest image decoded.
const MAX_SIDE: u32 = 16_384;
/// The most pixels in one decoded frame (about 160 MB as RGBA).
const MAX_PIXELS: u64 = 40_000_000;
/// The most memory every frame of an animation may decode to, as RGBA.
const MAX_DECODED_BYTES: u64 = 256 * 1024 * 1024;

/// The URI of an image that needs `team`'s token.
pub fn authed(team: &str, url: &str) -> String {
    format!("{PREFIX}{team}:{url}")
}

/// The team and URL in a URI from [`authed`].
fn split(uri: &str) -> Option<(Option<&str>, &str)> {
    if let Some(rest) = uri.strip_prefix(PREFIX) {
        let (team, url) = rest.split_once(':')?;
        return url.starts_with("https://").then_some((Some(team), url));
    }
    (uri.starts_with("https://") || uri.starts_with("http://")).then_some((None, uri))
}

/// Refuses an image that would decode to far more memory than its download
/// size suggests (a "decompression bomb"): egui decodes whatever it is
/// handed, and every frame of a GIF at once. Reading the header and
/// walking a GIF's blocks costs no decoding.
fn check_decoded_size(bytes: &[u8]) -> Result<(), String> {
    let reader = image::ImageReader::new(std::io::Cursor::new(bytes))
        .with_guessed_format()
        .map_err(|e| e.to_string())?;
    let format = reader.format();
    let Ok((width, height)) = reader.into_dimensions() else {
        // Not a raster format `image` knows (an SVG, say); egui's own
        // loaders decide.
        return Ok(());
    };
    let pixels = u64::from(width) * u64::from(height);
    if width > MAX_SIDE || height > MAX_SIDE || pixels > MAX_PIXELS {
        return Err(format!("image too large to show ({width}×{height})"));
    }
    if format == Some(image::ImageFormat::Gif) {
        let frames = gif_frames(bytes).ok_or_else(|| "damaged GIF".to_owned())?;
        if frames.saturating_mul(pixels).saturating_mul(4) > MAX_DECODED_BYTES {
            return Err(format!("animation too large to show ({frames} frames)"));
        }
    }
    Ok(())
}

/// The number of frames in a GIF, from its block structure alone, or `None`
/// when the file is cut short or malformed.
fn gif_frames(bytes: &[u8]) -> Option<u64> {
    /// Skips a chain of data sub-blocks, ending at the zero-length one.
    fn sub_blocks(bytes: &[u8], mut at: usize) -> Option<usize> {
        loop {
            let size = usize::from(*bytes.get(at)?);
            at += 1 + size;
            if size == 0 {
                return Some(at);
            }
        }
    }
    fn color_table(flags: u8) -> usize {
        if flags & 0x80 == 0 {
            0
        } else {
            3 << ((flags & 0x07) + 1)
        }
    }
    if !bytes.starts_with(b"GIF") {
        return None;
    }
    let mut at = 13 + color_table(*bytes.get(10)?);
    let mut frames = 0;
    loop {
        match *bytes.get(at)? {
            // Trailer.
            0x3B => return Some(frames),
            // Extension: a label, then sub-blocks.
            0x21 => at = sub_blocks(bytes, at + 2)?,
            // Image: a 9-byte descriptor, a local color table, the LZW code
            // size, then sub-blocks.
            0x2C => {
                let flags = *bytes.get(at + 9)?;
                at = sub_blocks(bytes, at + 10 + color_table(flags) + 1)?;
                frames += 1;
            }
            _ => return None,
        }
    }
}

/// Why an image could not be had.
#[derive(Clone, Debug, PartialEq)]
enum Failure {
    /// The network or the server failed; worth asking again later.
    Fetch(String),
    /// The image arrived but is refused (too large to decode): asking again
    /// brings the same bytes.
    Refused(String),
    /// The workspace has no client yet (still starting) or any more
    /// (signed out). [`ImageLoader::set_client`] clears these.
    SignedOut,
}

impl Failure {
    fn message(&self) -> String {
        match self {
            Self::Fetch(error) | Self::Refused(error) => error.clone(),
            Self::SignedOut => "workspace signed out".to_owned(),
        }
    }
}

enum Entry {
    /// Being fetched, after `failures` failed attempts.
    Pending {
        failures: u32,
    },
    Ready {
        bytes: Arc<[u8]>,
        used: Instant,
    },
    Failed {
        failure: Failure,
        at: Instant,
        /// Failed attempts in a row, this one included.
        failures: u32,
    },
}

/// What a failed download means for asking again: a client error (a file
/// that is gone, 404, or not ours to see, 403) answers the same next time,
/// while timeouts, rate limits and server trouble may pass.
fn failure_for(error: slack::SlackError) -> Failure {
    match error {
        slack::SlackError::Http(status)
            if (400..500).contains(&status) && status != 408 && status != 429 =>
        {
            Failure::Refused(error.to_string())
        }
        error => Failure::Fetch(error.to_string()),
    }
}

/// How long after its `failures`-th failure in a row an image is asked for
/// again, or `None` for never: from 5 seconds, doubling, up to 10 minutes.
fn retry_delay(failure: &Failure, failures: u32) -> Option<Duration> {
    const FIRST: Duration = Duration::from_secs(5);
    const LONGEST: Duration = Duration::from_secs(600);
    match failure {
        Failure::Fetch(_) => {
            let doublings = failures.saturating_sub(1).min(16);
            Some(FIRST.saturating_mul(1 << doublings).min(LONGEST))
        }
        Failure::Refused(_) => None,
        // Retried when the client arrives, not on a timer.
        Failure::SignedOut => None,
    }
}

struct Inner {
    entries: Mutex<HashMap<String, Entry>>,
    clients: RwLock<HashMap<String, slack::Client>>,
    http: reqwest::Client,
    runtime: tokio::runtime::Handle,
    cache_dir: PathBuf,
}

/// The loader egui asks for every image URI. Cheap to clone.
#[derive(Clone)]
pub struct ImageLoader {
    inner: Arc<Inner>,
}

impl ImageLoader {
    /// A loader caching on disk in `cache_dir`. Trims that folder to
    /// [`DISK_BYTES`] in the background first.
    pub fn new(http: reqwest::Client, runtime: tokio::runtime::Handle, cache_dir: PathBuf) -> Self {
        let _ = std::fs::create_dir_all(&cache_dir);
        let dir = cache_dir.clone();
        runtime.spawn_blocking(move || prune_disk(&dir, DISK_BYTES));
        Self {
            inner: Arc::new(Inner {
                entries: Mutex::new(HashMap::new()),
                clients: RwLock::new(HashMap::new()),
                http,
                runtime,
                cache_dir,
            }),
        }
    }

    /// Lets the loader fetch `team`'s files, including those asked for
    /// before the workspace was ready.
    pub fn set_client(&self, team: &str, client: slack::Client) {
        write(&self.inner.clients).insert(team.to_owned(), client);
        let prefix = authed(team, "");
        lock(&self.inner.entries).retain(|uri, entry| {
            !(uri.starts_with(&prefix)
                && matches!(
                    entry,
                    Entry::Failed {
                        failure: Failure::SignedOut,
                        ..
                    }
                ))
        });
    }

    /// Where a file of `team`'s at `url` is kept to be opened in another
    /// app (a video, a sound): with the workspace's other private files,
    /// so signing out deletes it and the cache's size limit covers it,
    /// in a folder of its own so it keeps its real name for the player.
    pub fn open_dir(&self, team: &str, url: &str) -> PathBuf {
        self.inner
            .private_dir(team)
            .join("open")
            .join(cache_name(url))
    }

    /// Stops fetching `team`'s files and deletes the ones already fetched,
    /// from memory and from disk: they are private to the workspace.
    pub fn remove_client(&self, team: &str) {
        write(&self.inner.clients).remove(team);
        let prefix = authed(team, "");
        lock(&self.inner.entries).retain(|uri, _| !uri.starts_with(&prefix));
        let dir = self.inner.private_dir(team);
        self.inner.runtime.spawn(async move {
            match tokio::fs::remove_dir_all(&dir).await {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => log::warn!("could not delete a workspace's cached files: {error}"),
            }
        });
    }
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn write<T>(lock: &RwLock<T>) -> std::sync::RwLockWriteGuard<'_, T> {
    lock.write()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn read<T>(lock: &RwLock<T>) -> std::sync::RwLockReadGuard<'_, T> {
    lock.read()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn cache_name(url: &str) -> String {
    let digest = Sha1::digest(url.as_bytes());
    digest.iter().map(|b| format!("{b:02x}")).collect()
}

impl Inner {
    /// Where `team`'s private files are cached, apart from public images so
    /// signing out can delete them all.
    fn private_dir(&self, team: &str) -> PathBuf {
        // Team ids are `T` and alphanumerics; anything else is hashed so it
        // can never name a path outside the cache.
        let name = if !team.is_empty() && team.bytes().all(|b| b.is_ascii_alphanumeric()) {
            team.to_owned()
        } else {
            cache_name(team)
        };
        self.cache_dir.join("private").join(name)
    }

    async fn fetch(&self, team: Option<&str>, url: &str) -> Result<Vec<u8>, Failure> {
        let path = match team {
            Some(team) => self.private_dir(team).join(cache_name(url)),
            None => self.cache_dir.join(cache_name(url)),
        };
        if let Ok(bytes) = tokio::fs::read(&path).await
            && !bytes.is_empty()
        {
            check_decoded_size(&bytes).map_err(Failure::Refused)?;
            // The disk cache is trimmed oldest first by modification time;
            // mark this one as recently used.
            tokio::task::spawn_blocking(move || touch(&path));
            return Ok(bytes);
        }
        let bytes = match team {
            Some(team) => {
                let client = read(&self.clients).get(team).cloned();
                let client = client.ok_or(Failure::SignedOut)?;
                client.get_bytes(url, MAX_IMAGE_BYTES).await
            }
            None => slack::client::get_bytes(&self.http, url, None, None, MAX_IMAGE_BYTES).await,
        }
        .map_err(failure_for)?;
        check_decoded_size(&bytes).map_err(Failure::Refused)?;
        if let Err(error) = crate::paths::write_atomic(&path, &bytes) {
            log::debug!("image not cached: {error}");
        }
        Ok(bytes)
    }
}

/// Drops the least recently used bytes once more than [`HELD_BYTES`] are
/// held, down to three quarters of it, never dropping `keep` (the image
/// that just arrived).
fn evict(entries: &mut HashMap<String, Entry>, keep: &str) {
    let mut held: usize = entries
        .values()
        .map(|e| match e {
            Entry::Ready { bytes, .. } => bytes.len(),
            _ => 0,
        })
        .sum();
    if held <= HELD_BYTES {
        return;
    }
    let mut ready: Vec<(String, Instant, usize)> = entries
        .iter()
        .filter_map(|(uri, e)| match e {
            Entry::Ready { bytes, used } if uri != keep => Some((uri.clone(), *used, bytes.len())),
            _ => None,
        })
        .collect();
    ready.sort_by_key(|(_, used, _)| *used);
    for (uri, _, size) in ready {
        if held <= HELD_BYTES * 3 / 4 {
            break;
        }
        entries.remove(&uri);
        held -= size;
    }
}

/// Sets a cached file's modification time to now, so trimming the disk
/// cache keeps it.
fn touch(path: &Path) {
    let touched = std::fs::OpenOptions::new()
        .write(true)
        .open(path)
        .and_then(|file| file.set_modified(SystemTime::now()));
    if let Err(error) = touched {
        log::debug!("could not mark a cached image as used: {error}");
    }
}

/// One file in the disk cache.
#[derive(Clone, Debug, PartialEq)]
struct CachedFile {
    path: PathBuf,
    size: u64,
    modified: SystemTime,
}

/// The files to delete so the rest fit in `cap` bytes: the least recently
/// used first. Trimming goes down to nine tenths of the cap, so the next
/// start does not trim again straight away.
fn to_prune(mut files: Vec<CachedFile>, cap: u64) -> Vec<PathBuf> {
    let mut total: u64 = files.iter().map(|f| f.size).sum();
    if total <= cap {
        return Vec::new();
    }
    let target = cap / 10 * 9;
    files.sort_by_key(|f| f.modified);
    let mut doomed = Vec::new();
    for file in files {
        if total <= target {
            break;
        }
        total = total.saturating_sub(file.size);
        doomed.push(file.path);
    }
    doomed
}

/// Every file under `dir`, the per-workspace folders included.
fn cached_files(dir: &Path) -> Vec<CachedFile> {
    let mut files = Vec::new();
    let mut folders = vec![dir.to_path_buf()];
    while let Some(folder) = folders.pop() {
        let Ok(entries) = std::fs::read_dir(&folder) else {
            continue;
        };
        for entry in entries.flatten() {
            let Ok(meta) = entry.metadata() else {
                continue;
            };
            if meta.is_dir() {
                folders.push(entry.path());
            } else if meta.is_file() {
                files.push(CachedFile {
                    path: entry.path(),
                    size: meta.len(),
                    modified: meta.modified().unwrap_or(SystemTime::UNIX_EPOCH),
                });
            }
        }
    }
    files
}

/// Deletes the least recently used files under `dir` until it holds at
/// most `cap` bytes.
fn prune_disk(dir: &Path, cap: u64) {
    let doomed = to_prune(cached_files(dir), cap);
    if doomed.is_empty() {
        return;
    }
    log::debug!("trimming the image cache by {} files", doomed.len());
    for path in doomed {
        if let Err(error) = std::fs::remove_file(&path) {
            log::debug!("could not delete a cached image: {error}");
        }
    }
}

impl BytesLoader for ImageLoader {
    fn id(&self) -> &str {
        egui::generate_loader_id!(ImageLoader)
    }

    fn load(&self, ctx: &egui::Context, uri: &str) -> BytesLoadResult {
        let Some((team, url)) = split(uri) else {
            return Err(LoadError::NotSupported);
        };
        {
            let mut entries = lock(&self.inner.entries);
            let failures = match entries.get_mut(uri) {
                Some(Entry::Ready { bytes, used }) => {
                    *used = Instant::now();
                    return Ok(BytesPoll::Ready {
                        size: None,
                        bytes: Bytes::Shared(bytes.clone()),
                        mime: None,
                    });
                }
                Some(Entry::Pending { .. }) => return Ok(BytesPoll::Pending { size: None }),
                Some(Entry::Failed {
                    failure,
                    at,
                    failures,
                }) => {
                    let due = retry_delay(failure, *failures).is_some_and(|d| at.elapsed() >= d);
                    if !due {
                        return Err(LoadError::Loading(failure.message()));
                    }
                    *failures
                }
                None => 0,
            };
            entries.insert(uri.to_owned(), Entry::Pending { failures });
        }
        let inner = self.inner.clone();
        let ctx = ctx.clone();
        let uri = uri.to_owned();
        let team = team.map(str::to_owned);
        let url = url.to_owned();
        self.inner.runtime.spawn(async move {
            let result = inner.fetch(team.as_deref(), &url).await;
            let mut entries = lock(&inner.entries);
            // A `forget` while fetching drops the result.
            let Some(&Entry::Pending { failures }) = entries.get(&uri) else {
                return;
            };
            let mut retry = None;
            match result {
                Ok(bytes) => {
                    entries.insert(
                        uri.clone(),
                        Entry::Ready {
                            bytes: bytes.into(),
                            used: Instant::now(),
                        },
                    );
                    evict(&mut entries, &uri);
                }
                Err(failure) => {
                    let failures = failures.saturating_add(1);
                    log::debug!("image failed ({failures} times): {}", failure.message());
                    retry = retry_delay(&failure, failures);
                    entries.insert(
                        uri,
                        Entry::Failed {
                            failure,
                            at: Instant::now(),
                            failures,
                        },
                    );
                }
            }
            drop(entries);
            ctx.request_repaint();
            // Ask again once the wait is over, if the image is still shown.
            if let Some(retry) = retry {
                ctx.request_repaint_after(retry);
            }
        });
        Ok(BytesPoll::Pending { size: None })
    }

    fn forget(&self, uri: &str) {
        lock(&self.inner.entries).remove(uri);
    }

    fn forget_all(&self) {
        lock(&self.inner.entries).clear();
    }

    fn byte_size(&self) -> usize {
        lock(&self.inner.entries)
            .values()
            .map(|e| match e {
                Entry::Ready { bytes, .. } => bytes.len(),
                _ => 0,
            })
            .sum()
    }

    fn has_pending(&self) -> bool {
        lock(&self.inner.entries)
            .values()
            .any(|e| matches!(e, Entry::Pending { .. }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn authed_uris_carry_the_team_not_the_token() {
        let uri = authed("T01", "https://files.slack.com/files-pri/T01-F1/a.png");
        assert_eq!(
            split(&uri),
            Some((
                Some("T01"),
                "https://files.slack.com/files-pri/T01-F1/a.png"
            ))
        );
        assert_eq!(
            split("https://avatars.slack-edge.com/a.png"),
            Some((None, "https://avatars.slack-edge.com/a.png"))
        );
        assert_eq!(split("nsauth:T01:file:///etc/passwd"), None);
        assert_eq!(split("bytes://icon.svg"), None);
    }

    /// A GIF of `frames` 1×1 frames, with a `width`×`height` screen.
    fn gif(width: u16, height: u16, frames: usize) -> Vec<u8> {
        let mut bytes = b"GIF89a".to_vec();
        bytes.extend_from_slice(&width.to_le_bytes());
        bytes.extend_from_slice(&height.to_le_bytes());
        // A two-color global table.
        bytes.extend_from_slice(&[0x80, 0, 0, 0, 0, 0, 255, 255, 255]);
        // A comment extension, to be skipped.
        bytes.extend_from_slice(&[0x21, 0xFE, 2, b'h', b'i', 0]);
        for _ in 0..frames {
            bytes.extend_from_slice(&[0x2C, 0, 0, 0, 0, 1, 0, 1, 0, 0]);
            bytes.extend_from_slice(&[2, 2, 0x4C, 0x01, 0]);
        }
        bytes.push(0x3B);
        bytes
    }

    #[test]
    fn gif_frames_are_counted_without_decoding() {
        assert_eq!(gif_frames(&gif(1, 1, 3)), Some(3));
        assert_eq!(gif_frames(&gif(1, 1, 0)), Some(0));
        let mut cut = gif(1, 1, 2);
        cut.truncate(cut.len() - 4);
        assert_eq!(gif_frames(&cut), None);
        assert_eq!(gif_frames(b"PNG"), None);
    }

    #[test]
    fn decompression_bombs_are_refused() {
        assert_eq!(check_decoded_size(&gif(16, 16, 10)), Ok(()));
        // 4000×4000 RGBA is 64 MB a frame: five frames are over budget.
        assert!(check_decoded_size(&gif(4000, 4000, 5)).is_err());
        assert!(check_decoded_size(&gif(20_000, 1, 1)).is_err());
        assert!(check_decoded_size(&gif(8000, 8000, 1)).is_err());
        // Something `image` cannot size is left to egui.
        assert_eq!(check_decoded_size(b"<svg/>"), Ok(()));
    }

    #[test]
    fn failed_fetches_are_retried_later_and_refusals_never() {
        let fetch = Failure::Fetch("HTTP 503".into());
        assert_eq!(retry_delay(&fetch, 1), Some(Duration::from_secs(5)));
        assert_eq!(retry_delay(&fetch, 2), Some(Duration::from_secs(10)));
        assert_eq!(retry_delay(&fetch, 4), Some(Duration::from_secs(40)));
        assert_eq!(retry_delay(&fetch, 9), Some(Duration::from_secs(600)));
        assert_eq!(
            retry_delay(&fetch, u32::MAX),
            Some(Duration::from_secs(600))
        );
        assert_eq!(retry_delay(&Failure::Refused("too large".into()), 1), None);
        // A missing or forbidden file is not asked for again; a rate limit,
        // a timeout or a server error is.
        for gone in [404, 403, 410] {
            let failure = failure_for(slack::SlackError::Http(gone));
            assert_eq!(retry_delay(&failure, 1), None, "{gone}");
        }
        for passing in [429, 408, 500, 503] {
            let failure = failure_for(slack::SlackError::Http(passing));
            assert!(retry_delay(&failure, 1).is_some(), "{passing}");
        }
        assert!(retry_delay(&failure_for(slack::SlackError::RateLimited), 1).is_some());
        assert_eq!(retry_delay(&Failure::SignedOut, 1), None);
    }

    fn ready(size: usize, age: u64, now: Instant) -> Entry {
        Entry::Ready {
            bytes: vec![0; size].into(),
            used: now.checked_sub(Duration::from_secs(age)).unwrap_or(now),
        }
    }

    #[test]
    fn held_bytes_are_evicted_least_recently_used_first() {
        let now = Instant::now();
        let third = HELD_BYTES / 3 + 1;
        let mut entries = HashMap::from([
            ("old".to_owned(), ready(third, 30, now)),
            ("middle".to_owned(), ready(third, 20, now)),
            ("new".to_owned(), ready(third, 10, now)),
            ("pending".to_owned(), Entry::Pending { failures: 0 }),
        ]);
        evict(&mut entries, "new");
        assert!(!entries.contains_key("old"));
        assert!(entries.contains_key("middle") && entries.contains_key("new"));
        assert!(entries.contains_key("pending"));
        // The image just fetched stays even when it is the oldest.
        let mut entries = HashMap::from([
            ("kept".to_owned(), ready(HELD_BYTES + 1, 99, now)),
            ("other".to_owned(), ready(1, 1, now)),
        ]);
        evict(&mut entries, "kept");
        assert!(entries.contains_key("kept"));
        assert!(!entries.contains_key("other"));
    }

    fn file(name: &str, size: u64, age: u64) -> CachedFile {
        CachedFile {
            path: PathBuf::from(name),
            size,
            modified: SystemTime::UNIX_EPOCH + Duration::from_secs(1_000 - age),
        }
    }

    #[test]
    fn the_disk_cache_is_trimmed_least_recently_used_first() {
        let files = vec![
            file("new", 40, 1),
            file("oldest", 40, 30),
            file("old", 40, 20),
        ];
        assert!(to_prune(files.clone(), 120).is_empty(), "within the cap");
        assert_eq!(to_prune(files.clone(), 100), [PathBuf::from("oldest")]);
        // Down to nine tenths of the cap: 80 > 72, so two go.
        assert_eq!(
            to_prune(files, 80),
            [PathBuf::from("oldest"), PathBuf::from("old")]
        );
        assert!(to_prune(Vec::new(), 0).is_empty());
    }

    #[test]
    fn pruning_reaches_private_folders() {
        let dir = crate::paths::TestDir::new("image-cache");
        let private = dir.0.join("private").join("T01");
        std::fs::create_dir_all(&private).expect("dirs");
        let old = private.join("old");
        let new = dir.0.join("new");
        std::fs::write(&old, [0; 100]).expect("write");
        std::fs::write(&new, [0; 100]).expect("write");
        let stale = SystemTime::now() - Duration::from_secs(3600);
        std::fs::File::options()
            .write(true)
            .open(&old)
            .and_then(|f| f.set_modified(stale))
            .expect("age");
        assert_eq!(cached_files(&dir.0).len(), 2);
        prune_disk(&dir.0, 150);
        assert!(!old.exists(), "the older, private file goes");
        assert!(new.exists());
        // Using a file makes it the newest.
        touch(&new);
        let modified = std::fs::metadata(&new)
            .and_then(|m| m.modified())
            .expect("mtime");
        assert!(modified > stale);
    }

    #[test]
    fn cache_names_are_stable_hex() {
        assert_eq!(cache_name("a"), "86f7e437faa5a7fce15d1ddcb9eaeaea377667b8");
    }
}
