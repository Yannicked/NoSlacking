//! The Slack Web API over reqwest: bearer auth, rate limits, token refresh.
//!
//! Every method is a form POST to `https://slack.com/api/<method>` that
//! answers `{"ok": true, ...}` or `{"ok": false, "error": "<code>"}`. A 429
//! carries `Retry-After`; this client waits it out and tries again, a few
//! times, without ever spinning. At most six calls per workspace are in
//! flight at once, so a burst of sidebar refreshes cannot starve a send.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures_util::future::BoxFuture;
use serde::de::DeserializeOwned;
use tokio::sync::Semaphore;

use super::types;

pub const API: &str = "https://slack.com/api/";
const MAX_IN_FLIGHT: usize = 6;
const MAX_ATTEMPTS: u32 = 4;
/// Refresh a rotating token this long before it expires.
const REFRESH_MARGIN: i64 = 300;

#[derive(Clone, Debug, thiserror::Error, PartialEq)]
pub enum SlackError {
    /// Slack answered `ok: false` with this code.
    #[error("Slack said: {0}")]
    Api(String),
    #[error("Slack is rate limiting requests; try again shortly")]
    RateLimited,
    #[error("HTTP {0}")]
    Http(u16),
    #[error("network: {0}")]
    Network(String),
    #[error("unexpected response: {0}")]
    Decode(String),
}

/// The error codes that mean a token no longer works and the workspace
/// needs signing in again. The one list for every check, so the sign-out
/// logic, the messages and the sockets cannot disagree.
pub const AUTH_ERRORS: &[&str] = &[
    "invalid_auth",
    "not_authed",
    "token_revoked",
    "account_inactive",
    "token_expired",
    // A rotating token's refresh token was refused: nothing can renew it.
    "invalid_refresh_token",
    "invalid_grant",
];

/// Whether `code` is one of [`AUTH_ERRORS`].
pub fn is_auth_code(code: &str) -> bool {
    AUTH_ERRORS.contains(&code)
}

impl SlackError {
    /// Whether the token no longer works and the workspace needs signing in
    /// again.
    pub fn is_auth(&self) -> bool {
        matches!(self, Self::Api(code) if is_auth_code(code))
    }

    pub fn code(&self) -> Option<&str> {
        match self {
            Self::Api(code) => Some(code),
            _ => None,
        }
    }
}

/// A workspace's user token and, when the app rotates tokens, what renews it.
#[derive(Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Token {
    pub access: String,
    #[serde(default)]
    pub refresh: Option<String>,
    /// Unix seconds; `None` for a token that never expires.
    #[serde(default)]
    pub expires_at: Option<i64>,
    /// The `d` session cookie (value already `xoxd-…`), for a workspace
    /// signed in through the browser session rather than OAuth. When set,
    /// `access` is an `xoxc-` token and it is sent with this cookie; it
    /// never rotates.
    #[serde(default)]
    pub cookie: Option<String>,
    /// The workspace URL (`https://team.slack.com`), kept so the `xoxc`
    /// token can be derived again from a fresh cookie.
    #[serde(default)]
    pub workspace_url: Option<String>,
}

impl Token {
    pub fn plain(access: impl Into<String>) -> Self {
        Self {
            access: access.into(),
            refresh: None,
            expires_at: None,
            cookie: None,
            workspace_url: None,
        }
    }

    /// A browser-session token: an `xoxc-` token plus its `d` cookie.
    pub fn session(
        access: impl Into<String>,
        cookie: impl Into<String>,
        workspace_url: impl Into<String>,
    ) -> Self {
        Self {
            access: access.into(),
            refresh: None,
            expires_at: None,
            cookie: Some(cookie.into()),
            workspace_url: Some(workspace_url.into()),
        }
    }

    pub fn is_session(&self) -> bool {
        self.cookie.is_some()
    }

    fn needs_refresh(&self, now: i64) -> bool {
        self.refresh.is_some() && self.expires_at.is_some_and(|at| now >= at - REFRESH_MARGIN)
    }
}

/// Shows only when the token expires and what kind it is.
impl std::fmt::Debug for Token {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Token")
            .field("access", &crate::redact::REDACTED)
            .field(
                "refresh",
                &self.refresh.as_ref().map(|_| crate::redact::REDACTED),
            )
            .field("expires_at", &self.expires_at)
            .field(
                "cookie",
                &self.cookie.as_ref().map(|_| crate::redact::REDACTED),
            )
            .finish()
    }
}

/// The app's OAuth credentials, for refreshing rotating tokens.
#[derive(Clone)]
pub struct OauthApp {
    pub client_id: String,
    pub client_secret: String,
}

impl std::fmt::Debug for OauthApp {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OauthApp")
            .field("client_id", &self.client_id)
            .field("client_secret", &crate::redact::REDACTED)
            .finish()
    }
}

type OnRefresh = Arc<dyn Fn(Result<Token, SlackError>) -> BoxFuture<'static, ()> + Send + Sync>;

/// The shortest and longest pause after a refresh that failed for a
/// passing reason (the network, Slack having a bad moment).
const REFRESH_RETRY_FIRST: Duration = Duration::from_secs(30);
const REFRESH_RETRY_MAX: Duration = Duration::from_secs(30 * 60);

/// A refresh that failed, so the next calls do not all try again at once.
#[derive(Clone, Debug)]
struct RefreshFailure {
    error: SlackError,
    /// Failures in a row.
    failures: u32,
    retry_at: std::time::Instant,
}

/// How long to leave a rotating token alone after `failures` failed
/// refreshes in a row.
fn refresh_wait(failures: u32) -> Duration {
    let doublings = failures.saturating_sub(1).min(10);
    (REFRESH_RETRY_FIRST * 2u32.pow(doublings)).min(REFRESH_RETRY_MAX)
}

/// What every clone of one workspace's client shares: one token, one
/// refresh at a time, and one set of request slots. Clones handed to the
/// image loader or to tasks see a renewed token or a new app at once.
struct Shared {
    token: Mutex<Token>,
    /// Held for a whole refresh, including saving the new token, so
    /// renewals happen and are stored strictly one after another.
    refresh_lock: tokio::sync::Mutex<()>,
    failure: Mutex<Option<RefreshFailure>>,
    app: Mutex<Option<OauthApp>>,
    on_refresh: Mutex<Option<OnRefresh>>,
    limit: Semaphore,
}

/// One workspace's API access.
#[derive(Clone)]
pub struct Client {
    http: reqwest::Client,
    shared: Arc<Shared>,
    base: String,
}

impl std::fmt::Debug for Client {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Client").finish_non_exhaustive()
    }
}

/// The shared HTTP client: rustls, gzip, a sane timeout, an honest agent.
pub fn http() -> reqwest::Client {
    reqwest::Client::builder()
        .user_agent(concat!("NoSlacking/", env!("CARGO_PKG_VERSION")))
        .connect_timeout(Duration::from_secs(15))
        .timeout(Duration::from_secs(120))
        .build()
        .unwrap_or_else(|error| {
            log::error!("HTTP client setup failed, using defaults: {error}");
            reqwest::Client::new()
        })
}

pub fn now() -> i64 {
    jiff::Timestamp::now().as_second()
}

impl Client {
    pub fn new(http: reqwest::Client, token: Token) -> Self {
        Self {
            http,
            shared: Arc::new(Shared {
                token: Mutex::new(token),
                refresh_lock: tokio::sync::Mutex::new(()),
                failure: Mutex::new(None),
                app: Mutex::new(None),
                on_refresh: Mutex::new(None),
                limit: Semaphore::new(MAX_IN_FLIGHT),
            }),
            base: API.to_owned(),
        }
    }

    /// Renews a rotating token with `app`, reporting each outcome: the new
    /// token, or why the refresh failed. The report is awaited before the
    /// next refresh can start, so new tokens are stored in the order Slack
    /// issued them.
    pub fn with_refresh<F, Fut>(self, app: Option<OauthApp>, on_refresh: F) -> Self
    where
        F: Fn(Result<Token, SlackError>) -> Fut + Send + Sync + 'static,
        Fut: std::future::Future<Output = ()> + Send + 'static,
    {
        self.set_app(app);
        let on_refresh: OnRefresh = Arc::new(move |result| Box::pin(on_refresh(result)));
        *lock(&self.shared.on_refresh) = Some(on_refresh);
        self
    }

    /// Switches the app that renews the token, for this client and every
    /// clone of it. A new app also gets a fresh chance to refresh.
    pub fn set_app(&self, app: Option<OauthApp>) {
        *lock(&self.shared.app) = app;
        *lock(&self.shared.failure) = None;
    }

    pub fn http(&self) -> &reqwest::Client {
        &self.http
    }

    pub fn token(&self) -> Token {
        lock(&self.shared.token).clone()
    }

    fn cookie(&self) -> Option<String> {
        lock(&self.shared.token).cookie.clone()
    }

    async fn access_token(&self) -> Result<String, SlackError> {
        let token = self.token();
        if !token.needs_refresh(now()) {
            return Ok(token.access);
        }
        let _guard = self.shared.refresh_lock.lock().await;
        // Another call may have refreshed while this one waited.
        let token = self.token();
        if !token.needs_refresh(now()) {
            return Ok(token.access);
        }
        let app = lock(&self.shared.app).clone();
        let (Some(app), Some(refresh)) = (app, token.refresh.clone()) else {
            return Ok(token.access);
        };
        let failure = lock(&self.shared.failure).clone();
        if let Some(failure) = failure {
            // A refused refresh token stays refused, and a passing failure
            // gets a pause: either way, not one more try per API call.
            if failure.error.is_auth() || std::time::Instant::now() < failure.retry_at {
                return fallback(&token, failure.error);
            }
        }
        let on_refresh = lock(&self.shared.on_refresh).clone();
        match refresh_token(&self.http, &self.base, &app, &refresh).await {
            Ok(renewed) => {
                *lock(&self.shared.token) = renewed.clone();
                *lock(&self.shared.failure) = None;
                log::info!("renewed a rotating Slack token");
                if let Some(on_refresh) = on_refresh {
                    on_refresh(Ok(renewed.clone())).await;
                }
                Ok(renewed.access)
            }
            Err(error) => {
                let failures = lock(&self.shared.failure)
                    .as_ref()
                    .map_or(1, |f| f.failures.saturating_add(1));
                log::warn!("could not renew a rotating Slack token: {error}");
                *lock(&self.shared.failure) = Some(RefreshFailure {
                    error: error.clone(),
                    failures,
                    retry_at: std::time::Instant::now() + refresh_wait(failures),
                });
                if let Some(on_refresh) = on_refresh {
                    on_refresh(Err(error.clone())).await;
                }
                fallback(&token, error)
            }
        }
    }

    /// Calls a read method; network failures are retried.
    pub async fn call<T: DeserializeOwned>(
        &self,
        method: &str,
        params: &[(&str, String)],
    ) -> Result<T, SlackError> {
        self.request(method, params, true).await
    }

    /// Calls a method that changes something; only rate limits are retried,
    /// because a network failure may have happened after Slack acted.
    pub async fn act<T: DeserializeOwned>(
        &self,
        method: &str,
        params: &[(&str, String)],
    ) -> Result<T, SlackError> {
        self.request(method, params, false).await
    }

    async fn request<T: DeserializeOwned>(
        &self,
        method: &str,
        params: &[(&str, String)],
        idempotent: bool,
    ) -> Result<T, SlackError> {
        let url = format!("{}{method}", self.base);
        let mut attempt = 0;
        loop {
            attempt += 1;
            // The permit covers one attempt, not the waits between them:
            // a call sitting out a Retry-After must not hold a slot that a
            // send could use.
            let permit = self
                .shared
                .limit
                .acquire()
                .await
                .map_err(|_| SlackError::Network("client closed".into()))?;
            let token = self.access_token().await?;
            let mut request = self.http.post(&url).bearer_auth(&token).form(params);
            if let Some(cookie) = self.cookie() {
                request = request.header(reqwest::header::COOKIE, format!("d={cookie}"));
            }
            let sent = request.send().await;
            let response = match sent {
                Ok(response) => response,
                Err(error) if idempotent && attempt < MAX_ATTEMPTS => {
                    log::debug!("{method}: {error}; retrying");
                    drop(permit);
                    tokio::time::sleep(backoff(attempt)).await;
                    continue;
                }
                Err(error) => return Err(SlackError::Network(error.without_url().to_string())),
            };
            let status = response.status().as_u16();
            if let Some(wait) = retry_after(status, response.headers().get("retry-after")) {
                if attempt >= MAX_ATTEMPTS {
                    return Err(SlackError::RateLimited);
                }
                log::debug!("{method}: rate limited for {wait:?}");
                drop(permit);
                tokio::time::sleep(wait).await;
                continue;
            }
            if status >= 500 && idempotent && attempt < MAX_ATTEMPTS {
                drop(permit);
                tokio::time::sleep(backoff(attempt)).await;
                continue;
            }
            let bytes = response
                .bytes()
                .await
                .map_err(|e| SlackError::Network(e.without_url().to_string()))?;
            drop(permit);
            if !(200..300).contains(&status) && bytes.is_empty() {
                return Err(SlackError::Http(status));
            }
            return decode(&bytes);
        }
    }

    /// Downloads a file that needs the token (`url_private`). The token and
    /// cookie go only to Slack's file host; any other URL, which a bot or a
    /// link preview can choose, is fetched without them.
    pub async fn get_bytes(&self, url: &str, max: usize) -> Result<Vec<u8>, SlackError> {
        if !is_slack_file_url(url) {
            return get_bytes(&self.http, url, None, None, max).await;
        }
        let token = self.access_token().await?;
        get_bytes(&self.http, url, Some(&token), self.cookie().as_deref(), max).await
    }

    /// Uploads a file with Slack's two-step external upload.
    pub async fn upload(
        &self,
        channel: &str,
        thread: Option<&str>,
        name: &str,
        bytes: Vec<u8>,
        comment: &str,
    ) -> Result<(), SlackError> {
        let target: types::UploadUrl = self
            .act(
                "files.getUploadURLExternal",
                &[
                    ("filename", name.to_owned()),
                    ("length", bytes.len().to_string()),
                ],
            )
            .await?;
        let part = reqwest::multipart::Part::bytes(bytes).file_name(name.to_owned());
        let form = reqwest::multipart::Form::new().part("file", part);
        let response = self
            .http
            .post(&target.upload_url)
            .multipart(form)
            .send()
            .await
            .map_err(|e| SlackError::Network(e.without_url().to_string()))?;
        if !response.status().is_success() {
            return Err(SlackError::Http(response.status().as_u16()));
        }
        let files = serde_json::json!([{ "id": target.file_id, "title": name }]).to_string();
        let mut params = vec![("files", files), ("channel_id", channel.to_owned())];
        if let Some(thread) = thread {
            params.push(("thread_ts", thread.to_owned()));
        }
        if !comment.is_empty() {
            params.push(("initial_comment", comment.to_owned()));
        }
        let _: serde_json::Value = self.act("files.completeUploadExternal", &params).await?;
        Ok(())
    }
}

/// Decodes a Web API answer, turning `ok: false` into [`SlackError::Api`].
pub fn decode<T: DeserializeOwned>(bytes: &[u8]) -> Result<T, SlackError> {
    let value: serde_json::Value =
        serde_json::from_slice(bytes).map_err(|e| SlackError::Decode(e.to_string()))?;
    if value.get("ok").and_then(serde_json::Value::as_bool) != Some(true) {
        let code = value
            .get("error")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("unknown_error");
        return Err(SlackError::Api(code.to_owned()));
    }
    if let Some(warning) = value.get("warning").and_then(serde_json::Value::as_str) {
        log::debug!("Slack warning: {warning}");
    }
    serde_json::from_value(value).map_err(|e| SlackError::Decode(e.to_string()))
}

/// How long to wait before retrying, when Slack asks for it.
pub fn retry_after(status: u16, header: Option<&reqwest::header::HeaderValue>) -> Option<Duration> {
    if status != 429 {
        return None;
    }
    let seconds = header
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.trim().parse::<u64>().ok())
        .unwrap_or(5);
    Some(Duration::from_secs(seconds.clamp(1, 120)))
}

/// After a failed refresh: the old access token while it has not quite
/// expired (refreshes start a few minutes early), the error after that or
/// when the refresh token itself was refused.
fn fallback(token: &Token, error: SlackError) -> Result<String, SlackError> {
    let unexpired = token.expires_at.is_some_and(|at| now() < at);
    if unexpired && !error.is_auth() {
        Ok(token.access.clone())
    } else {
        Err(error)
    }
}

fn backoff(attempt: u32) -> Duration {
    Duration::from_millis(500 * 2u64.pow(attempt.min(5)))
}

/// The hosts that serve private files and may see the token.
const FILE_HOSTS: &[&str] = &["files.slack.com"];

/// Whether `url` is a private file on Slack's file host: `https`, an exact
/// host match, the default port and no credentials of its own. Anything
/// else must never be sent the workspace's token.
pub fn is_slack_file_url(url: &str) -> bool {
    let Ok(url) = reqwest::Url::parse(url) else {
        return false;
    };
    url.scheme() == "https"
        && url.port().is_none()
        && url.username().is_empty()
        && url.password().is_none()
        && url
            .host_str()
            .is_some_and(|host| FILE_HOSTS.contains(&host))
}

/// Fetches `url`, with the token when given, refusing more than `max` bytes
/// and HTML (Slack answers a bad token on a file with its sign-in page).
pub async fn get_bytes(
    http: &reqwest::Client,
    url: &str,
    token: Option<&str>,
    cookie: Option<&str>,
    max: usize,
) -> Result<Vec<u8>, SlackError> {
    let mut request = http.get(url);
    if let Some(token) = token {
        request = request.bearer_auth(token);
    }
    if let Some(cookie) = cookie {
        request = request.header(reqwest::header::COOKIE, format!("d={cookie}"));
    }
    let mut response = request
        .send()
        .await
        .map_err(|e| SlackError::Network(e.without_url().to_string()))?;
    if !response.status().is_success() {
        return Err(SlackError::Http(response.status().as_u16()));
    }
    let html = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.starts_with("text/html"));
    if html {
        return Err(SlackError::Api("file_needs_sign_in".into()));
    }
    let mut bytes = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|e| SlackError::Network(e.without_url().to_string()))?
    {
        bytes.extend_from_slice(&chunk);
        if bytes.len() > max {
            return Err(SlackError::Decode("file too large".into()));
        }
    }
    Ok(bytes)
}

/// Exchanges a refresh token for a new access token.
pub async fn refresh_token(
    http: &reqwest::Client,
    base: &str,
    app: &OauthApp,
    refresh: &str,
) -> Result<Token, SlackError> {
    let response = http
        .post(format!("{base}oauth.v2.access"))
        .form(&[
            ("client_id", app.client_id.as_str()),
            ("client_secret", app.client_secret.as_str()),
            ("grant_type", "refresh_token"),
            ("refresh_token", refresh),
        ])
        .send()
        .await
        .map_err(|e| SlackError::Network(e.without_url().to_string()))?;
    let bytes = response
        .bytes()
        .await
        .map_err(|e| SlackError::Network(e.without_url().to_string()))?;
    let access: types::OauthAccess = decode(&bytes)?;
    token_from(access).ok_or_else(|| SlackError::Decode("no user token in refresh".into()))
}

/// The user token in an `oauth.v2.access` answer.
pub fn token_from(access: types::OauthAccess) -> Option<Token> {
    let now = now();
    if let Some(token) = access.authed_user.access_token {
        return Some(Token {
            access: token,
            refresh: access.authed_user.refresh_token,
            expires_at: access.authed_user.expires_in.map(|s| now + s),
            cookie: None,
            workspace_url: None,
        });
    }
    access.access_token.map(|token| Token {
        access: token,
        refresh: access.refresh_token,
        expires_at: access.expires_in.map(|s| now + s),
        cookie: None,
        workspace_url: None,
    })
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn errors_decode_with_their_code() {
        let error = decode::<serde_json::Value>(br#"{"ok":false,"error":"invalid_auth"}"#)
            .expect_err("fails");
        assert_eq!(error, SlackError::Api("invalid_auth".into()));
        assert!(error.is_auth());
        assert!(SlackError::Api("invalid_refresh_token".into()).is_auth());
        assert!(SlackError::Api("invalid_grant".into()).is_auth());
        assert!(!SlackError::RateLimited.is_auth());
        assert!(!SlackError::Api("channel_not_found".into()).is_auth());
        assert!(!SlackError::Api("missing_scope".into()).is_auth());
        assert!(decode::<serde_json::Value>(b"<html>").is_err());
    }

    #[test]
    fn rate_limits_wait_as_told_within_bounds() {
        let header = reqwest::header::HeaderValue::from_static("30");
        assert_eq!(
            retry_after(429, Some(&header)),
            Some(Duration::from_secs(30))
        );
        assert_eq!(retry_after(429, None), Some(Duration::from_secs(5)));
        let huge = reqwest::header::HeaderValue::from_static("100000");
        assert_eq!(
            retry_after(429, Some(&huge)),
            Some(Duration::from_secs(120))
        );
        assert_eq!(retry_after(200, Some(&header)), None);
    }

    #[test]
    fn secrets_never_print() {
        let token = Token {
            access: "xoxp-1".into(),
            refresh: Some("xoxe-1".into()),
            expires_at: Some(5),
            cookie: Some("xoxd-1".into()),
            workspace_url: None,
        };
        let app = OauthApp {
            client_id: "123.456".into(),
            client_secret: "s3cr3t".into(),
        };
        let printed = format!("{token:?} {app:?} {:#?}", token);
        assert!(
            !printed.contains("xox") && !printed.contains("s3cr3t"),
            "{printed}"
        );
        assert!(printed.contains("123.456") && printed.contains("expires_at"));
    }

    #[test]
    fn only_slack_file_urls_may_see_the_token() {
        assert!(is_slack_file_url(
            "https://files.slack.com/files-pri/T1-F1/a.png"
        ));
        assert!(is_slack_file_url("https://FILES.slack.com/files-pri/a.png"));
        for hostile in [
            "https://evil.example/x.png?files.slack.com",
            "https://evil.example/files.slack.com/x.png",
            "https://files.slack.com.evil.example/x.png",
            "https://files.slack.com@evil.example/x.png",
            "https://user@files.slack.com/x.png",
            "https://files.slack.com:8443/x.png",
            "http://files.slack.com/x.png",
            "https://evilfiles.slack.com/x.png",
            "files.slack.com/x.png",
            "",
        ] {
            assert!(!is_slack_file_url(hostile), "{hostile}");
        }
    }

    #[test]
    fn rotating_tokens_refresh_before_they_expire() {
        let token = Token {
            access: "xoxe.xoxp-1".into(),
            refresh: Some("xoxe-1".into()),
            expires_at: Some(1_000),
            cookie: None,
            workspace_url: None,
        };
        assert!(!token.needs_refresh(600));
        assert!(token.needs_refresh(701));
        // A session token never refreshes.
        assert!(!Token::session("xoxc-1", "xoxd-1", "https://x.slack.com").needs_refresh(i64::MAX));
        assert!(!Token::plain("xoxp-1").needs_refresh(i64::MAX));
    }

    #[test]
    fn failed_refreshes_back_off() {
        assert_eq!(refresh_wait(1), Duration::from_secs(30));
        assert_eq!(refresh_wait(2), Duration::from_secs(60));
        assert_eq!(refresh_wait(4), Duration::from_secs(240));
        assert_eq!(refresh_wait(50), REFRESH_RETRY_MAX);
        assert_eq!(refresh_wait(u32::MAX), REFRESH_RETRY_MAX);
    }

    #[test]
    fn a_failed_refresh_keeps_the_token_until_it_expires() {
        let mut token = Token::plain("xoxe.xoxp-1");
        token.expires_at = Some(now() + 120);
        let outage = SlackError::Network("down".into());
        assert_eq!(fallback(&token, outage.clone()), Ok("xoxe.xoxp-1".into()));
        let refused = SlackError::Api("invalid_refresh_token".into());
        assert_eq!(fallback(&token, refused.clone()), Err(refused));
        token.expires_at = Some(now() - 1);
        assert_eq!(fallback(&token, outage.clone()), Err(outage));
    }

    #[test]
    fn clones_share_one_app_and_token() {
        let client = Client::new(reqwest::Client::new(), Token::plain("xoxp-1"));
        let clone = client.clone();
        client.set_app(Some(OauthApp {
            client_id: "1.2".into(),
            client_secret: "s".into(),
        }));
        assert!(lock(&clone.shared.app).is_some());
        assert!(Arc::ptr_eq(&client.shared, &clone.shared));
    }

    #[test]
    fn backoff_doubles_and_stops_growing() {
        assert_eq!(backoff(1), Duration::from_millis(1000));
        assert_eq!(backoff(2), Duration::from_millis(2000));
        assert_eq!(backoff(5), backoff(9));
    }

    #[test]
    fn user_tokens_come_from_either_shape() {
        let exchange: types::OauthAccess = serde_json::from_str(
            r#"{"ok":true,"authed_user":{"id":"U1","access_token":"xoxp-1","refresh_token":"r","expires_in":43200},"team":{"id":"T1"}}"#,
        )
        .expect("parses");
        let token = token_from(exchange).expect("token");
        assert_eq!(token.access, "xoxp-1");
        assert!(token.expires_at.is_some());
        let refresh: types::OauthAccess =
            serde_json::from_str(r#"{"ok":true,"access_token":"xoxe.xoxp-2","refresh_token":"r2","expires_in":43200,"token_type":"user"}"#)
                .expect("parses");
        assert_eq!(token_from(refresh).expect("token").access, "xoxe.xoxp-2");
    }
}
