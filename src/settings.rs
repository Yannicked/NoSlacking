//! Preferences and remembered layout, kept as JSON in the config directory.
//!
//! Nothing secret lives here: tokens and the app's client secret are in the
//! OS keyring (see [`crate::credentials`]).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use crate::i18n::Locale;
/// The proxy setting's parts, which the Network page edits.
pub use crate::slack::net::{ProxyError, ProxyMode, ProxySettings, parse_manual};
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

/// How tightly messages are laid out.
#[derive(
    Clone, Copy, Debug, Default, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize,
)]
#[serde(rename_all = "snake_case")]
pub enum Density {
    /// Avatars, a name line above the text, room between messages.
    #[default]
    Comfortable,
    /// One line per message as in IRC: time, name and text side by side,
    /// no avatars, little room between.
    Compact,
}

impl Density {
    /// Both densities, as the settings list them.
    pub const ALL: [Self; 2] = [Self::Comfortable, Self::Compact];
}

/// How Slack sends the browser back after sign-in.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Redirect {
    /// `http://localhost:<port>/callback`: needs no desktop integration.
    /// The bundled manifest lists it for the default port.
    #[default]
    Loopback,
    /// `noslacking://oauth/callback`, delivered by the desktop's URL handler.
    /// Slack's manifest only takes http(s) redirect URLs, so the user adds
    /// this one by hand under the app's OAuth & Permissions.
    Scheme,
}

/// A signed-in workspace, minus its token.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct WorkspaceMeta {
    #[serde(default)]
    pub service: crate::model::Service,
    pub team_id: String,
    pub name: String,
    #[serde(default)]
    pub domain: String,
    #[serde(default)]
    pub icon: Option<String>,
    pub user_id: String,
    /// The user scopes Slack granted an app sign-in, so a workspace whose
    /// app lacks newer ones says so before Slack answers. Not secret.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scopes: Option<crate::scopes::Scopes>,
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
    /// Unread conversations at the top of their sidebar sections, mentions
    /// and direct messages first; on by default.
    pub unread_first: bool,
    /// After how long without a new message a conversation is hidden
    /// from the sidebar, behind its section's "N more"; a month by
    /// default.
    pub hide_inactive: crate::sidebar::HideInactive,
    /// Notifications and the rest of the desktop integration.
    pub desktop: crate::desktop::DesktopSettings,
    /// Your skin tone for emoji that have them, as Slack counts: 2 (light)
    /// to 6 (dark), anything else the default yellow.
    pub skin_tone: u8,
    /// The emoji you used lately, newest first, by name without a tone;
    /// at most [`crate::emoji::RECENT_MAX`].
    pub recent_emoji: Vec<String>,
    /// How tightly messages are laid out.
    pub density: Density,
    /// Whether pictures and link previews show in messages, or wait for
    /// a click.
    pub inline_media: bool,
    /// Programs to run on new messages; off by default.
    pub hooks: crate::hooks::Hooks,
    /// Which proxy every connection goes through (Settings → Network).
    pub proxy: ProxySettings,
    /// Spell checking in the composer.
    pub spelling: crate::spell::SpellSettings,
    /// Direct messages closed in the sidebar, by workspace: each one's
    /// newest message when it was closed. Anything newer brings it back.
    pub closed: BTreeMap<String, BTreeMap<String, String>>,
    /// Whether huddle video is decoded, and our camera encoded, on the GPU
    /// when it can be (both in the `noslacking-video` helper, which
    /// otherwise decodes in software; our camera then encodes in the app);
    /// on by default, as decoding takes 3–6× less CPU whenever pictures
    /// are shown smaller than they come and encoding 10× less
    /// (docs/research/huddle-video.md §6), and software takes over
    /// whenever the GPU cannot. The key keeps its first name, from when it
    /// was decoding only.
    pub hardware_video: bool,
    /// The camera, microphone and speaker chosen for huddles (Settings →
    /// Huddles, or the menus beside Mute and Video in a call); each the
    /// system's default until chosen. See [`crate::devices`].
    pub devices: crate::devices::Chosen,
    /// Whether your Slack app is known to be made from an older manifest,
    /// which Slack would not authorize with the newer scopes: sign-ins
    /// then ask for the older set only (see [`crate::scopes::Request`]).
    pub older_app: bool,
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
            unread_first: true,
            hide_inactive: crate::sidebar::HideInactive::Month,
            desktop: crate::desktop::DesktopSettings::default(),
            skin_tone: 0,
            recent_emoji: Vec::new(),
            density: Density::Comfortable,
            inline_media: true,
            hooks: crate::hooks::Hooks::default(),
            proxy: ProxySettings::default(),
            spelling: crate::spell::SpellSettings::default(),
            closed: BTreeMap::new(),
            hardware_video: true,
            devices: crate::devices::Chosen::default(),
            older_app: false,
        }
    }
}

impl<'de> serde::Deserialize<'de> for Settings {
    /// Never fails on a field: see `Settings::from_json`.
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
            unread_first,
            hide_inactive,
            desktop,
            skin_tone,
            recent_emoji,
            density,
            inline_media,
            hooks,
            proxy,
            spelling,
            closed,
            hardware_video,
            devices,
            older_app,
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

    /// The file's bytes, or `None` (logged) if they cannot be encoded.
    pub fn encode(&self) -> Option<Vec<u8>> {
        serde_json::to_vec_pretty(self)
            .map_err(|error| log::warn!("could not encode settings: {error}"))
            .ok()
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
        self.desktop.forget_workspace(team);
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

fn write(path: &Path, bytes: &[u8]) {
    if let Err(error) = crate::paths::write_atomic(path, bytes) {
        log::warn!("could not save settings: {error}");
    }
}

/// How long settings must hold still before they are written: dragging a
/// panel edge or the zoom slider changes them every frame.
pub const SAVE_AFTER: Duration = Duration::from_millis(500);

/// When a burst of changes is due to be saved: each change pushes the
/// moment back, so a drag is written once, after it ends.
#[derive(Debug, Default)]
pub struct Debounce {
    due: Option<Instant>,
}

impl Debounce {
    /// Notes a change at `now`.
    pub fn poke(&mut self, now: Instant) {
        self.due = Some(now + SAVE_AFTER);
    }

    /// Whether a change is waiting at all.
    pub fn pending(&self) -> bool {
        self.due.is_some()
    }

    /// Whether the changes have held still long enough by `now`; answering
    /// yes forgets them.
    pub fn take_due(&mut self, now: Instant) -> bool {
        if self.due.is_some_and(|due| now >= due) {
            self.due = None;
            return true;
        }
        false
    }

    /// Forgets the waiting changes, for a save made some other way.
    pub fn clear(&mut self) {
        self.due = None;
    }
}

enum Job {
    Write(PathBuf, Vec<u8>),
    /// Answer once everything sent before has been written.
    Flush(mpsc::Sender<()>),
}

/// Writes the settings file on its own thread, so a slow disk never stalls
/// a frame. Writes queued faster than the disk takes them collapse into the
/// newest, and they land in the order they were sent.
pub struct Saver {
    jobs: Option<mpsc::Sender<Job>>,
}

impl Default for Saver {
    fn default() -> Self {
        Self::new()
    }
}

impl Saver {
    /// Starts the writer thread; without one, saves happen in place.
    pub fn new() -> Self {
        let (jobs, queue) = mpsc::channel::<Job>();
        let spawned = std::thread::Builder::new()
            .name("settings-saver".into())
            .spawn(move || run(&queue));
        match spawned {
            Ok(_) => Self { jobs: Some(jobs) },
            Err(error) => {
                log::warn!("no settings thread, saving in place: {error}");
                Self { jobs: None }
            }
        }
    }

    /// Writes `settings` to `path` soon, off this thread.
    pub fn save(&self, settings: &Settings, path: &Path) {
        let Some(bytes) = settings.encode() else {
            return;
        };
        let Some(jobs) = &self.jobs else {
            write(path, &bytes);
            return;
        };
        if let Err(mpsc::SendError(Job::Write(path, bytes))) =
            jobs.send(Job::Write(path.to_owned(), bytes))
        {
            // The thread is gone: write here rather than lose the change.
            write(&path, &bytes);
        }
    }

    /// Writes `settings` and waits until it is on disk, for quitting: the
    /// newest state must win over any write still queued.
    pub fn save_now(&self, settings: &Settings, path: &Path) {
        self.save(settings, path);
        let Some(jobs) = &self.jobs else {
            return;
        };
        let (done, wait) = mpsc::channel();
        if jobs.send(Job::Flush(done)).is_ok() && wait.recv_timeout(Duration::from_secs(5)).is_err()
        {
            log::warn!("the settings file took too long to write");
        }
    }
}

impl std::fmt::Debug for Saver {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Saver")
            .field("threaded", &self.jobs.is_some())
            .finish()
    }
}

/// The writer thread: takes the newest of whatever is queued, writes it,
/// and answers flushes once what came before them is written.
fn run(queue: &mpsc::Receiver<Job>) {
    while let Ok(first) = queue.recv() {
        let mut latest = None;
        let mut flushes = Vec::new();
        for job in std::iter::once(first).chain(queue.try_iter()) {
            match job {
                Job::Write(path, bytes) => latest = Some((path, bytes)),
                Job::Flush(done) => flushes.push(done),
            }
        }
        if let Some((path, bytes)) = latest {
            write(&path, &bytes);
        }
        for done in flushes {
            let _ = done.send(());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::paths::TestDir;

    #[test]
    fn a_burst_of_changes_is_saved_once_it_holds_still() {
        let start = Instant::now();
        let mut debounce = Debounce::default();
        assert!(!debounce.take_due(start));
        debounce.poke(start);
        debounce.poke(start + Duration::from_millis(300));
        assert!(!debounce.take_due(start + Duration::from_millis(600)));
        assert!(debounce.pending());
        assert!(debounce.take_due(start + Duration::from_millis(800)));
        assert!(!debounce.pending());
        assert!(!debounce.take_due(start + Duration::from_millis(900)));
    }

    #[test]
    fn the_saver_leaves_the_newest_settings_on_disk() {
        let dir = TestDir::new("saver");
        let path = dir.0.join("settings.json");
        let saver = Saver::new();
        let mut settings = Settings::default();
        for zoom in [1.1, 1.2, 1.3] {
            settings.zoom = zoom;
            saver.save(&settings, &path);
        }
        settings.zoom = 1.4;
        saver.save_now(&settings, &path);
        assert!((Settings::load(&path).zoom - 1.4).abs() < 1e-6);
    }

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
        write(&path, &settings.encode().expect("encodes"));
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
        write(&good, &Settings::default().encode().expect("encodes"));
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
    fn density_and_inline_media_round_trip() {
        let settings: Settings =
            serde_json::from_str(r#"{"density":"compact","inline_media":false}"#).expect("parses");
        assert_eq!(settings.density, Density::Compact);
        assert!(!settings.inline_media);
        let encoded = settings.encode().expect("encodes");
        let again: Settings = serde_json::from_slice(&encoded).expect("parses");
        assert_eq!(again.density, Density::Compact);
        // Older files have neither: messages look as they always did.
        let old: Settings = serde_json::from_str("{}").expect("parses");
        assert_eq!(old.density, Density::Comfortable);
        assert!(old.inline_media);
    }

    #[test]
    fn hardware_video_is_on_until_turned_off() {
        let old: Settings = serde_json::from_str("{}").expect("parses");
        assert!(old.hardware_video, "older files: on");
        let off: Settings = serde_json::from_str(r#"{"hardware_video":false}"#).expect("parses");
        assert!(!off.hardware_video);
        let again: Settings =
            serde_json::from_slice(&off.encode().expect("encodes")).expect("parses");
        assert!(!again.hardware_video);
    }

    #[test]
    fn chosen_devices_round_trip_and_default_to_the_systems() {
        use crate::devices::{Choice, Kind};
        let old: Settings = serde_json::from_str("{}").expect("parses");
        assert_eq!(
            old.devices,
            crate::devices::Chosen::default(),
            "older files"
        );
        let file = r#"{"devices":{"speaker":{"id":"alsa:sysdefault:CARD=Audio","name":"USB Audio"},
            "camera":{"id":"v4l2:/dev/video2","name":"Logitech BRIO"}}}"#;
        let settings: Settings = serde_json::from_str(file).expect("parses");
        assert_eq!(settings.devices.get(Kind::Microphone), None);
        assert_eq!(
            settings.devices.get(Kind::Speaker),
            Some(&Choice {
                id: "alsa:sysdefault:CARD=Audio".into(),
                name: "USB Audio".into(),
            })
        );
        let again: Settings =
            serde_json::from_slice(&settings.encode().expect("encodes")).expect("parses");
        assert_eq!(again.devices, settings.devices);
        // A damaged choice falls back to the defaults, keeping the rest.
        let (bad, problems) =
            Settings::from_json(serde_json::json!({"devices": {"camera": 3}, "zoom": 1.5}));
        assert_eq!(bad.devices, crate::devices::Chosen::default());
        assert_eq!(bad.zoom, 1.5);
        assert_eq!(problems.len(), 1);
    }

    #[test]
    fn unread_first_is_on_until_turned_off() {
        // Older files do not have it: unread conversations come first.
        let old: Settings = serde_json::from_str("{}").expect("parses");
        assert!(old.unread_first);
        let off: Settings = serde_json::from_str(r#"{"unread_first":false}"#).expect("parses");
        assert!(!off.unread_first);
        let encoded = off.encode().expect("encodes");
        let again: Settings = serde_json::from_slice(&encoded).expect("parses");
        assert!(!again.unread_first);
        let (bad, problems) = Settings::from_json(serde_json::json!({"unread_first": "yes"}));
        assert!(bad.unread_first, "unreadable falls back to on");
        assert_eq!(problems.len(), 1);
    }

    #[test]
    fn hiding_inactive_conversations_defaults_to_a_month() {
        use crate::sidebar::HideInactive;
        // Older files do not have it: quiet conversations hide after a
        // month.
        let old: Settings = serde_json::from_str("{}").expect("parses");
        assert_eq!(old.hide_inactive, HideInactive::Month);
        for choice in [
            HideInactive::Off,
            HideInactive::Week,
            HideInactive::Month,
            HideInactive::ThreeMonths,
        ] {
            let settings = Settings {
                hide_inactive: choice,
                ..Settings::default()
            };
            let encoded = settings.encode().expect("encodes");
            let again: Settings = serde_json::from_slice(&encoded).expect("parses");
            assert_eq!(again.hide_inactive, choice);
        }
        let off: Settings = serde_json::from_str(r#"{"hide_inactive":"off"}"#).expect("parses");
        assert_eq!(off.hide_inactive, HideInactive::Off);
        let (bad, problems) = Settings::from_json(serde_json::json!({
            "hide_inactive": "fortnight",
            "zoom": 1.5,
        }));
        assert_eq!(bad.hide_inactive, HideInactive::Month, "unknown falls back");
        assert_eq!(bad.zoom, 1.5, "the other fields are kept");
        assert_eq!(problems.len(), 1);
    }

    #[test]
    fn granted_scopes_and_an_older_app_are_remembered() {
        let mut settings = Settings {
            older_app: true,
            ..Settings::default()
        };
        settings.upsert_workspace(WorkspaceMeta {
            service: crate::model::Service::Slack,
            team_id: "T1".into(),
            name: "Acme".into(),
            domain: String::new(),
            icon: None,
            user_id: "U1".into(),
            scopes: Some(crate::scopes::Scopes::parse("chat:write,dnd:read")),
        });
        let encoded = serde_json::to_value(&settings).expect("encodes");
        assert_eq!(
            encoded["workspaces"][0]["scopes"],
            serde_json::json!(["chat:write", "dnd:read"])
        );
        let (again, problems) = Settings::from_json(encoded);
        assert!(problems.is_empty(), "{problems:?}");
        assert!(again.older_app);
        assert_eq!(again.workspaces, settings.workspaces);
        // A file from before has neither, and reads as not known.
        let (old, problems) = Settings::from_json(serde_json::json!({
            "workspaces": [{"team_id": "T1", "name": "Acme", "user_id": "U1"}],
        }));
        assert!(problems.is_empty(), "{problems:?}");
        assert!(!old.older_app);
        assert_eq!(old.workspaces[0].scopes, None);
    }

    #[test]
    fn removing_the_active_workspace_picks_another() {
        let mut settings = Settings::default();
        for id in ["T1", "T2"] {
            settings.upsert_workspace(WorkspaceMeta {
                service: crate::model::Service::Slack,
                team_id: id.into(),
                name: id.into(),
                domain: String::new(),
                icon: None,
                user_id: "U1".into(),
                scopes: None,
            });
        }
        settings.active_workspace = Some("T1".into());
        settings.remove_workspace("T1");
        assert_eq!(settings.active_workspace.as_deref(), Some("T2"));
    }
}
