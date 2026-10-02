//! Avatars, custom emoji, workspace icons and file previews: fetched once,
//! kept on disk, decoded by egui on demand.
//!
//! Public images (avatars, emoji) are plain `https://` URIs. Files need the
//! workspace's token, so their URI names the team: `nsauth:T0123:https://…`
//! (see [`authed`]). The token never appears in the URI itself.
//!
//! Bytes are held in memory up to a budget and then evicted oldest first;
//! egui keeps the decoded texture, and asks again (served from disk) only
//! if it forgets it.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, RwLock};
use std::time::Instant;

use egui::load::{Bytes, BytesLoadResult, BytesLoader, BytesPoll, LoadError};
use sha1::{Digest as _, Sha1};

use crate::slack;

/// Bytes held in memory at most.
const HELD_BYTES: usize = 96 * 1024 * 1024;
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

enum Entry {
    Pending,
    Ready { bytes: Arc<[u8]>, used: Instant },
    Failed(String),
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
    pub fn new(http: reqwest::Client, runtime: tokio::runtime::Handle, cache_dir: PathBuf) -> Self {
        let _ = std::fs::create_dir_all(&cache_dir);
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

    /// Lets the loader fetch `team`'s files.
    pub fn set_client(&self, team: &str, client: slack::Client) {
        write(&self.inner.clients).insert(team.to_owned(), client);
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

    async fn fetch(&self, team: Option<&str>, url: &str) -> Result<Vec<u8>, String> {
        let path = match team {
            Some(team) => self.private_dir(team).join(cache_name(url)),
            None => self.cache_dir.join(cache_name(url)),
        };
        if let Ok(bytes) = tokio::fs::read(&path).await
            && !bytes.is_empty()
        {
            check_decoded_size(&bytes)?;
            return Ok(bytes);
        }
        let bytes = match team {
            Some(team) => {
                let client = read(&self.clients).get(team).cloned();
                let client = client.ok_or_else(|| "workspace signed out".to_owned())?;
                client.get_bytes(url, MAX_IMAGE_BYTES).await
            }
            None => slack::client::get_bytes(&self.http, url, None, None, MAX_IMAGE_BYTES).await,
        }
        .map_err(|e| e.to_string())?;
        check_decoded_size(&bytes)?;
        if let Err(error) = crate::paths::write_atomic(&path, &bytes) {
            log::debug!("image not cached: {error}");
        }
        Ok(bytes)
    }

    fn evict(&self, entries: &mut HashMap<String, Entry>, keep: &str) {
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
                Entry::Ready { bytes, used } if uri != keep => {
                    Some((uri.clone(), *used, bytes.len()))
                }
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
            match entries.get_mut(uri) {
                Some(Entry::Ready { bytes, used }) => {
                    *used = Instant::now();
                    return Ok(BytesPoll::Ready {
                        size: None,
                        bytes: Bytes::Shared(bytes.clone()),
                        mime: None,
                    });
                }
                Some(Entry::Pending) => return Ok(BytesPoll::Pending { size: None }),
                Some(Entry::Failed(error)) => return Err(LoadError::Loading(error.clone())),
                None => {
                    entries.insert(uri.to_owned(), Entry::Pending);
                }
            }
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
            if !matches!(entries.get(&uri), Some(Entry::Pending)) {
                return;
            }
            match result {
                Ok(bytes) => {
                    entries.insert(
                        uri.clone(),
                        Entry::Ready {
                            bytes: bytes.into(),
                            used: Instant::now(),
                        },
                    );
                    inner.evict(&mut entries, &uri);
                }
                Err(error) => {
                    log::debug!("image failed: {error}");
                    entries.insert(uri, Entry::Failed(error));
                }
            }
            drop(entries);
            ctx.request_repaint();
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
            .any(|e| matches!(e, Entry::Pending))
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
    fn cache_names_are_stable_hex() {
        assert_eq!(cache_name("a"), "86f7e437faa5a7fce15d1ddcb9eaeaea377667b8");
    }
}
