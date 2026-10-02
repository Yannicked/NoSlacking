//! History away from the newest messages: the stretch around one message,
//! for jumping to it, and the pages after it, for reading on towards the
//! present.
//!
//! `conversations.history` answers the messages nearest `latest` when it
//! is given, and the ones nearest `oldest` when only that is given, so one
//! call of each reads either side of a message.

use super::worker::describe;
use super::{Event, Sink};
use crate::model::{Message, Ts};
use crate::slack::{Client, SlackError, types};

/// How many messages are read on either side of the one jumped to.
const SIDE: u32 = 25;
/// How many messages a newer page holds.
const NEWER_PAGE: u32 = 50;

/// One side of a message: the page and what lies beyond it.
struct Side {
    messages: Vec<Message>,
    has_more: bool,
    cursor: Option<String>,
}

/// Reads up to `limit` messages before (and including) `latest`, or after
/// `oldest`.
async fn side(
    client: &Client,
    channel: &str,
    latest: Option<&Ts>,
    oldest: Option<&Ts>,
    limit: u32,
) -> Result<Side, SlackError> {
    let mut params = vec![
        ("channel", channel.to_owned()),
        ("limit", limit.to_string()),
        ("include_all_metadata", "false".to_owned()),
    ];
    if let Some(latest) = latest {
        params.push(("latest", latest.0.clone()));
        params.push(("inclusive", "true".to_owned()));
    }
    if let Some(oldest) = oldest {
        params.push(("oldest", oldest.0.clone()));
        params.push(("inclusive", "false".to_owned()));
    }
    let page: types::HistoryPage = client.call("conversations.history", &params).await?;
    Ok(Side {
        messages: page
            .messages
            .into_iter()
            .filter_map(types::Message::into_model)
            .collect(),
        has_more: page.has_more,
        cursor: page.response_metadata.cursor(),
    })
}

/// Puts pages together oldest first, each message once, whatever order
/// Slack answered them in.
pub fn window(pages: impl IntoIterator<Item = Vec<Message>>) -> Vec<Message> {
    let mut messages: Vec<Message> = pages.into_iter().flatten().collect();
    messages.sort_by(|a, b| a.ts.cmp(&b.ts));
    messages.dedup_by(|a, b| a.ts == b.ts);
    messages
}

/// The messages around `ts`, as [`Event::Around`].
pub async fn around(client: Client, team: String, channel: String, ts: Ts, sink: Sink) {
    let (before, after) = tokio::join!(
        side(&client, &channel, Some(&ts), None, SIDE),
        side(&client, &channel, None, Some(&ts), SIDE),
    );
    match (before, after) {
        (Ok(before), Ok(after)) => sink.send(Event::Around {
            team,
            channel,
            ts,
            messages: window([before.messages, after.messages]),
            has_older: before.has_more,
            cursor: before.cursor,
            has_newer: after.has_more,
        }),
        (Err(error), _) | (_, Err(error)) => sink.send(Event::HistoryFailed {
            team,
            channel,
            error: describe(&error),
        }),
    }
}

/// The page right after `after`, as [`Event::Newer`].
pub async fn newer(client: Client, team: String, channel: String, after: Ts, sink: Sink) {
    match side(&client, &channel, None, Some(&after), NEWER_PAGE).await {
        Ok(page) => sink.send(Event::Newer {
            team,
            channel,
            messages: window([page.messages]),
            has_newer: page.has_more,
        }),
        Err(error) => sink.send(Event::HistoryFailed {
            team,
            channel,
            error: describe(&error),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn message(ts: &str) -> Message {
        serde_json::from_str::<types::Message>(&format!(
            r#"{{"type":"message","ts":"{ts}","user":"U1","text":"{ts}"}}"#
        ))
        .ok()
        .and_then(types::Message::into_model)
        .expect("a message")
    }

    #[test]
    fn both_sides_make_one_list_oldest_first() {
        // Slack answers newest first; the message asked for can come back
        // on both sides.
        let before = vec![message("5.0"), message("4.0"), message("3.0")];
        let after = vec![message("7.0"), message("6.0"), message("5.0")];
        let order: Vec<String> = window([before, after])
            .into_iter()
            .map(|m| m.ts.0)
            .collect();
        assert_eq!(order, ["3.0", "4.0", "5.0", "6.0", "7.0"]);
        assert!(window([Vec::new(), Vec::new()]).is_empty());
    }
}
