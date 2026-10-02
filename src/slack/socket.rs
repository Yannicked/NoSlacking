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

#[derive(Debug)]
pub enum SocketEvent {
    Connected,
    Disconnected(String),
    /// The app token was refused; reconnecting will not help.
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

/// Opens a Socket Mode URL with the app-level token.
async fn open_url(http: &reqwest::Client, app_token: &str) -> Result<String, SlackError> {
    let response = http
        .post(format!("{}apps.connections.open", client::API))
        .bearer_auth(app_token)
        .send()
        .await
        .map_err(|e| SlackError::Network(e.without_url().to_string()))?;
    let bytes = response
        .bytes()
        .await
        .map_err(|e| SlackError::Network(e.without_url().to_string()))?;
    let open: types::ConnectionsOpen = client::decode(&bytes)?;
    Ok(open.url)
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
        match outcome {
            Ok(reason) => {
                log::info!("Socket Mode reconnecting: {reason}");
                backoff = Duration::from_secs(1);
                continue;
            }
            Err(SlackError::Api(code)) if is_fatal(&code) => {
                log::warn!("Socket Mode app token refused: {code}");
                sink(SocketEvent::Rejected(code));
                // Wait for a new token (the worker restarts this task).
                let _ = stop.changed().await;
                return;
            }
            Err(error) => {
                log::warn!("Socket Mode connection lost: {error}");
                sink(SocketEvent::Disconnected(error.to_string()));
            }
        }
        if started.elapsed() > STABLE {
            backoff = Duration::from_secs(1);
        }
        tokio::select! {
            _ = tokio::time::sleep(backoff) => {}
            _ = stop.changed() => return,
        }
        backoff = (backoff * 2).min(Duration::from_secs(60));
    }
}

fn is_fatal(code: &str) -> bool {
    matches!(
        code,
        "invalid_auth"
            | "not_authed"
            | "token_revoked"
            | "not_allowed_token_type"
            | "invalid_token"
    )
}

/// One socket, from open to close. `Ok` carries why Slack asked us to
/// reconnect.
async fn connection(
    http: &reqwest::Client,
    app_token: &str,
    sink: &(impl Fn(SocketEvent) + Send + Sync),
) -> Result<String, SlackError> {
    let url = open_url(http, app_token).await?;
    let (mut socket, _) = tokio_tungstenite::connect_async(url.as_str())
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
    fn other_envelopes_are_still_acknowledged() {
        let (ack, frame) =
            read_frame(r#"{"envelope_id":"e2","type":"slash_commands","payload":{}}"#);
        assert!(ack.is_some());
        assert_eq!(frame, Meaning::Ignore);
    }
}
