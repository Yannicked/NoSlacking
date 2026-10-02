//! What is waiting across every workspace, shown outside the window: in the
//! window title (`(3) NoSlacking`), which taskbars and window switchers
//! show everywhere, and on Linux as a count on the launcher icon.
//!
//! The launcher count uses the `com.canonical.Unity.LauncherEntry` D-Bus
//! signal, which KDE Plasma's task manager, Dash to Dock, Dash to Panel and
//! Plank draw. Desktops without one ignore it. macOS and Windows keep the
//! title, which their app switchers show; a Dock or taskbar badge there
//! needs calls into AppKit or COM that this crate keeps out (no `unsafe`).

/// What is waiting.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Unread {
    /// Mentions of you and unread direct messages.
    pub mentions: u32,
    /// Whether any conversation has unread messages at all.
    pub unread: bool,
}

/// The window title for `unread`: `(3) NoSlacking` for mentions, a dot
/// for unread messages without one, the plain name otherwise.
pub fn window_title(unread: Unread) -> String {
    if unread.mentions > 0 {
        format!("({}) NoSlacking", unread.mentions)
    } else if unread.unread {
        "• NoSlacking".to_owned()
    } else {
        "NoSlacking".to_owned()
    }
}

/// Sends the count to the launcher icon on a thread of its own, since a
/// D-Bus call can take a moment. Only the newest count matters, so counts
/// queued faster than the bus takes them collapse into it.
pub struct Launcher {
    counts: std::sync::mpsc::Sender<Unread>,
}

impl std::fmt::Debug for Launcher {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Launcher").finish_non_exhaustive()
    }
}

impl Launcher {
    /// Starts the launcher thread, or `None` where there is no launcher to
    /// tell (anything but Linux) or the thread did not start.
    pub fn spawn() -> Option<Self> {
        if !launcher::AVAILABLE {
            return None;
        }
        let (counts, queue) = std::sync::mpsc::channel::<Unread>();
        let spawned = std::thread::Builder::new()
            .name("launcher-badge".into())
            .spawn(move || launcher::run(&queue));
        match spawned {
            Ok(_) => Some(Self { counts }),
            Err(error) => {
                log::debug!("no launcher badge: {error}");
                None
            }
        }
    }

    /// Shows `unread` on the launcher icon soon.
    pub fn set(&self, unread: Unread) {
        let _ = self.counts.send(unread);
    }
}

#[cfg(target_os = "linux")]
mod launcher {
    use std::collections::HashMap;
    use std::sync::mpsc;

    use zbus::zvariant::Value;

    use super::Unread;

    pub const AVAILABLE: bool = true;

    /// The desktop file the launcher knows the app by: the Flatpak's own
    /// id inside one, the packaged id otherwise.
    fn app_uri() -> String {
        let id = std::env::var("FLATPAK_ID").unwrap_or_else(|_| crate::paths::APP_ID.to_owned());
        format!("application://{id}.desktop")
    }

    pub fn run(queue: &mpsc::Receiver<Unread>) {
        let connection = match zbus::blocking::Connection::session() {
            Ok(connection) => connection,
            Err(error) => {
                log::debug!("no launcher badge without a session bus: {error}");
                // Drain, so senders never notice.
                for _ in queue {}
                return;
            }
        };
        let uri = app_uri();
        while let Ok(first) = queue.recv() {
            let unread = queue.try_iter().last().unwrap_or(first);
            let properties: HashMap<&str, Value<'_>> = HashMap::from([
                ("count", Value::from(i64::from(unread.mentions))),
                ("count-visible", Value::from(unread.mentions > 0)),
                ("urgent", Value::from(false)),
            ]);
            let sent = connection.emit_signal(
                None::<&str>,
                "/com/canonical/unity/launcherentry/noslacking",
                "com.canonical.Unity.LauncherEntry",
                "Update",
                &(uri.as_str(), properties),
            );
            if let Err(error) = sent {
                log::debug!("could not update the launcher badge: {error}");
            }
        }
    }
}

#[cfg(not(target_os = "linux"))]
mod launcher {
    use super::Unread;

    pub const AVAILABLE: bool = false;

    pub fn run(queue: &std::sync::mpsc::Receiver<Unread>) {
        for _ in queue {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_title_counts_mentions_and_marks_unread() {
        assert_eq!(window_title(Unread::default()), "NoSlacking");
        assert_eq!(
            window_title(Unread {
                mentions: 0,
                unread: true
            }),
            "• NoSlacking"
        );
        assert_eq!(
            window_title(Unread {
                mentions: 3,
                unread: true
            }),
            "(3) NoSlacking"
        );
    }
}
