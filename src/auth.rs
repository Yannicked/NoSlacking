//! Signing in to a workspace with the user's own Slack app: OAuth v2 with
//! PKCE, asking only for user scopes (NoSlacking acts as you, never as a
//! bot).
//!
//! 1. [`Flow::start`] makes the `state` and PKCE verifier and the browser
//!    URL for `https://slack.com/oauth/v2/authorize`.
//! 2. Slack sends the browser back to the redirect URI: the loopback
//!    listener here, or `noslacking://oauth/callback`, which the desktop
//!    hands to a second launch that forwards it (see
//!    [`crate::single_instance`]).
//! 3. [`exchange`] trades the code for the user token with
//!    `oauth.v2.access`.

use base64::Engine as _;
use rand::RngCore as _;
use sha2::Digest as _;
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

use crate::credentials::AppCredentials;
use crate::settings::Redirect;
use crate::slack::{SlackError, Token, client, types};

pub const SCHEME: &str = "noslacking";
pub const SCHEME_REDIRECT: &str = "noslacking://oauth/callback";

/// Everything NoSlacking reads and does, as you. Keep in step with
/// `slack-app-manifest.yaml`.
pub const USER_SCOPES: &[&str] = &[
    "channels:history",
    "channels:read",
    "groups:history",
    "groups:read",
    "im:history",
    "im:read",
    "mpim:history",
    "mpim:read",
    "chat:write",
    "reactions:read",
    "reactions:write",
    "users:read",
    "files:read",
    "files:write",
    "emoji:read",
    "team:read",
];

pub fn loopback_redirect(port: u16) -> String {
    format!("http://127.0.0.1:{port}/callback")
}

/// One sign-in attempt.
#[derive(Clone)]
pub struct Flow {
    pub state: String,
    pub verifier: String,
    pub redirect_uri: String,
    pub url: String,
}

/// Leaves out the PKCE verifier and the state, which together finish the
/// sign-in.
impl std::fmt::Debug for Flow {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Flow")
            .field("redirect_uri", &self.redirect_uri)
            .finish_non_exhaustive()
    }
}

fn random_token(bytes: usize) -> String {
    let mut buffer = vec![0u8; bytes];
    rand::rng().fill_bytes(&mut buffer);
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(buffer)
}

impl Flow {
    pub fn start(app: &AppCredentials, redirect: Redirect, port: u16) -> Self {
        let state = random_token(24);
        let verifier = random_token(48);
        let challenge = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(sha2::Sha256::digest(verifier.as_bytes()));
        let redirect_uri = match redirect {
            Redirect::Scheme => SCHEME_REDIRECT.to_owned(),
            Redirect::Loopback => loopback_redirect(port),
        };
        let url = format!(
            "https://slack.com/oauth/v2/authorize?client_id={}&user_scope={}&redirect_uri={}&state={}&code_challenge={}&code_challenge_method=S256",
            urlencoding::encode(app.client_id.trim()),
            urlencoding::encode(&USER_SCOPES.join(",")),
            urlencoding::encode(&redirect_uri),
            urlencoding::encode(&state),
            urlencoding::encode(&challenge),
        );
        Self {
            state,
            verifier,
            redirect_uri,
            url,
        }
    }
}

/// The code in a redirect, after checking it belongs to this attempt.
pub fn parse_callback(url: &str, expected_state: &str) -> Result<String, String> {
    let query = url.split_once('?').map_or("", |(_, q)| q);
    let query = query.split('#').next().unwrap_or("");
    let mut code = None;
    let mut state = None;
    let mut error = None;
    for pair in query.split('&') {
        let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
        let value =
            urlencoding::decode(value).map_or_else(|_| value.to_owned(), |v| v.into_owned());
        match key {
            "code" => code = Some(value),
            "state" => state = Some(value),
            "error" => error = Some(value),
            _ => {}
        }
    }
    if let Some(error) = error {
        return Err(if error == "access_denied" {
            "Sign-in was cancelled.".to_owned()
        } else {
            format!("Slack refused the sign-in: {error}")
        });
    }
    if state.as_deref() != Some(expected_state) {
        return Err("This sign-in link does not belong to the current attempt.".to_owned());
    }
    code.filter(|c| !c.is_empty())
        .ok_or_else(|| "Slack sent no authorization code.".to_owned())
}

/// Who signed in where, with which token.
#[derive(Clone, Debug)]
pub struct SignedIn {
    pub team_id: String,
    pub user_id: String,
    pub token: Token,
}

pub async fn exchange(
    http: &reqwest::Client,
    app: &AppCredentials,
    flow: &Flow,
    code: &str,
) -> Result<SignedIn, SlackError> {
    let response = http
        .post(format!("{}oauth.v2.access", client::API))
        .form(&[
            ("client_id", app.client_id.trim()),
            ("client_secret", app.client_secret.trim()),
            ("code", code),
            ("redirect_uri", flow.redirect_uri.as_str()),
            ("code_verifier", flow.verifier.as_str()),
        ])
        .send()
        .await
        .map_err(|e| SlackError::Network(e.without_url().to_string()))?;
    let bytes = response
        .bytes()
        .await
        .map_err(|e| SlackError::Network(e.without_url().to_string()))?;
    let access: types::OauthAccess = client::decode(&bytes)?;
    let team_id = access.team.id.clone();
    let user_id = access.authed_user.id.clone();
    let token = client::token_from(access).ok_or_else(|| {
        SlackError::Decode("Slack returned no user token; check the app's user scopes".into())
    })?;
    Ok(SignedIn {
        team_id,
        user_id,
        token,
    })
}

const DONE_PAGE: &str = "<!doctype html><meta charset=utf-8><title>NoSlacking</title>\
<style>body{font:16px system-ui;display:grid;place-items:center;height:90vh;color:#333}</style>\
<p>You can close this tab and go back to NoSlacking.</p>";

/// Waits for Slack to send the browser to the loopback redirect, answers it,
/// and returns the URL it asked for.
pub async fn loopback(port: u16) -> std::io::Result<String> {
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", port)).await?;
    loop {
        let (mut stream, _) = listener.accept().await?;
        let mut buffer = vec![0u8; 8192];
        let read = match tokio::time::timeout(
            std::time::Duration::from_secs(10),
            stream.read(&mut buffer),
        )
        .await
        {
            Ok(Ok(read)) => read,
            _ => continue,
        };
        let request = String::from_utf8_lossy(&buffer[..read]);
        let Some(path) = request
            .lines()
            .next()
            .and_then(|line| line.strip_prefix("GET "))
            .and_then(|rest| rest.split_whitespace().next())
        else {
            continue;
        };
        if !path.starts_with("/callback") {
            let _ = stream
                .write_all(
                    b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                )
                .await;
            continue;
        }
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{DONE_PAGE}",
            DONE_PAGE.len()
        );
        let _ = stream.write_all(response.as_bytes()).await;
        let _ = stream.shutdown().await;
        return Ok(format!("http://127.0.0.1:{port}{path}"));
    }
}

/// Registers `noslacking://` with the desktop so the browser can hand the
/// redirect back. Linux writes a desktop file for this executable; Windows
/// writes the per-user URL protocol keys. macOS needs an app bundle, which
/// declares the scheme in its Info.plist.
pub fn register_scheme() -> Result<(), String> {
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    register_scheme_for(&exe)
}

#[cfg(target_os = "linux")]
fn register_scheme_for(exe: &std::path::Path) -> Result<(), String> {
    use crate::paths::APP_ID;
    // A Flatpak or distro package installs its own desktop file.
    if std::env::var_os("FLATPAK_ID").is_some() {
        return Ok(());
    }
    let applications = directories::BaseDirs::new()
        .ok_or("no home directory")?
        .data_local_dir()
        .join("applications");
    std::fs::create_dir_all(&applications).map_err(|e| e.to_string())?;
    let file = applications.join(format!("{APP_ID}.desktop"));
    let quoted = exe.display().to_string().replace('"', "\\\"");
    let entry = format!(
        "[Desktop Entry]\nType=Application\nName=NoSlacking\nComment=A native Slack client\n\
         Exec=\"{quoted}\" %u\nIcon={APP_ID}\nTerminal=false\nCategories=Network;InstantMessaging;Chat;\n\
         MimeType=x-scheme-handler/{SCHEME};\nStartupWMClass={APP_ID}\n"
    );
    let current = std::fs::read_to_string(&file).unwrap_or_default();
    if current != entry {
        std::fs::write(&file, entry).map_err(|e| e.to_string())?;
        let _ = std::process::Command::new("update-desktop-database")
            .arg(&applications)
            .status();
    }
    std::process::Command::new("xdg-mime")
        .args([
            "default",
            &format!("{APP_ID}.desktop"),
            &format!("x-scheme-handler/{SCHEME}"),
        ])
        .status()
        .map_err(|e| format!("xdg-mime: {e}"))
        .and_then(|status| {
            if status.success() {
                Ok(())
            } else {
                Err("xdg-mime could not register the link handler".into())
            }
        })
}

#[cfg(windows)]
fn register_scheme_for(exe: &std::path::Path) -> Result<(), String> {
    let key = format!(r"HKCU\Software\Classes\{SCHEME}");
    let command = format!("\"{}\" \"%1\"", exe.display());
    for args in [
        vec!["add", &key, "/ve", "/d", "URL:NoSlacking", "/f"],
        vec!["add", &key, "/v", "URL Protocol", "/d", "", "/f"],
    ] {
        run_reg(&args)?;
    }
    let command_key = format!(r"{key}\shell\open\command");
    run_reg(&["add", &command_key, "/ve", "/d", &command, "/f"])
}

#[cfg(windows)]
fn run_reg(args: &[&str]) -> Result<(), String> {
    let status = std::process::Command::new("reg")
        .args(args)
        .status()
        .map_err(|e| e.to_string())?;
    if status.success() {
        Ok(())
    } else {
        Err("reg.exe failed".into())
    }
}

#[cfg(not(any(target_os = "linux", windows)))]
fn register_scheme_for(_exe: &std::path::Path) -> Result<(), String> {
    Err(
        "noslacking:// links need the app bundle on this platform; use the loopback redirect"
            .into(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn app() -> AppCredentials {
        AppCredentials {
            client_id: "123.456".into(),
            client_secret: "secret".into(),
            app_token: String::new(),
        }
    }

    #[test]
    fn the_authorize_url_asks_for_user_scopes_only() {
        let flow = Flow::start(&app(), Redirect::Scheme, 0);
        assert!(
            flow.url
                .starts_with("https://slack.com/oauth/v2/authorize?client_id=123.456&")
        );
        assert!(flow.url.contains("user_scope=channels%3Ahistory%2C"));
        assert!(!flow.url.contains("&scope="));
        assert!(
            flow.url
                .contains("redirect_uri=noslacking%3A%2F%2Foauth%2Fcallback")
        );
        assert!(flow.url.contains("code_challenge_method=S256"));
        assert_ne!(flow.state, Flow::start(&app(), Redirect::Scheme, 0).state);
        let loopback = Flow::start(&app(), Redirect::Loopback, 53682);
        assert_eq!(loopback.redirect_uri, "http://127.0.0.1:53682/callback");
    }

    #[test]
    fn callbacks_must_match_the_attempt() {
        assert_eq!(
            parse_callback("noslacking://oauth/callback?code=abc%2F1&state=s1", "s1"),
            Ok("abc/1".to_owned())
        );
        assert!(parse_callback("noslacking://oauth/callback?code=abc&state=other", "s1").is_err());
        assert_eq!(
            parse_callback(
                "http://127.0.0.1:1/callback?error=access_denied&state=s1",
                "s1"
            ),
            Err("Sign-in was cancelled.".to_owned())
        );
        assert!(parse_callback("noslacking://oauth/callback?state=s1", "s1").is_err());
    }
}
