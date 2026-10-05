//! Starting NoSlacking when you log in, the way each desktop expects:
//! an XDG autostart entry on Linux, a `Run` value in the registry on
//! Windows, a LaunchAgent on macOS. Each starts it with `--hidden`, which
//! opens no window when it can keep running in the tray.
//!
//! The entries name the executable that wrote them, so turning the setting
//! on again after moving the app updates them.

use std::path::Path;

/// The flag that starts NoSlacking in the tray.
pub const HIDDEN: &str = "--hidden";

/// Adds (or with `false` removes) the login entry for this executable.
pub fn set(enabled: bool) -> Result<(), String> {
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    platform::set(&exe, enabled)
}

/// `path` as one argument of a desktop entry's `Exec` key, ready to write
/// into the file. Shared by the autostart entry and the link handler's
/// desktop file (see [`crate::auth`]).
///
/// The Desktop Entry specification reads the value in two steps, so it is
/// written in two: first as an `Exec` argument (in double quotes, with
/// `"`, `` ` ``, `$` and `\` escaped by a backslash, and `%` doubled so it
/// is never taken for a field code like `%u`), then as a key-file string
/// (every backslash doubled again, and line breaks and tabs spelled out so
/// a strange path cannot end the line and add keys of its own).
pub fn exec_quote(path: &str) -> String {
    let mut quoted = String::with_capacity(path.len() + 2);
    quoted.push('"');
    for c in path.chars() {
        match c {
            '"' | '`' | '$' | '\\' => {
                quoted.push('\\');
                quoted.push(c);
            }
            '%' => quoted.push_str("%%"),
            c => quoted.push(c),
        }
    }
    quoted.push('"');
    let mut out = String::with_capacity(quoted.len());
    for c in quoted.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c => out.push(c),
        }
    }
    out
}

/// The XDG autostart entry that starts `exe` hidden.
pub fn desktop_entry(exe: &Path) -> String {
    use crate::paths::APP_ID;
    let exec = exec_quote(&exe.display().to_string());
    format!(
        "[Desktop Entry]\nType=Application\nName=NoSlacking\nComment=A native Slack client\n\
         Exec={exec} {HIDDEN}\nIcon={APP_ID}\nTerminal=false\nX-GNOME-Autostart-enabled=true\n"
    )
}

/// `text` safe inside an XML element.
fn xml_escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// The LaunchAgent property list that starts `exe` hidden at login.
pub fn launch_agent(exe: &Path) -> String {
    use crate::paths::APP_ID;
    let program = xml_escape(&exe.display().to_string());
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
         <!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n\
         <plist version=\"1.0\">\n<dict>\n\
         \t<key>Label</key>\n\t<string>{APP_ID}</string>\n\
         \t<key>ProgramArguments</key>\n\t<array>\n\t\t<string>{program}</string>\n\t\t<string>{HIDDEN}</string>\n\t</array>\n\
         \t<key>RunAtLoad</key>\n\t<true/>\n\
         </dict>\n</plist>\n"
    )
}

/// The `Run` value that starts `exe` hidden on Windows.
pub fn run_command(exe: &Path) -> String {
    format!("\"{}\" {HIDDEN}", exe.display())
}

/// Writes `contents` to `path`, or removes it, creating its folder.
#[cfg_attr(not(any(target_os = "linux", target_os = "macos")), allow(dead_code))]
fn write_or_remove(path: &Path, contents: Option<String>) -> Result<(), String> {
    match contents {
        Some(contents) => {
            if let Some(folder) = path.parent() {
                std::fs::create_dir_all(folder).map_err(|e| e.to_string())?;
            }
            crate::paths::write_atomic(path, contents.as_bytes()).map_err(|e| e.to_string())
        }
        None => match std::fs::remove_file(path) {
            Err(error) if error.kind() != std::io::ErrorKind::NotFound => Err(error.to_string()),
            _ => Ok(()),
        },
    }
}

#[cfg(target_os = "linux")]
mod platform {
    use std::path::Path;

    pub fn set(exe: &Path, enabled: bool) -> Result<(), String> {
        // The sandbox may not write the user's autostart folder; Flatpak
        // apps ask the desktop through its background portal instead.
        if std::env::var_os("FLATPAK_ID").is_some() {
            return Err("inside Flatpak, add NoSlacking to your desktop's startup apps".into());
        }
        let file = directories::BaseDirs::new()
            .ok_or("no home folder")?
            .config_dir()
            .join("autostart")
            .join(format!("{}.desktop", crate::paths::APP_ID));
        super::write_or_remove(&file, enabled.then(|| super::desktop_entry(exe)))
    }
}

#[cfg(target_os = "macos")]
mod platform {
    use std::path::Path;

    pub fn set(exe: &Path, enabled: bool) -> Result<(), String> {
        // launchd reads the folder at the next login.
        let file = directories::BaseDirs::new()
            .ok_or("no home folder")?
            .home_dir()
            .join("Library/LaunchAgents")
            .join(format!("{}.plist", crate::paths::APP_ID));
        super::write_or_remove(&file, enabled.then(|| super::launch_agent(exe)))
    }
}

#[cfg(windows)]
mod platform {
    use std::path::Path;

    const KEY: &str = r"HKCU\Software\Microsoft\Windows\CurrentVersion\Run";

    pub fn set(exe: &Path, enabled: bool) -> Result<(), String> {
        let command = super::run_command(exe);
        let args: Vec<&str> = if enabled {
            vec![
                "add",
                KEY,
                "/v",
                "NoSlacking",
                "/t",
                "REG_SZ",
                "/d",
                &command,
                "/f",
            ]
        } else {
            vec!["delete", KEY, "/v", "NoSlacking", "/f"]
        };
        let status = std::process::Command::new("reg")
            .args(&args)
            .status()
            .map_err(|e| format!("reg.exe: {e}"))?;
        // Deleting a value that is not there fails too, which is fine.
        if status.success() || !enabled {
            Ok(())
        } else {
            Err("reg.exe could not add the login entry".into())
        }
    }
}

#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
mod platform {
    use std::path::Path;

    pub fn set(_exe: &Path, _enabled: bool) -> Result<(), String> {
        Err("starting at login is not supported on this system".into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_desktop_entry_quotes_the_path_and_starts_hidden() {
        let entry = desktop_entry(Path::new("/opt/No Slacking/$bin/noslacking"));
        assert!(
            entry.contains("Exec=\"/opt/No Slacking/\\\\$bin/noslacking\" --hidden\n"),
            "{entry}"
        );
        assert!(entry.starts_with("[Desktop Entry]\n"));
    }

    #[test]
    fn exec_arguments_follow_the_desktop_entry_spec() {
        // The spec's own examples: a literal backslash takes four, a
        // literal dollar sign `\\$`.
        assert_eq!(exec_quote(r"a\b"), r#""a\\\\b""#);
        assert_eq!(exec_quote("$HOME"), r#""\\$HOME""#);
        assert_eq!(exec_quote(r#"say "hi" `now`"#), r#""say \\"hi\\" \\`now\\`""#);
        // A percent sign is never a field code.
        assert_eq!(exec_quote("/opt/100%u/app"), r#""/opt/100%%u/app""#);
        // A line break cannot start a new key.
        assert_eq!(exec_quote("a\nIcon=x"), r#""a\nIcon=x""#);
        assert_eq!(exec_quote("/usr/bin/noslacking"), r#""/usr/bin/noslacking""#);
    }

    #[test]
    fn the_launch_agent_escapes_the_path() {
        let plist = launch_agent(Path::new("/Applications/A&B.app/Contents/MacOS/noslacking"));
        assert!(
            plist.contains("<string>/Applications/A&amp;B.app/Contents/MacOS/noslacking</string>")
        );
        assert!(plist.contains("<string>--hidden</string>"));
        assert!(plist.contains("<key>RunAtLoad</key>\n\t<true/>"));
    }

    #[test]
    fn the_run_value_quotes_the_path() {
        assert_eq!(
            run_command(Path::new(r"C:\Program Files\noslacking.exe")),
            r#""C:\Program Files\noslacking.exe" --hidden"#
        );
    }

    #[test]
    fn entries_are_written_and_removed() {
        let dir = crate::paths::TestDir::new("autostart");
        let file = dir.0.join("sub").join("entry.desktop");
        write_or_remove(&file, Some("x".into())).expect("written");
        assert_eq!(std::fs::read_to_string(&file).expect("read"), "x");
        write_or_remove(&file, None).expect("removed");
        assert!(!file.exists());
        write_or_remove(&file, None).expect("already gone is fine");
    }
}
