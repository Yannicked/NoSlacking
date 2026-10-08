//! One NoSlacking per user.
//!
//! The first launch listens on a loopback port and writes the port and a
//! random secret to the state directory. A later launch (the desktop
//! opening a `noslacking://` sign-in link, or the user starting the app
//! again) connects, hands over its link, and exits; the running one
//! handles the link and raises its window.
//!
//! Only this user can read the file, and the secret itself never crosses
//! the wire: each side proves it knows it by answering the other's random
//! challenge with a keyed hash. The later launch sends its link only once
//! the listener has proved itself, so a program that took the port after
//! a crash left the file behind learns nothing. The listener acts only on
//! a launch that proved itself too, and the running app only accepts
//! sign-in callbacks it is waiting for.
//!
//! The exchange, one line each:
//!
//! 1. launch → listener: the launch's challenge.
//! 2. listener → launch: its proof for that challenge, and its own
//!    challenge.
//! 3. launch → listener: its proof for both challenges, then the request.
//! 4. listener → launch: `ok`.

use std::io::Write as _;
use std::net::{Ipv4Addr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use rand::Rng as _;
use sha2::Digest as _;

use crate::text::hex;

/// What a later launch asked for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Request {
    /// Bring the window forward.
    Show,
    /// Handle this link (a sign-in callback).
    Open(String),
}

pub enum Outcome {
    /// This is the only instance; keep the guard for the app's lifetime.
    Primary(Guard),
    /// Another instance took the request.
    Forwarded,
}

/// Removes the instance file when the primary instance exits.
pub struct Guard {
    file: PathBuf,
}

impl Drop for Guard {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.file);
    }
}

fn encode(request: &Request) -> String {
    match request {
        Request::Show => "show".to_owned(),
        Request::Open(url) => format!("open {}", url.replace(['\n', '\r'], "")),
    }
}

fn decode(line: &str) -> Option<Request> {
    let line = line.trim_end();
    if line == "show" {
        return Some(Request::Show);
    }
    line.strip_prefix("open ")
        .map(|url| Request::Open(url.to_owned()))
}

/// Hands `request` to a running instance, or becomes the running instance
/// and calls `handle` for every later request.
pub fn acquire(
    file: &Path,
    request: Request,
    handle: impl Fn(Request) + Send + Sync + 'static,
) -> std::io::Result<Outcome> {
    if forward(file, &request).is_ok() {
        return Ok(Outcome::Forwarded);
    }
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))?;
    let port = listener.local_addr()?.port();
    let secret: Arc<str> = random_hex().into();
    write_private(file, &format!("{port}\n{secret}\n"))?;
    let handle: Arc<dyn Fn(Request) + Send + Sync> = Arc::new(handle);
    let serving = Arc::new(AtomicUsize::new(0));
    std::thread::Builder::new()
        .name("noslacking-instance".into())
        .spawn(move || {
            for stream in listener.incoming().flatten() {
                // Each connection on a thread of its own, so one that is
                // slow or never finishes cannot keep a real launch waiting.
                // Past a few at once the rest are turned away rather than
                // let anyone pile up threads.
                let Some(slot) = Slot::take(&serving) else {
                    continue;
                };
                let secret = secret.clone();
                let handle = handle.clone();
                let spawned = std::thread::Builder::new()
                    .name("noslacking-launch".into())
                    .spawn(move || {
                        let _slot = slot;
                        if let Some(request) = serve(stream, &secret) {
                            handle(request);
                        }
                    });
                if let Err(error) = spawned {
                    log::debug!("a later launch was not heard: {error}");
                }
            }
        })?;
    // The first instance handles its own request too.
    Ok(Outcome::Primary(Guard {
        file: file.to_path_buf(),
    }))
}

/// The most later launches heard at once.
const MAX_SERVING: usize = 16;

/// Counts a connection being served for as long as it lives.
struct Slot(Arc<AtomicUsize>);

impl Slot {
    fn take(count: &Arc<AtomicUsize>) -> Option<Self> {
        count
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |n| {
                (n < MAX_SERVING).then_some(n + 1)
            })
            .ok()
            .map(|_| Self(count.clone()))
    }
}

impl Drop for Slot {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}

/// The listener's side of the exchange: the request of a launch that
/// proved it knows `secret`, or `None`.
fn serve(mut stream: TcpStream, secret: &str) -> Option<Request> {
    // A short deadline and a small budget, so a connection that never
    // finishes lets go of its thread soon.
    let _ = stream.set_read_timeout(Some(Duration::from_millis(500)));
    let _ = stream.set_write_timeout(Some(Duration::from_secs(1)));
    let deadline = Instant::now() + Duration::from_secs(2);
    let theirs = read_line(&mut stream, deadline).filter(|c| is_challenge(c))?;
    let ours = random_hex();
    let answer = format!("{} {ours}\n", proof(secret, Role::Listener, &theirs, ""));
    stream.write_all(answer.as_bytes()).ok()?;
    let proved = read_line(&mut stream, deadline)?;
    if !same(&proved, &proof(secret, Role::Launch, &ours, &theirs)) {
        return None;
    }
    let request = read_line(&mut stream, deadline).and_then(|l| decode(&l))?;
    let _ = stream.write_all(b"ok\n");
    Some(request)
}

/// Who a proof is from. Each side's proofs are keyed apart, so the
/// listener's answer to a challenge can never pass for a launch's.
#[derive(Clone, Copy)]
enum Role {
    Listener,
    Launch,
}

/// Proof of knowing `secret` for these challenges: HMAC-SHA256 over the
/// role and the challenges, in hex.
fn proof(secret: &str, role: Role, challenge: &str, other: &str) -> String {
    let role = match role {
        Role::Listener => "listener",
        Role::Launch => "launch",
    };
    hex(&hmac_sha256(
        secret.as_bytes(),
        format!("noslacking-instance {role} {challenge} {other}").as_bytes(),
    ))
}

/// HMAC-SHA256 (RFC 2104), built on the SHA-256 the app already links.
fn hmac_sha256(key: &[u8], message: &[u8]) -> [u8; 32] {
    const BLOCK: usize = 64;
    let mut block = [0u8; BLOCK];
    if key.len() > BLOCK {
        block[..32].copy_from_slice(&sha2::Sha256::digest(key));
    } else {
        block[..key.len()].copy_from_slice(key);
    }
    let pad = |byte: u8| block.map(|b| b ^ byte);
    let inner = sha2::Sha256::new()
        .chain_update(pad(0x36))
        .chain_update(message)
        .finalize();
    sha2::Sha256::new()
        .chain_update(pad(0x5c))
        .chain_update(inner)
        .finalize()
        .into()
}

/// Whether two proofs match, taking as long whichever byte differs, so
/// timing cannot tell a guess how close it came.
fn same(a: &str, b: &str) -> bool {
    a.len() == b.len()
        && a.bytes()
            .zip(b.bytes())
            .fold(0u8, |diff, (x, y)| diff | (x ^ y))
            == 0
}

/// Whether `line` looks like a challenge [`random_hex`] makes.
fn is_challenge(line: &str) -> bool {
    line.len() == 32 && line.bytes().all(|b| b.is_ascii_hexdigit())
}

/// 16 random bytes in hex: the secret, and each challenge.
fn random_hex() -> String {
    let mut bytes = [0u8; 16];
    rand::rng().fill_bytes(&mut bytes);
    hex(&bytes)
}

/// The longest line either side sends: a challenge, a proof, or a sign-in
/// link.
const MAX_LINE: usize = 4096;

/// One line from `stream` (a connection, read one byte at a time so
/// nothing past the line is consumed), without its newline, or `None` when it is too
/// long, not UTF-8, or does not arrive before `deadline`.
fn read_line(stream: &mut impl std::io::Read, deadline: Instant) -> Option<String> {
    let mut line = Vec::new();
    let mut byte = [0u8; 1];
    while Instant::now() < deadline && line.len() <= MAX_LINE {
        match stream.read(&mut byte) {
            Ok(0) => break,
            Ok(_) if byte[0] == b'\n' => return String::from_utf8(line).ok(),
            Ok(_) => line.push(byte[0]),
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) => {}
            Err(_) => return None,
        }
    }
    None
}

/// The later launch's side of the exchange: hands `request` over once the
/// listener has proved it is the NoSlacking that wrote `file`.
fn forward(file: &Path, request: &Request) -> std::io::Result<()> {
    let contents = std::fs::read_to_string(file)?;
    let mut lines = contents.lines();
    let port: u16 = lines
        .next()
        .and_then(|p| p.parse().ok())
        .ok_or(std::io::ErrorKind::InvalidData)?;
    let secret = lines.next().ok_or(std::io::ErrorKind::InvalidData)?;
    let mut stream = TcpStream::connect_timeout(
        &(Ipv4Addr::LOCALHOST, port).into(),
        Duration::from_millis(500),
    )?;
    stream.set_read_timeout(Some(Duration::from_millis(500)))?;
    stream.set_write_timeout(Some(Duration::from_secs(1)))?;
    let deadline = Instant::now() + Duration::from_secs(2);
    let refused = || std::io::Error::from(std::io::ErrorKind::ConnectionRefused);
    let ours = random_hex();
    stream.write_all(format!("{ours}\n").as_bytes())?;
    let answer = read_line(&mut stream, deadline).ok_or_else(refused)?;
    let (proved, theirs) = answer.split_once(' ').ok_or_else(refused)?;
    // Whatever listens there now may not be NoSlacking (the file outlives
    // a crash); it hears nothing more unless it knows the secret.
    if !is_challenge(theirs) || !same(proved, &proof(secret, Role::Listener, &ours, "")) {
        return Err(refused());
    }
    let proof = proof(secret, Role::Launch, theirs, &ours);
    stream.write_all(format!("{proof}\n{}\n", encode(request)).as_bytes())?;
    match read_line(&mut stream, deadline) {
        Some(answer) if answer == "ok" => Ok(()),
        _ => Err(refused()),
    }
}

fn write_private(file: &Path, contents: &str) -> std::io::Result<()> {
    if let Some(parent) = file.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    let mut opened = options.open(file)?;
    // `mode` applies only when the file is created; tighten a file left
    // by an older version or another tool too.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        opened.set_permissions(std::fs::Permissions::from_mode(0o600))?;
    }
    opened.write_all(contents.as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requests_survive_the_wire_format() {
        for request in [
            Request::Show,
            Request::Open("noslacking://oauth/callback?code=1&state=2".into()),
        ] {
            assert_eq!(decode(&encode(&request)), Some(request));
        }
        assert_eq!(decode("open a\n"), Some(Request::Open("a".into())));
        assert_eq!(decode("rm -rf"), None);
    }

    #[test]
    fn hmac_matches_the_rfc_vectors() {
        // RFC 4231, test cases 2 and 6 (a key longer than a block).
        assert_eq!(
            hex(&hmac_sha256(b"Jefe", b"what do ya want for nothing?")),
            "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843"
        );
        assert_eq!(
            hex(&hmac_sha256(
                &[0xaa; 131],
                b"Test Using Larger Than Block-Size Key - Hash Key First"
            )),
            "60e431591ee0b67f0d8a26aacbf5b77f8e0bc6213728c5140546040f0ee37f54"
        );
    }

    #[test]
    fn proofs_need_the_secret_and_the_role() {
        let challenge = random_hex();
        assert!(is_challenge(&challenge));
        assert_ne!(challenge, random_hex());
        let listener = proof("s3cret", Role::Listener, &challenge, "");
        assert!(same(
            &listener,
            &proof("s3cret", Role::Listener, &challenge, "")
        ));
        assert!(!same(
            &listener,
            &proof("other", Role::Listener, &challenge, "")
        ));
        assert!(!same(
            &listener,
            &proof("s3cret", Role::Listener, &random_hex(), "")
        ));
        // The listener's answer to a challenge never passes as a launch's.
        assert!(!same(
            &listener,
            &proof("s3cret", Role::Launch, &challenge, "")
        ));
        assert!(!same(&listener, &listener[1..]));
        assert!(!is_challenge("show"));
        assert!(!is_challenge(&"g".repeat(32)));
    }

    #[test]
    fn a_launch_hands_over_only_to_a_listener_that_knows_the_secret() {
        let dir = crate::paths::TestDir::new("instance");
        let file = dir.0.join("instance");
        let (heard, hear) = std::sync::mpsc::channel();
        let Outcome::Primary(_guard) = acquire(&file, Request::Show, move |request| {
            let _ = heard.send(request);
        })
        .expect("primary") else {
            panic!("nothing else was running");
        };
        let link = Request::Open("noslacking://oauth/callback?code=1&state=2".into());
        forward(&file, &link).expect("handed over");
        assert_eq!(hear.recv_timeout(Duration::from_secs(5)), Ok(link));

        // A stale file whose port someone else now holds: that listener
        // cannot prove itself, so it never sees the request.
        let impostor = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).expect("bind");
        let port = impostor.local_addr().expect("port").port();
        let stale = dir.0.join("stale");
        write_private(&stale, &format!("{port}\n{}\n", random_hex())).expect("file");
        let seen = std::thread::spawn(move || {
            let (mut stream, _) = impostor.accept().expect("accept");
            let deadline = Instant::now() + Duration::from_secs(5);
            let challenge = read_line(&mut stream, deadline).expect("challenge");
            let answer = format!("{} {}\n", "0".repeat(64), random_hex());
            stream.write_all(answer.as_bytes()).expect("answer");
            let mut rest = String::new();
            let _ = std::io::Read::read_to_string(&mut stream, &mut rest);
            (challenge, rest)
        });
        assert!(forward(&stale, &Request::Open("noslacking://x".into())).is_err());
        let (challenge, rest) = seen.join().expect("impostor");
        assert!(is_challenge(&challenge));
        assert_eq!(rest, "", "nothing after a failed proof");
    }

    #[test]
    fn lines_are_bounded() {
        let mut wire = b"abc\n".to_vec();
        wire.extend_from_slice(&[b'x'; MAX_LINE + 10]);
        wire.extend_from_slice(b"\nnext\n");
        let mut stream = std::io::Cursor::new(wire);
        let deadline = Instant::now() + Duration::from_secs(5);
        assert_eq!(read_line(&mut stream, deadline).as_deref(), Some("abc"));
        assert_eq!(read_line(&mut stream, deadline), None);
        assert_eq!(
            read_line(&mut std::io::Cursor::new(b"no newline"), deadline),
            None
        );
        let past = Instant::now() - Duration::from_secs(1);
        assert_eq!(read_line(&mut std::io::Cursor::new(b"late\n"), past), None);
    }
}
