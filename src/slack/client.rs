//! The Slack Web API over reqwest: bearer auth, rate limits, token refresh.
//!
//! Every method is a form POST to `https://slack.com/api/<method>` that
//! answers `{"ok": true, ...}` or `{"ok": false, "error": "<code>"}`. A 429
//! carries `Retry-After`; this client waits it out and tries again, a few
//! times, without ever spinning. At most six calls per workspace are in
//! flight at once, so a burst of sidebar refreshes cannot starve a send.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use futures_util::future::BoxFuture;
use serde::de::DeserializeOwned;
use tokio::sync::Semaphore;

use super::types;
use crate::scopes::Scopes;
use crate::sync::lock;

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
    /// A browser-session sign-in did not work, before Slack's API had a say.
    #[error("session sign-in: {0:?}")]
    Session(super::session::Refusal),
    /// An OAuth sign-in's answer held no user token.
    #[error("no user token")]
    NoUserToken,
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

/// A transport failure. The URL is left out: a file URL or a socket URL
/// can carry a secret.
impl From<reqwest::Error> for SlackError {
    fn from(error: reqwest::Error) -> Self {
        Self::Network(error.without_url().to_string())
    }
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

/// The app's OAuth identity, for refreshing rotating tokens. Only the
/// client id: a PKCE app refreshes without its secret.
#[derive(Clone, Debug)]
pub struct OauthApp {
    pub client_id: String,
}

type OnRefresh = Arc<dyn Fn(Result<Token, SlackError>) -> BoxFuture<'static, ()> + Send + Sync>;

/// What asks Slack for a new token: the app and the refresh token in, the
/// renewed token out. `None` in [`Shared`] means [`refresh_token`] over
/// HTTP; tests put a pretend one in its place.
type Refresher =
    Arc<dyn Fn(OauthApp, String) -> BoxFuture<'static, Result<Token, SlackError>> + Send + Sync>;

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
    retry_at: Instant,
}

/// How long to leave a rotating token alone after `failures` failed
/// refreshes in a row.
fn refresh_wait(failures: u32) -> Duration {
    let doublings = failures.saturating_sub(1).min(10);
    crate::retry::backoff(REFRESH_RETRY_FIRST, REFRESH_RETRY_MAX, doublings)
}

/// Everything about a workspace's sign-in that changes as it is used, kept
/// together so one look at it is never half old and half new.
///
/// It lives in a plain [`Mutex`] that is only ever held for a few field
/// reads or writes and never across an `.await`, so it cannot deadlock and
/// a slow Slack never makes a reader wait.
struct AuthState {
    /// The token every call uses. Only a refresh replaces it, and refreshes
    /// take turns (see [`Shared::refreshing`]), so no renewal overwrites a
    /// newer one.
    token: Token,
    /// The last refresh's failure, so the calls after it wait it out
    /// rather than each asking Slack again. Cleared by a refresh that works
    /// and by a new app.
    failure: Option<RefreshFailure>,
    /// The app that renews the token, `None` when it cannot be renewed.
    app: Option<OauthApp>,
    /// Counts changes of `app`, so a refresh that started with the old app
    /// and fails cannot hold its failure against the new one.
    app_changes: u64,
    /// Hears each refresh's outcome, to save the new token. Read when the
    /// outcome is reported, not when the refresh starts, so a client told
    /// to stop reporting mid-refresh does not save afterwards.
    on_refresh: Option<OnRefresh>,
    /// The scopes Slack last said the token has (an app's token only).
    scopes: Option<Scopes>,
}

/// What [`AuthState::plan`] says a call needing the token should do.
enum Plan {
    /// Go on with this token, or stop with this error, without Slack.
    Ready(Result<String, SlackError>),
    /// Ask Slack for a new token.
    Refresh {
        app: OauthApp,
        refresh: String,
        /// The token being renewed, to fall back on if that fails.
        old: Token,
        /// [`AuthState::app_changes`] when the refresh started.
        app_changes: u64,
    },
}

impl AuthState {
    fn new(token: Token) -> Self {
        Self {
            token,
            failure: None,
            app: None,
            app_changes: 0,
            on_refresh: None,
            scopes: None,
        }
    }

    /// Whether the token needs renewing at `now` (Unix seconds) and, if a
    /// refresh failed lately, whether `instant` is past its pause.
    fn plan(&self, now: i64, instant: Instant) -> Plan {
        let token = &self.token;
        if !token.needs_refresh(now) {
            return Plan::Ready(Ok(token.access.clone()));
        }
        let (Some(app), Some(refresh)) = (&self.app, &token.refresh) else {
            return Plan::Ready(Ok(token.access.clone()));
        };
        // A refused refresh token stays refused, and a passing failure
        // gets a pause: either way, not one more try per API call.
        if let Some(failure) = &self.failure
            && (failure.error.is_auth() || instant < failure.retry_at)
        {
            return Plan::Ready(fallback(token, failure.error.clone(), now));
        }
        Plan::Refresh {
            app: app.clone(),
            refresh: refresh.clone(),
            old: token.clone(),
            app_changes: self.app_changes,
        }
    }

    /// Keeps what a refresh planned by [`AuthState::plan`] came to, and
    /// answers the token the waiting call should use.
    fn settle(
        &mut self,
        old: &Token,
        app_changes: u64,
        outcome: &Result<Token, SlackError>,
        now: i64,
        instant: Instant,
    ) -> Result<String, SlackError> {
        match outcome {
            Ok(renewed) => {
                self.token = renewed.clone();
                self.failure = None;
                Ok(renewed.access.clone())
            }
            Err(error) => {
                // A new app came in meanwhile: its fresh chance stands.
                if app_changes == self.app_changes {
                    let failures = self
                        .failure
                        .as_ref()
                        .map_or(1, |f| f.failures.saturating_add(1));
                    self.failure = Some(RefreshFailure {
                        error: error.clone(),
                        failures,
                        retry_at: instant + refresh_wait(failures),
                    });
                }
                fallback(old, error.clone(), now)
            }
        }
    }
}

/// What every clone of one workspace's client shares: one sign-in, one
/// refresh at a time, and one set of request slots. Clones handed to the
/// image loader or to tasks see a renewed token or a new app at once.
///
/// Two locks and a limiter, never nested the other way round:
/// `refreshing` may be held while `auth` is taken briefly, never the
/// reverse, and `limit` is never held while waiting for either.
struct Shared {
    /// The sign-in's state; see [`AuthState`].
    auth: Mutex<AuthState>,
    /// Held from deciding to refresh until the outcome has been reported,
    /// which keeps two promises:
    ///
    /// - Only one refresh asks Slack at a time. Calls that queue behind
    ///   it look again once it is through and use the token it got.
    /// - Reports go out one at a time, in the order Slack issued the
    ///   tokens, so the newest token is always the one saved last.
    ///
    /// Calls whose token is still good never take it, so only calls that
    /// arrived while a refresh was on wait for its report to be saved.
    refreshing: Arc<tokio::sync::Mutex<()>>,
    /// At most [`MAX_IN_FLIGHT`] requests at once. A limiter, not a lock:
    /// a permit is taken after the token is in hand and covers one
    /// attempt, never a pause between attempts.
    limit: Semaphore,
    /// What asks Slack for a new token; `None` for the real thing.
    refresher: Option<Refresher>,
}

/// One workspace's API access.
#[derive(Clone)]
pub struct Client {
    /// A client of its own (a browser-like agent, a test), or `None` for
    /// [`super::net::api`], taken afresh for each call so a new proxy
    /// setting applies at once.
    http: Option<reqwest::Client>,
    shared: Arc<Shared>,
    base: String,
}

impl std::fmt::Debug for Client {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Client").finish_non_exhaustive()
    }
}

/// The shared HTTP client for Web API calls, through the current proxy
/// (see [`super::net`]).
pub fn http() -> reqwest::Client {
    super::net::api()
}

/// The client for uploads and downloads, through the current proxy.
fn transfers() -> reqwest::Client {
    super::net::transfers()
}

pub fn now() -> i64 {
    jiff::Timestamp::now().as_second()
}

impl Client {
    /// A client that goes through `http` alone: a browser-like one, or
    /// a test's.
    pub fn new(http: reqwest::Client, token: Token) -> Self {
        Self::with_http(Some(http), token)
    }

    /// A client on the app's shared HTTP client, which follows the proxy
    /// setting as it changes.
    pub fn shared(token: Token) -> Self {
        Self::with_http(None, token)
    }

    fn with_http(http: Option<reqwest::Client>, token: Token) -> Self {
        Self::with_parts(http, token, None)
    }

    fn with_parts(
        http: Option<reqwest::Client>,
        token: Token,
        refresher: Option<Refresher>,
    ) -> Self {
        Self {
            http,
            shared: Arc::new(Shared {
                auth: Mutex::new(AuthState::new(token)),
                refreshing: Arc::new(tokio::sync::Mutex::new(())),
                limit: Semaphore::new(MAX_IN_FLIGHT),
                refresher,
            }),
            base: API.to_owned(),
        }
    }

    /// Renews a rotating token with `app`, reporting each outcome: the new
    /// token, or why the refresh failed. Reports go out one at a time, in
    /// the order Slack issued the tokens.
    pub fn with_refresh<F, Fut>(self, app: Option<OauthApp>, on_refresh: F) -> Self
    where
        F: Fn(Result<Token, SlackError>) -> Fut + Send + Sync + 'static,
        Fut: std::future::Future<Output = ()> + Send + 'static,
    {
        self.set_app(app);
        let on_refresh: OnRefresh = Arc::new(move |result| Box::pin(on_refresh(result)));
        self.auth().on_refresh = Some(on_refresh);
        self
    }

    /// Stops reporting refreshes, for a client on its way out: a token
    /// renewed from now on stays in memory and is never saved. A refresh
    /// already asking Slack looks for the reporter only once it has an
    /// answer, so it is not saved either.
    pub fn stop_reporting(&self) {
        self.auth().on_refresh = None;
    }

    /// Switches the app that renews the token, for this client and every
    /// clone of it. A new app also gets a fresh chance to refresh.
    pub fn set_app(&self, app: Option<OauthApp>) {
        let mut auth = self.auth();
        auth.app = app;
        auth.app_changes = auth.app_changes.wrapping_add(1);
        auth.failure = None;
    }

    /// The HTTP client this call should use.
    pub fn http(&self) -> reqwest::Client {
        self.http.clone().unwrap_or_else(super::net::api)
    }

    pub fn token(&self) -> Token {
        self.auth().token.clone()
    }

    /// The scopes this token has, as recorded at sign-in or as Slack's
    /// last answer said; `None` for a session or when not known.
    pub fn scopes(&self) -> Option<Scopes> {
        self.auth().scopes.clone()
    }

    /// Starts from the scopes recorded for this sign-in.
    pub fn set_scopes(&self, scopes: Option<Scopes>) {
        self.auth().scopes = scopes;
    }

    /// Whether this sign-in may call a method that needs `scope`, so a
    /// call Slack would refuse with `missing_scope` is not made at all.
    pub fn may(&self, scope: &str) -> bool {
        let auth = self.auth();
        crate::scopes::allows(auth.scopes.as_ref(), auth.token.is_session(), scope)
    }

    /// Keeps the list an answer's `x-oauth-scopes` header gives. Slack
    /// documents it: "a x-oauth-scopes HTTP header will be returned with
    /// every response indicating which scopes the calling token currently
    /// has". A session's token has no such list worth keeping.
    fn note_scopes(&self, header: Option<&reqwest::header::HeaderValue>) {
        let Some(scopes) = scopes_header(header) else {
            return;
        };
        let mut auth = self.auth();
        if !auth.token.is_session() {
            auth.scopes = Some(scopes);
        }
    }

    fn cookie(&self) -> Option<String> {
        self.auth().token.cookie.clone()
    }

    /// The sign-in's state, for a moment: the guard must be gone before
    /// the next `.await`.
    fn auth(&self) -> std::sync::MutexGuard<'_, AuthState> {
        lock(&self.shared.auth)
    }

    /// The access token for a call, renewed first when it is about to
    /// expire.
    async fn access_token(&self) -> Result<String, SlackError> {
        if let Plan::Ready(ready) = self.auth().plan(now(), Instant::now()) {
            return ready;
        }
        let refreshing = Arc::clone(&self.shared.refreshing).lock_owned().await;
        // Another call may have refreshed while this one waited.
        let (app, refresh, old, app_changes) = match self.auth().plan(now(), Instant::now()) {
            Plan::Ready(ready) => return ready,
            Plan::Refresh {
                app,
                refresh,
                old,
                app_changes,
            } => (app, refresh, old, app_changes),
        };
        // The refresh and its report run on a task of their own, which
        // keeps the lock until the report is out. A caller that gives up
        // half way (a task aborted when a view closes) cannot lose a
        // renewed token before it is saved: with a rotating token, the
        // saved refresh token may already be spent.
        let (answer, answered) = tokio::sync::oneshot::channel();
        let client = self.clone();
        tokio::spawn(async move {
            let outcome = match &client.shared.refresher {
                Some(refresher) => refresher(app, refresh).await,
                None => refresh_token(&client.http(), &client.base, &app, &refresh).await,
            };
            let (token, on_refresh) = {
                let mut auth = client.auth();
                let token = auth.settle(&old, app_changes, &outcome, now(), Instant::now());
                (token, auth.on_refresh.clone())
            };
            match &outcome {
                Ok(_) => log::info!("renewed a rotating Slack token"),
                Err(error) => log::warn!("could not renew a rotating Slack token: {error}"),
            }
            let _ = answer.send(token);
            if let Some(on_refresh) = on_refresh {
                on_refresh(outcome).await;
            }
            drop(refreshing);
        });
        answered
            .await
            .unwrap_or_else(|_| Err(SlackError::Network("token refresh stopped".into())))
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
            // The token comes first: a refresh can wait on Slack and on
            // another refresh, and must not hold a slot meanwhile.
            let token = self.access_token().await?;
            // The permit covers one attempt, not the waits between them:
            // a call sitting out a Retry-After must not hold a slot that a
            // send could use.
            let permit = self
                .shared
                .limit
                .acquire()
                .await
                .map_err(|_| SlackError::Network("client closed".into()))?;
            let mut request = self.http().post(&url).bearer_auth(&token).form(params);
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
                Err(error) => return Err(error.into()),
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
            self.note_scopes(response.headers().get("x-oauth-scopes"));
            let bytes = response.bytes().await?;
            drop(permit);
            return answer(status, &bytes);
        }
    }

    /// Downloads a file that needs the token (`url_private`). The token and
    /// cookie go only to Slack's file host; any other URL, which a bot or a
    /// link preview can choose, is fetched without them.
    pub async fn get_bytes(&self, url: &str, max: usize) -> Result<Vec<u8>, SlackError> {
        if !is_slack_file_url(url) {
            return get_bytes(&self.http(), url, None, None, max).await;
        }
        let token = self.access_token().await?;
        get_bytes(
            &self.http(),
            url,
            Some(&token),
            self.cookie().as_deref(),
            max,
        )
        .await
    }

    /// Starts downloading a file for saving, with the same rule for the
    /// token as [`Client::get_bytes`]. The body is left to the caller to
    /// read chunk by chunk, so a large file never sits in memory.
    pub async fn download(&self, url: &str) -> Result<reqwest::Response, SlackError> {
        if !is_slack_file_url(url) {
            return fetch(&transfers(), url, None, None).await;
        }
        let token = self.access_token().await?;
        fetch(&transfers(), url, Some(&token), self.cookie().as_deref()).await
    }

    /// Uploads a file with Slack's two-step external upload, streaming
    /// `length` bytes from `file` rather than reading it into memory.
    /// `progress` hears how many bytes have been read for sending so far.
    ///
    /// `finish` is asked once the bytes are up, just before Slack is told
    /// to share the file, which can't be taken back: it answers whether to
    /// go on. `Ok(false)` means it said no and nothing was posted.
    pub async fn upload(
        &self,
        channel: &str,
        thread: Option<&str>,
        outgoing: Outgoing<'_>,
        comment: &str,
        progress: impl Fn(u64) + Send + Sync + 'static,
        finish: impl FnOnce() -> bool,
    ) -> Result<bool, SlackError> {
        let Outgoing { file, length, name } = outgoing;
        let target: types::UploadUrl = self
            .act(
                "files.getUploadURLExternal",
                &[
                    ("filename", name.to_owned()),
                    ("length", length.to_string()),
                ],
            )
            .await?;
        let body = reqwest::Body::wrap_stream(counted(file, progress));
        let part =
            reqwest::multipart::Part::stream_with_length(body, length).file_name(name.to_owned());
        let form = reqwest::multipart::Form::new().part("file", part);
        let response = transfers()
            .post(&target.upload_url)
            .multipart(form)
            .send()
            .await?;
        if !response.status().is_success() {
            return Err(SlackError::Http(response.status().as_u16()));
        }
        if !finish() {
            return Ok(false);
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
        Ok(true)
    }
}

/// The file [`Client::upload`] sends: its open handle, its size in bytes
/// (Slack asks for it before the bytes come) and the name it is shared
/// under.
pub struct Outgoing<'a> {
    pub file: tokio::fs::File,
    pub length: u64,
    pub name: &'a str,
}

/// Where and what [`Client::add_emoji`] posts, but the picture: the
/// workspace's own `emoji.add` (as Slack's web client and the tools built
/// on it do), and the form fields `token`, `name` and `mode=data`. `None`
/// for a sign-in that is not a browser session, which Slack would refuse.
///
/// The workspace URL is used only when it is an `https` Slack address;
/// otherwise the call goes to `base`, so the token never leaves Slack.
pub fn emoji_add_request(
    token: &Token,
    base: &str,
    name: &str,
) -> Option<(String, Vec<(&'static str, String)>)> {
    if !token.is_session() {
        return None;
    }
    let workspace = token
        .workspace_url
        .as_deref()
        .and_then(|url| reqwest::Url::parse(url).ok())
        .filter(|url| {
            url.scheme() == "https"
                && url.port().is_none()
                && url.username().is_empty()
                && url.password().is_none()
                && url
                    .host_str()
                    .is_some_and(|host| host.ends_with(".slack.com"))
        })
        .and_then(|url| url.host_str().map(|host| format!("https://{host}/api/")));
    let url = format!("{}emoji.add", workspace.as_deref().unwrap_or(base));
    let fields = vec![
        ("token", token.access.clone()),
        ("name", name.to_owned()),
        ("mode", "data".to_owned()),
    ];
    Some((url, fields))
}

impl Client {
    /// Adds custom emoji `name` with `image` (`mime`, from `file_name`)
    /// the way Slack's web client does: a multipart `emoji.add` with the
    /// session's token and cookie. Not part of Slack's public API, so
    /// only browser sessions get here. Not retried: Slack may have added
    /// it before a connection broke.
    pub async fn add_emoji(
        &self,
        name: &str,
        image: Vec<u8>,
        file_name: &str,
        mime: &str,
    ) -> Result<(), SlackError> {
        let token = self.token();
        let Some((url, fields)) = emoji_add_request(&token, &self.base, name) else {
            return Err(SlackError::Api("not_allowed_token_type".into()));
        };
        let mut form = reqwest::multipart::Form::new();
        for (key, value) in fields {
            form = form.text(key, value);
        }
        let part = reqwest::multipart::Part::bytes(image)
            .file_name(file_name.to_owned())
            .mime_str(mime)
            .map_err(|e| SlackError::Decode(e.to_string()))?;
        form = form.part("image", part);
        let permit = self
            .shared
            .limit
            .acquire()
            .await
            .map_err(|_| SlackError::Network("client closed".into()))?;
        let mut request = self.http().post(&url).multipart(form);
        if let Some(cookie) = token.cookie.as_deref() {
            request = request.header(reqwest::header::COOKIE, format!("d={cookie}"));
        }
        let response = request.send().await?;
        let status = response.status().as_u16();
        if retry_after(status, response.headers().get("retry-after")).is_some() {
            return Err(SlackError::RateLimited);
        }
        let bytes = response.bytes().await?;
        drop(permit);
        answer::<serde_json::Value>(status, &bytes).map(|_| ())
    }
}

/// The bytes of `file` as a stream for a request body, telling `progress`
/// the running total after each chunk. A read error ends the stream after
/// it is passed on, which fails the request.
fn counted(
    file: tokio::fs::File,
    progress: impl Fn(u64) + Send + Sync + 'static,
) -> impl futures_util::Stream<Item = Result<Vec<u8>, std::io::Error>> + Send + 'static {
    use tokio::io::AsyncReadExt;
    const CHUNK: usize = 64 * 1024;
    futures_util::stream::unfold(
        (Some(file), 0u64, progress),
        |(file, sent, progress)| async move {
            let mut file = file?;
            let mut buffer = vec![0; CHUNK];
            match file.read(&mut buffer).await {
                Ok(0) => None,
                Ok(read) => {
                    buffer.truncate(read);
                    let sent = sent + read as u64;
                    progress(sent);
                    Some((Ok(buffer), (Some(file), sent, progress)))
                }
                Err(error) => Some((Err(error), (None, sent, progress))),
            }
        },
    )
}

/// Reads the answer to a Web API call that came back with HTTP `status`.
/// An error status still carries Slack's own JSON at times, whose code says
/// more than the status; anything else (an empty body, a proxy's or a load
/// balancer's HTML page) is the status alone, not a decoding error.
fn answer<T: DeserializeOwned>(status: u16, bytes: &[u8]) -> Result<T, SlackError> {
    if !(200..300).contains(&status) && !is_slack_answer(bytes) {
        return Err(SlackError::Http(status));
    }
    decode(bytes)
}

/// Whether `bytes` is a Web API answer: a JSON object with `ok`.
fn is_slack_answer(bytes: &[u8]) -> bool {
    serde_json::from_slice::<serde_json::Value>(bytes)
        .is_ok_and(|value| value.get("ok").is_some_and(serde_json::Value::is_boolean))
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
fn fallback(token: &Token, error: SlackError, now: i64) -> Result<String, SlackError> {
    let unexpired = token.expires_at.is_some_and(|at| now < at);
    if unexpired && !error.is_auth() {
        Ok(token.access.clone())
    } else {
        Err(error)
    }
}

/// The wait before retrying a request for the `attempt`-th time: from
/// half a second, doubling, up to 16 seconds.
fn backoff(attempt: u32) -> Duration {
    crate::retry::backoff(Duration::from_millis(500), Duration::from_secs(16), attempt)
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
    let mut response = fetch(http, url, token, cookie).await?;
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await? {
        bytes.extend_from_slice(&chunk);
        if bytes.len() > max {
            return Err(SlackError::Decode("file too large".into()));
        }
    }
    Ok(bytes)
}

/// Sends a GET for a file and checks the answer is the file: a success,
/// and not HTML (Slack answers a bad token on a file with its sign-in
/// page).
async fn fetch(
    http: &reqwest::Client,
    url: &str,
    token: Option<&str>,
    cookie: Option<&str>,
) -> Result<reqwest::Response, SlackError> {
    let mut request = http.get(url);
    if let Some(token) = token {
        request = request.bearer_auth(token);
    }
    if let Some(cookie) = cookie {
        request = request.header(reqwest::header::COOKIE, format!("d={cookie}"));
    }
    let response = request.send().await?;
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
    Ok(response)
}

/// Exchanges a refresh token for a new access token. Slack's PKCE flow
/// sends only the client id and the refresh token here: no secret, and no
/// verifier, which belongs to the first exchange alone.
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
            ("grant_type", "refresh_token"),
            ("refresh_token", refresh),
        ])
        .send()
        .await?;
    let bytes = response.bytes().await?;
    let access: types::OauthAccess = decode(&bytes)?;
    token_from(access).ok_or_else(|| SlackError::Decode("no user token in refresh".into()))
}

/// The scopes an `x-oauth-scopes` header lists, if it lists any.
pub fn scopes_header(header: Option<&reqwest::header::HeaderValue>) -> Option<Scopes> {
    let scopes = Scopes::parse(header?.to_str().ok()?);
    (!scopes.is_empty()).then_some(scopes)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn emoji_add_goes_to_the_workspace_with_the_web_clients_fields() {
        let session = Token::session("xoxc-1", "xoxd-2", "https://acme.slack.com/");
        let (url, fields) = emoji_add_request(&session, API, "shipit").expect("a session");
        assert_eq!(url, "https://acme.slack.com/api/emoji.add");
        assert_eq!(
            fields,
            vec![
                ("token", "xoxc-1".to_owned()),
                ("name", "shipit".to_owned()),
                ("mode", "data".to_owned()),
            ]
        );
        // The token goes to Slack only, whatever the saved address says.
        for odd in [
            "http://acme.slack.com",
            "https://acme.slack.com.evil.example",
            "https://evil.example",
            "https://user@acme.slack.com",
            "not a url",
        ] {
            let token = Token::session("xoxc-1", "xoxd-2", odd);
            let (url, _) = emoji_add_request(&token, API, "shipit").expect("a session");
            assert_eq!(url, "https://slack.com/api/emoji.add", "{odd}");
        }
        assert!(
            emoji_add_request(&Token::plain("xoxp-1"), API, "shipit").is_none(),
            "an OAuth token cannot add emoji"
        );
    }

    #[test]
    fn an_error_page_is_its_http_status() {
        let html = b"<html><body><h1>502 Bad Gateway</h1></body></html>";
        assert_eq!(
            answer::<serde_json::Value>(502, html),
            Err(SlackError::Http(502))
        );
        assert_eq!(
            answer::<serde_json::Value>(503, b""),
            Err(SlackError::Http(503))
        );
        assert_eq!(
            answer::<serde_json::Value>(500, br#"{"message":"oops"}"#),
            Err(SlackError::Http(500))
        );
        // Slack's own answer on an error status still gives its code.
        assert_eq!(
            answer::<serde_json::Value>(400, br#"{"ok":false,"error":"invalid_arguments"}"#),
            Err(SlackError::Api("invalid_arguments".into()))
        );
        // A success that cannot be read stays a decoding error.
        assert!(matches!(
            answer::<serde_json::Value>(200, html),
            Err(SlackError::Decode(_))
        ));
    }

    #[test]
    fn the_scopes_header_lists_what_the_token_has() {
        use reqwest::header::HeaderValue;
        let header = HeaderValue::from_static("identify,chat:write, dnd:read");
        let scopes = scopes_header(Some(&header)).expect("listed");
        assert!(scopes.has("chat:write") && scopes.has("dnd:read"));
        assert!(!scopes.has("dnd:write"));
        assert_eq!(scopes_header(Some(&HeaderValue::from_static(""))), None);
        assert_eq!(scopes_header(None), None);
    }

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
        };
        let printed = format!("{token:?} {app:?} {:#?}", token);
        assert!(!printed.contains("xox"), "{printed}");
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
        token.expires_at = Some(1_000);
        let outage = SlackError::Network("down".into());
        assert_eq!(
            fallback(&token, outage.clone(), 880),
            Ok("xoxe.xoxp-1".into())
        );
        let refused = SlackError::Api("invalid_refresh_token".into());
        assert_eq!(fallback(&token, refused.clone(), 880), Err(refused));
        assert_eq!(fallback(&token, outage.clone(), 1_001), Err(outage));
    }

    #[test]
    fn clones_share_one_app_and_token() {
        let client = Client::new(reqwest::Client::new(), Token::plain("xoxp-1"));
        let clone = client.clone();
        client.set_app(Some(OauthApp {
            client_id: "1.2".into(),
        }));
        assert!(clone.auth().app.is_some());
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

    /// A rotating token numbered `n` that is due for renewal whatever the
    /// time, and so is every token renewed from it in these tests.
    fn due(n: usize) -> Token {
        Token {
            access: format!("xoxe.xoxp-{n}"),
            refresh: Some(format!("xoxe-{n}")),
            expires_at: Some(0),
            cookie: None,
            workspace_url: None,
        }
    }

    fn app() -> Option<OauthApp> {
        Some(OauthApp {
            client_id: "1.2".into(),
        })
    }

    /// A client whose refreshes go to `refresher` and never to Slack.
    fn pretend(token: Token, refresher: Refresher) -> Client {
        let client = Client::with_parts(None, token, Some(refresher));
        client.set_app(app());
        client
    }

    /// The refresh tokens a pretend refresher was handed, in order.
    type Seen = Arc<Mutex<Vec<String>>>;

    /// What a pretend reporter heard: each access token, or the error.
    type Reports = Arc<Mutex<Vec<Result<String, SlackError>>>>;

    /// A refresher that waits for `gate`, then hands out the next token
    /// in the numbering, keeping which refresh token it was given.
    fn numbered(gate: Arc<tokio::sync::Notify>) -> (Refresher, Seen) {
        let seen: Seen = Arc::default();
        let kept = Arc::clone(&seen);
        let refresher: Refresher = Arc::new(move |_app, refresh: String| {
            let gate = Arc::clone(&gate);
            let seen = Arc::clone(&kept);
            Box::pin(async move {
                gate.notified().await;
                let n = {
                    let mut seen = lock(&seen);
                    seen.push(refresh);
                    seen.len()
                };
                Ok(due(n + 1))
            })
        });
        (refresher, seen)
    }

    /// A reporter that keeps each outcome, after dawdling so a report out
    /// of turn would have room to overtake.
    fn reporter(client: Client) -> (Client, Reports) {
        let reports: Reports = Arc::default();
        let kept = Arc::clone(&reports);
        let client = client.with_refresh(app(), move |outcome| {
            let reports = Arc::clone(&kept);
            async move {
                for _ in 0..10 {
                    tokio::task::yield_now().await;
                }
                lock(&reports).push(outcome.map(|token| token.access));
            }
        });
        (client, reports)
    }

    /// Lets every spawned task run until it waits on something.
    async fn settle_tasks() {
        for _ in 0..50 {
            tokio::task::yield_now().await;
        }
    }

    /// Waits until no refresh or report is going on.
    async fn quiet(client: &Client) {
        drop(client.shared.refreshing.lock().await);
    }

    fn counter() -> Arc<std::sync::atomic::AtomicUsize> {
        Arc::new(std::sync::atomic::AtomicUsize::new(0))
    }

    fn count(counter: &std::sync::atomic::AtomicUsize) -> usize {
        counter.load(std::sync::atomic::Ordering::SeqCst)
    }

    #[tokio::test]
    async fn calls_waiting_on_a_refresh_use_its_token() {
        let gate = Arc::new(tokio::sync::Notify::new());
        let refreshes = counter();
        let counted = Arc::clone(&refreshes);
        let opened = Arc::clone(&gate);
        let refresher: Refresher = Arc::new(move |_app, _refresh| {
            counted.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let gate = Arc::clone(&opened);
            Box::pin(async move {
                gate.notified().await;
                let mut renewed = due(2);
                renewed.expires_at = Some(i64::MAX);
                Ok(renewed)
            })
        });
        let client = pretend(due(1), refresher);
        let calls: Vec<_> = (0..5)
            .map(|_| {
                let client = client.clone();
                tokio::spawn(async move { client.access_token().await })
            })
            .collect();
        settle_tasks().await;
        gate.notify_one();
        for call in calls {
            assert_eq!(call.await.expect("joins"), Ok("xoxe.xoxp-2".into()));
        }
        assert_eq!(count(&refreshes), 1);
    }

    #[tokio::test]
    async fn refreshes_take_turns_and_report_in_the_order_issued() {
        let gate = Arc::new(tokio::sync::Notify::new());
        let (refresher, seen) = numbered(Arc::clone(&gate));
        let (client, reports) = reporter(pretend(due(1), refresher));
        // Every renewed token is due again, so each call refreshes.
        let calls: Vec<_> = (0..4)
            .map(|_| {
                let client = client.clone();
                tokio::spawn(async move { client.access_token().await })
            })
            .collect();
        for _ in 0..4 {
            settle_tasks().await;
            gate.notify_one();
        }
        for call in calls {
            assert!(call.await.expect("joins").is_ok());
        }
        quiet(&client).await;
        // Each refresh was handed the token the one before it got.
        assert_eq!(*lock(&seen), ["xoxe-1", "xoxe-2", "xoxe-3", "xoxe-4"]);
        let issued: Vec<Result<String, SlackError>> =
            (2..=5).map(|n| Ok(format!("xoxe.xoxp-{n}"))).collect();
        assert_eq!(*lock(&reports), issued);
        assert_eq!(client.token().access, "xoxe.xoxp-5");
    }

    #[tokio::test]
    async fn a_renewed_token_is_reported_when_its_caller_gives_up() {
        let gate = Arc::new(tokio::sync::Notify::new());
        let (refresher, _) = numbered(Arc::clone(&gate));
        let (client, reports) = reporter(pretend(due(1), refresher));
        let caller = client.clone();
        let call = tokio::spawn(async move { caller.access_token().await });
        settle_tasks().await;
        call.abort();
        gate.notify_one();
        settle_tasks().await;
        quiet(&client).await;
        assert_eq!(*lock(&reports), [Ok("xoxe.xoxp-2".to_owned())]);
        assert_eq!(client.token().access, "xoxe.xoxp-2");
    }

    #[tokio::test]
    async fn a_refresh_under_way_is_not_reported_after_stopping() {
        let gate = Arc::new(tokio::sync::Notify::new());
        let (refresher, _) = numbered(Arc::clone(&gate));
        let (client, reports) = reporter(pretend(due(1), refresher));
        let caller = client.clone();
        let call = tokio::spawn(async move { caller.access_token().await });
        settle_tasks().await;
        client.stop_reporting();
        gate.notify_one();
        assert_eq!(call.await.expect("joins"), Ok("xoxe.xoxp-2".into()));
        quiet(&client).await;
        assert!(lock(&reports).is_empty());
        // Still used until the client goes, just never saved.
        assert_eq!(client.token().access, "xoxe.xoxp-2");
    }

    #[tokio::test]
    async fn a_failed_refresh_is_tried_once_then_waited_out() {
        let refreshes = counter();
        let counted = Arc::clone(&refreshes);
        let refresher: Refresher = Arc::new(move |_app, _refresh| {
            counted.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Box::pin(async { Err(SlackError::Network("down".into())) })
        });
        let (client, reports) = reporter(pretend(due(1), refresher));
        let outage: Result<String, SlackError> = Err(SlackError::Network("down".into()));
        assert_eq!(client.access_token().await, outage);
        assert_eq!(client.access_token().await, outage);
        assert_eq!(count(&refreshes), 1);
        quiet(&client).await;
        assert_eq!(*lock(&reports), [outage]);
    }

    #[test]
    fn refresh_failures_pause_and_a_new_app_starts_afresh() {
        let client = Client::with_parts(None, due(1), None);
        client.set_app(app());
        let start = Instant::now();
        let plan = |at: Instant| client.auth().plan(0, at);
        let Plan::Refresh {
            old, app_changes, ..
        } = plan(start)
        else {
            panic!("a due token with an app is refreshed");
        };
        let outage = SlackError::Network("down".into());
        let _ = client
            .auth()
            .settle(&old, app_changes, &Err(outage.clone()), 0, start);
        assert!(matches!(plan(start), Plan::Ready(Err(ref e)) if *e == outage));
        assert!(matches!(
            plan(start + REFRESH_RETRY_FIRST),
            Plan::Refresh { .. }
        ));
        // A refused refresh token stays refused, however long it waits.
        let refused = SlackError::Api("invalid_refresh_token".into());
        let _ = client
            .auth()
            .settle(&old, app_changes, &Err(refused.clone()), 0, start);
        assert!(matches!(
            plan(start + REFRESH_RETRY_MAX * 2),
            Plan::Ready(Err(ref e)) if *e == refused
        ));
        // A new app gets its chance at once, and a refresh still out with
        // the old app cannot take it away when it fails.
        client.set_app(app());
        let _ = client
            .auth()
            .settle(&old, app_changes, &Err(refused), 0, start);
        assert!(matches!(plan(start), Plan::Refresh { .. }));
        // Without an app the token is used as it is until Slack says no.
        client.set_app(None);
        assert!(matches!(plan(start), Plan::Ready(Ok(_))));
    }
}
