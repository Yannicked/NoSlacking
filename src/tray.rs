//! The tray item: a StatusNotifierItem on Linux, a notification-area icon
//! on Windows, a menu-bar item on macOS, through fastframe-tray (as in
//! ZapFast and Spotifast). It shows or hides the window, says what is
//! unread, and quits.
//!
//! fastframe-tray draws one icon for the life of the item, so the unread
//! state shows in its menu, not on the icon.

use crate::badge::Unread;
use crate::i18n::{t, tn};
use crate::paths::APP_ID;

/// What the tray asked for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Request {
    /// Show the window if it is hidden, hide it otherwise.
    Toggle,
    Show,
    Quit,
}

/// The menu entry that says what is unread; choosing it shows the window.
const UNREAD: &str = "unread";

/// The tray item, while the desktop offers one.
pub struct Tray {
    tray: fastframe_tray::Tray,
    shown: Option<Unread>,
}

impl std::fmt::Debug for Tray {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Tray")
            .field("shown", &self.shown)
            .finish_non_exhaustive()
    }
}

/// The app's icon at `size` pixels a side, as RGBA.
fn icon(size: usize) -> Vec<u8> {
    let bytes =
        include_bytes!("../packaging/icons/hicolor/64x64/apps/cloud.yannick.NoSlacking.png");
    let side = u32::try_from(size).unwrap_or(64).max(1);
    match image::load_from_memory(bytes) {
        Ok(image) => {
            let image = if image.width() == side {
                image
            } else {
                image.resize_exact(side, side, image::imageops::FilterType::Triangle)
            };
            image.to_rgba8().into_raw()
        }
        Err(_) => vec![0; size * size * 4],
    }
}

/// What the unread entry says.
pub fn unread_label(unread: Unread) -> String {
    if unread.mentions > 0 {
        tn(
            "{count} mention or direct message",
            "{count} mentions or direct messages",
            unread.mentions,
        )
    } else if unread.unread {
        t("Unread messages").into_owned()
    } else {
        t("Nothing unread").into_owned()
    }
}

impl Tray {
    /// Registers the item; `None` when the desktop has no tray. `wake` is
    /// called, from the tray's thread, whenever something was clicked.
    pub fn spawn(wake: impl Fn() + Send + Sync + 'static) -> Option<Self> {
        use fastframe_tray::MenuItem;
        let config = fastframe_tray::Config {
            id: APP_ID,
            title: "NoSlacking".to_owned(),
            icon,
            template_icon: None,
            menu: vec![
                MenuItem::action("show", t("Show NoSlacking")),
                MenuItem::action(UNREAD, unread_label(Unread::default())),
                MenuItem::Separator,
                MenuItem::action("quit", t("Quit NoSlacking")),
            ],
        };
        let tray = fastframe_tray::Tray::spawn(config, wake)?;
        Some(Self { tray, shown: None })
    }

    /// What was asked for since the last call, oldest first.
    pub fn requests(&self) -> Vec<Request> {
        use fastframe_tray::Event;
        self.tray
            .events()
            .into_iter()
            .filter_map(|event| match event {
                Event::Toggle => Some(Request::Toggle),
                Event::Show | Event::Menu("show" | UNREAD) => Some(Request::Show),
                Event::Menu("quit") => Some(Request::Quit),
                Event::Menu(_) => None,
            })
            .collect()
    }

    /// Says what is unread, when that changed.
    pub fn set_unread(&mut self, unread: Unread) {
        if self.shown != Some(unread) {
            self.shown = Some(unread);
            self.tray.set_label(UNREAD, unread_label(unread));
        }
    }

    /// A window exists: on macOS this makes the item (the first time) and
    /// brings the app forward.
    pub fn attach(&mut self) {
        self.tray.attach();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_icon_comes_in_any_size() {
        assert_eq!(icon(64).len(), 64 * 64 * 4);
        assert_eq!(icon(22).len(), 22 * 22 * 4);
    }

    #[test]
    fn the_menu_says_what_is_unread() {
        assert_eq!(unread_label(Unread::default()), "Nothing unread");
        assert_eq!(
            unread_label(Unread {
                mentions: 2,
                unread: true
            }),
            "2 mentions or direct messages"
        );
    }
}
