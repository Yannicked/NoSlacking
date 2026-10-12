//! Presence: how available people are, and telling Teams you are here.

use super::{TeamsClient, id_of_mri, refused, user_mri};
use crate::failure::Failure;
use crate::teams::auth::{Account, RESOURCE_MT_PERSONAL, RESOURCE_PRESENCE};

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

impl TeamsClient {
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
    /// Trouter connection): without it Teams shows you offline. Keeps the
    /// endpoint for later presence requests.
    pub async fn publish_presence(&self, endpoint: &str) -> Result<(), Failure> {
        if let Ok(mut held) = self.endpoint.write() {
            *held = Some(endpoint.to_owned());
        }
        let body = serde_json::json!({
            "id": endpoint,
            "availability": "Available",
            "activity": "Available",
            "activityReporting": "Transport",
            "deviceType": "Desktop",
        });
        let resp = self
            .presence_request("me/endpoints/", |request| request.json(&body))
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
        // As the web client sends it with each request; none before
        // Trouter first connects.
        let endpoint = self
            .endpoint
            .read()
            .ok()
            .and_then(|e| e.clone())
            .unwrap_or_default();
        let build = |request: reqwest::RequestBuilder| {
            let request = if endpoint.is_empty() {
                request
            } else {
                request.header("x-ms-endpoint-id", &endpoint)
            };
            build(request)
        };
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
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
