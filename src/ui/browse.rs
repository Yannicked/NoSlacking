//! Dialogs for starting and finding conversations: "New message", which
//! picks people for a direct message.
//!
//! Shortcuts: Ctrl+N (⌘N) starts a new message.

use egui::{CornerRadius, Key, Margin, Modifiers, RichText, Sense, Vec2};

use crate::app::{App, Page};
use crate::convos::{Action as Convos, MAX_PEOPLE};
use crate::i18n::{t, tf};
use crate::model::Action;
use crate::theme::{self, Icon};

/// The shortcuts for these dialogs, while the main page shows and nothing
/// covers it.
pub fn keys(app: &mut App, ctx: &egui::Context) {
    if app.page != Page::Main || app.overlay_open() || app.workspaces.is_empty() {
        return;
    }
    if ctx.input_mut(|i| i.consume_key(Modifiers::COMMAND, Key::N)) {
        app.actions.push(Action::Convos(Convos::NewMessage));
    }
}

/// The sidebar header's buttons for these dialogs, laid out right to left.
pub fn header_buttons(
    ui: &mut egui::Ui,
    palette: &crate::theme::Palette,
    actions: &mut Vec<Action>,
) {
    let tip = tf(
        "New message ({shortcut})",
        &[("shortcut", &super::keys::command("N"))],
    );
    if theme::icon_button(ui, palette, Icon::SquarePen, 16.0, &tip).clicked() {
        actions.push(Action::Convos(Convos::NewMessage));
    }
}

/// Draws whichever of the dialogs is open.
pub fn show(app: &mut App, ctx: &egui::Context) {
    new_message(app, ctx);
}

/// The frame every dialog here sits in, like the other overlays'.
fn frame(app: &App) -> egui::Frame {
    egui::Frame::new()
        .fill(app.palette.overlay)
        .stroke(egui::Stroke::new(1.0, app.palette.outline))
        .corner_radius(CornerRadius::same(theme::RADIUS + 4))
        .shadow(egui::epaint::Shadow {
            offset: [0, 8],
            blur: 32,
            spread: 0,
            color: app.palette.shadow,
        })
        .inner_margin(Margin::same(16))
}

/// A dialog's heading.
fn heading(ui: &mut egui::Ui, app: &App, text: &str) -> egui::Response {
    ui.label(
        RichText::new(text)
            .font(theme::bold(17.0))
            .color(app.palette.text),
    )
}

/// Picks people for a direct message (one) or a group message (several),
/// with suggestions as you type.
fn new_message(app: &mut App, ctx: &egui::Context) {
    let Some(mut dialog) = app.convos.new_message.take() else {
        return;
    };
    let focus = std::mem::take(&mut app.focus_overlay);
    let palette = app.palette;
    let Some(workspace) = crate::app::active_in(&app.workspaces, &app.settings) else {
        return;
    };
    let found = dialog.suggestions(workspace);
    let (down, up, enter, escape) = ctx.input_mut(|input| {
        (
            input.consume_key(Modifiers::NONE, Key::ArrowDown),
            input.consume_key(Modifiers::NONE, Key::ArrowUp),
            input.consume_key(Modifiers::NONE, Key::Enter),
            input.consume_key(Modifiers::NONE, Key::Escape),
        )
    });
    if !found.is_empty() {
        if down {
            dialog.selected = (dialog.selected + 1) % found.len();
        }
        if up {
            dialog.selected = (dialog.selected + found.len() - 1) % found.len();
        }
        dialog.selected = dialog.selected.min(found.len() - 1);
    }
    let full = dialog.picked.len() >= MAX_PEOPLE;
    let mut pick = None;
    let mut unpick = None;
    let mut go = false;
    let mut close = escape;
    let response = egui::Modal::new(egui::Id::new("new-message"))
        .frame(frame(app))
        .show(ctx, |ui| {
            ui.set_width(440.0);
            let title = heading(ui, app, &t("New message"));
            ui.add_space(6.0);
            if !dialog.picked.is_empty() {
                ui.horizontal_wrapped(|ui| {
                    ui.spacing_mut().item_spacing = Vec2::new(6.0, 6.0);
                    for id in &dialog.picked {
                        let name = workspace.user_label(id);
                        let chip = egui::Frame::new()
                            .fill(palette.surface)
                            .corner_radius(CornerRadius::same(theme::RADIUS_SMALL + 2))
                            .inner_margin(Margin::symmetric(6, 3))
                            .show(ui, |ui| {
                                ui.horizontal(|ui| {
                                    ui.spacing_mut().item_spacing.x = 4.0;
                                    let avatar =
                                        workspace.user(id).and_then(|u| u.avatar.as_deref());
                                    super::avatar(ui, avatar, &name, id, 18.0);
                                    ui.label(RichText::new(&name).color(palette.text));
                                    let tip = tf("Remove {name}", &[("name", &name)]);
                                    if theme::icon_button(ui, &palette, Icon::X, 12.0, &tip)
                                        .clicked()
                                    {
                                        unpick = Some(id.clone());
                                    }
                                });
                            });
                        chip.response.on_hover_text(
                            workspace
                                .user(id)
                                .map(|u| u.name.clone())
                                .unwrap_or_default(),
                        );
                    }
                });
                ui.add_space(6.0);
            }
            let hint = if full {
                tf(
                    "A group message has at most {count} other people",
                    &[("count", &MAX_PEOPLE.to_string())],
                )
            } else if dialog.picked.is_empty() {
                t("Type a name").into_owned()
            } else {
                t("Add someone else").into_owned()
            };
            let field = ui
                .add_enabled(
                    !full && !dialog.busy,
                    egui::TextEdit::singleline(&mut dialog.query)
                        .id(egui::Id::new("new-message-query"))
                        .hint_text(hint)
                        .font(theme::regular(15.0))
                        .desired_width(f32::INFINITY)
                        .margin(Margin::symmetric(10, 7)),
                )
                .labelled_by(title.id);
            if focus || unpick.is_some() {
                field.request_focus();
            }
            // Backspace in an empty field takes back the last person, as
            // in Slack.
            if field.has_focus()
                && dialog.query.is_empty()
                && ui.input(|i| i.key_pressed(Key::Backspace))
            {
                unpick = dialog.picked.last().cloned();
            }
            ui.add_space(6.0);
            let typing = !dialog.query.trim().is_empty();
            if typing || dialog.picked.is_empty() {
                for (index, id) in found.iter().enumerate() {
                    let Some(user) = workspace.user(id) else {
                        continue;
                    };
                    let (rect, row) = ui
                        .allocate_exact_size(Vec2::new(ui.available_width(), 36.0), Sense::click());
                    let highlighted = index == dialog.selected;
                    if highlighted || row.hovered() {
                        ui.painter().rect_filled(
                            rect,
                            CornerRadius::same(theme::RADIUS_SMALL),
                            if highlighted {
                                palette.accent.gamma_multiply(0.25)
                            } else {
                                palette.surface_hover
                            },
                        );
                    }
                    let picture = egui::Rect::from_center_size(
                        egui::pos2(rect.left() + 20.0, rect.center().y),
                        Vec2::splat(24.0),
                    );
                    super::paint_avatar(ui, picture, user.avatar.as_deref(), user.label(), id);
                    let label = ui.painter().text(
                        egui::pos2(rect.left() + 42.0, rect.center().y),
                        egui::Align2::LEFT_CENTER,
                        user.label(),
                        theme::semibold(14.5),
                        palette.text,
                    );
                    let detail = if user.is_bot {
                        t("App").into_owned()
                    } else if !user.real_name.is_empty() && user.real_name != user.label() {
                        user.real_name.clone()
                    } else {
                        format!("@{}", user.name)
                    };
                    ui.painter().text(
                        egui::pos2(label.right() + 8.0, rect.center().y),
                        egui::Align2::LEFT_CENTER,
                        detail,
                        theme::regular(13.0),
                        palette.dim,
                    );
                    theme::describe_selected(
                        &row,
                        egui::WidgetType::SelectableLabel,
                        highlighted,
                        user.label(),
                    );
                    if row
                        .on_hover_cursor(egui::CursorIcon::PointingHand)
                        .clicked()
                    {
                        pick = Some(id.clone());
                    }
                }
                if found.is_empty() && typing {
                    ui.label(RichText::new(t("Nobody by that name.")).color(palette.dim));
                }
            }
            ui.add_space(10.0);
            ui.horizontal(|ui| {
                if theme::secondary_button(ui, &palette, &t("Cancel")).clicked() {
                    close = true;
                }
                let label = if dialog.picked.len() > 1 {
                    t("Start group message")
                } else {
                    t("Open")
                };
                let ready = !dialog.picked.is_empty() && !dialog.busy;
                if ui
                    .add_enabled_ui(ready, |ui| theme::primary_button(ui, &palette, &label))
                    .inner
                    .clicked()
                {
                    go = true;
                }
                if dialog.busy {
                    ui.add(egui::Spinner::new().size(16.0).color(palette.dim));
                }
            });
        });
    if response.should_close() {
        close = true;
    }
    // Enter picks the highlighted person while typing, and opens the
    // conversation once the field is empty again.
    if enter && !dialog.busy {
        if !dialog.query.trim().is_empty() {
            pick = pick.or_else(|| found.get(dialog.selected).cloned());
        } else if !dialog.picked.is_empty() {
            go = true;
        } else {
            pick = pick.or_else(|| found.get(dialog.selected).cloned());
        }
    }
    if let Some(id) = pick {
        dialog.pick(id);
        app.focus_overlay = true;
    }
    if let Some(id) = unpick {
        dialog.picked.retain(|p| *p != id);
    }
    if close {
        return;
    }
    if go {
        let users = dialog.picked.clone();
        app.convos.new_message = Some(dialog);
        app.actions.push(Action::Convos(Convos::Open { users }));
        return;
    }
    app.convos.new_message = Some(dialog);
}
