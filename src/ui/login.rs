//! First run and "add a workspace".
//!
//! The quick path reuses your Slack browser session, as wee-slack and msga
//! do: sign in through the browser, or paste the workspace address and the
//! `d` cookie. The advanced path registers your own Slack app for official
//! OAuth and live Socket Mode updates.

use egui::{CornerRadius, Margin, RichText, Stroke};

use crate::app::App;
use crate::backend::SignIn;
use crate::credentials::AppCredentials;
use crate::i18n::{t, tf};
use crate::model::Action;
use crate::theme::{self, Palette};

/// The app NoSlacking asks Slack to create for you (advanced path).
pub const MANIFEST: &str = include_str!("../../slack-app-manifest.json");

/// Slack's "create an app from this manifest" page.
pub fn manifest_url() -> String {
    let compact: serde_json::Value = serde_json::from_str(MANIFEST).unwrap_or_default();
    format!(
        "https://api.slack.com/apps?new_app=1&manifest_json={}",
        urlencoding::encode(&compact.to_string())
    )
}

fn step(ui: &mut egui::Ui, palette: &Palette, number: u8, title: &str, done: bool) {
    ui.horizontal(|ui| {
        let (rect, _) = ui.allocate_exact_size(egui::Vec2::splat(24.0), egui::Sense::hover());
        let fill = if done {
            palette.accent
        } else {
            palette.surface_active
        };
        ui.painter().circle_filled(rect.center(), 12.0, fill);
        ui.painter().text(
            rect.center(),
            egui::Align2::CENTER_CENTER,
            if done {
                "✓".to_owned()
            } else {
                number.to_string()
            },
            theme::bold(12.0),
            if done {
                palette.on_accent
            } else {
                palette.text
            },
        );
        ui.label(
            RichText::new(title)
                .font(theme::bold(16.0))
                .color(palette.text),
        );
    });
}

fn field(
    ui: &mut egui::Ui,
    palette: &Palette,
    label: &str,
    value: &mut String,
    hint: &str,
    secret: bool,
) {
    let label = ui.label(
        RichText::new(label)
            .font(theme::semibold(13.0))
            .color(palette.secondary),
    );
    ui.add(
        egui::TextEdit::singleline(value)
            .password(secret)
            .hint_text(hint)
            .desired_width(f32::INFINITY)
            .margin(Margin::symmetric(8, 6)),
    )
    .labelled_by(label.id);
}

fn card(palette: &Palette) -> egui::Frame {
    egui::Frame::new()
        .fill(palette.surface)
        .stroke(Stroke::new(1.0, palette.outline))
        .corner_radius(CornerRadius::same(theme::RADIUS + 2))
        .inner_margin(Margin::same(20))
}

pub fn show(app: &mut App, ui: &mut egui::Ui) {
    let palette = app.palette;
    egui::CentralPanel::default()
        .frame(egui::Frame::new().fill(palette.window))
        .show(ui, |ui| {
            egui::ScrollArea::vertical()
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    let width = 580.0_f32.min(ui.available_width() - 32.0);
                    let margin = ((ui.available_width() - width) / 2.0).max(16.0);
                    ui.horizontal(|ui| {
                        ui.add_space(margin);
                        ui.vertical(|ui| {
                            ui.set_width(width);
                            ui.add_space(40.0 + theme::titlebar_inset(ui.ctx()));
                            header(app, ui, &palette);
                            session_card(app, ui, &palette);
                            ui.add_space(12.0);
                            app_card(app, ui, &palette);
                            keyring_note(app, ui, &palette);
                            ui.add_space(48.0);
                        });
                    });
                });
        });
}

fn header(app: &mut App, ui: &mut egui::Ui, palette: &Palette) {
    ui.with_layout(egui::Layout::top_down(egui::Align::Min), |ui| {
        ui.horizontal(|ui| {
            ui.label(RichText::new("NoSlacking").font(theme::bold(30.0)).color(palette.text));
            if !app.workspaces.is_empty() {
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if theme::secondary_button(ui, palette, &t("Back")).clicked() {
                        app.actions.push(Action::HideSettings);
                    }
                });
            }
        });
        ui.label(
            RichText::new(t("A native Slack client. Connect a workspace by reusing your browser session, or with your own Slack app."))
                .font(theme::regular(14.5))
                .color(palette.secondary),
        );
        ui.add_space(20.0);
    });
}

fn busy(app: &App) -> bool {
    matches!(app.sign_in, Some(SignIn::Waiting(_) | SignIn::Exchanging))
}

fn session_card(app: &mut App, ui: &mut egui::Ui, palette: &Palette) {
    card(palette).show(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.spacing_mut().item_spacing.y = 8.0;
        step(ui, palette, 1, &t("Sign in with your Slack session"), false);
        ui.label(
            RichText::new(t("Reuse the Slack you are already logged in to in your browser. Nothing to register. New messages arrive live over Slack's session socket."))
                .font(theme::regular(13.5))
                .color(palette.secondary),
        );
        ui.add_space(2.0);
        browser_sign_in(app, ui, palette);
        ui.add_space(6.0);
        ui.label(
            RichText::new(t("Or paste the session cookie"))
                .font(theme::semibold(13.5))
                .color(palette.text),
        );
        field(
            ui,
            palette,
            &t("Workspace address"),
            &mut app.setup.session_workspace,
            "acme.slack.com",
            false,
        );
        field(
            ui,
            palette,
            &t("Session cookie (the d cookie)"),
            &mut app.setup.session_cookie,
            "xoxd-…",
            true,
        );
        egui::CollapsingHeader::new(
            RichText::new(t("How to find the d cookie"))
                .font(theme::medium(13.0))
                .color(palette.link),
        )
        .id_salt("cookie-help")
        .show(ui, |ui| {
            ui.label(
                RichText::new(t(
                    "1. Open app.slack.com in your browser and sign in.\n\
                     2. Open developer tools (F12) → Application (or Storage) → Cookies → https://app.slack.com.\n\
                     3. Copy the value of the cookie named d — it starts with xoxd-.\n\
                     It is a secret: treat it like a password. One cookie covers every workspace you are signed in to.",
                ))
                .font(theme::regular(13.0))
                .color(palette.secondary),
            );
            if ui.link(t("Open app.slack.com")).clicked() {
                app.actions.push(Action::OpenUrl("https://app.slack.com".into()));
            }
        });
        let ready = app.setup.session_cookie.trim().starts_with("xoxd-")
            && !app.setup.session_workspace.trim().is_empty()
            && !busy(app);
        ui.horizontal(|ui| {
            ui.add_enabled_ui(ready, |ui| {
                if theme::primary_button(ui, palette, &t("Sign in")).clicked() {
                    app.actions.push(Action::SignInSession);
                }
            });
            sign_in_status(app, ui, palette);
        });
    });
}

/// Signing in through the browser, as msga does: Slack's page hands its
/// `slack://` link back through the desktop, or offers it to paste here.
fn browser_sign_in(app: &mut App, ui: &mut egui::Ui, palette: &Palette) {
    ui.horizontal(|ui| {
        if theme::secondary_button(ui, palette, &t("Sign in with your browser")).clicked() {
            app.actions.push(Action::StartBrowserSignIn);
        }
    });
    ui.label(
        RichText::new(t(
            "Sign in there as usual; NoSlacking finishes by itself. If your browser asks, let it open NoSlacking. If nothing happens, paste the slack:// link from the page here (open the page source with Ctrl+U and search for magic-login).",
        ))
        .font(theme::regular(13.0))
        .color(palette.secondary),
    );
    field(
        ui,
        palette,
        &t("Sign-in link"),
        &mut app.setup.session_link,
        "slack://…",
        true,
    );
    let ready = app.setup.session_link.trim().starts_with("slack://") && !busy(app);
    ui.horizontal(|ui| {
        ui.add_enabled_ui(ready, |ui| {
            if theme::primary_button(ui, palette, &t("Sign in with the link")).clicked() {
                app.actions.push(Action::SignInLink);
            }
        });
    });
}

fn sign_in_status(app: &App, ui: &mut egui::Ui, palette: &Palette) {
    match &app.sign_in {
        Some(SignIn::Waiting(_) | SignIn::Exchanging) => {
            ui.add(egui::Spinner::new().size(14.0).color(palette.dim));
            ui.label(
                RichText::new(t("Signing in…"))
                    .font(theme::regular(13.0))
                    .color(palette.secondary),
            );
        }
        Some(SignIn::Failed(error)) => {
            ui.label(
                RichText::new(error.sentence())
                    .font(theme::regular(13.0))
                    .color(palette.danger),
            );
        }
        Some(SignIn::Done(name)) => {
            ui.label(
                RichText::new(tf("Signed in to {name}.", &[("name", name)]))
                    .font(theme::regular(13.0))
                    .color(palette.accent),
            );
        }
        None => {}
    }
}

fn app_card(app: &mut App, ui: &mut egui::Ui, palette: &Palette) {
    let saved = app
        .app_credentials
        .as_ref()
        .is_some_and(AppCredentials::can_sign_in);
    let header = egui::CollapsingHeader::new(
        RichText::new(t("Advanced: use your own Slack app"))
            .font(theme::semibold(14.0))
            .color(palette.text),
    )
    .id_salt("app-path")
    .default_open(app.setup.show_app && !saved);
    header.show_unindented(ui, |ui| {
        ui.add_space(6.0);
        ui.label(
            RichText::new(t("Official OAuth sign-in and live Socket Mode updates, through a free Slack app you create. A workspace admin may need to approve it."))
                .font(theme::regular(13.0))
                .color(palette.secondary),
        );
        ui.add_space(8.0);
        card(palette).show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.spacing_mut().item_spacing.y = 8.0;
            step(ui, palette, 1, &t("Create your Slack app from the manifest"), saved);
            ui.horizontal(|ui| {
                if theme::primary_button(ui, palette, &t("Create the app on Slack")).clicked() {
                    app.actions.push(Action::OpenUrl(manifest_url()));
                }
                if theme::secondary_button(ui, palette, &t("Copy manifest")).clicked() {
                    app.actions.push(Action::Copy(MANIFEST.to_owned()));
                }
            });
            ui.label(
                RichText::new(t("After creating it, generate an app-level token with connections:write on Basic Information. The manifest already allows the sign-in redirect http://localhost:53682/callback; to come back through noslacking:// links instead, add noslacking://oauth/callback under OAuth & Permissions yourself and pick it in Settings."))
                    .font(theme::regular(12.5))
                    .color(palette.dim),
            );
            ui.separator();
            step(ui, palette, 2, &t("Paste the app's credentials"), saved);
            field(ui, palette, &t("Client ID"), &mut app.setup.client_id, "1234567890.1234567890", false);
            field(ui, palette, &t("Client secret (optional)"), &mut app.setup.client_secret, &t("Not needed: sign-in uses PKCE"), true);
            field(ui, palette, &t("App-level token"), &mut app.setup.app_token, "xapp-1-…", true);
            let form = AppCredentials {
                client_id: app.setup.client_id.trim().to_owned(),
                client_secret: app.setup.client_secret.trim().to_owned(),
                app_token: app.setup.app_token.trim().to_owned(),
            };
            let changed = app.app_credentials.as_ref() != Some(&form);
            ui.add_enabled_ui(changed && form.can_sign_in(), |ui| {
                if theme::primary_button(ui, palette, &t("Save")).clicked() {
                    app.actions.push(Action::SaveApp);
                }
            });
            ui.separator();
            step(ui, palette, 3, &t("Sign in"), matches!(app.sign_in, Some(SignIn::Done(_))));
            ui.horizontal(|ui| {
                ui.add_enabled_ui(saved && !busy(app), |ui| {
                    if theme::primary_button(ui, palette, &t("Sign in with Slack")).clicked() {
                        app.actions.push(Action::StartSignIn);
                    }
                });
                if busy(app) && theme::secondary_button(ui, palette, &t("Cancel")).clicked() {
                    app.actions.push(Action::CancelSignIn);
                }
                sign_in_status(app, ui, palette);
            });
            if let Some(SignIn::Waiting(url)) = &app.sign_in
                && ui.link(t("Copy the sign-in link")).clicked()
            {
                app.actions.push(Action::Copy(url.clone()));
            }
            egui::CollapsingHeader::new(
                RichText::new(t("Or paste a user token")).font(theme::medium(13.0)).color(palette.secondary),
            )
            .id_salt("manual-token")
            .show(ui, |ui| {
                let label = ui.label(
                    RichText::new(t("User token"))
                        .font(theme::semibold(13.0))
                        .color(palette.secondary),
                );
                ui.horizontal(|ui| {
                    ui.add(
                        egui::TextEdit::singleline(&mut app.setup.user_token)
                            .password(true)
                            .hint_text("xoxp-…")
                            .desired_width(ui.available_width() - 90.0)
                            .margin(Margin::symmetric(8, 6)),
                    )
                    .labelled_by(label.id);
                    let ready = app.setup.user_token.trim().starts_with("xox") && !busy(app);
                    ui.add_enabled_ui(ready, |ui| {
                        if theme::primary_button(ui, palette, &t("Sign in")).clicked() {
                            app.actions.push(Action::PasteToken);
                        }
                    });
                });
            });
        });
    });
}

fn keyring_note(app: &App, ui: &mut egui::Ui, palette: &Palette) {
    if let Some(error) = &app.keyring_error {
        ui.add_space(12.0);
        ui.label(
            RichText::new(tf(
                "The system keyring is unavailable: {error}. NoSlacking keeps tokens only there; unlock it or install a Secret Service provider such as GNOME Keyring or KWallet.",
                &[("error", error)],
            ))
            .font(theme::regular(13.0))
            .color(palette.warning),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_manifest_matches_the_scopes_and_redirect_sign_in_uses() {
        let manifest: serde_json::Value = serde_json::from_str(MANIFEST).expect("valid JSON");
        let scopes: Vec<&str> = manifest["oauth_config"]["scopes"]["user"]
            .as_array()
            .expect("user scopes")
            .iter()
            .filter_map(serde_json::Value::as_str)
            .collect();
        assert_eq!(scopes, crate::auth::USER_SCOPES);
        assert!(manifest["oauth_config"]["scopes"].get("bot").is_none());
        assert_eq!(manifest["settings"]["socket_mode_enabled"], true);
        // Slack takes a non-https redirect only from a PKCE app, and then
        // always rotates its tokens.
        assert_eq!(manifest["oauth_config"]["pkce_enabled"], true);
        assert_eq!(manifest["settings"]["token_rotation_enabled"], true);
        let port = crate::settings::Settings::default().loopback_port;
        assert_eq!(
            manifest["oauth_config"]["redirect_urls"],
            serde_json::json!([crate::auth::loopback_redirect(port)])
        );
        assert_eq!(
            crate::settings::Redirect::default(),
            crate::settings::Redirect::Loopback
        );
        assert!(
            manifest_url().starts_with("https://api.slack.com/apps?new_app=1&manifest_json=%7B")
        );
    }
}
