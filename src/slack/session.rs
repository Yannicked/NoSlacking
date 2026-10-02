//! Signing in with your own Slack browser session, the way wee-slack and
//! msga do, for people who cannot or do not want to register a Slack app.
//!
//! Slack's web client authenticates with a per-session `xoxc-` token that is
//! embedded in each workspace's boot page, plus the account-wide `d` cookie
//! (`xoxd-…`). Given the cookie and a workspace URL, [`derive`] fetches the
//! boot page and reads the token out of it, then checks it with `auth.test`.
//!
//! One `d` cookie covers every workspace the account is signed in to. It
//! rotates when you sign out or change your password, so the workspace URL
//! is kept to derive a fresh token from a new cookie later. This uses
//! undocumented endpoints and is a fallback to the Slack-app sign-in.

use std::sync::Arc;
use std::time::Duration;

use reqwest::cookie::Jar;

use super::client::{self, SlackError};
use super::{Token, types};

/// Who a derived token belongs to.
#[derive(Clone, Debug)]
pub struct SessionSignIn {
    pub team_id: String,
    pub user_id: String,
    pub token: Token,
}

/// Normalises what the user typed into `https://team.slack.com`.
pub fn normalize_workspace(input: &str) -> Option<String> {
    let trimmed = input.trim().trim_end_matches('/');
    if trimmed.is_empty() {
        return None;
    }
    let host = trimmed
        .strip_prefix("https://")
        .or_else(|| trimmed.strip_prefix("http://"))
        .unwrap_or(trimmed)
        .split('/')
        .next()
        .unwrap_or(trimmed);
    // A bare subdomain ("acme") becomes the full host.
    let host = if host.contains('.') {
        host.to_ascii_lowercase()
    } else {
        format!("{}.slack.com", host.to_ascii_lowercase())
    };
    // The session cookie only works on Slack's own hosts, and the boot page
    // must come from one: anything else (a port, credentials, a look-alike
    // domain) is refused rather than fetched.
    let url = reqwest::Url::parse(&format!("https://{host}")).ok()?;
    let valid = url.port().is_none()
        && url.username().is_empty()
        && url.password().is_none()
        && url
            .host_str()
            .and_then(|h| h.strip_suffix(".slack.com"))
            .is_some_and(|sub| !sub.is_empty() && !sub.starts_with('.'));
    valid.then(|| format!("https://{host}"))
}

/// A recent desktop Chrome user agent. Slack serves the full web-client boot
/// page (the one that carries the `xoxc-` token) only to a browser-like
/// agent; a plain client gets a page without it.
const BROWSER_UA: &str = "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/131.0.0.0 Safari/537.36";

/// Cleans up a pasted `d` cookie: a `d=` prefix, surrounding quotes and a
/// trailing `;` all come along when people copy from dev tools.
fn clean_cookie(raw: &str) -> String {
    let mut value = raw.trim().trim_matches('"').trim_matches('\'').trim();
    if let Some(rest) = value.strip_prefix("d=") {
        value = rest.trim();
    }
    value.trim_end_matches(';').trim().to_owned()
}

/// The `xoxc-` token embedded in a workspace boot page. Slack has written it
/// as `"api_token":"xoxc-…"` and `"token":"xoxc-…"` over the years; fall back
/// to any `xoxc-` run on the page.
fn scrape_token(html: &str) -> Option<String> {
    for key in ["\"api_token\":\"", "\"token\":\"", "\"api_token\": \""] {
        if let Some(token) = html
            .split_once(key)
            .and_then(|(_, rest)| rest.split('"').next())
            .filter(|token| token.starts_with("xoxc-"))
        {
            return Some(token.to_owned());
        }
    }
    let start = html.find("xoxc-")?;
    let end = html[start..]
        .find(|c: char| !(c.is_ascii_alphanumeric() || c == '-'))
        .map_or(html.len(), |offset| start + offset);
    Some(html[start..end].to_owned())
}

/// Whether the page is a signed-in web client (it names the token slot, even
/// when that slot is null) rather than a sign-in page.
fn looks_logged_in(html: &str) -> bool {
    html.contains("\"api_token\"") || html.contains("\"team_id\"") || html.contains("boot_data")
}

/// A client whose cookie jar carries the `d` cookie across the boot page's
/// redirect chain (`/` → `/ssb/redirect`), which a raw header would not
/// survive.
fn seeded_client(cookie: &str) -> Result<reqwest::Client, SlackError> {
    let jar = Arc::new(Jar::default());
    let url = "https://slack.com"
        .parse::<reqwest::Url>()
        .map_err(|e| SlackError::Network(e.to_string()))?;
    jar.add_cookie_str(&format!("d={cookie}; Domain=.slack.com; Path=/"), &url);
    reqwest::Client::builder()
        .user_agent(BROWSER_UA)
        .connect_timeout(Duration::from_secs(15))
        .timeout(Duration::from_secs(60))
        .cookie_provider(jar)
        .build()
        .map_err(|e| SlackError::Network(e.to_string()))
}

/// Derives and validates a session token for one workspace.
pub async fn derive(cookie: &str, workspace_url: &str) -> Result<SessionSignIn, SlackError> {
    let cookie = clean_cookie(cookie);
    if !cookie.starts_with("xoxd-") {
        return Err(SlackError::Api(
            "the d cookie should start with xoxd-; copy its value from the cookie named d".into(),
        ));
    }
    let http = seeded_client(&cookie)?;
    // The boot page returns HTTP 403 while still carrying the token, so the
    // body is what matters, not the status.
    let response = http.get(workspace_url).send().await?;
    let status = response.status();
    let body = response.text().await?;
    log::debug!(
        "session boot {workspace_url}: HTTP {status}, {} bytes, logged_in={}",
        body.len(),
        looks_logged_in(&body)
    );
    let token = scrape_token(&body).ok_or_else(|| {
        let reason = if looks_logged_in(&body) {
            "signed in, but Slack did not put a session token on the page for this workspace"
        } else {
            "the d cookie did not sign in; copy a fresh one from a browser where this workspace is open"
        };
        SlackError::Api(reason.into())
    })?;
    let session = Token::session(token, cookie, workspace_url);
    let client = client::Client::new(http, session.clone());
    let test: types::AuthTest = client.call("auth.test", &[]).await?;
    Ok(SessionSignIn {
        team_id: test.team_id,
        user_id: test.user_id,
        token: session,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn workspace_urls_normalize() {
        assert_eq!(
            normalize_workspace("acme"),
            Some("https://acme.slack.com".into())
        );
        assert_eq!(
            normalize_workspace("acme.slack.com"),
            Some("https://acme.slack.com".into())
        );
        assert_eq!(
            normalize_workspace("https://acme.slack.com/messages/"),
            Some("https://acme.slack.com".into())
        );
        assert_eq!(
            normalize_workspace("Acme.Enterprise.Slack.com"),
            Some("https://acme.enterprise.slack.com".into())
        );
        assert_eq!(normalize_workspace("  "), None);
        for hostile in [
            "evil.example",
            "acme.slack.com.evil.example",
            "evilslack.com",
            "slack.com",
            ".slack.com",
            "acme.slack.com:8443",
            "user@acme.slack.com",
            "https://evil.example/acme.slack.com",
        ] {
            assert_eq!(normalize_workspace(hostile), None, "{hostile}");
        }
    }

    #[test]
    fn the_token_is_read_from_the_boot_page() {
        assert_eq!(
            scrape_token(r#"...,"api_token":"xoxc-123-abc","no":1"#).as_deref(),
            Some("xoxc-123-abc")
        );
        assert_eq!(
            scrape_token(r#"{"token":"xoxc-7-x"}"#).as_deref(),
            Some("xoxc-7-x")
        );
        assert_eq!(
            scrape_token("var x = 'xoxc-999-zzz';").as_deref(),
            Some("xoxc-999-zzz")
        );
        assert_eq!(scrape_token(r#"{"api_token":null}"#), None);
        assert_eq!(scrape_token("logged out"), None);
    }

    #[test]
    fn pasted_cookies_are_cleaned_up() {
        assert_eq!(clean_cookie("  xoxd-abc  "), "xoxd-abc");
        assert_eq!(clean_cookie("d=xoxd-abc;"), "xoxd-abc");
        assert_eq!(clean_cookie("\"xoxd-abc\""), "xoxd-abc");
    }
}
