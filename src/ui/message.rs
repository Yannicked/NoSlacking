//! One message: avatar, name and time, text, files, cards, reactions and
//! the thread summary, with a hover toolbar.

use egui::{Align, CornerRadius, Layout, Margin, RichText, Sense, Stroke, UiBuilder, Vec2};

use super::rich::{self, Rich};
use crate::app::{Editing, WorkspaceState};
use crate::i18n::{t, tn};
use crate::model::{
    Accessory, Action, Attachment, Button, ContextItem, Delivery, Field, File, KitBlock, Message,
};
use crate::theme::{self, Icon, Palette};

pub struct Row<'a> {
    pub palette: &'a Palette,
    pub workspace: &'a WorkspaceState,
    pub channel: &'a str,
    pub in_thread: bool,
    /// Whether Enter saves an edit (else Ctrl+Enter), as in the composer.
    pub enter_sends: bool,
    /// Whether a dialog is open over the list, which then owns Esc.
    pub overlay: bool,
}

/// How a message relates to the one before it.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Lead {
    /// A new author or a long pause: avatar and name.
    Full,
    /// The same author moments later: just the text.
    Compact,
}

/// Whether `message` continues `previous` without a new header.
pub fn continues(previous: Option<&Message>, message: &Message) -> bool {
    let Some(previous) = previous else {
        return false;
    };
    if previous.is_system() || message.is_system() {
        return false;
    }
    if previous.user != message.user || previous.username != message.username {
        return false;
    }
    match (previous.ts.seconds(), message.ts.seconds()) {
        (Some(a), Some(b)) => (b - a).abs() < 5 * 60,
        _ => message.ts.is_local(),
    }
}

const GUTTER: f32 = 44.0;

pub fn show(
    ui: &mut egui::Ui,
    row: &Row<'_>,
    message: &Message,
    lead: Lead,
    editing: &mut Option<Editing>,
    actions: &mut Vec<Action>,
) {
    let palette = row.palette;
    let workspace = row.workspace;
    let me = message.user.as_deref() == Some(workspace.info.user_id.as_str());
    if message.is_system() {
        system(ui, row, message, actions);
        return;
    }
    let background = ui.painter().add(egui::Shape::Noop);
    let is_editing = editing.as_ref().is_some_and(|e| {
        e.ts == message.ts && e.channel == row.channel && e.in_thread == row.in_thread
    });
    let top = if lead == Lead::Full { 8 } else { 2 };
    let response = egui::Frame::new()
        .inner_margin(Margin {
            left: 16,
            right: 16,
            top,
            bottom: 3,
        })
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal_top(|ui| {
                ui.spacing_mut().item_spacing.x = 8.0;
                gutter(ui, row, message, lead, actions);
                ui.vertical(|ui| {
                    ui.spacing_mut().item_spacing.y = 4.0;
                    if lead == Lead::Full {
                        header(ui, row, message, actions);
                    }
                    if is_editing {
                        edit(ui, row, editing, actions);
                    } else {
                        body(ui, row, message, actions);
                    }
                });
            });
        })
        .response;
    let rect = response.rect;
    let hovered = ui.rect_contains_pointer(rect) && !is_editing;
    if hovered {
        ui.painter().set(
            background,
            egui::Shape::rect_filled(
                rect,
                CornerRadius::ZERO,
                palette.surface.gamma_multiply(0.6),
            ),
        );
        if lead == Lead::Compact {
            let time = super::short_time(&message.ts);
            ui.painter().text(
                egui::pos2(rect.left() + 16.0 + GUTTER / 2.0 - 4.0, rect.top() + 12.0),
                egui::Align2::CENTER_CENTER,
                time,
                theme::regular(11.0),
                palette.dim,
            );
        }
        if message.delivery == Delivery::Sent {
            toolbar(ui, row, message, me, rect, actions);
        }
    }
}

fn system(ui: &mut egui::Ui, row: &Row<'_>, message: &Message, actions: &mut Vec<Action>) {
    egui::Frame::new()
        .inner_margin(Margin {
            left: 16 + GUTTER as i8 + 8,
            right: 16,
            top: 4,
            bottom: 4,
        })
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            let rich = Rich::new(row.palette, row.workspace)
                .size(13.0)
                .color(row.palette.secondary);
            rich::show(ui, &rich, &message.text, false, actions);
        });
}

fn gutter(
    ui: &mut egui::Ui,
    row: &Row<'_>,
    message: &Message,
    lead: Lead,
    actions: &mut Vec<Action>,
) {
    if lead == Lead::Compact {
        ui.allocate_exact_size(Vec2::new(GUTTER - 8.0, 1.0), Sense::hover());
        return;
    }
    let url = row.workspace.author_icon(message);
    let name = row.workspace.author(message);
    let seed = message
        .user
        .as_deref()
        .or(message.bot_id.as_deref())
        .unwrap_or(&name);
    let response = super::avatar(ui, url, &name, seed, theme::AVATAR);
    if response.clicked()
        && let Some(user) = &message.user
    {
        actions.push(Action::OpenProfile(user.clone()));
    }
}

fn header(ui: &mut egui::Ui, row: &Row<'_>, message: &Message, actions: &mut Vec<Action>) {
    let palette = row.palette;
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 6.0;
        let name = row.workspace.author(message);
        let response = ui
            .add(
                egui::Label::new(
                    RichText::new(&name)
                        .font(theme::bold(14.5))
                        .color(palette.text),
                )
                .sense(Sense::click()),
            )
            .on_hover_cursor(egui::CursorIcon::PointingHand);
        if response.clicked()
            && let Some(user) = &message.user
        {
            actions.push(Action::OpenProfile(user.clone()));
        }
        // A person posting through an app (/giphy) keeps their own name.
        let bot = message.username.is_some()
            || (message.bot_id.is_some() && message.user.is_none())
            || message
                .user
                .as_deref()
                .and_then(|id| row.workspace.user(id))
                .is_some_and(|u| u.is_bot);
        if bot {
            ui.label(
                RichText::new(t("APP"))
                    .font(theme::semibold(10.0))
                    .color(palette.secondary)
                    .background_color(palette.surface_active),
            );
        }
        let time = ui.label(
            RichText::new(super::short_time(&message.ts))
                .font(theme::regular(12.0))
                .color(palette.dim),
        );
        // The full date only when asked for: formatting it for every
        // message on every frame was wasted work.
        if message.ts.seconds().is_some() {
            time.on_hover_ui(|ui| {
                if let Some(zoned) = message.ts.zoned() {
                    ui.label(zoned.strftime("%A, %B %-d, %Y at %H:%M:%S").to_string());
                }
            });
        }
    });
}

fn body(ui: &mut egui::Ui, row: &Row<'_>, message: &Message, actions: &mut Vec<Action>) {
    let palette = row.palette;
    let faded = message.delivery == Delivery::Sending;
    if faded {
        ui.set_opacity(0.55);
    }
    if message.uses_blocks() {
        // Apps send `text` only as the notification fallback; the blocks are
        // the message.
        blocks_view(ui, row, &message.blocks, actions);
    } else if !message.text.is_empty() {
        let rich = Rich::new(palette, row.workspace);
        rich::show(ui, &rich, &message.text, message.edited, actions);
    }
    for file in &message.files {
        file_view(ui, row, file, actions);
    }
    for attachment in &message.attachments {
        attachment_view(ui, row, attachment, actions);
    }
    if !message.reactions.is_empty() {
        reactions(ui, row, message, actions);
    }
    if message.reply_count > 0 && !row.in_thread && !message.is_reply() {
        thread_summary(ui, row, message, actions);
    }
    if message.is_reply() && message.broadcast && !row.in_thread {
        let text = RichText::new(t("replied to a thread"))
            .font(theme::regular(12.0))
            .color(palette.secondary);
        if ui
            .add(egui::Label::new(text).sense(Sense::click()))
            .on_hover_cursor(egui::CursorIcon::PointingHand)
            .clicked()
            && let Some(parent) = &message.thread_ts
        {
            actions.push(Action::OpenThread {
                channel: row.channel.to_owned(),
                ts: parent.clone(),
            });
        }
    }
    if let Delivery::Failed(error) = &message.delivery {
        ui.horizontal(|ui| {
            let (icon, _) = ui.allocate_exact_size(Vec2::splat(14.0), Sense::hover());
            Icon::CircleAlert
                .image(palette.danger, 14.0)
                .paint_at(ui, icon);
            ui.label(
                RichText::new(format!("{} {error}.", t("Not sent:")))
                    .font(theme::regular(12.5))
                    .color(palette.danger),
            );
            if ui
                .add(
                    egui::Label::new(
                        RichText::new(t("Retry"))
                            .font(theme::semibold(12.5))
                            .color(palette.link),
                    )
                    .sense(Sense::click()),
                )
                .on_hover_cursor(egui::CursorIcon::PointingHand)
                .clicked()
            {
                actions.push(Action::Retry {
                    channel: row.channel.to_owned(),
                    local: message.ts.clone(),
                });
            }
            if ui
                .add(
                    egui::Label::new(
                        RichText::new(t("Discard"))
                            .font(theme::semibold(12.5))
                            .color(palette.secondary),
                    )
                    .sense(Sense::click()),
                )
                .clicked()
            {
                actions.push(Action::Delete {
                    channel: row.channel.to_owned(),
                    ts: message.ts.clone(),
                });
            }
        });
    }
}

/// The edit field's id: one per message and panel, since a thread's parent
/// shows in both the conversation and the thread.
pub fn edit_id(editing: &Editing) -> egui::Id {
    egui::Id::new((
        "edit",
        editing.channel.as_str(),
        editing.ts.as_str(),
        editing.in_thread,
    ))
}

fn edit(
    ui: &mut egui::Ui,
    row: &Row<'_>,
    editing: &mut Option<Editing>,
    actions: &mut Vec<Action>,
) {
    let palette = row.palette;
    let Some(current) = editing.as_mut() else {
        return;
    };
    let id = edit_id(current);
    // Read focus before taking the input lock: egui guards input and memory
    // with one context lock, so asking for memory inside `input_mut`
    // deadlocks the interface.
    let focused = ui.memory(|m| m.has_focus(id));
    let save_with = if row.enter_sends {
        egui::Modifiers::NONE
    } else {
        egui::Modifiers::COMMAND
    };
    let (save, cancel) = ui.input_mut(|input| {
        (
            focused && input.consume_key(save_with, egui::Key::Enter),
            // An open dialog takes Esc for itself.
            focused && !row.overlay && input.consume_key(egui::Modifiers::NONE, egui::Key::Escape),
        )
    });
    egui::Frame::new()
        .fill(palette.surface)
        .stroke(Stroke::new(1.0, palette.accent))
        .corner_radius(CornerRadius::same(theme::RADIUS))
        .inner_margin(Margin::same(8))
        .show(ui, |ui| {
            let response = ui.add(
                egui::TextEdit::multiline(&mut current.text)
                    .id(id)
                    .frame(egui::Frame::NONE)
                    .desired_rows(1)
                    .desired_width(f32::INFINITY)
                    .font(theme::regular(14.5)),
            );
            if std::mem::take(&mut current.focus) {
                response.request_focus();
            }
        });
    ui.horizontal(|ui| {
        let cancel_clicked = theme::secondary_button(ui, palette, &t("Cancel")).clicked();
        let save_clicked = theme::primary_button(ui, palette, &t("Save")).clicked();
        if cancel || cancel_clicked {
            actions.push(Action::CancelEdit);
        } else if save || save_clicked {
            actions.push(Action::Edit {
                channel: current.channel.clone(),
                ts: current.ts.clone(),
                text: current.text.clone(),
            });
        }
        let hint = if row.enter_sends {
            t("Enter to save, Esc to cancel")
        } else {
            t("Ctrl+Enter to save, Esc to cancel")
        };
        ui.label(
            RichText::new(hint)
                .font(theme::regular(12.0))
                .color(palette.dim),
        );
    });
}

fn file_view(ui: &mut egui::Ui, row: &Row<'_>, file: &File, actions: &mut Vec<Action>) {
    let palette = row.palette;
    let team = &row.workspace.info.team_id;
    if file.is_image()
        && let Some(thumb) = &file.thumb
    {
        let [w, h] = file.thumb_size.unwrap_or([360.0, 240.0]);
        let max = Vec2::new(ui.available_width().min(420.0), 320.0);
        let scale = (max.x / w).min(max.y / h).min(1.0);
        let size = Vec2::new(w * scale, h * scale).max(Vec2::splat(24.0));
        let uri = super::image_uri(team, thumb);
        let response = ui
            .add(
                egui::Image::new(uri.clone())
                    .fit_to_exact_size(size)
                    .corner_radius(CornerRadius::same(theme::RADIUS))
                    .show_loading_spinner(true)
                    .sense(Sense::click()),
            )
            .on_hover_cursor(egui::CursorIcon::ZoomIn)
            .on_hover_text(&file.name);
        if response.clicked() {
            let full = file
                .url_private
                .as_deref()
                .filter(|_| !file.mimetype.contains("gif") || file.size < 8 * 1024 * 1024)
                .map_or(uri, |url| super::image_uri(team, url));
            actions.push(Action::Preview {
                uri: full,
                name: file.name.clone(),
            });
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
                    let kind = file
                        .mimetype
                        .split('/')
                        .next_back()
                        .unwrap_or("")
                        .to_uppercase();
                    ui.label(
                        RichText::new(format!("{} · {kind}", super::file_size(file.size)))
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

fn attachment_view(
    ui: &mut egui::Ui,
    row: &Row<'_>,
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
                let content = if attachment.thumb.is_some() {
                    width - THUMB - 12.0
                } else {
                    width
                };
                ui.vertical(|ui| {
                    ui.set_max_width(content);
                    ui.spacing_mut().item_spacing.y = 4.0;
                    if let Some(service) = &attachment.service {
                        ui.label(
                            RichText::new(crate::mrkdwn::unescape(service))
                                .font(theme::semibold(12.5))
                                .color(palette.secondary),
                        );
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
                        blocks_view(ui, row, &attachment.blocks, actions);
                    }
                    if let Some(image) = &attachment.image {
                        ui.add(
                            egui::Image::new(super::image_uri(team, image))
                                .fit_to_original_size(1.0)
                                .max_size(Vec2::new(ui.available_width().min(400.0), 300.0))
                                .corner_radius(CornerRadius::same(theme::RADIUS_SMALL)),
                        );
                    }
                    if let Some(footer) = &attachment.footer {
                        let rich = Rich::new(palette, row.workspace)
                            .size(12.0)
                            .color(palette.dim);
                        rich::show(ui, &rich, footer, false, actions);
                    }
                });
                if let Some(thumb) = &attachment.thumb {
                    ui.add(
                        egui::Image::new(super::image_uri(team, thumb))
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

/// A Block Kit button: links open in the browser; anything else needs the
/// app's own server, so it is shown but cannot be pressed.
fn kit_button(ui: &mut egui::Ui, palette: &Palette, button: &Button, actions: &mut Vec<Action>) {
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
    match &button.url {
        Some(url) => {
            let response = ui
                .add(widget)
                .on_hover_cursor(egui::CursorIcon::PointingHand)
                .on_hover_text(url);
            if response.clicked() {
                actions.push(Action::OpenUrl(url.clone()));
            }
        }
        None => {
            ui.add_enabled(false, widget)
                .on_disabled_hover_text(t("This button works only in Slack itself."));
        }
    }
}

/// Block Kit: headers, sections with fields and accessories, context lines,
/// dividers, images and link buttons.
fn blocks_view(ui: &mut egui::Ui, row: &Row<'_>, blocks: &[KitBlock], actions: &mut Vec<Action>) {
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
                KitBlock::RichText(text) => {
                    let rich = Rich::new(palette, row.workspace);
                    rich::show(ui, &rich, text, false, actions);
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
                                    egui::Image::new(super::image_uri(team, url))
                                        .fit_to_exact_size(Vec2::splat(SIDE))
                                        .corner_radius(CornerRadius::same(theme::RADIUS_SMALL)),
                                )
                                .on_hover_text(alt);
                            }
                            Some(Accessory::Button(button)) => {
                                kit_button(ui, palette, button, actions)
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
                                        egui::Image::new(super::image_uri(team, url))
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
                    let uri = super::image_uri(team, url);
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
                        for button in buttons {
                            kit_button(ui, palette, button, actions);
                        }
                    });
                }
            }
        }
    });
}

fn reactions(ui: &mut egui::Ui, row: &Row<'_>, message: &Message, actions: &mut Vec<Action>) {
    let palette = row.palette;
    let me = &row.workspace.info.user_id;
    ui.horizontal_wrapped(|ui| {
        ui.spacing_mut().item_spacing = Vec2::new(4.0, 4.0);
        for reaction in &message.reactions {
            let mine = reaction.users.contains(me);
            let (fill, stroke) = if mine {
                (
                    palette.accent.gamma_multiply(0.18),
                    Stroke::new(1.0, palette.accent),
                )
            } else {
                (palette.surface, Stroke::new(1.0, palette.surface))
            };
            let response = egui::Frame::new()
                .fill(fill)
                .stroke(stroke)
                .corner_radius(CornerRadius::same(12))
                .inner_margin(Margin::symmetric(7, 2))
                .show(ui, |ui| {
                    ui.horizontal(|ui| {
                        ui.spacing_mut().item_spacing.x = 4.0;
                        let rich = Rich::new(palette, row.workspace);
                        rich::emoji(ui, &rich, &reaction.name, 14.0);
                        ui.label(
                            RichText::new(reaction.count.to_string())
                                .font(theme::semibold(12.5))
                                .color(if mine {
                                    palette.accent
                                } else {
                                    palette.secondary
                                }),
                        );
                    });
                })
                .response
                .interact(Sense::click())
                .on_hover_cursor(egui::CursorIcon::PointingHand);
            let names: Vec<String> = reaction
                .users
                .iter()
                .take(12)
                .map(|id| {
                    if id == me {
                        t("You").into_owned()
                    } else {
                        row.workspace.user_label(id)
                    }
                })
                .collect();
            let response =
                response.on_hover_text(format!("{} :{}:", names.join(", "), reaction.name));
            if response.clicked() {
                actions.push(Action::React {
                    channel: row.channel.to_owned(),
                    ts: message.ts.clone(),
                    name: reaction.name.clone(),
                });
            }
        }
        let add = egui::Frame::new()
            .fill(palette.surface)
            .corner_radius(CornerRadius::same(12))
            .inner_margin(Margin::symmetric(7, 3))
            .show(ui, |ui| {
                let (icon, _) = ui.allocate_exact_size(Vec2::splat(15.0), Sense::hover());
                Icon::SmilePlus
                    .image(palette.secondary, 15.0)
                    .paint_at(ui, icon);
            })
            .response
            .interact(Sense::click())
            .on_hover_cursor(egui::CursorIcon::PointingHand)
            .on_hover_text(t("Add reaction"));
        if add.clicked() {
            actions.push(Action::PickReaction {
                channel: row.channel.to_owned(),
                ts: message.ts.clone(),
            });
        }
    });
}

fn thread_summary(ui: &mut egui::Ui, row: &Row<'_>, message: &Message, actions: &mut Vec<Action>) {
    let palette = row.palette;
    let response = egui::Frame::new()
        .corner_radius(CornerRadius::same(theme::RADIUS_SMALL))
        .inner_margin(Margin::symmetric(4, 3))
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 4.0;
                for id in message.reply_users.iter().take(5) {
                    let user = row.workspace.user(id);
                    let name = row.workspace.user_label(id);
                    super::avatar(ui, user.and_then(|u| u.avatar.as_deref()), &name, id, 20.0);
                }
                ui.add_space(4.0);
                ui.label(
                    RichText::new(tn("{count} reply", "{count} replies", message.reply_count))
                        .font(theme::bold(13.0))
                        .color(palette.link),
                );
                if let Some(latest) = &message.latest_reply {
                    ui.label(
                        RichText::new(format!("{} {}", t("Last reply"), super::relative(latest)))
                            .font(theme::regular(12.5))
                            .color(palette.dim),
                    );
                }
            });
        })
        .response
        .interact(Sense::click())
        .on_hover_cursor(egui::CursorIcon::PointingHand);
    if response.hovered() {
        ui.painter().rect_stroke(
            response.rect,
            CornerRadius::same(theme::RADIUS_SMALL),
            Stroke::new(1.0, palette.outline),
            egui::StrokeKind::Inside,
        );
    }
    if response.clicked() {
        actions.push(Action::OpenThread {
            channel: row.channel.to_owned(),
            ts: message.ts.clone(),
        });
    }
}

/// Quick reactions, react, reply, edit, delete and copy, over the message's
/// top-right corner.
fn toolbar(
    ui: &mut egui::Ui,
    row: &Row<'_>,
    message: &Message,
    me: bool,
    rect: egui::Rect,
    actions: &mut Vec<Action>,
) {
    let palette = row.palette;
    let mut buttons = 3 + usize::from(!row.in_thread);
    if me {
        buttons += 2;
    }
    let width = buttons as f32 * 30.0 + 28.0 + 8.0;
    let bar = egui::Rect::from_min_size(
        egui::pos2(rect.right() - width - 16.0, rect.top() + 2.0),
        Vec2::new(width, 32.0),
    );
    let mut child = ui.new_child(
        UiBuilder::new()
            .max_rect(bar)
            .layout(Layout::left_to_right(Align::Center)),
    );
    let frame = egui::Frame::new()
        .fill(palette.overlay)
        .stroke(Stroke::new(1.0, palette.outline))
        .corner_radius(CornerRadius::same(theme::RADIUS))
        .shadow(egui::epaint::Shadow {
            offset: [0, 2],
            blur: 6,
            spread: 0,
            color: palette.shadow.gamma_multiply(0.5),
        })
        .inner_margin(Margin::same(2));
    frame.show(&mut child, |ui| {
        ui.spacing_mut().item_spacing.x = 0.0;
        for name in ["white_check_mark", "eyes"] {
            let rich = Rich::new(palette, row.workspace);
            let (rect, response) = ui.allocate_exact_size(Vec2::splat(28.0), Sense::click());
            if response.hovered() {
                ui.painter().rect_filled(
                    rect,
                    CornerRadius::same(theme::RADIUS_SMALL),
                    palette.surface_hover,
                );
            }
            let mut inner = ui.new_child(
                UiBuilder::new()
                    .max_rect(rect.shrink(4.0))
                    .layout(Layout::centered_and_justified(egui::Direction::LeftToRight)),
            );
            rich::emoji(&mut inner, &rich, name, 13.0);
            if response
                .on_hover_cursor(egui::CursorIcon::PointingHand)
                .on_hover_text(format!(":{name}:"))
                .clicked()
            {
                actions.push(Action::React {
                    channel: row.channel.to_owned(),
                    ts: message.ts.clone(),
                    name: name.to_owned(),
                });
            }
        }
        if theme::icon_button(ui, palette, Icon::SmilePlus, 16.0, &t("Add reaction")).clicked() {
            actions.push(Action::PickReaction {
                channel: row.channel.to_owned(),
                ts: message.ts.clone(),
            });
        }
        if !row.in_thread
            && theme::icon_button(
                ui,
                palette,
                Icon::MessageCircle,
                16.0,
                &t("Reply in thread"),
            )
            .clicked()
        {
            actions.push(Action::OpenThread {
                channel: row.channel.to_owned(),
                ts: message
                    .thread_ts
                    .clone()
                    .unwrap_or_else(|| message.ts.clone()),
            });
        }
        if theme::icon_button(ui, palette, Icon::Copy, 16.0, &t("Copy text")).clicked() {
            let text = crate::mrkdwn::plain(&message.text, |inline| match inline {
                crate::mrkdwn::Inline::User { id, .. } => {
                    Some(format!("@{}", row.workspace.user_label(id)))
                }
                crate::mrkdwn::Inline::Channel { id, label } => Some(format!(
                    "#{}",
                    row.workspace
                        .conversation(id)
                        .map(|c| c.name.clone())
                        .or_else(|| label.clone())
                        .unwrap_or_else(|| id.clone())
                )),
                _ => None,
            });
            actions.push(Action::Copy(text));
        }
        if me {
            if theme::icon_button(ui, palette, Icon::Pencil, 16.0, &t("Edit message")).clicked() {
                let channel = row.channel.to_owned();
                let ts = message.ts.clone();
                actions.push(if row.in_thread {
                    Action::StartEditInThread { channel, ts }
                } else {
                    Action::StartEdit { channel, ts }
                });
            }
            if theme::icon_button(ui, palette, Icon::Trash, 16.0, &t("Delete message")).clicked() {
                actions.push(Action::AskDelete {
                    channel: row.channel.to_owned(),
                    ts: message.ts.clone(),
                });
            }
        }
    });
}
