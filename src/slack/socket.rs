//! Socket Mode: Slack's events over a WebSocket, with no public endpoint.
//!
//! `apps.connections.open` (with the app-level `xapp` token) returns a
//! one-off `wss://` URL. Slack sends `hello`, then envelopes; every envelope
//! with an `envelope_id` must be acknowledged within three seconds or Slack
//! sends it again. `disconnect` means Slack is about to close this socket
//! (routinely, every few hours): open a fresh URL and carry on.
//!
//! One socket serves every workspace that installed the app; each event
//! names its team.

use std::time::Duration;

use futures_util::{SinkExt as _, StreamExt as _};
use tokio_tungstenite::tungstenite::Message as Frame;

use super::client::{self, SlackError};
use super::types;

/// No frame at all for this long means the connection is dead.
const SILENCE: Duration = Duration::from_secs(90);
const PING_EVERY: Duration = Duration::from_secs(30);
/// A connection that lasted this long resets the reconnect backoff.
const STABLE: Duration = Duration::from_secs(60);
/// The longest wait between reconnect attempts.
const MAX_BACKOFF: Duration = Duration::from_secs(60);

#[derive(Debug)]
pub enum SocketEvent {
    Connected,
    /// The connection dropped, for this reason; reconnecting.
    Disconnected(SlackError),
    /// The app token was refused, with Slack's code; reconnecting will not
    /// help.
    Rejected(String),
    Event {
        team: String,
        event: serde_json::Value,
    },
}

/// What one frame asks of the connection.
#[derive(Debug, PartialEq)]
pub enum Meaning {
    Hello,
    Reconnect(String),
    Event {
        team: String,
        event: serde_json::Value,
    },
    Ignore,
}

/// Reads one text frame: the acknowledgement to send, if any, and what it
/// means.
pub fn read_frame(text: &str) -> (Option<String>, Meaning) {
    let envelope: types::Envelope = match serde_json::from_str(text) {
        Ok(envelope) => envelope,
        Err(error) => {
            log::debug!("unreadable Socket Mode frame: {error}");
            return (None, Meaning::Ignore);
        }
    };
    let ack = envelope
        .envelope_id
        .as_ref()
        .map(|id| serde_json::json!({ "envelope_id": id }).to_string());
    let frame = match envelope.kind.as_str() {
        "hello" => Meaning::Hello,
        "disconnect" => Meaning::Reconnect(envelope.reason.unwrap_or_else(|| "disconnect".into())),
        "events_api" => match serde_json::from_value::<types::EventCallback>(envelope.payload) {
            Ok(callback) => {
                let team = callback
                    .authorizations
                    .iter()
                    .find_map(|a| a.team_id.clone())
                    .filter(|t| !t.is_empty())
                    .unwrap_or(callback.team_id);
                Meaning::Event {
                    team,
                    event: callback.event,
                }
            }
            Err(error) => {
                log::debug!("unreadable event payload: {error}");
                Meaning::Ignore
            }
        },
        _ => Meaning::Ignore,
    };
    (ack, frame)
}

/// How one connection ended.
#[derive(Debug, PartialEq)]
enum Ended {
    /// Slack asked for a fresh connection, for this reason.
    Reconnect(String),
    /// Slack asked to wait this long before opening another.
    RateLimited(Duration),
    Failed(SlackError),
}

/// Opens a Socket Mode URL with the app-level token.
async fn open_url(http: &reqwest::Client, app_token: &str) -> Result<String, Ended> {
    let network = |e: reqwest::Error| Ended::Failed(e.into());
    let response = http
        .post(format!("{}apps.connections.open", client::API))
        .bearer_auth(app_token)
        .send()
        .await
        .map_err(network)?;
    if let Some(wait) = client::retry_after(
        response.status().as_u16(),
        response.headers().get("retry-after"),
    ) {
        return Err(Ended::RateLimited(wait));
    }
    let bytes = response.bytes().await.map_err(network)?;
    let open: types::ConnectionsOpen = client::decode(&bytes).map_err(Ended::Failed)?;
    Ok(open.url)
}

/// How long to wait before the next connection, after one that ended as
/// `ended` having lasted `lasted`; updates the running `backoff`.
///
/// Only a routine `disconnect` from a connection that had been up a while
/// reconnects at once. One that comes straight after connecting backs off
/// like any failure, so a Slack that keeps asking cannot make a hot loop.
fn wait_before_next(ended: &Ended, lasted: Duration, backoff: &mut Duration) -> Duration {
    if lasted > STABLE {
        *backoff = Duration::from_secs(1);
        if matches!(ended, Ended::Reconnect(_)) {
            return Duration::ZERO;
        }
    }
    let wait = match ended {
        Ended::RateLimited(asked) => (*asked).max(*backoff),
        _ => *backoff,
    };
    *backoff = crate::retry::backoff(*backoff, MAX_BACKOFF, 1);
    wait
}

/// Keeps a Socket Mode connection open until `stop` changes, reporting
/// through `sink`.
pub async fn run(
    http: reqwest::Client,
    app_token: String,
    sink: impl Fn(SocketEvent) + Send + Sync,
    mut stop: tokio::sync::watch::Receiver<bool>,
) {
    let mut backoff = Duration::from_secs(1);
    loop {
        if *stop.borrow() {
            return;
        }
        let started = tokio::time::Instant::now();
        let outcome = tokio::select! {
            outcome = connection(&http, &app_token, &sink) => outcome,
            _ = stop.changed() => return,
        };
        let fatal = match &outcome {
            Ended::Failed(SlackError::Api(code)) | Ended::Reconnect(code) if is_fatal(code) => {
                Some(code.clone())
            }
            _ => None,
        };
        if let Some(code) = fatal {
            log::warn!("Socket Mode refused: {code}");
            sink(SocketEvent::Rejected(code));
            // Wait for a new token (the worker restarts this task).
            let _ = stop.changed().await;
            return;
        }
        match &outcome {
            Ended::Reconnect(reason) => log::info!("Socket Mode reconnecting: {reason}"),
            Ended::RateLimited(wait) => {
                log::warn!("Socket Mode rate limited for {wait:?}");
                sink(SocketEvent::Disconnected(SlackError::RateLimited));
            }
            Ended::Failed(error) => {
                log::warn!("Socket Mode connection lost: {error}");
                sink(SocketEvent::Disconnected(error.clone()));
            }
        }
        let wait = wait_before_next(&outcome, started.elapsed(), &mut backoff);
        tokio::select! {
            _ = tokio::time::sleep(wait) => {}
            _ = stop.changed() => return,
        }
    }
}

/// Whether reconnecting cannot help until a new app token is saved: Slack
/// refused the token (any sign-in error, or not an app-level token at
/// all), or Socket Mode was turned off for the app (`link_disabled`, sent
/// as a `disconnect` reason).
fn is_fatal(code: &str) -> bool {
    client::is_auth_code(code)
        || matches!(
            code,
            "not_allowed_token_type" | "invalid_token" | "link_disabled"
        )
}

/// One socket, from opening its URL to its end.
async fn connection(
    http: &reqwest::Client,
    app_token: &str,
    sink: &(impl Fn(SocketEvent) + Send + Sync),
) -> Ended {
    let url = match open_url(http, app_token).await {
        Ok(url) => url,
        Err(ended) => return ended,
    };
    match stream(&url, sink).await {
        Ok(reason) => Ended::Reconnect(reason),
        Err(error) => Ended::Failed(error),
    }
}

/// Reads one open socket until it ends. `Ok` carries why Slack asked us to
/// reconnect.
async fn stream(
    url: &str,
    sink: &(impl Fn(SocketEvent) + Send + Sync),
) -> Result<String, SlackError> {
    use tokio_tungstenite::tungstenite::client::IntoClientRequest as _;
    let request = url
        .into_client_request()
        .map_err(|e| SlackError::Decode(e.to_string()))?;
    let (mut socket, _) = super::net::websocket(request)
        .await
        .map_err(|e| SlackError::Network(e.to_string()))?;
    let mut ping = tokio::time::interval(PING_EVERY);
    ping.tick().await;
    let mut heard = tokio::time::Instant::now();
    loop {
        let frame = tokio::select! {
            frame = socket.next() => frame,
            () = tokio::time::sleep_until(heard + SILENCE) => {
                return Err(SlackError::Network("connection went silent".into()));
            }
            _ = ping.tick() => {
                socket
                    .send(Frame::Ping(Vec::new().into()))
                    .await
                    .map_err(|e| SlackError::Network(e.to_string()))?;
                continue;
            }
        };
        heard = tokio::time::Instant::now();
        let frame = match frame {
            None => return Err(SlackError::Network("connection closed".into())),
            Some(Err(error)) => return Err(SlackError::Network(error.to_string())),
            Some(Ok(frame)) => frame,
        };
        match frame {
            Frame::Text(text) => {
                let (ack, meaning) = read_frame(text.as_str());
                if let Some(ack) = ack {
                    socket
                        .send(Frame::Text(ack.into()))
                        .await
                        .map_err(|e| SlackError::Network(e.to_string()))?;
                }
                match meaning {
                    Meaning::Hello => sink(SocketEvent::Connected),
                    Meaning::Reconnect(reason) => {
                        let _ = socket.close(None).await;
                        return Ok(reason);
                    }
                    Meaning::Event { team, event } => sink(SocketEvent::Event { team, event }),
                    Meaning::Ignore => {}
                }
            }
            Frame::Ping(_) => {
                // tungstenite queued the pong; send it now.
                socket
                    .flush()
                    .await
                    .map_err(|e| SlackError::Network(e.to_string()))?;
            }
            Frame::Close(close) => {
                let reason = close.map_or_else(|| "closed".to_owned(), |c| c.reason.to_string());
                return Err(SlackError::Network(reason));
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn events_are_acknowledged_and_routed_by_team() {
        let (ack, frame) = read_frame(
            r#"{"envelope_id":"e1","type":"events_api","accepts_response_payload":false,
               "payload":{"team_id":"T0","authorizations":[{"team_id":"T1","user_id":"U1"}],
                          "event":{"type":"message","channel":"C1","ts":"1.0","text":"hi"}}}"#,
        );
        assert_eq!(ack.as_deref(), Some(r#"{"envelope_id":"e1"}"#));
        match frame {
            Meaning::Event { team, event } => {
                assert_eq!(team, "T1");
                assert_eq!(event["channel"], "C1");
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn hello_and_disconnect_are_understood() {
        assert_eq!(
            read_frame(r#"{"type":"hello","num_connections":1}"#),
            (None, Meaning::Hello)
        );
        assert_eq!(
            read_frame(r#"{"type":"disconnect","reason":"refresh_requested"}"#),
            (None, Meaning::Reconnect("refresh_requested".into()))
        );
        assert_eq!(read_frame("garbage"), (None, Meaning::Ignore));
    }

    #[test]
    fn refused_tokens_are_fatal_and_outages_are_not() {
        for code in [
            "invalid_auth",
            "token_revoked",
            "not_allowed_token_type",
            "link_disabled",
        ] {
            assert!(is_fatal(code), "{code}");
        }
        for code in ["ratelimited", "internal_error", "channel_not_found"] {
            assert!(!is_fatal(code), "{code}");
        }
    }

    #[test]
    fn only_routine_reconnects_skip_the_backoff() {
        let mut backoff = Duration::from_secs(1);
        let routine = Ended::Reconnect("refresh_requested".into());
        // A long-lived connection being refreshed reconnects at once.
        assert_eq!(
            wait_before_next(&routine, Duration::from_secs(3600), &mut backoff),
            Duration::ZERO
        );
        assert_eq!(backoff, Duration::from_secs(1));
        // A disconnect right after connecting backs off, doubling.
        let quick = Duration::from_secs(2);
        assert_eq!(
            wait_before_next(&routine, quick, &mut backoff),
            Duration::from_secs(1)
        );
        assert_eq!(
            wait_before_next(&routine, quick, &mut backoff),
            Duration::from_secs(2)
        );
        for _ in 0..10 {
            wait_before_next(&routine, quick, &mut backoff);
        }
        assert_eq!(backoff, MAX_BACKOFF);
    }

    #[test]
    fn rate_limits_wait_at_least_as_long_as_asked() {
        let mut backoff = Duration::from_secs(1);
        let limited = Ended::RateLimited(Duration::from_secs(30));
        assert_eq!(
            wait_before_next(&limited, Duration::ZERO, &mut backoff),
            Duration::from_secs(30)
        );
        let failed = Ended::Failed(SlackError::Network("down".into()));
        assert_eq!(
            wait_before_next(&failed, Duration::from_secs(600), &mut backoff),
            Duration::from_secs(1)
        );
    }

    #[test]
    fn other_envelopes_are_still_acknowledged() {
        let (ack, frame) =
            read_frame(r#"{"envelope_id":"e2","type":"slash_commands","payload":{}}"#);
        assert!(ack.is_some());
        assert_eq!(frame, Meaning::Ignore);
    }
}
