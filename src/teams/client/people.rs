//! People: looking them up by id, searching for them, your own
//! profile, and the photos their profiles point to.

use super::{TeamsClient, id_of_mri, refused};
use crate::failure::Failure;
use crate::teams::auth::{Account, RESOURCE_GRAPH, RESOURCE_MT_PERSONAL};
use crate::teams::types::UserDetails;

/// Where a personal account looks up people with personal accounts, as
/// the Teams web client asks (recorded).
pub const SHORT_PROFILES: &str = "fetchShortProfile?isMailAddress=false&enableGuest=true&skypeTeamsInfo=true&canBeSmtpAddress=false&includeIBBarredUsers=false&includeDisabledAccounts=false&ggEnabled=true";
/// Where a personal account looks up work people met in its chats.
pub const FEDERATED: &str = "fetchFederated?edEnabled=false&includeDisabledAccounts=true";

/// What `fetchShortProfile` answers.
#[derive(serde::Deserialize)]
struct ShortProfiles {
    #[serde(default)]
    value: Vec<ShortProfile>,
}

/// Logs how a lookup went: how many were asked about and named, and the
/// kinds (not the ids) of those left unnamed, which say which service
/// does not know them.
fn log_lookup(ids: &[String], found: &[UserDetails]) {
    let named = |id: &String| {
        found.iter().any(|f| {
            &f.id == id
                && f.display_name
                    .as_deref()
                    .is_some_and(|n| !n.trim().is_empty())
        })
    };
    let mut unnamed: std::collections::BTreeMap<String, usize> = std::collections::BTreeMap::new();
    for id in ids.iter().filter(|id| !named(id)) {
        let mri = user_mri(id);
        let kind = match mri.split(':').collect::<Vec<_>>().as_slice() {
            ["8", kind, ..] => format!("8:{kind}"),
            [kind, ..] => (*kind).to_owned(),
            [] => "?".to_owned(),
        };
        *unnamed.entry(kind).or_default() += 1;
    }
    log::info!(
        "Teams people lookup: {} asked, {} named; unnamed: {unnamed:?}",
        ids.len(),
        ids.len() - unnamed.values().sum::<usize>()
    );
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
    #[serde(default, rename = "imageUri")]
    image_uri: Option<String>,
    /// A personal account's addresses, where `email` is not given.
    #[serde(default, rename = "emailsInfo")]
    emails_info: Vec<EmailInfo>,
    #[serde(default, rename = "jobTitle")]
    job_title: Option<String>,
    #[serde(default)]
    department: Option<String>,
    #[serde(default, rename = "companyName")]
    company_name: Option<String>,
    #[serde(default, rename = "tenantName")]
    tenant_name: Option<String>,
    #[serde(default, rename = "telephoneNumber")]
    telephone_number: Option<String>,
    #[serde(default)]
    mobile: Option<String>,
    #[serde(default, rename = "physicalDeliveryOfficeName")]
    office_name: Option<String>,
    #[serde(default, rename = "userLocation")]
    user_location: Option<String>,
}

/// One of a personal account's addresses.
#[derive(serde::Deserialize)]
struct EmailInfo {
    #[serde(default)]
    address: Option<String>,
}

/// `value` if it says something.
fn said(value: Option<String>) -> Option<String> {
    value.map(|v| v.trim().to_owned()).filter(|v| !v.is_empty())
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
        let email = said(self.email).or_else(|| {
            self.emails_info
                .into_iter()
                .find_map(|info| said(info.address))
        });
        Some(UserDetails {
            id,
            display_name: self.display_name.filter(|n| !n.trim().is_empty()),
            email,
            user_principal_name: self.user_principal_name,
            image_uri: self.image_uri.filter(|u| !u.is_empty()),
            job_title: said(self.job_title),
            department: said(self.department),
            organization: said(self.company_name).or_else(|| said(self.tenant_name)),
            phone: said(self.telephone_number).or_else(|| said(self.mobile)),
            office: said(self.office_name).or_else(|| said(self.user_location)),
        })
    }
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

impl TeamsClient {
    /// Remembers where the photos of `profiles` are.
    pub fn remember_photos(&self, profiles: &[UserDetails]) {
        let Ok(mut photos) = self.photos.write() else {
            return;
        };
        for profile in profiles {
            if let Some(photo) = profile.image_uri.as_ref().filter(|u| !u.is_empty()) {
                photos.insert(profile.id.clone(), photo.clone());
            }
        }
    }

    /// Where the photo of the person with id `id` is, if a profile said.
    pub fn photo(&self, id: &str) -> Option<String> {
        self.photos.read().ok()?.get(id).cloned()
    }

    /// Looks people up by their directory (object) ids: through the
    /// middle tier, as Teams itself does, and through Microsoft Graph when
    /// that fails. People neither knows, such as guests from elsewhere,
    /// are left out.
    pub async fn get_users(&self, ids: &[String]) -> Result<Vec<UserDetails>, Failure> {
        let found = self.look_people_up(ids).await;
        if let Ok(found) = &found {
            log_lookup(ids, found);
        }
        found
    }

    async fn look_people_up(&self, ids: &[String]) -> Result<Vec<UserDetails>, Failure> {
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
        // Each kind to the service that takes it: one MRI it does not
        // take fails its whole request ("No valid Mris found"). Meeting
        // guests and bots have no profile here.
        let mris: Vec<String> = ids.iter().map(|id| user_mri(id)).collect();
        let personal: Vec<String> = mris
            .iter()
            .filter(|mri| mri.starts_with("8:live:"))
            .cloned()
            .collect();
        let work: Vec<String> = mris
            .iter()
            .filter(|mri| mri.starts_with("8:orgid:"))
            .cloned()
            .collect();
        let mut found = Vec::new();
        for (path, mris) in [(SHORT_PROFILES, personal), (FEDERATED, work)] {
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
                // A batch refused is these people unnamed, not everyone.
                if !resp.status().is_success() {
                    let _ = refused(resp, "look people up").await;
                    continue;
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
                crate::percent::encode_strict(id)
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
            image_uri: None,
            ..UserDetails::default()
        })
    }

    /// The current user, from the access token's claims: the Teams
    /// tokens are not good for Microsoft Graph's `/me`.
    pub fn get_me(&self) -> Result<UserDetails, Failure> {
        self.user_from_token()
            .ok_or_else(|| Failure::Unexpected("no user in the Teams token".into()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::teams::auth::TeamsCredentials;

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

    #[test]
    fn profiles_say_where_people_work_and_how_to_reach_them() {
        // A work account's, in the middle tier's fields (recorded shape).
        let work: ShortProfile = serde_json::from_value(serde_json::json!({
            "mri": "8:orgid:aaaa-bbbb",
            "displayName": "Pim Example",
            "email": "pim@example.com",
            "jobTitle": "Engineer",
            "department": "Platform",
            "companyName": "",
            "tenantName": "Example Inc.",
            "telephoneNumber": null,
            "mobile": "+31 6 0000 0000",
            "physicalDeliveryOfficeName": "Utrecht",
        }))
        .expect("reads");
        let details = work.into_details().expect("an id");
        assert_eq!(details.job_title.as_deref(), Some("Engineer"));
        assert_eq!(details.department.as_deref(), Some("Platform"));
        assert_eq!(
            details.organization.as_deref(),
            Some("Example Inc."),
            "the tenant's, the company's empty"
        );
        assert_eq!(
            details.phone.as_deref(),
            Some("+31 6 0000 0000"),
            "the mobile, with no office phone"
        );
        assert_eq!(details.office.as_deref(), Some("Utrecht"));
        // A personal account's address comes in `emailsInfo`.
        let personal: ShortProfile = serde_json::from_value(serde_json::json!({
            "mri": "8:live:someone",
            "displayName": "Someone",
            "emailsInfo": [{"address": "someone@example.com"}],
        }))
        .expect("reads");
        let details = personal.into_details().expect("an id");
        assert_eq!(details.email.as_deref(), Some("someone@example.com"));
        assert_eq!(details.job_title, None);
    }

    #[test]
    fn a_photo_address_is_remembered_for_later_lists() {
        let client = TeamsClient::new(TeamsCredentials::default());
        client.remember_photos(&[
            UserDetails {
                id: "live:ana".into(),
                image_uri: Some(
                    "https://substrate.office.com/profile/v1.0/users/cid:A/image".into(),
                ),
                ..UserDetails::default()
            },
            UserDetails {
                id: "live:bob".into(),
                ..UserDetails::default()
            },
        ]);
        assert!(
            client
                .photo("live:ana")
                .is_some_and(|p| p.ends_with("/image"))
        );
        assert_eq!(client.photo("live:bob"), None);
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
}
