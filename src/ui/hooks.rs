//! The settings for scripting hooks (see [`crate::hooks`]).

use egui::{Margin, RichText};

use crate::app::App;
use crate::hooks::Hook;
use crate::i18n::{t, tf};
use crate::theme::{self, Palette};

/// "Scripting hooks": turn them on, and list the programs to run.
pub fn settings_group(app: &mut App, ui: &mut egui::Ui, palette: &Palette) {
    let mut hooks = app.settings.hooks.clone();
    super::settings::group(ui, palette, &t("Scripting hooks"), |ui| {
        super::settings::row(
            ui,
            palette,
            &t("Run hooks"),
            &tf(
                "Runs your programs on new messages, with the message as JSON on their input. Each may run for {seconds} seconds.",
                &[("seconds", &crate::hooks::TIMEOUT.as_secs().to_string())],
            ),
            |ui, name| {
                ui.checkbox(&mut hooks.enabled, "").labelled_by(name);
            },
        );
        let mut remove = None;
        for (index, hook) in hooks.list.iter_mut().enumerate() {
            ui.separator();
            hook_row(ui, palette, index, hook, &mut remove);
        }
        if let Some(index) = remove {
            hooks.list.remove(index);
        }
        ui.horizontal(|ui| {
            if theme::secondary_button(ui, palette, &t("Add a hook")).clicked() {
                hooks.list.push(Hook {
                    mentions: true,
                    direct: true,
                    ..Hook::default()
                });
            }
            ui.label(
                RichText::new(t(
                    "Programs run directly, not through a shell. Put quotes around a path with spaces.",
                ))
                .font(theme::regular(12.5))
                .color(palette.dim),
            );
        });
    });
    app.update_setting(|s| &mut s.hooks, hooks);
}

/// One hook: its command line, what it runs for, and a way to remove it.
fn hook_row(
    ui: &mut egui::Ui,
    palette: &Palette,
    index: usize,
    hook: &mut Hook,
    remove: &mut Option<usize>,
) {
    let label = ui.label(
        RichText::new(t("Program"))
            .font(theme::semibold(13.0))
            .color(palette.secondary),
    );
    ui.add(
        egui::TextEdit::singleline(&mut hook.command)
            .id(egui::Id::new(("hook-command", index)))
            .hint_text(t("/path/to/program --flag"))
            .font(egui::TextStyle::Monospace)
            .desired_width(f32::INFINITY)
            .margin(Margin::symmetric(8, 6)),
    )
    .labelled_by(label.id);
    ui.horizontal(|ui| {
        ui.checkbox(&mut hook.mentions, t("Mentions of you"));
        ui.checkbox(&mut hook.direct, t("Direct messages"));
    });
    ui.horizontal(|ui| {
        let label = ui.label(
            RichText::new(t("Keywords"))
                .font(theme::medium(13.0))
                .color(palette.text),
        );
        ui.add(
            egui::TextEdit::singleline(&mut hook.keywords)
                .id(egui::Id::new(("hook-keywords", index)))
                .hint_text(t("deploy, outage"))
                .desired_width(260.0)
                .margin(Margin::symmetric(8, 5)),
        )
        .labelled_by(label.id);
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if theme::secondary_button(ui, palette, &t("Remove")).clicked() {
                *remove = Some(index);
            }
        });
    });
}
