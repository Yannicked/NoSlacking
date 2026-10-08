//! The call window's own native window (the `huddle-video` feature),
//! in a deferred egui viewport: a share and a grid of cameras want room
//! the main window's panels cannot give them, and a window of its own can
//! go to another screen. It is open from Watch or Video until closed.
//!
//! The window repaints apart from the main window. Each new picture wakes
//! it alone, and it uploads the picture and draws; the main window, which
//! has nothing new to show, sleeps. An immediate viewport would have had
//! the main window's whole interface built and painted again for every
//! frame of every camera.
//!
//! The two meet in a [`CallWindow`]: the main window's frames write what
//! the call window shows (names, tiles, controls) and repaint it when that
//! changed; the call window leaves there what it did (its controls push
//! the same actions as the call bar's, the room it has for tiles) and
//! wakes the main window to apply it.

use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Instant;

use super::App;
use crate::huddles;
use crate::model::Action;
use crate::theme::Palette;
use crate::ui::call_window::{self, CallView, TileView};

/// The key of your own tile.
#[cfg(feature = "huddle-camera")]
const YOU: &str = "you";

/// The call window's viewport.
fn viewport() -> egui::ViewportId {
    egui::ViewportId::from_hash_of("huddle-call-window")
}

/// What the call window draws from and what it did, between the main
/// window's frames, which write the view, and the call window's own,
/// which upload the pictures and draw.
#[derive(Default)]
pub struct CallWindow {
    /// What to show, the pictures still to fill in; none while closed.
    view: Option<CallView>,
    /// When the huddle went live, for the clock, which moves without the
    /// main window.
    since: Option<Instant>,
    /// Whether the last tile is your own, which takes a place the others'
    /// cannot.
    yours: bool,
    /// The watched share's pictures, and the cameras'; each wakes the call
    /// window alone.
    screen: Option<huddles::Screen>,
    gallery: Option<huddles::Gallery>,
    /// Your camera's pictures put so far, to repaint the window when your
    /// tile has a new one.
    preview_pictures: u64,
    /// The share's newest picture, uploaded, and the share's own size.
    texture: Option<egui::TextureHandle>,
    source: [usize; 2],
    /// Which share it is of.
    of: Option<String>,
    /// Each camera tile's newest picture, uploaded, and the camera's own
    /// size, by camera.
    tiles: std::collections::BTreeMap<String, (egui::TextureHandle, [usize; 2])>,
    /// What the window's controls pushed, for the main window to apply.
    actions: Vec<Action>,
    /// The room for tiles (count and size) the window last had, and
    /// whether the main window has yet to tell the session.
    room: Option<(usize, [u32; 2])>,
    room_new: bool,
    /// The window was closed with its own button.
    closed: bool,
    /// Pixels handed to the textures in the window's last frame, for the
    /// demo's frame times.
    #[cfg(feature = "demo")]
    pub uploaded: usize,
}

impl std::fmt::Debug for CallWindow {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CallWindow")
            .field("open", &self.view.is_some())
            .field("source", &self.source)
            .field("of", &self.of)
            .field("tiles", &self.tiles.keys().collect::<Vec<_>>())
            .finish_non_exhaustive()
    }
}

/// Locks the window's state, going on with what a panicked frame left:
/// at worst a picture is drawn once more.
fn lock(window: &Mutex<CallWindow>) -> MutexGuard<'_, CallWindow> {
    window.lock().unwrap_or_else(PoisonError::into_inner)
}

impl App {
    /// Shows the call window if it is open: call once a frame, inside the
    /// main window's frame. Writes what it shows, applies what it did
    /// since the last frame, and keeps its viewport.
    pub fn show_call_window(&mut self, ctx: &egui::Context) {
        let shared = Arc::clone(&self.huddles.picture.window);
        let open = self
            .huddles
            .listening
            .as_ref()
            .filter(|l| l.window && l.in_huddle());
        let Some(listening) = open else {
            // Closed: the textures go with it, and anything it did late.
            *lock(&shared) = CallWindow::default();
            return;
        };
        let workspace = self
            .workspaces
            .iter()
            .find(|w| w.info.team_id == listening.team);
        let name = |user: Option<&str>| {
            user.and_then(|id| workspace.map(|w| w.user_label(id)))
                .unwrap_or_else(|| crate::i18n::t("Someone").into_owned())
        };
        let shares: Vec<(String, String)> = listening
            .shares
            .iter()
            .map(|s| (s.key.clone(), name(s.user.as_deref())))
            .collect();
        let key = listening.watching.clone();
        let sharer = key.as_ref().map(|key| {
            shares
                .iter()
                .find(|(k, _)| k == key)
                .map_or_else(|| name(None), |(_, n)| n.clone())
        });
        let screen = listening.screen.clone();
        let gallery = listening.gallery.clone();
        let faces = huddles::faces(&listening.roster);
        let cameras = &listening.cameras;
        let controls = controls(
            listening,
            workspace,
            (&self.settings.devices, &self.devices),
            Instant::now(),
        );
        let since = match listening.phase {
            huddles::Phase::Live { since } => Some(since),
            _ => None,
        };
        #[cfg_attr(not(feature = "huddle-camera"), allow(unused_mut))]
        let mut tiles: Vec<TileView> = cameras
            .iter()
            .filter(|c| c.tile)
            .map(|camera| {
                let face = camera
                    .user
                    .as_ref()
                    .and_then(|user| faces.iter().find(|f| f.user.as_ref() == Some(user)));
                let user = camera
                    .user
                    .as_deref()
                    .and_then(|id| workspace.and_then(|w| w.user(id)));
                TileView {
                    key: camera.key.clone(),
                    name: name(camera.user.as_deref()),
                    avatar: user.and_then(|u| u.avatar.clone()),
                    seed: camera.user.clone().unwrap_or_default(),
                    speaking: face.is_some_and(|f| f.speaking),
                    muted: face.is_some_and(|f| f.muted),
                    paused: camera.paused,
                    // The call window fills it in as it draws.
                    picture: None,
                }
            })
            .collect();
        // Your own camera, while it is on: one more tile, never received,
        // its picture the call bar's self-preview.
        #[cfg(feature = "huddle-camera")]
        let yours = listening.camera != crate::huddle_camera::Cam::Off;
        #[cfg(not(feature = "huddle-camera"))]
        let yours = false;
        #[cfg(feature = "huddle-camera")]
        if yours {
            let me = workspace
                .map(|w| w.info.user_id.clone())
                .unwrap_or_default();
            let face = faces.iter().find(|f| f.me);
            tiles.push(TileView {
                key: YOU.into(),
                name: crate::i18n::tf("{name} (you)", &[("name", &name(Some(&me)))]),
                avatar: workspace
                    .and_then(|w| w.user(&me))
                    .and_then(|u| u.avatar.clone()),
                seed: me,
                speaking: face.is_some_and(|f| f.speaking),
                muted: listening.mic != crate::huddle_mic::Mic::Live,
                paused: false,
                picture: self
                    .huddles
                    .preview
                    .texture
                    .as_ref()
                    .map(|t| (t.id(), self.huddles.preview.size)),
            });
        }
        #[cfg(feature = "huddle-camera")]
        let preview_pictures = listening
            .preview
            .as_ref()
            .map_or(0, crate::huddle_camera::Preview::pictures);
        #[cfg(not(feature = "huddle-camera"))]
        let preview_pictures = 0;
        let view = CallView {
            title: match (sharer.as_deref(), listening.is_call()) {
                (Some(sharer), _) => call_window::title(sharer),
                (None, true) if listening.meeting => crate::i18n::t("Meeting").into_owned(),
                (None, true) => call_window::call_title(&controls.name),
                (None, false) => call_window::cameras_title(),
            },
            shares,
            current: key,
            picture: None,
            more: cameras.iter().filter(|c| !c.tile).count(),
            tiles,
            no_video: screen.as_ref().is_some_and(huddles::Screen::no_video)
                || gallery.as_ref().is_some_and(huddles::Gallery::no_video),
            controls,
        };
        #[cfg_attr(not(feature = "demo"), allow(unused_mut))]
        let mut builder = egui::ViewportBuilder::default()
            .with_title(format!("{} – NoSlacking", view.title))
            .with_app_id(crate::paths::APP_ID)
            .with_inner_size([1280.0, 780.0])
            .with_min_inner_size([360.0, 320.0]);
        // The demo's size holds even over a size the window remembers.
        #[cfg(feature = "demo")]
        if let Some(size) = self.huddles.picture.size {
            builder = builder
                .with_inner_size(size)
                .with_min_inner_size(size)
                .with_max_inner_size(size);
        }
        #[cfg(feature = "demo")]
        if self.huddles.picture.embed {
            ctx.set_embed_viewports(true);
        }

        let (actions, room, closed) = {
            let mut window = lock(&shared);
            // New pictures wake the call window alone from now on.
            let wake = |ctx: &egui::Context| {
                let ctx = ctx.clone();
                move || ctx.request_repaint_of(viewport())
            };
            if window.screen != screen {
                if let Some(screen) = &screen {
                    screen.set_wake(wake(ctx));
                }
                window.screen = screen;
            }
            if window.gallery != gallery {
                if let Some(gallery) = &gallery {
                    gallery.set_wake(wake(ctx));
                }
                window.gallery = gallery;
            }
            // Repainted only when what it shows changed: names, faces
            // speaking, the controls, your own new picture.
            let changed =
                window.view.as_ref() != Some(&view) || window.preview_pictures != preview_pictures;
            window.view = Some(view);
            window.since = since;
            window.yours = yours;
            window.preview_pictures = preview_pictures;
            if changed {
                ctx.request_repaint_of(viewport());
            }
            let room = std::mem::take(&mut window.room_new).then_some(window.room);
            (
                std::mem::take(&mut window.actions),
                room,
                std::mem::take(&mut window.closed),
            )
        };
        self.actions.extend(actions);
        if closed {
            self.actions
                .push(Action::Huddle(huddles::Action::Watch(None)));
            return;
        }
        if let Some(room) = room {
            huddles::tell_wish(self, room);
        }
        let palette = self.palette;
        ctx.show_viewport_deferred(viewport(), builder, move |ui, _class| {
            draw(ui, &palette, &shared);
        });
    }
}

/// One frame of the call window: takes and uploads the new pictures,
/// draws, and leaves what it did for the main window, waking it only for
/// that.
fn draw(ui: &mut egui::Ui, palette: &Palette, shared: &Mutex<CallWindow>) {
    let ctx = ui.ctx().clone();
    let wake_main = || ctx.request_repaint_of(egui::ViewportId::ROOT);
    if ui.input(|i| i.viewport().close_requested()) {
        lock(shared).closed = true;
        wake_main();
        return;
    }
    let (view, screen, gallery, yours) = {
        let mut window = lock(shared);
        let Some(mut view) = window.view.clone() else {
            return;
        };
        upload(&ctx, &mut window, &mut view);
        if let (Some(since), true) = (window.since, view.controls.live) {
            view.controls.time = huddles::clock(Instant::now().saturating_duration_since(since));
        }
        (
            view,
            window.screen.clone(),
            window.gallery.clone(),
            window.yours,
        )
    };
    let mut actions = Vec::new();
    let shown = call_window::show(ui, palette, &view, &mut actions);
    // Minimised or covered, as far as the system says: the helper then
    // sends no pictures until it can be seen again.
    let visible = ui.input(|i| i.viewport().visible()).unwrap_or(true);
    if let Some(screen) = &screen {
        screen.set_fit(shown.share[0], shown.share[1]);
        screen.set_visible(visible);
    }
    if let Some(gallery) = &gallery {
        gallery.set_fit(shown.tile[0] as usize, shown.tile[1] as usize);
        gallery.set_visible(visible);
    }
    // Your own tile takes a place the others' cannot.
    let room = Some((
        if yours {
            shown.room.saturating_sub(1)
        } else {
            shown.room
        },
        shown.tile,
    ));
    let mut window = lock(shared);
    let mut wake = !actions.is_empty();
    window.actions.append(&mut actions);
    if window.room != room {
        window.room = room;
        window.room_new = true;
        wake = true;
    }
    drop(window);
    if wake {
        wake_main();
    }
}

/// Uploads the pictures that came since the last frame into `window`'s
/// textures, and puts them in `view`.
fn upload(ctx: &egui::Context, window: &mut CallWindow, view: &mut CallView) {
    #[cfg(feature = "demo")]
    {
        window.uploaded = 0;
    }
    if window.of != view.current {
        // Another share, or none: not the last one's picture.
        window.texture = None;
        window.source = [0, 0];
        window.of.clone_from(&view.current);
    }
    if let Some(new) = window.screen.as_ref().and_then(huddles::Screen::take) {
        #[cfg(feature = "demo")]
        {
            window.uploaded += new.image.pixels.len();
        }
        let options = egui::TextureOptions::LINEAR;
        match &mut window.texture {
            Some(texture) => texture.set(new.image, options),
            None => window.texture = Some(ctx.load_texture("huddle-share", new.image, options)),
        }
        window.source = new.source;
    }
    // Tiles that went lose their texture; new pictures replace the old.
    window
        .tiles
        .retain(|key, _| view.tiles.iter().any(|t| t.key == *key));
    let new = window
        .gallery
        .as_ref()
        .map(huddles::Gallery::take)
        .unwrap_or_default();
    for (camera, new) in new {
        if !view.tiles.iter().any(|t| t.key == camera) {
            continue;
        }
        #[cfg(feature = "demo")]
        {
            window.uploaded += new.image.pixels.len();
        }
        let options = egui::TextureOptions::LINEAR;
        match window.tiles.get_mut(&camera) {
            Some((texture, source)) => {
                texture.set(new.image, options);
                *source = new.source;
            }
            None => {
                let texture =
                    ctx.load_texture(format!("huddle-camera-{camera}"), new.image, options);
                window.tiles.insert(camera, (texture, new.source));
            }
        }
    }
    view.picture = window.texture.as_ref().map(|t| (t.id(), window.source));
    for tile in &mut view.tiles {
        if let Some((texture, source)) = window.tiles.get(&tile.key) {
            tile.picture = Some((texture.id(), *source));
        }
    }
}

/// The call window's controls for `listening` at `now`: the huddle's
/// name as the sidebar has it, how long it has run, and your microphone
/// and camera.
fn controls(
    listening: &huddles::Listening,
    workspace: Option<&super::WorkspaceState>,
    devices: (&crate::devices::Chosen, &crate::devices::State),
    now: std::time::Instant,
) -> call_window::Controls {
    let conversation = workspace.and_then(|w| {
        w.conversation(&listening.channel)
            .map(|c| (c.kind.is_dm(), w.title(c)))
    });
    let name = match conversation {
        Some((true, name)) => name,
        Some((false, name)) => format!("#{name}"),
        None if listening.meeting => crate::i18n::t("Meeting").into_owned(),
        None => listening.channel.clone(),
    };
    let (live, time) = match listening.phase {
        huddles::Phase::Live { since } => {
            (true, huddles::clock(now.saturating_duration_since(since)))
        }
        huddles::Phase::Ringing => (false, crate::i18n::t("Ringing…").into_owned()),
        _ => (false, crate::i18n::t("Joining…").into_owned()),
    };
    call_window::Controls {
        name,
        time,
        live,
        leaving: crate::ui::call_bar::Leaving::of(listening),
        mic: listening.mic,
        #[cfg(feature = "huddle-camera")]
        camera: listening.camera,
        #[cfg(feature = "huddle-share")]
        sharing: listening.sharing,
        chosen: devices.0.clone(),
        lists: devices.1.clone(),
    }
}
