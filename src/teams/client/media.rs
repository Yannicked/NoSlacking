//! Pictures and files on Teams' media hosts, fetched with the cookie
//! those hosts sign in with.

use super::{PERSONAL_ORIGIN, TeamsClient, is_refusal, refused};
use crate::failure::Failure;
use crate::teams::auth::{Account, RESOURCE_IC3, RESOURCE_MT_PERSONAL};

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

impl TeamsClient {
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
        // What went wrong with a picture, by its kind only: its address
        // names people.
        let kind = if url.contains("/profilepicturev2") {
            "a person's picture"
        } else {
            "a picture"
        };
        if !resp.status().is_success() {
            log::info!("Teams refused {kind}: HTTP {}", resp.status().as_u16());
            return Err(Failure::Http(resp.status().as_u16()));
        }
        let content_type = resp
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_owned();
        if !content_type.starts_with("image/") {
            log::info!("Teams sent {kind} that is not one: {content_type:?}");
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
        // As the web client's page asks for a picture.
        self.http
            .get(url)
            .header(reqwest::header::COOKIE, cookie)
            .header(
                reqwest::header::ACCEPT,
                "image/avif,image/webp,image/apng,image/svg+xml,image/*,*/*;q=0.8",
            )
            .header(reqwest::header::REFERER, format!("{PERSONAL_ORIGIN}/v2/"))
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
            if fresh {
                // Asked again after a refusal: as the web client asks
                // (recorded), by the skype token alone, as a form from its
                // page.
                self.http
                    .post(&url)
                    .header("x-skypetoken", &skype)
                    .header("x-ms-client-type", "web")
                    .header(reqwest::header::ACCEPT, "*/*")
                    .header(
                        reqwest::header::CONTENT_TYPE,
                        "application/x-www-form-urlencoded",
                    )
                    .header(reqwest::header::ORIGIN, PERSONAL_ORIGIN)
                    .header(reqwest::header::REFERER, format!("{PERSONAL_ORIGIN}/v2/"))
                    .header(reqwest::header::CONTENT_LENGTH, "0")
                    .send()
                    .await
                    .map_err(|e| Failure::Network(e.without_url().to_string()))?
            } else {
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
            }
        };
        if !resp.status().is_success() {
            return Err(refused(resp, "sign in for pictures").await);
        }
        let cookie = cookies_of(resp.headers());
        // Which cookies came, by name: their values are the sign-in.
        let names: Vec<&str> = cookie
            .split("; ")
            .filter_map(|pair| pair.split_once('=').map(|(name, _)| name))
            .collect();
        log::debug!(
            "Teams cookies for pictures at {host}{}: {names:?}",
            if fresh {
                ", asked as the web client asks"
            } else {
                ""
            }
        );
        if cookie.is_empty() {
            return Err(Failure::Unexpected("no cookie for pictures".into()));
        }
        if let Ok(mut held) = self.media_cookies.write() {
            held.insert(host.to_owned(), cookie.clone());
        }
        Ok(cookie)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
