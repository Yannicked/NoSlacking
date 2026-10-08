//! Borrowing the `slack://` links for a browser sign-in, and giving them
//! back.
//!
//! Slack's browser sign-in ends with a `slack://` link, so for that one
//! sign-in NoSlacking makes itself their handler ([`claim`]). Anyone with
//! the official Slack app wants them back afterwards: [`release`] puts back
//! whatever handled them before, once the link has come, the sign-in was
//! cancelled or its time ran out, and when the app quits.
//!
//! What the claim replaced is kept in memory and in a small file in the
//! state folder (nothing secret: a desktop file name, or whether a registry
//! key was saved), so a crash in between is undone at the next start
//! ([`release_at_start`]). That also cleans up a claim an older version
//! left behind without any record.
//!
//! - **Linux**: the claim is the `x-scheme-handler/slack` line under
//!   `[Default Applications]` in the user's `mimeapps.list` (written by
//!   `xdg-mime`), and `x-scheme-handler/slack` in the `MimeType` of the
//!   desktop file NoSlacking writes for itself. Giving back restores the
//!   line as it was, or removes it, but only while it still names
//!   NoSlacking, and drops `slack` from the desktop file again.
//! - **Windows**: the claim is `HKCU\Software\Classes\slack`. A key that
//!   was there is exported first and imported again on release; a key
//!   NoSlacking created is deleted, so the machine-wide registration (if
//!   any) shows through `HKCR` again. Either way only while the key still
//!   names NoSlacking.
//! - **macOS**: nothing is claimed. The app cannot receive links there yet
//!   (see CONTRIBUTING.md), and changing the handler needs Launch Services
//!   calls this crate cannot make without `unsafe`.

use std::path::Path;
#[cfg(any(target_os = "linux", test))]
use std::path::PathBuf;
use std::sync::{Mutex, MutexGuard};

use serde::{Deserialize, Serialize};

use crate::sync::lock;

/// What a claim replaced, to be put back.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Claim {
    /// Linux: the `x-scheme-handler/slack` value under `[Default
    /// Applications]` in the user's `mimeapps.list` before the claim, such
    /// as `slack.desktop;`; none when there was no such line.
    #[serde(default)]
    pub previous: Option<String>,
    /// Windows: `HKCU\Software\Classes\slack` existed and was exported to
    /// the backup file beside the record.
    #[serde(default)]
    pub backup: bool,
}

/// This run's claim, if it holds one. Also keeps a claim and a release
/// from running at once.
static HELD: Mutex<Option<Claim>> = Mutex::new(None);

/// The record of a claim in progress, in the state folder.
const RECORD: &str = "slack-links.json";
/// Windows: the exported `HKCU\Software\Classes\slack`, beside the record.
#[cfg(windows)]
const BACKUP: &str = "slack-links.reg";
/// The MIME type desktops file `slack://` links under.
#[cfg(any(target_os = "linux", test))]
const MIME: &str = "x-scheme-handler/slack";

fn held() -> MutexGuard<'static, Option<Claim>> {
    lock(&HELD)
}

/// Whether this run holds the `slack://` links now.
pub fn claimed() -> bool {
    held().is_some()
}

/// Makes NoSlacking the handler of `slack://` links for a browser sign-in,
/// after remembering what handled them (in memory and in `state`). A
/// second claim while one is held keeps the first one's memory: the
/// handler before that was NoSlacking itself.
pub fn claim(state: &Path) -> Result<(), String> {
    let mut held = held();
    if held.is_none() {
        let remembered = match read_record(state) {
            // A crash left a claim: what it replaced is still what to
            // give back.
            Some(recorded) => recorded,
            None => match platform::remember(state)? {
                Some(remembered) => remembered,
                // Nothing NoSlacking may change here.
                None => return Ok(()),
            },
        };
        // Written before claiming, so a crash in between is undone too.
        write_record(state, &remembered)?;
        *held = Some(remembered);
    }
    platform::claim()
}

/// Gives the `slack://` links back if this run, or a run that crashed,
/// claimed them. Whether anything was given back.
pub fn release(state: &Path) -> Result<bool, String> {
    release_with(state, false)
}

/// At start-up: gives back a claim a crashed run left, or one an older
/// version made without a record, unless this run holds one already.
pub fn release_at_start(state: &Path) -> Result<bool, String> {
    release_with(state, true)
}

fn release_with(state: &Path, at_start: bool) -> Result<bool, String> {
    let mut held = held();
    let Some(claim) = to_give_back(
        at_start,
        held.as_ref(),
        read_record(state),
        platform::leftover,
    ) else {
        return Ok(false);
    };
    platform::give_back(state, &claim)?;
    if let Err(error) = std::fs::remove_file(state.join(RECORD))
        && error.kind() != std::io::ErrorKind::NotFound
    {
        log::warn!("could not remove the slack:// link record: {error}");
    }
    *held = None;
    Ok(true)
}

/// Which claim to give back, if any: this run's, else a recorded one, else
/// (only at start-up) one found on the desktop with no record. At start-up
/// a claim this run holds is a sign-in in progress, so it stays.
fn to_give_back(
    at_start: bool,
    live: Option<&Claim>,
    recorded: Option<Claim>,
    leftover: impl FnOnce() -> Option<Claim>,
) -> Option<Claim> {
    match (live, recorded) {
        (Some(_), _) if at_start => None,
        (Some(live), _) => Some(live.clone()),
        (None, Some(recorded)) => Some(recorded),
        (None, None) if at_start => leftover(),
        (None, None) => None,
    }
}

fn read_record(state: &Path) -> Option<Claim> {
    let text = std::fs::read_to_string(state.join(RECORD)).ok()?;
    match serde_json::from_str(&text) {
        Ok(claim) => Some(claim),
        Err(error) => {
            // Still a claim to give back; what it replaced is unknown.
            log::warn!("the slack:// link record is unreadable: {error}");
            Some(Claim::default())
        }
    }
}

fn write_record(state: &Path, claim: &Claim) -> Result<(), String> {
    std::fs::create_dir_all(state).map_err(|e| e.to_string())?;
    let text = serde_json::to_string(claim).map_err(|e| e.to_string())?;
    std::fs::write(state.join(RECORD), text).map_err(|e| e.to_string())
}

/// The value `list`, a `mimeapps.list`, gives `mime` under `[Default
/// Applications]`, trimmed.
#[cfg(any(target_os = "linux", test))]
pub(crate) fn default_for(list: &str, mime: &str) -> Option<String> {
    let mut defaults = false;
    for line in list.lines().map(str::trim) {
        if line.starts_with('[') {
            defaults = line == "[Default Applications]";
        } else if defaults
            && let Some((key, apps)) = line.split_once('=')
            && key.trim() == mime
        {
            return Some(apps.trim().to_owned());
        }
    }
    None
}

/// The application a `mimeapps.list` value names first: the default.
#[cfg(any(target_os = "linux", test))]
pub(crate) fn first_app(apps: &str) -> &str {
    apps.split(';').next().map_or("", str::trim)
}

/// `list` with `mime`'s default given back: set to `previous`, or removed
/// when there was none, but only while it names `ours` first. A default
/// someone chose since is theirs, and everything else is left as it was.
#[cfg(any(target_os = "linux", test))]
fn restore_default(list: &str, mime: &str, ours: &str, previous: Option<&str>) -> String {
    let mut out = String::with_capacity(list.len());
    let mut defaults = false;
    let mut done = false;
    for line in list.split_inclusive('\n') {
        let trimmed = line.trim();
        if trimmed.starts_with('[') {
            defaults = trimmed == "[Default Applications]";
        } else if defaults
            && !done
            && let Some((key, apps)) = trimmed.split_once('=')
            && key.trim() == mime
        {
            // Only the first line counts, as for the desktop.
            done = true;
            if first_app(apps) == ours {
                if let Some(previous) = previous {
                    let end = if line.ends_with('\n') { "\n" } else { "" };
                    out.push_str(&format!("{mime}={previous}{end}"));
                }
                continue;
            }
        }
        out.push_str(line);
    }
    out
}

/// The folders desktop files are looked up in (each with an
/// `applications` folder): the user's data folder, `XDG_DATA_DIRS` (or its
/// default), and where Flatpak and Snap export theirs, in case the session
/// does not list them.
#[cfg(any(target_os = "linux", test))]
fn data_dirs(
    data_home: Option<PathBuf>,
    home: Option<&Path>,
    xdg_data_dirs: Option<&str>,
) -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = data_home.into_iter().collect();
    let system = xdg_data_dirs
        .filter(|dirs| !dirs.trim().is_empty())
        .unwrap_or("/usr/local/share:/usr/share");
    dirs.extend(
        system
            .split(':')
            .filter(|dir| !dir.is_empty())
            .map(PathBuf::from),
    );
    if let Some(home) = home {
        dirs.push(home.join(".local/share/flatpak/exports/share"));
    }
    dirs.push(PathBuf::from("/var/lib/flatpak/exports/share"));
    dirs.push(PathBuf::from("/var/lib/snapd/desktop"));
    dirs
}

/// The official Slack app's desktop file, if one is installed in `dirs`:
/// the package's, the Flatpak's or the Snap's, in that order.
#[cfg(any(target_os = "linux", test))]
fn pick_slack_desktop(dirs: &[PathBuf], exists: impl Fn(&Path) -> bool) -> Option<&'static str> {
    [
        "slack.desktop",
        "com.slack.Slack.desktop",
        "slack_slack.desktop",
    ]
    .into_iter()
    .find(|name| {
        dirs.iter()
            .any(|dir| exists(&dir.join("applications").join(name)))
    })
}

/// The `reg.exe` runs that give `HKCU\Software\Classes\slack` back: none
/// unless it still names NoSlacking; then delete it (only NoSlacking's
/// own keys are under it, or a copy of what the backup holds), and import
/// the backup when there is one.
#[cfg(any(windows, test))]
fn registry_release_plan(ours: bool, backup: Option<&Path>) -> Vec<Vec<String>> {
    if !ours {
        return Vec::new();
    }
    let mut plan = vec![vec![
        "delete".to_owned(),
        crate::auth::SLACK_KEY.to_owned(),
        "/f".to_owned(),
    ]];
    if let Some(backup) = backup {
        plan.push(vec!["import".to_owned(), backup.display().to_string()]);
    }
    plan
}

/// What `reg query …\slack /ve` says about the key.
#[cfg(any(windows, test))]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Key {
    Absent,
    /// NoSlacking's claim: its default value is NoSlacking's name.
    Ours,
    Theirs,
}

/// Reads [`Key`] from whether `reg query` succeeded and what it printed.
#[cfg(any(windows, test))]
fn key_from_query(found: bool, output: &str) -> Key {
    if !found {
        Key::Absent
    } else if output.contains(crate::auth::REGISTRY_NAME) {
        Key::Ours
    } else {
        Key::Theirs
    }
}

#[cfg(target_os = "linux")]
mod platform {
    use std::path::{Path, PathBuf};

    use super::{Claim, MIME, default_for, first_app, restore_default};
    use crate::auth::{SCHEME, SLACK_SCHEME};
    use crate::paths::APP_ID;

    fn ours() -> String {
        format!("{APP_ID}.desktop")
    }

    /// A Flatpak's `mimeapps.list` is its sandbox's, not the desktop's;
    /// the Flatpak's own desktop file lists the scheme instead.
    fn in_flatpak() -> bool {
        std::env::var_os("FLATPAK_ID").is_some()
    }

    fn mimeapps() -> Option<PathBuf> {
        directories::BaseDirs::new().map(|dirs| dirs.config_dir().join("mimeapps.list"))
    }

    fn read_list() -> String {
        mimeapps()
            .and_then(|path| std::fs::read_to_string(path).ok())
            .unwrap_or_default()
    }

    /// The Slack app's desktop file as a `mimeapps.list` value, to hand
    /// the links to when what they had before is not known.
    fn slack_app() -> Option<String> {
        let base = directories::BaseDirs::new();
        let dirs = super::data_dirs(
            base.as_ref().map(|dirs| dirs.data_local_dir().to_owned()),
            base.as_ref().map(directories::BaseDirs::home_dir),
            std::env::var("XDG_DATA_DIRS").ok().as_deref(),
        );
        super::pick_slack_desktop(&dirs, Path::exists).map(|name| format!("{name};"))
    }

    pub(super) fn remember(_state: &Path) -> Result<Option<Claim>, String> {
        if in_flatpak() {
            return Ok(None);
        }
        let previous = match default_for(&read_list(), MIME) {
            // Already NoSlacking's, from an older version: the Slack app's
            // is the best guess at what was there.
            Some(apps) if first_app(&apps) == ours() => slack_app(),
            other => other,
        };
        Ok(Some(Claim {
            previous,
            backup: false,
        }))
    }

    pub(super) fn claim() -> Result<(), String> {
        let exe = std::env::current_exe().map_err(|e| e.to_string())?;
        crate::auth::register_scheme_for(&exe, &[SCHEME, SLACK_SCHEME])
    }

    pub(super) fn leftover() -> Option<Claim> {
        if in_flatpak() {
            return None;
        }
        let apps = default_for(&read_list(), MIME)?;
        (first_app(&apps) == ours()).then(|| Claim {
            previous: slack_app(),
            backup: false,
        })
    }

    pub(super) fn give_back(_state: &Path, claim: &Claim) -> Result<(), String> {
        if in_flatpak() {
            return Ok(());
        }
        let path = mimeapps().ok_or("no home directory")?;
        match std::fs::read_to_string(&path) {
            Ok(list) => {
                let restored = restore_default(&list, MIME, &ours(), claim.previous.as_deref());
                if restored != list {
                    // Whole or not at all: other apps' defaults live here.
                    let temporary = path.with_extension("list.noslacking");
                    std::fs::write(&temporary, restored).map_err(|e| e.to_string())?;
                    std::fs::rename(&temporary, &path).map_err(|e| e.to_string())?;
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.to_string()),
        }
        // The desktop file stops offering NoSlacking for slack:// links.
        let exe = std::env::current_exe().map_err(|e| e.to_string())?;
        crate::auth::register_scheme_for(&exe, &[SCHEME])
    }
}

#[cfg(windows)]
mod platform {
    use std::path::Path;

    use super::{BACKUP, Claim, Key, key_from_query, registry_release_plan};
    use crate::auth::{SCHEME, SLACK_KEY, SLACK_SCHEME, run_reg};

    fn key() -> Key {
        match std::process::Command::new("reg")
            .args(["query", SLACK_KEY, "/ve"])
            .output()
        {
            Ok(output) => key_from_query(
                output.status.success(),
                &String::from_utf8_lossy(&output.stdout),
            ),
            Err(error) => {
                log::warn!("could not run reg.exe: {error}");
                Key::Absent
            }
        }
    }

    pub(super) fn remember(state: &Path) -> Result<Option<Claim>, String> {
        let backup = match key() {
            Key::Absent => false,
            // Left by an older version: there is nothing better to put back.
            Key::Ours => false,
            Key::Theirs => {
                std::fs::create_dir_all(state).map_err(|e| e.to_string())?;
                let file = state.join(BACKUP);
                let file = file.display().to_string();
                run_reg(&["export", SLACK_KEY, &file, "/y"])?;
                true
            }
        };
        Ok(Some(Claim {
            previous: None,
            backup,
        }))
    }

    pub(super) fn claim() -> Result<(), String> {
        let exe = std::env::current_exe().map_err(|e| e.to_string())?;
        crate::auth::register_scheme_for(&exe, &[SCHEME, SLACK_SCHEME])
    }

    pub(super) fn leftover() -> Option<Claim> {
        (key() == Key::Ours).then(Claim::default)
    }

    pub(super) fn give_back(state: &Path, claim: &Claim) -> Result<(), String> {
        let backup = state.join(BACKUP);
        let plan = registry_release_plan(
            key() == Key::Ours,
            (claim.backup && backup.exists()).then_some(backup.as_path()),
        );
        for args in plan {
            let args: Vec<&str> = args.iter().map(String::as_str).collect();
            run_reg(&args)?;
        }
        let _ = std::fs::remove_file(&backup);
        Ok(())
    }
}

#[cfg(not(any(target_os = "linux", windows)))]
mod platform {
    use std::path::Path;

    use super::Claim;

    pub(super) fn remember(_state: &Path) -> Result<Option<Claim>, String> {
        Err("NoSlacking cannot change the slack:// link handler on this platform".into())
    }

    pub(super) fn claim() -> Result<(), String> {
        Ok(())
    }

    pub(super) fn leftover() -> Option<Claim> {
        None
    }

    pub(super) fn give_back(_state: &Path, _claim: &Claim) -> Result<(), String> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const OURS: &str = "cloud.yannick.NoSlacking.desktop";

    #[test]
    fn the_slack_line_goes_back_to_what_it_was() {
        let list = "[Default Applications]\n\
                    text/html=firefox.desktop\n\
                    x-scheme-handler/slack=cloud.yannick.NoSlacking.desktop\n\
                    x-scheme-handler/http=firefox.desktop\n\n\
                    [Added Associations]\n\
                    x-scheme-handler/slack=cloud.yannick.NoSlacking.desktop;\n";
        let restored = restore_default(list, MIME, OURS, Some("slack.desktop;"));
        assert_eq!(
            restored,
            "[Default Applications]\n\
             text/html=firefox.desktop\n\
             x-scheme-handler/slack=slack.desktop;\n\
             x-scheme-handler/http=firefox.desktop\n\n\
             [Added Associations]\n\
             x-scheme-handler/slack=cloud.yannick.NoSlacking.desktop;\n",
            "other entries and sections stay as they were"
        );
        assert_eq!(
            default_for(&restored, MIME).as_deref(),
            Some("slack.desktop;")
        );
    }

    #[test]
    fn with_nothing_before_the_line_is_removed() {
        let list = "[Default Applications]\n\
                    x-scheme-handler/slack = cloud.yannick.NoSlacking.desktop;\n\
                    text/html=firefox.desktop";
        assert_eq!(
            restore_default(list, MIME, OURS, None),
            "[Default Applications]\ntext/html=firefox.desktop"
        );
        // The last line, without a newline, keeps going without one.
        let last =
            "[Default Applications]\nx-scheme-handler/slack=cloud.yannick.NoSlacking.desktop";
        assert_eq!(
            restore_default(last, MIME, OURS, Some("slack.desktop")),
            "[Default Applications]\nx-scheme-handler/slack=slack.desktop"
        );
    }

    #[test]
    fn a_handler_chosen_since_is_left_alone() {
        let theirs = "[Default Applications]\nx-scheme-handler/slack=slack.desktop;\n";
        assert_eq!(restore_default(theirs, MIME, OURS, None), theirs);
        assert_eq!(
            restore_default(theirs, MIME, OURS, Some("other.desktop")),
            theirs
        );
        // NoSlacking only among the alternatives: not the default.
        let second = "[Default Applications]\n\
                      x-scheme-handler/slack=slack.desktop;cloud.yannick.NoSlacking.desktop;\n";
        assert_eq!(restore_default(second, MIME, OURS, None), second);
        // Only under [Added Associations]: not a default to give back.
        let added = "[Added Associations]\n\
                     x-scheme-handler/slack=cloud.yannick.NoSlacking.desktop;\n";
        assert_eq!(restore_default(added, MIME, OURS, None), added);
    }

    #[test]
    fn a_missing_file_or_section_changes_nothing() {
        assert_eq!(restore_default("", MIME, OURS, Some("slack.desktop")), "");
        let no_section = "[Added Associations]\ntext/html=firefox.desktop\n";
        assert_eq!(
            restore_default(no_section, MIME, OURS, Some("slack.desktop")),
            no_section
        );
        let no_line = "[Default Applications]\ntext/html=firefox.desktop\n";
        assert_eq!(restore_default(no_line, MIME, OURS, None), no_line);
        assert_eq!(default_for(no_line, MIME), None);
        assert_eq!(first_app(" a.desktop ;b.desktop"), "a.desktop");
        assert_eq!(first_app(""), "");
    }

    #[test]
    fn the_slack_apps_desktop_file_is_found_where_desktops_look() {
        let dirs = data_dirs(
            Some(PathBuf::from("/home/u/.local/share")),
            Some(Path::new("/home/u")),
            Some("/opt/share::/usr/share"),
        );
        assert_eq!(dirs[0], PathBuf::from("/home/u/.local/share"));
        assert!(dirs.contains(&PathBuf::from("/opt/share")));
        assert!(!dirs.contains(&PathBuf::new()), "empty entries are skipped");
        assert!(dirs.contains(&PathBuf::from("/home/u/.local/share/flatpak/exports/share")));
        assert!(dirs.contains(&PathBuf::from("/var/lib/flatpak/exports/share")));
        let defaults = data_dirs(None, None, None);
        assert!(defaults.contains(&PathBuf::from("/usr/share")));
        assert!(defaults.contains(&PathBuf::from("/usr/local/share")));

        let installed = |files: &'static [&'static str]| {
            move |path: &Path| files.iter().any(|file| path == Path::new(file))
        };
        assert_eq!(
            pick_slack_desktop(
                &dirs,
                installed(&["/var/lib/flatpak/exports/share/applications/com.slack.Slack.desktop"])
            ),
            Some("com.slack.Slack.desktop")
        );
        assert_eq!(
            pick_slack_desktop(
                &dirs,
                installed(&[
                    "/var/lib/flatpak/exports/share/applications/com.slack.Slack.desktop",
                    "/usr/share/applications/slack.desktop",
                ])
            ),
            Some("slack.desktop"),
            "the package before the Flatpak"
        );
        assert_eq!(pick_slack_desktop(&dirs, installed(&[])), None);
    }

    #[test]
    fn what_is_given_back_and_when() {
        let live = Claim {
            previous: Some("slack.desktop;".into()),
            backup: false,
        };
        let recorded = Claim {
            previous: None,
            backup: true,
        };
        let leftover = Claim::default();
        let never =
            || -> Option<Claim> { panic!("only looked for at start-up with nothing known") };
        // A sign-in ending: this run's claim.
        assert_eq!(
            to_give_back(false, Some(&live), Some(live.clone()), never),
            Some(live.clone())
        );
        // At start-up a claim held by this run is a sign-in going on.
        assert_eq!(to_give_back(true, Some(&live), None, never), None);
        // A crashed run's record, at start-up or at any later release.
        assert_eq!(
            to_give_back(true, None, Some(recorded.clone()), never),
            Some(recorded.clone())
        );
        assert_eq!(
            to_give_back(false, None, Some(recorded.clone()), never),
            Some(recorded)
        );
        // No record: only start-up looks for an older version's claim.
        assert_eq!(
            to_give_back(true, None, None, || Some(leftover.clone())),
            Some(leftover)
        );
        assert_eq!(to_give_back(true, None, None, || None), None);
        assert_eq!(to_give_back(false, None, None, never), None);
    }

    #[test]
    fn the_record_survives_a_restart() {
        let dir = crate::paths::TestDir::new("slack-links");
        assert_eq!(read_record(&dir.0), None);
        let claim = Claim {
            previous: Some("slack.desktop;".into()),
            backup: false,
        };
        write_record(&dir.0, &claim).expect("written");
        assert_eq!(read_record(&dir.0), Some(claim));
        // A damaged record still says there is a claim to give back.
        std::fs::write(dir.0.join(RECORD), "{").expect("damaged");
        assert_eq!(read_record(&dir.0), Some(Claim::default()));
        // An older record without the Windows field reads too.
        std::fs::write(dir.0.join(RECORD), r#"{"previous":null}"#).expect("older");
        assert_eq!(read_record(&dir.0), Some(Claim::default()));
    }

    #[test]
    fn the_registry_is_given_back_only_from_noslacking() {
        let backup = Path::new(r"C:\state\slack-links.reg");
        assert!(registry_release_plan(false, Some(backup)).is_empty());
        assert_eq!(
            registry_release_plan(true, None),
            vec![vec![
                "delete".to_owned(),
                r"HKCU\Software\Classes\slack".to_owned(),
                "/f".to_owned()
            ]]
        );
        let plan = registry_release_plan(true, Some(backup));
        assert_eq!(plan.len(), 2);
        assert_eq!(plan[0][0], "delete");
        assert_eq!(
            plan[1],
            vec!["import".to_owned(), backup.display().to_string()]
        );

        let ours = "\nHKEY_CURRENT_USER\\Software\\Classes\\slack\n    (Default)    REG_SZ    URL:NoSlacking\n";
        let slack =
            "\nHKEY_CURRENT_USER\\Software\\Classes\\slack\n    (Default)    REG_SZ    URL:slack\n";
        assert_eq!(key_from_query(true, ours), Key::Ours);
        assert_eq!(key_from_query(true, slack), Key::Theirs);
        assert_eq!(key_from_query(false, ""), Key::Absent);
    }
}
