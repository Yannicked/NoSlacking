//! HTTP client for Microsoft Teams native web APIs.
//!
//! Handles calling ChatSvc, CSA, and Middle-Tier endpoints with the appropriate
//! SkypeToken and Bearer token headers. Never logs or exposes tokens in errors.

use std::sync::{Arc, RwLock};

use futures_util::future::BoxFuture;

use crate::failure::Failure;
use crate::teams::auth::{
    Account, AudienceToken, RESOURCE_CSA, RESOURCE_GRAPH, RESOURCE_MT_PERSONAL, TeamsCredentials,
    now_secs,
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
    if id.starts_with("8:") {
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
        match self.short_profiles(ids).await {
            Ok(found) => Ok(found),
            Err(error) => {
                log::info!("Teams people lookup failed ({error:?}), trying Graph");
                self.graph_users(ids).await
            }
        }
    }

    /// `fetchShortProfile` of the middle tier, with the chat service's own
    /// token: what the Teams client names people with.
    async fn short_profiles(&self, ids: &[String]) -> Result<Vec<UserDetails>, Failure> {
        let creds = self.ensure_fresh_tokens().await?;
        let base = creds
            .middle_tier_url()
            .ok_or_else(|| Failure::Unexpected("no middle tier in regionGtms".into()))?
            .trim_end_matches('/')
            .to_owned();
        let url = format!(
            "{base}/beta/users/fetchShortProfile?isMailAddress=false&enableGuest=true&includeIBBarredUsers=true&skypeTeamsInfo=true"
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
                "fetchShortProfile?isMailAddress=false&canBeSmtpAddress=false&enableGuest=true&includeIBBarredUsers=true&skypeTeamsInfo=true&includeDisabledAccounts=true",
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
