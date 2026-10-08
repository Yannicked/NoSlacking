//! Settings → Spelling: whether the composer marks misspelt words, and in
//! which of the computer's dictionaries.

use egui::RichText;

use super::{group, row};
use crate::app::App;
use crate::i18n::{t, tf};
use crate::model::Action;
use crate::theme::{self, Palette};

pub(super) fn show(app: &mut App, ui: &mut egui::Ui, palette: &Palette) {
    group(ui, palette, &t("Spelling"), |ui| {
        let available = crate::spell::available(&app.dirs.config);
        let mut settings = app.settings.spelling.clone();
        row(
            ui,
            palette,
            &t("Check spelling as you type"),
            &t("Right-click a marked word for spellings."),
            |ui, name| {
                ui.checkbox(&mut settings.enabled, "").labelled_by(name);
            },
        );
        if available.is_empty() {
            ui.label(
                RichText::new(tf(
                    "No dictionaries found. Install Hunspell dictionaries (such as hunspell-en-us) or put a .aff and .dic pair in {folder}.",
                    &[("folder", &app.dirs.config.join("dictionaries").display().to_string())],
                ))
                .font(theme::regular(12.5))
                .color(palette.dim),
            );
        } else {
            let automatic = t("Automatic");
            row(ui, palette, &t("Language"), "", |ui, name| {
                ui.add_enabled_ui(settings.enabled, |ui| {
                    egui::ComboBox::from_id_salt("spelling-language")
                        .selected_text(settings.language.as_deref().unwrap_or(&automatic))
                        .width(200.0)
                        .show_ui(ui, |ui| {
                            ui.selectable_value(&mut settings.language, None, automatic.as_ref());
                            for found in available {
                                ui.selectable_value(
                                    &mut settings.language,
                                    Some(found.tag.clone()),
                                    &found.tag,
                                );
                            }
                        })
                        .response
                        .labelled_by(name);
                });
            });
        }
        if app.update_setting(|s| &mut s.spelling, settings) {
            app.actions.push(Action::ApplySpelling);
        }
    });
}
