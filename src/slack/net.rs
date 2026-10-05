//! The HTTP clients every Slack call goes through, and the proxy they use.
//!
//! The proxy is one choice for the whole app (Settings → Network):
//!
//! - **System**, the default: the `HTTPS_PROXY`, `HTTP_PROXY`, `ALL_PROXY`
//!   and `NO_PROXY` variables, then the operating system's own proxy
//!   settings on macOS and Windows.
//! - **Direct**: no proxy at all, whatever the environment says.
//! - **Manual**: one `http://`, `socks5://` or `socks5h://` proxy URL.
//!
//! A reqwest client fixes its proxy when it is built, and keeps its pooled
//! connections, so [`configure`] builds fresh clients and everything takes
//! them from here ([`api`], [`transfers`], [`builder`]) at the moment it
//! needs one. The worker then restarts its sockets, which also go through
//! the proxy: [`websocket`] opens them through an HTTP `CONNECT` tunnel or
//! a SOCKS5 proxy, as the setting says.

use std::sync::{Arc, RwLock};
use std::time::Duration;

use tokio::io::{AsyncRead, AsyncReadExt as _, AsyncWrite, AsyncWriteExt as _};
use tokio_tungstenite::tungstenite::client::IntoClientRequest as _;
use tokio_tungstenite::tungstenite::http::{HeaderValue, Uri};
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};

/// Which proxy NoSlacking uses.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProxyMode {
    /// The environment's and the system's proxy settings.
    #[default]
    System,
    /// None, whatever the environment says.
    Direct,
    /// The URL in [`ProxySettings::url`].
    Manual,
}

/// The proxy setting as the settings file keeps it. The URL stays when
/// another mode is picked, so switching back does not lose it.
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ProxySettings {
    /// Which proxy applies.
    #[serde(default)]
    pub mode: ProxyMode,
    /// The manual proxy, such as `http://proxy.example:3128` or
    /// `socks5h://127.0.0.1:1080`.
    #[serde(default)]
    pub url: String,
}

/// A checked proxy setting.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Route {
    /// As [`ProxyMode::System`].
    System,
    /// As [`ProxyMode::Direct`].
    Direct,
    /// A manual proxy whose URL checked out.
    Manual(reqwest::Url),
}

impl ProxySettings {
    /// The setting checked, or why the manual URL cannot be used.
    pub fn route(&self) -> Result<Route, ProxyError> {
        Ok(match self.mode {
            ProxyMode::System => Route::System,
            ProxyMode::Direct => Route::Direct,
            ProxyMode::Manual => Route::Manual(parse_manual(&self.url)?),
        })
    }
}

/// Why a manual proxy URL was turned down.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ProxyError {
    /// Manual, but nothing typed.
    #[error("enter a proxy URL")]
    Empty,
    /// Not a URL, or one without a host and port.
    #[error("the proxy URL cannot be read")]
    Invalid,
    /// A scheme no proxy client here speaks (https:// proxies included).
    #[error("the proxy must start with http://, socks5:// or socks5h://")]
    Scheme,
    /// A password would have to live in the settings file, and secrets
    /// live only in the keyring.
    #[error("a proxy password cannot be saved in the settings")]
    Password,
}

/// Checks a manual proxy URL. A bare `host:port` means an HTTP proxy.
pub fn parse_manual(text: &str) -> Result<reqwest::Url, ProxyError> {
    let text = text.trim();
    if text.is_empty() {
        return Err(ProxyError::Empty);
    }
    let text = if text.contains("://") {
        text.to_owned()
    } else {
        format!("http://{text}")
    };
    let url = reqwest::Url::parse(&text).map_err(|_| ProxyError::Invalid)?;
    if !matches!(url.scheme(), "http" | "socks5" | "socks5h") {
        return Err(ProxyError::Scheme);
    }
    if url.password().is_some() {
        return Err(ProxyError::Password);
    }
    if url.host_str().is_none_or(str::is_empty) || url.port_or_known_default().is_none() {
        return Err(ProxyError::Invalid);
    }
    Ok(url)
}

/// The clients built for the current route.
struct Net {
    route: Route,
    api: reqwest::Client,
    transfers: reqwest::Client,
}

static NET: RwLock<Option<Arc<Net>>> = RwLock::new(None);

/// How long a transfer may go without a single byte moving.
const TRANSFER_STALL: Duration = Duration::from_secs(60);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
/// How long opening a socket may take in all: the connection, the proxy's
/// tunnel, TLS and the WebSocket upgrade. Without a bound, a stalled network
/// leaves the socket waiting forever and its reconnect loop never runs.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(30);
const USER_AGENT: &str = concat!("NoSlacking/", env!("CARGO_PKG_VERSION"));

impl Net {
    fn build(route: Route) -> Self {
        let api = finish(
            apply(reqwest::Client::builder(), &route)
                .user_agent(USER_AGENT)
                .connect_timeout(CONNECT_TIMEOUT)
                .timeout(Duration::from_secs(120)),
        );
        // No total deadline: a large file on a slow line can take longer
        // than any fixed one. It fails instead when nothing has moved for
        // a while.
        let transfers = finish(
            apply(reqwest::Client::builder(), &route)
                .user_agent(USER_AGENT)
                .connect_timeout(CONNECT_TIMEOUT)
                .read_timeout(TRANSFER_STALL),
        );
        Self {
            route,
            api,
            transfers,
        }
    }
}

fn finish(builder: reqwest::ClientBuilder) -> reqwest::Client {
    builder.build().unwrap_or_else(|error| {
        log::error!("HTTP client setup failed, using defaults: {error}");
        reqwest::Client::new()
    })
}

/// Points a client builder at the route's proxy.
fn apply(builder: reqwest::ClientBuilder, route: &Route) -> reqwest::ClientBuilder {
    match route {
        // reqwest reads the environment and the system by itself.
        Route::System => builder,
        Route::Direct => builder.no_proxy(),
        Route::Manual(url) => match reqwest::Proxy::all(url.as_str()) {
            Ok(proxy) => builder.proxy(proxy),
            Err(error) => {
                // parse_manual let it through, so this should not happen;
                // going direct would quietly ignore the user's choice.
                log::error!("could not use the proxy {url}: {error}");
                builder.proxy(blackhole())
            }
        },
    }
}

/// A proxy nothing listens on, so a broken manual setting fails loudly
/// instead of going around the proxy.
fn blackhole() -> reqwest::Proxy {
    reqwest::Proxy::all("http://127.0.0.1:9")
        .unwrap_or_else(|_| reqwest::Proxy::custom(|_| None::<reqwest::Url>))
}

fn current() -> Arc<Net> {
    if let Some(net) = NET.read().ok().and_then(|net| net.clone()) {
        return net;
    }
    let mut slot = NET
        .write()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    slot.get_or_insert_with(|| Arc::new(Net::build(Route::System)))
        .clone()
}

/// Uses `settings` from now on: new clients, with new connections. The
/// caller restarts whatever holds a connection open. A setting that does
/// not check out changes nothing.
pub fn configure(settings: &ProxySettings) -> Result<(), ProxyError> {
    let route = settings.route()?;
    if NET
        .read()
        .ok()
        .and_then(|net| net.as_ref().map(|net| net.route == route))
        .unwrap_or(false)
    {
        return Ok(());
    }
    match &route {
        Route::System => log::info!("network: system proxy settings"),
        Route::Direct => log::info!("network: no proxy"),
        Route::Manual(url) => log::info!("network: proxy {url}"),
    }
    let net = Arc::new(Net::build(route));
    *NET.write()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(net);
    Ok(())
}

/// The client for Web API calls: a timeout of two minutes.
pub fn api() -> reqwest::Client {
    current().api.clone()
}

/// The client for uploads and downloads, which has no total deadline.
pub fn transfers() -> reqwest::Client {
    current().transfers.clone()
}

/// A builder for a client of its own (one with a cookie jar, say) that
/// goes through the current proxy.
pub fn builder() -> reqwest::ClientBuilder {
    apply(reqwest::Client::builder(), &current().route)
}

/// The proxy for a connection to `host:port` over TLS, if any.
fn intercept(route: &Route, host: &str, port: u16) -> Option<Proxy> {
    let matcher = match route {
        Route::Direct => return None,
        Route::System => hyper_util::client::proxy::matcher::Matcher::from_system(),
        Route::Manual(url) => hyper_util::client::proxy::matcher::Matcher::builder()
            .all(url.to_string())
            .build(),
    };
    let target: Uri = format!("https://{host}:{port}/").parse().ok()?;
    let found = matcher.intercept(&target)?;
    let uri = found.uri();
    let kind = match uri.scheme_str() {
        Some("http") => Kind::Connect(found.basic_auth().cloned()),
        Some("socks5") => Kind::Socks {
            remote_dns: false,
            auth: owned(found.raw_auth()),
        },
        Some("socks5h") => Kind::Socks {
            remote_dns: true,
            auth: owned(found.raw_auth()),
        },
        other => {
            log::warn!(
                "the proxy scheme {} cannot carry the live connection; going direct",
                other.unwrap_or("(none)")
            );
            return None;
        }
    };
    let proxy_host = uri.host()?.trim_matches(['[', ']']).to_owned();
    let default_port = if matches!(kind, Kind::Connect(_)) {
        80
    } else {
        1080
    };
    Some(Proxy {
        host: proxy_host,
        port: uri.port_u16().unwrap_or(default_port),
        kind,
    })
}

fn owned(auth: Option<(&str, &str)>) -> Option<(String, String)> {
    auth.map(|(user, password)| (user.to_owned(), password.to_owned()))
}

/// A proxy for one socket.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Proxy {
    host: String,
    port: u16,
    kind: Kind,
}

#[derive(Clone, PartialEq, Eq)]
enum Kind {
    /// An HTTP proxy, asked for a `CONNECT` tunnel, with its
    /// `Proxy-Authorization` if it has one.
    Connect(Option<HeaderValue>),
    /// A SOCKS5 proxy; `remote_dns` lets it resolve the host (socks5h).
    Socks {
        remote_dns: bool,
        auth: Option<(String, String)>,
    },
}

/// Leaves out the proxy's credentials.
impl std::fmt::Debug for Kind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Connect(_) => f.write_str("Connect"),
            Self::Socks { remote_dns, .. } => f
                .debug_struct("Socks")
                .field("remote_dns", remote_dns)
                .finish_non_exhaustive(),
        }
    }
}

/// An open WebSocket, the type `connect_async` gives.
pub type Socket = WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>;

/// What opening a socket gives: the socket and Slack's handshake answer.
type Opened = Result<
    (
        Socket,
        tokio_tungstenite::tungstenite::handshake::client::Response,
    ),
    tokio_tungstenite::tungstenite::Error,
>;

/// Opens a `wss://` WebSocket through the current proxy, giving up after
/// [`HANDSHAKE_TIMEOUT`].
pub async fn websocket(
    request: tokio_tungstenite::tungstenite::handshake::client::Request,
) -> Opened {
    tokio::time::timeout(HANDSHAKE_TIMEOUT, open(request))
        .await
        .unwrap_or_else(|_| {
            Err(tokio_tungstenite::tungstenite::Error::Io(
                std::io::Error::new(std::io::ErrorKind::TimedOut, "the handshake timed out"),
            ))
        })
}

/// [`websocket`] without the overall time limit.
async fn open(request: tokio_tungstenite::tungstenite::handshake::client::Request) -> Opened {
    use tokio_tungstenite::tungstenite::Error;
    let route = current().route.clone();
    let host = request.uri().host().unwrap_or_default().to_owned();
    let port = request.uri().port_u16().unwrap_or(443);
    let Some(proxy) = intercept(&route, &host, port) else {
        return tokio_tungstenite::connect_async(request).await;
    };
    let tunnel = async {
        let mut stream = tokio::net::TcpStream::connect((proxy.host.as_str(), proxy.port)).await?;
        match &proxy.kind {
            Kind::Connect(auth) => http_connect(&mut stream, &host, port, auth.as_ref()).await?,
            Kind::Socks { remote_dns, auth } => {
                let target = if *remote_dns {
                    Target::Name(host.clone())
                } else {
                    let address = tokio::net::lookup_host((host.as_str(), port))
                        .await?
                        .next()
                        .ok_or_else(|| std::io::Error::other("the host has no address"))?;
                    Target::Address(address.ip())
                };
                socks5(&mut stream, &target, port, auth.as_ref()).await?;
            }
        }
        Ok::<_, std::io::Error>(stream)
    };
    let stream = tokio::time::timeout(CONNECT_TIMEOUT, tunnel)
        .await
        .map_err(|_| Error::Io(std::io::Error::other("the proxy did not answer")))?
        .map_err(Error::Io)?;
    tokio_tungstenite::client_async_tls_with_config(
        request.into_client_request()?,
        stream,
        None,
        None,
    )
    .await
}

/// Asks an HTTP proxy for a tunnel to `host:port` and reads its answer up
/// to the blank line, leaving the stream at the start of the tunnel.
async fn http_connect<S: AsyncRead + AsyncWrite + Unpin>(
    stream: &mut S,
    host: &str,
    port: u16,
    auth: Option<&HeaderValue>,
) -> std::io::Result<()> {
    let authority = if host.contains(':') {
        format!("[{host}]:{port}")
    } else {
        format!("{host}:{port}")
    };
    let mut request = format!("CONNECT {authority} HTTP/1.1\r\nHost: {authority}\r\n");
    if let Some(auth) = auth.and_then(|auth| auth.to_str().ok()) {
        request.push_str(&format!("Proxy-Authorization: {auth}\r\n"));
    }
    request.push_str("\r\n");
    stream.write_all(request.as_bytes()).await?;
    // One byte at a time: anything after the blank line already belongs to
    // the TLS handshake.
    let mut head = Vec::new();
    while !head.ends_with(b"\r\n\r\n") {
        if head.len() > 8192 {
            return Err(std::io::Error::other("the proxy's answer is too long"));
        }
        let mut byte = [0u8];
        if stream.read(&mut byte).await? == 0 {
            return Err(std::io::Error::other("the proxy closed the connection"));
        }
        head.push(byte[0]);
    }
    let head = String::from_utf8_lossy(&head);
    let status = head
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .unwrap_or("");
    if status == "200" {
        Ok(())
    } else {
        Err(std::io::Error::other(format!(
            "the proxy refused the tunnel (HTTP {status})"
        )))
    }
}

/// Where a SOCKS5 proxy should connect.
enum Target {
    Name(String),
    Address(std::net::IpAddr),
}

/// The SOCKS5 handshake (RFC 1928, with RFC 1929's user name and password
/// when the proxy has them) for a TCP connection to the target.
async fn socks5<S: AsyncRead + AsyncWrite + Unpin>(
    stream: &mut S,
    target: &Target,
    port: u16,
    auth: Option<&(String, String)>,
) -> std::io::Result<()> {
    let refused = |what: &str| std::io::Error::other(format!("the SOCKS proxy {what}"));
    // Methods: 0 is none, 2 is a user name and password.
    let greeting: &[u8] = if auth.is_some() {
        &[5, 2, 0, 2]
    } else {
        &[5, 1, 0]
    };
    stream.write_all(greeting).await?;
    let mut choice = [0u8; 2];
    stream.read_exact(&mut choice).await?;
    match (choice, auth) {
        ([5, 0], _) => {}
        ([5, 2], Some((user, password))) => {
            let (user, password) = (user.as_bytes(), password.as_bytes());
            let (Ok(user_len), Ok(password_len)) =
                (u8::try_from(user.len()), u8::try_from(password.len()))
            else {
                return Err(refused("credentials are too long"));
            };
            let mut message = vec![1, user_len];
            message.extend(user);
            message.push(password_len);
            message.extend(password);
            stream.write_all(&message).await?;
            let mut status = [0u8; 2];
            stream.read_exact(&mut status).await?;
            if status[1] != 0 {
                return Err(refused("turned down the user name or password"));
            }
        }
        _ => return Err(refused("wants a sign-in this proxy setting does not have")),
    }
    let mut request = vec![5, 1, 0];
    match target {
        Target::Name(name) => {
            let len =
                u8::try_from(name.len()).map_err(|_| refused("cannot take so long a name"))?;
            request.push(3);
            request.push(len);
            request.extend(name.as_bytes());
        }
        Target::Address(std::net::IpAddr::V4(ip)) => {
            request.push(1);
            request.extend(ip.octets());
        }
        Target::Address(std::net::IpAddr::V6(ip)) => {
            request.push(4);
            request.extend(ip.octets());
        }
    }
    request.extend(port.to_be_bytes());
    stream.write_all(&request).await?;
    let mut reply = [0u8; 4];
    stream.read_exact(&mut reply).await?;
    if reply[0] != 5 || reply[1] != 0 {
        return Err(refused(&format!("could not connect (code {})", reply[1])));
    }
    // The address the proxy bound, which a client does not need.
    let skip = match reply[3] {
        1 => 4,
        4 => 16,
        3 => {
            let mut len = [0u8];
            stream.read_exact(&mut len).await?;
            usize::from(len[0])
        }
        _ => return Err(refused("answered in a way this cannot read")),
    };
    let mut rest = vec![0u8; skip + 2];
    stream.read_exact(&mut rest).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manual_proxies_are_checked() {
        let ok = |text: &str| parse_manual(text).map(|url| url.to_string());
        assert_eq!(
            ok("proxy.example:3128"),
            Ok("http://proxy.example:3128/".into())
        );
        assert_eq!(
            ok(" socks5h://127.0.0.1:1080 "),
            Ok("socks5h://127.0.0.1:1080".into())
        );
        assert_eq!(
            ok("http://user@proxy.example:8080"),
            Ok("http://user@proxy.example:8080/".into())
        );
        assert_eq!(parse_manual(""), Err(ProxyError::Empty));
        assert_eq!(parse_manual("ftp://proxy.example"), Err(ProxyError::Scheme));
        assert_eq!(
            parse_manual("https://proxy.example"),
            Err(ProxyError::Scheme)
        );
        assert_eq!(
            parse_manual("http://u:p@proxy.example:1"),
            Err(ProxyError::Password)
        );
        assert_eq!(parse_manual("http://"), Err(ProxyError::Invalid));
        assert_eq!(
            parse_manual("socks5://proxy.example"),
            Err(ProxyError::Invalid)
        );
        let settings = ProxySettings {
            mode: ProxyMode::Direct,
            url: "not a url at all".into(),
        };
        // Only the manual mode reads the URL.
        assert_eq!(settings.route(), Ok(Route::Direct));
    }

    #[test]
    fn the_socket_follows_the_manual_proxy() {
        let route = Route::Manual(parse_manual("socks5h://127.0.0.1:1080").expect("valid"));
        assert_eq!(
            intercept(&route, "wss-primary.slack.com", 443),
            Some(Proxy {
                host: "127.0.0.1".into(),
                port: 1080,
                kind: Kind::Socks {
                    remote_dns: true,
                    auth: None
                },
            })
        );
        let route = Route::Manual(parse_manual("http://bob@proxy.example:3128").expect("valid"));
        let proxy = intercept(&route, "wss-primary.slack.com", 443).expect("proxied");
        assert_eq!((proxy.host.as_str(), proxy.port), ("proxy.example", 3128));
        assert!(matches!(proxy.kind, Kind::Connect(_)));
        assert_eq!(
            intercept(&Route::Direct, "wss-primary.slack.com", 443),
            None
        );
    }

    #[test]
    fn settings_round_trip() {
        let settings = ProxySettings {
            mode: ProxyMode::Manual,
            url: "socks5://127.0.0.1:1080".into(),
        };
        let json = serde_json::to_string(&settings).expect("serializes");
        assert_eq!(json, r#"{"mode":"manual","url":"socks5://127.0.0.1:1080"}"#);
        let back: ProxySettings = serde_json::from_str(&json).expect("parses");
        assert_eq!(back, settings);
        let empty: ProxySettings = serde_json::from_str("{}").expect("parses");
        assert_eq!(empty.mode, ProxyMode::System);
    }

    #[tokio::test]
    async fn an_http_proxy_opens_a_tunnel() {
        let (mut ours, mut proxy) = tokio::io::duplex(4096);
        let server = tokio::spawn(async move {
            let mut buffer = vec![0u8; 1024];
            let read = proxy.read(&mut buffer).await.expect("reads");
            proxy
                .write_all(b"HTTP/1.1 200 Connection established\r\n\r\nTLS")
                .await
                .expect("writes");
            String::from_utf8_lossy(&buffer[..read]).into_owned()
        });
        let auth = HeaderValue::from_static("Basic Ym9iOg==");
        http_connect(&mut ours, "wss.slack.com", 443, Some(&auth))
            .await
            .expect("tunnel");
        let asked = server.await.expect("server");
        assert_eq!(
            asked,
            "CONNECT wss.slack.com:443 HTTP/1.1\r\nHost: wss.slack.com:443\r\nProxy-Authorization: Basic Ym9iOg==\r\n\r\n"
        );
        // What came after the blank line is left for the TLS handshake.
        let mut rest = [0u8; 3];
        ours.read_exact(&mut rest).await.expect("rest");
        assert_eq!(&rest, b"TLS");
    }

    #[tokio::test]
    async fn an_http_proxy_can_refuse() {
        let (mut ours, mut proxy) = tokio::io::duplex(4096);
        tokio::spawn(async move {
            let mut buffer = vec![0u8; 1024];
            let _ = proxy.read(&mut buffer).await;
            let _ = proxy
                .write_all(b"HTTP/1.1 407 Proxy Authentication Required\r\n\r\n")
                .await;
        });
        let error = http_connect(&mut ours, "wss.slack.com", 443, None)
            .await
            .expect_err("refused");
        assert!(error.to_string().contains("407"), "{error}");
    }

    #[tokio::test]
    async fn a_socks_proxy_connects_by_name_with_a_password() {
        let (mut ours, mut proxy) = tokio::io::duplex(4096);
        let server = tokio::spawn(async move {
            let mut greeting = [0u8; 4];
            proxy.read_exact(&mut greeting).await.expect("greeting");
            assert_eq!(greeting, [5, 2, 0, 2]);
            proxy.write_all(&[5, 2]).await.expect("method");
            let mut login = [0u8; 9];
            proxy.read_exact(&mut login).await.expect("login");
            assert_eq!(&login, b"\x01\x03bob\x03pw!");
            proxy.write_all(&[1, 0]).await.expect("accepted");
            let mut request = vec![0u8; 5 + 9 + 2];
            proxy.read_exact(&mut request).await.expect("request");
            assert_eq!(&request[..5], &[5, 1, 0, 3, 9]);
            assert_eq!(&request[5..14], b"slack.com");
            assert_eq!(&request[14..], &443u16.to_be_bytes());
            proxy
                .write_all(&[5, 0, 0, 1, 10, 0, 0, 1, 0x1F, 0x90])
                .await
                .expect("reply");
        });
        let auth = ("bob".to_owned(), "pw!".to_owned());
        socks5(
            &mut ours,
            &Target::Name("slack.com".into()),
            443,
            Some(&auth),
        )
        .await
        .expect("connected");
        server.await.expect("server");
    }

    #[tokio::test]
    async fn a_socks_proxy_can_refuse() {
        let (mut ours, mut proxy) = tokio::io::duplex(4096);
        tokio::spawn(async move {
            let mut greeting = [0u8; 3];
            let _ = proxy.read_exact(&mut greeting).await;
            let _ = proxy.write_all(&[5, 0]).await;
            let mut request = [0u8; 10];
            let _ = proxy.read_exact(&mut request).await;
            let _ = proxy.write_all(&[5, 5, 0, 1, 0, 0, 0, 0, 0, 0]).await;
        });
        let target = Target::Address(std::net::Ipv4Addr::new(10, 0, 0, 1).into());
        let error = socks5(&mut ours, &target, 443, None)
            .await
            .expect_err("refused");
        assert!(error.to_string().contains("code 5"), "{error}");
    }
}
