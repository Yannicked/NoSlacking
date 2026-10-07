//! A small TURN client (RFC 8656, the parts a relayed WebRTC call needs),
//! written like `str0m`: no sockets, only bytes in and bytes out.
//!
//! Chime's media servers are reached only through its TURN servers
//! (HuddleFM sets `iceTransportPolicy: "relay"`), and neither `str0m` nor
//! `webrtc-rs` talks TURN over TCP or TLS, so the relay is ours. One
//! [`Client`] holds one allocation:
//!
//! 1. Allocate (UDP relay), answered 401 with a realm and nonce, then
//!    again with the long-term credential (MESSAGE-INTEGRITY keyed with
//!    MD5 of `username:realm:password`);
//! 2. CreatePermission for each peer (the media server's address from the
//!    SDP answer), renewed every four minutes;
//! 3. Send indications out, Data indications in;
//! 4. Refresh before the allocation runs out, and with a lifetime of zero
//!    to let it go.
//!
//! Over UDP, requests are sent again until answered (RFC 8489's
//! retransmission, shortened); over TCP and TLS each message goes once and
//! [`split_stream`] cuts the incoming bytes into messages.

use std::collections::{HashMap, VecDeque};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::time::{Duration, Instant};

use hmac::{KeyInit as _, Mac as _};

/// STUN's magic cookie, in every header.
pub const MAGIC: u32 = 0x2112_A442;
const HEADER: usize = 20;

/// How a TURN server is reached.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Transport {
    /// UDP, `turn:…?transport=udp`.
    Udp,
    /// Plain TCP, `turn:…?transport=tcp`.
    Tcp,
    /// TLS over TCP (`turns:`).
    Tls,
}

/// A TURN server from a `turn:` or `turns:` URI.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Server {
    /// Its name or address.
    pub host: String,
    /// Its port.
    pub port: u16,
    /// How it is reached.
    pub transport: Transport,
}

impl std::fmt::Display for Server {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}:{} over {:?}", self.host, self.port, self.transport)
    }
}

/// Reads `turn:host[:port][?transport=udp|tcp]` or `turns:…` (RFC 7065).
/// `turns:` is TLS over TCP whatever the transport says.
pub fn parse_uri(uri: &str) -> Option<Server> {
    let (scheme, rest) = uri.split_once(':')?;
    let tls = match scheme.to_ascii_lowercase().as_str() {
        "turn" => false,
        "turns" => true,
        _ => return None,
    };
    let (address, query) = rest.split_once('?').unwrap_or((rest, ""));
    let tcp = query.split('&').any(|pair| {
        pair.split_once('=').is_some_and(|(k, v)| {
            k.eq_ignore_ascii_case("transport") && v.eq_ignore_ascii_case("tcp")
        })
    });
    let (host, port) = if let Some(v6) = address.strip_prefix('[') {
        let (host, after) = v6.split_once(']')?;
        (host, after.strip_prefix(':'))
    } else {
        match address.rsplit_once(':') {
            Some((host, port)) => (host, Some(port)),
            None => (address, None),
        }
    };
    if host.is_empty() {
        return None;
    }
    let port = match port {
        Some(port) => port.parse().ok()?,
        None if tls => 5349,
        None => 3478,
    };
    let transport = if tls {
        Transport::Tls
    } else if tcp {
        Transport::Tcp
    } else {
        Transport::Udp
    };
    Some(Server {
        host: host.to_owned(),
        port,
        transport,
    })
}

/// The order to try servers in: UDP first (what media wants), then TLS
/// (what gets through firewalls), then plain TCP.
pub fn by_preference(uris: &[String]) -> Vec<Server> {
    let mut servers: Vec<Server> = uris.iter().filter_map(|u| parse_uri(u)).collect();
    servers.sort_by_key(|s| match s.transport {
        Transport::Udp => 0,
        Transport::Tls => 1,
        Transport::Tcp => 2,
    });
    servers
}

// ---- STUN messages ---------------------------------------------------

/// STUN and TURN methods.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Method {
    /// STUN's own (RFC 8489), seen only in test vectors here.
    Binding = 0x001,
    /// Asks for a relay.
    Allocate = 0x003,
    /// Renews or releases one.
    Refresh = 0x004,
    /// Data to a peer (an indication).
    Send = 0x006,
    /// Data from a peer (an indication).
    Data = 0x007,
    /// Lets a peer's address through.
    CreatePermission = 0x008,
    /// Binds a channel to a peer; not used.
    ChannelBind = 0x009,
}

impl Method {
    fn of(bits: u16) -> Option<Self> {
        Some(match bits {
            0x001 => Self::Binding,
            0x003 => Self::Allocate,
            0x004 => Self::Refresh,
            0x006 => Self::Send,
            0x007 => Self::Data,
            0x008 => Self::CreatePermission,
            0x009 => Self::ChannelBind,
            _ => return None,
        })
    }
}

/// A message's class.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Class {
    /// Wants an answer.
    Request = 0,
    /// Wants none.
    Indication = 1,
    /// A request's success.
    Success = 2,
    /// A request's refusal.
    Error = 3,
}

/// The message type field: method bits interleaved with the class bits
/// (RFC 8489 §5).
#[cfg(test)]
fn message_type(method: Method, class: Class) -> u16 {
    type_bits(method as u16, class)
}

fn type_bits(m: u16, class: Class) -> u16 {
    let c = class as u16;
    (m & 0x000F) | ((m & 0x0070) << 1) | ((m & 0x0F80) << 2) | ((c & 1) << 4) | ((c & 2) << 7)
}

fn split_type(kind: u16) -> (u16, Class) {
    let method = (kind & 0x000F) | ((kind >> 1) & 0x0070) | ((kind >> 2) & 0x0F80);
    let class = match ((kind >> 4) & 1) | ((kind >> 7) & 2) {
        0 => Class::Request,
        1 => Class::Indication,
        2 => Class::Success,
        _ => Class::Error,
    };
    (method, class)
}

/// Attribute types used here.
pub mod attr {
    /// The long-term credential's username.
    pub const USERNAME: u16 = 0x0006;
    /// HMAC-SHA1 over the message before it.
    pub const MESSAGE_INTEGRITY: u16 = 0x0008;
    /// Why a request was refused.
    pub const ERROR_CODE: u16 = 0x0009;
    /// A channel's number; not used.
    pub const CHANNEL_NUMBER: u16 = 0x000C;
    /// An allocation's lifetime, in seconds.
    pub const LIFETIME: u16 = 0x000D;
    /// A peer's address.
    pub const XOR_PEER_ADDRESS: u16 = 0x0012;
    /// What a Send or Data indication carries.
    pub const DATA: u16 = 0x0013;
    /// The server's realm, for the credential.
    pub const REALM: u16 = 0x0014;
    /// The server's nonce, for the credential.
    pub const NONCE: u16 = 0x0015;
    /// The relay's address.
    pub const XOR_RELAYED_ADDRESS: u16 = 0x0016;
    /// The relay's transport: UDP.
    pub const REQUESTED_TRANSPORT: u16 = 0x0019;
    /// Our address as the server sees it.
    pub const XOR_MAPPED_ADDRESS: u16 = 0x0020;
    /// The sender's software; not sent.
    pub const SOFTWARE: u16 = 0x8022;
    /// Where to allocate instead (with a 300 Try Alternate), written as a
    /// plain address, not XORed.
    pub const ALTERNATE_SERVER: u16 = 0x8023;
    /// A CRC of the message; not sent.
    pub const FINGERPRINT: u16 = 0x8028;
}

/// A STUN transaction id.
pub type TransactionId = [u8; 12];

/// A STUN message being built or one read.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Message {
    /// The raw method bits; [`Message::method`] names the ones known.
    pub method: u16,
    /// Request, indication, success or error.
    pub class: Class,
    /// Pairs an answer with its request.
    pub transaction: TransactionId,
    /// Attributes in order, values without padding.
    pub attributes: Vec<(u16, Vec<u8>)>,
}

/// Why bytes are not a STUN message.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum StunError {
    /// Shorter than a header.
    #[error("too short")]
    Short,
    /// No magic cookie, or the first bits are set.
    #[error("not STUN")]
    NotStun,
    /// The lengths do not add up.
    #[error("the length does not match")]
    Length,
}

impl Message {
    /// A new message with no attributes.
    pub fn new(method: Method, class: Class, transaction: TransactionId) -> Self {
        Self {
            method: method as u16,
            class,
            transaction,
            attributes: Vec::new(),
        }
    }

    /// The method, when it is one this client knows.
    pub fn method(&self) -> Option<Method> {
        Method::of(self.method)
    }

    /// Adds an attribute.
    pub fn with(mut self, kind: u16, value: impl Into<Vec<u8>>) -> Self {
        self.attributes.push((kind, value.into()));
        self
    }

    /// The first attribute of `kind`.
    pub fn get(&self, kind: u16) -> Option<&[u8]> {
        self.attributes
            .iter()
            .find(|(k, _)| *k == kind)
            .map(|(_, v)| v.as_slice())
    }

    /// The bytes, ending with MESSAGE-INTEGRITY when a key is given.
    pub fn encode(&self, key: Option<&[u8]>) -> Vec<u8> {
        let mut out = Vec::with_capacity(128);
        out.extend_from_slice(&type_bits(self.method, self.class).to_be_bytes());
        out.extend_from_slice(&[0, 0]);
        out.extend_from_slice(&MAGIC.to_be_bytes());
        out.extend_from_slice(&self.transaction);
        for (kind, value) in &self.attributes {
            push_attribute(&mut out, *kind, value);
        }
        if let Some(key) = key {
            // The length counts the integrity attribute it is computed
            // without (RFC 8489 §14.5).
            let length = out.len() - HEADER + 24;
            set_length(&mut out, length);
            let mac = hmac_sha1(key, &out);
            push_attribute(&mut out, attr::MESSAGE_INTEGRITY, &mac);
        }
        let length = out.len() - HEADER;
        set_length(&mut out, length);
        out
    }

    /// Reads one message.
    pub fn decode(bytes: &[u8]) -> Result<Self, StunError> {
        if bytes.len() < HEADER {
            return Err(StunError::Short);
        }
        if bytes[0] & 0xC0 != 0 || be32(&bytes[4..8]) != MAGIC {
            return Err(StunError::NotStun);
        }
        let length = usize::from(be16(&bytes[2..4]));
        if length % 4 != 0 || HEADER + length > bytes.len() {
            return Err(StunError::Length);
        }
        let (method, class) = split_type(be16(&bytes[0..2]));
        let mut transaction = [0u8; 12];
        transaction.copy_from_slice(&bytes[8..20]);
        let mut attributes = Vec::new();
        let mut at = HEADER;
        let end = HEADER + length;
        while at + 4 <= end {
            let kind = be16(&bytes[at..at + 2]);
            let len = usize::from(be16(&bytes[at + 2..at + 4]));
            let start = at + 4;
            if start + len > end {
                return Err(StunError::Length);
            }
            attributes.push((kind, bytes[start..start + len].to_vec()));
            at = start + padded(len);
        }
        Ok(Self {
            method,
            class,
            transaction,
            attributes,
        })
    }
}

/// Checks MESSAGE-INTEGRITY on a message as received: the HMAC over the
/// bytes before it, with the length set to end just after it.
pub fn integrity_ok(bytes: &[u8], key: &[u8]) -> bool {
    let Ok(message) = Message::decode(bytes) else {
        return false;
    };
    let Some(expected) = message.get(attr::MESSAGE_INTEGRITY) else {
        return false;
    };
    // Find where the attribute starts.
    let mut at = HEADER;
    let end = HEADER + usize::from(be16(&bytes[2..4]));
    while at + 4 <= end {
        let kind = be16(&bytes[at..at + 2]);
        let len = usize::from(be16(&bytes[at + 2..at + 4]));
        if kind == attr::MESSAGE_INTEGRITY {
            let mut covered = bytes[..at].to_vec();
            set_length(&mut covered, at - HEADER + 24);
            return hmac_sha1(key, &covered).as_slice() == expected;
        }
        at += 4 + padded(len);
    }
    false
}

fn push_attribute(out: &mut Vec<u8>, kind: u16, value: &[u8]) {
    out.extend_from_slice(&kind.to_be_bytes());
    // Attribute values here are far below 64 KiB.
    out.extend_from_slice(&u16::try_from(value.len()).unwrap_or(u16::MAX).to_be_bytes());
    out.extend_from_slice(value);
    out.resize(out.len() + padded(value.len()) - value.len(), 0);
}

fn set_length(out: &mut [u8], length: usize) {
    let length = u16::try_from(length).unwrap_or(u16::MAX).to_be_bytes();
    out[2..4].copy_from_slice(&length);
}

fn padded(len: usize) -> usize {
    len.div_ceil(4) * 4
}

fn be16(bytes: &[u8]) -> u16 {
    u16::from_be_bytes([bytes[0], bytes[1]])
}

fn be32(bytes: &[u8]) -> u32 {
    u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]])
}

fn hmac_sha1(key: &[u8], data: &[u8]) -> Vec<u8> {
    // HMAC takes a key of any length.
    let Ok(mut mac) = hmac::Hmac::<sha1::Sha1>::new_from_slice(key) else {
        return vec![0; 20];
    };
    mac.update(data);
    mac.finalize().into_bytes().to_vec()
}

/// The long-term credential's key: MD5 of `username:realm:password`
/// (RFC 8489 §9.2.2, without SASLprep, which ASCII credentials need not).
pub fn long_term_key(username: &str, realm: &str, password: &str) -> [u8; 16] {
    use md5::Digest as _;
    let mut md5 = md5::Md5::new();
    md5.update(format!("{username}:{realm}:{password}").as_bytes());
    md5.finalize().into()
}

/// An XOR-…-ADDRESS value for `address` (RFC 8489 §14.2).
pub fn xor_address(address: SocketAddr, transaction: &TransactionId) -> Vec<u8> {
    let port = address.port() ^ (MAGIC >> 16) as u16;
    let mut out = vec![0];
    match address.ip() {
        IpAddr::V4(ip) => {
            out.push(1);
            out.extend_from_slice(&port.to_be_bytes());
            let x = u32::from(ip) ^ MAGIC;
            out.extend_from_slice(&x.to_be_bytes());
        }
        IpAddr::V6(ip) => {
            out.push(2);
            out.extend_from_slice(&port.to_be_bytes());
            let mut mask = [0u8; 16];
            mask[..4].copy_from_slice(&MAGIC.to_be_bytes());
            mask[4..].copy_from_slice(transaction);
            for (byte, m) in ip.octets().iter().zip(mask) {
                out.push(byte ^ m);
            }
        }
    }
    out
}

/// Reads an XOR-…-ADDRESS value.
pub fn read_xor_address(value: &[u8], transaction: &TransactionId) -> Option<SocketAddr> {
    if value.len() < 8 {
        return None;
    }
    let port = be16(&value[2..4]) ^ (MAGIC >> 16) as u16;
    let ip = match value[1] {
        1 => IpAddr::V4(Ipv4Addr::from(be32(&value[4..8]) ^ MAGIC)),
        2 if value.len() >= 20 => {
            let mut mask = [0u8; 16];
            mask[..4].copy_from_slice(&MAGIC.to_be_bytes());
            mask[4..].copy_from_slice(transaction);
            let mut octets = [0u8; 16];
            for (i, o) in octets.iter_mut().enumerate() {
                *o = value[4 + i] ^ mask[i];
            }
            IpAddr::V6(Ipv6Addr::from(octets))
        }
        _ => return None,
    };
    Some(SocketAddr::new(ip, port))
}

/// An ERROR-CODE value's code and reason.
/// An address as STUN writes one unXORed (`ALTERNATE-SERVER`): a zero, the
/// family (1 IPv4, 2 IPv6), the port, the address.
pub fn read_plain_address(value: &[u8]) -> Option<SocketAddr> {
    let port = u16::from_be_bytes([*value.get(2)?, *value.get(3)?]);
    let ip = match value.get(1)? {
        1 => {
            let b: [u8; 4] = value.get(4..8)?.try_into().ok()?;
            IpAddr::from(b)
        }
        2 => {
            let b: [u8; 16] = value.get(4..20)?.try_into().ok()?;
            IpAddr::from(b)
        }
        _ => return None,
    };
    Some(SocketAddr::new(ip, port))
}

pub fn read_error(value: &[u8]) -> Option<(u16, String)> {
    if value.len() < 4 {
        return None;
    }
    let code = u16::from(value[2] & 0x07) * 100 + u16::from(value[3]);
    Some((code, String::from_utf8_lossy(&value[4..]).into_owned()))
}

/// Cuts the next whole message off a TCP or TLS stream's bytes: a STUN
/// message, or ChannelData (padded to four bytes over a stream). `None`
/// until one is complete.
pub fn split_stream(buffer: &mut Vec<u8>) -> Option<Vec<u8>> {
    if buffer.len() < 4 {
        return None;
    }
    let length = usize::from(be16(&buffer[2..4]));
    let total = if buffer[0] & 0xC0 == 0 {
        HEADER + length
    } else {
        4 + padded(length)
    };
    if buffer.len() < total {
        return None;
    }
    let rest = buffer.split_off(total);
    Some(std::mem::replace(buffer, rest))
}

// ---- the client --------------------------------------------------------

/// What the allocation asks for and how long permissions last.
const ALLOCATION_LIFETIME: u32 = 600;
const PERMISSION_REFRESH: Duration = Duration::from_secs(240);
/// The first wait before sending a request again over UDP, doubled each
/// time, and how many times to send it in all.
const FIRST_RTO: Duration = Duration::from_millis(500);
const SENDS: u32 = 6;

/// What a [`Client`] tells its driver.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Event {
    /// The relay is ready: peers reach us at `relayed`.
    Allocated {
        /// The relay's address.
        relayed: SocketAddr,
        /// Our address as the server sees it.
        mapped: Option<SocketAddr>,
        /// How long it lasts unless renewed, in seconds.
        lifetime: u32,
    },
    /// A peer may now send to us.
    Permitted(IpAddr),
    /// Bytes from a peer through the relay.
    Data {
        /// Who sent it.
        peer: SocketAddr,
        /// What they sent.
        data: Vec<u8>,
    },
    /// The allocation failed or was lost; nothing more will come.
    Failed(String),
    /// Worth a line in the log; ends nothing.
    Note(String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum Purpose {
    Allocate,
    Refresh { lifetime: u32 },
    Permission(Vec<IpAddr>),
}

#[derive(Debug)]
struct Pending {
    purpose: Purpose,
    bytes: Vec<u8>,
    sends: u32,
    next_send: Instant,
    wait: Duration,
    /// Whether this request already answered a 401 or 438 with new
    /// credentials, so a second one is a real refusal.
    authenticated_retry: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum State {
    Idle,
    Allocating,
    Allocated,
    Closing,
    Failed,
}

/// One TURN allocation, without I/O. The driver writes [`Client::poll_transmit`]
/// to the server, feeds back what it reads with [`Client::handle_input`],
/// and wakes it at [`Client::poll_timeout`].
pub struct Client {
    transport: Transport,
    username: String,
    password: String,
    realm: Option<String>,
    nonce: Option<Vec<u8>>,
    /// Where the server sent us instead, with a 300 Try Alternate.
    alternate: Option<SocketAddr>,
    key: Option<[u8; 16]>,
    state: State,
    relayed: Option<SocketAddr>,
    refresh_at: Option<Instant>,
    permissions: HashMap<IpAddr, Instant>,
    pending: HashMap<TransactionId, Pending>,
    transmit: VecDeque<Vec<u8>>,
    events: VecDeque<Event>,
    next_transaction: Box<dyn FnMut() -> TransactionId + Send>,
}

impl std::fmt::Debug for Client {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Client")
            .field("transport", &self.transport)
            .field("state", &self.state)
            .field("relayed", &self.relayed)
            .field("password", &crate::redact::REDACTED)
            .finish_non_exhaustive()
    }
}

/// Random transaction ids.
pub fn random_transaction() -> TransactionId {
    use rand::Rng as _;
    let mut id = [0u8; 12];
    rand::rng().fill_bytes(&mut id);
    id
}

impl Client {
    /// A client for a server reached over `transport`, with the TURN
    /// credentials from JOIN_ACK.
    pub fn new(transport: Transport, username: &str, password: &str) -> Self {
        Self::with_transactions(transport, username, password, Box::new(random_transaction))
    }

    /// As [`Client::new`], with transaction ids from `next` (tests).
    pub fn with_transactions(
        transport: Transport,
        username: &str,
        password: &str,
        next: Box<dyn FnMut() -> TransactionId + Send>,
    ) -> Self {
        Self {
            transport,
            username: username.to_owned(),
            password: password.to_owned(),
            realm: None,
            alternate: None,
            nonce: None,
            key: None,
            state: State::Idle,
            relayed: None,
            refresh_at: None,
            permissions: HashMap::new(),
            pending: HashMap::new(),
            transmit: VecDeque::new(),
            events: VecDeque::new(),
            next_transaction: next,
        }
    }

    /// The relayed address, once allocated.
    pub fn relayed(&self) -> Option<SocketAddr> {
        self.relayed
    }

    /// Whether `ip` may send to us.
    pub fn permitted(&self, ip: IpAddr) -> bool {
        self.permissions.contains_key(&ip)
    }

    /// Asks for the allocation.
    pub fn allocate(&mut self, now: Instant) {
        if self.state != State::Idle {
            return;
        }
        self.state = State::Allocating;
        self.request(Purpose::Allocate, now);
    }

    /// Asks the server to let `peers` reach us (and us them). Already
    /// permitted ones are left alone.
    pub fn permit(&mut self, peers: &[IpAddr], now: Instant) {
        let mut wanted: Vec<IpAddr> = peers
            .iter()
            .copied()
            .filter(|ip| !self.permissions.contains_key(ip))
            .filter(|ip| {
                !self
                    .pending
                    .values()
                    .any(|p| matches!(&p.purpose, Purpose::Permission(ips) if ips.contains(ip)))
            })
            .collect();
        wanted.sort();
        wanted.dedup();
        if wanted.is_empty() || self.state != State::Allocated {
            return;
        }
        self.request(Purpose::Permission(wanted), now);
    }

    /// Sends `data` to `peer` through the relay (a Send indication),
    /// asking for its permission first if it has none yet.
    pub fn send_to(&mut self, peer: SocketAddr, data: &[u8], now: Instant) {
        if self.state != State::Allocated {
            return;
        }
        if !self.permitted(peer.ip()) {
            self.permit(&[peer.ip()], now);
        }
        let transaction = (self.next_transaction)();
        let message = Message::new(Method::Send, Class::Indication, transaction)
            .with(attr::XOR_PEER_ADDRESS, xor_address(peer, &transaction))
            .with(attr::DATA, data.to_vec());
        self.transmit.push_back(message.encode(None));
    }

    /// Lets the allocation go (a Refresh with lifetime 0).
    pub fn close(&mut self, now: Instant) {
        if self.state == State::Allocated {
            self.state = State::Closing;
            self.request(Purpose::Refresh { lifetime: 0 }, now);
        }
    }

    /// The next bytes for the server.
    pub fn poll_transmit(&mut self) -> Option<Vec<u8>> {
        self.transmit.pop_front()
    }

    /// The next event.
    pub fn poll_event(&mut self) -> Option<Event> {
        self.events.pop_front()
    }

    /// When to call [`Client::handle_timeout`] next.
    pub fn poll_timeout(&self) -> Option<Instant> {
        let resend = if self.transport == Transport::Udp {
            self.pending.values().map(|p| p.next_send).min()
        } else {
            None
        };
        let permissions = self.permissions.values().min().copied();
        [resend, self.refresh_at, permissions]
            .into_iter()
            .flatten()
            .min()
    }

    /// Sends again what went unanswered, renews what is due.
    pub fn handle_timeout(&mut self, now: Instant) {
        if self.transport == Transport::Udp {
            let mut given_up = Vec::new();
            for (id, pending) in &mut self.pending {
                if pending.next_send > now {
                    continue;
                }
                if pending.sends >= SENDS {
                    given_up.push(*id);
                    continue;
                }
                pending.sends += 1;
                pending.wait *= 2;
                pending.next_send = now + pending.wait;
                self.transmit.push_back(pending.bytes.clone());
            }
            for id in given_up {
                if let Some(pending) = self.pending.remove(&id) {
                    self.unanswered(&pending.purpose);
                }
            }
        }
        if self.state == State::Allocated && self.refresh_at.is_some_and(|at| at <= now) {
            self.refresh_at = None;
            self.request(
                Purpose::Refresh {
                    lifetime: ALLOCATION_LIFETIME,
                },
                now,
            );
        }
        let due: Vec<IpAddr> = self
            .permissions
            .iter()
            .filter(|(_, at)| **at <= now)
            .map(|(ip, _)| *ip)
            .collect();
        if !due.is_empty() && self.state == State::Allocated {
            for ip in &due {
                self.permissions.remove(ip);
            }
            self.request(Purpose::Permission(due), now);
        }
    }

    /// Reads one message from the server.
    pub fn handle_input(&mut self, bytes: &[u8], now: Instant) {
        let Ok(message) = Message::decode(bytes) else {
            self.events.push_back(Event::Note(format!(
                "a {}-byte message from the TURN server that is not STUN",
                bytes.len()
            )));
            return;
        };
        if message.class == Class::Indication {
            if message.method() == Some(Method::Data) {
                let peer = message
                    .get(attr::XOR_PEER_ADDRESS)
                    .and_then(|v| read_xor_address(v, &message.transaction));
                if let (Some(peer), Some(data)) = (peer, message.get(attr::DATA)) {
                    self.events.push_back(Event::Data {
                        peer,
                        data: data.to_vec(),
                    });
                }
            }
            return;
        }
        let Some(pending) = self.pending.remove(&message.transaction) else {
            return;
        };
        if message.class == Class::Success {
            // Answers to authenticated requests are signed with the same
            // key; one that is not is not the server's.
            if let Some(key) = self.key
                && message.get(attr::MESSAGE_INTEGRITY).is_some()
                && !integrity_ok(bytes, &key)
            {
                self.events.push_back(Event::Note(
                    "a TURN answer failed its integrity check".into(),
                ));
                self.pending.insert(message.transaction, pending);
                return;
            }
            self.succeeded(&pending.purpose, &message, now);
            return;
        }
        let (code, reason) = message
            .get(attr::ERROR_CODE)
            .and_then(read_error)
            .unwrap_or((0, String::new()));
        // 401: credentials wanted (the first answer to every request
        // before the realm is known); 438: the nonce went stale.
        if (code == 401 || code == 438) && !pending.authenticated_retry {
            if let Some(realm) = message.get(attr::REALM) {
                let realm = String::from_utf8_lossy(realm).into_owned();
                self.key = Some(long_term_key(&self.username, &realm, &self.password));
                self.realm = Some(realm);
            }
            if let Some(nonce) = message.get(attr::NONCE) {
                self.nonce = Some(nonce.to_vec());
            }
            if self.key.is_some() && self.nonce.is_some() {
                let id = self.send_request(pending.purpose, now, true);
                if code == 438 {
                    self.events
                        .push_back(Event::Note(format!("TURN nonce renewed ({id:02x?})")));
                }
                return;
            }
        }
        if code == 300 && pending.purpose == Purpose::Allocate {
            self.alternate = message.get(attr::ALTERNATE_SERVER).and_then(read_plain_address);
        }
        self.refused(&pending.purpose, code, &reason);
    }

    /// The server a 300 Try Alternate sent us to, if one did: the
    /// allocation failed here and should be asked for there.
    pub fn alternate(&self) -> Option<SocketAddr> {
        self.alternate
    }

    fn request(&mut self, purpose: Purpose, now: Instant) {
        self.send_request(purpose, now, false);
    }

    /// Builds, signs (once a key is known), queues and remembers a request.
    fn send_request(&mut self, purpose: Purpose, now: Instant, retry: bool) -> TransactionId {
        let transaction = (self.next_transaction)();
        let mut message = match &purpose {
            Purpose::Allocate => Message::new(Method::Allocate, Class::Request, transaction)
                // UDP (17), the only relay WebRTC uses.
                .with(attr::REQUESTED_TRANSPORT, vec![17, 0, 0, 0])
                .with(attr::LIFETIME, ALLOCATION_LIFETIME.to_be_bytes().to_vec()),
            Purpose::Refresh { lifetime } => {
                Message::new(Method::Refresh, Class::Request, transaction)
                    .with(attr::LIFETIME, lifetime.to_be_bytes().to_vec())
            }
            Purpose::Permission(ips) => {
                let mut message =
                    Message::new(Method::CreatePermission, Class::Request, transaction);
                for ip in ips {
                    message = message.with(
                        attr::XOR_PEER_ADDRESS,
                        xor_address(SocketAddr::new(*ip, 0), &transaction),
                    );
                }
                message
            }
        };
        if let (Some(key), Some(realm), Some(nonce)) = (self.key, &self.realm, &self.nonce) {
            message = message
                .with(attr::USERNAME, self.username.as_bytes().to_vec())
                .with(attr::REALM, realm.as_bytes().to_vec())
                .with(attr::NONCE, nonce.clone());
            let bytes = message.encode(Some(&key));
            self.remember(transaction, purpose, bytes, now, retry);
        } else {
            let bytes = message.encode(None);
            self.remember(transaction, purpose, bytes, now, retry);
        }
        transaction
    }

    fn remember(
        &mut self,
        transaction: TransactionId,
        purpose: Purpose,
        bytes: Vec<u8>,
        now: Instant,
        retry: bool,
    ) {
        self.transmit.push_back(bytes.clone());
        self.pending.insert(
            transaction,
            Pending {
                purpose,
                bytes,
                sends: 1,
                next_send: now + FIRST_RTO,
                wait: FIRST_RTO,
                authenticated_retry: retry,
            },
        );
    }

    fn succeeded(&mut self, purpose: &Purpose, message: &Message, now: Instant) {
        match purpose {
            Purpose::Allocate => {
                let relayed = message
                    .get(attr::XOR_RELAYED_ADDRESS)
                    .and_then(|v| read_xor_address(v, &message.transaction));
                let Some(relayed) = relayed else {
                    self.fail("the Allocate answer has no relayed address".into());
                    return;
                };
                let mapped = message
                    .get(attr::XOR_MAPPED_ADDRESS)
                    .and_then(|v| read_xor_address(v, &message.transaction));
                let lifetime = message
                    .get(attr::LIFETIME)
                    .filter(|v| v.len() == 4)
                    .map_or(ALLOCATION_LIFETIME, be32);
                self.state = State::Allocated;
                self.relayed = Some(relayed);
                self.refresh_at = Some(now + refresh_after(lifetime));
                self.events.push_back(Event::Allocated {
                    relayed,
                    mapped,
                    lifetime,
                });
            }
            Purpose::Refresh { lifetime: 0 } => {
                self.state = State::Idle;
                self.relayed = None;
                self.refresh_at = None;
                self.events
                    .push_back(Event::Note("the TURN allocation is released".into()));
            }
            Purpose::Refresh { .. } => {
                let lifetime = message
                    .get(attr::LIFETIME)
                    .filter(|v| v.len() == 4)
                    .map_or(ALLOCATION_LIFETIME, be32);
                self.refresh_at = Some(now + refresh_after(lifetime));
            }
            Purpose::Permission(ips) => {
                for ip in ips {
                    self.permissions.insert(*ip, now + PERMISSION_REFRESH);
                    self.events.push_back(Event::Permitted(*ip));
                }
            }
        }
    }

    fn refused(&mut self, purpose: &Purpose, code: u16, reason: &str) {
        match purpose {
            Purpose::Allocate => self.fail(format!("Allocate refused: {code} {reason}")),
            Purpose::Refresh { lifetime: 0 } => {
                self.state = State::Idle;
                self.events.push_back(Event::Note(format!(
                    "releasing the TURN allocation: {code} {reason}"
                )));
            }
            Purpose::Refresh { .. } => self.fail(format!("Refresh refused: {code} {reason}")),
            Purpose::Permission(ips) => self.events.push_back(Event::Note(format!(
                "CreatePermission for {ips:?} refused: {code} {reason}"
            ))),
        }
    }

    fn unanswered(&mut self, purpose: &Purpose) {
        match purpose {
            Purpose::Allocate => self.fail("the TURN server did not answer Allocate".into()),
            Purpose::Refresh { lifetime: 0 } => self.state = State::Idle,
            Purpose::Refresh { .. } => self.fail("the TURN server did not answer Refresh".into()),
            Purpose::Permission(ips) => self.events.push_back(Event::Note(format!(
                "the TURN server did not answer CreatePermission for {ips:?}"
            ))),
        }
    }

    fn fail(&mut self, why: String) {
        self.state = State::Failed;
        self.relayed = None;
        self.pending.clear();
        self.events.push_back(Event::Failed(why));
    }
}

/// When to renew an allocation of `lifetime` seconds: a minute early, or
/// halfway for short ones.
fn refresh_after(lifetime: u32) -> Duration {
    let lifetime = u64::from(lifetime);
    Duration::from_secs(if lifetime > 120 {
        lifetime - 60
    } else {
        lifetime / 2
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(text: &str) -> Vec<u8> {
        let digits: String = text.split_whitespace().collect();
        (0..digits.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&digits[i..i + 2], 16).expect("hex"))
            .collect()
    }

    /// RFC 5769 §2.4: a request with long-term authentication. Its
    /// username and password are the SASLprep'd forms the RFC gives.
    const LONG_TERM: &str = "00 01 00 60 21 12 a4 42 78 ad 34 33 c6 ad 72 c0 29 da 41 2e
        00 06 00 12 e3 83 9e e3 83 88 e3 83 aa e3 83 83 e3 82 af e3 82 b9 00 00
        00 15 00 1c 66 2f 2f 34 39 39 6b 39 35 34 64 36 4f 4c 33 34 6f 4c 39 46 53 54 76 79 36 34 73 41
        00 14 00 0b 65 78 61 6d 70 6c 65 2e 6f 72 67 00
        00 08 00 14 f6 70 24 65 6d d6 4a 3e 02 b8 e0 71 2e 85 c9 a2 8c a8 96 66";

    #[test]
    fn the_rfc_5769_long_term_vector_checks_out() {
        let bytes = hex(LONG_TERM);
        let key = long_term_key("マトリックス", "example.org", "TheMatrIX");
        assert!(integrity_ok(&bytes, &key));
        assert!(!integrity_ok(
            &bytes,
            &long_term_key("x", "example.org", "TheMatrIX")
        ));
        // Built again from its parts, it comes out byte for byte.
        let message = Message::decode(&bytes).expect("decodes");
        assert_eq!(message.method(), Some(Method::Binding));
        assert_eq!(message.class, Class::Request);
        let rebuilt = Message {
            attributes: message
                .attributes
                .iter()
                .filter(|(k, _)| *k != attr::MESSAGE_INTEGRITY)
                .cloned()
                .collect(),
            ..message
        };
        assert_eq!(rebuilt.encode(Some(&key)), bytes);
    }

    #[test]
    fn the_rfc_5769_short_term_vector_checks_out() {
        // §2.1, up to its MESSAGE-INTEGRITY; the key is the password.
        let bytes = hex("00 01 00 58 21 12 a4 42 b7 e7 a7 01 bc 34 d6 86 fa 87 df ae
             80 22 00 10 53 54 55 4e 20 74 65 73 74 20 63 6c 69 65 6e 74
             00 24 00 04 6e 00 01 ff
             80 29 00 08 93 2f f9 b1 51 26 3b 36
             00 06 00 09 65 76 74 6a 3a 68 36 76 59 20 20 20
             00 08 00 14 9a ea a7 0c bf d8 cb 56 78 1e f2 b5 b2 d3 f2 49 c1 b5 71 a2
             80 28 00 04 e5 7a 3b cf");
        assert!(integrity_ok(&bytes, b"VOkJxbRl1RmTxUk/WvJxBt"));
    }

    #[test]
    fn xor_addresses_match_the_rfc() {
        // RFC 5769 §2.2: 192.0.2.1:32853.
        let transaction: TransactionId = hex("b7 e7 a7 01 bc 34 d6 86 fa 87 df ae")
            .try_into()
            .expect("12 bytes");
        let value = hex("00 01 a1 47 e1 12 a6 43");
        let address: SocketAddr = "192.0.2.1:32853".parse().expect("an address");
        assert_eq!(read_xor_address(&value, &transaction), Some(address));
        assert_eq!(xor_address(address, &transaction), value);
        // §2.3: the same over IPv6.
        let v6: SocketAddr = "[2001:db8:1234:5678:11:2233:4455:6677]:32853"
            .parse()
            .expect("an address");
        let encoded = xor_address(v6, &transaction);
        assert_eq!(
            encoded,
            hex("00 02 a1 47 01 13 a9 fa a5 d3 f1 79 bc 25 f4 b5 be d2 b9 d9")
        );
        assert_eq!(read_xor_address(&encoded, &transaction), Some(v6));
    }

    #[test]
    fn message_types_interleave_the_class() {
        assert_eq!(message_type(Method::Binding, Class::Request), 0x0001);
        assert_eq!(message_type(Method::Allocate, Class::Request), 0x0003);
        assert_eq!(message_type(Method::Allocate, Class::Success), 0x0103);
        assert_eq!(message_type(Method::Allocate, Class::Error), 0x0113);
        assert_eq!(message_type(Method::Send, Class::Indication), 0x0016);
        assert_eq!(message_type(Method::Data, Class::Indication), 0x0017);
        assert_eq!(split_type(0x0113), (Method::Allocate as u16, Class::Error));
        assert_eq!(split_type(0x0017), (Method::Data as u16, Class::Indication));
    }

    #[test]
    fn turn_uris_read_as_chime_sends_them() {
        assert_eq!(
            parse_uri("turn:1.2.3.4:3478?transport=udp"),
            Some(Server {
                host: "1.2.3.4".into(),
                port: 3478,
                transport: Transport::Udp
            })
        );
        assert_eq!(
            parse_uri("turns:turn.example:443?transport=tcp").map(|s| s.transport),
            Some(Transport::Tls)
        );
        assert_eq!(
            parse_uri("turn:t.example?transport=tcp"),
            Some(Server {
                host: "t.example".into(),
                port: 3478,
                transport: Transport::Tcp
            })
        );
        assert_eq!(
            parse_uri("turn:[2001:db8::1]:3478").map(|s| s.host),
            Some("2001:db8::1".into())
        );
        assert_eq!(parse_uri("stun:s.example:3478"), None);
        assert_eq!(parse_uri("turn:"), None);
        let order: Vec<Transport> = by_preference(&[
            "turns:a:443?transport=tcp".into(),
            "turn:b:3478?transport=tcp".into(),
            "turn:c:3478?transport=udp".into(),
        ])
        .into_iter()
        .map(|s| s.transport)
        .collect();
        assert_eq!(order, [Transport::Udp, Transport::Tls, Transport::Tcp]);
    }

    #[test]
    fn streams_are_cut_into_messages() {
        let stun = Message::new(Method::Binding, Class::Request, [1; 12]).encode(None);
        let mut buffer = stun.clone();
        // ChannelData: channel 0x4000, 3 bytes, padded to 4.
        buffer.extend_from_slice(&[0x40, 0x00, 0x00, 0x03, 9, 9, 9, 0]);
        buffer.extend_from_slice(&stun[..10]);
        assert_eq!(split_stream(&mut buffer), Some(stun.clone()));
        assert_eq!(
            split_stream(&mut buffer),
            Some(vec![0x40, 0x00, 0x00, 0x03, 9, 9, 9, 0])
        );
        assert_eq!(split_stream(&mut buffer), None, "half a message waits");
        buffer.extend_from_slice(&stun[10..]);
        assert_eq!(split_stream(&mut buffer), Some(stun));
        assert!(buffer.is_empty());
    }

    /// A pretend TURN server's answers to the client's requests.
    fn answer(
        request: &[u8],
        class: Class,
        attributes: Vec<(u16, Vec<u8>)>,
        key: Option<&[u8]>,
    ) -> Vec<u8> {
        let request = Message::decode(request).expect("a request");
        Message {
            method: request.method,
            class,
            transaction: request.transaction,
            attributes,
        }
        .encode(key)
    }

    fn counter() -> Box<dyn FnMut() -> TransactionId + Send> {
        let mut n = 0u8;
        Box::new(move || {
            n += 1;
            [n; 12]
        })
    }

    #[test]
    fn a_try_alternate_names_the_server_to_ask_instead() {
        let now = Instant::now();
        let mut client = Client::with_transactions(Transport::Udp, "user", "pass", counter());
        client.allocate(now);
        let first = client.poll_transmit().expect("an Allocate");
        let mut error = vec![0, 0, 3, 0];
        error.extend_from_slice(b"Try Alternate");
        client.handle_input(
            &answer(
                &first,
                Class::Error,
                vec![
                    (attr::ERROR_CODE, error),
                    (attr::ALTERNATE_SERVER, vec![0, 1, 0x0d, 0x96, 203, 0, 113, 9]),
                ],
                None,
            ),
            now,
        );
        assert_eq!(
            client.alternate(),
            Some("203.0.113.9:3478".parse().expect("an address"))
        );
        assert!(matches!(client.poll_event(), Some(Event::Failed(_))));
    }

    #[test]
    fn an_allocation_authenticates_and_relays() {
        let now = Instant::now();
        let mut client = Client::with_transactions(Transport::Udp, "user", "pass", counter());
        client.allocate(now);
        let first = client.poll_transmit().expect("an Allocate");
        let parsed = Message::decode(&first).expect("STUN");
        assert_eq!(parsed.method(), Some(Method::Allocate));
        assert_eq!(
            parsed.get(attr::REQUESTED_TRANSPORT),
            Some(&[17, 0, 0, 0][..])
        );
        assert_eq!(parsed.get(attr::MESSAGE_INTEGRITY), None);

        // 401 with the realm and nonce.
        let mut error = vec![0, 0, 4, 1];
        error.extend_from_slice(b"Unauthorized");
        client.handle_input(
            &answer(
                &first,
                Class::Error,
                vec![
                    (attr::ERROR_CODE, error),
                    (attr::REALM, b"chime".to_vec()),
                    (attr::NONCE, b"n0nce".to_vec()),
                ],
                None,
            ),
            now,
        );
        let second = client.poll_transmit().expect("an authenticated Allocate");
        let key = long_term_key("user", "chime", "pass");
        assert!(integrity_ok(&second, &key));
        let parsed = Message::decode(&second).expect("STUN");
        assert_eq!(parsed.get(attr::USERNAME), Some(&b"user"[..]));
        assert_eq!(parsed.get(attr::NONCE), Some(&b"n0nce"[..]));

        let relayed: SocketAddr = "203.0.113.5:50000".parse().expect("an address");
        let mapped: SocketAddr = "198.51.100.7:40000".parse().expect("an address");
        let transaction = parsed.transaction;
        client.handle_input(
            &answer(
                &second,
                Class::Success,
                vec![
                    (
                        attr::XOR_RELAYED_ADDRESS,
                        xor_address(relayed, &transaction),
                    ),
                    (attr::XOR_MAPPED_ADDRESS, xor_address(mapped, &transaction)),
                    (attr::LIFETIME, 600u32.to_be_bytes().to_vec()),
                ],
                Some(&key),
            ),
            now,
        );
        assert_eq!(
            client.poll_event(),
            Some(Event::Allocated {
                relayed,
                mapped: Some(mapped),
                lifetime: 600
            })
        );

        // A send to a new peer asks for its permission first.
        let peer: SocketAddr = "192.0.2.9:3478".parse().expect("an address");
        client.send_to(peer, b"hello", now);
        let permission = client.poll_transmit().expect("CreatePermission");
        let parsed = Message::decode(&permission).expect("STUN");
        assert_eq!(parsed.method(), Some(Method::CreatePermission));
        assert!(integrity_ok(&permission, &key));
        let send = Message::decode(&client.poll_transmit().expect("a Send")).expect("STUN");
        assert_eq!(send.class, Class::Indication);
        assert_eq!(send.get(attr::DATA), Some(&b"hello"[..]));
        assert_eq!(
            send.get(attr::XOR_PEER_ADDRESS)
                .and_then(|v| read_xor_address(v, &send.transaction)),
            Some(peer)
        );
        client.handle_input(
            &answer(&permission, Class::Success, vec![], Some(&key)),
            now,
        );
        assert_eq!(client.poll_event(), Some(Event::Permitted(peer.ip())));
        assert!(client.permitted(peer.ip()));

        // A Data indication comes out as the peer's bytes.
        let data = Message::new(Method::Data, Class::Indication, [9; 12])
            .with(attr::XOR_PEER_ADDRESS, xor_address(peer, &[9; 12]))
            .with(attr::DATA, b"back".to_vec())
            .encode(None);
        client.handle_input(&data, now);
        assert_eq!(
            client.poll_event(),
            Some(Event::Data {
                peer,
                data: b"back".to_vec()
            })
        );

        // Closing asks for lifetime zero.
        client.close(now);
        let release = Message::decode(&client.poll_transmit().expect("a Refresh")).expect("STUN");
        assert_eq!(release.method(), Some(Method::Refresh));
        assert_eq!(release.get(attr::LIFETIME), Some(&[0, 0, 0, 0][..]));
    }

    #[test]
    fn unanswered_requests_are_sent_again_then_given_up() {
        let start = Instant::now();
        let mut client = Client::with_transactions(Transport::Udp, "u", "p", counter());
        client.allocate(start);
        let first = client.poll_transmit().expect("an Allocate");
        let mut now = start;
        let mut sends = 1;
        while let Some(at) = client.poll_timeout() {
            now = at;
            client.handle_timeout(now);
            while let Some(again) = client.poll_transmit() {
                assert_eq!(again, first);
                sends += 1;
            }
            if let Some(event) = client.poll_event() {
                assert!(matches!(event, Event::Failed(_)), "{event:?}");
                break;
            }
        }
        assert_eq!(sends, SENDS);
        assert!(now - start > Duration::from_secs(10));
        assert_eq!(client.relayed(), None);
    }

    #[test]
    fn a_refusal_after_credentials_ends_the_allocation() {
        let now = Instant::now();
        let mut client = Client::with_transactions(Transport::Tls, "u", "p", counter());
        client.allocate(now);
        let first = client.poll_transmit().expect("an Allocate");
        let unauthorized = |request: &[u8]| {
            answer(
                request,
                Class::Error,
                vec![
                    (attr::ERROR_CODE, vec![0, 0, 4, 1]),
                    (attr::REALM, b"r".to_vec()),
                    (attr::NONCE, b"n".to_vec()),
                ],
                None,
            )
        };
        client.handle_input(&unauthorized(&first), now);
        let second = client.poll_transmit().expect("again, signed");
        client.handle_input(&unauthorized(&second), now);
        assert!(matches!(client.poll_event(), Some(Event::Failed(why)) if why.contains("401")));
        // Over TLS nothing is ever sent twice.
        assert_eq!(client.poll_timeout(), None);
    }

    #[test]
    fn the_password_never_prints() {
        let client = Client::new(Transport::Udp, "user", "s3cret");
        assert!(!format!("{client:?}").contains("s3cret"));
    }
}
