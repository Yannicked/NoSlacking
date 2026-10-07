//! HTTP client for Microsoft Teams native web APIs.
//!
//! Handles calling ChatSvc, CSA, and Middle-Tier endpoints with the appropriate
//! SkypeToken and Bearer token headers. Never logs or exposes tokens in errors.

use std::sync::{Arc, RwLock};

use futures_util::future::BoxFuture;

use crate::failure::Failure;
use crate::teams::auth::{
    Account, AudienceToken, RESOURCE_CSA, RESOURCE_GRAPH, RESOURCE_GROUPS_PERSONAL, RESOURCE_IC3,
    RESOURCE_MT_PERSONAL, RESOURCE_PRESENCE, TeamsCredentials, now_secs,
};
use crate::teams::types::{
    Conversation, ConversationsResponse, Message, MessagesResponse, PostedMessage, Team,
    TeamsResponse, UserDetails,
};

/// Who a message is from, as the chat service's message bodies name them.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Author {
    /// The id messages name you by (`live:…` or an object id).
    pub id: String,
    /// Your name, which Teams shows with the message, if known.
    pub name: Option<String>,
}

/// A message as the Teams web client sends it, less what each call adds.
fn message_body(chat_id: &str, html_content: &str, me: &Author) -> serde_json::Value {
    let mri = user_mri(&me.id);
    serde_json::json!({
        "type": "Message",
        "conversationid": chat_id,
        "from": mri,
        "fromUserId": mri,
        "content": html_content,
        "messagetype": "RichText/Html",
        "contenttype": "Text",
        "imdisplayname": me.name.clone().unwrap_or_default(),
        "amsreferences": [],
        "properties": {
            "importance": "",
            "subject": "",
            "title": "",
            "cards": "[]",
            "links": "[]",
            "mentions": "[]",
            "files": "[]",
            "formatVariant": "TEAMS"
        }
    })
}

/// Now, in epoch milliseconds, as Teams stamps reactions and edits.
fn now_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or_default()
}

/// The chat service's `clientmessageid` for one of ours: the service
/// wants digits, where ours is a UUID, so it is the UUID's first 64 bits
/// as a number. The same UUID always gives the same number.
fn numeric_message_id(id: &str) -> String {
    if id.bytes().all(|b| b.is_ascii_digit()) {
        return id.to_owned();
    }
    let hex: String = id
        .chars()
        .filter(char::is_ascii_hexdigit)
        .take(16)
        .collect();
    u64::from_str_radix(&hex, 16).map_or_else(|_| id.to_owned(), |n| n.to_string())
}

/// Logs why the chat service refused `what`, by its own error code and
/// message (it does not echo what was sent), and answers the failure.
async fn refused(resp: reqwest::Response, what: &str) -> Failure {
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
    Failure::Http(status.as_u16())
}

/// What `fetchShortProfile` answers.
#[derive(serde::Deserialize)]
struct ShortProfiles {
    #[serde(default)]
    value: Vec<ShortProfile>,
}

/// Whether `url` is a picture Teams serves with the sign-in: a person's
/// avatar on a middle tier, or a picture on a media service. Only these
/// are fetched with it.
pub fn is_media_url(url: &str) -> bool {
    media_host(url).is_some()
}

/// The host of a Teams media address (see [`is_media_url`]).
fn media_host(url: &str) -> Option<String> {
    let parsed = reqwest::Url::parse(url).ok()?;
    if parsed.scheme() != "https" {
        return None;
    }
    let host = parsed.host_str()?.to_ascii_lowercase();
    let media = host.ends_with(".asm.skype.com") || host.ends_with(".asyncgw.teams.microsoft.com");
    let avatar = matches!(
        host.as_str(),
        "teams.live.com" | "teams.microsoft.com" | "teams.cloud.microsoft"
    ) && parsed.path().contains("/profilepicturev2");
    (media || avatar).then_some(host)
}

/// Whether a picture's host turned its sign-in down, so another way of
/// signing in is worth a try.
fn is_refusal(status: reqwest::StatusCode) -> bool {
    matches!(status.as_u16(), 401 | 403)
}

/// The `name=value` pairs a response sets as cookies, as a `Cookie` header.
fn cookies_of(headers: &reqwest::header::HeaderMap) -> String {
    headers
        .get_all(reqwest::header::SET_COOKIE)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .filter_map(|cookie| cookie.split(';').next())
        .map(str::trim)
        .filter(|pair| pair.contains('='))
        .collect::<Vec<_>>()
        .join("; ")
}

/// The site the personal web client runs at, which some of its services
/// want named as the request's origin.
const PERSONAL_ORIGIN: &str = "https://teams.live.com";

/// Where a personal account's presence is read and said.
const PERSONAL_PRESENCE_URL: &str = "https://teams.live.com/ups/global";

/// One person's answer from `getpresence`.
#[derive(serde::Deserialize)]
struct PresenceAnswer {
    #[serde(default)]
    mri: String,
    #[serde(default)]
    presence: Option<PresenceState>,
}

/// What `getpresence` says of one person.
#[derive(serde::Deserialize)]
struct PresenceState {
    #[serde(default)]
    availability: Option<String>,
}

/// Where a personal account's chats are started.
const PERSONAL_THREADS_URL: &str = "https://teams.live.com/api/groups/v1/threads";

/// The new chat's id, from the answer's body (the groups service:
/// `{"value":{"threadId":…}}`) or else its `Location` (the chat service:
/// `…/v1/threads/{id}`).
fn created_thread(body: &str, location: Option<&str>) -> Option<String> {
    serde_json::from_str::<serde_json::Value>(body)
        .ok()
        .and_then(|v| {
            v.pointer("/value/threadId")
                .and_then(|id| id.as_str())
                .map(str::to_owned)
        })
        .or_else(|| {
            location
                .and_then(|l| l.rsplit("/threads/").next())
                .filter(|id| id.starts_with("19:"))
                .map(str::to_owned)
        })
}

/// One query's answer from `searchUsers`.
#[derive(serde::Deserialize)]
struct SearchResult {
    #[serde(default, rename = "userProfiles")]
    user_profiles: Vec<ShortProfile>,
}

/// What `/beta/users/me` answers.
#[derive(serde::Deserialize)]
struct OwnProfile {
    value: ShortProfile,
}

/// One person, as the middle tier describes them.
#[derive(serde::Deserialize)]
struct ShortProfile {
    #[serde(default, rename = "objectId")]
    object_id: Option<String>,
    #[serde(default)]
    mri: Option<String>,
    #[serde(default, rename = "displayName")]
    display_name: Option<String>,
    #[serde(default)]
    email: Option<String>,
    #[serde(default, rename = "userPrincipalName")]
    user_principal_name: Option<String>,
}

impl ShortProfile {
    /// The person under the id messages name them by (the object id, as
    /// `8:orgid:` MRIs carry it), if the profile has one.
    fn into_details(self) -> Option<UserDetails> {
        // By the MRI, as messages name people: a personal account's object
        // id is a GUID that messages never use.
        let id = self
            .mri
            .as_deref()
            .map(id_of_mri)
            .filter(|id| !id.is_empty())
            .or_else(|| self.object_id.filter(|id| !id.is_empty()))?;
        Some(UserDetails {
            id,
            display_name: self.display_name.filter(|n| !n.trim().is_empty()),
            email: self.email,
            user_principal_name: self.user_principal_name,
        })
    }
}

/// The id messages name a person by, from their MRI: the object id of
/// `8:orgid:{id}`, and `live:…` of `8:live:…`.
fn id_of_mri(mri: &str) -> String {
    ["8:orgid:", "8:teamsvisitor:", "8:guest:", "8:"]
        .iter()
        .find_map(|prefix| mri.strip_prefix(prefix))
        .unwrap_or(mri)
        .to_owned()
}

/// The MRI of a person by the id messages name them by: a work object
/// id is `8:orgid:{id}`, a personal `live:…` id is `8:live:…`, and an
/// MRI stays as it is.
pub fn user_mri(id: &str) -> String {
    // An MRI already: `8:…` for people, `28:…` for bots.
    let typed = id
        .split_once(':')
        .is_some_and(|(kind, _)| !kind.is_empty() && kind.bytes().all(|b| b.is_ascii_digit()));
    if typed {
        id.to_owned()
    } else if id.contains(':') {
        format!("8:{id}")
    } else {
        format!("8:orgid:{id}")
    }
}

/// The teams-and-channels list of the chat service aggregator (CSA).
const TEAMS_URL: &str = "https://teams.microsoft.com/api/csa/api/v1/teams/users/me?isPrefetch=false&enableMembershipSummary=true";

/// What a refresh hands to whoever keeps the credentials: the renewed
/// credentials, or why they could not be renewed.
type OnRefresh =
    Arc<dyn Fn(Result<TeamsCredentials, Failure>) -> BoxFuture<'static, ()> + Send + Sync>;

/// A page of a conversation's history, oldest first.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct HistoryPage {
    pub messages: Vec<Message>,
    /// Where the page before this one is, if there is one.
    pub older: Option<String>,
}

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
    /// The cookies that let pictures be fetched (avatars; pictures in
    /// messages where the token header is not taken), by the host that
    /// set them.
    media_cookies: Arc<RwLock<std::collections::HashMap<String, String>>>,
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
            media_cookies: Arc::new(RwLock::new(std::collections::HashMap::new())),
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
                        creds.refresh_token = Some(rotated);
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

    fn skype_token(&self) -> Result<String, Failure> {
        let creds = self.credentials();
        creds
            .skype_token
            .filter(|t| !t.is_empty())
            .ok_or(Failure::NoSavedSignIn)
    }

    /// A conversation's address on the chat service; its id holds `:` and
    /// `@`, so it is escaped, as the messages page always was.
    fn conversation_url(&self, chat_id: &str) -> String {
        format!(
            "{}/v1/users/ME/conversations/{}",
            self.chat_service_url(),
            percent_encoding::utf8_percent_encode(chat_id, percent_encoding::NON_ALPHANUMERIC)
        )
    }

    fn chat_service_url(&self) -> String {
        self.credentials()
            .chat_service_url()
            .trim_end_matches('/')
            .to_owned()
    }

    /// Fetches recent conversations (chats and channel threads).
    pub async fn get_conversations(&self, limit: usize) -> Result<Vec<Conversation>, Failure> {
        let url = format!(
            "{}/v1/users/ME/conversations?view=mychats&pageSize={}",
            self.chat_service_url(),
            limit
        );
        let resp = self
            .authed_skype_request(|http, token| {
                http.get(&url)
                    .header("Authentication", format!("skypetoken={}", token))
            })
            .await?;
        if !resp.status().is_success() {
            return Err(Failure::Http(resp.status().as_u16()));
        }
        let data: ConversationsResponse = resp
            .json()
            .await
            .map_err(|e| Failure::Unexpected(e.to_string()))?;
        Ok(data.conversations)
    }

    /// Reads a page of a conversation's messages: the newest, or the one
    /// at `older`, a link an earlier page gave.
    pub async fn get_messages(
        &self,
        chat_id: &str,
        older: Option<&str>,
        limit: usize,
    ) -> Result<HistoryPage, Failure> {
        let base = self.chat_service_url();
        let url = match older {
            Some(link) => older_link(&base, link)?.to_owned(),
            None => format!(
                "{}/v1/users/ME/conversations/{}/messages?pageSize={}",
                base,
                percent_encoding::utf8_percent_encode(chat_id, percent_encoding::NON_ALPHANUMERIC),
                limit
            ),
        };

        let resp = self
            .authed_skype_request(|http, token| {
                http.get(&url)
                    .header("Authentication", format!("skypetoken={}", token))
            })
            .await?;

        if !resp.status().is_success() {
            return Err(Failure::Http(resp.status().as_u16()));
        }

        let data: MessagesResponse = resp
            .json()
            .await
            .map_err(|e| Failure::Unexpected(e.to_string()))?;
        Ok(history_page(data))
    }

    /// The people in a chat, by MRI (`8:orgid:…`, `8:live:…`), from the
    /// thread's own record.
    pub async fn get_members(&self, chat_id: &str) -> Result<Vec<String>, Failure> {
        let url = format!(
            "{}/v1/threads/{}?view=msnp24Equivalent",
            self.chat_service_url(),
            percent_encoding::utf8_percent_encode(chat_id, percent_encoding::NON_ALPHANUMERIC)
        );
        let resp = self
            .authed_skype_request(|http, token| {
                http.get(&url)
                    .header("Authentication", format!("skypetoken={}", token))
            })
            .await?;
        if !resp.status().is_success() {
            return Err(refused(resp, "list a chat's members").await);
        }
        let thread: crate::teams::types::Thread = resp
            .json()
            .await
            .map_err(|e| Failure::Unexpected(e.to_string()))?;
        Ok(thread.members.into_iter().map(|m| m.id).collect())
    }

    /// Sends an HTML message to a conversation, answering with its id
    /// when the server gave one.
    pub async fn send_message(
        &self,
        chat_id: &str,
        html_content: &str,
        client_message_id: Option<&str>,
        me: &Author,
    ) -> Result<Option<String>, Failure> {
        let url = format!("{}/messages", self.conversation_url(chat_id));

        let mut body = message_body(chat_id, html_content, me);
        if let Some(id) = client_message_id {
            body["clientmessageid"] = numeric_message_id(id).into();
        }

        let resp = self
            .authed_skype_request(|http, token| {
                http.post(&url)
                    .header("Authentication", format!("skypetoken={}", token))
                    .json(&body)
            })
            .await?;

        if !resp.status().is_success() {
            return Err(refused(resp, "send").await);
        }

        let posted: PostedMessage = resp.json().await.unwrap_or_default();
        Ok(posted.original_arrival_time.map(|ms| ms.to_string()))
    }

    /// Replaces a message's text, as the Teams web client edits: the
    /// message again with its new content and the time of the edit.
    pub async fn edit_message(
        &self,
        chat_id: &str,
        message_id: &str,
        html_content: &str,
        me: &Author,
    ) -> Result<(), Failure> {
        let url = format!("{}/messages/{}", self.conversation_url(chat_id), message_id);
        let mut body = message_body(chat_id, html_content, me);
        body["id"] = message_id.into();
        body["properties"]["edittime"] = now_millis().into();
        let resp = self
            .authed_skype_request(|http, token| {
                http.put(&url)
                    .header("Authentication", format!("skypetoken={}", token))
                    .json(&body)
            })
            .await?;
        if resp.status().is_success() {
            Ok(())
        } else {
            Err(refused(resp, "edit").await)
        }
    }

    /// Adds (or takes back) your reaction `key` (`like`, `heart`, …).
    pub async fn react(
        &self,
        chat_id: &str,
        message_id: &str,
        key: &str,
        add: bool,
    ) -> Result<(), Failure> {
        let url = format!(
            "{}/messages/{}/properties?name=emotions",
            self.conversation_url(chat_id),
            message_id
        );
        let body = if add {
            serde_json::json!({ "emotions": { "key": key, "value": now_millis() } })
        } else {
            serde_json::json!({ "emotions": { "key": key } })
        };
        let resp = self
            .authed_skype_request(|http, token| {
                let request = if add {
                    http.put(&url)
                } else {
                    http.delete(&url)
                };
                request
                    .header("Authentication", format!("skypetoken={}", token))
                    .json(&body)
            })
            .await?;
        if resp.status().is_success() {
            Ok(())
        } else {
            Err(refused(resp, if add { "react" } else { "take a reaction back" }).await)
        }
    }

    /// Deletes a message (soft-delete).
    pub async fn delete_message(&self, chat_id: &str, message_id: &str) -> Result<(), Failure> {
        let url = format!(
            "{}/messages/{}?behavior=softDelete",
            self.conversation_url(chat_id),
            message_id
        );

        let resp = self
            .authed_skype_request(|http, token| {
                http.delete(&url)
                    .header("Authentication", format!("skypetoken={}", token))
            })
            .await?;

        if resp.status().is_success() {
            Ok(())
        } else {
            Err(refused(resp, "delete").await)
        }
    }

    /// Updates read state (consumption horizon) for a conversation.
    pub async fn set_consumption_horizon(
        &self,
        chat_id: &str,
        message_id: &str,
    ) -> Result<(), Failure> {
        let url = format!(
            "{}/properties?name=consumptionhorizon",
            self.conversation_url(chat_id)
        );

        let body = serde_json::json!({
            "consumptionhorizon": format!("{};{};{}", message_id, message_id, message_id)
        });

        let resp = self
            .authed_skype_request(|http, token| {
                http.put(&url)
                    .header("Authentication", format!("skypetoken={}", token))
                    .json(&body)
            })
            .await?;

        if resp.status().is_success() {
            Ok(())
        } else {
            Err(refused(resp, "mark read").await)
        }
    }

    /// Fetches joined teams and channels from the chat service aggregator,
    /// which wants a bearer token of its own audience.
    pub async fn get_teams(&self) -> Result<Vec<Team>, Failure> {
        // Teams free has chats only: no teams, and no list to ask.
        if self.credentials().account == Account::Personal {
            return Ok(Vec::new());
        }
        let skype = self.credentials().skype_token;
        let resp = self
            .bearer(RESOURCE_CSA, |http, token| {
                let request = http
                    .get(TEAMS_URL)
                    .bearer_auth(token)
                    .header("x-ms-client-version", "1416/1.0.0.2024050301");
                match &skype {
                    Some(skype) => request.header("X-Skypetoken", skype),
                    None => request,
                }
            })
            .await?;
        if !resp.status().is_success() {
            return Err(Failure::Http(resp.status().as_u16()));
        }
        let data: TeamsResponse = resp
            .json()
            .await
            .map_err(|e| Failure::Unexpected(e.to_string()))?;
        Ok(data.teams)
    }

    /// Looks people up by their directory (object) ids: through the
    /// middle tier, as Teams itself does, and through Microsoft Graph when
    /// that fails. People neither knows, such as guests from elsewhere,
    /// are left out.
    pub async fn get_users(&self, ids: &[String]) -> Result<Vec<UserDetails>, Failure> {
        if self.credentials().account == Account::Personal {
            return self.personal_profiles(ids).await;
        }
        // As the work web client does: `fetchShortProfile` names your own
        // organisation's people; `fetch` the rest, such as people from
        // other organisations and bots (recorded).
        let mut found = match self.short_profiles("fetchShortProfile", ids).await {
            Ok(found) => found,
            Err(error) => {
                log::info!("Teams short profiles failed ({error:?})");
                Vec::new()
            }
        };
        let missing: Vec<String> = ids
            .iter()
            .filter(|id| !found.iter().any(|f| &f.id == *id))
            .cloned()
            .collect();
        if !missing.is_empty() {
            match self.short_profiles("fetch", &missing).await {
                Ok(more) => found.extend(more),
                Err(error) => log::info!("Teams profiles failed ({error:?})"),
            }
        }
        if found.is_empty() {
            return self.graph_users(ids).await;
        }
        Ok(found)
    }

    /// People from the middle tier's `endpoint` (`fetchShortProfile` or
    /// `fetch`), with the chat service's own token. Someone it does not
    /// know is left out; asked about alone, they get a 404, which means
    /// nobody was found rather than a failure.
    async fn short_profiles(
        &self,
        endpoint: &str,
        ids: &[String],
    ) -> Result<Vec<UserDetails>, Failure> {
        let creds = self.ensure_fresh_tokens().await?;
        let base = creds
            .middle_tier_url()
            .ok_or_else(|| Failure::Unexpected("no middle tier in regionGtms".into()))?
            .trim_end_matches('/')
            .to_owned();
        let url = format!(
            "{base}/beta/users/{endpoint}?isMailAddress=false&enableGuest=true&skypeTeamsInfo=true&canBeSmtpAddress=false&includeIBBarredUsers=true&includeDisabledAccounts=true"
        );
        let mris: Vec<String> = ids.iter().map(|id| user_mri(id)).collect();
        let mut found = Vec::new();
        for batch in mris.chunks(100) {
            let resp = self
                .http
                .post(&url)
                .bearer_auth(&creds.access_token)
                .json(batch)
                .send()
                .await
                .map_err(|e| Failure::Network(e.without_url().to_string()))?;
            if resp.status() == reqwest::StatusCode::NOT_FOUND {
                continue;
            }
            if !resp.status().is_success() {
                return Err(Failure::Http(resp.status().as_u16()));
            }
            let page: ShortProfiles = resp
                .json()
                .await
                .map_err(|e| Failure::Unexpected(e.to_string()))?;
            found.extend(
                page.value
                    .into_iter()
                    .filter_map(ShortProfile::into_details),
            );
        }
        Ok(found)
    }

    /// People whose name or address matches `query`, as the New message
    /// dialog's search finds them: an address is looked up exactly, a name
    /// searched for.
    pub async fn search_people(&self, query: &str) -> Result<Vec<UserDetails>, Failure> {
        let creds = self.ensure_fresh_tokens().await?;
        let url = format!(
            "{}/beta/users/searchUsers?ggEnabled=true&resultCount=20",
            creds
                .middle_tier_url()
                .ok_or_else(|| Failure::Unexpected("no middle tier in regionGtms".into()))?
                .trim_end_matches('/')
        );
        let body = if query.contains('@') {
            serde_json::json!({ "emails": [query], "phones": [] })
        } else {
            serde_json::json!({ "emails": [], "phones": [], "searchKeyWord": query })
        };
        let skype = creds.skype_token.clone().unwrap_or_default();
        let resp = match creds.account {
            Account::Personal => {
                self.bearer(RESOURCE_MT_PERSONAL, |http, token| {
                    crate::teams::auth::consumer_headers(
                        http.post(&url)
                            .bearer_auth(token)
                            .header("x-skypetoken", &skype)
                            .json(&body),
                    )
                })
                .await?
            }
            Account::Work => self
                .http
                .post(&url)
                .bearer_auth(&creds.access_token)
                .header("x-skypetoken", &skype)
                .json(&body)
                .send()
                .await
                .map_err(|e| Failure::Network(e.without_url().to_string()))?,
        };
        if !resp.status().is_success() {
            return Err(refused(resp, "search for people").await);
        }
        let found: std::collections::HashMap<String, SearchResult> = resp
            .json()
            .await
            .map_err(|e| Failure::Unexpected(e.to_string()))?;
        Ok(found
            .into_values()
            .flat_map(|result| result.user_profiles)
            .filter_map(ShortProfile::into_details)
            .collect())
    }

    /// Starts a chat with `others` and you, answering its id: one other
    /// person makes a one-to-one chat, more a group.
    pub async fn create_chat(&self, me: &str, others: &[String]) -> Result<String, Failure> {
        let creds = self.ensure_fresh_tokens().await?;
        let members: Vec<serde_json::Value> = std::iter::once(me)
            .chain(others.iter().map(String::as_str))
            .map(|id| serde_json::json!({ "id": user_mri(id), "role": "User" }))
            .collect();
        let resp = match creds.account {
            // As the personal web client starts a chat (recorded).
            Account::Personal => {
                let skype = creds.skype_token.clone().unwrap_or_default();
                let body = serde_json::json!({
                    "members": members,
                    "properties": { "threadType": "chat", "isStickyThread": "true" },
                });
                self.bearer(RESOURCE_GROUPS_PERSONAL, |http, token| {
                    crate::teams::auth::consumer_headers(
                        http.post(PERSONAL_THREADS_URL)
                            .bearer_auth(token)
                            .header("x-skypetoken", &skype)
                            .json(&body),
                    )
                })
                .await?
            }
            // The chat service's own way, which work clients have used:
            // not yet seen in a recording of the work web client.
            Account::Work => {
                let url = format!("{}/v1/threads", self.chat_service_url());
                let body = serde_json::json!({
                    "members": members,
                    "properties": { "threadType": "chat", "fixedRoster": "true", "uniquerosterthread": "true" },
                });
                self.authed_skype_request(|http, token| {
                    http.post(&url)
                        .header("Authentication", format!("skypetoken={}", token))
                        .json(&body)
                })
                .await?
            }
        };
        if !resp.status().is_success() {
            return Err(refused(resp, "start a chat").await);
        }
        let location = resp
            .headers()
            .get(reqwest::header::LOCATION)
            .and_then(|l| l.to_str().ok())
            .map(str::to_owned);
        let text = resp.text().await.unwrap_or_default();
        created_thread(&text, location.as_deref())
            .ok_or_else(|| Failure::Unexpected("no chat id in the answer".into()))
    }

    /// A picture from Teams: a person's avatar or a picture in a message,
    /// at most `max` bytes. Only Microsoft's own media and profile hosts
    /// are asked (see [`is_media_url`]): the sign-in goes with the request,
    /// and a URL in a message must not take it anywhere else.
    pub async fn get_media(&self, url: &str, max: usize) -> Result<Vec<u8>, Failure> {
        let Some(host) = media_host(url) else {
            return Err(Failure::Unexpected("not a Teams media address".into()));
        };
        let creds = self.ensure_fresh_tokens().await?;
        let skype = creds.skype_token.clone().unwrap_or_default();
        let resp = if host.ends_with(".asyncgw.teams.microsoft.com") {
            // The work media service takes the IC3 token (recorded).
            self.bearer(RESOURCE_IC3, |http, token| http.get(url).bearer_auth(token))
                .await?
        } else if host.ends_with(".asm.skype.com") {
            // The personal media service: the skype token as a header, as
            // uploads send it; else the cookie its sign-in sets.
            let resp = self
                .http
                .get(url)
                .header("Authorization", format!("skype_token {skype}"))
                .send()
                .await
                .map_err(|e| Failure::Network(e.without_url().to_string()))?;
            if is_refusal(resp.status()) {
                self.get_with_cookie(url, &host, false).await?
            } else {
                resp
            }
        } else if creds.account == Account::Personal {
            // Avatars on the personal middle tier: by cookie only.
            let mut resp = self.get_with_cookie(url, &host, false).await?;
            if is_refusal(resp.status()) {
                resp = self.get_with_cookie(url, &host, true).await?;
            }
            resp
        } else {
            self.http
                .get(url)
                .bearer_auth(&creds.access_token)
                .header("x-skypetoken", &skype)
                .send()
                .await
                .map_err(|e| Failure::Network(e.without_url().to_string()))?
        };
        if !resp.status().is_success() {
            return Err(Failure::Http(resp.status().as_u16()));
        }
        if resp.content_length().is_some_and(|len| len > max as u64) {
            return Err(Failure::TooLarge);
        }
        let bytes = resp
            .bytes()
            .await
            .map_err(|e| Failure::Network(e.without_url().to_string()))?;
        if bytes.len() > max {
            return Err(Failure::TooLarge);
        }
        Ok(bytes.to_vec())
    }

    async fn get_with_cookie(
        &self,
        url: &str,
        host: &str,
        fresh: bool,
    ) -> Result<reqwest::Response, Failure> {
        let cookie = self.media_cookie(host, fresh).await?;
        self.http
            .get(url)
            .header(reqwest::header::COOKIE, cookie)
            .send()
            .await
            .map_err(|e| Failure::Network(e.without_url().to_string()))
    }

    /// The cookies `host` wants for pictures, asked for once and kept
    /// until refused (`fresh`): the media service's from its
    /// `skypetokenauth`, the middle tier's from `imageauth/cookie`.
    async fn media_cookie(&self, host: &str, fresh: bool) -> Result<String, Failure> {
        let held = || {
            self.media_cookies
                .read()
                .ok()
                .and_then(|held| held.get(host).cloned())
        };
        if !fresh && let Some(cookie) = held() {
            return Ok(cookie);
        }
        let _asking = self.cookie_asked.lock().await;
        // Another picture may have asked while this one waited.
        if !fresh && let Some(cookie) = held() {
            return Ok(cookie);
        }
        let creds = self.ensure_fresh_tokens().await?;
        let skype = creds.skype_token.clone().unwrap_or_default();
        let resp = if host.ends_with(".asm.skype.com") {
            self.http
                .post(format!("https://{host}/v1/skypetokenauth"))
                .header("Authorization", format!("skype_token {skype}"))
                .form(&[("skypetoken", skype.as_str())])
                .send()
                .await
                .map_err(|e| Failure::Network(e.without_url().to_string()))?
        } else {
            let base = creds
                .middle_tier_url()
                .ok_or_else(|| Failure::Unexpected("no middle tier in regionGtms".into()))?
                .trim_end_matches('/')
                .to_owned();
            let url = format!("{base}/beta/imageauth/cookie");
            // The middle tier sets its cookie only for a request that
            // says which site it comes from, as a browser's always does.
            self.bearer(RESOURCE_MT_PERSONAL, |http, token| {
                crate::teams::auth::consumer_headers(
                    http.post(&url)
                        .bearer_auth(token)
                        .header("x-skypetoken", &skype)
                        .header(reqwest::header::ORIGIN, PERSONAL_ORIGIN)
                        .header(reqwest::header::REFERER, format!("{PERSONAL_ORIGIN}/"))
                        .header(reqwest::header::CONTENT_LENGTH, "0"),
                )
            })
            .await?
        };
        if !resp.status().is_success() {
            return Err(refused(resp, "sign in for pictures").await);
        }
        let cookie = cookies_of(resp.headers());
        if cookie.is_empty() {
            return Err(Failure::Unexpected("no cookie for pictures".into()));
        }
        if let Ok(mut held) = self.media_cookies.write() {
            held.insert(host.to_owned(), cookie.clone());
        }
        Ok(cookie)
    }

    /// The presence of the people with these ids: `(id, availability)`,
    /// availability as Teams words it (`Available`, `Away`, `Busy`, …).
    pub async fn get_presence(&self, ids: &[String]) -> Result<Vec<(String, String)>, Failure> {
        let body: Vec<serde_json::Value> = ids
            .iter()
            .map(|id| serde_json::json!({ "mri": user_mri(id), "source": "ups" }))
            .collect();
        let resp = self
            .presence_request("presence/getpresence/", |request| request.json(&body))
            .await?;
        if !resp.status().is_success() {
            return Err(refused(resp, "read presence").await);
        }
        let answers: Vec<PresenceAnswer> = resp
            .json()
            .await
            .map_err(|e| Failure::Unexpected(e.to_string()))?;
        Ok(answers
            .into_iter()
            .filter_map(|answer| {
                let availability = answer.presence?.availability?;
                Some((id_of_mri(&answer.mri), availability))
            })
            .collect())
    }

    /// Says you are here, from the endpoint `endpoint` (this app's
    /// Trouter connection): without it Teams shows you offline.
    pub async fn publish_presence(&self, endpoint: &str) -> Result<(), Failure> {
        let body = serde_json::json!({
            "id": endpoint,
            "availability": "Available",
            "activity": "Available",
            "activityReporting": "Transport",
            "deviceType": "Desktop",
        });
        let resp = self
            .presence_request("me/endpoints/", |request| {
                request.header("x-ms-endpoint-id", endpoint).json(&body)
            })
            .await?;
        if resp.status().is_success() {
            Ok(())
        } else {
            Err(refused(resp, "say you are here").await)
        }
    }

    /// A request to the presence service, signed as each kind of account
    /// signs it: personal through `teams.live.com/ups/global` with the
    /// middle tier's token (recorded), work through the region's
    /// `unifiedPresence` with the presence service's own.
    async fn presence_request(
        &self,
        path: &str,
        build: impl Fn(reqwest::RequestBuilder) -> reqwest::RequestBuilder,
    ) -> Result<reqwest::Response, Failure> {
        let creds = self.ensure_fresh_tokens().await?;
        let skype = creds.skype_token.clone().unwrap_or_default();
        let put = path.starts_with("me/");
        match creds.account {
            Account::Personal => {
                let url = format!("{PERSONAL_PRESENCE_URL}/v1/{path}");
                self.bearer(RESOURCE_MT_PERSONAL, |http, token| {
                    let request = if put { http.put(&url) } else { http.post(&url) };
                    build(crate::teams::auth::consumer_headers(
                        request.bearer_auth(token).header("x-skypetoken", &skype),
                    ))
                })
                .await
            }
            Account::Work => {
                let base = creds
                    .region_gtms
                    .as_ref()
                    .and_then(|g| g.get("unifiedPresence"))
                    .and_then(|u| u.as_str())
                    .ok_or_else(|| Failure::Unexpected("no presence service in regionGtms".into()))?
                    .trim_end_matches('/')
                    .to_owned();
                let url = format!("{base}/v1/{path}");
                self.bearer(RESOURCE_PRESENCE, |http, token| {
                    let request = if put { http.put(&url) } else { http.post(&url) };
                    build(request.bearer_auth(token).header("x-skypetoken", &skype))
                })
                .await
            }
        }
    }

    /// Your own profile on a personal account's middle tier (its token
    /// carries no name), named by the id messages name you by.
    pub async fn own_profile(&self) -> Result<UserDetails, Failure> {
        let creds = self.ensure_fresh_tokens().await?;
        let url = format!(
            "{}/beta/users/me/?skypeTeamsInfo=true&ggEnabled=true",
            creds
                .middle_tier_url()
                .ok_or_else(|| Failure::Unexpected("no middle tier in regionGtms".into()))?
                .trim_end_matches('/')
        );
        let skype = creds.skype_token.unwrap_or_default();
        let resp = self
            .bearer(RESOURCE_MT_PERSONAL, |http, token| {
                crate::teams::auth::consumer_headers(
                    http.get(&url)
                        .bearer_auth(token)
                        .header("x-skypetoken", &skype),
                )
            })
            .await?;
        if !resp.status().is_success() {
            return Err(refused(resp, "read your profile").await);
        }
        let me: OwnProfile = resp
            .json()
            .await
            .map_err(|e| Failure::Unexpected(e.to_string()))?;
        me.value
            .into_details()
            .ok_or_else(|| Failure::Unexpected("no id in your profile".into()))
    }

    /// People as a personal account's middle tier knows them, the way the
    /// Teams web client asks: personal accounts through `fetchShortProfile`,
    /// work accounts met in personal chats through `fetchFederated`, each
    /// with a token for the middle tier's own audience and the skype token.
    async fn personal_profiles(&self, ids: &[String]) -> Result<Vec<UserDetails>, Failure> {
        let creds = self.ensure_fresh_tokens().await?;
        let base = creds
            .middle_tier_url()
            .ok_or_else(|| Failure::Unexpected("no middle tier in regionGtms".into()))?
            .trim_end_matches('/')
            .to_owned();
        let skype = creds.skype_token.unwrap_or_default();
        let (work, personal): (Vec<String>, Vec<String>) = ids
            .iter()
            .map(|id| user_mri(id))
            .partition(|mri| mri.starts_with("8:orgid:"));
        let mut found = Vec::new();
        for (path, mris) in [
            (
                "fetchShortProfile?isMailAddress=false&enableGuest=true&skypeTeamsInfo=true&canBeSmtpAddress=false&includeIBBarredUsers=false&includeDisabledAccounts=false&ggEnabled=true",
                personal,
            ),
            (
                "fetchFederated?edEnabled=false&includeDisabledAccounts=true",
                work,
            ),
        ] {
            let url = format!("{base}/beta/users/{path}");
            for batch in mris.chunks(100) {
                let resp = self
                    .bearer(RESOURCE_MT_PERSONAL, |http, token| {
                        crate::teams::auth::consumer_headers(
                            http.post(&url)
                                .bearer_auth(token)
                                .header("x-skypetoken", &skype)
                                .json(batch),
                        )
                    })
                    .await?;
                if !resp.status().is_success() {
                    return Err(refused(resp, "look people up").await);
                }
                let page: ShortProfiles = resp
                    .json()
                    .await
                    .map_err(|e| Failure::Unexpected(e.to_string()))?;
                found.extend(
                    page.value
                        .into_iter()
                        .filter_map(ShortProfile::into_details),
                );
            }
        }
        Ok(found)
    }

    /// Graph's `/users/{id}`, one person at a time: it needs only the basic
    /// profile permission, where looking many up at once needs directory
    /// access the Teams sign-in may not have.
    async fn graph_users(&self, ids: &[String]) -> Result<Vec<UserDetails>, Failure> {
        let mut found = Vec::new();
        let mut refused = None;
        for id in ids {
            let url = format!(
                "https://graph.microsoft.com/v1.0/users/{}?$select=id,displayName,userPrincipalName,mail",
                percent_encoding::utf8_percent_encode(id, percent_encoding::NON_ALPHANUMERIC)
            );
            let resp = self
                .bearer(RESOURCE_GRAPH, |http, token| {
                    http.get(&url).bearer_auth(token)
                })
                .await?;
            match resp.status() {
                status if status.is_success() => {
                    if let Ok(user) = resp.json::<UserDetails>().await {
                        found.push(user);
                    }
                }
                // Someone Graph does not know here: the others may still be.
                reqwest::StatusCode::NOT_FOUND => {}
                status => refused = Some(Failure::Http(status.as_u16())),
            }
        }
        match refused {
            Some(error) if found.is_empty() => Err(error),
            _ => Ok(found),
        }
    }

    /// Extracts user profile information from the JWT token claims if available.
    pub fn user_from_token(&self) -> Option<UserDetails> {
        let creds = self.credentials();
        let Some(claims) = crate::teams::auth::parse_jwt_claims(&creds.access_token) else {
            // A personal account's access token is opaque; its skype token
            // says who you are (`skypeid`: `live:.cid.…`), if not your name.
            let claims = crate::teams::auth::parse_jwt_claims(creds.skype_token.as_deref()?)?;
            let id = claims.get("skypeid").and_then(|v| v.as_str())?;
            return Some(UserDetails {
                id: id.to_owned(),
                ..UserDetails::default()
            });
        };
        let id = claims
            .get("oid")
            .or_else(|| claims.get("sub"))
            .and_then(|v| v.as_str())?
            .to_string();
        let display_name = claims
            .get("name")
            .and_then(|v| v.as_str())
            .map(String::from);
        let email = claims
            .get("email")
            .or_else(|| claims.get("preferred_username"))
            .or_else(|| claims.get("upn"))
            .or_else(|| claims.get("unique_name"))
            .and_then(|v| v.as_str())
            .map(String::from);
        let user_principal_name = claims
            .get("upn")
            .or_else(|| claims.get("unique_name"))
            .and_then(|v| v.as_str())
            .map(String::from);
        Some(UserDetails {
            id,
            display_name,
            email,
            user_principal_name,
        })
    }

    /// The current user, from the access token's claims: the Teams
    /// tokens are not good for Microsoft Graph's `/me`.
    pub fn get_me(&self) -> Result<UserDetails, Failure> {
        self.user_from_token()
            .ok_or_else(|| Failure::Unexpected("no user in the Teams token".into()))
    }
}

/// `link` if it is a page of the chat service at `base`: the skype token
/// goes with it, so a link elsewhere is refused rather than followed.
fn older_link<'a>(base: &str, link: &'a str) -> Result<&'a str, Failure> {
    let inside = link
        .strip_prefix(base)
        .is_some_and(|rest| rest.starts_with('/'));
    if inside {
        Ok(link)
    } else {
        Err(Failure::Unexpected(
            "older messages link is not on the chat service".into(),
        ))
    }
}

/// A messages answer as a page: control messages and deleted ones left
/// out, oldest first (Teams gives newest first), and the older page's link
/// only when there is something before this one.
fn history_page(data: MessagesResponse) -> HistoryPage {
    let older = data
        .metadata
        .and_then(|meta| meta.backward_link)
        .filter(|link| !link.is_empty() && !data.messages.is_empty());
    let mut messages: Vec<Message> = data
        .messages
        .into_iter()
        .filter(|m| {
            !m.message_type
                .as_deref()
                .is_some_and(|t| t.starts_with("Control/"))
                && !m.properties.as_ref().is_some_and(|p| p.is_deleted())
        })
        .collect();
    messages.reverse();
    HistoryPage { messages, older }
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

    #[test]
    fn older_pages_must_be_on_the_chat_service() {
        let base = "https://emea.ng.msg.teams.microsoft.com";
        let good = "https://emea.ng.msg.teams.microsoft.com/v1/users/ME/conversations/x/messages?syncState=a";
        assert_eq!(older_link(base, good), Ok(good));
        for bad in [
            "https://evil.example/v1/messages",
            "https://emea.ng.msg.teams.microsoft.com.evil.example/v1",
            "",
        ] {
            assert!(older_link(base, bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn a_history_page_is_oldest_first_with_its_older_link() {
        let json = r#"{
            "messages": [
                {"id": "3", "messagetype": "RichText/Html", "content": "c"},
                {"id": "2", "messagetype": "Control/Typing", "content": ""},
                {"id": "1", "messagetype": "Text", "content": "a"}
            ],
            "_metadata": {"backwardLink": "https://chat/v1/older"}
        }"#;
        let page = history_page(serde_json::from_str(json).expect("valid page"));
        let ids: Vec<&str> = page.messages.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(ids, ["1", "3"]);
        assert_eq!(page.older.as_deref(), Some("https://chat/v1/older"));

        let empty = r#"{"messages": [], "_metadata": {"backwardLink": "https://chat/v1/older"}}"#;
        let page = history_page(serde_json::from_str(empty).expect("valid page"));
        assert_eq!(page.older, None);
    }

    #[test]
    fn short_profiles_are_named_by_object_id() {
        let page: ShortProfiles = serde_json::from_str(
            r#"{"type":"Microsoft.SkypeSpaces.MiddleTier.Models.IUserIdentity","value":[
                {"objectId":"a-1","mri":"8:orgid:a-1","displayName":"Alice","email":"a@x.org"},
                {"mri":"8:orgid:b-2","displayName":" "},
                {"displayName":"Nobody"}
            ]}"#,
        )
        .expect("a page");
        let people: Vec<UserDetails> = page
            .value
            .into_iter()
            .filter_map(ShortProfile::into_details)
            .collect();
        assert_eq!(people.len(), 2);
        assert_eq!(people[0].id, "a-1");
        assert_eq!(people[0].display_name.as_deref(), Some("Alice"));
        assert_eq!(people[1].id, "b-2");
        assert_eq!(people[1].display_name, None);
        assert_eq!(user_mri("a-1"), "8:orgid:a-1");
        assert_eq!(user_mri("8:live:x"), "8:live:x");
        assert_eq!(user_mri("live:.cid.x"), "8:live:.cid.x");
        assert_eq!(user_mri("28:3914e2ec-62b6"), "28:3914e2ec-62b6");
        assert_eq!(id_of_mri("28:3914e2ec-62b6"), "28:3914e2ec-62b6");
        assert_eq!(id_of_mri("8:live:.cid.x"), "live:.cid.x");
        assert_eq!(id_of_mri("8:orgid:a-1"), "a-1");

        // A personal account's profile: named by its MRI, not its GUID.
        let personal: ShortProfiles = serde_json::from_str(
            r#"{"value":[{"objectId":"00000000-0000-0000-3448-d9d5387f3b43","mri":"8:live:.cid.3448","displayName":"Yan"}]}"#,
        )
        .expect("a page");
        let details: Vec<UserDetails> = personal
            .value
            .into_iter()
            .filter_map(ShortProfile::into_details)
            .collect();
        assert_eq!(details[0].id, "live:.cid.3448");
    }

    #[test]
    fn a_personal_account_is_known_by_its_skype_id() {
        use base64::Engine as _;
        let jwt = |claims: &str| {
            let body = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(claims);
            format!("eyJhbGciOiJub25lIn0.{body}.sig")
        };
        let client = TeamsClient::new(TeamsCredentials {
            access_token: "EwA-opaque".into(),
            skype_token: Some(jwt(r#"{"skypeid":"live:.cid.4a5b"}"#)),
            account: Account::Personal,
            ..TeamsCredentials::default()
        });
        let me = client.get_me().expect("who you are");
        assert_eq!(me.id, "live:.cid.4a5b");
        assert_eq!(me.display_name, None);

        let work = TeamsClient::new(TeamsCredentials {
            access_token: jwt(r#"{"oid":"o-1","name":"Ann"}"#),
            ..TeamsCredentials::default()
        });
        let me = work.get_me().expect("who you are");
        assert_eq!(
            (me.id.as_str(), me.display_name.as_deref()),
            ("o-1", Some("Ann"))
        );
    }

    #[tokio::test]
    async fn a_personal_account_has_no_teams_to_list() {
        let client = TeamsClient::new(TeamsCredentials {
            account: Account::Personal,
            ..TeamsCredentials::default()
        });
        assert_eq!(client.get_teams().await, Ok(Vec::new()));
    }

    #[test]
    fn the_sign_in_goes_only_to_teams_media_hosts() {
        for url in [
            "https://eu-api.asm.skype.com/v1/objects/0-weu-d1-abc/views/imgo",
            "https://fr-prod.asyncgw.teams.microsoft.com/v1/objects/0-x/views/imgo",
            "https://teams.live.com/api/mt/beta/users/8:live:x/profilepicturev2?size=HR64x64",
            "https://teams.cloud.microsoft/api/mt/emea/beta/users/8:orgid:a/profilepicturev2/x",
        ] {
            assert!(is_media_url(url), "{url}");
        }
        for url in [
            "https://evil.example/v1/objects/x/views/imgo",
            "https://asm.skype.com.evil.example/x",
            "http://eu-api.asm.skype.com/v1/objects/x",
            "https://teams.live.com/api/chatsvc/consumer/v1/users/ME/conversations",
            "https://statics.teams.cdn.office.net/emoji.png",
        ] {
            assert!(!is_media_url(url), "{url}");
        }
    }

    #[test]
    fn presence_answers_are_named_by_id() {
        let answers: Vec<PresenceAnswer> = serde_json::from_str(
            r#"[{"mri":"8:live:.cid.3448","source":"ups","presence":{"sourceNetwork":"Self","availability":"Available","activity":"Available","deviceType":"Web"},"status":20000},
                {"mri":"8:orgid:a-1","presence":{"availability":"Away"}},
                {"mri":"8:orgid:b-2","status":40400}]"#,
        )
        .expect("answers");
        let found: Vec<(String, String)> = answers
            .into_iter()
            .filter_map(|a| Some((id_of_mri(&a.mri), a.presence?.availability?)))
            .collect();
        assert_eq!(
            found,
            [
                ("live:.cid.3448".to_owned(), "Available".to_owned()),
                ("a-1".to_owned(), "Away".to_owned())
            ]
        );
    }

    #[test]
    fn set_cookies_become_one_cookie_header() {
        let mut headers = reqwest::header::HeaderMap::new();
        for value in [
            "skypetoken_asm=abc; Path=/; Secure; HttpOnly",
            "platformid_asm=1; Path=/",
        ] {
            headers.append(
                reqwest::header::SET_COOKIE,
                reqwest::header::HeaderValue::from_static(value),
            );
        }
        assert_eq!(cookies_of(&headers), "skypetoken_asm=abc; platformid_asm=1");
    }

    #[test]
    fn a_new_chat_is_found_in_either_answer() {
        let groups =
            r#"{"value":{"threadId":"19:uni01_abc@thread.v2","membersStatus":[]},"Type":"x"}"#;
        assert_eq!(
            created_thread(groups, None).as_deref(),
            Some("19:uni01_abc@thread.v2")
        );
        let located = created_thread(
            "",
            Some("https://x.msg.teams.microsoft.com/v1/threads/19:abc@thread.v2"),
        );
        assert_eq!(located.as_deref(), Some("19:abc@thread.v2"));
        assert_eq!(created_thread("{}", None), None);
    }

    #[test]
    fn search_results_are_named_by_mri() {
        let found: std::collections::HashMap<String, SearchResult> = serde_json::from_str(
            r#"{"pim":{"userProfiles":[{"mri":"8:live:.cid.8a2c","displayName":"pim pim","objectId":"00000000-0000-0000-8a2c-09c8303fc296"}]}}"#,
        )
        .expect("an answer");
        let people: Vec<UserDetails> = found
            .into_values()
            .flat_map(|r| r.user_profiles)
            .filter_map(ShortProfile::into_details)
            .collect();
        assert_eq!(people[0].id, "live:.cid.8a2c");
        assert_eq!(people[0].display_name.as_deref(), Some("pim pim"));
    }

    #[test]
    fn your_profile_reads_as_you() {
        let me: OwnProfile = serde_json::from_str(
            r#"{"value":{"isShortProfile":false,"objectId":"00000000-0000-0000-3448-d9d5387f3b43","mri":"8:live:.cid.3448d9d5387f3b43","displayName":"Yannick de Jong","email":"y@x.org"},"type":"Microsoft.SkypeSpaces.MiddleTier.Models.IUserIdentity"}"#,
        )
        .expect("a profile");
        let me = me.value.into_details().expect("an id");
        assert_eq!(me.id, "live:.cid.3448d9d5387f3b43");
        assert_eq!(me.display_name.as_deref(), Some("Yannick de Jong"));
    }

    #[test]
    fn messages_go_out_as_the_web_client_sends_them() {
        let me = Author {
            id: "live:.cid.4a5b".into(),
            name: Some("Yan".into()),
        };
        let body = message_body("19:x@thread.v2", "<p>hi</p>", &me);
        assert_eq!(body["from"], "8:live:.cid.4a5b");
        assert_eq!(body["imdisplayname"], "Yan");
        assert_eq!(body["messagetype"], "RichText/Html");
        assert_eq!(body["properties"]["formatVariant"], "TEAMS");
        let nameless = message_body("19:x@thread.v2", "<p>hi</p>", &Author::default());
        assert_eq!(nameless["imdisplayname"], "");
    }

    #[test]
    fn client_message_ids_are_numbers() {
        let id = numeric_message_id("8d6f1c2a-0b3e-4f5a-9c7d-1e2f3a4b5c6d");
        assert!(id.bytes().all(|b| b.is_ascii_digit()), "{id}");
        assert_eq!(
            id,
            numeric_message_id("8d6f1c2a-0b3e-4f5a-9c7d-1e2f3a4b5c6d")
        );
        assert_eq!(
            id,
            u64::from_str_radix("8d6f1c2a0b3e4f5a", 16)
                .expect("hex")
                .to_string()
        );
        assert_eq!(numeric_message_id("12345"), "12345");
    }

    #[test]
    fn a_posted_message_gives_its_id() {
        let posted: PostedMessage =
            serde_json::from_str(r#"{"OriginalArrivalTime": 1700000000123}"#).expect("valid");
        assert_eq!(posted.original_arrival_time, Some(1700000000123));
    }
}
