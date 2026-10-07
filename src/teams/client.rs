//! HTTP client for Microsoft Teams native web APIs.
//!
//! Handles calling ChatSvc, CSA, and Middle-Tier endpoints with the appropriate
//! SkypeToken and Bearer token headers. Never logs or exposes tokens in errors.

use std::sync::{Arc, RwLock};

use futures_util::future::BoxFuture;

use crate::failure::Failure;
use crate::teams::auth::{RESOURCE_CSA, TeamsCredentials, now_secs};
use crate::teams::types::{
    Conversation, ConversationsResponse, Message, MessagesResponse, PostedMessage, Team,
    TeamsResponse, UserDetails,
};

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

    /// A bearer token for the chat service aggregator, minted from the
    /// refresh token when there is none or `refused` was turned down.
    async fn csa_token(&self, refused: Option<&str>) -> Result<String, Failure> {
        let usable = |creds: &TeamsCredentials| {
            creds
                .fresh_csa_token(now_secs())
                .filter(|token| Some(*token) != refused)
                .map(str::to_owned)
        };
        if let Some(token) = usable(&self.credentials()) {
            return Ok(token);
        }
        let creds = self
            .refresh_if(
                |creds| usable(creds).is_none(),
                |http, creds| async move {
                    let minted = crate::teams::auth::redeem(&http, &creds, RESOURCE_CSA).await?;
                    Ok(TeamsCredentials {
                        csa_token: Some(minted.access_token),
                        csa_expires_at: minted.expires_in.map(|s| now_secs() + s),
                        refresh_token: minted.refresh_token.or(creds.refresh_token.clone()),
                        ..creds
                    })
                },
            )
            .await?;
        creds.csa_token.ok_or(Failure::SignedOut)
    }

    /// Executes an HTTP request with automatic token refresh on HTTP 401.
    async fn authed_skype_request<F>(&self, make_request: F) -> Result<reqwest::Response, Failure>
    where
        F: Fn(&reqwest::Client, &str) -> reqwest::RequestBuilder,
    {
        let token = self.skype_token()?;
        let resp = make_request(&self.http, &token)
            .send()
            .await
            .map_err(|e| Failure::Network(e.without_url().to_string()))?;

        if resp.status() == reqwest::StatusCode::UNAUTHORIZED {
            log::info!("Teams API returned 401 Unauthorized, refreshing token...");
            if let Ok(new_creds) = self.force_refresh(&token).await
                && let Some(new_token) = new_creds.skype_token
            {
                return make_request(&self.http, &new_token)
                    .send()
                    .await
                    .map_err(|e| Failure::Network(e.without_url().to_string()));
            }
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

    /// Sends an HTML message to a conversation, answering with its id
    /// when the server gave one.
    pub async fn send_message(
        &self,
        chat_id: &str,
        html_content: &str,
        client_message_id: Option<&str>,
    ) -> Result<Option<String>, Failure> {
        let base = self.chat_service_url();
        let url = format!("{}/v1/users/ME/conversations/{}/messages", base, chat_id);

        let mut body = serde_json::json!({
            "content": html_content,
            "messagetype": "RichText/Html",
            "contenttype": "text"
        });
        if let Some(id) = client_message_id {
            body["clientmessageid"] = id.into();
        }

        let resp = self
            .authed_skype_request(|http, token| {
                http.post(&url)
                    .header("Authentication", format!("skypetoken={}", token))
                    .json(&body)
            })
            .await?;

        if !resp.status().is_success() {
            return Err(Failure::Http(resp.status().as_u16()));
        }

        let posted: PostedMessage = resp.json().await.unwrap_or_default();
        Ok(posted.original_arrival_time.map(|ms| ms.to_string()))
    }

    /// Deletes a message (soft-delete).
    pub async fn delete_message(&self, chat_id: &str, message_id: &str) -> Result<(), Failure> {
        let base = self.chat_service_url();
        let url = format!(
            "{}/v1/users/ME/conversations/{}/messages/{}?behavior=softDelete",
            base, chat_id, message_id
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
            Err(Failure::Http(resp.status().as_u16()))
        }
    }

    /// Updates read state (consumption horizon) for a conversation.
    pub async fn set_consumption_horizon(
        &self,
        chat_id: &str,
        message_id: &str,
    ) -> Result<(), Failure> {
        let base = self.chat_service_url();
        let url = format!(
            "{}/v1/users/ME/conversations/{}/properties?name=consumptionhorizon",
            base, chat_id
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
            Err(Failure::Http(resp.status().as_u16()))
        }
    }

    /// Fetches joined teams and channels from the chat service aggregator,
    /// which wants a bearer token of its own audience.
    pub async fn get_teams(&self) -> Result<Vec<Team>, Failure> {
        let send = |token: String| {
            let mut request = self
                .http
                .get(TEAMS_URL)
                .bearer_auth(token)
                .header("x-ms-client-version", "1416/1.0.0.2024050301");
            if let Some(skype) = self.credentials().skype_token {
                request = request.header("X-Skypetoken", skype);
            }
            request.send()
        };
        let token = self.csa_token(None).await?;
        let mut resp = send(token.clone())
            .await
            .map_err(|e| Failure::Network(e.without_url().to_string()))?;
        if resp.status() == reqwest::StatusCode::UNAUTHORIZED {
            log::info!("teams list refused the CSA token, minting another");
            resp = send(self.csa_token(Some(&token)).await?)
                .await
                .map_err(|e| Failure::Network(e.without_url().to_string()))?;
        }

        if !resp.status().is_success() {
            return Err(Failure::Http(resp.status().as_u16()));
        }

        let data: TeamsResponse = resp
            .json()
            .await
            .map_err(|e| Failure::Unexpected(e.to_string()))?;
        Ok(data.teams)
    }

    /// Extracts user profile information from the JWT token claims if available.
    pub fn user_from_token(&self) -> Option<UserDetails> {
        let creds = self.credentials();
        let claims = crate::teams::auth::parse_jwt_claims(&creds.access_token)?;
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
    fn a_posted_message_gives_its_id() {
        let posted: PostedMessage =
            serde_json::from_str(r#"{"OriginalArrivalTime": 1700000000123}"#).expect("valid");
        assert_eq!(posted.original_arrival_time, Some(1700000000123));
    }
}
