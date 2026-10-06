//! A message's files: pictures and videos inline (or behind a
//! placeholder until asked for), and a card for anything else.

use egui::{CornerRadius, Margin, RichText, Sense, Stroke, Vec2};

use super::{PLACEHOLDER, Row};
use crate::i18n::{t, tf};
use crate::model::{Action, File, Media, Message};
use crate::theme::{self, Icon};

pub(super) fn file_view(
    ui: &mut egui::Ui,
    row: &Row<'_>,
    message: &Message,
    file: &File,
    actions: &mut Vec<Action>,
) {
    let palette = row.palette;
    let team = &row.workspace.info.team_id;
    // Deleted, or being deleted: Slack's own words in its place.
    if file.deleted || !row.workspace.shows_file(&file.id) {
        ui.label(
            RichText::new(t("This file was deleted."))
                .font(theme::regular(13.0))
                .italics()
                .color(palette.dim),
        );
        return;
    }
    let deletable = file.deletable_by(&row.workspace.info.user_id);
    if file.is_image()
        && let Some(thumb) = &file.thumb
    {
        let size = thumb_size(file, ui.available_width());
        let uri = crate::ui::image_uri(team, thumb);
        if !shows(ui, row, &uri) {
            placeholder(ui, row, &uri, &file.name);
            return;
        }
        let response = crate::ui::picture(
            ui,
            uri.clone(),
            size,
            CornerRadius::same(theme::RADIUS),
            Sense::click(),
        )
        .on_hover_cursor(egui::CursorIcon::ZoomIn)
        .on_hover_text(&file.name);
        // In the thread panel the viewer steps through the thread's
        // pictures; a parent is its own thread.
        let thread = row.in_thread.then(|| {
            message
                .thread_ts
                .clone()
                .unwrap_or_else(|| message.ts.clone())
        });
        if response.hovered() {
            crate::ui::context::hover(
                ui,
                crate::ui::context::Target::Image {
                    channel: row.channel.to_owned(),
                    thread: thread.clone(),
                    ts: message.ts.clone(),
                    file: file.id.clone(),
                    name: file.name.clone(),
                    download: file
                        .url_private
                        .clone()
                        .or_else(|| file.download_url.clone()),
                    permalink: file.permalink.clone(),
                    copy: file
                        .url_private
                        .iter()
                        .map(|full| crate::ui::image_uri(team, full))
                        .chain([uri.clone()])
                        .collect(),
                    deletable,
                },
            );
        }
        if response.clicked() {
            actions.push(Action::ViewImage {
                channel: row.channel.to_owned(),
                thread,
                ts: message.ts.clone(),
                file: file.id.clone(),
            });
        }
        return;
    }
    // What plays opens in the system's player, from a copy in the cache.
    let play = file
        .url_private
        .clone()
        .or_else(|| file.download_url.clone());
    let poster = file
        .poster
        .as_deref()
        .map(|poster| crate::ui::image_uri(team, poster));
    if let Some(uri) = &poster
        && !shows(ui, row, uri)
    {
        placeholder(ui, row, uri, &file.name);
    } else if let Some(uri) = poster {
        let size = poster_size(file, ui.available_width());
        let response = crate::ui::picture(
            ui,
            uri,
            size,
            CornerRadius::same(theme::RADIUS),
            Sense::click(),
        )
        .on_hover_cursor(egui::CursorIcon::PointingHand);
        let tip = if file.media().is_some() {
            tf("Play {name}", &[("name", &file.name)])
        } else {
            tf("Open {name}", &[("name", &file.name)])
        };
        if file.media() == Some(Media::Video) {
            play_badge(ui, response.rect.center(), response.hovered());
        }
        if response.hovered() {
            hover_file(ui, file, deletable);
        }
        theme::describe(&response, egui::WidgetType::Button, &tip);
        if response.on_hover_text(&tip).clicked()
            && let Some(url) = play.clone()
        {
            actions.push(Action::OpenFile {
                url,
                name: file.name.clone(),
            });
        }
    }
    if file.media().is_some() {
        let card = media_card(ui, row, file, play, actions);
        if ui.rect_contains_pointer(card) {
            hover_file(ui, file, deletable);
        }
        return;
    }
    let response = egui::Frame::new()
        .fill(palette.surface)
        .stroke(Stroke::new(1.0, palette.outline))
        .corner_radius(CornerRadius::same(theme::RADIUS))
        .inner_margin(Margin::same(10))
        .show(ui, |ui| {
            ui.set_max_width(360.0);
            ui.horizontal(|ui| {
                let (rect, _) = ui.allocate_exact_size(Vec2::splat(36.0), Sense::hover());
                ui.painter().rect_filled(
                    rect,
                    CornerRadius::same(6),
                    palette.accent.gamma_multiply(0.2),
                );
                Icon::FileText.image(palette.accent, 20.0).paint_at(
                    ui,
                    egui::Rect::from_center_size(rect.center(), Vec2::splat(20.0)),
                );
                ui.vertical(|ui| {
                    ui.spacing_mut().item_spacing.y = 1.0;
                    ui.add(
                        egui::Label::new(
                            RichText::new(&file.name)
                                .font(theme::semibold(14.0))
                                .color(palette.text),
                        )
                        .truncate(),
                    );
                    ui.label(
                        RichText::new(file_detail(file))
                            .font(theme::regular(12.0))
                            .color(palette.secondary),
                    );
                });
            });
        })
        .response
        .interact(Sense::click())
        .on_hover_cursor(egui::CursorIcon::PointingHand)
        .on_hover_text(t("Download"));
    theme::focus_ring(ui, &response, palette, theme::RADIUS);
    theme::describe(
        &response,
        egui::WidgetType::Button,
        &tf("Download {name}", &[("name", &file.name)]),
    );
    if response.hovered() {
        hover_file(ui, file, deletable);
    }
    if response.clicked()
        && let Some(url) = file
            .download_url
            .clone()
            .or_else(|| file.url_private.clone())
    {
        actions.push(Action::Download {
            url,
            name: file.name.clone(),
        });
    }
}

/// Tells the message's right-click menu that `file` is under the pointer.
fn hover_file(ui: &egui::Ui, file: &File, deletable: bool) {
    crate::ui::context::hover(
        ui,
        crate::ui::context::Target::File {
            file: file.id.clone(),
            name: file.name.clone(),
            download: file
                .download_url
                .clone()
                .or_else(|| file.url_private.clone()),
            deletable,
        },
    );
}

/// Where it is remembered that a held-back picture was asked for.
fn reveal_id(uri: &str) -> egui::Id {
    egui::Id::new(("show-picture", uri))
}

/// Whether the picture at `uri` shows: always, unless pictures are held
/// back and this one has not been clicked yet.
pub(super) fn shows(ui: &egui::Ui, row: &Row<'_>, uri: &str) -> bool {
    row.look.inline_media || ui.data(|d| d.get_temp::<bool>(reveal_id(uri)).unwrap_or(false))
}

/// A short bar standing in for a held-back picture; clicking it shows the
/// picture (and only then is it fetched).
pub(super) fn placeholder(ui: &mut egui::Ui, row: &Row<'_>, uri: &str, name: &str) {
    let palette = row.palette;
    let width = ui.available_width().min(360.0);
    let (rect, response) = ui.allocate_exact_size(Vec2::new(width, PLACEHOLDER), Sense::click());
    let fill = if response.hovered() {
        palette.surface_hover
    } else {
        palette.surface
    };
    ui.painter()
        .rect_filled(rect, CornerRadius::same(theme::RADIUS), fill);
    ui.painter().rect_stroke(
        rect,
        CornerRadius::same(theme::RADIUS),
        Stroke::new(1.0, palette.outline),
        egui::StrokeKind::Inside,
    );
    let icon = egui::Rect::from_center_size(
        egui::pos2(rect.left() + 18.0, rect.center().y),
        Vec2::splat(15.0),
    );
    Icon::Image
        .image(palette.secondary, 15.0)
        .paint_at(ui, icon);
    let label = if name.trim().is_empty() {
        t("Show the picture").into_owned()
    } else {
        tf("Show the picture: {name}", &[("name", name)])
    };
    let galley =
        ui.painter()
            .layout_no_wrap(label.clone(), theme::regular(13.0), palette.secondary);
    // One line: a long name is cut at the bar's end.
    ui.painter().with_clip_rect(rect.shrink(6.0)).galley(
        egui::pos2(rect.left() + 34.0, rect.center().y - galley.size().y / 2.0),
        galley,
        palette.secondary,
    );
    theme::focus_ring(ui, &response, palette, theme::RADIUS);
    theme::describe(&response, egui::WidgetType::Button, &label);
    if response
        .on_hover_cursor(egui::CursorIcon::PointingHand)
        .clicked()
    {
        ui.data_mut(|d| d.insert_temp(reveal_id(uri), true));
    }
}

/// `size` scaled down to fit `max`, never up, and never smaller than a
/// speck; `fallback` when the size is not known.
pub(super) fn fit_within(size: Option<[f32; 2]>, max: Vec2, fallback: Vec2) -> Vec2 {
    let [w, h] = size
        .filter(|[w, h]| *w > 0.0 && *h > 0.0)
        .unwrap_or([fallback.x, fallback.y]);
    let scale = (max.x / w).min(max.y / h).min(1.0);
    Vec2::new(w * scale, h * scale).max(Vec2::splat(24.0))
}

/// How large a picture file is shown, in a column `width` wide: the size
/// of the thumbnail Slack gave, shrunk to fit, or a fixed box when Slack
/// gave none. The same before it has loaded as after.
pub(super) fn thumb_size(file: &File, width: f32) -> Vec2 {
    fit_within(
        file.thumb_size,
        Vec2::new(width.min(420.0), 320.0),
        Vec2::new(360.0, 240.0),
    )
}

/// How large a video's or PDF's still is shown, in a column `width` wide.
/// A page is shown smaller than a frame: it is there to recognise the
/// document, not to read it.
pub(super) fn poster_size(file: &File, width: f32) -> Vec2 {
    if file.media().is_some() {
        fit_within(
            file.poster_size,
            Vec2::new(width.min(400.0), 260.0),
            Vec2::new(400.0, 225.0),
        )
    } else {
        fit_within(
            file.poster_size,
            Vec2::new(width.min(240.0), 300.0),
            Vec2::new(212.0, 300.0),
        )
    }
}

/// A round play button painted over a picture, centred on `center`.
pub(super) fn play_badge(ui: &egui::Ui, center: egui::Pos2, hovered: bool) {
    let radius = 24.0;
    let fill = egui::Color32::from_black_alpha(if hovered { 200 } else { 150 });
    let painter = ui.painter();
    painter.circle_filled(center, radius, fill);
    // A triangle, nudged right so it looks centred.
    let c = center + Vec2::new(3.0, 0.0);
    painter.add(egui::Shape::convex_polygon(
        vec![
            c + Vec2::new(-8.0, -11.0),
            c + Vec2::new(11.0, 0.0),
            c + Vec2::new(-8.0, 11.0),
        ],
        egui::Color32::WHITE,
        Stroke::NONE,
    ));
}

/// "1.2 MB · MP4": a file's size and kind.
fn file_detail(file: &File) -> String {
    let kind = file
        .mimetype
        .split('/')
        .next_back()
        .unwrap_or("")
        .to_uppercase();
    format!("{} · {kind}", crate::ui::file_size(file.size))
}

/// A video or sound: a play button that opens it in the system's player,
/// its name, and a download button. Returns where the card is.
fn media_card(
    ui: &mut egui::Ui,
    row: &Row<'_>,
    file: &File,
    play: Option<String>,
    actions: &mut Vec<Action>,
) -> egui::Rect {
    let palette = row.palette;
    egui::Frame::new()
        .fill(palette.surface)
        .stroke(Stroke::new(1.0, palette.outline))
        .corner_radius(CornerRadius::same(theme::RADIUS))
        .inner_margin(Margin::same(10))
        .show(ui, |ui| {
            ui.set_max_width(360.0);
            ui.horizontal(|ui| {
                let (rect, response) = ui.allocate_exact_size(Vec2::splat(36.0), Sense::click());
                let fill = if response.hovered() {
                    palette.accent
                } else {
                    palette.accent.gamma_multiply(0.85)
                };
                ui.painter().circle_filled(rect.center(), 18.0, fill);
                Icon::Play.image(palette.on_accent, 18.0).paint_at(
                    ui,
                    egui::Rect::from_center_size(
                        rect.center() + Vec2::new(1.5, 0.0),
                        Vec2::splat(18.0),
                    ),
                );
                let tip = tf("Play {name}", &[("name", &file.name)]);
                theme::focus_ring(ui, &response, palette, 18);
                theme::describe(&response, egui::WidgetType::Button, &tip);
                if response
                    .on_hover_cursor(egui::CursorIcon::PointingHand)
                    .on_hover_text(t("Play in your media player"))
                    .clicked()
                    && let Some(url) = play.clone()
                {
                    actions.push(Action::OpenFile {
                        url,
                        name: file.name.clone(),
                    });
                }
                let download = file.download_url.clone().or(play);
                // The name takes what the download button leaves.
                let room = (ui.available_width() - 36.0).max(60.0);
                ui.vertical(|ui| {
                    ui.set_max_width(room);
                    ui.spacing_mut().item_spacing.y = 1.0;
                    ui.add(
                        egui::Label::new(
                            RichText::new(&file.name)
                                .font(theme::semibold(14.0))
                                .color(palette.text),
                        )
                        .truncate(),
                    );
                    ui.label(
                        RichText::new(file_detail(file))
                            .font(theme::regular(12.0))
                            .color(palette.secondary),
                    );
                });
                if let Some(url) = download
                    && theme::icon_button(ui, palette, Icon::Download, 16.0, &t("Download"))
                        .clicked()
                {
                    actions.push(Action::Download {
                        url,
                        name: file.name.clone(),
                    });
                }
            });
        })
        .response
        .rect
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pictures_shrink_to_fit_and_never_grow() {
        let max = Vec2::new(400.0, 300.0);
        let fallback = Vec2::new(400.0, 225.0);
        assert_eq!(
            fit_within(Some([1200.0, 600.0]), max, fallback),
            Vec2::new(400.0, 200.0)
        );
        assert_eq!(
            fit_within(Some([100.0, 50.0]), max, fallback),
            Vec2::new(100.0, 50.0)
        );
        assert_eq!(fit_within(None, max, fallback), fallback);
        assert_eq!(fit_within(Some([0.0, 10.0]), max, fallback), fallback);
        // A sliver stays big enough to see and press.
        assert_eq!(fit_within(Some([4000.0, 10.0]), max, fallback).y, 24.0);
    }

    #[test]
    fn a_picture_file_takes_the_same_room_loaded_or_not() {
        let file = File {
            mimetype: "image/png".into(),
            thumb: Some("https://files.slack.com/t720.png".into()),
            thumb_size: Some([540.0, 720.0]),
            ..File::default()
        };
        let size = thumb_size(&file, 800.0);
        assert_eq!(size, Vec2::new(240.0, 320.0));
        let rect = egui::Rect::from_min_size(egui::Pos2::ZERO, size);
        // While it loads, and once the thumbnail Slack described arrives.
        assert_eq!(crate::ui::contain(rect, None), rect);
        assert_eq!(
            crate::ui::contain(rect, Some(Vec2::new(540.0, 720.0))),
            rect
        );
    }

    #[test]
    fn a_picture_file_of_unknown_size_gets_a_fixed_box() {
        let file = File {
            mimetype: "image/png".into(),
            thumb: Some("https://files.slack.com/t.png".into()),
            ..File::default()
        };
        assert_eq!(thumb_size(&file, 800.0), Vec2::new(360.0, 240.0));
        // A narrow column shrinks the box, keeping its shape.
        assert_eq!(thumb_size(&file, 180.0), Vec2::new(180.0, 120.0));
    }
}
