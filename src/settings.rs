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

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct Settings {
    pub appearance: Appearance,
    /// The last custom palette, so the window opens in it before the themes
    /// folder has been read.
    #[serde(deserialize_with = "fastframe_theme::read_cached_theme")]
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

impl Settings {
    /// Reads the settings, falling back to defaults for a missing or
    /// damaged file (and logging why).
    pub fn load(path: &Path) -> Self {
        match std::fs::read(path) {
            Ok(bytes) => match serde_json::from_slice(&bytes) {
                Ok(settings) => settings,
                Err(error) => {
                    log::warn!("ignoring unreadable settings: {error}");
                    Self::default()
                }
            },
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Self::default(),
            Err(error) => {
                log::warn!("could not read settings: {error}");
                Self::default()
            }
        }
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

#[cfg(test)]
mod tests {
    use super::*;

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
