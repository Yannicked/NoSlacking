//! The image viewer over the whole window: the picture fitted to it,
//! zoomed with the wheel, a pinch or + and -, moved by dragging, and
//! stepped through with ← and →. The arithmetic is in `crate::lightbox`.

use egui::{Color32, CornerRadius, Key, Modifiers, RichText, Sense, Vec2};

use crate::app::App;
use crate::i18n::{t, tf};
use crate::lightbox::{Lightbox, ZOOM_STEP};
use crate::model::Action;
use crate::theme::{self, Icon};

/// The bar across the top, with the name and the buttons.
const BAR: f32 = 52.0;
/// Room kept free around the picture.
const MARGIN: f32 = 24.0;
/// The viewer is dark in either theme, as photos are best seen.
const BACKDROP: Color32 = Color32::from_rgba_premultiplied(6, 6, 8, 246);
const FOREGROUND: Color32 = Color32::from_rgb(236, 236, 240);
const MUTED: Color32 = Color32::from_rgb(170, 170, 178);

/// What the viewer was asked to do this frame.
#[derive(Default)]
struct Asked {
    close: bool,
    step: isize,
    /// Zoom by this much around the middle of the window.
    zoom: Option<f32>,
    reset: bool,
}

pub fn show(app: &mut App, ctx: &egui::Context) {
    let Some(mut lightbox) = app.preview.take() else {
        return;
    };
    let mut asked = keys(ctx);
    let screen = ctx.content_rect();
    egui::Area::new(egui::Id::new("lightbox"))
        .order(egui::Order::Foreground)
        .fixed_pos(screen.min)
        .show(ctx, |ui| {
            ui.set_min_size(screen.size());
            ui.painter()
                .rect_filled(screen, CornerRadius::ZERO, BACKDROP);
            let area = egui::Rect::from_min_max(
                screen.min + Vec2::new(MARGIN, BAR),
                screen.max - Vec2::splat(MARGIN),
            );
            picture(ui, &mut lightbox, screen, area, &mut asked);
            bar(ui, &lightbox, screen, &mut asked, &mut app.actions);
            arrows(ui, &lightbox, screen, &mut asked);
        });
    if asked.close {
        forget(ctx, &lightbox);
        return;
    }
    if let Some(factor) = asked.zoom {
        lightbox.view.zoom_at(factor, Vec2::ZERO);
    }
    if asked.reset {
        lightbox.view = crate::lightbox::View::default();
    }
    if asked.step != 0 {
        let before = lightbox.clone();
        if lightbox.step(asked.step) {
            forget(ctx, &before);
        }
    }
    app.preview = Some(lightbox);
}

/// Frees the full-size picture shown, which can be large, once it is no
/// longer looked at. A thumbnail the message still shows is kept.
fn forget(ctx: &egui::Context, lightbox: &Lightbox) {
    if let Some(picture) = lightbox.current()
        && picture.thumb.as_deref() != Some(picture.uri.as_str())
        && picture.source.is_some()
    {
        ctx.forget_image(&picture.uri);
    }
}

/// The viewer's keys, taken before anything else sees them.
fn keys(ctx: &egui::Context) -> Asked {
    ctx.input_mut(|input| {
        let mut asked = Asked::default();
        let none = Modifiers::NONE;
        asked.close = input.consume_key(none, Key::Escape);
        if input.consume_key(none, Key::ArrowLeft) {
            asked.step -= 1;
        }
        if input.consume_key(none, Key::ArrowRight) {
            asked.step += 1;
        }
        // + often needs Shift, and = shares its key.
        let zoom_in = input.consume_key(none, Key::Plus)
            || input.consume_key(Modifiers::SHIFT, Key::Plus)
            || input.consume_key(none, Key::Equals)
            || input.consume_key(Modifiers::SHIFT, Key::Equals);
        let zoom_out = input.consume_key(none, Key::Minus);
        if zoom_in {
            asked.zoom = Some(ZOOM_STEP);
        } else if zoom_out {
            asked.zoom = Some(1.0 / ZOOM_STEP);
        }
        asked.reset = input.consume_key(none, Key::Num0);
        asked
    })
}

/// The picture, or its thumbnail while it loads, and the pointer's say
/// over zoom and position.
fn picture(
    ui: &mut egui::Ui,
    lightbox: &mut Lightbox,
    screen: egui::Rect,
    area: egui::Rect,
    asked: &mut Asked,
) {
    let Some(shown) = lightbox.current().cloned() else {
        asked.close = true;
        return;
    };
    let ctx = ui.ctx().clone();
    let load = |uri: &str| {
        egui::Image::new(uri.to_owned())
            .load_for_size(&ctx, area.size())
            .ok()
    };
    let full = egui::Image::new(shown.uri.clone()).load_for_size(&ctx, area.size());
    let thumb = shown.thumb.as_deref().and_then(load);
    let ready = |poll: &egui::load::TexturePoll| match poll {
        egui::load::TexturePoll::Ready { texture } => Some(*texture),
        egui::load::TexturePoll::Pending { .. } => None,
    };
    let full_texture = full.as_ref().ok().and_then(ready);
    let thumb_texture = thumb.as_ref().and_then(ready);
    // The picture's own size: from the file, from what has loaded, or a
    // guess, so the thumbnail fills the same place the full one will.
    let natural = shown
        .size
        .map(|[w, h]| Vec2::new(w, h))
        .or(full_texture.map(|t| t.size))
        .or(thumb_texture.map(|t| t.size))
        .unwrap_or(Vec2::new(800.0, 600.0));
    let scale = crate::lightbox::fit(natural, area.size()) * lightbox.view.zoom;
    let size = natural * scale;

    // Dragging moves the picture, the wheel or a pinch zooms it under the
    // pointer, a double click zooms in or back, and a click beside it
    // closes the viewer.
    let response = ui.interact(
        screen,
        egui::Id::new("lightbox-surface"),
        Sense::click_and_drag(),
    );
    if response.dragged() {
        lightbox.view.pan += response.drag_delta();
    }
    let (scroll, pinch, pointer) = ui.input(|input| {
        (
            input.smooth_scroll_delta.y,
            input.zoom_delta(),
            input.pointer.hover_pos(),
        )
    });
    let at = pointer.map_or(Vec2::ZERO, |p| p - area.center());
    let factor = pinch * (scroll / 240.0).exp();
    if (factor - 1.0).abs() > f32::EPSILON {
        lightbox.view.zoom_at(factor, at);
    }
    let image_rect = egui::Rect::from_center_size(area.center() + lightbox.view.pan, size);
    if response.double_clicked() {
        if lightbox.view.zoom > 1.0 {
            lightbox.view = crate::lightbox::View::default();
        } else {
            lightbox.view.zoom_at(2.0, at);
        }
    } else if response.clicked()
        && pointer.is_some_and(|p| !image_rect.contains(p) && p.y > screen.top() + BAR)
    {
        asked.close = true;
    }
    let scaled = natural * crate::lightbox::fit(natural, area.size()) * lightbox.view.zoom;
    lightbox.view.clamp_pan(scaled, area.size());
    let image_rect = egui::Rect::from_center_size(area.center() + lightbox.view.pan, scaled);
    if lightbox.view.zoom > 1.0 || scaled.x > area.width() || scaled.y > area.height() {
        let cursor = if response.dragged() {
            egui::CursorIcon::Grabbing
        } else {
            egui::CursorIcon::Grab
        };
        response.clone().on_hover_cursor(cursor);
    }

    let painter = ui.painter().with_clip_rect(screen);
    let uv = egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0));
    match (full_texture, thumb_texture) {
        (Some(texture), _) | (None, Some(texture)) => {
            painter.image(texture.id, image_rect, uv, Color32::WHITE);
        }
        (None, None) => {}
    }
    match &full {
        Ok(_) if full_texture.is_some() => {}
        Ok(_) => {
            let spinner = egui::Rect::from_center_size(image_rect.center(), Vec2::splat(28.0));
            ui.put(spinner, egui::Spinner::new().size(28.0).color(FOREGROUND));
        }
        Err(error) => {
            let text = tf(
                "Could not show this image: {error}",
                &[("error", &error.to_string())],
            );
            painter.text(
                egui::pos2(area.center().x, area.bottom() - 8.0),
                egui::Align2::CENTER_BOTTOM,
                text,
                theme::regular(13.0),
                MUTED,
            );
        }
    }
    theme::describe(&response, egui::WidgetType::Image, &shown.name);
}

/// A light-on-dark button: the palette's own would vanish on the dark
/// backdrop in the light theme.
fn tool(ui: &mut egui::Ui, icon: Icon, tip: &str) -> egui::Response {
    let (rect, response) = ui.allocate_exact_size(Vec2::splat(34.0), Sense::click());
    if response.hovered() || response.has_focus() {
        ui.painter().rect_filled(
            rect,
            CornerRadius::same(theme::RADIUS_SMALL),
            Color32::from_white_alpha(28),
        );
    }
    icon.image(FOREGROUND, 18.0).paint_at(
        ui,
        egui::Rect::from_center_size(rect.center(), Vec2::splat(18.0)),
    );
    theme::describe(&response, egui::WidgetType::Button, tip);
    response
        .on_hover_cursor(egui::CursorIcon::PointingHand)
        .on_hover_text(tip)
}

/// The name, where in the gallery it is, and the buttons.
fn bar(
    ui: &mut egui::Ui,
    lightbox: &Lightbox,
    screen: egui::Rect,
    asked: &mut Asked,
    actions: &mut Vec<Action>,
) {
    let Some(shown) = lightbox.current() else {
        return;
    };
    let inset = theme::titlebar_inset(ui.ctx());
    let rect = egui::Rect::from_min_size(
        screen.min + Vec2::new(16.0, inset),
        Vec2::new(screen.width() - 32.0, BAR - 8.0),
    );
    let mut bar = ui.new_child(
        egui::UiBuilder::new()
            .max_rect(rect)
            .layout(egui::Layout::right_to_left(egui::Align::Center)),
    );
    bar.spacing_mut().item_spacing.x = 4.0;
    if tool(&mut bar, Icon::X, &t("Close (Esc)")).clicked() {
        asked.close = true;
    }
    if let Some(link) = &shown.permalink
        && tool(&mut bar, Icon::ExternalLink, &t("Open in browser")).clicked()
    {
        actions.push(Action::OpenUrl(link.clone()));
    }
    if let Some(url) = &shown.download
        && tool(&mut bar, Icon::Download, &t("Download")).clicked()
    {
        actions.push(Action::Download {
            url: url.clone(),
            name: shown.name.clone(),
        });
    }
    bar.add_space(8.0);
    if tool(&mut bar, Icon::ZoomIn, &t("Zoom in (+)")).clicked() {
        asked.zoom = Some(ZOOM_STEP);
    }
    let percent = format!("{:.0}%", lightbox.view.zoom * 100.0);
    if bar
        .add(
            egui::Label::new(
                RichText::new(percent)
                    .font(theme::regular(12.5))
                    .color(MUTED),
            )
            .sense(Sense::click()),
        )
        .on_hover_text(t("Fit to the window (0)"))
        .clicked()
    {
        asked.reset = true;
    }
    if tool(&mut bar, Icon::ZoomOut, &t("Zoom out (-)")).clicked() {
        asked.zoom = Some(1.0 / ZOOM_STEP);
    }
    bar.add_space(8.0);
    bar.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
        ui.spacing_mut().item_spacing.x = 10.0;
        if lightbox.pictures.len() > 1 {
            ui.label(
                RichText::new(tf(
                    "{index} of {count}",
                    &[
                        ("index", &(lightbox.index + 1).to_string()),
                        ("count", &lightbox.pictures.len().to_string()),
                    ],
                ))
                .font(theme::regular(13.0))
                .color(MUTED),
            );
        }
        ui.add(
            egui::Label::new(
                RichText::new(&shown.name)
                    .font(theme::semibold(14.0))
                    .color(FOREGROUND),
            )
            .truncate(),
        );
    });
}

/// Buttons at either side for the picture before and after.
fn arrows(ui: &mut egui::Ui, lightbox: &Lightbox, screen: egui::Rect, asked: &mut Asked) {
    let y = screen.center().y + BAR / 2.0;
    let mut arrow = |x: f32, icon: Icon, tip: &str, by: isize| {
        let rect = egui::Rect::from_center_size(egui::pos2(x, y), Vec2::splat(34.0));
        let mut child = ui.new_child(egui::UiBuilder::new().max_rect(rect));
        if tool(&mut child, icon, tip).clicked() {
            asked.step += by;
        }
    };
    if lightbox.index > 0 {
        arrow(
            screen.left() + 28.0,
            Icon::ChevronLeft,
            &t("Previous image (←)"),
            -1,
        );
    }
    if lightbox.index + 1 < lightbox.pictures.len() {
        arrow(
            screen.right() - 28.0,
            Icon::ChevronRight,
            &t("Next image (→)"),
            1,
        );
    }
}
