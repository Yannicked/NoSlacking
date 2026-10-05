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

    /// Creates the directories. State (the log, the panic log, drafts) and
    /// the cache (a workspace's files) are yours alone, so on Unix only
    /// you may open them, whatever files inside them a library creates.
    pub fn ensure(&self) -> std::io::Result<()> {
        std::fs::create_dir_all(&self.config)?;
        create_private_dir(&self.state)?;
        create_private_dir(&self.cache)?;
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

    /// Unsent messages, so they survive a restart. State, not config:
    /// they are yours alone and nothing to back up or sync.
    pub fn drafts_file(&self) -> PathBuf {
        self.state.join("drafts.json")
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

    /// Images pasted from the clipboard, written out for upload and
    /// removed once sent.
    pub fn pasted(&self) -> PathBuf {
        self.cache.join("pasted")
    }

    pub fn images(&self) -> PathBuf {
        self.cache.join("images")
    }

    /// The encrypted offline cache (see [`crate::offline`]).
    pub fn offline(&self) -> PathBuf {
        crate::offline::root(&self.cache)
    }

    /// Where older builds kept a workspace's users unencrypted; removed
    /// now that [`crate::offline`] keeps them.
    pub fn users_cache(&self, team: &str) -> PathBuf {
        self.cache.join(format!("users-{}.json", sanitize(team)))
    }

    /// Where older builds kept a workspace's conversation list unencrypted.
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

/// Creates `dir` and any missing folders above it, and on Unix lets only
/// you open it (0700), tightening one an older version left open. Other
/// platforms keep the folder's inherited permissions, which on Windows and
/// macOS already keep other users out of your profile.
pub fn create_private_dir(dir: &Path) -> std::io::Result<()> {
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::{DirBuilderExt as _, PermissionsExt as _};
        builder.mode(0o700);
        builder.create(dir)?;
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))
    }
    #[cfg(not(unix))]
    builder.create(dir)
}

/// Lets only you read the file at `path` (0600) on Unix, for files a
/// library creates with the usual permissions. Elsewhere it does nothing.
pub fn make_private(path: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        Ok(())
    }
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
    write_file(path, bytes, false)
}

/// Like [`write_atomic`], for what only you should read (unsent drafts):
/// on Unix the file is readable by its owner alone from the moment it is
/// created. Elsewhere it keeps the folder's permissions, which on Windows
/// and macOS already keep other users out of your profile.
pub fn write_private(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    write_file(path, bytes, true)
}

fn write_file(path: &Path, bytes: &[u8], private: bool) -> std::io::Result<()> {
    let parent = match path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent,
        _ => Path::new("."),
    };
    std::fs::create_dir_all(parent)?;
    let (tmp, mut file) = create_temp(parent, path, private)?;
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
fn create_temp(
    dir: &Path,
    path: &Path,
    private: bool,
) -> std::io::Result<(PathBuf, std::fs::File)> {
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
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        if private {
            std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
        }
        #[cfg(not(unix))]
        let _ = private;
        match options.open(&tmp) {
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

    #[cfg(unix)]
    #[test]
    fn private_folders_and_files_are_the_owners_alone() {
        use std::os::unix::fs::PermissionsExt as _;
        let mode =
            |path: &Path| std::fs::metadata(path).expect("meta").permissions().mode() & 0o777;
        let dir = TestDir::new("private");
        let dirs = AppDirs::under(&dir.0);
        // A folder an older version left open is tightened too.
        std::fs::create_dir_all(&dirs.state).expect("state");
        std::fs::set_permissions(&dirs.state, std::fs::Permissions::from_mode(0o755))
            .expect("open it");
        dirs.ensure().expect("ensure");
        assert_eq!(mode(&dirs.state), 0o700);
        assert_eq!(mode(&dirs.cache), 0o700);
        let nested = dir.0.join("a").join("b");
        create_private_dir(&nested).expect("nested");
        assert_eq!(mode(&dir.0.join("a")), 0o700);
        assert_eq!(mode(&nested), 0o700);
        let log = dirs.log_file();
        std::fs::write(&log, "x").expect("log");
        make_private(&log).expect("private");
        assert_eq!(mode(&log), 0o600);
        write_private(&nested.join("f"), b"x").expect("write");
        assert_eq!(mode(&nested.join("f")), 0o600);
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
