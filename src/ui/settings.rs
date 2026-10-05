//! Preferences: appearance, language, sending, the Slack app, sign-in and
//! the signed-in workspaces.

use egui::{CornerRadius, Margin, RichText, Stroke};

use crate::app::App;
use crate::backend::Socket;
use crate::credentials::AppCredentials;
use crate::i18n::{Locale, t, tf};
use crate::model::Action;
use crate::settings::{Appearance, Density, Redirect};
use crate::theme::{self, Palette};

mod network;
mod spelling;

pub(super) fn group(
    ui: &mut egui::Ui,
    palette: &Palette,
    title: &str,
    add: impl FnOnce(&mut egui::Ui),
) {
    ui.add_space(18.0);
    ui.label(
        RichText::new(title)
            .font(theme::bold(15.0))
            .color(palette.text),
    );
    ui.add_space(6.0);
    egui::Frame::new()
        .fill(palette.surface)
        .stroke(Stroke::new(1.0, palette.outline))
        .corner_radius(CornerRadius::same(theme::RADIUS + 2))
        .inner_margin(Margin::same(16))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.spacing_mut().item_spacing.y = 10.0;
            add(ui);
        });
}

/// A setting: its name and explanation on the left, its control on the
/// right. `add` gets the name's id, so a control without a label of its own
/// can be `labelled_by` it for screen readers.
pub(super) fn row(
    ui: &mut egui::Ui,
    palette: &Palette,
    label: &str,
    detail: &str,
    add: impl FnOnce(&mut egui::Ui, egui::Id),
) {
    ui.horizontal(|ui| {
        let mut name = egui::Id::NULL;
        ui.vertical(|ui| {
            ui.spacing_mut().item_spacing.y = 1.0;
            name = ui
                .label(
                    RichText::new(label)
                        .font(theme::medium(14.0))
                        .color(palette.text),
                )
                .id;
            if !detail.is_empty() {
                ui.label(
                    RichText::new(detail)
                        .font(theme::regular(12.5))
                        .color(palette.dim),
                );
            }
        });
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            add(ui, name);
        });
    });
}

pub fn show(app: &mut App, ui: &mut egui::Ui) {
    let palette = app.palette;
    egui::CentralPanel::default()
        .frame(egui::Frame::new().fill(palette.window))
        .show(ui, |ui| {
            egui::ScrollArea::vertical()
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    egui::Frame::new()
                        .inner_margin(Margin {
                            left: 32,
                            right: 32,
                            top: 24 + theme::titlebar_inset(ui.ctx()) as i8,
                            bottom: 32,
                        })
                        .show(ui, |ui| {
                            ui.set_max_width(680.0);
                            content(app, ui, &palette);
                        });
                });
        });
}

fn content(app: &mut App, ui: &mut egui::Ui, palette: &Palette) {
    ui.horizontal(|ui| {
        ui.label(
            RichText::new(t("Settings"))
                .font(theme::bold(24.0))
                .color(palette.text),
        );
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if theme::secondary_button(ui, palette, &t("Done")).clicked() {
                app.actions.push(Action::HideSettings);
            }
        });
    });

    group(ui, palette, &t("Appearance"), |ui| {
        let current = app.settings.appearance.clone();
        let mut choice = current.clone();
        row(ui, palette, &t("Theme"), "", |ui, name| {
            let label = match &current {
                Appearance::System => t("Follow the system").into_owned(),
                Appearance::Dark => t("Dark").into_owned(),
                Appearance::Light => t("Light").into_owned(),
                Appearance::Custom(name) => fastframe_theme::display_name(name).to_owned(),
            };
            egui::ComboBox::from_id_salt("theme")
                .selected_text(label)
                .width(220.0)
                .show_ui(ui, |ui| {
                    ui.selectable_value(&mut choice, Appearance::System, t("Follow the system"));
                    ui.selectable_value(&mut choice, Appearance::Dark, t("Dark"));
                    ui.selectable_value(&mut choice, Appearance::Light, t("Light"));
                    let themes: Vec<String> = app
                        .catalog
                        .picker_themes()
                        .map(|theme| theme.filename.clone())
                        .collect();
                    if !themes.is_empty() {
                        ui.separator();
                    }
                    for filename in themes {
                        let name = fastframe_theme::display_name(&filename).to_owned();
                        ui.selectable_value(&mut choice, Appearance::Custom(filename), name);
                    }
                })
                .response
                .labelled_by(name);
        });
        if choice != current {
            app.set_appearance(choice);
        }
        ui.horizontal(|ui| {
            ui.label(
                RichText::new(t(
                    "Add your own palettes as JSON files in the themes folder.",
                ))
                .font(theme::regular(12.5))
                .color(palette.dim),
            );
            if ui.link(t("Open folder")).clicked() {
                app.actions.push(Action::OpenFolder(app.dirs.themes()));
            }
        });
        let mut zoom = app.settings.zoom;
        row(ui, palette, &t("Zoom"), "", |ui, name| {
            ui.add(
                egui::Slider::new(&mut zoom, 0.75..=1.75)
                    .step_by(0.05)
                    .fixed_decimals(2),
            )
            .labelled_by(name);
        });
        if (zoom - app.settings.zoom).abs() > f32::EPSILON {
            app.settings.zoom = zoom;
            app.settings_changed();
        }
        let current = crate::i18n::locale();
        let mut locale = current;
        row(ui, palette, &t("Language"), "", |ui, name| {
            egui::ComboBox::from_id_salt("language")
                .selected_text(current.native_name())
                .width(220.0)
                .show_ui(ui, |ui| {
                    for option in Locale::ALL {
                        ui.selectable_value(&mut locale, option, option.native_name());
                    }
                })
                .response
                .labelled_by(name);
        });
        if locale != current {
            app.set_language(locale);
        }
    });

    group(ui, palette, &t("Messages"), |ui| {
        let mut enter = app.settings.enter_sends;
        row(
            ui,
            palette,
            &t("Enter sends"),
            &tf(
                "Off: {shortcut} sends and Enter starts a new line.",
                &[("shortcut", &super::keys::command("Enter"))],
            ),
            |ui, name| {
                ui.checkbox(&mut enter, "").labelled_by(name);
            },
        );
        if enter != app.settings.enter_sends {
            app.settings.enter_sends = enter;
            app.settings_changed();
        }
        row(
            ui,
            palette,
            &t("Keyboard shortcuts"),
            &tf(
                "Every shortcut, also with {shortcut}.",
                &[("shortcut", &super::keys::command("/"))],
            ),
            |ui, name| {
                if theme::secondary_button(ui, palette, &t("Show"))
                    .labelled_by(name)
                    .clicked()
                {
                    app.actions.push(Action::ShowShortcuts);
                }
            },
        );
        let mut density = app.settings.density;
        row(
            ui,
            palette,
            &t("Message density"),
            &t("Compact puts the time, name and text of each message on one line."),
            |ui, name| {
                let label = |density: Density| match density {
                    Density::Comfortable => t("Comfortable"),
                    Density::Compact => t("Compact"),
                };
                egui::ComboBox::from_id_salt("density")
                    .selected_text(label(density))
                    .width(200.0)
                    .show_ui(ui, |ui| {
                        for option in [Density::Comfortable, Density::Compact] {
                            ui.selectable_value(&mut density, option, label(option));
                        }
                    })
                    .response
                    .labelled_by(name);
            },
        );
        if density != app.settings.density {
            app.settings.density = density;
            app.settings_changed();
        }
        let mut inline = app.settings.inline_media;
        row(
            ui,
            palette,
            &t("Show images and previews inline"),
            &t("Off: pictures and link previews wait for a click, and are not fetched before."),
            |ui, name| {
                ui.checkbox(&mut inline, "").labelled_by(name);
            },
        );
        if inline != app.settings.inline_media {
            app.settings.inline_media = inline;
            app.settings_changed();
        }
        let mut sort = app.settings.sidebar_sort;
        row(
            ui,
            palette,
            &t("Sort channels"),
            &t("Within each sidebar section. Direct messages are always newest first."),
            |ui, name| {
                egui::ComboBox::from_id_salt("sidebar-sort")
                    .selected_text(match sort {
                        crate::sidebar::Sort::Name => t("By name"),
                        crate::sidebar::Sort::Recent => t("By recent activity"),
                    })
                    .width(200.0)
                    .show_ui(ui, |ui| {
                        ui.selectable_value(&mut sort, crate::sidebar::Sort::Name, t("By name"));
                        ui.selectable_value(
                            &mut sort,
                            crate::sidebar::Sort::Recent,
                            t("By recent activity"),
                        );
                    })
                    .response
                    .labelled_by(name);
            },
        );
        if sort != app.settings.sidebar_sort {
            app.settings.sidebar_sort = sort;
            app.settings_changed();
        }
        let mut unread_first = app.settings.unread_first;
        row(
            ui,
            palette,
            &t("Unread conversations first"),
            &t("At the top of each sidebar section, mentions and direct messages before the rest."),
            |ui, name| {
                ui.checkbox(&mut unread_first, "").labelled_by(name);
            },
        );
        if unread_first != app.settings.unread_first {
            app.settings.unread_first = unread_first;
            app.settings_changed();
        }
    });

    super::desktop::settings_group(app, ui, palette);
    super::desktop::window_group(app, ui, palette);
    super::hooks::settings_group(app, ui, palette);

    group(ui, palette, &t("Workspaces"), |ui| {
        let workspaces: Vec<(String, String, Option<crate::failure::Failure>)> = app
            .workspaces
            .iter()
            .map(|w| {
                (
                    w.info.team_id.clone(),
                    w.info.name.clone(),
                    w.signed_out.clone(),
                )
            })
            .collect();
        for (team, name, signed_out) in workspaces {
            let detail = signed_out.map(|f| f.message()).unwrap_or_default();
            row(ui, palette, &name, &detail, |ui, _| {
                if theme::secondary_button(ui, palette, &t("Sign out")).clicked() {
                    app.actions.push(Action::SignOut(team.clone()));
                }
            });
        }
        if theme::primary_button(ui, palette, &t("Add a workspace")).clicked() {
            app.actions.push(Action::AddWorkspace);
        }
    });

    group(ui, palette, &t("Slack app"), |ui| {
        let status = match &app.socket {
            Socket::Connected => t("Live updates connected"),
            Socket::Connecting => t("Connecting…"),
            Socket::Off => t("No live connection: messages are fetched every few seconds"),
            Socket::Disconnected(_) => t("Offline, reconnecting"),
            Socket::Rejected(_) => t("Slack refused the app-level token"),
        };
        row(ui, palette, &t("Connection"), &status, |ui, _| {
            if theme::secondary_button(ui, palette, &t("Reconnect")).clicked() {
                app.actions.push(Action::Reconnect);
            }
        });
        for (label, value, secret) in [
            (t("Client ID"), &mut app.setup.client_id, false),
            (
                t("Client secret (optional)"),
                &mut app.setup.client_secret,
                true,
            ),
            (t("App-level token"), &mut app.setup.app_token, true),
        ] {
            let label = ui.label(
                RichText::new(label)
                    .font(theme::semibold(13.0))
                    .color(palette.secondary),
            );
            ui.add(
                egui::TextEdit::singleline(value)
                    .password(secret)
                    .desired_width(f32::INFINITY)
                    .margin(Margin::symmetric(8, 6)),
            )
            .labelled_by(label.id);
        }
        let form = AppCredentials {
            client_id: app.setup.client_id.trim().to_owned(),
            client_secret: app.setup.client_secret.trim().to_owned(),
            app_token: app.setup.app_token.trim().to_owned(),
        };
        let changed = app.app_credentials.as_ref() != Some(&form);
        // As on the sign-in page: an app without a client id cannot sign
        // anyone in, so it is not worth saving. The secret may stay empty.
        ui.add_enabled_ui(changed && form.can_sign_in(), |ui| {
            if theme::primary_button(ui, palette, &t("Save")).clicked() {
                app.actions.push(Action::SaveApp);
            }
        });
        ui.separator();
        let mut redirect = app.settings.redirect;
        row(
            ui,
            palette,
            &t("Sign-in redirect"),
            &t("Must be one of the app's redirect URLs under OAuth & Permissions."),
            |ui, name| {
                egui::ComboBox::from_id_salt("redirect")
                    .selected_text(match redirect {
                        Redirect::Scheme => crate::auth::SCHEME_REDIRECT.to_owned(),
                        Redirect::Loopback => {
                            crate::auth::loopback_redirect(app.settings.loopback_port)
                        }
                    })
                    .width(280.0)
                    .show_ui(ui, |ui| {
                        ui.selectable_value(
                            &mut redirect,
                            Redirect::Scheme,
                            crate::auth::SCHEME_REDIRECT,
                        );
                        ui.selectable_value(
                            &mut redirect,
                            Redirect::Loopback,
                            crate::auth::loopback_redirect(app.settings.loopback_port),
                        );
                    })
                    .response
                    .labelled_by(name);
            },
        );
        // Under the row: next to the menu it would run beneath it.
        ui.label(
            RichText::new(t("The manifest adds the loopback one; add noslacking://oauth/callback there yourself to use it."))
                .font(theme::regular(12.5))
                .color(palette.dim),
        );
        if redirect != app.settings.redirect {
            app.settings.redirect = redirect;
            app.settings_changed();
        }
        if app.settings.redirect == Redirect::Loopback {
            let mut port = app.settings.loopback_port;
            row(ui, palette, &t("Loopback port"), "", |ui, name| {
                ui.add(egui::DragValue::new(&mut port).range(1024..=65535))
                    .labelled_by(name);
            });
            if port != app.settings.loopback_port {
                app.settings.loopback_port = port;
                app.settings_changed();
            }
        }
    });

    spelling::show(app, ui, palette);
    network::show(app, ui, palette);

    group(ui, palette, &t("Files"), |ui| {
        let folders = [
            (t("Settings and themes"), app.dirs.config.clone()),
            (t("Logs"), app.dirs.state.clone()),
            (t("Cache"), app.dirs.cache.clone()),
        ];
        for (label, path) in folders {
            row(ui, palette, &label, &path.display().to_string(), |ui, _| {
                if theme::secondary_button(ui, palette, &t("Open")).clicked() {
                    app.actions.push(Action::OpenFolder(path.clone()));
                }
            });
        }
    });
    ui.add_space(12.0);
    ui.label(
        RichText::new(format!("NoSlacking {}", env!("CARGO_PKG_VERSION")))
            .font(theme::regular(12.0))
            .color(palette.dim),
    );
}
