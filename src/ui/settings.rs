//! Preferences: appearance, language, sending, the Slack app, sign-in and
//! the signed-in workspaces.

use egui::{CornerRadius, Margin, RichText, Stroke};

use crate::app::App;
use crate::credentials::AppCredentials;
use crate::devices::Kind;
use crate::i18n::{Locale, t, tf};
use crate::model::Action;
use crate::model::Socket;
use crate::scopes::Feature;
use crate::settings::{Appearance, Density, Redirect};
use crate::sidebar::{HideInactive, Sort};
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

/// A setting that is on or off: a [`row`] with a checkbox. Returns the
/// value as the checkbox left it.
pub(super) fn toggle(
    ui: &mut egui::Ui,
    palette: &Palette,
    label: &str,
    detail: &str,
    mut on: bool,
) -> bool {
    row(ui, palette, label, detail, |ui, name| {
        ui.checkbox(&mut on, "").labelled_by(name);
    });
    on
}

/// A drop-down of `options`, each worded by `label`, `width` points
/// wide and named by `name` for screen readers. Returns the one picked,
/// or `current` if none was.
pub(super) fn choice<T: Copy + PartialEq>(
    ui: &mut egui::Ui,
    id: &str,
    width: f32,
    name: egui::Id,
    (current, options): (T, &[T]),
    label: impl Fn(T) -> String,
) -> T {
    let mut picked = current;
    egui::ComboBox::from_id_salt(id)
        .selected_text(label(current))
        .width(width)
        .show_ui(ui, |ui| {
            for option in options {
                ui.selectable_value(&mut picked, *option, label(*option));
            }
        })
        .response
        .labelled_by(name);
    picked
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

    appearance(app, ui, palette);
    messages(app, ui, palette);

    super::desktop::settings_group(app, ui, palette);
    super::desktop::window_group(app, ui, palette);
    super::hooks::settings_group(app, ui, palette);

    workspaces(app, ui, palette);

    slack_app(app, ui, palette);
    spelling::show(app, ui, palette);
    network::show(app, ui, palette);

    group(ui, palette, &t("Calls and huddles"), |ui| {
        devices(app, ui, palette);
        #[cfg(feature = "video-helper")]
        hardware_video(app, ui, palette);
    });

    files(app, ui, palette);
    ui.add_space(12.0);
    ui.label(
        RichText::new(format!("NoSlacking {}", env!("CARGO_PKG_VERSION")))
            .font(theme::regular(12.0))
            .color(palette.dim),
    );
}

/// Theme, zoom and language.
fn appearance(app: &mut App, ui: &mut egui::Ui, palette: &Palette) {
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
            app.actions.push(Action::SetAppearance(choice));
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
        app.update_setting(|s| &mut s.zoom, zoom);
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
}

/// How messages are written, shown and listed in the sidebar.
fn messages(app: &mut App, ui: &mut egui::Ui, palette: &Palette) {
    group(ui, palette, &t("Messages"), |ui| {
        let enter = toggle(
            ui,
            palette,
            &t("Enter sends"),
            &tf(
                "Off: {shortcut} sends and Enter starts a new line.",
                &[("shortcut", &super::keys::command("Enter"))],
            ),
            app.settings.enter_sends,
        );
        app.update_setting(|s| &mut s.enter_sends, enter);
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
                let label = |density: Density| {
                    match density {
                        Density::Comfortable => t("Comfortable"),
                        Density::Compact => t("Compact"),
                    }
                    .into_owned()
                };
                density = choice(ui, "density", 200.0, name, (density, &Density::ALL), label);
            },
        );
        app.update_setting(|s| &mut s.density, density);
        let inline = toggle(
            ui,
            palette,
            &t("Show images and previews inline"),
            &t("Off: pictures and link previews wait for a click, and are not fetched before."),
            app.settings.inline_media,
        );
        app.update_setting(|s| &mut s.inline_media, inline);
        let mut sort = app.settings.sidebar_sort;
        row(
            ui,
            palette,
            &t("Sort channels"),
            &t("Within each sidebar section. Direct messages are always newest first."),
            |ui, name| {
                let label = |sort: Sort| {
                    match sort {
                        Sort::Name => t("By name"),
                        Sort::Recent => t("By recent activity"),
                    }
                    .into_owned()
                };
                sort = choice(ui, "sidebar-sort", 200.0, name, (sort, &Sort::ALL), label);
            },
        );
        app.update_setting(|s| &mut s.sidebar_sort, sort);
        let unread_first = toggle(
            ui,
            palette,
            &t("Unread conversations first"),
            &t("At the top of each sidebar section, mentions and direct messages before the rest."),
            app.settings.unread_first,
        );
        app.update_setting(|s| &mut s.unread_first, unread_first);
        let mut hide = app.settings.hide_inactive;
        row(
            ui,
            palette,
            &t("Hide inactive conversations"),
            &t("Under “more” in their section. Unread, starred and open ones stay."),
            |ui, name| {
                let all = &HideInactive::ALL;
                hide = choice(
                    ui,
                    "hide-inactive",
                    200.0,
                    name,
                    (hide, all),
                    HideInactive::label,
                );
            },
        );
        if hide != app.settings.hide_inactive {
            app.actions.push(Action::HideInactive(hide));
        }
    });
}

/// The signed-in workspaces, a way to sign out of each and to add one.
fn workspaces(app: &mut App, ui: &mut egui::Ui, palette: &Palette) {
    group(ui, palette, &t("Workspaces"), |ui| {
        type Row = (
            String,
            String,
            Option<crate::failure::Failure>,
            Vec<Feature>,
        );
        let workspaces: Vec<Row> = app
            .workspaces
            .iter()
            .map(|w| {
                (
                    w.info.team_id.clone(),
                    w.info.name.clone(),
                    w.signed_out.clone(),
                    w.info.lacking(),
                )
            })
            .collect();
        for (team, name, signed_out, lacking) in workspaces {
            let detail = signed_out.map(|f| f.message()).unwrap_or_default();
            row(ui, palette, &name, &detail, |ui, _| {
                if theme::secondary_button(ui, palette, &t("Sign out")).clicked() {
                    app.actions.push(Action::SignOut(team.clone()));
                }
            });
            if !lacking.is_empty() {
                super::login::older_app_note(ui, palette, &lacking, &mut app.actions);
                ui.add_space(6.0);
            }
        }
        if theme::primary_button(ui, palette, &t("Add a workspace")).clicked() {
            app.actions.push(Action::AddWorkspace);
        }
    });
}

/// The Slack app NoSlacking signs in with, and its live connection.
fn slack_app(app: &mut App, ui: &mut egui::Ui, palette: &Palette) {
    // A setup of Microsoft Teams workspaces only has no Slack app to
    // speak of.
    let slack = app.workspaces.is_empty() || app.workspaces.iter().any(|w| !w.info.is_teams());
    if slack {
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
            let port = app.settings.loopback_port;
            row(
                ui,
                palette,
                &t("Sign-in redirect"),
                &t("Must be one of the app's redirect URLs under OAuth & Permissions."),
                |ui, name| {
                    let label = |redirect: Redirect| match redirect {
                        Redirect::Scheme => crate::auth::SCHEME_REDIRECT.to_owned(),
                        Redirect::Loopback => crate::auth::loopback_redirect(port),
                    };
                    let all = &[Redirect::Scheme, Redirect::Loopback];
                    redirect = choice(ui, "redirect", 280.0, name, (redirect, all), label);
                },
            );
            // Under the row: next to the menu it would run beneath it.
            ui.label(
            RichText::new(t("The manifest adds the loopback one; add noslacking://oauth/callback there yourself to use it."))
                .font(theme::regular(12.5))
                .color(palette.dim),
        );
            app.update_setting(|s| &mut s.redirect, redirect);
            if app.settings.redirect == Redirect::Loopback {
                let mut port = app.settings.loopback_port;
                row(ui, palette, &t("Loopback port"), "", |ui, name| {
                    ui.add(egui::DragValue::new(&mut port).range(1024..=65535))
                        .labelled_by(name);
                });
                app.update_setting(|s| &mut s.loopback_port, port);
            }
        });
    }
}

/// Where NoSlacking keeps its files.
fn files(app: &mut App, ui: &mut egui::Ui, palette: &Palette) {
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
}

/// The camera, microphone and speaker huddles use: a picker each (see
/// [`super::devices`]), and why the cameras cannot be listed, if so.
fn devices(app: &mut App, ui: &mut egui::Ui, palette: &Palette) {
    let pickers = super::devices::Pickers {
        chosen: &app.settings.devices,
        lists: &app.devices,
    };
    let mut actions = Vec::new();
    for kind in [Kind::Camera, Kind::Microphone, Kind::Speaker] {
        if !Kind::all().contains(&kind) {
            continue;
        }
        let detail = match kind {
            Kind::Camera => t("What the others see when you turn on video."),
            Kind::Microphone => t("What the others hear when you unmute."),
            Kind::Speaker => t("Where the huddle plays: speakers or headphones."),
        };
        row(
            ui,
            palette,
            &super::devices::kind_label(kind),
            &detail,
            |ui, name| super::devices::picker(ui, palette, kind, pickers, name, &mut actions),
        );
        // Under the row, where there is room for it: no helper, no
        // cameras to list (as turning the camera on says).
        if let Some(failure) = pickers.lists.listing(kind).failure() {
            ui.label(
                RichText::new(failure.sentence())
                    .font(theme::regular(12.5))
                    .color(palette.dim),
            );
        }
    }
    app.actions.append(&mut actions);
}

/// Settings → Huddles → Use the graphics card for video.
#[cfg(feature = "video-helper")]
fn hardware_video(app: &mut App, ui: &mut egui::Ui, palette: &Palette) {
    let hardware = toggle(
        ui,
        palette,
        &t("Use the graphics card for video"),
        &t(
            "When it can: shared screens and cameras in, your camera out. Off: all on the processor.",
        ),
        app.settings.hardware_video,
    );
    if app.update_setting(|s| &mut s.hardware_video, hardware) {
        // Streams (and a camera turned on) from now on; one playing
        // keeps its decoder until its next keyframe after a loss, and our
        // camera its encoder until its size changes.
        crate::huddle_audio::helper::set_gpu(hardware);
    }
}
