//! HTTP client for Microsoft Teams native web APIs.
//!
//! Handles calling ChatSvc, CSA, and Middle-Tier endpoints with the appropriate
//! SkypeToken and Bearer token headers. Never logs or exposes tokens in errors.

use std::sync::{Arc, RwLock};

use futures_util::future::BoxFuture;

use crate::failure::Failure;
use crate::teams::auth::{Account, AudienceToken, TeamsCredentials, now_secs};

mod channels;
mod chat;
mod media;
mod people;
mod presence;

pub use channels::Post;
pub use chat::{Author, HistoryPage, MadeMeeting};
pub use media::is_media_url;
pub use people::user_mri;

/// Now, in epoch milliseconds, as Teams stamps reactions and edits.
fn now_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or_default()
}

/// Logs why the chat service refused `what`, by its own error code and
/// message (it does not echo what was sent), and answers the failure.
pub(crate) async fn refused(resp: reqwest::Response, what: &str) -> Failure {
    let status = resp.status();
    let body = resp.text().await.unwrap_or_default();
    let message = serde_json::from_str::<serde_json::Value>(&body)
        .ok()
        .and_then(|v| v.get("message").and_then(|m| m.as_str()).map(str::to_owned))
        .unwrap_or_default();
    log::warn!(
        "Teams refused to {what}: HTTP {status} ({}) {}",
        crate::teams::auth::error_code(&body),
        message.chars().take(200).collect::<String>()
    );
    // Said as the rate limit it is, which passes.
    if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
        return Failure::RateLimited;
    }
    Failure::Http(status.as_u16())
}

/// Why a service answered 401, as its `WWW-Authenticate` header says
/// (the scheme and error, never a token): "unknown" without one.
fn why_unauthorized(resp: &reqwest::Response) -> String {
    resp.headers()
        .get(reqwest::header::WWW_AUTHENTICATE)
        .and_then(|value| value.to_str().ok())
        .map(|value| value.chars().take(200).collect())
        .unwrap_or_else(|| "unknown".to_owned())
}

/// Whether a picture's host turned its sign-in down, so another way of
/// signing in is worth a try.
fn is_refusal(status: reqwest::StatusCode) -> bool {
    matches!(status.as_u16(), 401 | 403)
}

/// The site the personal web client runs at, which some of its services
/// want named as the request's origin.
const PERSONAL_ORIGIN: &str = "https://teams.live.com";

/// The id messages name a person by, from their MRI: the object id of
/// `8:orgid:{id}`, and `live:…` of `8:live:…`.
fn id_of_mri(mri: &str) -> String {
    ["8:orgid:", "8:teamsvisitor:", "8:guest:", "8:"]
        .iter()
        .find_map(|prefix| mri.strip_prefix(prefix))
        .unwrap_or(mri)
        .to_owned()
}

/// What a refresh hands to whoever keeps the credentials: the renewed
/// credentials, or why they could not be renewed.
type OnRefresh =
    Arc<dyn Fn(Result<TeamsCredentials, Failure>) -> BoxFuture<'static, ()> + Send + Sync>;

/// Authenticated client for Microsoft Teams APIs.
#[derive(Clone)]
pub struct TeamsClient {
    http: reqwest::Client,
    credentials: Arc<RwLock<TeamsCredentials>>,
    /// Held across a refresh and the save that follows it. Two requests
    /// refused at once then refresh once: Microsoft rotates refresh tokens,
    /// so the second would spend one already used, and two saves racing
    /// could leave the older token in the keyring.
    refreshing: Arc<tokio::sync::Mutex<()>>,
    on_refresh: Option<OnRefresh>,
    /// Cleared on sign-out, so a refresh still running does not save its
    /// token back after the sign-in was deleted.
    reporting: Arc<std::sync::atomic::AtomicBool>,
    /// Your name, as messages sent from here carry it, once known.
    own_name: Arc<RwLock<Option<String>>>,
    /// This app's endpoint id with Teams (its Trouter connection's), which
    /// the presence service wants with every request.
    endpoint: Arc<RwLock<Option<String>>>,
    /// The people already looked up (see [`Self::not_yet_asked`]).
    asked: Arc<std::sync::Mutex<std::collections::HashSet<String>>>,
    /// The cookies that let pictures be fetched (avatars; pictures in
    /// messages where the token header is not taken), by the host that
    /// set them.
    media_cookies: Arc<RwLock<std::collections::HashMap<String, String>>>,
    /// Where this account's call pushes go (see
    /// [`crate::teams::calling::router`]).
    calls: crate::teams::calling::router::Router,
    /// Where people's photos are (their profiles' `imageUri`), by the id
    /// the interface knows them by: every picture link asked for them
    /// carries it, whichever list they arrive in.
    photos: Arc<RwLock<std::collections::HashMap<String, String>>>,
    /// Each channel's team, from the teams list: the CSA reads a channel
    /// by both.
    channel_teams: Arc<RwLock<std::collections::HashMap<String, String>>>,
    /// Held while a cookie is asked for, so the pictures of a whole
    /// screen ask once rather than each.
    cookie_asked: Arc<tokio::sync::Mutex<()>>,
}

impl std::fmt::Debug for TeamsClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TeamsClient").finish()
    }
}

impl TeamsClient {
    /// Creates a new Teams client with the given credentials.
    pub fn new(credentials: TeamsCredentials) -> Self {
        Self {
            http: crate::slack::net::api(),
            credentials: Arc::new(RwLock::new(credentials)),
            refreshing: Arc::new(tokio::sync::Mutex::new(())),
            on_refresh: None,
            reporting: Arc::new(std::sync::atomic::AtomicBool::new(true)),
            own_name: Arc::new(RwLock::new(None)),
            asked: Arc::new(std::sync::Mutex::new(std::collections::HashSet::new())),
            endpoint: Arc::new(RwLock::new(None)),
            media_cookies: Arc::new(RwLock::new(std::collections::HashMap::new())),
            calls: crate::teams::calling::router::Router::default(),
            photos: Arc::default(),
            channel_teams: Arc::default(),
            cookie_asked: Arc::new(tokio::sync::Mutex::new(())),
        }
    }

    /// Calls `save` after every refresh, while still holding the refresh
    /// lock, so saves happen in the order the tokens were issued.
    pub fn with_save<F, Fut>(mut self, save: F) -> Self
    where
        F: Fn(Result<TeamsCredentials, Failure>) -> Fut + Send + Sync + 'static,
        Fut: std::future::Future<Output = ()> + Send + 'static,
    {
        self.on_refresh = Some(Arc::new(move |result| Box::pin(save(result))));
        self
    }

    /// Of `ids`, those not looked up before, now marked as asked: the
    /// people met in reactions and system lines are looked up once each,
    /// however often they appear.
    pub fn not_yet_asked(&self, ids: impl IntoIterator<Item = String>) -> Vec<String> {
        let Ok(mut asked) = self.asked.lock() else {
            return Vec::new();
        };
        ids.into_iter()
            .filter(|id| asked.insert(id.clone()))
            .collect()
    }

    /// Your name, once known (see [`Self::set_own_name`]).
    pub fn own_name(&self) -> Option<String> {
        self.own_name.read().ok().and_then(|name| name.clone())
    }

    /// Remembers your name for the messages sent from here.
    pub fn set_own_name(&self, name: String) {
        if let Ok(mut held) = self.own_name.write() {
            *held = Some(name);
        }
    }

    /// This app's endpoint id with Teams, once the live connection has
    /// said we are here: calls name it as theirs.
    pub fn endpoint_id(&self) -> Option<String> {
        self.endpoint.read().ok().and_then(|e| e.clone())
    }

    /// Where this account's call pushes go, and the live connection's
    /// address for their callbacks.
    pub fn calls(&self) -> &crate::teams::calling::router::Router {
        &self.calls
    }

    /// Stops handing refreshes to the save callback, for good: the
    /// workspace signed out.
    pub fn stop_reporting(&self) {
        self.reporting
            .store(false, std::sync::atomic::Ordering::SeqCst);
    }

    /// Updates the credentials (e.g. after a token refresh).
    pub fn update_credentials(&self, creds: TeamsCredentials) {
        if let Ok(mut lock) = self.credentials.write() {
            *lock = creds;
        }
    }

    /// Current snapshot of credentials.
    pub fn credentials(&self) -> TeamsCredentials {
        self.credentials
            .read()
            .map(|c| c.clone())
            .unwrap_or_default()
    }

    /// Renews the credentials with `renew`, unless `stale` says the ones
    /// held now no longer need it: another task may have renewed them
    /// while this one waited for the lock.
    async fn refresh_if<R, Fut>(
        &self,
        stale: impl Fn(&TeamsCredentials) -> bool,
        renew: R,
    ) -> Result<TeamsCredentials, Failure>
    where
        R: FnOnce(reqwest::Client, TeamsCredentials) -> Fut,
        Fut: std::future::Future<Output = Result<TeamsCredentials, Failure>>,
    {
        let _held = self.refreshing.lock().await;
        let current = self.credentials();
        if !stale(&current) {
            return Ok(current);
        }
        let result = renew(self.http.clone(), current).await;
        if let Ok(renewed) = &result {
            self.update_credentials(renewed.clone());
        }
        if let Some(on_refresh) = &self.on_refresh
            && self.reporting.load(std::sync::atomic::Ordering::SeqCst)
        {
            on_refresh(result.clone()).await;
        }
        result
    }

    /// Renews the skype token, unless it is no longer the one `used`.
    async fn renew_skype(&self, used: Option<&str>) -> Result<TeamsCredentials, Failure> {
        self.refresh_if(
            |creds| creds.skype_token.as_deref().is_none_or(|t| Some(t) == used),
            |http, creds| async move { crate::teams::auth::refresh_credentials(&http, &creds).await },
        )
        .await
    }

    /// Checks if access token or SkypeToken is expired/missing and refreshes if needed.
    pub async fn ensure_fresh_tokens(&self) -> Result<TeamsCredentials, Failure> {
        let stale = |creds: &TeamsCredentials| {
            creds.is_expired(now_secs()) || creds.skype_token.as_deref().is_none_or(str::is_empty)
        };
        let creds = self.credentials();
        if !stale(&creds) {
            return Ok(creds);
        }
        self.refresh_if(stale, |http, creds| async move {
            crate::teams::auth::refresh_credentials(&http, &creds).await
        })
        .await
    }

    /// Renews the skype token after the one `used` was refused.
    pub async fn force_refresh(&self, used: &str) -> Result<TeamsCredentials, Failure> {
        self.renew_skype(Some(used)).await
    }

    /// A bearer token for `audience`, minted from the refresh token when
    /// there is none or `refused` was turned down.
    async fn token_for(&self, audience: &str, refused: Option<&str>) -> Result<String, Failure> {
        let usable = |creds: &TeamsCredentials| {
            creds
                .fresh_token_for(audience, now_secs())
                .filter(|token| Some(*token) != refused)
                .map(str::to_owned)
        };
        if let Some(token) = usable(&self.credentials()) {
            return Ok(token);
        }
        let audience = audience.to_owned();
        let creds = self
            .refresh_if(
                |creds| usable(creds).is_none(),
                |http, mut creds| async move {
                    let minted = crate::teams::auth::redeem(&http, &creds, &audience).await?;
                    if let Some(rotated) = minted.refresh_token {
                        creds.refresh_token = Some(rotated.0);
                    }
                    creds.audiences.insert(
                        audience,
                        AudienceToken {
                            token: minted.access_token,
                            expires_at: minted.expires_in.map(|s| now_secs() + s),
                        },
                    );
                    Ok(creds)
                },
            )
            .await?;
        usable(&creds).ok_or(Failure::SignedOut)
    }

    /// Sends a bearer-authed request for `audience`, minting a fresh token
    /// once if the first is refused.
    async fn bearer<F>(&self, audience: &str, make_request: F) -> Result<reqwest::Response, Failure>
    where
        F: Fn(&reqwest::Client, &str) -> reqwest::RequestBuilder,
    {
        let mut refused = None;
        loop {
            let token = self.token_for(audience, refused.as_deref()).await?;
            let resp = make_request(&self.http, &token)
                .send()
                .await
                .map_err(|e| Failure::Network(e.without_url().to_string()))?;
            if resp.status() != reqwest::StatusCode::UNAUTHORIZED || refused.is_some() {
                return Ok(resp);
            }
            log::info!("{audience} refused its token, minting another");
            refused = Some(token);
        }
    }

    /// Executes an HTTP request with automatic token refresh on HTTP 401.
    async fn authed_skype_request<F>(&self, make_request: F) -> Result<reqwest::Response, Failure>
    where
        F: Fn(&reqwest::Client, &str) -> reqwest::RequestBuilder,
    {
        let token = self.skype_token()?;
        let resp = self
            .send_as_account(make_request(&self.http, &token))
            .await?;

        if resp.status() == reqwest::StatusCode::UNAUTHORIZED {
            log::info!("Teams API returned 401 Unauthorized, refreshing token...");
            if let Ok(new_creds) = self.force_refresh(&token).await
                && let Some(new_token) = new_creds.skype_token
            {
                return self
                    .send_as_account(make_request(&self.http, &new_token))
                    .await;
            }
        }
        Ok(resp)
    }

    /// Sends `request` with what this kind of account's services expect:
    /// the personal ones get the headers Teams' personal client sends.
    async fn send_as_account(
        &self,
        request: reqwest::RequestBuilder,
    ) -> Result<reqwest::Response, Failure> {
        let request = match self.credentials().account {
            Account::Work => request,
            Account::Personal => crate::teams::auth::consumer_headers(request),
        };
        request
            .send()
            .await
            .map_err(|e| Failure::Network(e.without_url().to_string()))
    }

    /// The HTTP client every request of this sign-in goes through, for the
    /// call signalling, which sends its own headers.
    pub(crate) fn http(&self) -> &reqwest::Client {
        &self.http
    }

    /// Sends a request with the skype token as it is, renewing the token
    /// once on HTTP 401, but without the personal client's extra headers:
    /// the calling services were recorded without them.
    pub(crate) async fn plain_skype_request<F>(
        &self,
        make_request: F,
    ) -> Result<reqwest::Response, Failure>
    where
        F: Fn(&reqwest::Client, &str) -> reqwest::RequestBuilder,
    {
        let send = |request: reqwest::RequestBuilder| async move {
            request
                .send()
                .await
                .map_err(|e| Failure::Network(e.without_url().to_string()))
        };
        let token = self
            .ensure_fresh_tokens()
            .await?
            .skype_token
            .filter(|t| !t.is_empty())
            .ok_or(Failure::NoSavedSignIn)?;
        let resp = send(make_request(&self.http, &token)).await?;
        if resp.status() == reqwest::StatusCode::UNAUTHORIZED
            && let Ok(renewed) = self.force_refresh(&token).await
            && let Some(new_token) = renewed.skype_token
        {
            return send(make_request(&self.http, &new_token)).await;
        }
        Ok(resp)
    }

    fn skype_token(&self) -> Result<String, Failure> {
        let creds = self.credentials();
        creds
            .skype_token
            .filter(|t| !t.is_empty())
            .ok_or(Failure::NoSavedSignIn)
    }

    /// A channel post's reply chain: the channel with `;messageid=` and
    /// the post's id, which stays as it is (encoded, Teams would not know
    /// it), as the web client addresses a post's replies.
    fn reply_chain_url(&self, channel: &str, post: &str) -> String {
        format!("{};messageid={post}", self.conversation_url(channel))
    }

    /// A conversation's address on the chat service; its id holds `:` and
    /// `@`, so it is escaped, as the messages page always was.
    fn conversation_url(&self, chat_id: &str) -> String {
        format!(
            "{}/v1/users/ME/conversations/{}",
            self.chat_service_url(),
            crate::percent::encode_strict(chat_id)
        )
    }

    fn chat_service_url(&self) -> String {
        self.credentials()
            .chat_service_url()
            .trim_end_matches('/')
            .to_owned()
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;

    fn creds(skype: &str) -> TeamsCredentials {
        TeamsCredentials {
            access_token: "aad".into(),
            refresh_token: Some("rt".into()),
            skype_token: Some(skype.into()),
            ..TeamsCredentials::default()
        }
    }

    #[tokio::test]
    async fn two_refused_requests_refresh_once() {
        let saved = Arc::new(AtomicUsize::new(0));
        let counted = saved.clone();
        let client = TeamsClient::new(creds("old")).with_save(move |result| {
            let counted = counted.clone();
            async move {
                if result.is_ok() {
                    counted.fetch_add(1, Ordering::SeqCst);
                }
            }
        });
        let renewals = Arc::new(AtomicUsize::new(0));
        let renew = |renewals: Arc<AtomicUsize>| {
            move |_http, creds: TeamsCredentials| async move {
                renewals.fetch_add(1, Ordering::SeqCst);
                tokio::task::yield_now().await;
                Ok(TeamsCredentials {
                    skype_token: Some("new".into()),
                    refresh_token: Some("rt2".into()),
                    ..creds
                })
            }
        };
        // Both were refused while holding "old".
        let stale = |c: &TeamsCredentials| c.skype_token.as_deref() == Some("old");
        let (a, b) = tokio::join!(
            client.refresh_if(stale, renew(renewals.clone())),
            client.refresh_if(stale, renew(renewals.clone())),
        );
        assert_eq!(a.expect("renewed").skype_token.as_deref(), Some("new"));
        assert_eq!(b.expect("renewed").skype_token.as_deref(), Some("new"));
        assert_eq!(renewals.load(Ordering::SeqCst), 1);
        assert_eq!(saved.load(Ordering::SeqCst), 1);
        assert_eq!(client.credentials().refresh_token.as_deref(), Some("rt2"));
    }

    #[tokio::test]
    async fn a_failed_refresh_is_reported_and_keeps_the_credentials() {
        let reported = Arc::new(std::sync::Mutex::new(None));
        let seen = reported.clone();
        let client = TeamsClient::new(creds("old")).with_save(move |result| {
            if let Ok(mut seen) = seen.lock() {
                *seen = Some(result.err());
            }
            async {}
        });
        let result = client
            .refresh_if(|_| true, |_, _| async { Err(Failure::SignedOut) })
            .await;
        assert_eq!(result, Err(Failure::SignedOut));
        assert_eq!(client.credentials().skype_token.as_deref(), Some("old"));
        let reported = reported.lock().map(|r| r.clone()).ok().flatten();
        assert_eq!(reported, Some(Some(Failure::SignedOut)));
    }

    #[tokio::test]
    async fn a_signed_out_client_saves_nothing() {
        let saved = Arc::new(AtomicUsize::new(0));
        let counted = saved.clone();
        let client = TeamsClient::new(creds("old")).with_save(move |_| {
            counted.fetch_add(1, Ordering::SeqCst);
            async {}
        });
        client.stop_reporting();
        let renewed = client
            .refresh_if(|_| true, |_, creds| async move { Ok(creds) })
            .await;
        assert!(renewed.is_ok());
        assert_eq!(saved.load(Ordering::SeqCst), 0);
    }
}
