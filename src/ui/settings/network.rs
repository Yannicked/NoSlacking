//! Settings → Network: which proxy NoSlacking goes through.

use egui::{Margin, RichText};

use super::{group, row};
use crate::app::App;
use crate::i18n::t;
use crate::model::Action;
use crate::settings::{ProxyError, ProxyMode, parse_manual};
use crate::theme::{self, Palette};

/// The proxy choice and, for a manual proxy, its URL. A mode applies at
/// once; a typed URL applies with Apply (or Enter), so half a URL never
/// cuts the connection.
pub(super) fn show(app: &mut App, ui: &mut egui::Ui, palette: &Palette) {
    group(ui, palette, &t("Network"), |ui| {
        let label = |mode: ProxyMode| match mode {
            ProxyMode::System => t("Use system proxy"),
            ProxyMode::Direct => t("No proxy"),
            ProxyMode::Manual => t("Manual"),
        };
        // Manual, picked before its URL is good enough to apply.
        let picked_id = egui::Id::new("proxy-picked-manual");
        let mut picked_manual = ui.data(|data| data.get_temp::<bool>(picked_id).unwrap_or(false));
        let shown = if picked_manual {
            ProxyMode::Manual
        } else {
            app.settings.proxy.mode
        };
        let mut mode = shown;
        row(
            ui,
            palette,
            &t("Proxy"),
            &t(
                "The system proxy follows the HTTPS_PROXY and NO_PROXY variables, and on macOS and Windows the system's settings.",
            ),
            |ui, name| {
                egui::ComboBox::from_id_salt("proxy-mode")
                    .selected_text(label(mode))
                    .width(200.0)
                    .show_ui(ui, |ui| {
                        for option in [ProxyMode::System, ProxyMode::Direct, ProxyMode::Manual] {
                            ui.selectable_value(&mut mode, option, label(option));
                        }
                    })
                    .response
                    .labelled_by(name);
            },
        );
        let draft_id = egui::Id::new("proxy-url-draft");
        let mut draft = ui.data_mut(|data| {
            data.get_temp_mut_or_insert_with(draft_id, || app.settings.proxy.url.clone())
                .clone()
        });
        if mode != shown {
            picked_manual = mode == ProxyMode::Manual;
            ui.data_mut(|data| data.insert_temp(picked_id, picked_manual));
            // A manual proxy without a working URL waits for one before it
            // replaces the setting that works now.
            if mode != ProxyMode::Manual || parse_manual(&draft).is_ok() {
                app.settings.proxy.mode = mode;
                if mode == ProxyMode::Manual {
                    app.settings.proxy.url = draft.trim().to_owned();
                }
                app.settings_changed();
                app.actions.push(Action::ApplyProxy);
            }
        }
        if app.settings.proxy.mode != ProxyMode::Manual && !picked_manual {
            return;
        }
        let name = ui.label(
            RichText::new(t("Proxy URL"))
                .font(theme::semibold(13.0))
                .color(palette.secondary),
        );
        let checked = parse_manual(&draft);
        let mut apply = false;
        ui.horizontal(|ui| {
            let field = ui
                .add(
                    egui::TextEdit::singleline(&mut draft)
                        .hint_text("http://proxy.example:3128")
                        .desired_width(ui.available_width() - 90.0)
                        .margin(Margin::symmetric(8, 6)),
                )
                .labelled_by(name.id);
            let entered =
                field.lost_focus() && ui.input(|input| input.key_pressed(egui::Key::Enter));
            let changed = app.settings.proxy.mode != ProxyMode::Manual
                || draft.trim() != app.settings.proxy.url;
            ui.add_enabled_ui(changed && checked.is_ok(), |ui| {
                if theme::primary_button(ui, palette, &t("Apply")).clicked() {
                    apply = true;
                }
            });
            apply |= entered && changed && checked.is_ok();
        });
        if let Err(error) = checked
            && !draft.trim().is_empty()
        {
            ui.label(
                RichText::new(describe(error))
                    .font(theme::regular(12.5))
                    .color(palette.warning),
            );
        }
        ui.label(
            RichText::new(t("http:// or socks5:// (socks5h:// to let the proxy look up names). A user name may go in the URL; a password cannot be saved."))
                .font(theme::regular(12.5))
                .color(palette.dim),
        );
        if apply {
            app.settings.proxy.mode = ProxyMode::Manual;
            app.settings.proxy.url = draft.trim().to_owned();
            app.settings_changed();
            app.actions.push(Action::ApplyProxy);
        }
        ui.data_mut(|data| data.insert_temp(draft_id, draft));
    });
}

/// The interface's words for a URL that will not do.
fn describe(error: ProxyError) -> String {
    let text = match error {
        ProxyError::Empty => t("Enter a proxy URL."),
        ProxyError::Invalid => t("That is not a proxy URL with a host and port."),
        ProxyError::Scheme => t("The proxy URL must start with http://, socks5:// or socks5h://."),
        ProxyError::Password => t("A proxy password cannot be saved; leave it out of the URL."),
    };
    text.into_owned()
}
