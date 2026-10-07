//! `noslacking --teams-probe`: tries signing in to a personal Microsoft
//! account (Teams free) step by step, for a spike against a real account.
//!
//! Nothing here is used by the app yet. Microsoft documents none of this,
//! and the clients that do it disagree (see
//! `docs/research/microsoft-teams.md` §6.1), so the probe tries each
//! variant in turn and logs which worked:
//!
//! 1. a device code from the consumer client id, for each tenant and scope
//!    in `SIGN_INS` until one is accepted;
//! 2. waiting for you to enter it, then what kind of token came back (a
//!    JWT and its non-secret claims, or an opaque one);
//! 3. trading it, and the refresh token for the other scopes, at the
//!    consumer and work `authz` services, with and without the consumer
//!    headers;
//! 4. with the first skype token: who it says you are, the chat list at
//!    each candidate host, a page of the first chat, and Trouter.
//!
//! It saves nothing and logs no token, only their shapes; the last line
//! names the furthest step reached. Every step logs at info level.

use std::time::Duration;

use serde_json::Value;

use super::auth::{AUTHZ_URL_PERSONAL, AUTHZ_URL_WORK, TEAMS_CONSUMER_CLIENT_ID, parse_jwt_claims};

/// The scope ost asks for, the same as for work accounts.
const SCOPE_SPACES: &str = "https://api.spaces.skype.com/.default offline_access";
/// The scope purple-teams asks for in its personal build.
const SCOPE_MBI: &str = "service::api.fl.spaces.skype.com::MBI_SSL openid profile offline_access";

/// Tenant and scope pairs to ask a device code with, in order.
const SIGN_INS: [(&str, &str); 4] = [
    ("consumers", SCOPE_SPACES),
    ("consumers", SCOPE_MBI),
    ("common", SCOPE_SPACES),
    ("common", SCOPE_MBI),
];

/// The furthest the probe got, by the name the last line gives it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Reached {
    Nothing,
    DeviceCode,
    SignedIn,
    SkypeToken,
    ChatList,
    Messages,
    Trouter,
}

/// Runs the probe; answers the exit code: 0 once chats were listed.
pub fn run(settings: &std::path::Path) -> i32 {
    log::info!(
        "teams probe: NoSlacking {} on {}/{}: personal account sign-in",
        env!("CARGO_PKG_VERSION"),
        std::env::consts::OS,
        std::env::consts::ARCH,
    );
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            log::error!("teams probe: FAILED to start: {error}");
            return 1;
        }
    };
    let reached = runtime.block_on(probe(settings));
    let code = if reached >= Reached::ChatList { 0 } else { 1 };
    let line = format!("teams probe: reached {reached:?}");
    if code == 0 {
        log::info!("{line}");
    } else {
        log::error!("{line}");
    }
    log::logger().flush();
    code
}

async fn probe(settings: &std::path::Path) -> Reached {
    let settings = crate::settings::Settings::load(settings);
    if let Err(error) = crate::slack::net::configure(&settings.proxy) {
        log::warn!("teams probe: the proxy setting does not work ({error:?}); going without");
    }
    let http = crate::slack::net::api();

    // 1. A device code.
    let mut device = None;
    for (tenant, scope) in SIGN_INS {
        match device_code(&http, tenant, scope).await {
            Ok(code) => {
                log::info!("device code: accepted for tenant {tenant}, scope {scope}");
                device = Some((tenant, scope, code));
                break;
            }
            Err(why) => {
                log::warn!("device code: refused for tenant {tenant}, scope {scope}: {why}")
            }
        }
    }
    let Some((tenant, scope, device)) = device else {
        return Reached::Nothing;
    };
    show_code(&device);

    // 2. Signing in.
    let token = match wait_for_token(&http, tenant, &device).await {
        Ok(token) => token,
        Err(why) => {
            log::error!("sign-in: {why}");
            return Reached::DeviceCode;
        }
    };
    describe_token("access token", &token.access);
    log::info!(
        "sign-in: refresh token {}",
        if token.refresh.is_some() {
            "given"
        } else {
            "MISSING"
        }
    );

    // 3. Skype tokens, for every scope we can get an access token for.
    let mut access = vec![(scope, token.access.clone())];
    if let Some(refresh) = &token.refresh {
        for other in [SCOPE_SPACES, SCOPE_MBI]
            .into_iter()
            .filter(|s| *s != scope)
        {
            match redeem(&http, tenant, refresh, other).await {
                Ok(minted) => {
                    describe_token(&format!("access token for {other}"), &minted);
                    access.push((other, minted));
                }
                Err(why) => log::warn!("refresh for scope {other}: {why}"),
            }
        }
    }
    let mut skype = None;
    for (scope, token) in &access {
        for (url, consumer_headers) in [
            (AUTHZ_URL_PERSONAL, true),
            (AUTHZ_URL_PERSONAL, false),
            (AUTHZ_URL_WORK, false),
        ] {
            match authz(&http, url, token, consumer_headers).await {
                Ok(found) => {
                    log::info!(
                        "authz: OK at {url} with the {scope} token{}",
                        if consumer_headers {
                            " and consumer headers"
                        } else {
                            ""
                        }
                    );
                    if skype.is_none() {
                        skype = Some(found);
                    }
                }
                Err(why) => log::warn!(
                    "authz: refused at {url} with the {scope} token{}: {why}",
                    if consumer_headers {
                        " and consumer headers"
                    } else {
                        ""
                    }
                ),
            }
        }
    }
    let Some(skype) = skype else {
        return Reached::SignedIn;
    };
    describe_token("skype token", &skype.token);
    let gtms_keys: Vec<&str> = skype
        .region_gtms
        .as_object()
        .map_or(Vec::new(), |gtms| gtms.keys().map(String::as_str).collect());
    log::info!("authz: regionGtms keys: {}", gtms_keys.join(", "));
    for key in [
        "chatService",
        "chatServiceAggregator",
        "middleTier",
        "unifiedPresence",
    ] {
        if let Some(value) = skype.region_gtms.get(key).and_then(Value::as_str) {
            log::info!("authz: regionGtms.{key} = {value}");
        }
    }

    // 4. Chats.
    let mut hosts: Vec<String> = Vec::new();
    if let Some(chat) = skype.region_gtms.get("chatService").and_then(Value::as_str) {
        hosts.push(chat.trim_end_matches('/').to_owned());
    }
    hosts.push("https://teams.live.com/api/chatsvc/consumer".to_owned());
    hosts.push("https://msgapi.teams.live.com".to_owned());
    let mut reached = Reached::SkypeToken;
    let mut first_chat = None;
    let mut first_group: Option<(String, Value)> = None;
    for host in &hosts {
        match chats(&http, host, &skype.token).await {
            Ok(list) => {
                log::info!("chat list: OK at {host}: {} conversations", list.len());
                let id_of =
                    |c: &Value| c.get("id").and_then(Value::as_str).unwrap_or("").to_owned();
                for chat in list.iter().take(5) {
                    log::info!(
                        "chat list: a conversation of kind {}",
                        id_kind(&id_of(chat))
                    );
                }
                reached = Reached::ChatList;
                if first_chat.is_none() {
                    first_chat = list.first().map(|c| (host.clone(), id_of(c)));
                }
                if first_group.is_none() {
                    first_group = list
                        .iter()
                        .find(|c| {
                            let id = id_of(c);
                            id.ends_with("@thread.v2") && !id.starts_with("19:uni01_")
                        })
                        .map(|c| (host.clone(), c.clone()));
                }
            }
            Err(why) => log::warn!("chat list: refused at {host}: {why}"),
        }
    }
    if let Some((host, chat)) = first_chat {
        match messages(&http, &host, &chat, &skype.token).await {
            Ok(page) => {
                log::info!("messages: OK, {} in the first page", page.len());
                for (kind, from) in page.iter().take(8) {
                    log::info!("messages: type {kind}, from {}", mri_kind(from));
                }
                reached = Reached::Messages;
            }
            Err(why) => log::warn!("messages: refused at {host}: {why}"),
        }
    }

    // 4b. Names for a group chat without a topic.
    if let Some((host, group)) = &first_group {
        names(&http, host, group, &skype, &access).await;
    }

    // 5. Live updates.
    let epid = crate::model::new_client_msg_id();
    match super::socket::negotiate_trouter(&http, &skype.token, &epid).await {
        Ok(session) => {
            log::info!(
                "trouter: session negotiated (registrar given: {})",
                session.registrar_url.is_some()
            );
            if reached >= Reached::Messages {
                reached = Reached::Trouter;
            }
        }
        Err(error) => log::warn!("trouter: negotiation refused: {error:?}"),
    }
    reached
}

/// A device code to show.
struct DeviceCode {
    device_code: String,
    user_code: String,
    verification_uri: String,
    interval: u64,
    expires_in: u64,
}

async fn device_code(
    http: &reqwest::Client,
    tenant: &str,
    scope: &str,
) -> Result<DeviceCode, String> {
    let body = post_form(
        http,
        &super::auth::device_code_url(tenant),
        &[("client_id", TEAMS_CONSUMER_CLIENT_ID), ("scope", scope)],
    )
    .await?;
    let text = |key: &str| body.get(key).and_then(Value::as_str).map(str::to_owned);
    Ok(DeviceCode {
        device_code: text("device_code").ok_or("no device_code")?,
        user_code: text("user_code").ok_or("no user_code")?,
        verification_uri: text("verification_uri").ok_or("no verification_uri")?,
        interval: body
            .get("interval")
            .and_then(Value::as_u64)
            .unwrap_or(5)
            .max(1),
        expires_in: body
            .get("expires_in")
            .and_then(Value::as_u64)
            .unwrap_or(900),
    })
}

/// Shows the code where whoever runs the probe sees it, not only in the
/// log.
#[expect(clippy::print_stderr, reason = "the probe runs in a terminal")]
fn show_code(device: &DeviceCode) {
    eprintln!(
        "\nSign in with your PERSONAL Microsoft account: open {} and enter {}\n",
        device.verification_uri, device.user_code
    );
    log::info!(
        "device code: waiting for the sign-in at {}",
        device.verification_uri
    );
    if let Err(error) = open::that_detached(&device.verification_uri) {
        log::info!("device code: could not open the browser: {error}");
    }
}

/// What signing in gave.
struct Tokens {
    access: String,
    refresh: Option<String>,
}

async fn wait_for_token(
    http: &reqwest::Client,
    tenant: &str,
    device: &DeviceCode,
) -> Result<Tokens, String> {
    let started = std::time::Instant::now();
    let mut interval = device.interval;
    while started.elapsed() < Duration::from_secs(device.expires_in) {
        tokio::time::sleep(Duration::from_secs(interval)).await;
        let answer = post_form(
            http,
            &super::auth::token_url(tenant),
            &[
                ("client_id", TEAMS_CONSUMER_CLIENT_ID),
                ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
                ("device_code", &device.device_code),
            ],
        )
        .await;
        match answer {
            Ok(body) => return tokens(&body),
            Err(why) if why.contains("authorization_pending") => {}
            Err(why) if why.contains("slow_down") => interval += 5,
            Err(why) => return Err(why),
        }
    }
    Err("the code expired before it was entered".to_owned())
}

async fn redeem(
    http: &reqwest::Client,
    tenant: &str,
    refresh: &str,
    scope: &str,
) -> Result<String, String> {
    let body = post_form(
        http,
        &super::auth::token_url(tenant),
        &[
            ("client_id", TEAMS_CONSUMER_CLIENT_ID),
            ("grant_type", "refresh_token"),
            ("refresh_token", refresh),
            ("scope", scope),
        ],
    )
    .await?;
    tokens(&body).map(|t| t.access)
}

fn tokens(body: &Value) -> Result<Tokens, String> {
    let access = body
        .get("access_token")
        .and_then(Value::as_str)
        .ok_or("no access_token in the answer")?
        .to_owned();
    if let Some(scope) = body.get("scope").and_then(Value::as_str) {
        log::info!("token: granted scope {scope}");
    }
    Ok(Tokens {
        access,
        refresh: body
            .get("refresh_token")
            .and_then(Value::as_str)
            .map(str::to_owned),
    })
}

/// POSTs a form and answers its JSON, or why not: the status and the
/// OAuth error code, never the body, which can echo what was sent.
async fn post_form(
    http: &reqwest::Client,
    url: &str,
    form: &[(&str, &str)],
) -> Result<Value, String> {
    let resp = http
        .post(url)
        .form(form)
        .send()
        .await
        .map_err(|e| format!("network: {}", e.without_url()))?;
    let status = resp.status();
    let text = resp
        .text()
        .await
        .map_err(|e| format!("reading: {}", e.without_url()))?;
    if !status.is_success() {
        let codes = aad_codes(&text);
        return Err(format!(
            "HTTP {status} {} {codes}",
            super::auth::error_code(&text)
        ));
    }
    serde_json::from_str(&text).map_err(|e| format!("not JSON: {e}"))
}

/// Azure AD's numeric error codes (`AADSTS…`), which say why without
/// saying anything secret.
fn aad_codes(body: &str) -> String {
    serde_json::from_str::<Value>(body)
        .ok()
        .and_then(|v| v.get("error_codes").cloned())
        .map_or_else(String::new, |codes| format!("(AADSTS {codes})"))
}

/// What the skype token exchange gave.
struct Skype {
    token: String,
    region_gtms: Value,
}

async fn authz(
    http: &reqwest::Client,
    url: &str,
    access: &str,
    consumer_headers: bool,
) -> Result<Skype, String> {
    let mut request = http
        .post(url)
        .bearer_auth(access)
        .header("Content-Length", "0")
        .header("Accept", "application/json; ver=1.0");
    if consumer_headers {
        request = request
            .header("X-MS-Client-Consumer-Type", "teams4life")
            .header("ms-ic3-product", "tfl");
    }
    let resp = request
        .send()
        .await
        .map_err(|e| format!("network: {}", e.without_url()))?;
    let status = resp.status();
    let text = resp
        .text()
        .await
        .map_err(|e| format!("reading: {}", e.without_url()))?;
    if !status.is_success() {
        return Err(format!("HTTP {status} {}", super::auth::error_code(&text)));
    }
    let body: Value = serde_json::from_str(&text).map_err(|e| format!("not JSON: {e}"))?;
    let Some((path, token)) = SKYPE_TOKEN_PATHS.iter().find_map(|path| {
        body.pointer(path)
            .and_then(Value::as_str)
            .filter(|t| !t.is_empty())
            .map(|t| (*path, t.to_owned()))
    }) else {
        return Err(format!(
            "HTTP {status} but no skype token where expected; the answer's shape: {}",
            shape(&body)
        ));
    };
    log::info!("authz: skype token found at {path}");
    for path in ["/skypeToken/expiresIn", "/tokens/expiresIn", "/expiresIn"] {
        if let Some(expires) = body.pointer(path) {
            log::info!("authz: {path} = {expires}");
        }
    }
    Ok(Skype {
        token,
        region_gtms: body.get("regionGtms").cloned().unwrap_or(Value::Null),
    })
}

/// Where the skype token sits in an `authz` answer: the work service's
/// shape, then the shapes the personal service is said to use.
const SKYPE_TOKEN_PATHS: [&str; 4] = [
    "/tokens/skypeToken",
    "/skypeToken/skypetoken",
    "/skypeToken/skypeToken",
    "/skypeToken",
];

/// The keys of a JSON value and what kind each holds, nested, without any
/// value: enough to see where a token sits, never the token.
fn shape(value: &Value) -> String {
    match value {
        Value::Object(map) => {
            let fields: Vec<String> = map
                .iter()
                .map(|(key, value)| format!("{key}: {}", shape(value)))
                .collect();
            format!("{{{}}}", fields.join(", "))
        }
        Value::Array(items) => format!(
            "[{} × {}]",
            items.len(),
            items.first().map_or("-".to_owned(), shape)
        ),
        Value::String(s) => format!("string({})", s.len()),
        Value::Number(_) => "number".to_owned(),
        Value::Bool(_) => "bool".to_owned(),
        Value::Null => "null".to_owned(),
    }
}

async fn chats(http: &reqwest::Client, host: &str, skype: &str) -> Result<Vec<Value>, String> {
    let url = format!("{host}/v1/users/ME/conversations?view=mychats&pageSize=20");
    let body = get_skype(http, &url, skype).await?;
    Ok(body
        .get("conversations")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default())
}

/// Where a group chat's names could come from: the chat list's own entry,
/// the thread's record, and the people service with each way of signing
/// the request. Logs shapes and counts, never a name.
async fn names(
    http: &reqwest::Client,
    host: &str,
    group: &Value,
    skype: &Skype,
    access: &[(&str, String)],
) {
    log::info!("names: a group chat's list entry: {}", shape(group));
    let id = group.get("id").and_then(Value::as_str).unwrap_or("");
    let id = percent_encoding::utf8_percent_encode(id, percent_encoding::NON_ALPHANUMERIC);
    let url = format!("{host}/v1/threads/{id}?view=msnp24Equivalent");
    let members: Vec<String> = match get_skype(http, &url, &skype.token).await {
        Ok(thread) => {
            log::info!("names: the thread record: {}", shape(&thread));
            thread
                .get("members")
                .and_then(Value::as_array)
                .map(|list| {
                    list.iter()
                        .filter_map(|m| m.get("id").and_then(Value::as_str).map(str::to_owned))
                        .collect()
                })
                .unwrap_or_default()
        }
        Err(why) => {
            log::warn!("names: the thread record was refused: {why}");
            Vec::new()
        }
    };
    log::info!(
        "names: {} members, of kinds {}",
        members.len(),
        members
            .iter()
            .map(|m| mri_kind(m))
            .collect::<Vec<_>>()
            .join(", ")
    );
    if members.is_empty() {
        return;
    }
    let Some(middle) = skype.region_gtms.get("middleTier").and_then(Value::as_str) else {
        log::warn!("names: no middle tier in regionGtms");
        return;
    };
    let url = format!(
        "{}/beta/users/fetchShortProfile?isMailAddress=false&enableGuest=true&includeIBBarredUsers=true&skypeTeamsInfo=true",
        middle.trim_end_matches('/')
    );
    let access = access.first().map(|(_, t)| t.as_str()).unwrap_or("");
    for (how, bearer, skype_headers, consumer) in [
        ("access token", Some(access), false, false),
        ("access token, consumer headers", Some(access), false, true),
        (
            "access and skype tokens, consumer headers",
            Some(access),
            true,
            true,
        ),
        ("skype token only, consumer headers", None, true, true),
        (
            "skype token as bearer, consumer headers",
            Some(skype.token.as_str()),
            false,
            true,
        ),
    ] {
        let mut request = http.post(&url).json(&members);
        if let Some(bearer) = bearer {
            request = request.bearer_auth(bearer);
        }
        if skype_headers {
            request = request
                .header("X-Skypetoken", &skype.token)
                .header("Authentication", format!("skypetoken={}", skype.token));
        }
        if consumer {
            request = super::auth::consumer_headers(request);
        }
        match request.send().await {
            Ok(resp) => {
                let status = resp.status();
                let text = resp.text().await.unwrap_or_default();
                if status.is_success() {
                    let body: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
                    let named = body
                        .get("value")
                        .and_then(Value::as_array)
                        .map_or(0, |list| {
                            list.iter()
                                .filter(|p| {
                                    p.get("displayName")
                                        .and_then(Value::as_str)
                                        .is_some_and(|n| !n.trim().is_empty())
                                })
                                .count()
                        });
                    log::info!(
                        "names: fetchShortProfile OK with {how}: {named} of {} named; {}",
                        members.len(),
                        shape(&body)
                    );
                } else {
                    log::warn!(
                        "names: fetchShortProfile refused with {how}: HTTP {status} {}",
                        super::auth::error_code(&text)
                    );
                }
            }
            Err(error) => log::warn!(
                "names: fetchShortProfile with {how}: {}",
                error.without_url()
            ),
        }
    }
}

async fn messages(
    http: &reqwest::Client,
    host: &str,
    chat: &str,
    skype: &str,
) -> Result<Vec<(String, String)>, String> {
    let chat = percent_encoding::utf8_percent_encode(chat, percent_encoding::NON_ALPHANUMERIC);
    let url = format!("{host}/v1/users/ME/conversations/{chat}/messages?pageSize=20");
    let body = get_skype(http, &url, skype).await?;
    let text = |m: &Value, key: &str| m.get(key).and_then(Value::as_str).unwrap_or("").to_owned();
    Ok(body
        .get("messages")
        .and_then(Value::as_array)
        .map(|list| {
            list.iter()
                .map(|m| (text(m, "messagetype"), text(m, "from")))
                .collect()
        })
        .unwrap_or_default())
}

async fn get_skype(http: &reqwest::Client, url: &str, skype: &str) -> Result<Value, String> {
    let resp = http
        .get(url)
        .header("Authentication", format!("skypetoken={skype}"))
        .header("X-Skypetoken", skype)
        .header("X-MS-Client-Consumer-Type", "teams4life")
        .send()
        .await
        .map_err(|e| format!("network: {}", e.without_url()))?;
    let status = resp.status();
    let text = resp
        .text()
        .await
        .map_err(|e| format!("reading: {}", e.without_url()))?;
    if !status.is_success() {
        return Err(format!("HTTP {status} {}", super::auth::error_code(&text)));
    }
    serde_json::from_str(&text).map_err(|e| format!("not JSON: {e}"))
}

/// Logs what kind of token `token` is, and for a JWT the claims that
/// matter here, which say who and what for but are not themselves secret.
fn describe_token(what: &str, token: &str) {
    match parse_jwt_claims(token) {
        Some(claims) => {
            let claim = |key: &str| match claims.get(key) {
                Some(Value::String(s)) => s.clone(),
                Some(other) => other.to_string(),
                None => "-".to_owned(),
            };
            log::info!(
                "{what}: a JWT; aud {}, tid {}, scp {}, oid {}, skypeid {}, has puid {}",
                claim("aud"),
                claim("tid"),
                claim("scp"),
                if claims.get("oid").is_some() {
                    "present"
                } else {
                    "-"
                },
                mri_kind(&claim("skypeid")),
                claims.get("puid").is_some(),
            );
        }
        None => log::info!(
            "{what}: opaque, {} characters, starting {}",
            token.len(),
            token.chars().take(3).collect::<String>()
        ),
    }
}

/// The kind of a conversation id, without the id.
fn id_kind(id: &str) -> &'static str {
    if id.starts_with("19:uni01_") {
        "19:uni01_… (one-to-one)"
    } else if id.starts_with("48:") {
        "48: (notes, notifications)"
    } else if id.ends_with("@unq.gbl.spaces") {
        "19:…@unq.gbl.spaces (one-to-one)"
    } else if id.contains("@thread.v2") {
        "19:…@thread.v2 (group)"
    } else if id.contains("@thread.skype") {
        "19:…@thread.skype (group, old)"
    } else if id.starts_with("8:") {
        "8: (a person)"
    } else {
        "other"
    }
}

/// The kind of a user id (`8:live:…`, `8:orgid:…`), without the id.
/// Anything else, such as a conversation sending its own system
/// messages, is named by its kind only too.
fn mri_kind(mri: &str) -> String {
    let id = mri.rsplit('/').next().unwrap_or(mri);
    if id == "-" || id.is_empty() {
        return "-".to_owned();
    }
    if id.starts_with("19:") {
        return id_kind(id).to_owned();
    }
    let mut parts = id.splitn(3, ':');
    match (parts.next(), parts.next()) {
        (Some(first), Some(second))
            if first.chars().all(|c| c.is_ascii_digit())
                && second.chars().all(|c| c.is_ascii_alphabetic()) =>
        {
            format!("{first}:{second}:…")
        }
        (Some(first), Some(_)) if first.chars().all(|c| c.is_ascii_alphabetic()) => {
            format!("{first}:…")
        }
        _ => "other".to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kinds_leave_the_ids_out() {
        assert_eq!(mri_kind("8:live:.cid.123abc"), "8:live:…");
        assert_eq!(
            mri_kind("https://x/v1/users/ME/contacts/8:orgid:abc"),
            "8:orgid:…"
        );
        assert_eq!(mri_kind("live:.cid.123"), "live:…");
        assert_eq!(mri_kind("-"), "-");
        assert_eq!(
            mri_kind("https://x/v1/users/ME/contacts/19:uni01_abc@thread.v2"),
            "19:uni01_… (one-to-one)"
        );
        assert_eq!(
            id_kind("19:a_b@unq.gbl.spaces"),
            "19:…@unq.gbl.spaces (one-to-one)"
        );
        assert_eq!(id_kind("48:notes"), "48: (notes, notifications)");
    }

    #[test]
    fn shapes_name_keys_but_never_values() {
        let answer: Value = serde_json::from_str(
            r#"{"skypeToken":{"skypetoken":"eyJsecret","expiresIn":86399},"regionGtms":{"chatService":"https://x"},"list":[1,2]}"#,
        )
        .expect("JSON");
        let shape = shape(&answer);
        assert_eq!(
            shape,
            "{list: [2 × number], regionGtms: {chatService: string(9)}, skypeToken: {expiresIn: number, skypetoken: string(9)}}"
        );
        assert!(!shape.contains("secret"));
        let found = SKYPE_TOKEN_PATHS
            .iter()
            .find_map(|p| answer.pointer(p).and_then(Value::as_str));
        assert_eq!(found, Some("eyJsecret"));
    }

    #[test]
    fn refusals_carry_codes_not_bodies() {
        let body = r#"{"error":"invalid_scope","error_codes":[70011],"error_description":"secret eyJx.y.z"}"#;
        assert_eq!(aad_codes(body), "(AADSTS [70011])");
        assert_eq!(super::super::auth::error_code(body), "invalid_scope");
    }
}
