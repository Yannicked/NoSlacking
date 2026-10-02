//! One message: avatar, name and time, text, files, cards, reactions and
//! the thread summary, with a hover toolbar.

use egui::{Align, CornerRadius, Layout, Margin, RichText, Sense, Stroke, UiBuilder, Vec2};

use super::rich::{self, Rich};
use crate::app::{Editing, Selected, WorkspaceState};
use crate::i18n::{t, tf, tn};
use crate::model::{
    Accessory, Action, Attachment, Button, ContextItem, Delivery, Field, File, KitBlock, Media,
    Message,
};
use crate::settings::Density;
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
    /// The message picked with the keyboard, when it is in this list.
    pub selected: Option<&'a Selected>,
    pub look: Look,
}

/// How messages are drawn, from the settings.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Look {
    pub density: Density,
    /// Whether pictures and previews show at once, or wait for a click.
    pub inline_media: bool,
}

impl Look {
    pub fn of(settings: &crate::settings::Settings) -> Self {
        Self {
            density: settings.density,
            inline_media: settings.inline_media,
        }
    }

    /// Tells measured row heights apart by the look they were drawn in,
    /// for [`super::rows::Heights::for_layout`].
    pub fn key(self) -> u64 {
        egui::Id::new(self).value()
    }

    fn compact(self) -> bool {
        self.density == Density::Compact
    }
}

/// The width of the time in a compact row.
const COMPACT_TIME: f32 = 40.0;
/// The width of the name in a compact row, so the text lines up.
const COMPACT_NAME: f32 = 112.0;
/// How tall a picture waiting for a click is.
const PLACEHOLDER: f32 = 30.0;

/// The id of a message's row, which holds keyboard focus while the message
/// is selected.
pub fn row_id(channel: &str, ts: &crate::model::Ts, in_thread: bool) -> egui::Id {
    egui::Id::new(("message-row", channel, ts.as_str(), in_thread))
}

/// The id of a message's "More" menu, which keeps its toolbar up while it
/// is open.
pub fn more_id(channel: &str, ts: &crate::model::Ts, in_thread: bool) -> egui::Id {
    egui::Id::new(("message-more", channel, ts.as_str(), in_thread))
}

/// A message's text as plain words, with people and channels by name, for
/// the clipboard and screen readers.
pub fn plain_text(workspace: &WorkspaceState, message: &Message) -> String {
    crate::mrkdwn::plain(&message.text, |inline| match inline {
        crate::mrkdwn::Inline::User { id, .. } => Some(format!("@{}", workspace.user_label(id))),
        crate::mrkdwn::Inline::Channel { id, label } => Some(format!(
            "#{}",
            workspace
                .conversation(id)
                .map(|c| c.name.clone())
                .or_else(|| label.clone())
                .unwrap_or_else(|| id.clone())
        )),
        _ => None,
    })
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

/// About how tall a message will be before it is first drawn, for placing
/// it in a long list: close enough that the scroll bar does not lurch when
/// it is drawn and measured.
pub fn guess_height(message: &Message, lead: Lead, look: Look) -> f32 {
    if message.is_system() {
        return if look.compact() { 22.0 } else { 26.0 };
    }
    let mut height = match (look.compact(), lead) {
        (true, _) => 22.0,
        (false, Lead::Full) => 56.0,
        (false, Lead::Compact) => 26.0,
    };
    // About a line per hundred characters (fewer fit beside a compact
    // row's name, but its lines are shorter).
    height += (message.text.len() / 100) as f32 * 20.0;
    // As `file_view` and `attachment_view` size their pictures, before
    // they have loaded; a picture waiting for a click is a short bar.
    let picture = |size: f32| {
        if look.inline_media {
            size
        } else {
            PLACEHOLDER + 4.0
        }
    };
    for file in &message.files {
        height += match file.thumb_size {
            Some([w, h]) if file.is_image() && w > 0.0 && h > 0.0 => {
                let scale = (420.0 / w).min(320.0 / h).min(1.0);
                picture((h * scale).max(24.0) + 4.0)
            }
            _ if file.is_image() => picture(244.0),
            // A still above the card.
            _ if file.poster.is_some() => picture(poster_size(file, 420.0).y + 4.0) + 64.0,
            _ => 64.0,
        };
    }
    for attachment in &message.attachments {
        height += 90.0
            + attachment_media(attachment, 560.0).map_or(0.0, |(_, size)| picture(size.y + 4.0));
    }
    height += message.blocks.len() as f32 * 30.0;
    if !message.reactions.is_empty() {
        height += 32.0;
    }
    if message.reply_count > 0 {
        height += 32.0;
    }
    height
}

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
    let selected = row.selected.filter(|s| s.ts == message.ts);
    let compact = row.look.compact();
    let (top, bottom) = match (compact, lead) {
        (true, Lead::Full) => (3, 1),
        (true, Lead::Compact) => (1, 1),
        (false, Lead::Full) => (8, 3),
        (false, Lead::Compact) => (2, 3),
    };
    let response = egui::Frame::new()
        .inner_margin(Margin {
            left: 16,
            right: 16,
            top,
            bottom,
        })
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            if compact {
                ui.horizontal_top(|ui| {
                    ui.spacing_mut().item_spacing.x = 8.0;
                    compact_lead(ui, row, message, lead, actions);
                    ui.vertical(|ui| {
                        ui.spacing_mut().item_spacing.y = 2.0;
                        if is_editing {
                            edit(ui, row, editing, actions);
                        } else {
                            body(ui, row, message, actions);
                            if selected.is_some() {
                                keys_hint(ui, row, message);
                            }
                        }
                    });
                });
                return;
            }
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
                        if selected.is_some() {
                            keys_hint(ui, row, message);
                        }
                    }
                });
            });
        })
        .response;
    let rect = response.rect;
    if let Some(selected) = selected {
        keyboard_row(ui, row, message, selected, rect);
    }
    let menu_open =
        egui::Popup::is_id_open(ui.ctx(), more_id(row.channel, &message.ts, row.in_thread));
    let hovered =
        (ui.rect_contains_pointer(rect) || selected.is_some() || menu_open) && !is_editing;
    if hovered {
        ui.painter().set(
            background,
            egui::Shape::rect_filled(
                rect,
                CornerRadius::ZERO,
                palette.surface.gamma_multiply(0.6),
            ),
        );
        if lead == Lead::Compact && !compact {
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

/// The selected message: it holds focus so screen readers read it, and
/// shows an accent bar.
fn keyboard_row(
    ui: &mut egui::Ui,
    row: &Row<'_>,
    message: &Message,
    selected: &Selected,
    rect: egui::Rect,
) {
    let palette = row.palette;
    let id = row_id(row.channel, &message.ts, row.in_thread);
    let response = ui.interact(rect, id, Sense::focusable_noninteractive());
    if selected.reveal {
        response.request_focus();
        response.scroll_to_me(None);
    }
    let author = row.workspace.author(message);
    let spoken = tf(
        "{author}, {time}: {text}",
        &[
            ("author", &author),
            ("time", &super::short_time(&message.ts)),
            ("text", &plain_text(row.workspace, message)),
        ],
    );
    theme::describe(&response, egui::WidgetType::Label, &spoken);
    ui.painter().rect_filled(
        egui::Rect::from_min_size(rect.min, Vec2::new(3.0, rect.height())),
        CornerRadius::ZERO,
        palette.accent,
    );
}

/// The keys that act on the selected message, under it.
fn keys_hint(ui: &mut egui::Ui, row: &Row<'_>, message: &Message) {
    let me = message.user.as_deref() == Some(row.workspace.info.user_id.as_str());
    let hint = match (me, row.in_thread) {
        (true, false) => {
            t("↑↓ Move · R React · T Thread · E Edit · Del Delete · C Copy · Esc Back")
        }
        (true, true) => t("↑↓ Move · R React · E Edit · Del Delete · C Copy · Esc Back"),
        (false, false) => t("↑↓ Move · R React · T Thread · C Copy · Esc Back"),
        (false, true) => t("↑↓ Move · R React · C Copy · Esc Back"),
    };
    ui.label(
        RichText::new(hint)
            .font(theme::regular(11.5))
            .color(row.palette.dim),
    );
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

/// A compact row's start, IRC style: the time, then the name in a column
/// of its own so every message's text starts at the same place. A
/// message that continues the one before shows its name dimmed.
fn compact_lead(
    ui: &mut egui::Ui,
    row: &Row<'_>,
    message: &Message,
    lead: Lead,
    actions: &mut Vec<Action>,
) {
    let palette = row.palette;
    // Line the time and name up with the text's first line.
    let line = 20.0;
    let (rect, time) = ui.allocate_exact_size(Vec2::new(COMPACT_TIME, line), Sense::hover());
    ui.painter().text(
        egui::pos2(rect.left(), rect.center().y),
        egui::Align2::LEFT_CENTER,
        super::short_time(&message.ts),
        theme::regular(12.0),
        palette.dim,
    );
    if message.ts.seconds().is_some() {
        time.on_hover_ui(|ui| {
            if let Some(full) = super::full_time(&message.ts) {
                ui.label(full);
            }
        });
    }
    let name = row.workspace.author(message);
    let color = if lead == Lead::Full {
        palette.text
    } else {
        palette.dim
    };
    let mut column = ui.new_child(
        UiBuilder::new()
            .max_rect(egui::Rect::from_min_size(
                ui.cursor().min,
                Vec2::new(COMPACT_NAME, line),
            ))
            .layout(Layout::left_to_right(Align::Center)),
    );
    let response = column
        .add(
            egui::Label::new(RichText::new(&name).font(theme::bold(13.5)).color(color))
                .truncate()
                .sense(Sense::click()),
        )
        .on_hover_cursor(egui::CursorIcon::PointingHand);
    if response.clicked()
        && let Some(user) = &message.user
    {
        actions.push(Action::OpenProfile(user.clone()));
    }
    ui.allocate_exact_size(Vec2::new(COMPACT_NAME, line), Sense::hover());
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
        if message.pinned {
            let (rect, pin) = ui.allocate_exact_size(Vec2::splat(12.0), Sense::hover());
            Icon::Pin.image(palette.dim, 12.0).paint_at(ui, rect);
            pin.on_hover_text(t("Pinned"));
        }
        // The full date only when asked for: formatting it for every
        // message on every frame was wasted work.
        if message.ts.seconds().is_some() {
            time.on_hover_ui(|ui| {
                if let Some(full) = super::full_time(&message.ts) {
                    ui.label(full);
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
        file_view(ui, row, message, file, actions);
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
                RichText::new(tf("Not sent: {error}.", &[("error", error)]))
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
            t("Enter to save, Esc to cancel").into_owned()
        } else {
            tf(
                "{shortcut} to save, Esc to cancel",
                &[("shortcut", &super::keys::command("Enter"))],
            )
        };
        ui.label(
            RichText::new(hint)
                .font(theme::regular(12.0))
                .color(palette.dim),
        );
    });
}

fn file_view(
    ui: &mut egui::Ui,
    row: &Row<'_>,
    message: &Message,
    file: &File,
    actions: &mut Vec<Action>,
) {
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
        if !shows(ui, row, &uri) {
            placeholder(ui, row, &uri, &file.name);
            return;
        }
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
            // In the thread panel the viewer steps through the thread's
            // pictures; a parent is its own thread.
            let thread = row.in_thread.then(|| {
                message
                    .thread_ts
                    .clone()
                    .unwrap_or_else(|| message.ts.clone())
            });
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
        .map(|poster| super::image_uri(team, poster));
    if let Some(uri) = &poster
        && !shows(ui, row, uri)
    {
        placeholder(ui, row, uri, &file.name);
    } else if let Some(uri) = poster {
        let size = poster_size(file, ui.available_width());
        let response = ui
            .add(
                egui::Image::new(uri)
                    .fit_to_exact_size(size)
                    .corner_radius(CornerRadius::same(theme::RADIUS))
                    .show_loading_spinner(true)
                    .sense(Sense::click()),
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
        media_card(ui, row, file, play, actions);
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

/// Where it is remembered that a held-back picture was asked for.
fn reveal_id(uri: &str) -> egui::Id {
    egui::Id::new(("show-picture", uri))
}

/// Whether the picture at `uri` shows: always, unless pictures are held
/// back and this one has not been clicked yet.
fn shows(ui: &egui::Ui, row: &Row<'_>, uri: &str) -> bool {
    row.look.inline_media || ui.data(|d| d.get_temp::<bool>(reveal_id(uri)).unwrap_or(false))
}

/// A short bar standing in for a held-back picture; clicking it shows the
/// picture (and only then is it fetched).
fn placeholder(ui: &mut egui::Ui, row: &Row<'_>, uri: &str, name: &str) {
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
fn fit_within(size: Option<[f32; 2]>, max: Vec2, fallback: Vec2) -> Vec2 {
    let [w, h] = size
        .filter(|[w, h]| *w > 0.0 && *h > 0.0)
        .unwrap_or([fallback.x, fallback.y]);
    let scale = (max.x / w).min(max.y / h).min(1.0);
    Vec2::new(w * scale, h * scale).max(Vec2::splat(24.0))
}

/// How large a video's or PDF's still is shown, in a column `width` wide.
/// A page is shown smaller than a frame: it is there to recognise the
/// document, not to read it.
fn poster_size(file: &File, width: f32) -> Vec2 {
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
fn play_badge(ui: &egui::Ui, center: egui::Pos2, hovered: bool) {
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
    format!("{} · {kind}", super::file_size(file.size))
}

/// A video or sound: a play button that opens it in the system's player,
/// its name, and a download button.
fn media_card(
    ui: &mut egui::Ui,
    row: &Row<'_>,
    file: &File,
    play: Option<String>,
    actions: &mut Vec<Action>,
) {
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
        });
}

/// The large picture an attachment shows under its text.
enum CardMedia<'a> {
    /// A video's thumbnail, with a play button that opens `link`.
    Video { thumb: &'a str, link: &'a str },
    /// A picture whose size Slack gave.
    Image(&'a str),
}

/// What large picture `attachment` shows, and how big, in a card `width`
/// wide. A picture of unknown size is left out: it is sized once loaded.
fn attachment_media(attachment: &Attachment, width: f32) -> Option<(CardMedia<'_>, Vec2)> {
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
fn card_icon(ui: &mut egui::Ui, team: &str, url: &str, round: bool) {
    let radius = if round { 8 } else { 3 };
    ui.add(
        egui::Image::new(super::image_uri(team, url))
            .fit_to_exact_size(Vec2::splat(16.0))
            .corner_radius(CornerRadius::same(radius)),
    );
}

/// A link card or a bot's legacy attachment: the site and author, the
/// title, text and fields, a picture or a video's thumbnail, and a
/// footer, behind a coloured bar.
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
                        blocks_view(ui, row, &attachment.blocks, actions);
                    }
                    let name = attachment.title.as_deref().unwrap_or_default();
                    let picture = attachment
                        .video
                        .as_ref()
                        .and(attachment.thumb.as_deref())
                        .or(attachment.image.as_deref())
                        .map(|url| super::image_uri(team, url));
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
                                        egui::Image::new(super::image_uri(team, thumb))
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
                                let uri = super::image_uri(team, image);
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
                                        egui::Image::new(super::image_uri(team, image))
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
            let spoken = crate::i18n::fill(
                &tn(
                    "{count} reaction with :{emoji}:",
                    "{count} reactions with :{emoji}:",
                    reaction.count,
                ),
                &[("emoji", &reaction.name)],
            );
            theme::focus_ring(ui, &response, palette, 12);
            theme::describe_selected(&response, egui::WidgetType::Button, mine, &spoken);
            // Who reacted, built only while hovered rather than for every
            // reaction on every frame.
            let response = response.on_hover_ui(|ui| {
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
                ui.label(tf(
                    "{names} reacted with :{emoji}:",
                    &[("names", &names.join(", ")), ("emoji", &reaction.name)],
                ));
            });
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
        theme::focus_ring(ui, &add, palette, 12);
        theme::describe(&add, egui::WidgetType::Button, &t("Add reaction"));
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
                        RichText::new(tf(
                            "Last reply {when}",
                            &[("when", &super::relative(latest))],
                        ))
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
    theme::focus_ring(ui, &response, palette, theme::RADIUS_SMALL);
    theme::describe(
        &response,
        egui::WidgetType::Button,
        &tn("{count} reply", "{count} replies", message.reply_count),
    );
    if response.clicked() {
        actions.push(Action::OpenThread {
            channel: row.channel.to_owned(),
            ts: message.ts.clone(),
        });
    }
}

/// Quick reactions, react, reply, copy, edit, delete and a "More" menu
/// (save for later, copy link, pin), over the message's top-right corner.
fn toolbar(
    ui: &mut egui::Ui,
    row: &Row<'_>,
    message: &Message,
    me: bool,
    rect: egui::Rect,
    actions: &mut Vec<Action>,
) {
    let palette = row.palette;
    let quick: std::sync::Arc<Vec<String>> = ui
        .data(|d| d.get_temp(super::quick_reactions_id()))
        .unwrap_or_default();
    let mut buttons = 4 + usize::from(!row.in_thread);
    if me {
        buttons += 2;
    }
    // Two quick reactions fit the bar as it was first measured; each more
    // takes a cell.
    let extra = quick.len().saturating_sub(2) as f32 * 28.0;
    let width = buttons as f32 * 30.0 + 28.0 + 8.0 + extra;
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
        for name in quick.iter().map(String::as_str) {
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
            theme::focus_ring(ui, &response, palette, theme::RADIUS_SMALL);
            theme::describe(
                &response,
                egui::WidgetType::Button,
                &tf("React with :{emoji}:", &[("emoji", name)]),
            );
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
        if theme::icon_button(ui, palette, Icon::SmilePlus, 16.0, &t("Add reaction (R)")).clicked()
        {
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
                &t("Reply in thread (T)"),
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
        if theme::icon_button(ui, palette, Icon::Copy, 16.0, &t("Copy text (C)")).clicked() {
            actions.push(Action::Copy(plain_text(row.workspace, message)));
        }
        let more = theme::icon_button(ui, palette, Icon::Ellipsis, 16.0, &t("More actions"));
        egui::Popup::menu(&more)
            .id(more_id(row.channel, &message.ts, row.in_thread))
            .show(|ui| more_menu(ui, row, message, actions));
        if me {
            if theme::icon_button(ui, palette, Icon::Pencil, 16.0, &t("Edit message (E)")).clicked()
            {
                let channel = row.channel.to_owned();
                let ts = message.ts.clone();
                actions.push(if row.in_thread {
                    Action::StartEditInThread { channel, ts }
                } else {
                    Action::StartEdit { channel, ts }
                });
            }
            if theme::icon_button(ui, palette, Icon::Trash, 16.0, &t("Delete message (Del)"))
                .clicked()
            {
                actions.push(Action::AskDelete {
                    channel: row.channel.to_owned(),
                    ts: message.ts.clone(),
                });
            }
        }
    });
}

/// What a message's "More" menu offers: what is used less often than the
/// toolbar's own buttons.
fn more_menu(ui: &mut egui::Ui, row: &Row<'_>, message: &Message, actions: &mut Vec<Action>) {
    let saved = super::views::is_saved(ui, row.channel, &message.ts);
    let save = if saved {
        t("Remove from Later")
    } else {
        t("Save for later")
    };
    if ui.button(save).clicked() {
        actions.push(Action::Views(crate::views::Action::Save {
            channel: row.channel.to_owned(),
            ts: message.ts.clone(),
            save: !saved,
        }));
        ui.close();
    }
    if ui.button(t("Copy link")).clicked() {
        actions.push(Action::CopyLink {
            channel: row.channel.to_owned(),
            ts: message.ts.clone(),
            thread: message.thread_ts.clone(),
        });
        ui.close();
    }
    let pin = if message.pinned {
        t("Unpin from the conversation")
    } else {
        t("Pin to the conversation")
    };
    if ui.button(pin).clicked() {
        actions.push(Action::Convos(crate::convos::Action::Pin {
            channel: row.channel.to_owned(),
            ts: message.ts.clone(),
            pin: !message.pinned,
        }));
        ui.close();
    }
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
