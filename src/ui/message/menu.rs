//! What you can do to a message: the toolbar over its corner, the
//! right-click menu and the "More" menu.

use egui::{Align, CornerRadius, Layout, Margin, Sense, Stroke, UiBuilder, Vec2};

use super::{Row, more_id, plain_text, row_id};
use crate::i18n::{t, tf};
use crate::model::{Action, Message};
use crate::theme::{self, Icon};
use crate::ui::rich::{self, Rich};

/// Quick reactions, react, reply, copy, edit, delete and a "More" menu
/// (save for later, copy link, pin), over the message's top-right corner.
pub(super) fn toolbar(
    ui: &mut egui::Ui,
    row: &Row<'_>,
    message: &Message,
    me: bool,
    rect: egui::Rect,
    actions: &mut Vec<Action>,
) {
    let palette = row.palette;
    let quick: std::sync::Arc<Vec<String>> = ui
        .data(|d| d.get_temp(crate::ui::quick_reactions_id()))
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

/// The id of a message's right-click menu.
pub(super) fn context_id(channel: &str, ts: &crate::model::Ts, in_thread: bool) -> egui::Id {
    row_id(channel, ts, in_thread).with("context")
}

/// Opens the right-click menu when the message is right-clicked, anywhere
/// on it (its text and pictures included), and shows it while open.
pub(super) fn context_menu(
    ui: &mut egui::Ui,
    row: &Row<'_>,
    message: &Message,
    me: bool,
    rect: egui::Rect,
    actions: &mut Vec<Action>,
) {
    use crate::ui::context::Target;
    let id = context_id(row.channel, &message.ts, row.in_thread);
    let target_id = id.with("target");
    let opened = ui.input(|i| i.pointer.secondary_clicked()) && ui.rect_contains_pointer(rect);
    if opened {
        // What was under the pointer when it was clicked, not later.
        let target = crate::ui::context::hovered(ui.ctx());
        ui.data_mut(|d| d.insert_temp(target_id, target));
    }
    let target: Option<Target> = ui.data(|d| d.get_temp(target_id)).flatten();
    egui::Popup::new(
        id,
        ui.ctx().clone(),
        egui::PopupAnchor::PointerFixed,
        ui.layer_id(),
    )
    .kind(egui::PopupKind::Menu)
    .layout(Layout::top_down_justified(Align::Min))
    .style(egui::containers::menu::menu_style)
    .open_memory(opened.then_some(egui::SetOpenCommand::Bool(true)))
    .show(|ui| {
        match &target {
            Some(Target::Link(url)) => {
                if ui.button(t("Open link")).clicked() {
                    actions.push(Action::OpenUrl(url.clone()));
                    ui.close();
                }
                if ui.button(t("Copy link address")).clicked() {
                    actions.push(Action::Copy(url.clone()));
                    ui.close();
                }
                ui.separator();
            }
            Some(Target::Image {
                channel,
                thread,
                ts,
                file,
                name,
                download,
                permalink,
                copy,
            }) => {
                if ui.button(t("Open image")).clicked() {
                    actions.push(Action::ViewImage {
                        channel: channel.clone(),
                        thread: thread.clone(),
                        ts: ts.clone(),
                        file: file.clone(),
                    });
                    ui.close();
                }
                if ui.button(t("Copy image")).clicked() {
                    actions.push(Action::CopyImage(copy.clone()));
                    ui.close();
                }
                if let Some(url) = download
                    && ui.button(t("Save image")).clicked()
                {
                    actions.push(Action::Download {
                        url: url.clone(),
                        name: name.clone(),
                    });
                    ui.close();
                }
                if let Some(page) = permalink
                    && ui.button(t("Open in browser")).clicked()
                {
                    actions.push(Action::OpenUrl(page.clone()));
                    ui.close();
                }
                ui.separator();
            }
            None => {}
        }
        if ui.button(t("Add reaction")).clicked() {
            actions.push(Action::PickReaction {
                channel: row.channel.to_owned(),
                ts: message.ts.clone(),
            });
            ui.close();
        }
        if !row.in_thread && ui.button(t("Reply in thread")).clicked() {
            actions.push(Action::OpenThread {
                channel: row.channel.to_owned(),
                ts: message
                    .thread_ts
                    .clone()
                    .unwrap_or_else(|| message.ts.clone()),
            });
            ui.close();
        }
        if ui.button(t("Copy text")).clicked() {
            actions.push(Action::Copy(plain_text(row.workspace, message)));
            ui.close();
        }
        more_menu(ui, row, message, actions);
        if me {
            ui.separator();
            if ui.button(t("Edit message")).clicked() {
                let channel = row.channel.to_owned();
                let ts = message.ts.clone();
                actions.push(if row.in_thread {
                    Action::StartEditInThread { channel, ts }
                } else {
                    Action::StartEdit { channel, ts }
                });
                ui.close();
            }
            if ui.button(t("Delete message")).clicked() {
                actions.push(Action::AskDelete {
                    channel: row.channel.to_owned(),
                    ts: message.ts.clone(),
                });
                ui.close();
            }
        }
    });
}

/// What a message's "More" menu offers: what is used less often than the
/// toolbar's own buttons.
fn more_menu(ui: &mut egui::Ui, row: &Row<'_>, message: &Message, actions: &mut Vec<Action>) {
    let saved = crate::ui::views::is_saved(ui, row.channel, &message.ts);
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
