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

/// The notification choices for a conversation, for its context menu in
/// the sidebar.
pub fn conversation_menu(
    ui: &mut egui::Ui,
    workspace: &WorkspaceState,
    conversation: &Conversation,
    actions: &mut Vec<Action>,
) {
    let chosen = workspace.desktop.chosen(&conversation.id);
    let default = Level::default_for(conversation.kind);
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
