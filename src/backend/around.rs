//! History away from the newest messages: the stretch around one message,
//! for jumping to it, and the pages after it, for reading on towards the
//! present.
//!
//! `conversations.history` answers the messages nearest `latest` when it
//! is given, and the ones nearest `oldest` when only that is given, so one
//! call of each reads either side of a message.

use super::api::{HistoryQuery, failure};
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
    let mut query = HistoryQuery::new(channel, limit).without_metadata();
    if let Some(latest) = latest {
        query = query.up_to(latest);
    }
    if let Some(oldest) = oldest {
        query = query.after(oldest, Some(false));
    }
    let page: types::HistoryPage = query.page(client).await?;
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
            error: failure(&error),
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
            error: failure(&error),
        }),
    }
}

/// Message `ts` of `channel` by itself, to quote, as [`Event::Quoted`].
///
/// A reply is read from its thread (`conversations.replies` of `thread`),
/// anything else from the history (`conversations.history` ending at it).
/// Either way Slack answers the nearest messages, so the one asked for is
/// picked out by its time: a deleted message comes back as none.
pub async fn quote(
    client: Client,
    team: String,
    channel: String,
    ts: Ts,
    thread: Option<Ts>,
    sink: Sink,
) {
    let mut params = vec![
        ("channel", channel.clone()),
        ("latest", ts.0.clone()),
        ("inclusive", "true".to_owned()),
    ];
    let method = match &thread {
        Some(parent) => {
            // The parent comes first whatever the range, so room for two.
            params.push(("ts", parent.0.clone()));
            params.push(("oldest", ts.0.clone()));
            params.push(("limit", "2".to_owned()));
            "conversations.replies"
        }
        None => {
            params.push(("limit", "1".to_owned()));
            "conversations.history"
        }
    };
    let result = client
        .call::<types::HistoryPage>(method, &params)
        .await
        .map(|page| picked(page.messages, &ts))
        .map_err(|error| failure(&error));
    sink.send(Event::Quoted {
        team,
        channel,
        ts,
        result,
    });
}

/// The message at `ts` among what Slack answered, if it is there.
fn picked(messages: Vec<types::Message>, ts: &Ts) -> Option<Message> {
    messages
        .into_iter()
        .filter(|m| m.ts == ts.0)
        .find_map(types::Message::into_model)
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
    fn a_quote_is_the_message_asked_for_or_none() {
        let page: types::HistoryPage = serde_json::from_str(
            r#"{"ok":true,"messages":[
                {"type":"message","ts":"5.0","user":"U1","text":"parent","thread_ts":"5.0"},
                {"type":"message","ts":"7.0","user":"U2","text":"reply","thread_ts":"5.0"}
            ]}"#,
        )
        .expect("a page");
        let reply = picked(page.messages, &Ts::new("7.0")).expect("the reply");
        assert_eq!(reply.text, "reply");
        // Deleted: Slack answers the message before it instead.
        let page: types::HistoryPage = serde_json::from_str(
            r#"{"ok":true,"messages":[{"type":"message","ts":"4.0","user":"U1","text":"older"}]}"#,
        )
        .expect("a page");
        assert_eq!(picked(page.messages, &Ts::new("5.0")), None);
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
