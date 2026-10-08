//! Getting and keeping a TURN relay for a call, a huddle's or a Teams
//! call's: the servers tried in turn, the connection to each, and the
//! [`turn::Client`] on it.
//!
//! The session waits on [`RelayPool::wait`] in its `select!` (a read or
//! a timer, safe to drop half way), then takes [`RelayPool::events`]: the
//! relay is there, a peer sent something through it, it was lost, or no
//! server gave one. Writing to the server ([`RelayPool::flush`]) and
//! trying the next one happen outside the `select!`.
//!
//! A Teams call reaches a TURN server over UDP from the call's own socket
//! ([`Udp::Shared`]): the session reads that socket and hands the
//! server's messages over with [`RelayPool::input`].

use std::collections::VecDeque;
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

use super::turn::{self, Server, Transport};

/// The largest datagram read from a TURN server over UDP: more than any
/// Ethernet MTU.
const DATAGRAM: usize = 2048;

/// What the relay has for the session.
#[derive(Debug)]
pub enum RelayEvent {
    /// The relay is ready: peers reach us at `relayed`.
    Allocated {
        /// The relay's address.
        relayed: SocketAddr,
        /// Our address as the server saw it.
        mapped: Option<SocketAddr>,
        /// Our end of the connection to the server.
        local: SocketAddr,
        /// Whether the server is reached from the call's own socket, so
        /// `mapped` is that socket's address as the internet sees it.
        shared: bool,
        /// The line the log got, for a report.
        line: String,
    },
    /// Bytes from a peer through the relay.
    Data {
        /// Who sent it.
        peer: SocketAddr,
        /// What they sent.
        data: Vec<u8>,
    },
    /// The relay, once given, was lost (why, for the log); no other
    /// server is tried.
    Lost(String),
    /// No server gave a relay; nothing more comes.
    Exhausted,
}

/// How TURN over UDP is reached.
pub enum Udp {
    /// From a socket of the relay's own.
    Own,
    /// From the call's socket, at `local`; the session reads it.
    Shared {
        /// The call's socket.
        socket: Arc<tokio::net::UdpSocket>,
        /// Its address, as our candidates name it.
        local: SocketAddr,
    },
}

/// The connection to one TURN server.
enum Io {
    /// A connection of the relay's own.
    Own(RelayIo),
    /// The call's socket, to the server at this address.
    Shared(SocketAddr),
}

/// A relay being set up or in use.
struct Link {
    server: Server,
    client: turn::Client,
    io: Io,
    /// Our end of the connection to the server.
    local: SocketAddr,
    relayed: Option<SocketAddr>,
}

/// The TURN servers of a call, tried in turn until one gives a relay.
/// Holds the TURN password, so it has no `Debug`.
pub struct RelayPool {
    username: String,
    password: String,
    attempts: VecDeque<Server>,
    /// How long a server may take to be reached, and then to allocate.
    timeout: Duration,
    udp: Udp,
    /// Whether a server already sent us elsewhere: a second redirect is
    /// not followed.
    redirected: bool,
    link: Option<Link>,
    /// When the allocation must be there.
    deadline: Option<Instant>,
    /// Why the relay was lost, to tell with the next events.
    lost: Option<String>,
    /// Whether [`RelayEvent::Exhausted`] or [`RelayEvent::Lost`] was told.
    done: bool,
    /// Read into, again and again, so a datagram costs no allocation.
    buf: Vec<u8>,
}

impl RelayPool {
    /// A pool trying `attempts` in order with the TURN credentials, each
    /// given `timeout` to be reached and as long again to allocate.
    pub fn new(
        attempts: Vec<Server>,
        username: &str,
        password: &str,
        timeout: Duration,
        udp: Udp,
    ) -> Self {
        Self {
            username: username.to_owned(),
            password: password.to_owned(),
            attempts: attempts.into(),
            timeout,
            udp,
            redirected: false,
            link: None,
            deadline: None,
            lost: None,
            done: false,
            buf: vec![0; DATAGRAM],
        }
    }

    /// The relay's address, once allocated.
    pub fn relayed(&self) -> Option<SocketAddr> {
        self.link.as_ref().and_then(|link| link.relayed)
    }

    /// Whether a datagram on the call's socket from `from` is the TURN
    /// server's, for [`Self::input`].
    pub fn takes(&self, from: SocketAddr) -> bool {
        matches!(&self.link, Some(Link { io: Io::Shared(server), .. }) if *server == from)
    }

    /// A message from the server that came on the call's socket.
    pub fn input(&mut self, data: &[u8], now: Instant) {
        if let Some(link) = &mut self.link {
            link.client.handle_input(data, now);
        }
    }

    /// Lets `peers` reach us through the relay, once it is there.
    pub fn permit(&mut self, peers: &[IpAddr], now: Instant) {
        if let Some(link) = &mut self.link
            && link.relayed.is_some()
            && !peers.is_empty()
        {
            link.client.permit(peers, now);
        }
    }

    /// Sends `data` to `peer` through the relay, at the next flush.
    pub fn send_to(&mut self, peer: SocketAddr, data: &[u8], now: Instant) {
        if let Some(link) = &mut self.link {
            link.client.send_to(peer, data, now);
        }
    }

    /// Writes what the client queued for the server.
    pub async fn flush(&mut self) {
        let Some(link) = &mut self.link else {
            return;
        };
        while let Some(bytes) = link.client.poll_transmit() {
            let sent = match (&mut link.io, &self.udp) {
                (Io::Own(io), _) => io.send(&bytes).await,
                (Io::Shared(server), Udp::Shared { socket, .. }) => {
                    socket.send_to(&bytes, *server).await.map(|_| ())
                }
                (Io::Shared(_), Udp::Own) => Ok(()),
            };
            if let Err(error) = sent {
                log::warn!("relay: could not write to {}: {error}", link.server);
                break;
            }
        }
    }

    /// Lets the relay go.
    pub async fn close(&mut self, now: Instant) {
        if let Some(link) = &mut self.link {
            link.client.close(now);
        }
        self.flush().await;
    }

    /// Waits for the server: a message on its own connection, or a timer
    /// of the client's or the allocation's; never without a server. Safe
    /// to drop half way; [`Self::events`] says what it came to.
    pub async fn wait(&mut self) {
        let Some(link) = &mut self.link else {
            return std::future::pending().await;
        };
        let wake = [link.client.poll_timeout(), self.deadline]
            .into_iter()
            .flatten()
            .min();
        let buf = &mut self.buf;
        let received = tokio::select! {
            received = async {
                match &mut link.io {
                    Io::Own(RelayIo::Udp(socket)) => socket.recv(buf).await,
                    Io::Own(RelayIo::Stream { stream, .. }) => stream.read(buf).await,
                    Io::Shared(_) => std::future::pending().await,
                }
            } => Some(received),
            () = async {
                match wake {
                    Some(at) => tokio::time::sleep_until(at.into()).await,
                    None => std::future::pending().await,
                }
            } => None,
        };
        match received {
            Some(received) => self.received(received),
            None => self.on_time(Instant::now()),
        }
    }

    /// What happened since the last call, for the session: the client's
    /// events, a lost connection, and the next server tried (waited for
    /// here) when one failed; what the client queued is written after.
    /// Call it once the credentials are there to try the first server.
    pub async fn events(&mut self) -> Vec<RelayEvent> {
        let mut events = Vec::new();
        loop {
            if let Some(why) = self.lost.take() {
                events.push(self.give_up(why));
                break;
            }
            let Some(link) = &mut self.link else {
                if self.done {
                    break;
                }
                let Some(server) = self.attempts.pop_front() else {
                    self.done = true;
                    events.push(RelayEvent::Exhausted);
                    break;
                };
                self.open(server).await;
                continue;
            };
            let Some(event) = link.client.poll_event() else {
                break;
            };
            match event {
                turn::Event::Allocated {
                    relayed,
                    mapped,
                    lifetime,
                } => {
                    link.relayed = Some(relayed);
                    self.deadline = None;
                    let line = format!(
                        "{} relays at {relayed} (it sees us at {}; {lifetime} s)",
                        link.server,
                        mapped.map_or_else(|| "?".to_owned(), |m| m.to_string())
                    );
                    log::info!("relay: {line}");
                    events.push(RelayEvent::Allocated {
                        relayed,
                        mapped,
                        local: link.local,
                        shared: matches!(link.io, Io::Shared(_)),
                        line,
                    });
                }
                turn::Event::Permitted(ip) => log::info!("relay: {ip} may reach us"),
                turn::Event::Data { peer, data } => events.push(RelayEvent::Data { peer, data }),
                turn::Event::Failed(why) => {
                    log::warn!("relay: {}: {why}", link.server);
                    if link.relayed.is_some() {
                        events.push(self.give_up(why));
                        break;
                    }
                    // Sent elsewhere (300 Try Alternate): asked there
                    // next, over UDP, once, before the other servers.
                    if link.server.transport == Transport::Udp
                        && let Some(alternate) = link.client.alternate()
                        && !self.redirected
                    {
                        log::info!("relay: sent to {alternate}");
                        self.redirected = true;
                        self.attempts.push_front(Server {
                            host: alternate.ip().to_string(),
                            port: alternate.port(),
                            transport: Transport::Udp,
                        });
                    }
                    self.link = None;
                    self.deadline = None;
                }
                turn::Event::Note(note) => log::info!("relay: {note}"),
            }
        }
        self.flush().await;
        events
    }

    /// Opens the connection to `server` and asks it for a relay.
    async fn open(&mut self, server: Server) {
        log::info!("relay: trying {server}");
        let opened = match &self.udp {
            Udp::Shared { local, .. } if server.transport == Transport::Udp => {
                shared_link(&server, *local, self.timeout).await
            }
            _ => match tokio::time::timeout(self.timeout, connect_relay(&server)).await {
                Ok(Ok((io, local))) => Ok((Io::Own(io), local)),
                Ok(Err(why)) => Err(why),
                Err(_) => Err("no connection in time".to_owned()),
            },
        };
        match opened {
            Ok((io, local)) => {
                let now = Instant::now();
                let mut client =
                    turn::Client::new(server.transport, &self.username, &self.password);
                client.allocate(now);
                self.link = Some(Link {
                    server,
                    client,
                    io,
                    local,
                    relayed: None,
                });
                self.deadline = Some(now + self.timeout);
            }
            Err(why) => log::warn!("relay: {server}: {why}"),
        }
    }

    /// What was read from the server's own connection.
    fn received(&mut self, received: std::io::Result<usize>) {
        let now = Instant::now();
        let Some(link) = &mut self.link else {
            return;
        };
        match (received, &mut link.io) {
            (Ok(0), Io::Own(RelayIo::Stream { .. })) => {
                self.broken("the TURN server closed the connection".to_owned());
            }
            (Ok(n), Io::Own(RelayIo::Stream { buffer, .. })) => {
                buffer.extend_from_slice(&self.buf[..n]);
                while let Some(message) = turn::split_stream(buffer) {
                    link.client.handle_input(&message, now);
                }
            }
            (Ok(n), _) => link.client.handle_input(&self.buf[..n], now),
            (Err(error), _) => self.broken(error.to_string()),
        }
    }

    /// The connection to the server broke: the next server is tried, or
    /// the session hears the relay is lost.
    fn broken(&mut self, why: String) {
        let Some(link) = self.link.take() else {
            return;
        };
        log::warn!("relay: {}: {why}", link.server);
        self.deadline = None;
        if link.relayed.is_some() {
            self.lost = Some(why);
        }
    }

    /// The relay is lost for good: nothing more is tried.
    fn give_up(&mut self, why: String) -> RelayEvent {
        self.link = None;
        self.deadline = None;
        self.attempts.clear();
        self.done = true;
        RelayEvent::Lost(why)
    }

    /// What is due at `now`: the client's resends and refreshes, and the
    /// allocation's deadline.
    fn on_time(&mut self, now: Instant) {
        if let Some(link) = &mut self.link
            && link.client.poll_timeout().is_some_and(|at| at <= now)
        {
            link.client.handle_timeout(now);
        }
        if self.deadline.is_some_and(|at| at <= now) {
            if let Some(link) = &self.link {
                log::warn!("relay: {}: no allocation in time", link.server);
            }
            self.link = None;
            self.deadline = None;
        }
    }
}

/// TURN over UDP from the call's socket, which is at `local`: the
/// server's IPv4 address.
async fn shared_link(
    server: &Server,
    local: SocketAddr,
    timeout: Duration,
) -> Result<(Io, SocketAddr), String> {
    let address = tokio::time::timeout(
        timeout,
        tokio::net::lookup_host((server.host.as_str(), server.port)),
    )
    .await
    .map_err(|_| "no address in time".to_owned())?
    .map_err(|e| format!("{}: {e}", server.host))?
    .find(SocketAddr::is_ipv4)
    .ok_or_else(|| format!("{} has no IPv4 address", server.host))?;
    log::info!("relay: {server} at {address}, from our socket");
    Ok((Io::Shared(address), local))
}

/// The connection to a TURN server.
enum RelayIo {
    Udp(tokio::net::UdpSocket),
    Stream {
        stream: Box<dyn Stream>,
        /// What was read and is not yet a whole message.
        buffer: Vec<u8>,
    },
}

/// A TCP or TLS stream.
trait Stream: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send {}
impl<T: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send> Stream for T {}

impl RelayIo {
    /// Writes one message to the server.
    async fn send(&mut self, bytes: &[u8]) -> std::io::Result<()> {
        match self {
            Self::Udp(socket) => socket.send(bytes).await.map(|_| ()),
            Self::Stream { stream, .. } => stream.write_all(bytes).await,
        }
    }
}

/// The TLS setup for `turns:` servers: the system's roots, ring's crypto,
/// as the rest of the app's TLS.
fn tls_config() -> Result<Arc<tokio_rustls::rustls::ClientConfig>, String> {
    use tokio_rustls::rustls;
    let mut roots = rustls::RootCertStore::empty();
    for cert in rustls_native_certs::load_native_certs().certs {
        let _ = roots.add(cert);
    }
    let config = rustls::ClientConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .map_err(|e| e.to_string())?
    .with_root_certificates(roots)
    .with_no_client_auth();
    Ok(Arc::new(config))
}

/// Opens the connection to a TURN server, over its transport, and says
/// which local address it left from.
async fn connect_relay(server: &Server) -> Result<(RelayIo, SocketAddr), String> {
    let address = tokio::net::lookup_host((server.host.as_str(), server.port))
        .await
        .map_err(|e| format!("{}: {e}", server.host))?
        .next()
        .ok_or_else(|| format!("{} has no address", server.host))?;
    let (io, local) = match server.transport {
        Transport::Udp => {
            let any: SocketAddr = if address.is_ipv4() {
                "0.0.0.0:0".parse().map_err(|_| "no IPv4 wildcard")?
            } else {
                "[::]:0".parse().map_err(|_| "no IPv6 wildcard")?
            };
            let socket = tokio::net::UdpSocket::bind(any)
                .await
                .map_err(|e| e.to_string())?;
            socket.connect(address).await.map_err(|e| e.to_string())?;
            let local = socket.local_addr().map_err(|e| e.to_string())?;
            (RelayIo::Udp(socket), local)
        }
        Transport::Tcp | Transport::Tls => {
            let tcp = tokio::net::TcpStream::connect(address)
                .await
                .map_err(|e| e.to_string())?;
            let _ = tcp.set_nodelay(true);
            let local = tcp.local_addr().map_err(|e| e.to_string())?;
            let stream: Box<dyn Stream> = if server.transport == Transport::Tls {
                let name =
                    tokio_rustls::rustls::pki_types::ServerName::try_from(server.host.clone())
                        .map_err(|e| e.to_string())?;
                let tls = tokio_rustls::TlsConnector::from(tls_config()?)
                    .connect(name, tcp)
                    .await
                    .map_err(|e| format!("TLS: {e}"))?;
                Box::new(tls)
            } else {
                Box::new(tcp)
            };
            (
                RelayIo::Stream {
                    stream,
                    buffer: Vec::new(),
                },
                local,
            )
        }
    };
    log::info!("relay: connected to {server} at {address} from {local}");
    Ok((io, local))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A pool with no server says so once, then nothing more.
    #[tokio::test]
    async fn a_pool_with_no_server_is_exhausted_once() {
        let mut pool = RelayPool::new(
            Vec::new(),
            "user",
            "secret",
            Duration::from_secs(1),
            Udp::Own,
        );
        assert!(matches!(pool.events().await[..], [RelayEvent::Exhausted]));
        assert!(pool.events().await.is_empty());
        let waited = tokio::time::timeout(Duration::from_millis(20), pool.wait()).await;
        assert!(waited.is_err(), "nothing to wait for");
        assert!(pool.relayed().is_none());
    }

    /// A server that answers nothing is given up on after the timeout;
    /// with none left, the pool is exhausted.
    #[tokio::test]
    async fn a_silent_server_is_given_up_on() {
        // A UDP socket of ours that reads and never answers.
        let silent = tokio::net::UdpSocket::bind("127.0.0.1:0")
            .await
            .expect("a socket");
        let at = silent.local_addr().expect("an address");
        let server = Server {
            host: at.ip().to_string(),
            port: at.port(),
            transport: Transport::Udp,
        };
        let mut pool = RelayPool::new(
            vec![server],
            "user",
            "secret",
            Duration::from_millis(100),
            Udp::Own,
        );
        assert!(pool.events().await.is_empty(), "asking the first");
        // The allocation request reached the server.
        let mut buf = [0u8; 1500];
        let (n, _) = silent.recv_from(&mut buf).await.expect("a request");
        assert!(turn::Message::decode(&buf[..n]).is_ok());
        let events = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                pool.wait().await;
                let events = pool.events().await;
                if !events.is_empty() {
                    break events;
                }
            }
        })
        .await
        .expect("an answer in time");
        assert!(matches!(events[..], [RelayEvent::Exhausted]), "{events:?}");
    }
}
