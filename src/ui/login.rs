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
use crate::scopes::Feature;
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

/// Where you manage the Slack apps you made.
const APPS_PAGE: &str = "https://api.slack.com/apps";

/// A feature an older app's sign-in lacks, in words.
fn feature_name(feature: Feature) -> String {
    match feature {
        Feature::ReadDnd => t("Do Not Disturb from Slack"),
        Feature::SetDnd => t("snoozing in Slack"),
        Feature::GroupMentions => t("@group mentions"),
        Feature::EditBookmarks => t("bookmark editing"),
    }
    .into_owned()
}

/// Says that your Slack app was made from an older manifest, what that
/// leaves out (`lacking`), and how to update it: copy the new manifest,
/// paste it on the app's App Manifest page, reinstall, sign in again.
pub fn older_app_note(
    ui: &mut egui::Ui,
    palette: &Palette,
    lacking: &[Feature],
    actions: &mut Vec<Action>,
) {
    let features = lacking
        .iter()
        .map(|feature| feature_name(*feature))
        .collect::<Vec<_>>()
        .join(", ");
    ui.label(
        RichText::new(tf(
            "Your Slack app was made from an older manifest, so it lacks the permissions for: {features}.",
            &[("features", &features)],
        ))
        .font(theme::regular(13.0))
        .color(palette.warning),
    );
    ui.label(
        RichText::new(t("To unlock them, open your app on api.slack.com/apps, go to App Manifest, paste the new manifest and save, reinstall the app when Slack asks, then sign in again here."))
            .font(theme::regular(12.5))
            .color(palette.dim),
    );
    ui.horizontal(|ui| {
        if theme::secondary_button(ui, palette, &t("Copy manifest")).clicked() {
            actions.push(Action::Copy(MANIFEST.to_owned()));
        }
        if theme::secondary_button(ui, palette, &t("Open your Slack apps")).clicked() {
            actions.push(Action::OpenUrl(APPS_PAGE.to_owned()));
        }
        if theme::secondary_button(ui, palette, &t("Sign in again")).clicked() {
            actions.push(Action::SignInUpdated);
        }
    });
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
                            keyring_note(app, ui, &palette);
                            ui.add_space(28.0);
                            app_card(app, ui, &palette);
                            ui.add_space(48.0);
                        });
                    });
                });
        });
}

fn header(app: &mut App, ui: &mut egui::Ui, palette: &Palette) {
    ui.with_layout(egui::Layout::top_down(egui::Align::Min), |ui| {
        ui.horizontal(|ui| {
            // The app's own icon, from the same SVG the packages install.
            ui.add(
                egui::Image::new(egui::include_image!(
                    "../../packaging/icons/hicolor/scalable/apps/cloud.yannick.NoSlacking.svg"
                ))
                .fit_to_exact_size(egui::Vec2::splat(40.0)),
            );
            ui.add_space(4.0);
            ui.label(
                RichText::new("NoSlacking")
                    .font(theme::bold(30.0))
                    .color(palette.text),
            );
            if !app.workspaces.is_empty() {
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if theme::secondary_button(ui, palette, &t("Back")).clicked() {
                        app.actions.push(Action::HideSettings);
                    }
                });
            }
        });
        ui.label(
            RichText::new(t(
                "A native Slack client. Sign in with your browser to connect a workspace.",
            ))
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
        ui.label(
            RichText::new(t("Sign in to Slack"))
                .font(theme::semibold(17.0))
                .color(palette.text),
        );
        ui.label(
            RichText::new(t("NoSlacking opens Slack's sign-in page in your browser. Sign in as usual, with your password, an emailed code or single sign-on, and NoSlacking finishes by itself."))
                .font(theme::regular(13.5))
                .color(palette.secondary),
        );
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            ui.add_enabled_ui(!busy(app), |ui| {
                if theme::primary_button(ui, palette, &t("Sign in with your browser")).clicked() {
                    app.actions.push(Action::StartBrowserSignIn);
                }
            });
            sign_in_status(app, ui, palette);
        });
        ui.add_space(2.0);
        link_fallback(app, ui, palette);
    });
}

/// When the browser does not hand the sign-in back: Slack's page still
/// holds the `slack://` link, which can be pasted here instead.
fn link_fallback(app: &mut App, ui: &mut egui::Ui, palette: &Palette) {
    egui::CollapsingHeader::new(
        RichText::new(t("Nothing happened after signing in?"))
            .font(theme::medium(13.0))
            .color(palette.link),
    )
    .id_salt("link-fallback")
    .show(ui, |ui| {
        ui.label(
            RichText::new(t(
                "If your browser asks, let it open NoSlacking. Otherwise paste the slack:// link from Slack's page here: open the page source with Ctrl+U and search for magic-login.",
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
        RichText::new(t("Advanced: sign in with your own Slack app"))
            .font(theme::regular(13.0))
            .color(palette.dim),
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
            if app.settings.older_app && saved {
                older_app_note(ui, palette, &Feature::ALL, &mut app.actions);
            }
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
            // Slack may answer an app made from an older manifest with an
            // error page rather than a redirect; this asks again without
            // the newer scopes.
            if matches!(app.sign_in, Some(SignIn::Waiting(_)))
                && !app.settings.older_app
                && ui.link(t("Slack says the permissions are invalid? Sign in with those of the older manifest")).clicked()
            {
                app.actions.push(Action::SignInOlder);
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
                &[("error", &error.message())],
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

    #[test]
    fn the_manifest_file_grants_every_requested_scope_and_names_its_version() {
        // The file as shipped, not the copy built in.
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("slack-app-manifest.json");
        let text = std::fs::read_to_string(path).expect("readable");
        let manifest: serde_json::Value = serde_json::from_str(&text).expect("valid JSON");
        let granted: Vec<&str> = manifest["oauth_config"]["scopes"]["user"]
            .as_array()
            .expect("user scopes")
            .iter()
            .filter_map(serde_json::Value::as_str)
            .collect();
        for request in [crate::scopes::Request::Full, crate::scopes::Request::Older] {
            for scope in request.scopes() {
                assert!(granted.contains(&scope), "{scope} is not in the manifest");
            }
        }
        for scope in crate::scopes::NEWER {
            assert!(granted.contains(&scope), "{scope}");
        }
        // Slack's description field is at most 140 characters, and says
        // which version an app was made from.
        let description = manifest["display_information"]["description"]
            .as_str()
            .expect("a description");
        assert!(description.len() <= 140);
        assert!(
            description.contains(&format!("manifest v{}", crate::scopes::MANIFEST_VERSION)),
            "{description}"
        );
        assert!(
            manifest["settings"]["event_subscriptions"]["user_events"]
                .as_array()
                .expect("user events")
                .contains(&serde_json::json!("dnd_updated"))
        );
    }
}
