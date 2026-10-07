//! The call window (the `huddle-video` feature): the screen share you
//! chose to watch, fitted to the window, under a bar with whose screen it
//! is, a tab for each share when two people share at once, and Close.
//! Closing it, here or with the window's own button, stops receiving the
//! share. Until the first picture arrives (a keyframe has been asked for)
//! it says so.

use egui::{Color32, CornerRadius, Margin, Rect, RichText, Sense, Stroke, Vec2};

use crate::huddles;
use crate::i18n::{t, tf};
use crate::model::Action;
use crate::theme::{self, Icon, Palette};

/// What the window shows, gathered by the app.
pub struct CallView {
    /// "Ana's screen".
    pub title: String,
    /// Every share now, by key and the sharer's name, in order.
    pub shares: Vec<(String, String)>,
    /// The key of the one shown.
    pub current: String,
    /// Its newest picture, uploaded, and the share's own size.
    pub picture: Option<(egui::TextureId, [usize; 2])>,
}

/// The window's title: "Ana's screen".
pub fn title(name: &str) -> String {
    tf("{name}'s screen", &[("name", name)])
}

/// Where a `source`-sized picture goes in `stage`: as large as fits,
/// its shape kept, centred.
pub fn fitted(stage: Rect, source: [usize; 2]) -> Rect {
    let [width, height] = source.map(|n| n as f32);
    if width <= 0.0 || height <= 0.0 || stage.width() <= 0.0 || stage.height() <= 0.0 {
        return Rect::from_center_size(stage.center(), Vec2::ZERO);
    }
    let scale = (stage.width() / width).min(stage.height() / height);
    Rect::from_center_size(stage.center(), Vec2::new(width, height) * scale)
}

/// Draws the window; gives the size, in pixels, the share is shown at,
/// for the decoder to convert no more than that.
pub fn show(
    ui: &mut egui::Ui,
    palette: &Palette,
    view: &CallView,
    actions: &mut Vec<Action>,
) -> [usize; 2] {
    let inset = theme::titlebar_inset(ui.ctx());
    egui::Panel::top("call-window-header")
        .exact_size(48.0 + inset)
        .show_separator_line(false)
        .frame(egui::Frame::new().fill(palette.panel).inner_margin(Margin {
            left: 14,
            right: 8,
            top: inset as i8,
            bottom: 0,
        }))
        .show(ui, |ui| {
            let rect = ui.max_rect();
            ui.painter().hline(
                rect.x_range(),
                rect.bottom() - 0.5,
                Stroke::new(1.0, palette.outline),
            );
            ui.horizontal_centered(|ui| header(ui, palette, view, actions));
        });
    let mut shown = [0, 0];
    // The stage is dark in both themes: a picture reads best on black.
    let stage = if palette.dark {
        Color32::from_rgb(0x0f, 0x11, 0x14)
    } else {
        Color32::from_rgb(0x1d, 0x1f, 0x23)
    };
    egui::CentralPanel::default()
        .frame(egui::Frame::new().fill(stage).inner_margin(Margin::same(8)))
        .show(ui, |ui| {
            let (area, _) = ui.allocate_exact_size(ui.available_size(), Sense::hover());
            let pixels = ui.ctx().pixels_per_point();
            match view.picture {
                Some((texture, source)) => {
                    let rect = fitted(area, source);
                    ui.painter().image(
                        texture,
                        rect,
                        Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
                        Color32::WHITE,
                    );
                    let response = ui.interact(rect, ui.id().with("share"), Sense::hover());
                    theme::describe(&response, egui::WidgetType::Image, &view.title);
                    shown = [rect.width(), rect.height()].map(|n| (n * pixels).round() as usize);
                }
                None => {
                    waiting(ui, area);
                    shown = [area.width(), area.height()].map(|n| (n * pixels).round() as usize);
                }
            }
        });
    shown
}

/// The bar: whose screen, the tabs, Close.
fn header(ui: &mut egui::Ui, palette: &Palette, view: &CallView, actions: &mut Vec<Action>) {
    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
        if theme::icon_button(ui, palette, Icon::X, 16.0, &t("Close")).clicked() {
            actions.push(Action::Huddle(huddles::Action::Watch(None)));
        }
        ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
            ui.spacing_mut().item_spacing.x = 8.0;
            ui.add(Icon::Monitor.image(super::people::ACTIVE, 16.0));
            ui.add(
                egui::Label::new(
                    RichText::new(&view.title)
                        .font(theme::semibold(14.5))
                        .color(palette.text),
                )
                .truncate(),
            );
            if view.shares.len() > 1 {
                ui.add_space(8.0);
                for (key, name) in &view.shares {
                    if tab(ui, palette, name, *key == view.current).clicked()
                        && *key != view.current
                    {
                        actions.push(Action::Huddle(huddles::Action::Watch(Some(key.clone()))));
                    }
                }
            }
        });
    });
}

/// A tab for one share, lit when it is the one shown.
fn tab(ui: &mut egui::Ui, palette: &Palette, name: &str, current: bool) -> egui::Response {
    let (fill, text) = if current {
        (palette.surface_active, palette.text)
    } else {
        (Color32::TRANSPARENT, palette.secondary)
    };
    let response = ui
        .add(
            egui::Button::new(RichText::new(name).font(theme::medium(13.0)).color(text))
                .fill(fill)
                .stroke(Stroke::new(1.0, palette.outline))
                .corner_radius(CornerRadius::same(theme::RADIUS_SMALL + 2))
                .min_size(Vec2::new(0.0, 26.0)),
        )
        .on_hover_cursor(egui::CursorIcon::PointingHand);
    let said = if current {
        tf("{name}'s screen, shown", &[("name", name)])
    } else {
        tf("Show {name}'s screen", &[("name", name)])
    };
    theme::describe(&response, egui::WidgetType::Button, &said);
    response
}

/// No picture yet: a spinner and why.
fn waiting(ui: &mut egui::Ui, area: Rect) {
    let center = area.center();
    egui::Spinner::new()
        .size(22.0)
        .color(Color32::from_gray(0xc8))
        .paint_at(
            ui,
            Rect::from_center_size(center - Vec2::new(0.0, 18.0), Vec2::splat(22.0)),
        );
    ui.painter().text(
        center + Vec2::new(0.0, 14.0),
        egui::Align2::CENTER_CENTER,
        t("Waiting for the picture…"),
        theme::regular(13.0),
        Color32::from_gray(0xc8),
    );
    ui.ctx()
        .request_repaint_after(std::time::Duration::from_millis(100));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_share_fits_the_window_keeping_its_shape() {
        let stage = Rect::from_min_size(egui::pos2(0.0, 0.0), Vec2::new(1000.0, 1000.0));
        let rect = fitted(stage, [1920, 1080]);
        assert_eq!(rect.width(), 1000.0);
        assert!((rect.height() - 562.5).abs() < 0.01);
        assert_eq!(rect.center(), stage.center());
        let tall = fitted(stage, [480, 960]);
        assert_eq!((tall.width(), tall.height()), (500.0, 1000.0));
        assert_eq!(fitted(stage, [0, 0]).size(), Vec2::ZERO);
        assert_eq!(title("Ana"), "Ana's screen");
    }
}
