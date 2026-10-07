//! Microsoft Teams authentication handling.
//!
//! Uses Microsoft's first-party Teams client credentials to authenticate via
//! Azure AD OAuth2 Device Code flow and exchanges the resulting Azure AD access token
//! for a SkypeToken via the Teams `authsvc` endpoint.
//!
//! All secrets (tokens) are redacted when printed.

use serde::{Deserialize, Serialize};

use crate::failure::Failure;

/// The first-party Microsoft Teams desktop/mobile application ID for work and school accounts.
pub const TEAMS_CLIENT_ID: &str = "1fec8e78-bce4-4aaf-ab1b-5451cc387264";

/// The first-party Microsoft Teams application ID for personal / consumer accounts.
pub const TEAMS_CONSUMER_CLIENT_ID: &str = "8ec6bc83-69c8-4392-8f08-b3c986009232";

/// The default tenant identifier for accounts (multi-tenant common).
pub const DEFAULT_TENANT: &str = "common";

/// The Skype Spaces audience resource required for Teams chat APIs.
pub const RESOURCE_SPACES: &str = "https://api.spaces.skype.com";

/// The chat service aggregator's audience: the teams-and-channels list
/// (CSA) takes only a bearer token minted for it, not the skype token.
pub const RESOURCE_CSA: &str = "https://chatsvcagg.teams.microsoft.com";

/// The middle tier's audience for personal accounts, where people are
/// looked up; the Teams web client mints it from its refresh token (seen
/// in a recording of teams.live.com, 2026-10-07).
pub const RESOURCE_MT_PERSONAL: &str = "https://mtsvc.fl.teams.microsoft.com";

/// The groups service's audience for personal accounts, where chats are
/// started.
pub const RESOURCE_GROUPS_PERSONAL: &str = "https://groupssvc.fl.teams.microsoft.com";

/// The IC3 audience, which the work web client's chat and media services
/// take as a bearer token (pictures on `asyncgw.teams.microsoft.com`).
pub const RESOURCE_IC3: &str = "https://ic3.teams.office.com";

/// Microsoft Graph's audience, for looking people up by id.
pub const RESOURCE_GRAPH: &str = "https://graph.microsoft.com";

/// An access token for one audience other than the chat service's.
#[derive(Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AudienceToken {
    pub token: String,
    /// Unix timestamp (seconds) when it expires.
    #[serde(default)]
    pub expires_at: Option<u64>,
}

impl std::fmt::Debug for AudienceToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AudienceToken")
            .field("token", &crate::redact::REDACTED)
            .field("expires_at", &self.expires_at)
            .finish()
    }
}

/// Teams token authorization service for work accounts.
pub const AUTHZ_URL_WORK: &str = "https://teams.microsoft.com/api/authsvc/v1.0/authz";

/// Teams token authorization service for personal / consumer accounts.
pub const AUTHZ_URL_PERSONAL: &str = "https://teams.live.com/api/auth/v1.0/authz/consumer";

/// The only scope Microsoft gives the consumer client for the chat
/// service; `https://api.spaces.skype.com/.default` is refused with
/// AADSTS70011 (seen with `--teams-probe`, 2026-10-07).
pub const SCOPE_PERSONAL: &str =
    "service::api.fl.spaces.skype.com::MBI_SSL openid profile offline_access";

/// Which kind of Microsoft account a Teams sign-in is: they share the
/// chat protocol but sign in through different clients, scopes and
/// services, and a token from one is no good to the other's.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Account {
    /// A work or school account (Entra ID), on teams.microsoft.com.
    #[default]
    Work,
    /// A personal Microsoft account (Teams free), on teams.live.com.
    Personal,
}

impl Account {
    /// The first-party client id to sign in and refresh as.
    pub fn client_id(self) -> &'static str {
        match self {
            Self::Work => TEAMS_CLIENT_ID,
            Self::Personal => TEAMS_CONSUMER_CLIENT_ID,
        }
    }

    /// The tenant to sign in through when none is given.
    pub fn default_tenant(self) -> &'static str {
        match self {
            Self::Work => DEFAULT_TENANT,
            Self::Personal => "consumers",
        }
    }

    /// The scope that buys an access token for `resource`, if this kind of
    /// account can have one: personal accounts reach the chat service and
    /// the middle tier only.
    pub fn scope_for(self, resource: &str) -> Option<String> {
        match self {
            Self::Work => Some(format!("{resource}/.default offline_access")),
            Self::Personal if resource == RESOURCE_SPACES => Some(SCOPE_PERSONAL.to_owned()),
            Self::Personal if resource == RESOURCE_MT_PERSONAL => Some(format!(
                "{RESOURCE_MT_PERSONAL}/teams.mt.readwrite openid profile offline_access"
            )),
            Self::Personal if resource == RESOURCE_GROUPS_PERSONAL => Some(format!(
                "{RESOURCE_GROUPS_PERSONAL}/teams.readwrite openid profile offline_access"
            )),
            Self::Personal => None,
        }
    }

    /// Where the access token is traded for a skype token.
    pub fn authz_url(self) -> &'static str {
        match self {
            Self::Work => AUTHZ_URL_WORK,
            Self::Personal => AUTHZ_URL_PERSONAL,
        }
    }
}

/// Credentials held for an authenticated Teams session.
#[derive(Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TeamsCredentials {
    /// Azure AD access token for Skype Spaces API.
    pub access_token: String,
    /// Azure AD refresh token.
    #[serde(default)]
    pub refresh_token: Option<String>,
    /// Skype token derived from `authsvc`.
    #[serde(default)]
    pub skype_token: Option<String>,
    /// Unix timestamp (seconds) when the access token expires.
    #[serde(default)]
    pub expires_at: Option<u64>,
    /// Tenant ID or "organizations".
    #[serde(default)]
    pub tenant_id: Option<String>,
    /// Regional routing endpoints returned by authsvc.
    #[serde(default)]
    pub region_gtms: Option<serde_json::Value>,
    /// Access tokens for other audiences, such as the chat service
    /// aggregator ([`RESOURCE_CSA`]) or Graph ([`RESOURCE_GRAPH`]), by
    /// audience, each minted when first wanted.
    #[serde(default)]
    pub audiences: std::collections::BTreeMap<String, AudienceToken>,
    /// Which kind of account signed in; refreshes must use the same.
    #[serde(default)]
    pub account: Account,
}

impl std::fmt::Debug for TeamsCredentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TeamsCredentials")
            .field("access_token", &crate::redact::REDACTED)
            .field(
                "refresh_token",
                &self.refresh_token.as_ref().map(|_| crate::redact::REDACTED),
            )
            .field(
                "skype_token",
                &self.skype_token.as_ref().map(|_| crate::redact::REDACTED),
            )
            .field("expires_at", &self.expires_at)
            .field("tenant_id", &self.tenant_id)
            .field("audiences", &self.audiences)
            .field("account", &self.account)
            .finish()
    }
}

impl TeamsCredentials {
    /// Constructs credentials from an access token and optional refresh token.
    pub fn new(
        access_token: String,
        refresh_token: Option<String>,
        expires_in_secs: Option<u64>,
    ) -> Self {
        let expires_at = expires_in_secs.map(|s| now_secs() + s);
        Self {
            access_token,
            refresh_token,
            expires_at,
            tenant_id: Some(DEFAULT_TENANT.to_owned()),
            ..Self::default()
        }
    }

    /// Whether the access token is expired or within 5 minutes of expiring.
    pub fn is_expired(&self, now_secs: u64) -> bool {
        match self.expires_at {
            Some(exp) => now_secs + 300 >= exp,
            None => false,
        }
    }

    /// The token for `audience`, while it has more than five minutes left.
    pub fn fresh_token_for(&self, audience: &str, now_secs: u64) -> Option<&str> {
        let held = self.audiences.get(audience)?;
        let fresh = held.expires_at.is_some_and(|exp| now_secs + 300 < exp);
        Some(held.token.as_str()).filter(|token| fresh && !token.is_empty())
    }

    /// The base chat service URL from `region_gtms`, falling back to default.
    pub fn chat_service_url(&self) -> &str {
        self.region_gtms
            .as_ref()
            .and_then(|v| v.get("chatService"))
            .and_then(|s| s.as_str())
            .unwrap_or("https://amer.ng.msg.teams.microsoft.com")
    }

    /// The middle tier's URL from `region_gtms`, where Teams looks people
    /// up (`https://teams.microsoft.com/api/mt/…`).
    pub fn middle_tier_url(&self) -> Option<&str> {
        self.region_gtms
            .as_ref()
            .and_then(|v| v.get("middleTier"))
            .and_then(|s| s.as_str())
    }

    /// The chat service aggregator URL from `region_gtms`.
    pub fn chatsvcagg_url(&self) -> &str {
        self.region_gtms
            .as_ref()
            .and_then(|v| v.get("chatServiceAggregator"))
            .and_then(|s| s.as_str())
            .unwrap_or("https://chatsvcagg.teams.microsoft.com")
    }
}

/// Device code prompt returned by Azure AD device code flow.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceCodeResponse {
    #[serde(rename = "user_code")]
    pub user_code: String,
    #[serde(rename = "device_code")]
    pub device_code: String,
    #[serde(rename = "verification_uri")]
    pub verification_uri: String,
    #[serde(default)]
    pub expires_in: u64,
    #[serde(default = "default_interval")]
    pub interval: u64,
    #[serde(default)]
    pub message: String,
}

fn default_interval() -> u64 {
    5
}

/// Response from Azure AD token endpoint.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TokenResponse {
    pub access_token: String,
    #[serde(default)]
    pub refresh_token: Option<String>,
    #[serde(default)]
    pub token_type: Option<String>,
    #[serde(default)]
    pub expires_in: Option<u64>,
    #[serde(default)]
    pub id_token: Option<String>,
}

/// Response from Teams `authsvc` token exchange.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuthzResponse {
    /// Where the work service puts the skype token.
    pub tokens: Option<AuthzTokens>,
    /// Where the personal service puts it (`skypeToken.skypetoken`).
    #[serde(default, rename = "skypeToken")]
    pub consumer: Option<ConsumerToken>,
    #[serde(rename = "regionGtms")]
    pub region_gtms: Option<serde_json::Value>,
}

impl AuthzResponse {
    /// The skype token, from whichever shape the service answered in.
    pub fn skype_token(&self) -> Option<String> {
        self.tokens
            .as_ref()
            .and_then(|t| t.skype_token.clone())
            .or_else(|| self.consumer.as_ref().and_then(|t| t.skype_token.clone()))
            .filter(|t| !t.is_empty())
    }
}

/// The skype token as the personal service nests it.
#[derive(Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConsumerToken {
    #[serde(default, rename = "skypetoken")]
    pub skype_token: Option<String>,
    #[serde(default, rename = "expiresIn")]
    pub expires_in: Option<u64>,
}

impl std::fmt::Debug for ConsumerToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ConsumerToken")
            .field(
                "skype_token",
                &self.skype_token.as_ref().map(|_| crate::redact::REDACTED),
            )
            .field("expires_in", &self.expires_in)
            .finish()
    }
}

/// Tokens nested in `authsvc` response.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuthzTokens {
    #[serde(rename = "skypeToken")]
    pub skype_token: Option<String>,
    #[serde(rename = "expiresIn")]
    pub expires_in: Option<u64>,
}

/// Generates the device code URL for a given tenant.
pub fn device_code_url(tenant: &str) -> String {
    format!(
        "https://login.microsoftonline.com/{}/oauth2/v2.0/devicecode",
        tenant
    )
}

/// Generates the token exchange URL for a given tenant.
pub fn token_url(tenant: &str) -> String {
    format!(
        "https://login.microsoftonline.com/{}/oauth2/v2.0/token",
        tenant
    )
}

/// Seconds since the Unix epoch, for token expiry.
pub fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

/// The error code of a refused answer, which is safe to log. The body
/// itself is not: it can echo what was sent, tokens included.
pub fn error_code(body: &str) -> String {
    serde_json::from_str::<serde_json::Value>(body)
        .ok()
        .and_then(|value| {
            ["error", "errorCode", "code"]
                .iter()
                .find_map(|key| value.get(*key).and_then(|code| code.as_str()))
                .map(str::to_owned)
        })
        .unwrap_or_else(|| "(none)".to_owned())
}

/// Initiates the Azure AD device code authentication flow.
pub async fn start_device_code_flow(
    http: &reqwest::Client,
    account: Account,
    tenant: Option<&str>,
) -> Result<DeviceCodeResponse, Failure> {
    let url = device_code_url(tenant.unwrap_or(account.default_tenant()));
    let scope = account
        .scope_for(RESOURCE_SPACES)
        .ok_or(Failure::Unsupported)?;
    let params = [("client_id", account.client_id()), ("scope", &scope)];
    let resp = http
        .post(&url)
        .form(&params)
        .send()
        .await
        .map_err(|e| Failure::Network(e.without_url().to_string()))?;

    let status = resp.status();
    let body = resp
        .text()
        .await
        .map_err(|e| Failure::Unexpected(e.to_string()))?;

    if !status.is_success() {
        log::error!(
            "start_device_code_flow failed: HTTP {status} ({})",
            error_code(&body)
        );
        return Err(Failure::Http(status.as_u16()));
    }

    serde_json::from_str::<DeviceCodeResponse>(&body)
        .map_err(|e| Failure::Unexpected(e.to_string()))
}

/// Polls Azure AD for completion of the device code flow until success, expiration, or error.
pub async fn poll_device_code_token(
    http: &reqwest::Client,
    account: Account,
    device_code: &str,
    mut interval: u64,
    expires_in: u64,
    tenant: Option<&str>,
) -> Result<TokenResponse, Failure> {
    let url = token_url(tenant.unwrap_or(account.default_tenant()));
    let start = std::time::Instant::now();
    let timeout = std::time::Duration::from_secs(expires_in);

    if interval == 0 {
        interval = 5;
    }

    loop {
        if start.elapsed() >= timeout {
            return Err(Failure::LinkExpired);
        }

        tokio::time::sleep(std::time::Duration::from_secs(interval)).await;

        let params = [
            ("client_id", account.client_id()),
            ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
            ("device_code", device_code),
        ];

        let resp = http
            .post(&url)
            .form(&params)
            .send()
            .await
            .map_err(|e| Failure::Network(e.without_url().to_string()))?;

        let status = resp.status();
        let body = resp
            .text()
            .await
            .map_err(|e| Failure::Unexpected(e.to_string()))?;

        if status.is_success() {
            let token_resp: TokenResponse =
                serde_json::from_str(&body).map_err(|e| Failure::Unexpected(e.to_string()))?;
            return Ok(token_resp);
        }

        // Parse error response
        if let Ok(err_val) = serde_json::from_str::<serde_json::Value>(&body)
            && let Some(err_code) = err_val.get("error").and_then(|v| v.as_str())
        {
            match err_code {
                "authorization_pending" => {
                    // User hasn't finished entering code yet, continue waiting
                    continue;
                }
                "slow_down" => {
                    interval = interval.saturating_add(5);
                    continue;
                }
                "expired_token" => {
                    return Err(Failure::LinkExpired);
                }
                "access_denied" => {
                    return Err(Failure::SignedOut);
                }
                _ => {
                    log::warn!("poll_device_code_token error: {err_code}");
                    return Err(Failure::Unexpected(err_code.to_string()));
                }
            }
        }

        log::error!("poll_device_code_token HTTP {status}");
        return Err(Failure::Http(status.as_u16()));
    }
}

/// Exchanges an access token for a Teams skype token and the regional
/// routing endpoints, at the service for its kind of account. The token
/// is bound to the client that signed in, so trying the other service
/// would only be refused.
pub async fn exchange_skype_token(
    http: &reqwest::Client,
    access_token: &str,
    account: Account,
) -> Result<AuthzResponse, Failure> {
    let url = account.authz_url();
    log::debug!("exchanging the access token for a skype token at {url}");
    let mut request = http
        .post(url)
        .bearer_auth(access_token)
        .header("Content-Length", "0");
    if account == Account::Personal {
        request = consumer_headers(request);
    }
    let resp = request
        .send()
        .await
        .map_err(|e| Failure::Network(e.without_url().to_string()))?;
    let status = resp.status();
    let body = resp
        .text()
        .await
        .map_err(|e| Failure::Network(e.without_url().to_string()))?;
    if !status.is_success() {
        log::warn!(
            "skype token exchange failed at {url}: HTTP {status} ({})",
            error_code(&body)
        );
        return Err(Failure::Http(status.as_u16()));
    }
    let authz: AuthzResponse =
        serde_json::from_str(&body).map_err(|e| Failure::Unexpected(e.to_string()))?;
    if authz.skype_token().is_none() {
        return Err(Failure::Unexpected("no skype token in the answer".into()));
    }
    Ok(authz)
}

/// The headers Teams' personal web client sends, which the consumer
/// services were seen to answer with them (`--teams-probe`).
pub fn consumer_headers(request: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
    request
        .header("Accept", "application/json; ver=1.0")
        .header("X-MS-Client-Consumer-Type", "teams4life")
        .header("ms-ic3-product", "tfl")
}

/// Extracts claims JSON object from an unverified JWT token payload.
pub fn parse_jwt_claims(jwt: &str) -> Option<serde_json::Value> {
    use base64::Engine as _;
    let payload = jwt.split('.').nth(1)?;
    let decoded = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload)
        .or_else(|_| base64::engine::general_purpose::URL_SAFE.decode(payload))
        .or_else(|_| base64::engine::general_purpose::STANDARD_NO_PAD.decode(payload))
        .or_else(|_| base64::engine::general_purpose::STANDARD.decode(payload))
        .ok()?;
    serde_json::from_slice(&decoded).ok()
}

/// Trades the refresh token for an access token to `resource`. Microsoft
/// may rotate the refresh token as it answers, so the caller keeps the one
/// returned (see [`crate::teams::client::TeamsClient`], which runs one
/// redemption at a time).
pub async fn redeem(
    http: &reqwest::Client,
    creds: &TeamsCredentials,
    resource: &str,
) -> Result<TokenResponse, Failure> {
    let refresh_token = creds
        .refresh_token
        .as_deref()
        .filter(|t| !t.is_empty())
        .ok_or(Failure::SignedOut)?;
    let account = creds.account;
    let tenant = creds
        .tenant_id
        .as_deref()
        .unwrap_or(account.default_tenant());
    // A personal account has no token for anything but the chat service.
    let scope = account.scope_for(resource).ok_or(Failure::Unsupported)?;
    log::info!("redeeming the Teams refresh token for {resource} in tenant {tenant}");
    let params = [
        ("client_id", account.client_id()),
        ("grant_type", "refresh_token"),
        ("refresh_token", refresh_token),
        ("scope", &scope),
    ];
    let resp = http
        .post(token_url(tenant))
        .form(&params)
        .send()
        .await
        .map_err(|e| Failure::Network(e.without_url().to_string()))?;
    let status = resp.status();
    let body = resp
        .text()
        .await
        .map_err(|e| Failure::Unexpected(e.to_string()))?;
    if !status.is_success() {
        let code = error_code(&body);
        log::warn!("Azure AD token refresh failed: HTTP {status} ({code})");
        // The refresh token was revoked or ran out: only signing in again helps.
        return Err(if code == "invalid_grant" {
            Failure::SignedOut
        } else {
            Failure::Http(status.as_u16())
        });
    }
    serde_json::from_str(&body).map_err(|e| Failure::Unexpected(e.to_string()))
}

/// Renews the SkypeToken: from the current Azure AD access token while it
/// is fresh, else from a new one bought with the refresh token.
pub async fn refresh_credentials(
    http: &reqwest::Client,
    creds: &TeamsCredentials,
) -> Result<TeamsCredentials, Failure> {
    let now = now_secs();

    // 1. If access token is still fresh, try exchanging for Skype token first
    if !creds.is_expired(now)
        && !creds.access_token.is_empty()
        && let Ok(authz) = exchange_skype_token(http, &creds.access_token, creds.account).await
        && let Some(st) = authz.skype_token()
    {
        let mut refreshed = creds.clone();
        refreshed.skype_token = Some(st);
        if let Some(gtms) = authz.region_gtms {
            refreshed.region_gtms = Some(gtms);
        }
        log::info!("Skype token renewed using the current access token");
        return Ok(refreshed);
    }

    // 2. Otherwise refresh AAD access token using refresh_token
    let token_resp = redeem(http, creds, RESOURCE_SPACES).await?;
    let authz = exchange_skype_token(http, &token_resp.access_token, creds.account).await?;

    log::info!("Teams credentials successfully refreshed");

    Ok(TeamsCredentials {
        access_token: token_resp.access_token,
        refresh_token: token_resp
            .refresh_token
            .or_else(|| creds.refresh_token.clone()),
        skype_token: authz.skype_token(),
        expires_at: token_resp.expires_in.map(|s| now + s),
        region_gtms: authz.region_gtms.or_else(|| creds.region_gtms.clone()),
        ..creds.clone()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_device_code_response() {
        let json = r#"{
            "user_code": "ABCD-EFGH",
            "device_code": "dev-code-12345",
            "verification_uri": "https://microsoft.com/devicelogin",
            "expires_in": 900,
            "interval": 5,
            "message": "To sign in, use a web browser to open the page https://microsoft.com/devicelogin and enter the code ABCD-EFGH to authenticate."
        }"#;

        let resp: DeviceCodeResponse = serde_json::from_str(json).expect("valid response");
        assert_eq!(resp.user_code, "ABCD-EFGH");
        assert_eq!(resp.device_code, "dev-code-12345");
        assert_eq!(resp.verification_uri, "https://microsoft.com/devicelogin");
        assert_eq!(resp.expires_in, 900);
        assert_eq!(resp.interval, 5);
    }

    #[test]
    fn parses_token_response() {
        let json = r#"{
            "token_type": "Bearer",
            "scope": "https://api.spaces.skype.com/.default",
            "expires_in": 3600,
            "access_token": "secret-access-token",
            "refresh_token": "secret-refresh-token"
        }"#;

        let resp: TokenResponse = serde_json::from_str(json).expect("valid response");
        assert_eq!(resp.access_token, "secret-access-token");
        assert_eq!(resp.refresh_token.as_deref(), Some("secret-refresh-token"));
        assert_eq!(resp.expires_in, Some(3600));
    }

    #[test]
    fn parses_authz_response() {
        let json = r#"{
            "tokens": {
                "skypeToken": "secret-skype-token",
                "expiresIn": 86400
            },
            "regionGtms": {
                "chatService": "https://emea.ng.msg.teams.microsoft.com",
                "chatServiceAggregator": "https://chatsvcagg.teams.microsoft.com"
            }
        }"#;

        let resp: AuthzResponse = serde_json::from_str(json).expect("valid authz");
        let tokens = resp.tokens.expect("tokens");
        assert_eq!(tokens.skype_token.as_deref(), Some("secret-skype-token"));
        assert_eq!(tokens.expires_in, Some(86400));
        let gtms = resp.region_gtms.expect("regionGtms");
        assert_eq!(
            gtms["chatService"].as_str(),
            Some("https://emea.ng.msg.teams.microsoft.com")
        );
    }

    #[test]
    fn the_personal_service_nests_its_skype_token() {
        let personal: AuthzResponse = serde_json::from_str(
            r#"{"skypeToken":{"skypetoken":"secret-personal","expiresIn":86398,"skypeid":"live:.cid.1"},
                "regionGtms":{"chatService":"https://msgapi.teams.live.com","middleTier":"https://teams.live.com/api/mt"}}"#,
        )
        .expect("valid authz");
        assert_eq!(personal.skype_token().as_deref(), Some("secret-personal"));
        assert!(!format!("{personal:?}").contains("secret-personal"));
        let work: AuthzResponse =
            serde_json::from_str(r#"{"tokens":{"skypeToken":"secret-work","expiresIn":86400}}"#)
                .expect("valid authz");
        assert_eq!(work.skype_token().as_deref(), Some("secret-work"));
        assert_eq!(AuthzResponse::default().skype_token(), None);
    }

    #[test]
    fn each_account_signs_in_its_own_way() {
        assert_eq!(Account::Work.client_id(), TEAMS_CLIENT_ID);
        assert_eq!(Account::Personal.client_id(), TEAMS_CONSUMER_CLIENT_ID);
        assert_eq!(Account::Personal.default_tenant(), "consumers");
        assert_eq!(
            Account::Personal.scope_for(RESOURCE_SPACES).as_deref(),
            Some(SCOPE_PERSONAL)
        );
        assert_eq!(Account::Personal.scope_for(RESOURCE_GRAPH), None);
        assert_eq!(
            Account::Work.scope_for(RESOURCE_CSA).as_deref(),
            Some("https://chatsvcagg.teams.microsoft.com/.default offline_access")
        );
        // Saved before accounts had a kind: work.
        let old: TeamsCredentials =
            serde_json::from_str(r#"{"access_token":"a"}"#).expect("valid credentials");
        assert_eq!(old.account, Account::Work);
    }

    #[test]
    fn credentials_redacts_tokens_in_debug() {
        let creds = TeamsCredentials {
            access_token: "secret-access".into(),
            refresh_token: Some("secret-refresh".into()),
            skype_token: Some("secret-skype".into()),
            expires_at: Some(123456789),
            tenant_id: Some("org".into()),
            audiences: [(
                RESOURCE_CSA.to_owned(),
                AudienceToken {
                    token: "secret-csa".into(),
                    expires_at: None,
                },
            )]
            .into(),
            ..TeamsCredentials::default()
        };
        let debug_str = format!("{creds:?}");
        assert!(!debug_str.contains("secret-access"));
        assert!(!debug_str.contains("secret-refresh"));
        assert!(!debug_str.contains("secret-skype"));
        assert!(!debug_str.contains("secret-csa"));
        assert!(debug_str.contains("<redacted>"));
    }

    #[test]
    fn parses_jwt_claims_correctly() {
        // {"oid":"user-1234","name":"Test User","upn":"test@example.com","tid":"tenant-5678"}
        // base64url: eyJvaWQiOiJ1c2VyLTEyMzQiLCJuYW1lIjoiVGVzdCBVc2VyIiwidXBuIjoidGVzdEBleGFtcGxlLmNvbSIsInRpZCI6InRlbmFudC01Njc4In0
        let fake_jwt = "header.eyJvaWQiOiJ1c2VyLTEyMzQiLCJuYW1lIjoiVGVzdCBVc2VyIiwidXBuIjoidGVzdEBleGFtcGxlLmNvbSIsInRpZCI6InRlbmFudC01Njc4In0.sig";
        let claims = parse_jwt_claims(fake_jwt).expect("valid claims");
        assert_eq!(claims["oid"].as_str(), Some("user-1234"));
        assert_eq!(claims["name"].as_str(), Some("Test User"));
        assert_eq!(claims["upn"].as_str(), Some("test@example.com"));
        assert_eq!(claims["tid"].as_str(), Some("tenant-5678"));

        assert!(parse_jwt_claims("not-a-jwt").is_none());
    }

    #[test]
    fn credentials_expiry_check() {
        let now = 1000;
        let mut creds = TeamsCredentials::default();
        assert!(!creds.is_expired(now));

        // Expires in 100s: should be considered expired since buffer is 300s
        creds.expires_at = Some(1100);
        assert!(creds.is_expired(now));

        // Expires in 400s: not expired yet
        creds.expires_at = Some(1400);
        assert!(!creds.is_expired(now));
    }

    #[test]
    fn an_audience_token_is_used_only_while_fresh() {
        let held = |expires_at| TeamsCredentials {
            audiences: [(
                RESOURCE_CSA.to_owned(),
                AudienceToken {
                    token: "csa".into(),
                    expires_at,
                },
            )]
            .into(),
            ..TeamsCredentials::default()
        };
        assert_eq!(
            held(Some(1400)).fresh_token_for(RESOURCE_CSA, 1000),
            Some("csa")
        );
        assert_eq!(held(Some(1400)).fresh_token_for(RESOURCE_GRAPH, 1000), None);
        assert_eq!(held(Some(1100)).fresh_token_for(RESOURCE_CSA, 1000), None);
        assert_eq!(held(None).fresh_token_for(RESOURCE_CSA, 1000), None);
    }

    #[test]
    fn error_codes_leave_the_rest_of_the_body_out() {
        let body =
            r#"{"error":"invalid_grant","error_description":"AADSTS70000 token eyJabc.def.ghi"}"#;
        assert_eq!(error_code(body), "invalid_grant");
        assert_eq!(error_code(r#"{"errorCode":"Forbidden"}"#), "Forbidden");
        assert_eq!(error_code("<html>"), "(none)");
    }

    #[tokio::test]
    async fn refresh_credentials_fails_without_refresh_token() {
        let http = reqwest::Client::new();
        let creds = TeamsCredentials::default();
        let res = refresh_credentials(&http, &creds).await;
        assert_eq!(res, Err(Failure::SignedOut));
    }
}
