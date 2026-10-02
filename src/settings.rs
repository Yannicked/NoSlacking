//! Preferences and remembered layout, kept as JSON in the config directory.
//!
//! Nothing secret lives here: tokens and the app's client secret are in the
//! OS keyring (see [`crate::credentials`]).

use std::collections::BTreeMap;
use std::path::Path;

use crate::i18n::Locale;
use crate::theme::CustomTheme;

/// How the window is coloured.
#[derive(Clone, Debug, Default, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Appearance {
    /// Dark or light, as the desktop prefers.
    #[default]
    System,
    Dark,
    Light,
    /// A palette file from the themes folder, by filename.
    Custom(String),
}

/// How Slack sends the browser back after sign-in.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Redirect {
    /// `http://127.0.0.1:<port>/callback`: needs no desktop integration,
    /// but the Slack app must list it as a redirect URL.
    Loopback,
    /// `noslacking://oauth/callback`, delivered by the desktop's URL handler.
    /// What the bundled manifest registers.
    Scheme,
}

impl Default for Redirect {
    /// The scheme, except on macOS, where only an app bundle can own one.
    fn default() -> Self {
        if cfg!(target_os = "macos") {
            Self::Loopback
        } else {
            Self::Scheme
        }
    }
}

/// A signed-in workspace, minus its token.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct WorkspaceMeta {
    pub team_id: String,
    pub name: String,
    #[serde(default)]
    pub domain: String,
    #[serde(default)]
    pub icon: Option<String>,
    pub user_id: String,
}

/// The settings file format this build writes. Raise it when a field
/// changes meaning, so an older build can tell it is reading a newer file.
pub const VERSION: u32 = 1;

/// Everything the settings file holds.
///
/// It is read field by field: a field that is missing or cannot be read
/// takes its default, and every other field is kept (see [`Settings::load`]).
#[derive(Clone, Debug, serde::Serialize)]
pub struct Settings {
    /// The format the file was written in; [`VERSION`] once loaded.
    pub version: u32,
    pub appearance: Appearance,
    /// The last custom palette, so the window opens in it before the themes
    /// folder has been read.
    pub cached_theme: Option<CustomTheme>,
    pub language: Option<Locale>,
    pub redirect: Redirect,
    pub loopback_port: u16,
    /// Signed-in workspaces, in rail order.
    pub workspaces: Vec<WorkspaceMeta>,
    pub active_workspace: Option<String>,
    /// The conversation last open in each workspace.
    pub last_conversation: BTreeMap<String, String>,
    pub sidebar_width: f32,
    pub thread_width: f32,
    /// Interface zoom, 1.0 for egui's own scale.
    pub zoom: f32,
    /// Send with Enter (Shift+Enter for a new line), or with Ctrl+Enter.
    pub enter_sends: bool,
    /// How channels are ordered in their sidebar sections.
    pub sidebar_sort: crate::sidebar::Sort,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            version: VERSION,
            appearance: Appearance::System,
            cached_theme: None,
            language: None,
            redirect: Redirect::default(),
            loopback_port: 53682,
            workspaces: Vec::new(),
            active_workspace: None,
            last_conversation: BTreeMap::new(),
            sidebar_width: 260.0,
            thread_width: 380.0,
            zoom: 1.0,
            enter_sends: true,
            sidebar_sort: crate::sidebar::Sort::Name,
        }
    }
}

impl<'de> serde::Deserialize<'de> for Settings {
    /// Never fails on a field: see [`Settings::from_json`].
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = serde_json::Value::deserialize(deserializer)?;
        Ok(Self::from_json(value).0)
    }
}

impl Settings {
    /// Reads the settings, falling back to defaults for a missing file or
    /// for the fields it cannot read (and logging why).
    ///
    /// The next save rewrites the file with only what was understood, so a
    /// file that was damaged, or written by a newer NoSlacking, is first
    /// copied aside (`settings.json.bad`, `settings.json.v2`) for the user
    /// to recover.
    pub fn load(path: &Path) -> Self {
        let bytes = match std::fs::read(path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Self::default(),
            Err(error) => {
                log::warn!("could not read settings: {error}");
                return Self::default();
            }
        };
        let (settings, problems) = match serde_json::from_slice(&bytes) {
            Ok(value) => Self::from_json(value),
            Err(error) => (Self::default(), vec![error.to_string()]),
        };
        let backup = if !problems.is_empty() {
            log::warn!("ignoring unreadable settings: {}", problems.join("; "));
            Some(backup_path(path, "bad"))
        } else if settings.version > VERSION {
            log::warn!(
                "settings were written by a newer version (format {}); keeping a copy",
                settings.version
            );
            Some(backup_path(path, &format!("v{}", settings.version)))
        } else {
            None
        };
        if let Some(backup) = backup
            && let Err(error) = std::fs::copy(path, &backup)
        {
            log::warn!("could not back up the settings: {error}");
        }
        Self {
            version: VERSION,
            ..settings
        }
    }

    /// The settings in a JSON value, field by field: each field that is
    /// there but cannot be read keeps its default and is named in the
    /// returned problems. Unknown fields are ignored, so an older build can
    /// read a newer file.
    fn from_json(value: serde_json::Value) -> (Self, Vec<String>) {
        let mut settings = Self::default();
        let mut problems = Vec::new();
        let serde_json::Value::Object(mut fields) = value else {
            problems.push("the file is not a JSON object".to_owned());
            return (settings, problems);
        };
        macro_rules! read {
            ($($field:ident),* $(,)?) => {$(
                if let Some(value) = fields.remove(stringify!($field)) {
                    match serde_json::from_value(value) {
                        Ok(value) => settings.$field = value,
                        Err(error) => problems.push(format!("{}: {error}", stringify!($field))),
                    }
                }
            )*};
        }
        read!(
            version,
            appearance,
            language,
            redirect,
            loopback_port,
            active_workspace,
            sidebar_width,
            thread_width,
            zoom,
            enter_sends,
            sidebar_sort,
        );
        // One damaged workspace must not sign you out of the others, so
        // these are read entry by entry.
        if let Some(value) = fields.remove("workspaces") {
            match value {
                serde_json::Value::Array(items) => {
                    settings.workspaces = items
                        .into_iter()
                        .filter_map(|item| match serde_json::from_value(item) {
                            Ok(workspace) => Some(workspace),
                            Err(error) => {
                                problems.push(format!("workspaces: {error}"));
                                None
                            }
                        })
                        .collect();
                }
                _ => problems.push("workspaces: not a list".to_owned()),
            }
        }
        if let Some(value) = fields.remove("last_conversation") {
            match value {
                serde_json::Value::Object(entries) => {
                    for (team, channel) in entries {
                        match channel {
                            serde_json::Value::String(channel) => {
                                settings.last_conversation.insert(team, channel);
                            }
                            _ => problems.push(format!("last_conversation: {team} is not text")),
                        }
                    }
                }
                _ => problems.push("last_conversation: not an object".to_owned()),
            }
        }
        // A palette is only a cache of the themes folder: the window falls
        // back to the built-in look, and the file loses nothing it cannot
        // read again, so a bad one is not worth a backup.
        if let Some(value) = fields.remove("cached_theme") {
            settings.cached_theme = fastframe_theme::read_cached_theme(value).unwrap_or_default();
        }
        (settings, problems)
    }

    pub fn save(&self, path: &Path) {
        match serde_json::to_vec_pretty(self) {
            Ok(bytes) => {
                if let Err(error) = crate::paths::write_atomic(path, &bytes) {
                    log::warn!("could not save settings: {error}");
                }
            }
            Err(error) => log::warn!("could not encode settings: {error}"),
        }
    }

    pub fn workspace(&self, team: &str) -> Option<&WorkspaceMeta> {
        self.workspaces.iter().find(|w| w.team_id == team)
    }

    /// Adds or refreshes a workspace, keeping its place in the rail.
    pub fn upsert_workspace(&mut self, meta: WorkspaceMeta) {
        match self
            .workspaces
            .iter_mut()
            .find(|w| w.team_id == meta.team_id)
        {
            Some(existing) => *existing = meta,
            None => self.workspaces.push(meta),
        }
    }

    pub fn remove_workspace(&mut self, team: &str) {
        self.workspaces.retain(|w| w.team_id != team);
        self.last_conversation.remove(team);
        if self.active_workspace.as_deref() == Some(team) {
            self.active_workspace = self.workspaces.first().map(|w| w.team_id.clone());
        }
    }
}

/// `settings.json` with `suffix` added: `settings.json.bad`.
fn backup_path(path: &Path, suffix: &str) -> std::path::PathBuf {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(".");
    name.push(suffix);
    path.with_file_name(name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::paths::TestDir;

    #[test]
    fn one_bad_field_keeps_the_others() {
        let (settings, problems) = Settings::from_json(serde_json::json!({
            "zoom": "large",
            "sidebar_width": 300.0,
            "loopback_port": 99999,
            "redirect": "carrier_pigeon",
            "appearance": {"custom": "Nord.json"},
            "sidebar_sort": "recent",
            "workspaces": [
                {"team_id": "T1", "name": "One", "user_id": "U1"},
                {"team_id": 7},
                {"team_id": "T2", "name": "Two", "user_id": "U2"},
            ],
            "last_conversation": {"T1": "C1", "T2": 5},
            "cached_theme": {"not": "a theme"},
            "from_the_future": true,
        }));
        assert_eq!(settings.zoom, 1.0, "bad zoom falls back");
        assert_eq!(settings.loopback_port, 53682, "out of range falls back");
        assert_eq!(settings.redirect, Redirect::default(), "unknown variant");
        assert_eq!(settings.sidebar_width, 300.0);
        assert_eq!(settings.appearance, Appearance::Custom("Nord.json".into()));
        assert_eq!(settings.sidebar_sort, crate::sidebar::Sort::Recent);
        let teams: Vec<&str> = settings
            .workspaces
            .iter()
            .map(|w| w.team_id.as_str())
            .collect();
        assert_eq!(
            teams,
            ["T1", "T2"],
            "the damaged workspace alone is dropped"
        );
        assert_eq!(
            settings.last_conversation,
            BTreeMap::from([("T1".to_owned(), "C1".to_owned())])
        );
        assert!(settings.cached_theme.is_none());
        let fields: Vec<&str> = problems
            .iter()
            .map(|p| p.split(':').next().unwrap_or_default())
            .collect();
        assert_eq!(
            fields,
            [
                "redirect",
                "loopback_port",
                "zoom",
                "workspaces",
                "last_conversation"
            ]
        );
    }

    #[test]
    fn a_damaged_file_is_backed_up_and_read_leniently() {
        let dir = TestDir::new("settings-bad");
        let path = dir.0.join("settings.json");
        let damaged = r#"{"zoom": 1.5, "enter_sends": "sometimes"}"#;
        std::fs::write(&path, damaged).expect("write");
        let settings = Settings::load(&path);
        assert_eq!(settings.zoom, 1.5);
        assert!(settings.enter_sends);
        assert_eq!(
            std::fs::read_to_string(dir.0.join("settings.json.bad")).expect("backup"),
            damaged
        );
        // Saving then overwrites the original, but the backup stays.
        settings.save(&path);
        assert!(dir.0.join("settings.json.bad").exists());
        assert_eq!(Settings::load(&path).zoom, 1.5);
    }

    #[test]
    fn a_file_that_is_not_json_is_backed_up() {
        let dir = TestDir::new("settings-garbage");
        let path = dir.0.join("settings.json");
        std::fs::write(&path, "{\"zoom\": 1.5,").expect("write");
        let settings = Settings::load(&path);
        assert_eq!(settings.zoom, 1.0);
        assert!(dir.0.join("settings.json.bad").exists());
    }

    #[test]
    fn a_newer_file_is_kept_aside_and_a_good_one_is_not() {
        let dir = TestDir::new("settings-newer");
        let path = dir.0.join("settings.json");
        let newer = r#"{"version": 99, "zoom": 1.5, "new_field": [1, 2]}"#;
        std::fs::write(&path, newer).expect("write");
        let settings = Settings::load(&path);
        assert_eq!(settings.zoom, 1.5);
        assert_eq!(settings.version, VERSION, "saved in this build's format");
        assert_eq!(
            std::fs::read_to_string(dir.0.join("settings.json.v99")).expect("copy"),
            newer
        );
        assert!(!dir.0.join("settings.json.bad").exists());

        let good = dir.0.join("good.json");
        Settings::default().save(&good);
        let _ = Settings::load(&good);
        let mut names: Vec<_> = std::fs::read_dir(&dir.0)
            .expect("list")
            .map(|e| e.expect("entry").file_name())
            .collect();
        names.sort();
        assert_eq!(names, ["good.json", "settings.json", "settings.json.v99"]);
    }

    #[test]
    fn a_missing_file_is_the_defaults() {
        let dir = TestDir::new("settings-missing");
        let settings = Settings::load(&dir.0.join("settings.json"));
        assert_eq!(settings.version, VERSION);
        assert!(settings.workspaces.is_empty());
    }

    #[test]
    fn unknown_and_missing_fields_fall_back() {
        let settings: Settings =
            serde_json::from_str(r#"{"zoom":1.25,"appearance":{"custom":"Nord.json"}}"#)
                .expect("parses");
        assert_eq!(settings.zoom, 1.25);
        assert_eq!(settings.appearance, Appearance::Custom("Nord.json".into()));
        assert!(settings.enter_sends);
        assert_eq!(settings.redirect, Redirect::default());
    }

    #[test]
    fn removing_the_active_workspace_picks_another() {
        let mut settings = Settings::default();
        for id in ["T1", "T2"] {
            settings.upsert_workspace(WorkspaceMeta {
                team_id: id.into(),
                name: id.into(),
                domain: String::new(),
                icon: None,
                user_id: "U1".into(),
            });
        }
        settings.active_workspace = Some("T1".into());
        settings.remove_workspace("T1");
        assert_eq!(settings.active_workspace.as_deref(), Some("T2"));
    }
}
