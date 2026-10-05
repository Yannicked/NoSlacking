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
//! Slack sends `hello`. If Slack itself turns several attempts in a row
//! away (an error code, or closing the socket before `hello`), the socket
//! reports [`RtmEvent::Unavailable`] and stops, and the worker polls the
//! open conversation instead. Failing to reach Slack at all (no network,
//! a timeout, a rate limit) is not a refusal: it is retried for as long as
//! it takes, with a capped backoff.
//!
//! The socket also carries a few frames the other way: presence
//! subscriptions and "typing" notices. They are only sent once Slack has
//! said hello; anything queued while the socket was down is dropped, since
//! a stale "typing" is wrong and the worker subscribes again on every
//! connect.

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
/// Attempts in a row that Slack refused before RTM is given up on.
const MAX_REFUSED_ATTEMPTS: u32 = 3;
/// A connection that lasted this long resets the reconnect backoff; one
/// that dies sooner, even after hello, keeps backing off.
const STABLE: Duration = Duration::from_secs(60);
/// The longest wait between reconnect attempts.
const MAX_BACKOFF: Duration = Duration::from_secs(60);

/// What the socket tells the worker.
#[derive(Debug)]
pub enum RtmEvent {
    /// Slack said hello: live updates are flowing.
    Connected,
    /// A live connection dropped; reconnecting.
    Disconnected(SlackError),
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
#[derive(Debug)]
enum Ended {
    /// Slack said hello, then the connection closed.
    AfterHello(SlackError),
    /// Slack answered, but turned the connection away before hello.
    Refused(SlackError),
    /// Slack could not be reached: the network, a timeout, a rate limit.
    Unreachable(SlackError),
}

/// What to do once a connection has ended.
#[derive(Debug, PartialEq)]
enum Next {
    /// Stop: Slack will not give this session a socket.
    GiveUp,
    /// Try again, after a fresh backoff when `reset`.
    Retry { reset: bool },
}

/// Decides what follows `ended`, counting Slack's refusals in a row in
/// `refused`, for a connection that `lasted` this long. Only refusals
/// count; an outage, however long, never makes the socket give up.
fn next(ended: &Ended, refused: &mut u32, lasted: Duration) -> Next {
    match ended {
        // Refused outright: retrying will not help.
        Ended::AfterHello(error) | Ended::Refused(error) | Ended::Unreachable(error)
            if error.is_auth() =>
        {
            Next::GiveUp
        }
        Ended::AfterHello(_) => {
            *refused = 0;
            Next::Retry {
                reset: lasted > STABLE,
            }
        }
        Ended::Refused(_) => {
            *refused += 1;
            if *refused >= MAX_REFUSED_ATTEMPTS {
                Next::GiveUp
            } else {
                Next::Retry { reset: false }
            }
        }
        Ended::Unreachable(_) => Next::Retry { reset: false },
    }
}

/// The frames the worker sends: bare objects such as
/// `{"type":"typing","channel":"C1"}`, numbered here.
pub type Outgoing = tokio::sync::mpsc::UnboundedReceiver<serde_json::Value>;

/// Keeps the socket open until `stop` changes, reporting through `sink`
/// and sending what arrives on `outgoing`.
pub async fn run(
    client: Client,
    sink: impl Fn(RtmEvent) + Send + Sync,
    mut outgoing: Outgoing,
    mut stop: tokio::sync::watch::Receiver<bool>,
) {
    let mut backoff = Duration::from_secs(1);
    let mut refused = 0;
    loop {
        if *stop.borrow() {
            return;
        }
        let started = tokio::time::Instant::now();
        let ended = tokio::select! {
            ended = connection(&client, &sink, &mut outgoing) => ended,
            _ = stop.changed() => return,
        };
        match next(&ended, &mut refused, started.elapsed()) {
            Next::GiveUp => {
                let (Ended::AfterHello(error) | Ended::Refused(error) | Ended::Unreachable(error)) =
                    ended;
                sink(RtmEvent::Unavailable(error.to_string()));
                return;
            }
            Next::Retry { reset } => {
                if reset {
                    backoff = Duration::from_secs(1);
                }
                match ended {
                    Ended::AfterHello(error) => {
                        // A real drop after a working connection: say so.
                        log::info!("RTM connection lost: {error}");
                        sink(RtmEvent::Disconnected(error));
                    }
                    Ended::Refused(error) => log::info!("RTM attempt refused: {error}"),
                    Ended::Unreachable(error) => {
                        log::info!("RTM could not reach Slack, retrying: {error}");
                        sink(RtmEvent::Disconnected(error));
                    }
                }
            }
        }
        tokio::select! {
            _ = tokio::time::sleep(backoff) => {}
            _ = stop.changed() => return,
        }
        backoff = (backoff * 2).min(MAX_BACKOFF);
    }
}

async fn connection(
    client: &Client,
    sink: &(impl Fn(RtmEvent) + Send + Sync),
    outgoing: &mut Outgoing,
) -> Ended {
    // Presence comes only for the people subscribed to, in batches: a big
    // workspace would otherwise flood the socket with everyone's.
    let params = [
        ("batch_presence_aware", "1".to_owned()),
        ("presence_sub", "true".to_owned()),
    ];
    let connect: RtmConnect = match client.call("rtm.connect", &params).await {
        Ok(connect) => connect,
        Err(error @ SlackError::Api(_)) => return Ended::Refused(error),
        Err(error) => return Ended::Unreachable(error),
    };
    // The wss URL carries no auth of its own: the handshake must repeat the
    // `d` cookie, or Slack answers the socket with invalid_auth.
    let mut request = match connect.url.as_str().into_client_request() {
        Ok(request) => request,
        Err(error) => return Ended::Refused(SlackError::Decode(error.to_string())),
    };
    if let Some(cookie) = client.token().cookie
        && let Ok(value) = HeaderValue::from_str(&format!("d={cookie}"))
    {
        request.headers_mut().insert(COOKIE, value);
    }
    let (mut socket, _) = match super::net::websocket(request).await {
        Ok(socket) => socket,
        // Slack answered the handshake with an HTTP error: a refusal.
        Err(tokio_tungstenite::tungstenite::Error::Http(response)) => {
            return Ended::Refused(SlackError::Http(response.status().as_u16()));
        }
        Err(error) => return Ended::Unreachable(SlackError::Network(error.to_string())),
    };
    let mut hello = false;
    // Before hello, an answer from Slack (an error frame, a close) is a
    // refusal, and a dead line is not.
    let end = |hello: bool, from_slack: bool, error: SlackError| {
        if hello {
            Ended::AfterHello(error)
        } else if from_slack {
            Ended::Refused(error)
        } else {
            Ended::Unreachable(error)
        }
    };
    let mut ping = tokio::time::interval(PING_EVERY);
    ping.tick().await;
    let mut heard = tokio::time::Instant::now();
    // What was queued while there was no socket is out of date.
    while outgoing.try_recv().is_ok() {}
    let mut sent: u64 = 0;
    loop {
        let frame = tokio::select! {
            frame = socket.next() => frame,
            Some(out) = outgoing.recv(), if hello => {
                sent += 1;
                let Some(text) = numbered(out, sent) else {
                    continue;
                };
                if let Err(error) = socket.send(Frame::Text(text.into())).await {
                    return end(hello, false, SlackError::Network(error.to_string()));
                }
                continue;
            }
            () = tokio::time::sleep_until(heard + SILENCE) => {
                return end(hello, false, SlackError::Network("connection went silent".into()));
            }
            _ = ping.tick() => {
                if let Err(error) = socket.send(Frame::Ping(Vec::new().into())).await {
                    return end(hello, false, SlackError::Network(error.to_string()));
                }
                continue;
            }
        };
        heard = tokio::time::Instant::now();
        let frame = match frame {
            None => {
                return end(
                    hello,
                    false,
                    SlackError::Network("connection closed".into()),
                );
            }
            Some(Err(error)) => return end(hello, false, SlackError::Network(error.to_string())),
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
                        return end(hello, true, SlackError::Api(message.to_owned()));
                    }
                    Some(kind) if !is_housekeeping(kind) => sink(RtmEvent::Event(value)),
                    _ => {}
                }
            }
            Frame::Close(frame) => {
                let reason = frame.map_or_else(|| "closed".to_owned(), |c| c.reason.to_string());
                return end(hello, true, SlackError::Network(reason));
            }
            _ => {}
        }
    }
}

/// Whether a frame of this type is only the socket's own bookkeeping, with
/// nothing for the worker. Preference and Do Not Disturb changes are not:
/// they are how a session workspace hears of mutes and snoozes made in
/// another client.
fn is_housekeeping(kind: &str) -> bool {
    matches!(kind, "pong" | "reconnect_url")
}

/// An outgoing frame as text, with the `id` RTM wants on everything a
/// client sends. Anything but an object is not a frame.
fn numbered(mut frame: serde_json::Value, id: u64) -> Option<String> {
    frame.as_object_mut()?.insert("id".to_owned(), id.into());
    Some(frame.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn outgoing_frames_are_numbered() {
        let text =
            numbered(serde_json::json!({"type": "typing", "channel": "C1"}), 7).expect("an object");
        let back: serde_json::Value = serde_json::from_str(&text).expect("json");
        assert_eq!(back["id"], 7);
        assert_eq!(back["channel"], "C1");
        assert_eq!(numbered(serde_json::json!("typing"), 1), None);
    }

    #[test]
    fn preference_and_dnd_changes_reach_the_worker() {
        assert!(!is_housekeeping("pref_change"));
        assert!(!is_housekeeping("dnd_updated"));
        assert!(!is_housekeeping("message"));
        assert!(is_housekeeping("pong"));
        assert!(is_housekeeping("reconnect_url"));
    }

    fn network() -> SlackError {
        SlackError::Network("no route to host".into())
    }

    #[test]
    fn outages_never_give_up() {
        let mut refused = 0;
        for _ in 0..100 {
            assert_eq!(
                next(&Ended::Unreachable(network()), &mut refused, Duration::ZERO),
                Next::Retry { reset: false }
            );
        }
        assert_eq!(
            next(
                &Ended::Unreachable(SlackError::RateLimited),
                &mut refused,
                Duration::ZERO
            ),
            Next::Retry { reset: false }
        );
        assert_eq!(refused, 0);
    }

    #[test]
    fn repeated_refusals_give_up() {
        let mut refused = 0;
        let refusal = || Ended::Refused(SlackError::Api("not_allowed".into()));
        assert_eq!(
            next(&refusal(), &mut refused, Duration::ZERO),
            Next::Retry { reset: false }
        );
        // An outage in between neither counts nor clears the count.
        assert_eq!(
            next(&Ended::Unreachable(network()), &mut refused, Duration::ZERO),
            Next::Retry { reset: false }
        );
        assert_eq!(
            next(&refusal(), &mut refused, Duration::ZERO),
            Next::Retry { reset: false }
        );
        assert_eq!(next(&refusal(), &mut refused, Duration::ZERO), Next::GiveUp);
    }

    #[test]
    fn a_working_connection_clears_the_count() {
        let mut refused = 2;
        assert_eq!(
            next(&Ended::AfterHello(network()), &mut refused, STABLE * 2),
            Next::Retry { reset: true }
        );
        assert_eq!(refused, 0);
    }

    #[test]
    fn a_drop_right_after_hello_keeps_backing_off() {
        let mut refused = 2;
        assert_eq!(
            next(
                &Ended::AfterHello(network()),
                &mut refused,
                Duration::from_secs(1)
            ),
            Next::Retry { reset: false }
        );
        assert_eq!(refused, 0);
    }

    #[test]
    fn a_dead_sign_in_gives_up_at_once() {
        let mut refused = 0;
        let auth = || SlackError::Api("invalid_auth".into());
        assert_eq!(
            next(&Ended::AfterHello(auth()), &mut refused, Duration::ZERO),
            Next::GiveUp
        );
        assert_eq!(
            next(&Ended::Unreachable(auth()), &mut refused, Duration::ZERO),
            Next::GiveUp
        );
    }
}
