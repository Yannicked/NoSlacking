//! The call window's own native window (the `huddle-video` feature),
//! through egui's immediate viewports as the conversation pop-outs are:
//! a share wants room the main window's panels cannot give it, and a
//! window of its own can go to another screen. It is open while a share
//! is watched; each frame its newest picture, if one came, replaces the
//! texture it is drawn from.

use super::App;
use crate::huddles;
use crate::model::Action;
use crate::ui::call_window::{self, CallView};

impl App {
    /// Draws the call window if a share is watched; call once a frame,
    /// inside the main window's frame.
    pub fn show_call_window(&mut self, ctx: &egui::Context) {
        let watched = self
            .huddles
            .listening
            .as_ref()
            .and_then(|l| Some((l, l.watching.clone()?)));
        let Some((listening, key)) = watched else {
            // Closed: the texture goes with it.
            let picture = &mut self.huddles.picture;
            picture.texture = None;
            picture.source = [0, 0];
            picture.of = None;
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
        let sharer = shares
            .iter()
            .find(|(k, _)| *k == key)
            .map_or_else(|| name(None), |(_, n)| n.clone());
        let screen = listening.screen.clone();

        let picture = &mut self.huddles.picture;
        if picture.of.as_deref() != Some(key.as_str()) {
            // Another share: not the last one's picture.
            picture.texture = None;
            picture.source = [0, 0];
            picture.of = Some(key.clone());
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
        let view = CallView {
            title: call_window::title(&sharer),
            shares,
            current: key,
            picture: picture.texture.as_ref().map(|t| (t.id(), picture.source)),
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
        let open = ctx.show_viewport_immediate(viewport, builder, |ui, _class| {
            if ui.input(|i| i.viewport().close_requested()) {
                return false;
            }
            let shown = call_window::show(ui, &palette, &view, &mut actions);
            if let Some(screen) = &screen {
                screen.set_fit(shown[0], shown[1]);
            }
            true
        });
        if !open {
            actions.push(Action::Huddle(huddles::Action::Watch(None)));
        }
        self.actions.append(&mut actions);
    }
}
