//! What apps and links add to a message: attachments with their fields
//! and pictures, and Block Kit blocks with their buttons.

use egui::{CornerRadius, Margin, RichText, Sense, Stroke, Vec2};

use super::Row;
use super::files::{fit_within, placeholder, play_badge, shows};
use crate::i18n::t;
use crate::model::{
    Accessory, Action, Attachment, Button, ButtonUse, ContextItem, Field, KitBlock, Message,
    NotHere,
};
use crate::theme;
use crate::ui::rich::{self, Rich};

/// The large picture an attachment shows under its text.
pub(super) enum CardMedia<'a> {
    /// A video's thumbnail, with a play button that opens `link`.
    Video { thumb: &'a str, link: &'a str },
    /// A picture whose size Slack gave.
    Image(&'a str),
}

/// What large picture `attachment` shows, and how big, in a card `width`
/// wide. A picture of unknown size is left out: it is sized once loaded.
pub(super) fn attachment_media(
    attachment: &Attachment,
    width: f32,
) -> Option<(CardMedia<'_>, Vec2)> {
    if let (Some(link), Some(thumb)) = (&attachment.video, &attachment.thumb) {
        let size = fit_within(
            attachment.thumb_size,
            Vec2::new(width.min(400.0), 225.0),
            Vec2::new(400.0, 225.0),
        );
        return Some((CardMedia::Video { thumb, link }, size));
    }
    let image = attachment.image.as_deref()?;
    let size = attachment.image_size?;
    let size = fit_within(
        Some(size),
        Vec2::new(width.min(400.0), 300.0),
        Vec2::new(400.0, 300.0),
    );
    Some((CardMedia::Image(image), size))
}

/// A small picture before a name in a card's header: a site's or an
/// author's icon.
///
/// Drawn only once it has loaded: a site's favicon can be in a format no
/// decoder here reads, or broken, and the card says who it is without it,
/// where an error mark would only be noise.
fn card_icon(ui: &mut egui::Ui, team: &str, url: &str, round: bool) {
    let radius = if round { 8 } else { 3 };
    let size = Vec2::splat(16.0);
    let image = egui::Image::new(crate::ui::image_uri(team, url))
        .fit_to_exact_size(size)
        .corner_radius(CornerRadius::same(radius));
    match image.load_for_size(ui.ctx(), size) {
        Ok(egui::load::TexturePoll::Ready { .. }) => {
            let (rect, _) = ui.allocate_exact_size(size, egui::Sense::hover());
            image.paint_at(ui, rect);
        }
        // Held open while it loads, so the name beside it doesn't jump.
        Ok(egui::load::TexturePoll::Pending { .. }) => {
            ui.allocate_exact_size(size, egui::Sense::hover());
        }
        Err(_) => {}
    }
}

/// A link card or a bot's legacy attachment: the site and author, the
/// title, text and fields, a picture or a video's thumbnail, and a
/// footer, behind a coloured bar.
pub(super) fn attachment_view(
    ui: &mut egui::Ui,
    row: &Row<'_>,
    message: &Message,
    attachment: &Attachment,
    actions: &mut Vec<Action>,
) {
    let palette = row.palette;
    let team = &row.workspace.info.team_id;
    if let Some(pretext) = &attachment.pretext {
        let rich = Rich::new(palette, row.workspace);
        rich::show(ui, &rich, pretext, false, actions);
    }
    const THUMB: f32 = 64.0;
    // A video shows its thumbnail large; anything else keeps it small at
    // the side.
    // With pictures held back it is left out: too small to be worth a
    // button of its own.
    let side_thumb = attachment
        .thumb
        .as_deref()
        .filter(|_| attachment.video.is_none() && row.look.inline_media);
    let response = egui::Frame::new()
        .inner_margin(Margin {
            left: 12,
            right: 4,
            top: 2,
            bottom: 2,
        })
        .show(ui, |ui| {
            let width = ui.available_width().min(560.0);
            ui.set_max_width(width);
            ui.horizontal_top(|ui| {
                let content = if side_thumb.is_some() {
                    width - THUMB - 12.0
                } else {
                    width
                };
                ui.vertical(|ui| {
                    ui.set_max_width(content);
                    ui.spacing_mut().item_spacing.y = 4.0;
                    if attachment.service.is_some() || attachment.service_icon.is_some() {
                        ui.horizontal(|ui| {
                            ui.spacing_mut().item_spacing.x = 6.0;
                            // As tall as the text, not as a button.
                            ui.spacing_mut().interact_size.y = 16.0;
                            if let Some(icon) = &attachment.service_icon {
                                card_icon(ui, team, icon, false);
                            }
                            if let Some(service) = &attachment.service {
                                ui.label(
                                    RichText::new(crate::mrkdwn::unescape(service))
                                        .font(theme::semibold(12.5))
                                        .color(palette.secondary),
                                );
                            }
                        });
                    }
                    if let Some(author) = &attachment.author {
                        ui.horizontal(|ui| {
                            ui.spacing_mut().item_spacing.x = 6.0;
                            ui.spacing_mut().interact_size.y = 16.0;
                            if let Some(icon) = &attachment.author_icon {
                                card_icon(ui, team, icon, true);
                            }
                            let text = RichText::new(crate::mrkdwn::unescape(author))
                                .font(theme::semibold(13.0))
                                .color(palette.text);
                            let response = ui.add(egui::Label::new(text).sense(Sense::click()));
                            if let Some(link) = &attachment.author_link {
                                let response = response
                                    .on_hover_cursor(egui::CursorIcon::PointingHand)
                                    .on_hover_text(link);
                                if response.clicked() {
                                    actions.push(Action::OpenUrl(link.clone()));
                                }
                            }
                        });
                    }
                    if let Some(title) = &attachment.title {
                        let text = RichText::new(crate::mrkdwn::unescape(title))
                            .font(theme::bold(14.5))
                            .color(if attachment.title_link.is_some() {
                                palette.link
                            } else {
                                palette.text
                            });
                        let response = ui.add(
                            egui::Label::new(text)
                                .wrap()
                                .selectable(true)
                                .sense(Sense::click()),
                        );
                        if let Some(link) = &attachment.title_link {
                            let response = response
                                .on_hover_cursor(egui::CursorIcon::PointingHand)
                                .on_hover_text(link);
                            if response.clicked() {
                                actions.push(Action::OpenUrl(link.clone()));
                            }
                        }
                    }
                    if !attachment.text.is_empty() {
                        let rich = Rich::new(palette, row.workspace).size(14.0);
                        rich::show(ui, &rich, &attachment.text, false, actions);
                    }
                    if !attachment.fields.is_empty() {
                        fields_grid(ui, row, &attachment.fields, actions);
                    }
                    if !attachment.blocks.is_empty() {
                        blocks_view(ui, row, message, &attachment.blocks, actions);
                    }
                    let name = attachment.title.as_deref().unwrap_or_default();
                    let picture = attachment
                        .video
                        .as_ref()
                        .and(attachment.thumb.as_deref())
                        .or(attachment.image.as_deref())
                        .map(|url| crate::ui::image_uri(team, url));
                    if let Some(uri) = &picture
                        && !shows(ui, row, uri)
                    {
                        let label = attachment.service.as_deref().unwrap_or(name);
                        placeholder(ui, row, uri, label);
                    } else {
                        match attachment_media(attachment, ui.available_width()) {
                            Some((CardMedia::Video { thumb, link }, size)) => {
                                let response = ui
                                    .add(
                                        egui::Image::new(crate::ui::image_uri(team, thumb))
                                            .fit_to_exact_size(size)
                                            .corner_radius(CornerRadius::same(theme::RADIUS_SMALL))
                                            .show_loading_spinner(true)
                                            .sense(Sense::click()),
                                    )
                                    .on_hover_cursor(egui::CursorIcon::PointingHand);
                                play_badge(ui, response.rect.center(), response.hovered());
                                let tip = t("Play in the browser");
                                theme::describe(&response, egui::WidgetType::Button, &tip);
                                if response.on_hover_text(&*tip).clicked() {
                                    actions.push(Action::OpenUrl(link.to_owned()));
                                }
                            }
                            Some((CardMedia::Image(image), size)) => {
                                let uri = crate::ui::image_uri(team, image);
                                let response = ui
                                    .add(
                                        egui::Image::new(uri.clone())
                                            .fit_to_exact_size(size)
                                            .corner_radius(CornerRadius::same(theme::RADIUS_SMALL))
                                            .show_loading_spinner(true)
                                            .sense(Sense::click()),
                                    )
                                    .on_hover_cursor(egui::CursorIcon::ZoomIn);
                                if response.clicked() {
                                    actions.push(Action::Preview {
                                        uri,
                                        name: name.to_owned(),
                                    });
                                }
                            }
                            None => {
                                if let Some(image) = &attachment.image {
                                    ui.add(
                                        egui::Image::new(crate::ui::image_uri(team, image))
                                            .fit_to_original_size(1.0)
                                            .max_size(Vec2::new(
                                                ui.available_width().min(400.0),
                                                300.0,
                                            ))
                                            .corner_radius(CornerRadius::same(theme::RADIUS_SMALL)),
                                    );
                                }
                            }
                        }
                    }
                    if let Some(footer) = &attachment.footer {
                        let rich = Rich::new(palette, row.workspace)
                            .size(12.0)
                            .color(palette.dim);
                        rich::show(ui, &rich, footer, false, actions);
                    }
                });
                if let Some(thumb) = side_thumb {
                    ui.add(
                        egui::Image::new(crate::ui::image_uri(team, thumb))
                            .fit_to_exact_size(Vec2::splat(THUMB))
                            .corner_radius(CornerRadius::same(theme::RADIUS_SMALL)),
                    );
                }
            });
        });
    let rect = response.response.rect;
    ui.painter().rect_filled(
        egui::Rect::from_min_size(rect.min, Vec2::new(4.0, rect.height())),
        CornerRadius::same(2),
        attachment.color.unwrap_or(palette.outline),
    );
}

/// One labelled value: the title in bold, the value under it.
fn field_cell(
    ui: &mut egui::Ui,
    row: &Row<'_>,
    title: &str,
    value: &str,
    actions: &mut Vec<Action>,
) {
    ui.spacing_mut().item_spacing.y = 1.0;
    if !title.is_empty() {
        ui.add(
            egui::Label::new(
                RichText::new(crate::mrkdwn::unescape(title))
                    .font(theme::bold(13.5))
                    .color(row.palette.text),
            )
            .wrap()
            .selectable(true),
        );
    }
    if !value.is_empty() {
        let rich = Rich::new(row.palette, row.workspace).size(13.5);
        rich::show(ui, &rich, value, false, actions);
    }
}

/// Legacy attachment fields: short ones two to a row, the rest full width.
fn fields_grid(ui: &mut egui::Ui, row: &Row<'_>, fields: &[Field], actions: &mut Vec<Action>) {
    let mut index = 0;
    while index < fields.len() {
        let field = &fields[index];
        let pair = fields
            .get(index + 1)
            .filter(|next| field.short && next.short);
        ui.add_space(2.0);
        match pair {
            Some(next) => {
                ui.columns(2, |columns| {
                    field_cell(&mut columns[0], row, &field.title, &field.value, actions);
                    field_cell(&mut columns[1], row, &next.title, &next.value, actions);
                });
                index += 2;
            }
            None => {
                ui.vertical(|ui| field_cell(ui, row, &field.title, &field.value, actions));
                index += 1;
            }
        }
    }
}

/// A Block Kit button. A link opens in the browser. An app's interactive
/// button is pressed from a browser session, and busy until Slack takes
/// the press; with an OAuth sign-in it is shown but only works in Slack
/// (see [`crate::model::button_use`]). Returns whether it cannot be
/// pressed here, so the caller offers to open the message in Slack.
fn kit_button(
    ui: &mut egui::Ui,
    row: &Row<'_>,
    message: &Message,
    button: &Button,
    actions: &mut Vec<Action>,
) -> bool {
    let palette = row.palette;
    let (fill, text_color) = match button.style.as_deref() {
        Some("primary") => (palette.accent, palette.on_accent),
        Some("danger") => (palette.danger, egui::Color32::WHITE),
        _ => (palette.surface, palette.text),
    };
    let label = RichText::new(crate::mrkdwn::unescape(&button.text))
        .font(theme::medium(13.0))
        .color(text_color);
    let widget = egui::Button::new(label)
        .fill(fill)
        .stroke(Stroke::new(1.0, palette.outline))
        .corner_radius(CornerRadius::same(theme::RADIUS_SMALL + 2))
        .min_size(Vec2::new(0.0, 28.0));
    let sign_in = row.workspace.info.sign_in;
    match crate::model::button_use(sign_in, row.channel, message, button) {
        ButtonUse::Link(url) => {
            let response = ui
                .add(widget)
                .on_hover_cursor(egui::CursorIcon::PointingHand)
                .on_hover_text(url);
            if response.clicked() {
                actions.push(Action::OpenUrl(url.to_owned()));
            }
            false
        }
        ButtonUse::Press(press) if row.workspace.pressing.contains(&press) => {
            ui.add_enabled(false, widget)
                .on_disabled_hover_text(t("Waiting for Slack…"));
            ui.add(egui::Spinner::new().size(14.0).color(palette.secondary));
            false
        }
        ButtonUse::Press(press) => {
            let response = ui
                .add(widget)
                .on_hover_cursor(egui::CursorIcon::PointingHand);
            if response.clicked() {
                actions.push(Action::PressButton {
                    press,
                    confirm: button.confirm.clone(),
                    confirmed: false,
                });
            }
            false
        }
        ButtonUse::NotHere(why) => {
            let tip = match why {
                NotHere::NeedsSession => t(
                    "Slack lets only its own apps and browser sign-ins press an app's buttons. Open the message in Slack to use it.",
                ),
                NotHere::NoApp => t("This button works only in Slack itself."),
            };
            ui.add_enabled(false, widget).on_disabled_hover_text(tip);
            true
        }
    }
}

/// A quiet "Open in Slack" beside buttons that only work there.
fn open_in_slack(ui: &mut egui::Ui, row: &Row<'_>, message: &Message, actions: &mut Vec<Action>) {
    if message.ts.is_local() {
        return;
    }
    let label = RichText::new(format!("{} ↗", t("Open in Slack")))
        .font(theme::regular(13.0))
        .color(row.palette.accent);
    let response = ui
        .add(egui::Button::new(label).frame(false))
        .on_hover_cursor(egui::CursorIcon::PointingHand)
        .on_hover_text(t("Open this message in Slack, where its buttons work"));
    if response.clicked() {
        actions.push(Action::OpenInSlack {
            channel: row.channel.to_owned(),
            ts: message.ts.clone(),
            thread: message.thread_ts.clone(),
        });
    }
}

/// Block Kit: headers, sections with fields and accessories, context lines,
/// dividers, images and link buttons.
pub(super) fn blocks_view(
    ui: &mut egui::Ui,
    row: &Row<'_>,
    message: &Message,
    blocks: &[KitBlock],
    actions: &mut Vec<Action>,
) {
    let palette = row.palette;
    let team = &row.workspace.info.team_id;
    ui.vertical(|ui| {
        ui.spacing_mut().item_spacing.y = 6.0;
        for block in blocks {
            match block {
                KitBlock::Header(text) => {
                    let rich = Rich::new(palette, row.workspace).size(16.5);
                    rich::show(ui, &rich, &format!("*{}*", text.trim()), false, actions);
                }
                KitBlock::RichText(blocks) => {
                    let rich = Rich::new(palette, row.workspace);
                    rich::show_parsed(ui, &rich, blocks, false, actions);
                }
                KitBlock::Section {
                    text,
                    fields,
                    accessory,
                } => {
                    const SIDE: f32 = 72.0;
                    let width = ui.available_width();
                    ui.horizontal_top(|ui| {
                        let content = if accessory.is_some() {
                            (width - SIDE - 16.0).max(120.0)
                        } else {
                            width
                        };
                        ui.vertical(|ui| {
                            ui.set_max_width(content);
                            if let Some(text) = text {
                                let rich = Rich::new(palette, row.workspace);
                                rich::show(ui, &rich, text, false, actions);
                            }
                            // Section fields are always a two-column grid.
                            for pair in fields.chunks(2) {
                                ui.columns(2, |columns| {
                                    for (column, value) in columns.iter_mut().zip(pair) {
                                        let rich = Rich::new(palette, row.workspace).size(14.0);
                                        rich::show(column, &rich, value, false, actions);
                                    }
                                });
                            }
                        });
                        match accessory {
                            Some(Accessory::Image { url, alt }) => {
                                ui.add(
                                    egui::Image::new(crate::ui::image_uri(team, url))
                                        .fit_to_exact_size(Vec2::splat(SIDE))
                                        .corner_radius(CornerRadius::same(theme::RADIUS_SMALL)),
                                )
                                .on_hover_text(alt);
                            }
                            Some(Accessory::Button(button)) => {
                                ui.vertical(|ui| {
                                    if kit_button(ui, row, message, button, actions) {
                                        open_in_slack(ui, row, message, actions);
                                    }
                                });
                            }
                            None => {}
                        }
                    });
                }
                KitBlock::Context(items) => {
                    ui.horizontal_wrapped(|ui| {
                        ui.spacing_mut().item_spacing.x = 6.0;
                        for item in items {
                            match item {
                                ContextItem::Text(text) => {
                                    let rich = Rich::new(palette, row.workspace)
                                        .size(12.5)
                                        .color(palette.secondary);
                                    rich::show(ui, &rich, text, false, actions);
                                }
                                ContextItem::Image { url, alt } => {
                                    ui.add(
                                        egui::Image::new(crate::ui::image_uri(team, url))
                                            .fit_to_exact_size(Vec2::splat(16.0))
                                            .corner_radius(CornerRadius::same(3)),
                                    )
                                    .on_hover_text(alt);
                                }
                            }
                        }
                    });
                }
                KitBlock::Divider => {
                    let (rect, _) = ui
                        .allocate_exact_size(Vec2::new(ui.available_width(), 9.0), Sense::hover());
                    ui.painter().hline(
                        rect.x_range(),
                        rect.center().y,
                        Stroke::new(1.0, palette.outline),
                    );
                }
                KitBlock::Image { url, alt, title } => {
                    if let Some(title) = title {
                        let rich = Rich::new(palette, row.workspace)
                            .size(13.0)
                            .color(palette.secondary);
                        rich::show(ui, &rich, title, false, actions);
                    }
                    let uri = crate::ui::image_uri(team, url);
                    if !shows(ui, row, &uri) {
                        placeholder(ui, row, &uri, alt);
                        continue;
                    }
                    let response = ui
                        .add(
                            egui::Image::new(uri.clone())
                                .fit_to_original_size(1.0)
                                .max_size(Vec2::new(ui.available_width().min(440.0), 320.0))
                                .corner_radius(CornerRadius::same(theme::RADIUS))
                                .show_loading_spinner(true)
                                .sense(Sense::click()),
                        )
                        .on_hover_cursor(egui::CursorIcon::ZoomIn)
                        .on_hover_text(alt);
                    if response.clicked() {
                        actions.push(Action::Preview {
                            uri,
                            name: alt.clone(),
                        });
                    }
                }
                KitBlock::Actions(buttons) => {
                    ui.horizontal_wrapped(|ui| {
                        ui.spacing_mut().item_spacing.x = 6.0;
                        let mut not_here = false;
                        for button in buttons {
                            not_here |= kit_button(ui, row, message, button, actions);
                        }
                        if not_here {
                            open_in_slack(ui, row, message, actions);
                        }
                    });
                }
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn card_media_is_sized_before_it_loads() {
        let video = Attachment {
            video: Some("https://youtu.be/x".into()),
            thumb: Some("https://i.ytimg.com/x.jpg".into()),
            thumb_size: Some([1280.0, 720.0]),
            ..Attachment::default()
        };
        let Some((CardMedia::Video { link, .. }, size)) = attachment_media(&video, 560.0) else {
            panic!("a video");
        };
        assert_eq!(link, "https://youtu.be/x");
        assert_eq!(size, Vec2::new(400.0, 225.0));
        let unsized_image = Attachment {
            image: Some("https://blog.example/p.png".into()),
            ..Attachment::default()
        };
        assert!(attachment_media(&unsized_image, 560.0).is_none());
        let sized = Attachment {
            image_size: Some([800.0, 400.0]),
            ..unsized_image
        };
        assert!(matches!(
            attachment_media(&sized, 300.0),
            Some((CardMedia::Image(_), size)) if size == Vec2::new(300.0, 150.0)
        ));
    }
}
