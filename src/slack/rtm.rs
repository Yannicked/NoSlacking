//! Real-time messages for a browser-session workspace, over Slack's classic
//! RTM WebSocket.
//!
//! Socket Mode needs an app-level token, which a session sign-in does not
//! have. The web client instead opens a user-level socket with
//! `rtm.connect`: the frames are bare Slack event objects (the same shape as
//! the `event` inside a Socket Mode `events_api` envelope), with no envelope
//! and nothing to acknowledge. Each session workspace keeps its own socket.
//!
//! Slack does not offer RTM to every session. A connection only counts once
//! Slack sends `hello`; if several attempts in a row end without one, the
//! socket reports [`RtmEvent::Unavailable`] and stops, and the worker polls
//! the open conversation instead.

use std::time::Duration;

use futures_util::{SinkExt as _, StreamExt as _};
use tokio_tungstenite::tungstenite::Message as Frame;
use tokio_tungstenite::tungstenite::client::IntoClientRequest as _;
use tokio_tungstenite::tungstenite::http::HeaderValue;
use tokio_tungstenite::tungstenite::http::header::COOKIE;

use super::Client;
use super::client::SlackError;

const SILENCE: Duration = Duration::from_secs(60);
const PING_EVERY: Duration = Duration::from_secs(20);
/// Attempts in a row without a `hello` before RTM is given up on.
const MAX_FAILED_ATTEMPTS: u32 = 3;

/// What the socket tells the worker.
#[derive(Debug)]
pub enum RtmEvent {
    /// Slack said hello: live updates are flowing.
    Connected,
    /// A live connection dropped; reconnecting.
    Disconnected(String),
    /// Slack will not give this session a socket; poll instead. This never
    /// means signed out: the Web API decides that.
    Unavailable(String),
    Event(serde_json::Value),
}

#[derive(serde::Deserialize)]
struct RtmConnect {
    url: String,
}

/// How one connection ended.
enum Ended {
    /// Slack said hello, then the connection closed.
    AfterHello(SlackError),
    /// The connection ended before Slack said hello.
    BeforeHello(SlackError),
}

/// Keeps the socket open until `stop` changes, reporting through `sink`.
pub async fn run(
    client: Client,
    sink: impl Fn(RtmEvent) + Send + Sync,
    mut stop: tokio::sync::watch::Receiver<bool>,
) {
    let mut backoff = Duration::from_secs(1);
    let mut failed = 0;
    loop {
        if *stop.borrow() {
            return;
        }
        let ended = tokio::select! {
            ended = connection(&client, &sink) => ended,
            _ = stop.changed() => return,
        };
        match ended {
            // Refused outright: retrying will not help.
            Ended::AfterHello(error) | Ended::BeforeHello(error) if error.is_auth() => {
                sink(RtmEvent::Unavailable(error.to_string()));
                return;
            }
            Ended::AfterHello(error) => {
                // A real drop after a working connection: say so, then retry.
                failed = 0;
                backoff = Duration::from_secs(1);
                log::info!("RTM connection lost: {error}");
                sink(RtmEvent::Disconnected(error.to_string()));
            }
            Ended::BeforeHello(error) => {
                failed += 1;
                log::info!("RTM attempt {failed} failed: {error}");
                if failed >= MAX_FAILED_ATTEMPTS {
                    sink(RtmEvent::Unavailable(error.to_string()));
                    return;
                }
            }
        }
        tokio::select! {
            _ = tokio::time::sleep(backoff) => {}
            _ = stop.changed() => return,
        }
        backoff = (backoff * 2).min(Duration::from_secs(60));
    }
}

async fn connection(client: &Client, sink: &(impl Fn(RtmEvent) + Send + Sync)) -> Ended {
    let connect: RtmConnect = match client.call("rtm.connect", &[]).await {
        Ok(connect) => connect,
        Err(error) => return Ended::BeforeHello(error),
    };
    // The wss URL carries no auth of its own: the handshake must repeat the
    // `d` cookie, or Slack answers the socket with invalid_auth.
    let mut request = match connect.url.as_str().into_client_request() {
        Ok(request) => request,
        Err(error) => return Ended::BeforeHello(SlackError::Network(error.to_string())),
    };
    if let Some(cookie) = client.token().cookie
        && let Ok(value) = HeaderValue::from_str(&format!("d={cookie}"))
    {
        request.headers_mut().insert(COOKIE, value);
    }
    let (mut socket, _) = match tokio_tungstenite::connect_async(request).await {
        Ok(socket) => socket,
        Err(error) => return Ended::BeforeHello(SlackError::Network(error.to_string())),
    };
    let mut hello = false;
    let end = |hello: bool, error: SlackError| {
        if hello {
            Ended::AfterHello(error)
        } else {
            Ended::BeforeHello(error)
        }
    };
    let mut ping = tokio::time::interval(PING_EVERY);
    ping.tick().await;
    let mut heard = tokio::time::Instant::now();
    loop {
        let frame = tokio::select! {
            frame = socket.next() => frame,
            () = tokio::time::sleep_until(heard + SILENCE) => {
                return end(hello, SlackError::Network("connection went silent".into()));
            }
            _ = ping.tick() => {
                if let Err(error) = socket.send(Frame::Ping(Vec::new().into())).await {
                    return end(hello, SlackError::Network(error.to_string()));
                }
                continue;
            }
        };
        heard = tokio::time::Instant::now();
        let frame = match frame {
            None => return end(hello, SlackError::Network("connection closed".into())),
            Some(Err(error)) => return end(hello, SlackError::Network(error.to_string())),
            Some(Ok(frame)) => frame,
        };
        match frame {
            Frame::Text(text) => {
                let Ok(value) = serde_json::from_str::<serde_json::Value>(text.as_str()) else {
                    continue;
                };
                match value.get("type").and_then(serde_json::Value::as_str) {
                    Some("hello") => {
                        if !hello {
                            hello = true;
                            sink(RtmEvent::Connected);
                        }
                    }
                    Some("error") => {
                        let message = value
                            .pointer("/error/msg")
                            .and_then(serde_json::Value::as_str)
                            .unwrap_or("error");
                        return end(hello, SlackError::Api(message.to_owned()));
                    }
                    // Housekeeping frames carry no message.
                    Some("pong" | "reconnect_url" | "pref_change" | "dnd_updated") | None => {}
                    Some(_) => sink(RtmEvent::Event(value)),
                }
            }
            Frame::Close(frame) => {
                let reason = frame.map_or_else(|| "closed".to_owned(), |c| c.reason.to_string());
                return end(hello, SlackError::Network(reason));
            }
            _ => {}
        }
    }
}
