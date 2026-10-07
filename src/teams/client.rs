//! HTTP client for Microsoft Teams native web APIs.
//!
//! Handles calling ChatSvc, CSA, and Middle-Tier endpoints with the appropriate
//! SkypeToken and Bearer token headers. Never logs or exposes tokens in errors.

use std::sync::{Arc, RwLock};

use crate::failure::Failure;
use crate::teams::auth::TeamsCredentials;
use crate::teams::types::{
    Conversation, ConversationsResponse, Message, MessagesResponse, Team, TeamsResponse,
    UserDetails,
};

type TokenCallback = Arc<dyn Fn(TeamsCredentials) + Send + Sync>;

/// Authenticated client for Microsoft Teams APIs.
#[derive(Clone)]
pub struct TeamsClient {
    http: reqwest::Client,
    credentials: Arc<RwLock<TeamsCredentials>>,
    on_token_refreshed: Arc<RwLock<Option<TokenCallback>>>,
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
            on_token_refreshed: Arc::new(RwLock::new(None)),
        }
    }

    /// Sets a callback invoked whenever credentials are automatically refreshed.
    pub fn set_on_token_refreshed<F>(&self, callback: F)
    where
        F: Fn(TeamsCredentials) + Send + Sync + 'static,
    {
        if let Ok(mut lock) = self.on_token_refreshed.write() {
            *lock = Some(Arc::new(callback));
        }
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

    /// Checks if access token or SkypeToken is expired/missing and refreshes if needed.
    pub async fn ensure_fresh_tokens(&self) -> Result<TeamsCredentials, Failure> {
        let creds = self.credentials();
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();

        if !creds.is_expired(now) && creds.skype_token.is_some() {
            return Ok(creds);
        }

        self.force_refresh().await
    }

    /// Forces a refresh of credentials and invokes the save callback.
    pub async fn force_refresh(&self) -> Result<TeamsCredentials, Failure> {
        let current = self.credentials();
        let refreshed = crate::teams::auth::refresh_credentials(&self.http, &current).await?;
        self.update_credentials(refreshed.clone());

        let callback = self
            .on_token_refreshed
            .read()
            .ok()
            .and_then(|guard| guard.clone());
        if let Some(cb) = callback {
            cb(refreshed.clone());
        }

        Ok(refreshed)
    }

    /// Executes an HTTP request with automatic token refresh on HTTP 401.
    async fn authed_skype_request<F>(&self, make_request: F) -> Result<reqwest::Response, Failure>
    where
        F: Fn(&reqwest::Client, &str) -> reqwest::RequestBuilder,
    {
        let mut token = self.skype_token()?;
        let resp = make_request(&self.http, &token)
            .send()
            .await
            .map_err(|e| Failure::Network(e.to_string()))?;

        if resp.status() == reqwest::StatusCode::UNAUTHORIZED {
            log::info!("Teams API returned 401 Unauthorized, refreshing token...");
            if let Ok(new_creds) = self.force_refresh().await
                && let Some(new_token) = new_creds.skype_token
            {
                token = new_token;
                return make_request(&self.http, &token)
                    .send()
                    .await
                    .map_err(|e| Failure::Network(e.to_string()));
            }
        }
        Ok(resp)
    }

    /// Executes a Bearer-authed request with automatic token refresh on HTTP 401.
    async fn authed_bearer_request<F>(&self, make_request: F) -> Result<reqwest::Response, Failure>
    where
        F: Fn(&reqwest::Client, &str) -> reqwest::RequestBuilder,
    {
        let mut token = self.access_token()?;
        let resp = make_request(&self.http, &token)
            .send()
            .await
            .map_err(|e| Failure::Network(e.to_string()))?;

        if resp.status() == reqwest::StatusCode::UNAUTHORIZED {
            log::info!("Teams Bearer API returned 401 Unauthorized, refreshing token...");
            if let Ok(new_creds) = self.force_refresh().await {
                token = new_creds.access_token;
                return make_request(&self.http, &token)
                    .send()
                    .await
                    .map_err(|e| Failure::Network(e.to_string()));
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

    fn access_token(&self) -> Result<String, Failure> {
        let creds = self.credentials();
        if creds.access_token.is_empty() {
            Err(Failure::NoSavedSignIn)
        } else {
            Ok(creds.access_token)
        }
    }

    fn chat_service_url(&self) -> String {
        self.credentials().chat_service_url().to_owned()
    }

    fn chatsvcagg_url(&self) -> String {
        self.credentials().chatsvcagg_url().to_owned()
    }

    /// Fetches recent conversations (chats and channel threads).
    pub async fn get_conversations(&self, limit: usize) -> Result<Vec<Conversation>, Failure> {
        // Strategy 1: chatsvcagg
        let base = self.chatsvcagg_url();
        let url = format!(
            "{}/api/v2/users/ME/conversations?view=mychats&pageSize={}",
            base, limit
        );

        let resp = self
            .authed_skype_request(|http, token| {
                http.get(&url)
                    .header("Authentication", format!("skypetoken={}", token))
            })
            .await;

        if let Ok(r) = resp
            && r.status().is_success()
            && let Ok(data) = r.json::<ConversationsResponse>().await
        {
            return Ok(data.conversations);
        }

        // Strategy 2: regional chat service
        let base = self.chat_service_url();
        let fallback_url = format!(
            "{}/v1/users/ME/conversations?view=mychats&pageSize={}",
            base, limit
        );

        let resp = self
            .authed_skype_request(|http, token| {
                http.get(&fallback_url)
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

    /// Reads messages from a conversation.
    pub async fn get_messages(&self, chat_id: &str, limit: usize) -> Result<Vec<Message>, Failure> {
        let base = self.chat_service_url();
        let url = format!(
            "{}/v1/users/ME/conversations/{}/messages?pageSize={}",
            base, chat_id, limit
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

        let data: MessagesResponse = resp
            .json()
            .await
            .map_err(|e| Failure::Unexpected(e.to_string()))?;
        let messages = data
            .messages
            .into_iter()
            .filter(|m| {
                !m.message_type
                    .as_deref()
                    .is_some_and(|t| t.starts_with("Control/"))
                    && !m.properties.as_ref().is_some_and(|p| p.is_deleted())
            })
            .collect();
        Ok(messages)
    }

    /// Sends an HTML message to a conversation.
    pub async fn send_message(&self, chat_id: &str, html_content: &str) -> Result<String, Failure> {
        let base = self.chat_service_url();
        let url = format!("{}/v1/users/ME/conversations/{}/messages", base, chat_id);

        let body = serde_json::json!({
            "content": html_content,
            "messagetype": "RichText/Html",
            "contenttype": "text"
        });

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

        let text = resp
            .text()
            .await
            .map_err(|e| Failure::Unexpected(e.to_string()))?;
        Ok(text)
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

    /// Fetches joined teams and channels via CSA endpoint.
    pub async fn get_teams(&self) -> Result<Vec<Team>, Failure> {
        let url = "https://teams.microsoft.com/api/csa/api/v2/teams/users/me";

        let resp = self
            .authed_skype_request(|http, token| {
                http.get(url)
                    .bearer_auth(token)
                    .header("x-ms-client-version", "1416/1.0.0.2024050301")
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

    /// Fetches the current user profile, preferring the token claims over Graph API.
    pub async fn get_me(&self) -> Result<UserDetails, Failure> {
        if let Some(user) = self.user_from_token() {
            return Ok(user);
        }

        let url = "https://graph.microsoft.com/v1.0/me";

        let resp = self
            .authed_bearer_request(|http, token| http.get(url).bearer_auth(token))
            .await?;

        if !resp.status().is_success() {
            return Err(Failure::Http(resp.status().as_u16()));
        }

        let user: UserDetails = resp
            .json()
            .await
            .map_err(|e| Failure::Unexpected(e.to_string()))?;
        Ok(user)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicBool, Ordering};

    use super::*;

    #[test]
    fn updates_credentials_and_triggers_callback() {
        let creds = TeamsCredentials {
            access_token: "init-token".into(),
            refresh_token: Some("rt".into()),
            skype_token: Some("init-skype".into()),
            expires_at: Some(999999999),
            tenant_id: Some("test-tenant".into()),
            region_gtms: None,
        };
        let client = TeamsClient::new(creds);
        assert_eq!(client.credentials().access_token, "init-token");

        let called = Arc::new(AtomicBool::new(false));
        let called_clone = called.clone();
        client.set_on_token_refreshed(move |new_creds| {
            if new_creds.access_token == "new-token" {
                called_clone.store(true, Ordering::SeqCst);
            }
        });

        let mut updated = client.credentials();
        updated.access_token = "new-token".into();
        client.update_credentials(updated.clone());
        assert_eq!(client.credentials().access_token, "new-token");

        // Manually test callback dispatch
        if let Ok(guard) = client.on_token_refreshed.read()
            && let Some(ref cb) = *guard
        {
            cb(updated);
        }
        assert!(called.load(Ordering::SeqCst));
    }
}
