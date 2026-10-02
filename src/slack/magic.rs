//! Signing in through the browser, without a browser window of our own.
//!
//! [`SIGN_IN_URL`] opens in the user's browser. Once they have signed in
//! there (password, emailed code or SSO), Slack's page hands the browser a
//! `slack://` link carrying one-time "magic" tokens, in one of two shapes:
//!
//! - `slack://T0123/magic-login/<token>?host=acme.slack.com`, one per team;
//! - `slack://login-v2?0.host=acme.slack.com&0.tokens=a_b&1.host=…`, sets of
//!   tokens per host, each possibly marked `dpop=1`.
//!
//! [`parse_link`] reads both. [`redeem`] then calls `auth.loginMagicBulk` on
//! each host, which answers with the teams signed in to and sets the
//! account's `d` session cookie. From there it is the ordinary session
//! sign-in ([`super::session::derive`]).
//!
//! The idea of signing in through the browser and catching Slack's hand-off
//! comes from Make Slack Great Again (msga). None of these endpoints are
//! documented; they may change at any time.

use std::sync::Arc;
use std::time::Duration;

use reqwest::cookie::{CookieStore as _, Jar};

use super::client::SlackError;

/// Where the browser sign-in starts.
pub const SIGN_IN_URL: &str = "https://app.slack.com/ssb/signin";

/// NoSlacking's own user agent, as the rest of the client sends: Slack
/// redeems sign-in tokens for clients that name themselves.
const USER_AGENT: &str = concat!("NoSlacking/", env!("CARGO_PKG_VERSION"));

/// The host a link names when it names none.
const DEFAULT_HOST: &str = "slack.com";

/// One host's magic tokens, from a sign-in link.
#[derive(Clone, PartialEq, Eq)]
pub struct TokenSet {
    /// The host to redeem them on (`acme.slack.com`), already checked to be
    /// one of Slack's.
    pub host: String,
    /// The tokens as `auth.loginMagicBulk` takes them (`z-app-T0123-…`).
    pub tokens: Vec<String>,
    /// Slack asked for a proof-of-possession key with these. The desktop
    /// app reads this but sends no proof either; it is kept for logging.
    pub dpop: bool,
}

/// The tokens are one-time sign-in secrets; only the host prints.
impl std::fmt::Debug for TokenSet {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TokenSet")
            .field("host", &self.host)
            .field("tokens", &self.tokens.len())
            .field("dpop", &self.dpop)
            .finish()
    }
}

/// The token sets in a `slack://` sign-in link, or `None` when it is not
/// one (or names a host that is not Slack's).
pub fn parse_link(link: &str) -> Option<Vec<TokenSet>> {
    let url = reqwest::Url::parse(link.trim()).ok()?;
    if url.scheme() != "slack" {
        return None;
    }
    let query = |key: &str| {
        url.query_pairs()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.into_owned())
    };
    if url.host_str() == Some("login-v2") {
        let mut sets = Vec::new();
        for n in 0.. {
            let host = query(&format!("{n}.host"));
            let tokens = query(&format!("{n}.tokens"));
            let (Some(host), Some(tokens)) = (host, tokens) else {
                break;
            };
            let tokens: Vec<String> = tokens
                .split('_')
                .filter(|t| !t.is_empty())
                .map(str::to_owned)
                .collect();
            if tokens.is_empty() {
                break;
            }
            sets.push(TokenSet {
                host: slack_host(&host)?,
                tokens,
                dpop: query(&format!("{n}.dpop")).as_deref() == Some("1"),
            });
        }
        return (!sets.is_empty()).then_some(sets);
    }
    // `slack://T0123/magic-login/abc`: the team is the URL's host and the
    // token its last path segment; one link may carry several
    // `T…/magic-login/…` runs.
    let text = url.as_str().strip_prefix("slack://")?;
    let mut tokens = Vec::new();
    for (i, _) in text.match_indices("/magic-login/") {
        let team = text[..i]
            .rsplit(|c: char| !c.is_ascii_alphanumeric())
            .next()
            .filter(|t| t.len() >= 9 && (t.starts_with(['T', 't', 'E', 'e'])))?;
        let rest = &text[i + "/magic-login/".len()..];
        let token: String = rest
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric() || *c == '-')
            .collect();
        if token.is_empty() {
            return None;
        }
        tokens.push(format!("z-app-{}-{token}", team.to_ascii_uppercase()));
    }
    if tokens.is_empty() {
        return None;
    }
    let host = match query("host") {
        Some(host) => slack_host(&host)?,
        None => DEFAULT_HOST.to_owned(),
    };
    Some(vec![TokenSet {
        host,
        tokens,
        dpop: query("dpop").as_deref() == Some("1"),
    }])
}

/// `host` when it is Slack's own (`slack.com` or a subdomain), with no
/// port, path or credentials: the tokens and the cookie go nowhere else.
fn slack_host(host: &str) -> Option<String> {
    let host = host.trim().to_ascii_lowercase();
    let url = reqwest::Url::parse(&format!("https://{host}/")).ok()?;
    let valid = url.port().is_none()
        && url.username().is_empty()
        && url.password().is_none()
        && url.path() == "/"
        && url
            .host_str()
            .is_some_and(|h| h == host && (h == "slack.com" || h.ends_with(".slack.com")));
    valid.then_some(host)
}

/// What a host said about one team after redeeming its token.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TeamResult {
    /// Signed in: the team's address, for the session sign-in.
    SignedIn { url: String },
    /// The team wants another step in the browser first (SSO, a policy).
    Browser { url: String },
    /// Slack refused this token.
    Failed { reason: String },
}

/// The outcome of redeeming one host's tokens.
pub struct Redeemed {
    /// The `d` cookie Slack set, when it signed anything in.
    pub cookie: Option<String>,
    pub teams: Vec<TeamResult>,
}

/// The per-team answers in an `auth.loginMagicBulk` reply.
fn team_results(body: &serde_json::Value) -> Result<Vec<TeamResult>, SlackError> {
    if body["ok"] != true {
        let error = body["error"].as_str().unwrap_or("magic_login_failed");
        return Err(SlackError::Api(error.to_owned()));
    }
    let Some(results) = body["token_results"].as_object() else {
        return Err(SlackError::Decode("no token_results in the answer".into()));
    };
    let mut teams = Vec::new();
    for result in results.values() {
        let text = |key: &str| result[key].as_str().filter(|s| !s.is_empty());
        if let Some(url) = text("auth_redir") {
            teams.push(TeamResult::Browser {
                url: url.to_owned(),
            });
        } else if let Some(url) = result["team"]["url"].as_str().filter(|s| !s.is_empty()) {
            teams.push(TeamResult::SignedIn {
                url: url.trim_end_matches('/').to_owned(),
            });
        } else {
            let reason = text("error").or(text("reason")).unwrap_or("not signed in");
            teams.push(TeamResult::Failed {
                reason: reason.to_owned(),
            });
        }
    }
    Ok(teams)
}

/// The value of the `d` cookie in a `Cookie` header's worth of cookies.
fn d_cookie(header: &str) -> Option<String> {
    header
        .split(';')
        .filter_map(|pair| pair.trim().split_once('='))
        .find(|(name, _)| *name == "d")
        .map(|(_, value)| value.to_owned())
        .filter(|value| !value.is_empty())
}

/// Redeems one host's tokens with `auth.loginMagicBulk`, keeping the session
/// cookie Slack sets.
pub async fn redeem(set: &TokenSet) -> Result<Redeemed, SlackError> {
    if set.dpop {
        log::info!(
            "sign-in link asks for DPoP on {}; trying without a proof",
            set.host
        );
    }
    let jar = Arc::new(Jar::default());
    let http = super::net::builder()
        .user_agent(USER_AGENT)
        .connect_timeout(Duration::from_secs(15))
        .timeout(Duration::from_secs(60))
        .cookie_provider(jar.clone())
        .build()?;
    let api = reqwest::Url::parse(&format!("https://{}/api/auth.loginMagicBulk", set.host))
        .map_err(|e| SlackError::Network(e.to_string()))?;
    let body: serde_json::Value = http
        .get(api.clone())
        .query(&[("magic_tokens", set.tokens.join(",")), ("ssb", "1".into())])
        .send()
        .await?
        .json()
        .await?;
    let teams = team_results(&body)?;
    // The cookie is set for `.slack.com`; any Slack URL reads it back.
    let cookie = ["https://slack.com/", api.as_str()]
        .iter()
        .filter_map(|u| reqwest::Url::parse(u).ok())
        .filter_map(|u| jar.cookies(&u))
        .find_map(|header| header.to_str().ok().and_then(d_cookie));
    Ok(Redeemed { cookie, teams })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn magic_login_links_become_z_app_tokens() {
        let sets = parse_link("slack://T0123ABCD/magic-login/abc-123?host=acme.slack.com")
            .expect("a sign-in link");
        assert_eq!(
            sets,
            [TokenSet {
                host: "acme.slack.com".into(),
                tokens: vec!["z-app-T0123ABCD-abc-123".into()],
                dpop: false,
            }]
        );
        let sets = parse_link("slack://t0123abcd/magic-login/xyz/").expect("a sign-in link");
        assert_eq!(sets[0].host, "slack.com");
        assert_eq!(sets[0].tokens, ["z-app-T0123ABCD-xyz"]);
    }

    #[test]
    fn login_v2_links_carry_sets_per_host() {
        let sets = parse_link(
            "slack://login-v2?0.host=acme.slack.com&0.tokens=a_b&1.host=beta.slack.com&1.tokens=c&1.dpop=1",
        )
        .expect("a sign-in link");
        assert_eq!(sets.len(), 2);
        assert_eq!(sets[0].tokens, ["a", "b"]);
        assert!(!sets[0].dpop);
        assert_eq!(sets[1].host, "beta.slack.com");
        assert!(sets[1].dpop);
    }

    #[test]
    fn other_links_and_foreign_hosts_are_refused() {
        for link in [
            "slack://channel?team=T1&id=C1",
            "https://acme.slack.com/magic-login/abc",
            "slack://T0123ABCD/magic-login/abc?host=evil.example",
            "slack://T0123ABCD/magic-login/abc?host=acme.slack.com.evil.example",
            "slack://T0123ABCD/magic-login/abc?host=acme.slack.com:8443",
            "slack://login-v2?0.host=evil.example&0.tokens=a",
            "slack://login-v2?0.host=acme.slack.com",
            "slack://T0123ABCD/magic-login/",
            "not a link",
        ] {
            assert_eq!(parse_link(link), None, "{link}");
        }
    }

    #[test]
    fn token_sets_never_print_their_tokens() {
        let sets = parse_link("slack://T0123ABCD/magic-login/secret-1").expect("link");
        let printed = format!("{sets:?}");
        assert!(!printed.contains("secret"), "{printed}");
    }

    #[test]
    fn bulk_answers_split_into_teams() {
        let body = serde_json::json!({
            "ok": true,
            "token_results": {
                "z-app-T1-a": {"ok": true, "team": {"url": "https://acme.slack.com/"}, "redir": "/client/T1"},
                "z-app-T2-b": {"ok": true, "auth_redir": "https://beta.slack.com/sso/saml/start"},
                "z-app-T3-c": {"ok": false, "error": "invalid_magic_token"}
            }
        });
        let mut teams = team_results(&body).expect("ok");
        teams.sort_by_key(|t| format!("{t:?}"));
        assert!(teams.contains(&TeamResult::SignedIn {
            url: "https://acme.slack.com".into()
        }));
        assert!(teams.contains(&TeamResult::Browser {
            url: "https://beta.slack.com/sso/saml/start".into()
        }));
        assert!(teams.contains(&TeamResult::Failed {
            reason: "invalid_magic_token".into()
        }));
        assert_eq!(
            team_results(&serde_json::json!({"ok": false, "error": "ratelimited"}))
                .err()
                .map(|e| e.to_string()),
            Some(SlackError::Api("ratelimited".into()).to_string())
        );
    }

    #[test]
    fn the_d_cookie_is_read_from_the_jar_header() {
        assert_eq!(
            d_cookie("lc=1; d=xoxd-abc%2F; x=2").as_deref(),
            Some("xoxd-abc%2F")
        );
        assert_eq!(d_cookie("lc=1"), None);
        assert_eq!(d_cookie("d="), None);
    }
}
