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

/// Teams token authorization service for work accounts.
pub const AUTHZ_URL_WORK: &str = "https://teams.microsoft.com/api/authsvc/v1.0/authz";

/// Teams token authorization service for personal / consumer accounts.
pub const AUTHZ_URL_PERSONAL: &str = "https://teams.live.com/api/auth/v1.0/authz/consumer";

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
    /// Azure AD access token for the chat service aggregator
    /// ([`RESOURCE_CSA`]), minted when the teams list is first wanted.
    #[serde(default)]
    pub csa_token: Option<String>,
    /// Unix timestamp (seconds) when `csa_token` expires.
    #[serde(default)]
    pub csa_expires_at: Option<u64>,
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
            .field(
                "csa_token",
                &self.csa_token.as_ref().map(|_| crate::redact::REDACTED),
            )
            .field("csa_expires_at", &self.csa_expires_at)
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

    /// The CSA token, while it has more than five minutes left.
    pub fn fresh_csa_token(&self, now_secs: u64) -> Option<&str> {
        let fresh = self.csa_expires_at.is_some_and(|exp| now_secs + 300 < exp);
        self.csa_token
            .as_deref()
            .filter(|token| fresh && !token.is_empty())
    }

    /// The base chat service URL from `region_gtms`, falling back to default.
    pub fn chat_service_url(&self) -> &str {
        self.region_gtms
            .as_ref()
            .and_then(|v| v.get("chatService"))
            .and_then(|s| s.as_str())
            .unwrap_or("https://amer.ng.msg.teams.microsoft.com")
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
    pub tokens: Option<AuthzTokens>,
    #[serde(rename = "regionGtms")]
    pub region_gtms: Option<serde_json::Value>,
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
    tenant: Option<&str>,
) -> Result<DeviceCodeResponse, Failure> {
    let url = device_code_url(tenant.unwrap_or(DEFAULT_TENANT));
    let scope = format!("{RESOURCE_SPACES}/.default offline_access");
    let params = [("client_id", TEAMS_CLIENT_ID), ("scope", &scope)];
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
    device_code: &str,
    mut interval: u64,
    expires_in: u64,
    tenant: Option<&str>,
) -> Result<TokenResponse, Failure> {
    let url = token_url(tenant.unwrap_or(DEFAULT_TENANT));
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
            ("client_id", TEAMS_CLIENT_ID),
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

/// Exchanges an Azure AD access token for a Teams SkypeToken and regional routing endpoints.
pub async fn exchange_skype_token(
    http: &reqwest::Client,
    aad_access_token: &str,
    is_consumer: bool,
) -> Result<AuthzResponse, Failure> {
    let primary_url = if is_consumer {
        AUTHZ_URL_PERSONAL
    } else {
        AUTHZ_URL_WORK
    };

    log::debug!("exchanging AAD token for Skype token at {primary_url}...");

    let resp = http
        .post(primary_url)
        .bearer_auth(aad_access_token)
        .header("Content-Length", "0")
        .send()
        .await
        .map_err(|e| Failure::Network(e.without_url().to_string()))?;

    let status = resp.status();
    let body = resp
        .text()
        .await
        .map_err(|e| Failure::Network(e.without_url().to_string()))?;

    if status.is_success() {
        let authz: AuthzResponse =
            serde_json::from_str(&body).map_err(|e| Failure::Unexpected(e.to_string()))?;
        return Ok(authz);
    }

    log::warn!(
        "authsvc token exchange failed at {primary_url}: HTTP {status} ({})",
        error_code(&body)
    );

    // If the primary endpoint failed with 401 or 403, try the fallback endpoint
    // in case a consumer/personal Microsoft account was used or vice versa.
    let fallback_url = if is_consumer {
        AUTHZ_URL_WORK
    } else {
        AUTHZ_URL_PERSONAL
    };

    log::info!("attempting fallback authsvc endpoint at {fallback_url}...");
    let fallback_resp = http
        .post(fallback_url)
        .bearer_auth(aad_access_token)
        .header("Content-Length", "0")
        .send()
        .await
        .map_err(|e| Failure::Network(e.without_url().to_string()))?;

    let fallback_status = fallback_resp.status();
    let fallback_body = fallback_resp
        .text()
        .await
        .map_err(|e| Failure::Network(e.without_url().to_string()))?;

    if fallback_status.is_success() {
        let authz: AuthzResponse =
            serde_json::from_str(&fallback_body).map_err(|e| Failure::Unexpected(e.to_string()))?;
        return Ok(authz);
    }

    log::error!(
        "fallback authsvc exchange failed at {fallback_url}: HTTP {fallback_status} ({})",
        error_code(&fallback_body)
    );
    Err(Failure::Http(status.as_u16()))
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
    let tenant = creds.tenant_id.as_deref().unwrap_or(DEFAULT_TENANT);
    let scope = format!("{resource}/.default offline_access");
    log::info!("redeeming the Teams refresh token for {resource} in tenant {tenant}");
    let params = [
        ("client_id", TEAMS_CLIENT_ID),
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
    if !creds.is_expired(now) && !creds.access_token.is_empty() {
        let is_consumer = creds.tenant_id.as_deref() == Some("consumers");
        if let Ok(authz) = exchange_skype_token(http, &creds.access_token, is_consumer).await
            && let Some(st) = authz.tokens.and_then(|t| t.skype_token)
        {
            let mut refreshed = creds.clone();
            refreshed.skype_token = Some(st);
            if let Some(gtms) = authz.region_gtms {
                refreshed.region_gtms = Some(gtms);
            }
            log::info!("Skype token renewed using existing AAD access token");
            return Ok(refreshed);
        }
    }

    // 2. Otherwise refresh AAD access token using refresh_token
    let token_resp = redeem(http, creds, RESOURCE_SPACES).await?;
    let tenant = creds.tenant_id.as_deref().unwrap_or(DEFAULT_TENANT);
    let is_consumer = tenant == "consumers"
        || tenant == "personal"
        || parse_jwt_claims(&token_resp.access_token)
            .and_then(|c| {
                c.get("tid").and_then(|t| {
                    t.as_str()
                        .map(|s| s == "9188040d-6c67-4c5b-b112-36a304b66dad")
                })
            })
            .unwrap_or(false);

    let authz = exchange_skype_token(http, &token_resp.access_token, is_consumer).await?;

    log::info!("Teams credentials successfully refreshed");

    Ok(TeamsCredentials {
        access_token: token_resp.access_token,
        refresh_token: token_resp
            .refresh_token
            .or_else(|| creds.refresh_token.clone()),
        skype_token: authz.tokens.and_then(|t| t.skype_token),
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
    fn credentials_redacts_tokens_in_debug() {
        let creds = TeamsCredentials {
            access_token: "secret-access".into(),
            refresh_token: Some("secret-refresh".into()),
            skype_token: Some("secret-skype".into()),
            expires_at: Some(123456789),
            tenant_id: Some("org".into()),
            csa_token: Some("secret-csa".into()),
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
    fn a_csa_token_is_used_only_while_fresh() {
        let mut creds = TeamsCredentials {
            csa_token: Some("csa".into()),
            csa_expires_at: Some(1400),
            ..TeamsCredentials::default()
        };
        assert_eq!(creds.fresh_csa_token(1000), Some("csa"));
        creds.csa_expires_at = Some(1100);
        assert_eq!(creds.fresh_csa_token(1000), None);
        creds.csa_expires_at = None;
        assert_eq!(creds.fresh_csa_token(1000), None);
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
