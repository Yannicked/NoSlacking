//! One NoSlacking per user.
//!
//! The first launch listens on a loopback port and writes the port and a
//! random secret to the state directory. A later launch (the desktop
//! opening a `noslacking://` sign-in link, or the user starting the app
//! again) connects, sends the secret and its link, and exits; the running
//! one handles the link and raises its window.
//!
//! Only this user can read the file, so only this user's processes can talk
//! to the listener, and the running app only accepts sign-in callbacks it
//! is waiting for.

use std::io::{BufRead as _, BufReader, Write as _};
use std::net::{Ipv4Addr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use rand::Rng as _;

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
    handle: impl Fn(Request) + Send + 'static,
) -> std::io::Result<Outcome> {
    if forward(file, &request).is_ok() {
        return Ok(Outcome::Forwarded);
    }
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))?;
    let port = listener.local_addr()?.port();
    let mut secret = [0u8; 16];
    rand::rng().fill_bytes(&mut secret);
    let secret: String = secret.iter().map(|b| format!("{b:02x}")).collect();
    write_private(file, &format!("{port}\n{secret}\n"))?;
    std::thread::Builder::new()
        .name("noslacking-instance".into())
        .spawn(move || {
            for mut stream in listener.incoming().flatten() {
                // Connections are served one at a time, so a slow or chatty
                // client gets a short deadline and a small budget.
                let _ = stream.set_read_timeout(Some(Duration::from_millis(500)));
                let _ = stream.set_write_timeout(Some(Duration::from_secs(1)));
                let deadline = Instant::now() + Duration::from_secs(2);
                let Some(first) = read_line(&mut stream, deadline) else {
                    continue;
                };
                if first != secret {
                    continue;
                }
                if let Some(request) = read_line(&mut stream, deadline).and_then(|l| decode(&l)) {
                    let _ = stream.write_all(b"ok\n");
                    handle(request);
                }
            }
        })?;
    // The first instance handles its own request too.
    Ok(Outcome::Primary(Guard {
        file: file.to_path_buf(),
    }))
}

/// The longest line a later launch sends: the secret, or a sign-in link.
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
    stream.set_read_timeout(Some(Duration::from_secs(2)))?;
    stream.write_all(format!("{secret}\n{}\n", encode(request)).as_bytes())?;
    let mut answer = String::new();
    BufReader::new(stream).read_line(&mut answer)?;
    if answer.trim_end() == "ok" {
        Ok(())
    } else {
        Err(std::io::ErrorKind::ConnectionRefused.into())
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
