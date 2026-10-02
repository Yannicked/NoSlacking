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

    pub fn remove_client(&self, team: &str) {
        write(&self.inner.clients).remove(team);
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
    async fn fetch(&self, team: Option<&str>, url: &str) -> Result<Vec<u8>, String> {
        let path = self.cache_dir.join(cache_name(url));
        if let Ok(bytes) = tokio::fs::read(&path).await
            && !bytes.is_empty()
        {
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

    #[test]
    fn cache_names_are_stable_hex() {
        assert_eq!(cache_name("a"), "86f7e437faa5a7fce15d1ddcb9eaeaea377667b8");
    }
}
