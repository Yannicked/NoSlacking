//! What you can do to a message: the toolbar over its corner, the
//! right-click menu and the "More" menu.

use egui::{Align, CornerRadius, Layout, Margin, Sense, Stroke, UiBuilder, Vec2};

use super::{Row, Subject, Verb, more_id, row_id};
use crate::i18n::{t, tf};
use crate::model::{Ability, Action, Message};
use crate::theme::{self, Icon};
use crate::ui::rich::{self, Rich};

/// Quick reactions, react, reply, copy, edit, delete and a "More" menu
/// (save for later, mark unread, copy link, share, pin), over the message's
/// top-right corner.
pub(super) fn toolbar(
    ui: &mut egui::Ui,
    row: &Row<'_>,
    message: &Message,
    me: bool,
    rect: egui::Rect,
    actions: &mut Vec<Action>,
) {
    let palette = row.palette;
    let subject = row.subject(message);
    let has = |verb: Verb| verb.available(&subject);
    let reacts = has(Verb::React);
    let reply = has(Verb::Reply);
    let edit = has(Verb::Edit);
    let quick: std::sync::Arc<Vec<String>> = if reacts {
        ui.data(|d| d.get_temp(crate::ui::quick_reactions_id()))
            .unwrap_or_default()
    } else {
        Default::default()
    };
    let mut buttons = 3 + usize::from(reacts) + usize::from(reply) + usize::from(me);
    if edit {
        buttons += 1;
    }
    // Two quick reactions fit the bar as it was first measured; each more
    // takes a cell, and without reactions their room goes too.
    let extra = if reacts {
        quick.len().saturating_sub(2) as f32 * 28.0
    } else {
        -2.0 * 28.0
    };
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
        let offer = |ui: &mut egui::Ui, verb: Verb, actions: &mut Vec<Action>| {
            let Some(icon) = verb.icon() else { return };
            if has(verb) && theme::icon_button(ui, palette, icon, 16.0, &verb.tooltip()).clicked() {
                actions.push(verb.action(&subject));
            }
        };
        for verb in [Verb::React, Verb::Reply, Verb::Copy] {
            offer(ui, verb, actions);
        }
        let more = theme::icon_button(ui, palette, Icon::Ellipsis, 16.0, &t("More actions"));
        egui::Popup::menu(&more)
            .id(more_id(row.channel, &message.ts, row.in_thread))
            .show(|ui| more_menu(ui, row, message, actions));
        for verb in [Verb::Edit, Verb::Delete] {
            offer(ui, verb, actions);
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
    let offers = |ability| row.workspace.info.offers(ability);
    let files = offers(Ability::Files);
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
                deletable,
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
                if *deletable && files {
                    crate::ui::context::delete_file_item(ui, file, name, actions);
                }
                ui.separator();
            }
            Some(Target::File {
                file,
                name,
                download,
                deletable,
            }) => {
                if let Some(url) = download
                    && ui.button(t("Download")).clicked()
                {
                    actions.push(Action::Download {
                        url: url.clone(),
                        name: name.clone(),
                    });
                    ui.close();
                }
                if *deletable && files {
                    crate::ui::context::delete_file_item(ui, file, name, actions);
                }
                ui.separator();
            }
            None => {}
        }
        let subject = row.subject(message);
        for verb in [Verb::React, Verb::Reply, Verb::Copy] {
            menu_item(ui, verb, &subject, actions);
        }
        more_menu(ui, row, message, actions);
        if me {
            ui.separator();
            menu_item(ui, Verb::Edit, &subject, actions);
            menu_item(ui, Verb::Delete, &subject, actions);
        }
    });
}

/// A menu line doing `verb` to `subject`, if it can be done.
fn menu_item(ui: &mut egui::Ui, verb: Verb, subject: &Subject<'_>, actions: &mut Vec<Action>) {
    if verb.available(subject) && ui.button(verb.label()).clicked() {
        actions.push(verb.action(subject));
        ui.close();
    }
}

/// What a message's "More" menu offers: what is used less often than the
/// toolbar's own buttons.
fn more_menu(ui: &mut egui::Ui, row: &Row<'_>, message: &Message, actions: &mut Vec<Action>) {
    let offers = |ability| row.workspace.info.offers(ability);
    let saved = crate::ui::views::is_saved(ui, row.channel, &message.ts);
    let save = if saved {
        t("Remove from Later")
    } else {
        t("Save for later")
    };
    if offers(Ability::Later) && ui.button(save).clicked() {
        actions.push(Action::Views(crate::views::Action::Save {
            channel: row.channel.to_owned(),
            ts: message.ts.clone(),
            save: !saved,
        }));
        ui.close();
    }
    // A message still on its way has no link for the reminder to carry.
    if !message.ts.is_local() && offers(Ability::Reminders) {
        remind_menu(ui, row, message, actions);
    }
    let subject = row.subject(message);
    menu_item(ui, Verb::MarkUnread, &subject, actions);
    if offers(Ability::Links) && ui.button(t("Copy link")).clicked() {
        actions.push(Action::CopyLink {
            channel: row.channel.to_owned(),
            ts: message.ts.clone(),
            thread: message.thread_ts.clone(),
        });
        ui.close();
    }
    // For what only works in Slack itself, such as an app's buttons with
    // an OAuth sign-in.
    if !message.ts.is_local() && offers(Ability::Cards) && ui.button(t("Open in Slack")).clicked() {
        actions.push(Action::OpenInSlack {
            channel: row.channel.to_owned(),
            ts: message.ts.clone(),
            thread: message.thread_ts.clone(),
        });
        ui.close();
    }
    menu_item(ui, Verb::Share, &subject, actions);
    let pin = if message.pinned {
        t("Unpin from the conversation")
    } else {
        t("Pin to the conversation")
    };
    if offers(Ability::Pins) && ui.button(pin).clicked() {
        actions.push(Action::Convos(crate::convos::Action::Pin {
            channel: row.channel.to_owned(),
            ts: message.ts.clone(),
            pin: !message.pinned,
        }));
        ui.close();
    }
}

/// "Remind me", with the times to be reminded at and one of your own.
fn remind_menu(ui: &mut egui::Ui, row: &Row<'_>, message: &Message, actions: &mut Vec<Action>) {
    use crate::views::{Action as Views, remind::RemindIn};
    ui.menu_button(t("Remind me"), |ui| {
        for when in RemindIn::ALL {
            if ui.button(when.label()).clicked() {
                actions.push(Action::Views(Views::Remind {
                    channel: row.channel.to_owned(),
                    ts: message.ts.clone(),
                    thread: message.thread_ts.clone(),
                    when,
                }));
                ui.close();
            }
        }
        ui.separator();
        if ui.button(t("Custom time…")).clicked() {
            actions.push(Action::Views(Views::AskRemind {
                channel: row.channel.to_owned(),
                ts: message.ts.clone(),
                thread: message.thread_ts.clone(),
            }));
            ui.close();
        }
    });
}
