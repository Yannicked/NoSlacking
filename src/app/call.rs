//! The call window's own native window (the `huddle-video` feature),
//! through egui's immediate viewports as the conversation pop-outs are:
//! a share and a grid of cameras want room the main window's panels
//! cannot give them, and a window of its own can go to another screen.
//! It is open from Watch or Video until closed; each frame the newest
//! pictures, if any came, replace the textures they are drawn from, and
//! the session hears how many tiles of what size the window has room
//! for.

use super::App;
use crate::huddles;
use crate::model::Action;
use crate::ui::call_window::{self, CallView, TileView};

impl App {
    /// Draws the call window if it is open; call once a frame, inside
    /// the main window's frame.
    pub fn show_call_window(&mut self, ctx: &egui::Context) {
        let open = self
            .huddles
            .listening
            .as_ref()
            .filter(|l| l.window && l.in_huddle());
        let Some(listening) = open else {
            // Closed: the textures go with it.
            let picture = &mut self.huddles.picture;
            picture.texture = None;
            picture.source = [0, 0];
            picture.of = None;
            picture.tiles.clear();
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
        let cameras: Vec<huddles::Camera> = listening.cameras.clone();

        let picture = &mut self.huddles.picture;
        if picture.of != key {
            // Another share, or none: not the last one's picture.
            picture.texture = None;
            picture.source = [0, 0];
            picture.of.clone_from(&key);
        }
        if let Some(new) = screen.as_ref().and_then(huddles::Screen::take) {
            let options = egui::TextureOptions::LINEAR;
            match &mut picture.texture {
                Some(texture) => texture.set(new.image, options),
                None => {
                    picture.texture = Some(ctx.load_texture("huddle-share", new.image, options))
                }
            }
            picture.source = new.source;
        }
        // Tiles that went lose their texture; new pictures replace the old.
        picture
            .tiles
            .retain(|key, _| cameras.iter().any(|c| c.tile && c.key == *key));
        for (camera, new) in gallery
            .as_ref()
            .map(huddles::Gallery::take)
            .unwrap_or_default()
        {
            if !cameras.iter().any(|c| c.tile && c.key == camera) {
                continue;
            }
            let options = egui::TextureOptions::LINEAR;
            match picture.tiles.get_mut(&camera) {
                Some((texture, source)) => {
                    texture.set(new.image, options);
                    *source = new.source;
                }
                None => {
                    let texture =
                        ctx.load_texture(format!("huddle-camera-{camera}"), new.image, options);
                    picture.tiles.insert(camera, (texture, new.source));
                }
            }
        }
        let tiles: Vec<TileView> = cameras
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
                    picture: picture
                        .tiles
                        .get(&camera.key)
                        .map(|(texture, source)| (texture.id(), *source)),
                }
            })
            .collect();
        let view = CallView {
            title: sharer
                .as_deref()
                .map_or_else(call_window::cameras_title, call_window::title),
            shares,
            current: key,
            picture: picture.texture.as_ref().map(|t| (t.id(), picture.source)),
            more: cameras.iter().filter(|c| !c.tile).count(),
            tiles,
        };
        let builder = egui::ViewportBuilder::default()
            .with_title(format!("{} – NoSlacking", view.title))
            .with_app_id(crate::paths::APP_ID)
            .with_inner_size([1280.0, 780.0])
            .with_min_inner_size([480.0, 320.0]);
        let palette = self.palette;
        let mut actions = Vec::new();
        #[cfg(feature = "demo")]
        if picture.embed {
            ctx.set_embed_viewports(true);
        }
        let viewport = egui::ViewportId::from_hash_of("huddle-call-window");
        let mut room = None;
        let open = ctx.show_viewport_immediate(viewport, builder, |ui, _class| {
            if ui.input(|i| i.viewport().close_requested()) {
                return false;
            }
            let shown = call_window::show(ui, &palette, &view, &mut actions);
            if let Some(screen) = &screen {
                screen.set_fit(shown.share[0], shown.share[1]);
            }
            if let Some(gallery) = &gallery {
                gallery.set_fit(shown.tile[0] as usize, shown.tile[1] as usize);
            }
            room = Some((shown.room, shown.tile));
            true
        });
        if open {
            if room.is_some() {
                huddles::tell_wish(self, room);
            }
        } else {
            actions.push(Action::Huddle(huddles::Action::Watch(None)));
        }
        self.actions.append(&mut actions);
    }
}
