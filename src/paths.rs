//! Where NoSlacking keeps its files.
//!
//! Configuration (settings, themes) is what a user would back up; state is
//! what the app remembers between runs (read markers, the single-instance
//! port); cache is anything that can be fetched again (images, user lists).

use std::path::{Path, PathBuf};

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
pub fn write_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, bytes)?;
    std::fs::rename(&tmp, path)
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
}
