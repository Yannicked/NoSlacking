//! The cookies of the browser sign-in, carried by hand.
//!
//! The sign-in needs cookies in two places: `auth.loginMagicBulk` sets the
//! account's `d` cookie, and a workspace's boot page wants it back across a
//! short redirect chain. Rather than a general cookie jar, [`get`] follows
//! redirects itself, sends the cookies as an explicit `Cookie` header and
//! keeps what each hop sets. That keeps the rules small and strict: the
//! cookies only ever go to an `https` Slack host, and a redirect anywhere
//! else is not followed.

use reqwest::Url;
use reqwest::header::{COOKIE, HeaderValue, LOCATION, SET_COOKIE};

use super::client::SlackError;

/// How many redirects one fetch follows, as many as reqwest's own default.
pub const MAX_HOPS: usize = 10;

/// One cookie as a `Set-Cookie` header sets it.
#[derive(Clone, PartialEq, Eq)]
pub struct SetCookie {
    /// The cookie's name (`d`).
    pub name: String,
    /// Its value, without surrounding quotes.
    pub value: String,
    /// The `Domain` attribute, lowercased and without a leading dot.
    pub domain: Option<String>,
    /// The header removes the cookie rather than sets it: an empty value, a
    /// `Max-Age` of zero or less, or an `Expires` long in the past.
    pub expired: bool,
}

/// The value is a secret (the `d` cookie is the whole account); only the
/// name prints.
impl std::fmt::Debug for SetCookie {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SetCookie")
            .field("name", &self.name)
            .field("value", &crate::redact::REDACTED)
            .field("domain", &self.domain)
            .field("expired", &self.expired)
            .finish()
    }
}

/// Reads one `Set-Cookie` header. Attributes other than `Domain`,
/// `Max-Age` and `Expires` are ignored: every request here is `https`, and
/// the path does not matter for the few pages the sign-in fetches.
pub fn parse_set_cookie(header: &str) -> Option<SetCookie> {
    let mut parts = header.split(';');
    let (name, value) = parts.next()?.split_once('=')?;
    let name = name.trim();
    if name.is_empty() {
        return None;
    }
    let value = value.trim();
    let value = value
        .strip_prefix('"')
        .and_then(|v| v.strip_suffix('"'))
        .unwrap_or(value);
    let mut cookie = SetCookie {
        name: name.to_owned(),
        value: value.to_owned(),
        domain: None,
        expired: value.is_empty(),
    };
    for attribute in parts {
        let (key, val) = attribute.split_once('=').unwrap_or((attribute, ""));
        let val = val.trim();
        match key.trim().to_ascii_lowercase().as_str() {
            "domain" => {
                let domain = val.trim_start_matches('.').to_ascii_lowercase();
                cookie.domain = (!domain.is_empty()).then_some(domain);
            }
            "max-age" if val.parse::<i64>().is_ok_and(|age| age <= 0) => cookie.expired = true,
            "expires" if long_past(val) => cookie.expired = true,
            _ => {}
        }
    }
    Some(cookie)
}

/// Whether an `Expires` date is long past. Servers delete a cookie by
/// dating it 1970; reading the year alone is enough for that and keeps the
/// clock out of it.
fn long_past(date: &str) -> bool {
    date.split(|c: char| !c.is_ascii_digit())
        .filter(|run| run.len() == 4)
        .find_map(|run| run.parse::<u32>().ok())
        .is_some_and(|year| year < 2000)
}

/// Whether `host` is `slack.com` or one of its subdomains.
fn is_slack_host(host: &str) -> bool {
    host == "slack.com"
        || host
            .strip_suffix(".slack.com")
            .is_some_and(|sub| !sub.is_empty() && !sub.starts_with('.') && !sub.ends_with('.'))
}

/// The host of `url` when cookies may go there: `https`, a Slack host, no
/// port of its own and no credentials.
fn slack_host(url: &Url) -> Option<&str> {
    let host = url.host_str()?;
    let valid = url.scheme() == "https"
        && url.port().is_none()
        && url.username().is_empty()
        && url.password().is_none()
        && is_slack_host(host);
    valid.then_some(host)
}

/// A cookie kept between hops.
#[derive(Clone)]
struct Entry {
    name: String,
    value: String,
    /// The host it came from, or the domain it was set for.
    domain: String,
    /// Set without a `Domain`: it goes back to `domain` alone, not to its
    /// subdomains.
    host_only: bool,
}

impl Entry {
    fn matches(&self, host: &str) -> bool {
        host == self.domain
            || (!self.host_only
                && host
                    .strip_suffix(self.domain.as_str())
                    .is_some_and(|sub| sub.ends_with('.')))
    }
}

/// The cookies one sign-in step carries, all of them Slack's.
#[derive(Clone, Default)]
pub struct Cookies {
    entries: Vec<Entry>,
}

/// Cookie values are secrets; only the names print.
impl std::fmt::Debug for Cookies {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_list()
            .entries(self.entries.iter().map(|e| &e.name))
            .finish()
    }
}

impl Cookies {
    /// Cookies holding the account's `d` cookie for every Slack host, as the
    /// browser has it.
    pub fn with_d(value: &str) -> Self {
        let mut cookies = Self::default();
        cookies.put(Entry {
            name: "d".to_owned(),
            value: value.to_owned(),
            domain: "slack.com".to_owned(),
            host_only: false,
        });
        cookies
    }

    fn put(&mut self, entry: Entry) {
        self.entries.retain(|e| {
            !(e.name == entry.name && e.domain == entry.domain && e.host_only == entry.host_only)
        });
        self.entries.push(entry);
    }

    /// Keeps what a response from `url` set. A cookie whose `Domain` is not
    /// a Slack domain covering that host is dropped, as a browser would.
    pub fn store<'a>(&mut self, url: &Url, headers: impl IntoIterator<Item = &'a str>) {
        let Some(host) = slack_host(url) else {
            return;
        };
        for cookie in headers.into_iter().filter_map(parse_set_cookie) {
            let (domain, host_only) = match cookie.domain {
                Some(domain) => {
                    let covers = host == domain
                        || host
                            .strip_suffix(domain.as_str())
                            .is_some_and(|sub| sub.ends_with('.'));
                    if !(is_slack_host(&domain) && covers) {
                        continue;
                    }
                    (domain, false)
                }
                None => (host.to_owned(), true),
            };
            let entry = Entry {
                name: cookie.name,
                value: cookie.value,
                domain,
                host_only,
            };
            if cookie.expired {
                self.entries.retain(|e| {
                    !(e.name == entry.name
                        && e.domain == entry.domain
                        && e.host_only == entry.host_only)
                });
            } else {
                self.put(entry);
            }
        }
    }

    /// The latest value of the cookie `name`, from whichever Slack host set
    /// it.
    pub fn get(&self, name: &str) -> Option<&str> {
        self.entries
            .iter()
            .rev()
            .find(|e| e.name == name)
            .map(|e| e.value.as_str())
    }

    /// The `Cookie` header for a request to `url`, marked sensitive so it is
    /// never printed nor indexed by HTTP/2 header compression; `None` when
    /// `url` is not an `https` Slack address or nothing goes there.
    pub fn header_for(&self, url: &Url) -> Option<HeaderValue> {
        let host = slack_host(url)?;
        let pairs: Vec<String> = self
            .entries
            .iter()
            .filter(|e| e.matches(host))
            .map(|e| format!("{}={}", e.name, e.value))
            .collect();
        if pairs.is_empty() {
            return None;
        }
        let mut value = HeaderValue::from_str(&pairs.join("; ")).ok()?;
        value.set_sensitive(true);
        Some(value)
    }
}

/// What to do with a redirect.
#[derive(Debug, PartialEq, Eq)]
pub enum Hop {
    /// Go on to this address.
    Follow(Url),
    /// The address is not an `https` Slack one: stop at the redirect.
    Refused,
    /// [`MAX_HOPS`] redirects already.
    TooMany,
}

/// Where a redirect from `current` to `location` goes, after `hops`
/// redirects so far. Only an `https` Slack address is followed, so the
/// cookies never leave Slack, also not by way of a redirect.
pub fn next_hop(current: &Url, location: &str, hops: usize) -> Hop {
    if hops >= MAX_HOPS {
        return Hop::TooMany;
    }
    match current.join(location.trim()) {
        Ok(next) if slack_host(&next).is_some() => Hop::Follow(next),
        _ => Hop::Refused,
    }
}

/// Fetches `url`, following Slack's redirects by hand with `cookies` and
/// keeping every cookie a hop sets. `http` must not follow redirects
/// itself (see [`client`]). A redirect off Slack is not followed: its own
/// response is the answer.
pub async fn get(
    http: &reqwest::Client,
    mut url: Url,
    cookies: &mut Cookies,
) -> Result<reqwest::Response, SlackError> {
    let mut hops = 0;
    loop {
        let mut request = http.get(url.clone());
        if let Some(header) = cookies.header_for(&url) {
            request = request.header(COOKIE, header);
        }
        let response = request.send().await?;
        cookies.store(
            &url,
            response
                .headers()
                .get_all(SET_COOKIE)
                .iter()
                .filter_map(|v| v.to_str().ok()),
        );
        let redirect = matches!(response.status().as_u16(), 301 | 302 | 303 | 307 | 308);
        let location = response
            .headers()
            .get(LOCATION)
            .and_then(|v| v.to_str().ok());
        let (true, Some(location)) = (redirect, location) else {
            return Ok(response);
        };
        match next_hop(&url, location, hops) {
            Hop::Follow(next) => {
                hops += 1;
                url = next;
            }
            Hop::Refused => {
                log::debug!("not following a redirect away from Slack");
                return Ok(response);
            }
            Hop::TooMany => return Err(SlackError::Network("too many redirects".into())),
        }
    }
}

/// A client for [`get`]: the app's proxy and timeouts, and no redirects of
/// its own, since reqwest would follow them without the cookies.
pub fn client(user_agent: &str) -> Result<reqwest::Client, SlackError> {
    Ok(super::net::builder()
        .user_agent(user_agent)
        .connect_timeout(std::time::Duration::from_secs(15))
        .timeout(std::time::Duration::from_secs(60))
        .redirect(reqwest::redirect::Policy::none())
        .build()?)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn url(s: &str) -> Url {
        Url::parse(s).expect("a URL")
    }

    #[test]
    fn set_cookie_headers_are_parsed() {
        let d = parse_set_cookie(
            "d=xoxd-abc%2F; expires=Sat, 01-Jan-2050 00:00:00 GMT; Max-Age=315360000; \
             path=/; domain=.slack.com; secure; SameSite=Lax; HttpOnly",
        )
        .expect("a cookie");
        assert_eq!(d.name, "d");
        assert_eq!(d.value, "xoxd-abc%2F");
        assert_eq!(d.domain.as_deref(), Some("slack.com"));
        assert!(!d.expired);

        let quoted = parse_set_cookie("lc=\"1700000000\"; Path=/").expect("a cookie");
        assert_eq!(quoted.value, "1700000000");
        assert_eq!(quoted.domain, None);

        assert!(
            parse_set_cookie("d=; Domain=.slack.com")
                .expect("a cookie")
                .expired
        );
        assert!(
            parse_set_cookie("d=x; Max-Age=0")
                .expect("a cookie")
                .expired
        );
        assert!(
            parse_set_cookie("d=deleted; expires=Thu, 01-Jan-1970 00:00:01 GMT")
                .expect("a cookie")
                .expired
        );
        assert_eq!(parse_set_cookie("no equals sign"), None);
        assert_eq!(parse_set_cookie("=value"), None);
        assert_eq!(parse_set_cookie(""), None);
    }

    #[test]
    fn the_d_cookie_is_read_from_several_set_cookie_headers() {
        let mut cookies = Cookies::default();
        let api = url("https://acme.slack.com/api/auth.loginMagicBulk");
        cookies.store(
            &api,
            [
                "b=abc123; Domain=.slack.com; Path=/",
                "d=\"xoxd-new%2F\"; Domain=.slack.com; Path=/; Secure; HttpOnly",
                "d-s=1700000000; Domain=.slack.com",
                "lc=1700000000; Path=/",
            ],
        );
        assert_eq!(cookies.get("d"), Some("xoxd-new%2F"));
        assert_eq!(cookies.get("lc"), Some("1700000000"));
        assert_eq!(cookies.get("x"), None);
        assert_eq!(Cookies::default().get("d"), None);
        // An emptied `d` is no cookie at all.
        cookies.store(&api, ["d=; Domain=.slack.com"]);
        assert_eq!(cookies.get("d"), None);
    }

    #[test]
    fn cookies_for_other_domains_are_ignored() {
        let mut cookies = Cookies::default();
        cookies.store(
            &url("https://acme.slack.com/"),
            [
                "a=1; Domain=evil.example",
                "b=2; Domain=.evilslack.com",
                "c=3; Domain=beta.slack.com",
                "e=4; Domain=com",
            ],
        );
        assert_eq!(format!("{cookies:?}"), "[]");
        // Nothing is kept from a response off Slack, or over plain http.
        cookies.store(&url("https://evil.example/"), ["f=5"]);
        cookies.store(&url("http://acme.slack.com/"), ["g=6"]);
        assert_eq!(format!("{cookies:?}"), "[]");
    }

    #[test]
    fn the_cookie_header_carries_what_goes_to_the_host() {
        let mut cookies = Cookies::with_d("xoxd-abc");
        let acme = url("https://acme.slack.com/ssb/redirect");
        cookies.store(&acme, ["lc=1", "x=2; Domain=acme.slack.com"]);
        let header = |cookies: &Cookies, u: &str| {
            cookies
                .header_for(&url(u))
                .map(|v| v.to_str().expect("ascii").to_owned())
        };
        assert_eq!(
            header(&cookies, "https://acme.slack.com/").as_deref(),
            Some("d=xoxd-abc; lc=1; x=2")
        );
        // `lc` was host-only; `x` was for acme's subdomains too.
        assert_eq!(
            header(&cookies, "https://app.slack.com/client").as_deref(),
            Some("d=xoxd-abc")
        );
        assert_eq!(
            header(&cookies, "https://e.acme.slack.com/").as_deref(),
            Some("d=xoxd-abc; x=2")
        );
        // A newer `d` replaces the seeded one.
        cookies.store(&acme, ["d=xoxd-new; Domain=.slack.com"]);
        assert_eq!(
            header(&cookies, "https://slack.com/").as_deref(),
            Some("d=xoxd-new")
        );
    }

    #[test]
    fn the_cookie_never_leaves_slack() {
        let cookies = Cookies::with_d("xoxd-abc");
        for elsewhere in [
            "https://evil.example/",
            "https://acme.slack.com.evil.example/",
            "https://evilslack.com/",
            "http://acme.slack.com/",
            "https://acme.slack.com:8443/",
            "https://user@acme.slack.com/",
        ] {
            assert_eq!(cookies.header_for(&url(elsewhere)), None, "{elsewhere}");
        }
    }

    #[test]
    fn the_cookie_header_is_sensitive() {
        let header = Cookies::with_d("xoxd-abc")
            .header_for(&url("https://acme.slack.com/"))
            .expect("a header");
        assert!(header.is_sensitive());
        assert!(!format!("{header:?}").contains("xoxd"));
    }

    #[test]
    fn cookies_never_print_their_values() {
        let mut cookies = Cookies::with_d("xoxd-secret");
        cookies.store(&url("https://slack.com/"), ["b=secret-too"]);
        let printed = format!("{cookies:?}");
        assert!(!printed.contains("secret"), "{printed}");
        let printed = format!("{:?}", parse_set_cookie("d=xoxd-secret"));
        assert!(!printed.contains("secret"), "{printed}");
    }

    #[test]
    fn redirects_are_followed_only_on_slack() {
        let here = url("https://acme.slack.com/");
        assert_eq!(
            next_hop(&here, "/ssb/redirect", 0),
            Hop::Follow(url("https://acme.slack.com/ssb/redirect"))
        );
        assert_eq!(
            next_hop(&here, "https://acme.slack.com/client/T1", 3),
            Hop::Follow(url("https://acme.slack.com/client/T1"))
        );
        assert_eq!(
            next_hop(&here, "https://app.slack.com/client/T1", 0),
            Hop::Follow(url("https://app.slack.com/client/T1"))
        );
        assert_eq!(
            next_hop(&url("https://acme.slack.com/a/b"), "c?x=1", 0),
            Hop::Follow(url("https://acme.slack.com/a/c?x=1"))
        );
        for away in [
            "https://evil.example/",
            "//evil.example/path",
            "https://acme.slack.com.evil.example/",
            "http://acme.slack.com/",
            "https://acme.slack.com:8443/",
            "https://user:pw@acme.slack.com/",
            "slack://T1/magic-login/x",
        ] {
            assert_eq!(next_hop(&here, away, 0), Hop::Refused, "{away}");
        }
        assert_eq!(
            next_hop(&here, "/again", MAX_HOPS - 1),
            Hop::Follow(url("https://acme.slack.com/again"))
        );
        assert_eq!(next_hop(&here, "/again", MAX_HOPS), Hop::TooMany);
    }
}
