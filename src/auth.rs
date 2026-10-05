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
//!
//! Slack accepts a redirect that is not https (`http://localhost`, a
//! custom scheme) only from an app with PKCE turned on, and such an app is a
//! "public client": the code exchange carries the PKCE verifier and a token
//! refresh the refresh token, never the client secret, and the tokens
//! always rotate (refresh tokens last 30 days).

use base64::Engine as _;
use rand::Rng as _;
use sha2::Digest as _;
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

use crate::credentials::AppCredentials;
use crate::failure::Failure;
use crate::settings::Redirect;
use crate::slack::{SlackError, Token, client, types};

pub const SCHEME: &str = "noslacking";
/// Slack's own scheme: its browser sign-in finishes with a `slack://`
/// link, and "Open in Slack" links use it too.
pub const SLACK_SCHEME: &str = "slack";
/// Every scheme NoSlacking can handle, as its desktop file lists them.
/// Being able to is not being the default: `slack://` is only claimed
/// when a sign-in needs it (see [`claim_slack_links`]). macOS takes them
/// from the bundle's Info.plist instead.
#[cfg(target_os = "linux")]
const SCHEMES: [&str; 2] = [SCHEME, SLACK_SCHEME];
pub const SCHEME_REDIRECT: &str = "noslacking://oauth/callback";

/// Everything NoSlacking reads and does, as you. Keep in step with
/// `slack-app-manifest.json`.
pub const USER_SCOPES: &[&str] = &[
    "channels:history",
    "channels:read",
    "channels:write",
    "groups:history",
    "groups:read",
    "groups:write",
    "im:history",
    "im:read",
    "im:write",
    "mpim:history",
    "mpim:read",
    "mpim:write",
    "chat:write",
    "reactions:write",
    "stars:read",
    "stars:write",
    "users:read",
    "files:read",
    "files:write",
    "emoji:read",
    "team:read",
    "search:read",
    "pins:read",
    "pins:write",
    "bookmarks:read",
    "users.profile:write",
    "users:write",
    "reminders:read",
    "reminders:write",
];

/// The redirect URL of the loopback listener. It says `localhost` rather
/// than an address because that is what Slack's manifest accepts and what
/// the bundled manifest lists; [`loopback`] listens on both IPv4 and IPv6
/// so it answers whichever the browser picks.
pub fn loopback_redirect(port: u16) -> String {
    format!("{}/callback", loopback_origin(port))
}

/// `http://localhost:<port>`, the start of every URL the listener hands on.
fn loopback_origin(port: u16) -> String {
    format!("http://localhost:{port}")
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

/// The `code`, `state` and `error` of a redirect URL. A repeated key makes
/// the whole URL suspect, so it yields `None`.
fn callback_params(url: &str) -> Option<[Option<String>; 3]> {
    let query = url.split_once('?').map_or("", |(_, q)| q);
    let query = query.split('#').next().unwrap_or("");
    let mut params: [Option<String>; 3] = [None, None, None];
    for pair in query.split('&') {
        let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
        let slot = match key {
            "code" => 0,
            "state" => 1,
            "error" => 2,
            _ => continue,
        };
        if params[slot].is_some() {
            return None;
        }
        let value =
            urlencoding::decode(value).map_or_else(|_| value.to_owned(), |v| v.into_owned());
        params[slot] = Some(value);
    }
    Some(params)
}

/// Whether a redirect URL carries this attempt's `state`, so the loopback
/// listener can ignore requests that are not Slack's answer.
pub fn belongs_to(url: &str, expected_state: &str) -> bool {
    callback_params(url).is_some_and(|[_, state, _]| state.as_deref() == Some(expected_state))
}

/// The code in a redirect, after checking it belongs to this attempt.
pub fn parse_callback(url: &str, expected_state: &str) -> Result<String, Failure> {
    let [code, state, error] = callback_params(url).ok_or(Failure::ForeignLink)?;
    // The state comes first: a link without it must not be able to end, or
    // even cancel, the attempt.
    if state.as_deref() != Some(expected_state) {
        return Err(Failure::ForeignLink);
    }
    if let Some(error) = error {
        return Err(if error == "access_denied" {
            Failure::Cancelled
        } else {
            Failure::Refused(error)
        });
    }
    code.filter(|c| !c.is_empty()).ok_or(Failure::NoCode)
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
        // No client secret: a PKCE app is a public client, and the verifier
        // proves this is the attempt that asked for the code.
        .form(&[
            ("client_id", app.client_id.trim()),
            ("code", code),
            ("redirect_uri", flow.redirect_uri.as_str()),
            ("code_verifier", flow.verifier.as_str()),
        ])
        .send()
        .await?;
    let bytes = response.bytes().await?;
    let access: types::OauthAccess = client::decode(&bytes)?;
    let team_id = access.team.id.clone();
    let user_id = access.authed_user.id.clone();
    let token = client::token_from(access).ok_or(SlackError::NoUserToken)?;
    Ok(SignedIn {
        team_id,
        user_id,
        token,
    })
}

const DONE_PAGE: &str = "<!doctype html><meta charset=utf-8><title>NoSlacking</title>\
<style>body{font:16px system-ui;display:grid;place-items:center;height:90vh;color:#333}</style>\
<p>You can close this tab and go back to NoSlacking.</p>";

/// The loopback redirect's listeners, bound before the browser opens so
/// Slack's answer always has somewhere to go.
#[derive(Debug)]
pub struct Loopback {
    port: u16,
    v4: Option<tokio::net::TcpListener>,
    v6: Option<tokio::net::TcpListener>,
}

/// Listens on `port` of 127.0.0.1 and of ::1. Must run inside the tokio
/// runtime, which takes the listeners over.
pub fn bind_loopback(port: u16) -> std::io::Result<Loopback> {
    let bind = |address: std::net::SocketAddr| {
        let listener = std::net::TcpListener::bind(address)?;
        listener.set_nonblocking(true)?;
        tokio::net::TcpListener::from_std(listener)
    };
    let (v4, v6) = either_family(
        bind((std::net::Ipv4Addr::LOCALHOST, port).into()),
        bind((std::net::Ipv6Addr::LOCALHOST, port).into()),
    )?;
    Ok(Loopback { port, v4, v6 })
}

/// Whether a failed bind means someone else holds the port, rather than
/// the address family being missing (no IPv6 on this machine).
fn port_taken(error: &std::io::Error) -> bool {
    matches!(
        error.kind(),
        std::io::ErrorKind::AddrInUse | std::io::ErrorKind::PermissionDenied
    )
}

/// The listeners to use, from the binds of both families. Browsers resolve
/// `localhost` to 127.0.0.1 or ::1 as they like, so a family the machine
/// lacks may be left out, but one whose port someone else holds may not:
/// the browser could take Slack's answer, code and all, to that program.
fn either_family<L>(
    v4: std::io::Result<L>,
    v6: std::io::Result<L>,
) -> std::io::Result<(Option<L>, Option<L>)> {
    match (v4, v6) {
        (Ok(v4), Ok(v6)) => Ok((Some(v4), Some(v6))),
        (Ok(_), Err(error)) | (Err(error), Ok(_)) if port_taken(&error) => Err(error),
        (Ok(v4), Err(_)) => Ok((Some(v4), None)),
        (Err(_), Ok(v6)) => Ok((None, Some(v6))),
        (Err(v4), Err(v6)) => Err(if port_taken(&v6) { v6 } else { v4 }),
    }
}

/// Waits for Slack to send the browser to the loopback redirect carrying
/// `state`, answers it, and returns the URL it asked for. Any other request
/// (a stray page, an old redirect) is answered and ignored, so it cannot
/// end the attempt.
pub async fn loopback(listeners: Loopback, state: &str) -> std::io::Result<String> {
    let Loopback { port, v4, v6 } = listeners;
    loop {
        let (mut stream, _) = accept_either(v4.as_ref(), v6.as_ref()).await?;
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
        let ours = path == "/callback" || path.starts_with("/callback?");
        let url = format!("{}{path}", loopback_origin(port));
        if !ours || !belongs_to(&url, state) {
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
        return Ok(url);
    }
}

/// The next connection on whichever listener gets one first.
async fn accept_either(
    v4: Option<&tokio::net::TcpListener>,
    v6: Option<&tokio::net::TcpListener>,
) -> std::io::Result<(tokio::net::TcpStream, std::net::SocketAddr)> {
    match (v4, v6) {
        (Some(v4), Some(v6)) => tokio::select! {
            accepted = v4.accept() => accepted,
            accepted = v6.accept() => accepted,
        },
        (Some(only), None) | (None, Some(only)) => only.accept().await,
        (None, None) => Err(std::io::Error::other("no loopback listener")),
    }
}

/// Registers `noslacking://` with the desktop so the browser can hand
/// sign-in links back. Linux writes a desktop file for this executable;
/// Windows writes the per-user URL protocol keys. On macOS the app
/// bundle's Info.plist declares the schemes, but the links arrive as an
/// Apple event the app cannot receive without `unsafe` AppKit code, so this
/// fails there (see CONTRIBUTING.md).
///
/// Safe to run at every start: the scheme is NoSlacking's own.
pub fn register_scheme() -> Result<(), String> {
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    register_scheme_for(&exe, &[SCHEME])
}

/// Like [`register_scheme`], and makes NoSlacking the handler of
/// `slack://` links too, taking them over from the official Slack app if
/// it is installed. Only for a browser sign-in the user started, which
/// finishes with such a link; never at start-up.
pub fn claim_slack_links() -> Result<(), String> {
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    register_scheme_for(&exe, &[SCHEME, SLACK_SCHEME])
}

/// Writes the desktop file and makes it the default for `schemes`.
#[cfg(target_os = "linux")]
fn register_scheme_for(exe: &std::path::Path, schemes: &[&str]) -> Result<(), String> {
    use crate::paths::APP_ID;
    // A Flatpak or distro package installs its own desktop file.
    if std::env::var_os("FLATPAK_ID").is_some() {
        return Ok(());
    }
    let data = directories::BaseDirs::new()
        .ok_or("no home directory")?
        .data_local_dir()
        .to_owned();
    // The desktop file names its icon; without one installed, the taskbar
    // shows a blank square (Wayland finds windows' icons only this way).
    if let Err(error) = install_icons(&data) {
        log::warn!("could not install the app icon: {error}");
    }
    let applications = data.join("applications");
    std::fs::create_dir_all(&applications).map_err(|e| e.to_string())?;
    let file = applications.join(format!("{APP_ID}.desktop"));
    let exec = crate::autostart::exec_quote(&exe.display().to_string());
    let entry = format!(
        "[Desktop Entry]\nType=Application\nName=NoSlacking\nComment=A native Slack client\n\
         Exec={exec} %u\nIcon={APP_ID}\nTerminal=false\nCategories=Network;InstantMessaging;Chat;\n\
         MimeType={mime}\nStartupWMClass={APP_ID}\n",
        mime = SCHEMES
            .iter()
            .map(|scheme| format!("x-scheme-handler/{scheme};"))
            .collect::<String>(),
    );
    let current = std::fs::read_to_string(&file).unwrap_or_default();
    if current != entry {
        std::fs::write(&file, entry).map_err(|e| e.to_string())?;
        let _ = std::process::Command::new("update-desktop-database")
            .arg(&applications)
            .status();
    }
    let mut args = vec!["default".to_owned(), format!("{APP_ID}.desktop")];
    args.extend(
        schemes
            .iter()
            .map(|scheme| format!("x-scheme-handler/{scheme}")),
    );
    std::process::Command::new("xdg-mime")
        .args(&args)
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

/// The app's icons as the icon theme wants them, under the user's data
/// folder: the SVG for desktops that scale it, and a PNG for those that
/// take only bitmaps.
#[cfg(target_os = "linux")]
fn icon_files() -> [(String, &'static [u8]); 2] {
    use crate::paths::APP_ID;
    [
        (
            format!("icons/hicolor/scalable/apps/{APP_ID}.svg"),
            include_bytes!("../packaging/icons/hicolor/scalable/apps/cloud.yannick.NoSlacking.svg"),
        ),
        (
            format!("icons/hicolor/256x256/apps/{APP_ID}.png"),
            include_bytes!("../packaging/icons/hicolor/256x256/apps/cloud.yannick.NoSlacking.png"),
        ),
    ]
}

/// Writes [`icon_files`] under `data`, leaving files that already hold the
/// same bytes alone, so a start-up writes nothing once they are there.
#[cfg(target_os = "linux")]
fn install_icons(data: &std::path::Path) -> Result<(), String> {
    let mut wrote = false;
    for (relative, bytes) in icon_files() {
        let path = data.join(relative);
        if std::fs::read(&path).is_ok_and(|current| current == bytes) {
            continue;
        }
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        std::fs::write(&path, bytes).map_err(|e| e.to_string())?;
        wrote = true;
    }
    if wrote {
        // GTK desktops read a cache when there is one; a missing tool or
        // index only means they look the icon up without it.
        let _ = std::process::Command::new("gtk-update-icon-cache")
            .args(["--quiet", "--ignore-theme-index"])
            .arg(data.join("icons/hicolor"))
            .status();
    }
    Ok(())
}

/// Writes the per-user URL protocol keys for `schemes`.
#[cfg(windows)]
fn register_scheme_for(exe: &std::path::Path, schemes: &[&str]) -> Result<(), String> {
    let command = format!("\"{}\" \"%1\"", exe.display());
    for scheme in schemes {
        let key = format!(r"HKCU\Software\Classes\{scheme}");
        for args in [
            vec!["add", &key, "/ve", "/d", "URL:NoSlacking", "/f"],
            vec!["add", &key, "/v", "URL Protocol", "/d", "", "/f"],
        ] {
            run_reg(&args)?;
        }
        let command_key = format!(r"{key}\shell\open\command");
        run_reg(&["add", &command_key, "/ve", "/d", &command, "/f"])?;
    }
    Ok(())
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
fn register_scheme_for(_exe: &std::path::Path, _schemes: &[&str]) -> Result<(), String> {
    Err(
        "this platform does not hand noslacking:// links to the app yet; use the loopback redirect"
            .into(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(target_os = "linux")]
    #[test]
    fn the_icon_goes_where_the_desktop_file_looks() {
        let dir = crate::paths::TestDir::new("auth-icons");
        install_icons(&dir.0).expect("installed");
        let svg = dir
            .0
            .join("icons/hicolor/scalable/apps/cloud.yannick.NoSlacking.svg");
        let png = dir
            .0
            .join("icons/hicolor/256x256/apps/cloud.yannick.NoSlacking.png");
        assert!(std::fs::read(&svg).expect("svg").starts_with(b"<?xml"));
        assert!(std::fs::read(&png).expect("png").starts_with(b"\x89PNG"));
        // A second start finds them in place and leaves them alone.
        let before = std::fs::metadata(&png)
            .and_then(|m| m.modified())
            .expect("time");
        install_icons(&dir.0).expect("installed again");
        let after = std::fs::metadata(&png)
            .and_then(|m| m.modified())
            .expect("time");
        assert_eq!(before, after);
    }

    fn app() -> AppCredentials {
        AppCredentials {
            client_id: "123.456".into(),
            client_secret: String::new(),
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
        assert_eq!(loopback.redirect_uri, "http://localhost:53682/callback");
        assert!(
            loopback
                .url
                .contains("redirect_uri=http%3A%2F%2Flocalhost%3A53682%2Fcallback")
        );
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
            Err(Failure::Cancelled)
        );
        assert_eq!(
            parse_callback("noslacking://oauth/callback?state=s1", "s1"),
            Err(Failure::NoCode)
        );
        assert_eq!(
            parse_callback("x://cb?error=invalid_scope&state=s1", "s1"),
            Err(Failure::Refused("invalid_scope".into()))
        );
    }

    #[test]
    fn a_port_someone_else_holds_stops_the_sign_in() {
        use std::io::{Error, ErrorKind};
        let failed = |kind| -> std::io::Result<u8> { Err(Error::from(kind)) };
        assert!(matches!(
            either_family(Ok(4), Ok(6)),
            Ok((Some(4), Some(6)))
        ));
        // No IPv6 (or no IPv4) on this machine: the other family will do.
        assert!(matches!(
            either_family(Ok(4), failed(ErrorKind::AddrNotAvailable)),
            Ok((Some(4), None))
        ));
        assert!(matches!(
            either_family(failed(ErrorKind::Unsupported), Ok(6)),
            Ok((None, Some(6)))
        ));
        // Another program on either family could catch the browser.
        for kind in [ErrorKind::AddrInUse, ErrorKind::PermissionDenied] {
            assert_eq!(
                either_family(Ok(4), failed(kind))
                    .map(|_| ())
                    .map_err(|e| e.kind()),
                Err(kind)
            );
            assert_eq!(
                either_family(failed(kind), Ok(6))
                    .map(|_| ())
                    .map_err(|e| e.kind()),
                Err(kind)
            );
        }
        assert_eq!(
            either_family(
                failed(ErrorKind::AddrNotAvailable),
                failed(ErrorKind::AddrInUse)
            )
            .map(|_| ())
            .map_err(|e| e.kind()),
            Err(ErrorKind::AddrInUse)
        );
    }

    #[test]
    fn foreign_links_cannot_end_the_attempt() {
        // An error without the state is not Slack's answer.
        assert_eq!(
            parse_callback("http://127.0.0.1:1/callback?error=access_denied", "s1"),
            Err(Failure::ForeignLink)
        );
        // Neither is a link that names a key twice.
        assert!(parse_callback("x://cb?code=a&state=s1&state=s1", "s1").is_err());
        assert!(parse_callback("x://cb?code=a&code=b&state=s1", "s1").is_err());
        assert!(belongs_to(
            "http://127.0.0.1:1/callback?code=a&state=s1",
            "s1"
        ));
        assert!(!belongs_to("http://127.0.0.1:1/callback?state=x", "s1"));
        assert!(!belongs_to("http://127.0.0.1:1/callback", "s1"));
    }
}
