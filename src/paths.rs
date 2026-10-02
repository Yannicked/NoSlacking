//! Where NoSlacking keeps its files.
//!
//! Configuration (settings, themes) is what a user would back up; state is
//! what the app remembers between runs (read markers, the single-instance
//! port); cache is anything that can be fetched again (images, user lists).

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

/// The app's reverse-DNS identifier: the keyring service, the desktop file
/// and the window's app id.
pub const APP_ID: &str = "cloud.yannick.NoSlacking";

/// The directories the app writes to.
#[derive(Clone, Debug)]
pub struct AppDirs {
    pub config: PathBuf,
    pub state: PathBuf,
    pub cache: PathBuf,
}

impl AppDirs {
    /// The platform's directories for NoSlacking, or ones under the current
    /// directory when the platform has no home.
    pub fn discover() -> Self {
        match directories::ProjectDirs::from("cloud", "yannick", "noslacking") {
            Some(dirs) => Self {
                config: dirs.config_dir().to_path_buf(),
                state: dirs
                    .state_dir()
                    .unwrap_or_else(|| dirs.data_local_dir())
                    .to_path_buf(),
                cache: dirs.cache_dir().to_path_buf(),
            },
            None => Self::under(Path::new(".noslacking")),
        }
    }

    /// Every directory under `root`, for demos and tests.
    pub fn under(root: &Path) -> Self {
        Self {
            config: root.join("config"),
            state: root.join("state"),
            cache: root.join("cache"),
        }
    }

    /// Creates the directories.
    pub fn ensure(&self) -> std::io::Result<()> {
        for dir in [&self.config, &self.state, &self.cache] {
            std::fs::create_dir_all(dir)?;
        }
        std::fs::create_dir_all(self.images())?;
        std::fs::create_dir_all(self.themes())?;
        Ok(())
    }

    pub fn settings_file(&self) -> PathBuf {
        self.config.join("settings.json")
    }

    pub fn themes(&self) -> PathBuf {
        self.config.join("themes")
    }

    pub fn read_state_file(&self) -> PathBuf {
        self.state.join("read.json")
    }

    pub fn instance_file(&self) -> PathBuf {
        self.state.join("instance")
    }

    pub fn log_file(&self) -> PathBuf {
        self.state.join("noslacking.log")
    }

    pub fn panic_log(&self) -> PathBuf {
        self.state.join("panic.log")
    }

    pub fn images(&self) -> PathBuf {
        self.cache.join("images")
    }

    /// The users seen in a workspace, so names show before the network answers.
    pub fn users_cache(&self, team: &str) -> PathBuf {
        self.cache.join(format!("users-{}.json", sanitize(team)))
    }

    /// The conversation list of a workspace, for an instant sidebar on launch.
    pub fn conversations_cache(&self, team: &str) -> PathBuf {
        self.cache
            .join(format!("conversations-{}.json", sanitize(team)))
    }
}

/// A team id as a file name: Slack ids are `[A-Z0-9]`, but never trust that.
fn sanitize(id: &str) -> String {
    id.chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
        .collect()
}

/// Writes `bytes` to `path` through a temporary file, so a crash never
/// leaves half a file behind.
///
/// The temporary file has a name of its own (process id and a counter), so
/// two writers to the same path never write into each other's file; the
/// last rename wins and the result is always one whole file. The data is
/// flushed to disk before the rename, and the directory after it where the
/// platform allows, so a power cut cannot leave an empty file in its place.
pub fn write_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let parent = match path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent,
        _ => Path::new("."),
    };
    std::fs::create_dir_all(parent)?;
    let (tmp, mut file) = create_temp(parent, path)?;
    let written = std::io::Write::write_all(&mut file, bytes)
        .and_then(|()| file.sync_all())
        .and_then(|()| {
            drop(file);
            rename(&tmp, path)
        });
    if let Err(error) = written {
        let _ = std::fs::remove_file(&tmp);
        return Err(error);
    }
    sync_dir(parent);
    Ok(())
}

/// A new, empty temporary file in `dir`, named after `path`.
fn create_temp(dir: &Path, path: &Path) -> std::io::Result<(PathBuf, std::fs::File)> {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let name = path
        .file_name()
        .map_or_else(|| "file".into(), |n| n.to_string_lossy());
    let mut attempt = 0;
    loop {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let tmp = dir.join(format!(".{name}.{}.{n}.tmp", std::process::id()));
        // `create_new` never reuses a file another writer (or a crashed run
        // that had the same process id) left behind.
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&tmp)
        {
            Ok(file) => return Ok((tmp, file)),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists && attempt < 16 => {
                attempt += 1;
            }
            Err(error) => return Err(error),
        }
    }
}

/// Moves the temporary file over `path`.
fn rename(tmp: &Path, path: &Path) -> std::io::Result<()> {
    #[cfg(windows)]
    {
        // Windows refuses to replace a file someone holds open for a moment
        // (a virus scanner, a backup tool, another writer's rename). Such
        // locks are brief, so try again a few times.
        let mut attempt = 0;
        loop {
            match std::fs::rename(tmp, path) {
                Err(error)
                    if error.kind() == std::io::ErrorKind::PermissionDenied && attempt < 5 =>
                {
                    attempt += 1;
                    std::thread::sleep(std::time::Duration::from_millis(10 * attempt));
                }
                other => return other,
            }
        }
    }
    #[cfg(not(windows))]
    std::fs::rename(tmp, path)
}

/// Flushes a directory's entries, so a rename in it survives a power cut.
/// Only Unix can open a directory for this; elsewhere the file system is
/// trusted with the rename.
fn sync_dir(dir: &Path) {
    #[cfg(unix)]
    if let Err(error) = std::fs::File::open(dir).and_then(|d| d.sync_all()) {
        log::debug!("could not flush {}: {error}", dir.display());
    }
    #[cfg(not(unix))]
    let _ = dir;
}

/// A fresh directory under the system's temporary folder, deleted when
/// dropped, for tests that need real files.
#[cfg(test)]
pub(crate) struct TestDir(pub(crate) PathBuf);

#[cfg(test)]
impl TestDir {
    pub(crate) fn new(name: &str) -> Self {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir =
            std::env::temp_dir().join(format!("noslacking-test-{name}-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("test dir");
        Self(dir)
    }
}

#[cfg(test)]
impl Drop for TestDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn team_ids_cannot_escape_the_cache() {
        let dirs = AppDirs::under(Path::new("/x"));
        assert_eq!(
            dirs.users_cache("../../etc/T01"),
            PathBuf::from("/x/cache/users-etcT01.json")
        );
    }

    fn names(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(dir)
            .expect("list")
            .map(|e| e.expect("entry").file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    #[test]
    fn atomic_writes_replace_the_file_and_leave_nothing_behind() {
        let dir = TestDir::new("atomic");
        let path = dir.0.join("nested").join("settings.json");
        write_atomic(&path, b"one").expect("first write");
        write_atomic(&path, b"two").expect("second write");
        assert_eq!(std::fs::read(&path).expect("read"), b"two");
        assert_eq!(names(&dir.0.join("nested")), ["settings.json"]);
    }

    #[test]
    fn a_failed_atomic_write_cleans_up_after_itself() {
        let dir = TestDir::new("atomic-fail");
        // A non-empty directory where the file should go cannot be replaced.
        let path = dir.0.join("taken");
        std::fs::create_dir_all(path.join("inside")).expect("dir");
        assert!(write_atomic(&path, b"data").is_err());
        assert_eq!(names(&dir.0), ["taken"]);
    }

    #[test]
    fn concurrent_atomic_writers_never_mix_their_bytes() {
        let dir = TestDir::new("atomic-race");
        let path = dir.0.join("shared.json");
        let writers: Vec<_> = (0..8u8)
            .map(|writer| {
                let path = path.clone();
                std::thread::spawn(move || {
                    let bytes = vec![b'a' + writer; 64 * 1024];
                    for _ in 0..10 {
                        write_atomic(&path, &bytes).expect("write");
                    }
                })
            })
            .collect();
        for writer in writers {
            writer.join().expect("writer");
        }
        let bytes = std::fs::read(&path).expect("read");
        assert_eq!(bytes.len(), 64 * 1024);
        assert!(
            bytes.iter().all(|b| *b == bytes[0]),
            "one writer's bytes only"
        );
        assert_eq!(names(&dir.0), ["shared.json"]);
    }
}
