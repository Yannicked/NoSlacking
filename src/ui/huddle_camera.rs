//! Your camera in the call bar (the `huddle-camera` feature): its button
//! beside the microphone's while the huddle is live, and the small
//! self-preview above the buttons while it is on. Self-contained, so the
//! call bar only places them.
//!
//! Off it is a quiet button with a red, struck-through camera; on it is
//! filled with the huddle's green and a white camera, so a camera that
//! is on is never missed. Cmd+Shift+O toggles it (Teams' chord: Slack's
//! own is the composer's paste without formatting here) wherever the bar
//! shows. The preview is mirrored, as a mirror shows you.

use egui::{Color32, CornerRadius, Key, Modifiers, RichText, Sense, Vec2};

use super::people::ACTIVE;
use super::shortcuts::spell;
use crate::app::App;
use crate::huddle_camera::{Cam, CamAction};
use crate::i18n::{t, tf};
use crate::theme::{self, Palette};

/// The chord that toggles the camera, as the shortcut sheet lists it.
pub const TOGGLE: &str = "Cmd+Shift+O";
/// The preview's width at most, in points.
const PREVIEW_WIDTH: f32 = 220.0;

/// What a click or the chord asks in state `cam`.
pub fn toggled(cam: Cam) -> CamAction {
    match cam {
        Cam::Off => CamAction::On,
        Cam::Opening | Cam::On => CamAction::Off,
    }
}

/// Draws the button for `cam`; returns what was asked, by a click or the
/// chord.
pub fn camera_button(ui: &mut egui::Ui, palette: &Palette, cam: Cam) -> Option<CamAction> {
    let shortcut = spell(TOGGLE, cfg!(target_os = "macos"));
    let (icon, label, tip) = match cam {
        Cam::Off => (
            theme::Icon::VideoOff,
            t("Video"),
            tf(
                "Your camera is off. Start video ({shortcut})",
                &[("shortcut", &shortcut)],
            ),
        ),
        Cam::Opening => (
            theme::Icon::Video,
            t("Video"),
            tf(
                "Opening your camera. Stop video ({shortcut})",
                &[("shortcut", &shortcut)],
            ),
        ),
        Cam::On => (
            theme::Icon::Video,
            t("Video"),
            tf(
                "Your camera is on: everyone sees you. Stop video ({shortcut})",
                &[("shortcut", &shortcut)],
            ),
        ),
    };
    let (fill, ink, icon_ink) = match cam {
        Cam::On => (ACTIVE, Color32::WHITE, Color32::WHITE),
        Cam::Opening => (palette.surface_hover, palette.secondary, palette.secondary),
        Cam::Off => (palette.surface_hover, palette.text, palette.danger),
    };
    let button = egui::Button::image_and_text(
        icon.image(icon_ink, 14.0),
        RichText::new(label).font(theme::medium(13.0)).color(ink),
    )
    .fill(fill)
    .corner_radius(CornerRadius::same(theme::RADIUS_SMALL + 2))
    .min_size(Vec2::new(0.0, 28.0));
    let response = ui
        .add(button)
        .on_hover_cursor(egui::CursorIcon::PointingHand)
        .on_hover_text(tip);
    let chord =
        ui.input_mut(|input| input.consume_key(Modifiers::COMMAND | Modifiers::SHIFT, Key::O));
    (response.clicked() || chord).then(|| toggled(cam))
}

/// Where the call bar finds this frame's preview.
fn preview_id() -> egui::Id {
    egui::Id::new("huddle-camera-preview")
}

/// Uploads the newest picture of your camera, if one came, and leaves
/// it where the call bar finds it; with the camera off the texture goes.
/// Call once a frame, before the call bar is drawn.
pub fn refresh(app: &mut App, ctx: &egui::Context) {
    let preview = app
        .huddles
        .listening
        .as_ref()
        .filter(|l| l.in_huddle() && l.camera != Cam::Off)
        .and_then(|l| l.preview.clone());
    let picture = &mut app.huddles.preview;
    let Some(preview) = preview else {
        picture.texture = None;
        picture.size = [0, 0];
        ctx.data_mut(|d| d.remove::<(egui::TextureId, [usize; 2])>(preview_id()));
        return;
    };
    if let Some(image) = preview.take() {
        let size = image.size;
        let options = egui::TextureOptions::LINEAR;
        match &mut picture.texture {
            Some(texture) => texture.set(image, options),
            None => picture.texture = Some(ctx.load_texture("huddle-camera", image, options)),
        }
        picture.size = size;
    }
    let shown = picture.texture.as_ref().map(|t| (t.id(), picture.size));
    ctx.data_mut(|d| match shown {
        Some(shown) => {
            d.insert_temp(preview_id(), shown);
        }
        None => {
            d.remove::<(egui::TextureId, [usize; 2])>(preview_id());
        }
    });
}

/// The self-preview while the camera is on or opening: the picture, or a
/// dark box with a spinner until the first one comes.
pub fn preview(ui: &mut egui::Ui, palette: &Palette, cam: Cam) {
    if cam == Cam::Off {
        return;
    }
    let shown = ui
        .ctx()
        .data(|d| d.get_temp::<(egui::TextureId, [usize; 2])>(preview_id()));
    let width = ui.available_width().min(PREVIEW_WIDTH);
    let aspect = shown
        .filter(|(_, [w, h])| *w > 0 && *h > 0)
        .map_or(3.0 / 4.0, |(_, [w, h])| h as f32 / w as f32);
    let size = Vec2::new(width, (width * aspect).round());
    ui.horizontal(|ui| {
        let (rect, response) = ui.allocate_exact_size(size, Sense::hover());
        let corner = CornerRadius::same(theme::RADIUS);
        ui.painter()
            .rect_filled(rect, corner, Color32::from_gray(24));
        match shown {
            Some((texture, _)) => {
                egui::Image::new((texture, size))
                    .corner_radius(corner)
                    .paint_at(ui, rect);
            }
            None => {
                let spinner = egui::Rect::from_center_size(rect.center(), Vec2::splat(16.0));
                ui.put(
                    spinner,
                    egui::Spinner::new().size(16.0).color(palette.secondary),
                );
            }
        }
        let said = t("Your camera, as the others see it");
        theme::describe(&response, egui::WidgetType::Image, &said);
        response.on_hover_text(said);
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_button_and_the_chord_flip_the_camera() {
        assert_eq!(toggled(Cam::Off), CamAction::On);
        assert_eq!(toggled(Cam::On), CamAction::Off);
        // Opening can be called off.
        assert_eq!(toggled(Cam::Opening), CamAction::Off);
    }

    #[test]
    fn the_chord_is_on_the_sheet() {
        assert_eq!(
            super::super::shortcuts::keys_of("Turn the camera on / off"),
            Some(TOGGLE)
        );
    }
}
