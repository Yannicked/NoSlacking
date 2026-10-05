//! The desktop integration's controls: the Notifications settings and the
//! notification choices in a conversation's context menu.

use egui::{Margin, RichText};

use crate::app::{App, WorkspaceState};
use crate::i18n::t;
use crate::model::{Action, Conversation};
use crate::notify::Level;
use crate::theme::{self, Palette};

use super::settings::{group, row};

/// The egui memory slot holding the keywords as you type them, so a
/// trailing comma survives until you write the next word.
fn keywords_id() -> egui::Id {
    egui::Id::new("notification-keywords")
}

/// The Notifications group of the settings page.
pub fn settings_group(app: &mut App, ui: &mut egui::Ui, palette: &Palette) {
    group(ui, palette, &t("Notifications"), |ui| {
        let mut on = app.settings.desktop.notifications;
        row(
            ui,
            palette,
            &t("Desktop notifications"),
            &t("For direct messages, mentions and your keywords, while you are not looking."),
            |ui, name| {
                ui.checkbox(&mut on, "").labelled_by(name);
            },
        );
        if on != app.settings.desktop.notifications {
            app.settings.desktop.notifications = on;
            app.settings_changed();
        }
        ui.add_enabled_ui(on, |ui| {
            let mut sound = app.settings.desktop.sound;
            row(ui, palette, &t("Play a sound"), "", |ui, name| {
                ui.checkbox(&mut sound, "").labelled_by(name);
            });
            if sound != app.settings.desktop.sound {
                app.settings.desktop.sound = sound;
                app.settings_changed();
            }
            let label = ui.label(
                RichText::new(t("Keywords"))
                    .font(theme::semibold(13.0))
                    .color(palette.secondary),
            );
            let id = keywords_id();
            let mut text = ui
                .data(|d| d.get_temp::<String>(id))
                .unwrap_or_else(|| app.settings.desktop.keywords_text());
            let response = ui
                .add(
                    egui::TextEdit::singleline(&mut text)
                        .hint_text(t("deploy, outage, your nickname"))
                        .desired_width(f32::INFINITY)
                        .margin(Margin::symmetric(8, 6)),
                )
                .labelled_by(label.id);
            if response.changed() {
                app.settings.desktop.set_keywords_text(&text);
                app.settings_changed();
            }
            if response.has_focus() {
                ui.data_mut(|d| d.insert_temp(id, text));
            } else {
                // Once you leave the field it shows the list as saved.
                ui.data_mut(|d| d.remove::<String>(id));
            }
            ui.label(
                RichText::new(t(
                    "Separated by commas. Each notifies like a mention, in any conversation that is not set to Nothing.",
                ))
                .font(theme::regular(12.5))
                .color(palette.dim),
            );
        });
    });
}

/// The Window group of the settings page: the tray item and closing into
/// it.
pub fn window_group(app: &mut App, ui: &mut egui::Ui, palette: &Palette) {
    group(ui, palette, &t("Window"), |ui| {
        let mut tray = app.settings.desktop.tray;
        let detail = if tray && !app.has_tray() && !app.demo {
            t("This desktop shows no tray item.")
        } else {
            t("An icon in the system tray or menu bar that shows what is unread.")
        };
        row(ui, palette, &t("Show in the tray"), &detail, |ui, name| {
            ui.checkbox(&mut tray, "").labelled_by(name);
        });
        if tray != app.settings.desktop.tray {
            app.set_tray(tray);
        }
        ui.add_enabled_ui(app.has_tray(), |ui| {
            let mut keep = app.settings.desktop.close_to_tray;
            row(
                ui,
                palette,
                &t("Keep running in the tray"),
                &t("Closing the window leaves NoSlacking connected, with notifications. Quit from the tray."),
                |ui, name| {
                    ui.checkbox(&mut keep, "").labelled_by(name);
                },
            );
            if keep != app.settings.desktop.close_to_tray {
                app.settings.desktop.close_to_tray = keep;
                app.settings_changed();
            }
        });
        let mut always = app.settings.desktop.stay_active;
        row(
            ui,
            palette,
            &t("Always show as active"),
            &t(
                "While NoSlacking is connected, even when you are not using it. Browser sign-ins only.",
            ),
            |ui, name| {
                ui.checkbox(&mut always, "").labelled_by(name);
            },
        );
        if always != app.settings.desktop.stay_active {
            app.actions.push(crate::model::Action::People(
                crate::people::Action::StayActive(always),
            ));
        }
        let mut login = app.settings.desktop.start_on_login;
        let detail = if app.has_tray() && app.settings.desktop.close_to_tray {
            t("Starts in the tray, without a window.")
        } else {
            t("Starts with its window open.")
        };
        row(
            ui,
            palette,
            &t("Start when you log in"),
            &detail,
            |ui, name| {
                ui.checkbox(&mut login, "").labelled_by(name);
            },
        );
        if login != app.settings.desktop.start_on_login {
            app.set_start_on_login(login);
        }
    });
}

/// The notification choices for a conversation, for its context menu in
/// the sidebar.
pub fn conversation_menu(
    ui: &mut egui::Ui,
    workspace: &WorkspaceState,
    conversation: &Conversation,
    actions: &mut Vec<Action>,
) {
    let muted = workspace.desktop.is_muted(&conversation.id);
    let label = if muted {
        t("Unmute conversation")
    } else {
        t("Mute conversation")
    };
    if ui.button(label).clicked() {
        actions.push(Action::Mute {
            channel: conversation.id.clone(),
            muted: !muted,
        });
        ui.close();
    }
    let chosen = workspace.desktop.chosen(&conversation.id);
    let default = workspace
        .desktop
        .default_level(&conversation.id, conversation.kind);
    ui.menu_button(t("Notify me about"), |ui| {
        let mut pick = |ui: &mut egui::Ui, level: Option<Level>, label: String| {
            if ui.radio(chosen == level, label).clicked() {
                actions.push(Action::NotifyLevel {
                    channel: conversation.id.clone(),
                    level,
                });
                ui.close();
            }
        };
        pick(
            ui,
            None,
            crate::i18n::tf("Default ({level})", &[("level", &default.label())]),
        );
        ui.separator();
        for level in Level::ALL {
            pick(ui, Some(level), level.label());
        }
    });
}

/// The bell in the sidebar header: shows whether notifications are on, and
/// opens the snooze menu.
pub fn dnd_button(
    ui: &mut egui::Ui,
    palette: &Palette,
    workspace: &WorkspaceState,
    actions: &mut Vec<Action>,
) {
    let now = jiff::Zoned::now();
    let seconds = now.timestamp().as_second();
    let dnd = &workspace.desktop.dnd;
    let tz = now.time_zone().clone();
    let label = |until: i64| crate::dnd::until_label(until, now.date(), &tz);
    let (icon, tint, tip) = match (dnd.snoozed(seconds), dnd.quiet_until(seconds)) {
        (Some(until), _) => (
            theme::Icon::BellOff,
            palette.warning,
            crate::i18n::tf("Notifications paused {until}", &[("until", &label(until))]),
        ),
        (None, Some(until)) => (
            theme::Icon::BellOff,
            palette.warning,
            crate::i18n::tf("Do not disturb {until}", &[("until", &label(until))]),
        ),
        (None, None) => (
            theme::Icon::Bell,
            palette.dim,
            t("Notifications on").into_owned(),
        ),
    };
    let (rect, response) = ui.allocate_exact_size(egui::Vec2::splat(22.0), egui::Sense::click());
    if response.hovered() {
        ui.painter().rect_filled(
            rect,
            egui::CornerRadius::same(theme::RADIUS_SMALL),
            palette.surface_hover,
        );
    }
    icon.image(tint, 14.0).paint_at(
        ui,
        egui::Rect::from_center_size(rect.center(), egui::Vec2::splat(14.0)),
    );
    theme::focus_ring(ui, &response, palette, theme::RADIUS_SMALL);
    theme::describe(&response, egui::WidgetType::Button, &tip);
    let response = response
        .on_hover_cursor(egui::CursorIcon::PointingHand)
        .on_hover_text(&tip);
    egui::Popup::menu(&response).show(|ui| {
        ui.label(
            RichText::new(&tip)
                .font(theme::semibold(12.5))
                .color(palette.dim),
        );
        ui.separator();
        ui.label(
            RichText::new(t("Pause notifications"))
                .font(theme::medium(13.0))
                .color(palette.text),
        );
        for choice in crate::dnd::Snooze::ALL {
            if ui.button(choice.label()).clicked() {
                actions.push(Action::Snooze(Some(choice)));
                ui.close();
            }
        }
        if dnd.snoozed(seconds).is_some() {
            ui.separator();
            if ui.button(t("Resume notifications")).clicked() {
                actions.push(Action::Snooze(None));
                ui.close();
            }
        }
    });
}
