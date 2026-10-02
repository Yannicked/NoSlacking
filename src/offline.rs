//! What NoSlacking keeps on disk so a workspace opens at once, and can be
//! read without a network: each workspace's conversation list and people,
//! and the newest page of each conversation it opened.
//!
//! Everything is encrypted with XChaCha20-Poly1305 under a random key that
//! lives only in the OS keyring (see [`crate::credentials::Credentials::cache_key`]),
//! so the files say nothing to whoever copies the disk without the
//! keyring. Each file's place (workspace and name) is bound in as
//! associated data, so a file moved to another name does not open there.
//!
//! The history kept is capped per workspace ([`MAX_HISTORY_FILES`],
//! [`MAX_HISTORY_BYTES`]), dropping the conversations opened longest ago,
//! and a workspace's files go when it is signed out.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::SystemTime;

use chacha20poly1305::aead::{Aead as _, KeyInit as _, Payload};
use chacha20poly1305::{XChaCha20Poly1305, XNonce};
use rand::Rng as _;

/// The start of every file, naming its format.
const MAGIC: &[u8; 4] = b"NSO1";
const NONCE_LEN: usize = 24;
/// Conversations whose newest page is kept, per workspace.
pub const MAX_HISTORY_FILES: usize = 300;
/// The most their pages may take together, per workspace.
pub const MAX_HISTORY_BYTES: u64 = 48 * 1024 * 1024;
/// A single file larger than this is not worth keeping.
const MAX_FILE_BYTES: usize = 8 * 1024 * 1024;
/// The prefix of history files, so pruning leaves the lists alone.
const HISTORY_PREFIX: &str = "h-";

/// The key the cache is encrypted with. Never printed.
#[derive(Clone, PartialEq, Eq)]
pub struct CacheKey(pub [u8; 32]);

impl std::fmt::Debug for CacheKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(crate::redact::REDACTED)
    }
}

impl CacheKey {
    /// A fresh random key.
    pub fn random() -> Self {
        let mut key = [0u8; 32];
        rand::rng().fill_bytes(&mut key);
        Self(key)
    }
}

/// The encrypted cache, or none when the keyring held no key for it.
/// Cheap to clone.
#[derive(Clone)]
pub struct Cache {
    inner: Option<Arc<Inner>>,
}

struct Inner {
    root: PathBuf,
    cipher: XChaCha20Poly1305,
}

impl std::fmt::Debug for Cache {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Cache")
            .field("root", &self.inner.as_ref().map(|inner| &inner.root))
            .finish_non_exhaustive()
    }
}

impl Cache {
    /// A cache under `root`, encrypted with `key`.
    pub fn new(root: PathBuf, key: &CacheKey) -> Self {
        let cipher = XChaCha20Poly1305::new(&chacha20poly1305::Key::from(key.0));
        Self {
            inner: Some(Arc::new(Inner { root, cipher })),
        }
    }

    /// No cache: nothing is read or written.
    pub fn disabled() -> Self {
        Self { inner: None }
    }

    /// Whether anything is kept.
    pub fn is_enabled(&self) -> bool {
        self.inner.is_some()
    }

    /// A value kept under `name` for `team`, if one is there and opens.
    pub fn read<T: serde::de::DeserializeOwned>(&self, team: &str, name: &str) -> Option<T> {
        let inner = self.inner.as_ref()?;
        let path = inner.path(team, name);
        let sealed = std::fs::read(&path).ok()?;
        let Some(plain) = open(&inner.cipher, &place(team, name), &sealed) else {
            // Another key's file, or a damaged one: it will be replaced.
            log::debug!("offline cache: {} does not open", path.display());
            return None;
        };
        serde_json::from_slice(&plain).ok()
    }

    /// Keeps `value` under `name` for `team`, replacing what was there.
    pub fn write<T: serde::Serialize>(&self, team: &str, name: &str, value: &T) {
        let Some(inner) = self.inner.as_ref() else {
            return;
        };
        let plain = match serde_json::to_vec(value) {
            Ok(plain) if plain.len() <= MAX_FILE_BYTES => plain,
            Ok(_) => return,
            Err(error) => {
                log::debug!("offline cache: not encoded: {error}");
                return;
            }
        };
        let Some(sealed) = seal(&inner.cipher, &place(team, name), &plain) else {
            return;
        };
        let path = inner.path(team, name);
        if let Some(dir) = path.parent()
            && let Err(error) = std::fs::create_dir_all(dir)
        {
            log::debug!("offline cache: no folder: {error}");
            return;
        }
        if let Err(error) = crate::paths::write_atomic(&path, &sealed) {
            log::debug!("offline cache: not written: {error}");
        }
    }

    /// The newest page of a conversation, as Slack sent it.
    pub fn read_history(&self, team: &str, channel: &str) -> Option<serde_json::Value> {
        self.read(team, &history_name(channel))
    }

    /// Keeps the newest page of a conversation, then trims the workspace's
    /// history to its caps.
    pub fn write_history(&self, team: &str, channel: &str, page: &serde_json::Value) {
        self.write(team, &history_name(channel), page);
        if let Some(inner) = self.inner.as_ref() {
            inner.prune(team);
        }
    }

    /// Removes everything kept for `team`.
    pub fn wipe(&self, team: &str) {
        if let Some(inner) = self.inner.as_ref() {
            let dir = inner.root.join(safe(team));
            match std::fs::remove_dir_all(&dir) {
                Ok(()) => log::info!("offline cache of {team} removed"),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => log::warn!("could not remove the offline cache: {error}"),
            }
        }
    }
}

impl Inner {
    fn path(&self, team: &str, name: &str) -> PathBuf {
        self.root
            .join(safe(team))
            .join(format!("{}.bin", safe(name)))
    }

    fn prune(&self, team: &str) {
        let dir = self.root.join(safe(team));
        let Ok(entries) = std::fs::read_dir(&dir) else {
            return;
        };
        let files: Vec<Kept> = entries
            .filter_map(Result::ok)
            .filter(|entry| {
                entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with(HISTORY_PREFIX)
            })
            .filter_map(|entry| {
                let metadata = entry.metadata().ok()?;
                Some(Kept {
                    path: entry.path(),
                    bytes: metadata.len(),
                    modified: metadata.modified().unwrap_or(SystemTime::UNIX_EPOCH),
                })
            })
            .collect();
        for path in over_cap(files, MAX_HISTORY_FILES, MAX_HISTORY_BYTES) {
            let _ = std::fs::remove_file(path);
        }
    }
}

/// A kept file, for pruning.
#[derive(Clone, Debug)]
struct Kept {
    path: PathBuf,
    bytes: u64,
    modified: SystemTime,
}

/// The files to drop so at most `max_files` taking at most `max_bytes`
/// stay, keeping the most recently written.
fn over_cap(mut files: Vec<Kept>, max_files: usize, max_bytes: u64) -> Vec<PathBuf> {
    files.sort_by_key(|file| std::cmp::Reverse(file.modified));
    let mut total = 0u64;
    let mut dropped = Vec::new();
    for (index, file) in files.into_iter().enumerate() {
        total = total.saturating_add(file.bytes);
        if index >= max_files || total > max_bytes {
            dropped.push(file.path);
        }
    }
    dropped
}

fn history_name(channel: &str) -> String {
    format!("{HISTORY_PREFIX}{channel}")
}

/// What a file is bound to: its workspace and name.
fn place(team: &str, name: &str) -> String {
    format!("{team}/{name}")
}

/// An id as a file name. Slack ids are `[A-Z0-9]`, but never trust that.
fn safe(id: &str) -> String {
    id.chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
        .collect()
}

/// `MAGIC`, a random nonce, and the plaintext encrypted and authenticated
/// together with `aad`.
fn seal(cipher: &XChaCha20Poly1305, aad: &str, plain: &[u8]) -> Option<Vec<u8>> {
    let mut nonce = [0u8; NONCE_LEN];
    rand::rng().fill_bytes(&mut nonce);
    let sealed = cipher
        .encrypt(
            &XNonce::from(nonce),
            Payload {
                msg: plain,
                aad: aad.as_bytes(),
            },
        )
        .ok()?;
    let mut out = Vec::with_capacity(MAGIC.len() + NONCE_LEN + sealed.len());
    out.extend_from_slice(MAGIC);
    out.extend_from_slice(&nonce);
    out.extend(sealed);
    Some(out)
}

/// The plaintext of a [`seal`]ed file, if it is one, for this place, under
/// this key.
fn open(cipher: &XChaCha20Poly1305, aad: &str, sealed: &[u8]) -> Option<Vec<u8>> {
    let rest = sealed.strip_prefix(MAGIC.as_slice())?;
    let (nonce, ciphertext) = rest.split_at_checked(NONCE_LEN)?;
    let nonce: [u8; NONCE_LEN] = nonce.try_into().ok()?;
    cipher
        .decrypt(
            &XNonce::from(nonce),
            Payload {
                msg: ciphertext,
                aad: aad.as_bytes(),
            },
        )
        .ok()
}

/// Where the cache lives under the app's cache folder.
pub fn root(cache_dir: &Path) -> PathBuf {
    cache_dir.join("offline")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cipher(byte: u8) -> XChaCha20Poly1305 {
        XChaCha20Poly1305::new(&chacha20poly1305::Key::from([byte; 32]))
    }

    #[test]
    fn sealed_files_open_only_with_their_key_and_place() {
        let sealed = seal(&cipher(1), "T1/h-C1", b"hello").expect("seals");
        assert!(sealed.starts_with(MAGIC));
        assert!(!sealed.windows(5).any(|w| w == b"hello"));
        assert_eq!(
            open(&cipher(1), "T1/h-C1", &sealed),
            Some(b"hello".to_vec())
        );
        // Another key, another place, or a flipped bit: nothing.
        assert_eq!(open(&cipher(2), "T1/h-C1", &sealed), None);
        assert_eq!(open(&cipher(1), "T1/h-C2", &sealed), None);
        let mut damaged = sealed.clone();
        if let Some(last) = damaged.last_mut() {
            *last ^= 1;
        }
        assert_eq!(open(&cipher(1), "T1/h-C1", &damaged), None);
        assert_eq!(open(&cipher(1), "T1/h-C1", b"NSO1short"), None);
        assert_eq!(open(&cipher(1), "T1/h-C1", b""), None);
        // A fresh nonce each time.
        assert_ne!(seal(&cipher(1), "x", b"a"), seal(&cipher(1), "x", b"a"));
    }

    #[test]
    fn the_newest_files_stay_within_the_caps() {
        let at = |seconds: u64, bytes: u64, name: &str| Kept {
            path: PathBuf::from(name),
            bytes,
            modified: SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(seconds),
        };
        let files = vec![at(1, 10, "old"), at(3, 10, "new"), at(2, 10, "mid")];
        assert_eq!(over_cap(files.clone(), 2, 1_000), [PathBuf::from("old")]);
        assert_eq!(
            over_cap(files.clone(), 10, 15),
            [PathBuf::from("mid"), PathBuf::from("old")]
        );
        assert!(over_cap(files, 3, 30).is_empty());
    }

    #[test]
    fn ids_cannot_leave_the_folder() {
        assert_eq!(safe("../../etc/T1"), "etcT1");
        assert_eq!(history_name("C1"), "h-C1");
    }

    #[test]
    fn values_round_trip_through_the_disk_and_go_on_sign_out() {
        let root =
            std::env::temp_dir().join(format!("noslacking-offline-test-{}", std::process::id()));
        let key = CacheKey([7; 32]);
        let cache = Cache::new(root.clone(), &key);
        let page =
            serde_json::json!({"ok": true, "messages": [{"ts": "1.0", "text": "secret words"}]});
        cache.write_history("T1", "C1", &page);
        assert_eq!(cache.read_history("T1", "C1"), Some(page));
        let on_disk = std::fs::read(root.join("T1").join("h-C1.bin")).expect("written");
        assert!(!on_disk.windows(6).any(|w| w == b"secret"));
        cache.write("T1", "users", &vec!["U1".to_owned()]);
        assert_eq!(
            cache.read::<Vec<String>>("T1", "users"),
            Some(vec!["U1".to_owned()])
        );
        // Another key reads nothing.
        let other = Cache::new(root.clone(), &CacheKey([8; 32]));
        assert_eq!(other.read_history("T1", "C1"), None);
        // Nor does a disabled cache, which writes nothing either.
        assert_eq!(Cache::disabled().read_history("T1", "C1"), None);
        cache.wipe("T1");
        assert_eq!(cache.read_history("T1", "C1"), None);
        assert!(!root.join("T1").exists());
        let _ = std::fs::remove_dir_all(&root);
        assert_eq!(format!("{key:?}"), crate::redact::REDACTED);
    }
}
