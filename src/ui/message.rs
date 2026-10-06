//! One message: avatar, name and time, text, files, cards, reactions and
//! the thread summary, with a hover toolbar.

use egui::{Align, CornerRadius, Layout, Margin, RichText, Sense, Stroke, UiBuilder, Vec2};

use super::rich::{self, Rich};
use crate::app::{Editing, Selected, WorkspaceState};
use crate::i18n::{t, tf, tn};
use crate::model::{Action, Delivery, Message};
use crate::settings::Density;
use crate::theme::{self, Icon, Palette};

mod cards;
mod files;
mod menu;
mod quote;

use cards::{attachment_media, attachment_view, blocks_view};
use files::{file_view, poster_size};
use menu::{context_id, context_menu, toolbar};

pub struct Row<'a> {
    pub palette: &'a Palette,
    pub workspace: &'a WorkspaceState,
    /// Every workspace signed in here, for quoting a link into another.
    pub workspaces: &'a [WorkspaceState],
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
        crate::mrkdwn::Inline::Group { id, label } => {
            Some(workspace.group_label(id, label.as_deref()))
        }
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
    if !is_editing && message.delivery == Delivery::Sent {
        context_menu(ui, row, message, me, rect, actions);
    }
    let menu_open =
        egui::Popup::is_id_open(ui.ctx(), more_id(row.channel, &message.ts, row.in_thread))
            || egui::Popup::is_id_open(
                ui.ctx(),
                context_id(row.channel, &message.ts, row.in_thread),
            );
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
        (true, false) => t(
            "↑↓ Move · R React · T Thread · E Edit · Del Delete · C Copy · S Share · U Mark unread · Esc Back",
        ),
        (true, true) => t("↑↓ Move · R React · E Edit · Del Delete · C Copy · S Share · Esc Back"),
        (false, false) => {
            t("↑↓ Move · R React · T Thread · C Copy · S Share · U Mark unread · Esc Back")
        }
        // Marking unread works on the conversation, not inside a thread.
        (false, true) => t("↑↓ Move · R React · C Copy · S Share · Esc Back"),
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
            rich::message(ui, &rich, message, false, actions);
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
        rich::message(ui, &rich, message, message.edited, actions);
    }
    for file in &message.files {
        file_view(ui, row, message, file, actions);
    }
    for attachment in &message.attachments {
        attachment_view(ui, row, attachment, actions);
    }
    quote::own_quotes(ui, row, message, actions);
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
                RichText::new(tf("Not sent: {error}.", &[("error", &error.message())]))
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
            // Shift+Enter is a new line, as in the composer.
            focused
                && !crate::ui::composer::shift_enter(input)
                && input.consume_key(save_with, egui::Key::Enter),
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn copied_text_names_user_groups() {
        let mut w = WorkspaceState::new(crate::model::Workspace {
            team_id: "T1".into(),
            name: "One".into(),
            domain: String::new(),
            icon: None,
            user_id: "U1".into(),
        });
        w.groups.push(crate::model::UserGroup {
            id: "S1".into(),
            handle: "design".into(),
            name: "Design".into(),
            members: None,
        });
        let message = Message {
            ts: crate::model::Ts::new("1.0"),
            user: None,
            username: None,
            bot_icon: None,
            bot_id: None,
            text: "<!subteam^S1> and <!subteam^S9> and <!subteam^S8|@ops>".into(),
            thread_ts: None,
            reply_count: 0,
            replies_known: false,
            reply_users: Vec::new(),
            latest_reply: None,
            reactions: Vec::new(),
            files: Vec::new(),
            attachments: Vec::new(),
            blocks: Vec::new(),
            edited: false,
            subtype: None,
            delivery: Delivery::Sent,
            broadcast: false,
            pinned: false,
            client_msg_id: None,
        };
        assert_eq!(plain_text(&w, &message), "@design and @S9 and @ops");
    }
}
