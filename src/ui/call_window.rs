//! The call window (the `huddle-video` feature): the screen share you
//! chose to watch, fitted to the window, and a tile for each camera the
//! session receives, under a bar with whose screen it is, a tab for each
//! share when two people share at once, and Close.
//!
//! Without a share the tiles fill the window in a grid. With one, the
//! share is large and the tiles go beside it (a wide window) or below it
//! (a tall one). Each tile shows the person's picture filling it, or
//! their face or initials while their camera is paused or its first
//! picture has not come; their name, a mark when muted and a ring while
//! they speak, as the call bar's faces have. The tiles come from a list,
//! so your own preview is one more entry once there is one.
//!
//! The window works out how many tiles it has room for and how large,
//! and the app tells the session, which receives no more than that.
//! Closing it, here or with the window's own button, stops receiving
//! everything.

use egui::{Color32, CornerRadius, Margin, Rect, RichText, Sense, Stroke, Vec2};

use crate::huddles;
use crate::i18n::{t, tf, tn};
use crate::model::Action;
use crate::theme::{self, Icon, Palette};

/// A tile's width over its height: pictures are cropped to fill it.
pub const TILE_ASPECT: f32 = 4.0 / 3.0;
/// The narrowest a tile gets, in points: the room for tiles is counted
/// at this.
pub const MIN_TILE: f32 = 200.0;
/// Between tiles, and between the share and the tiles.
pub const GAP: f32 = 8.0;
/// The tile size the session hears is rounded up to this many pixels,
/// so resizing the window does not tell it of every pixel.
const TILE_STEP: u32 = 32;

/// One camera tile as the window draws it.
#[derive(Clone, Debug, PartialEq)]
pub struct TileView {
    /// Whose camera, by its key.
    pub key: String,
    /// Their name.
    pub name: String,
    /// Their avatar, shown while there is no picture.
    pub avatar: Option<String>,
    /// For the initials' colour.
    pub seed: String,
    /// Speaking now: a ring.
    pub speaking: bool,
    /// Their microphone is off: a mark.
    pub muted: bool,
    /// Their camera is paused: their face instead.
    pub paused: bool,
    /// The newest picture, uploaded, and the camera's own size.
    pub picture: Option<(egui::TextureId, [usize; 2])>,
}

/// What the window shows, gathered by the app.
pub struct CallView {
    /// "Ana's screen", or "Huddle video" without a share.
    pub title: String,
    /// Every share now, by key and the sharer's name, in order.
    pub shares: Vec<(String, String)>,
    /// The key of the share shown, if one is.
    pub current: Option<String>,
    /// Its newest picture, uploaded, and the share's own size.
    pub picture: Option<(egui::TextureId, [usize; 2])>,
    /// The camera tiles, in order.
    pub tiles: Vec<TileView>,
    /// Cameras on that have no tile, for want of room.
    pub more: usize,
}

/// What drawing the window found out, for the decoders and the session.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Shown {
    /// The size, in pixels, the share is shown at.
    pub share: [usize; 2],
    /// How many tiles there is room for.
    pub room: usize,
    /// A tile's size, in pixels.
    pub tile: [u32; 2],
}

/// The window's title: "Ana's screen".
pub fn title(name: &str) -> String {
    tf("{name}'s screen", &[("name", name)])
}

/// The window's title without a share.
pub fn cameras_title() -> String {
    t("Huddle video").into_owned()
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

/// The part of a `source`-sized picture that fills a `tile`, its shape
/// kept: the middle, cut at the sides or at the top and bottom, as
/// texture coordinates.
pub fn cover(tile: Vec2, source: [usize; 2]) -> Rect {
    let [width, height] = source.map(|n| n as f32);
    let whole = Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0));
    if width <= 0.0 || height <= 0.0 || tile.x <= 0.0 || tile.y <= 0.0 {
        return whole;
    }
    let scale = (tile.x / width).max(tile.y / height);
    let shown = Vec2::new(tile.x / (width * scale), tile.y / (height * scale));
    Rect::from_center_size(egui::pos2(0.5, 0.5), shown)
}

/// The best grid for `count` tiles in `area`: how many columns and rows,
/// and the size of a tile, as large as they can be.
pub fn grid(count: usize, area: Vec2) -> (usize, usize, Vec2) {
    let mut best = (1, count.max(1), Vec2::ZERO);
    for columns in 1..=count.max(1) {
        let rows = count.max(1).div_ceil(columns);
        let width = (area.x - GAP * (columns - 1) as f32) / columns as f32;
        let height = (area.y - GAP * (rows - 1) as f32) / rows as f32;
        let width = width.min(height * TILE_ASPECT).max(0.0);
        if width > best.2.x {
            best = (columns, rows, Vec2::new(width, width / TILE_ASPECT));
        }
    }
    best
}

/// How many tiles fit in `area` at least [`MIN_TILE`] wide, from one (a
/// small window still shows the person speaking) to
/// [`huddles::MAX_TILES`].
pub fn capacity(area: Vec2) -> usize {
    (1..=huddles::MAX_TILES)
        .rev()
        .find(|&count| grid(count, area).2.x >= MIN_TILE)
        .unwrap_or(1)
}

/// Where everything goes in the stage.
#[derive(Clone, Debug, PartialEq)]
pub struct Layout {
    /// The share's area, if one is shown.
    pub share: Option<Rect>,
    /// Each tile.
    pub tiles: Vec<Rect>,
    /// How many tiles there is room for.
    pub room: usize,
}

/// Lays out the stage for a share (or none) and `tiles` tiles: without a
/// share a grid over all of it; with one, the tiles in a column at the
/// side of a wide stage or a row under a tall one, the share in the
/// rest; a share alone has all of it.
pub fn layout(stage: Rect, share: bool, tiles: usize) -> Layout {
    let area = if !share {
        stage
    } else if stage.width() >= stage.height() * 1.2 {
        let width = (stage.width() * 0.24)
            .clamp(MIN_TILE, 360.0)
            .min(stage.width());
        Rect::from_min_max(egui::pos2(stage.right() - width, stage.top()), stage.max)
    } else {
        let height = (stage.height() * 0.26)
            .clamp(MIN_TILE / TILE_ASPECT, 280.0)
            .min(stage.height());
        Rect::from_min_max(egui::pos2(stage.left(), stage.bottom() - height), stage.max)
    };
    let room = capacity(area.size());
    let shown = tiles.min(room);
    let share = share.then(|| {
        if shown == 0 {
            stage
        } else if area.top() > stage.top() {
            Rect::from_min_max(stage.min, egui::pos2(stage.right(), area.top() - GAP))
        } else {
            Rect::from_min_max(stage.min, egui::pos2(area.left() - GAP, stage.bottom()))
        }
    });
    let mut rects = Vec::with_capacity(shown);
    if shown > 0 {
        let (columns, rows, size) = grid(shown, area.size());
        let height = rows as f32 * size.y + (rows - 1) as f32 * GAP;
        let top = area.center().y - height / 2.0;
        for row in 0..rows {
            // The last row may be short: centred.
            let in_row = (shown - row * columns).min(columns);
            let width = in_row as f32 * size.x + (in_row - 1) as f32 * GAP;
            let left = area.center().x - width / 2.0;
            for column in 0..in_row {
                let min = egui::pos2(
                    left + column as f32 * (size.x + GAP),
                    top + row as f32 * (size.y + GAP),
                );
                rects.push(Rect::from_min_size(min, size));
            }
        }
    }
    Layout {
        share,
        tiles: rects,
        room,
    }
}

/// A tile's size in pixels as the session hears it: rounded up to
/// [`TILE_STEP`].
pub fn tile_pixels(size: Vec2, pixels_per_point: f32) -> [u32; 2] {
    let step = |n: f32| {
        let n = (n * pixels_per_point).max(0.0).ceil() as u32;
        n.div_ceil(TILE_STEP) * TILE_STEP
    };
    [step(size.x), step(size.y)]
}

/// Draws the window; says the size the share is shown at and the room
/// for tiles, for the decoders and the session.
pub fn show(
    ui: &mut egui::Ui,
    palette: &Palette,
    view: &CallView,
    actions: &mut Vec<Action>,
) -> Shown {
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
    let mut shown = Shown::default();
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
            let layout = layout(area, view.current.is_some(), view.tiles.len());
            shown.room = layout.room;
            let tile = layout
                .tiles
                .first()
                .map_or_else(|| grid(layout.room, area.size()).2, |r| r.size());
            shown.tile = tile_pixels(tile, pixels);
            if let Some(share) = layout.share {
                shown.share = share_stage(ui, view, share, pixels);
            }
            for (rect, view) in layout.tiles.iter().zip(&view.tiles) {
                camera_tile(ui, palette, *rect, view);
            }
            if layout.share.is_none() && view.tiles.is_empty() {
                note(ui, area, &t("No one has their camera on"));
            }
        });
    shown
}

/// The share in its part of the stage; gives the size it is shown at.
fn share_stage(ui: &mut egui::Ui, view: &CallView, area: Rect, pixels: f32) -> [usize; 2] {
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
            [rect.width(), rect.height()].map(|n| (n * pixels).round() as usize)
        }
        None => {
            waiting(ui, area);
            [area.width(), area.height()].map(|n| (n * pixels).round() as usize)
        }
    }
}

/// One camera: the picture filling the tile, or the person's face; their
/// name, muted mark and speaking ring.
fn camera_tile(ui: &mut egui::Ui, palette: &Palette, rect: Rect, tile: &TileView) {
    let radius = CornerRadius::same(theme::RADIUS + 2);
    ui.painter()
        .rect_filled(rect, radius, Color32::from_rgb(0x2a, 0x2d, 0x33));
    match tile.picture.filter(|_| !tile.paused) {
        Some((texture, source)) => {
            egui::Image::new((texture, rect.size()))
                .uv(cover(rect.size(), source))
                .corner_radius(radius)
                .paint_at(ui, rect);
        }
        None => {
            let side = (rect.height() * 0.42).clamp(24.0, 112.0);
            let face =
                Rect::from_center_size(rect.center() - Vec2::new(0.0, 8.0), Vec2::splat(side));
            super::paint_avatar(ui, face, tile.avatar.as_deref(), &tile.name, &tile.seed);
            if !tile.paused {
                // The first picture is on its way.
                egui::Spinner::new()
                    .size(14.0)
                    .color(Color32::from_gray(0xc8))
                    .paint_at(
                        ui,
                        Rect::from_center_size(
                            rect.right_top() + Vec2::new(-16.0, 16.0),
                            Vec2::splat(14.0),
                        ),
                    );
                ui.ctx()
                    .request_repaint_after(std::time::Duration::from_millis(100));
            }
        }
    }
    name_plate(ui, palette, rect, tile);
    if tile.speaking {
        ui.painter().rect_stroke(
            rect,
            radius,
            Stroke::new(3.0, super::people::ACTIVE),
            egui::StrokeKind::Inside,
        );
    }
    let mut said = tile.name.clone();
    if tile.paused {
        said = tf("{name}, camera paused", &[("name", &said)]);
    }
    if tile.speaking {
        said = tf("{name}, speaking", &[("name", &said)]);
    } else if tile.muted {
        said = tf("{name}, muted", &[("name", &said)]);
    }
    let response = ui.interact(rect, ui.id().with(("tile", &tile.key)), Sense::hover());
    theme::describe(&response, egui::WidgetType::Image, &said);
    response.on_hover_text(said);
}

/// The name at a tile's foot, on a dark plate, with the muted mark.
fn name_plate(ui: &egui::Ui, palette: &Palette, rect: Rect, tile: &TileView) {
    let font = theme::medium(12.5);
    let mut text = tile.name.clone();
    if tile.paused {
        text = tf("{name} · camera paused", &[("name", &text)]);
    }
    let room = (rect.width() - 16.0 - if tile.muted { 18.0 } else { 0.0 }).max(10.0);
    let galley = ui.painter().layout(text, font, Color32::WHITE, room);
    // One line: cut what does not fit.
    let line = galley.rows.first().map_or(galley.size(), |r| r.size);
    let width = line.x.min(room) + 12.0 + if tile.muted { 18.0 } else { 0.0 };
    let plate = Rect::from_min_size(
        rect.left_bottom() + Vec2::new(6.0, -6.0 - (line.y + 6.0)),
        Vec2::new(width, line.y + 6.0),
    );
    ui.painter().rect_filled(
        plate,
        CornerRadius::same(theme::RADIUS_SMALL + 1),
        Color32::from_black_alpha(0xa0),
    );
    let mut x = plate.left() + 6.0;
    if tile.muted {
        Icon::MicOff.image(Color32::from_gray(0xe0), 12.0).paint_at(
            ui,
            Rect::from_center_size(egui::pos2(x + 6.0, plate.center().y), Vec2::splat(12.0)),
        );
        x += 18.0;
    }
    let clip = Rect::from_min_max(
        egui::pos2(x, plate.top()),
        egui::pos2(plate.right() - 6.0, plate.bottom()),
    );
    ui.painter().with_clip_rect(clip).galley(
        egui::pos2(x, plate.top() + 3.0),
        galley,
        Color32::WHITE,
    );
    let _ = palette;
}

/// The bar: whose screen, the tabs, how many cameras have no tile, Close.
fn header(ui: &mut egui::Ui, palette: &Palette, view: &CallView, actions: &mut Vec<Action>) {
    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
        if theme::icon_button(ui, palette, Icon::X, 16.0, &t("Close")).clicked() {
            actions.push(Action::Huddle(huddles::Action::Watch(None)));
        }
        if view.more > 0 {
            ui.add(egui::Label::new(
                RichText::new(tn(
                    "{count} more camera",
                    "{count} more cameras",
                    u32::try_from(view.more).unwrap_or(u32::MAX),
                ))
                .font(theme::regular(12.5))
                .color(palette.secondary),
            ));
        }
        ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
            ui.spacing_mut().item_spacing.x = 8.0;
            let icon = if view.current.is_some() {
                Icon::Monitor
            } else {
                Icon::Users
            };
            ui.add(icon.image(super::people::ACTIVE, 16.0));
            ui.add(
                egui::Label::new(
                    RichText::new(&view.title)
                        .font(theme::semibold(14.5))
                        .color(palette.text),
                )
                .truncate(),
            );
            if view.shares.len() > 1 || (view.current.is_none() && !view.shares.is_empty()) {
                ui.add_space(8.0);
                for (key, name) in &view.shares {
                    let current = view.current.as_deref() == Some(key.as_str());
                    if tab(ui, palette, name, current).clicked() && !current {
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
    note(
        ui,
        Rect::from_center_size(center + Vec2::new(0.0, 14.0), Vec2::ZERO),
        &t("Waiting for the picture…"),
    );
    ui.ctx()
        .request_repaint_after(std::time::Duration::from_millis(100));
}

/// A line of grey text in the middle of `area`.
fn note(ui: &egui::Ui, area: Rect, text: &str) {
    ui.painter().text(
        area.center(),
        egui::Align2::CENTER_CENTER,
        text,
        theme::regular(13.0),
        Color32::from_gray(0xc8),
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rect(width: f32, height: f32) -> Rect {
        Rect::from_min_size(egui::pos2(0.0, 0.0), Vec2::new(width, height))
    }

    #[test]
    fn the_share_fits_the_window_keeping_its_shape() {
        let stage = rect(1000.0, 1000.0);
        let rect = fitted(stage, [1920, 1080]);
        assert_eq!(rect.width(), 1000.0);
        assert!((rect.height() - 562.5).abs() < 0.01);
        assert_eq!(rect.center(), stage.center());
        let tall = fitted(stage, [480, 960]);
        assert_eq!((tall.width(), tall.height()), (500.0, 1000.0));
        assert_eq!(fitted(stage, [0, 0]).size(), Vec2::ZERO);
        assert_eq!(title("Ana"), "Ana's screen");
    }

    #[test]
    fn a_camera_fills_its_tile_cut_in_the_middle() {
        // A square camera in a 4:3 tile: the top and bottom go.
        let uv = cover(Vec2::new(400.0, 300.0), [480, 480]);
        assert!((uv.width() - 1.0).abs() < 1e-5);
        assert!((uv.height() - 0.75).abs() < 1e-5);
        assert_eq!(uv.center(), egui::pos2(0.5, 0.5));
        // A wide one in the same tile: the sides go.
        let uv = cover(Vec2::new(400.0, 300.0), [1280, 720]);
        assert!((uv.height() - 1.0).abs() < 1e-5);
        assert!((uv.width() - 0.75).abs() < 1e-3, "{uv:?}");
        // Unknown sizes show all of it.
        assert_eq!(
            cover(Vec2::new(400.0, 300.0), [0, 0]).size(),
            Vec2::splat(1.0)
        );
    }

    #[test]
    fn grids_make_tiles_as_large_as_they_can() {
        // One tile in a wide area: as tall as it is.
        let (columns, rows, size) = grid(1, Vec2::new(1200.0, 600.0));
        assert_eq!((columns, rows), (1, 1));
        assert_eq!(size, Vec2::new(800.0, 600.0));
        // Four in a wide area: two by two beats four in a row.
        let (columns, rows, _) = grid(4, Vec2::new(1200.0, 800.0));
        assert_eq!((columns, rows), (2, 2));
        // Four in a narrow column: one above another.
        let (columns, rows, size) = grid(4, Vec2::new(200.0, 1000.0));
        assert_eq!((columns, rows), (1, 4));
        assert_eq!(size.x, 200.0);
        // Nine in a big window: three by three.
        assert_eq!(grid(9, Vec2::new(1264.0, 732.0)).0, 3);
    }

    #[test]
    fn the_room_for_tiles_follows_the_window() {
        assert_eq!(
            capacity(Vec2::new(1264.0, 732.0)),
            9,
            "a large window: nine"
        );
        assert_eq!(capacity(Vec2::new(640.0, 360.0)), 6);
        assert_eq!(capacity(Vec2::new(320.0, 240.0)), 1);
        assert_eq!(capacity(Vec2::new(10.0, 10.0)), 1, "always the speaker");
        assert_eq!(capacity(Vec2::new(5000.0, 3000.0)), huddles::MAX_TILES);
    }

    #[test]
    fn tiles_go_beside_a_share_in_a_wide_window_and_below_in_a_tall_one() {
        let wide = layout(rect(1264.0, 732.0), true, 3);
        let share = wide.share.expect("a share");
        assert_eq!(wide.tiles.len(), 3);
        assert!(wide.tiles.iter().all(|t| t.left() > share.right()));
        assert!(share.width() > 900.0);
        let tall = layout(rect(700.0, 1000.0), true, 2);
        let share = tall.share.expect("a share");
        assert!(tall.tiles.iter().all(|t| t.top() > share.bottom()));
        // A share alone has the whole stage; tiles alone too.
        assert_eq!(
            layout(rect(800.0, 600.0), true, 0).share,
            Some(rect(800.0, 600.0))
        );
        let alone = layout(rect(1264.0, 732.0), false, 5);
        assert_eq!(alone.share, None);
        assert_eq!(alone.tiles.len(), 5);
        // The tiles never overlap and stay inside.
        for (i, a) in alone.tiles.iter().enumerate() {
            assert!(rect(1264.0, 732.0).contains_rect(*a), "{a:?}");
            for b in &alone.tiles[i + 1..] {
                assert!(!a.intersects(b.shrink(0.5)), "{a:?} {b:?}");
            }
        }
        // The short last row is centred.
        let (first, last) = (alone.tiles[0], alone.tiles[4]);
        assert!(last.center().x > first.center().x);
        // More cameras than room: only as many tiles as fit.
        let small = layout(rect(320.0, 240.0), false, 6);
        assert_eq!((small.room, small.tiles.len()), (1, 1));
    }

    #[test]
    fn the_session_hears_tile_sizes_in_steps() {
        assert_eq!(tile_pixels(Vec2::new(200.0, 150.0), 1.0), [224, 160]);
        assert_eq!(tile_pixels(Vec2::new(200.0, 150.0), 2.0), [416, 320]);
        assert_eq!(tile_pixels(Vec2::new(201.0, 150.0), 2.0), [416, 320]);
        assert_eq!(tile_pixels(Vec2::ZERO, 2.0), [0, 0]);
    }
}
