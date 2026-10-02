//! The worker's side of [`crate::views`]: the Web API calls behind the
//! views at the top of the sidebar, and the JSON they answer with.
//!
//! A browser session can read what Slack's own web client reads, such as
//! the activity feed; those methods are not documented and may change, so
//! every one of them falls back to documented methods when it fails.

use serde::Deserialize;

use super::worker::describe;
use super::{Event, Sink};
use crate::model::{Message, Ts};
use crate::slack::search::MessagesAnswer;
use crate::slack::{Client, SlackError, types};
use crate::views::{self, Activity, Command, Reason};

/// How many items of the activity feed are read.
const FEED_LIMIT: usize = 50;
/// How many mentions the search fallback lists.
const SEARCH_COUNT: usize = 50;
/// The most messages fetched one by one to fill in a list of references.
const FILL_LIMIT: usize = 40;

/// Runs one command and reports back. Every command is answered, so a view
/// waiting on it never waits for ever.
pub async fn run(client: Client, team: String, command: Command, sink: Sink) {
    let event = match command {
        Command::Activity { me } => {
            let (result, searched) = match activity(&client, &me).await {
                Ok((items, searched)) => (Ok(items), searched),
                Err(error) => (Err(describe(&error)), false),
            };
            views::Event::Activity { result, searched }
        }
    };
    reply(&sink, &team, event);
}

fn reply(sink: &Sink, team: &str, event: views::Event) {
    sink.send(Event::Views {
        team: team.to_owned(),
        event,
    });
}

// ---- activity ---------------------------------------------------------

/// `activity.feed`, the web client's activity list. Each item names a
/// message rather than carrying it.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct Feed {
    items: Vec<FeedItem>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct FeedItem {
    is_unread: bool,
    item: FeedBody,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct FeedBody {
    #[serde(rename = "type")]
    kind: String,
    message: Option<FeedMessage>,
    bundle_info: Option<Bundle>,
}

/// A message an item names. Some answers carry its text and author too.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct FeedMessage {
    ts: String,
    channel: String,
    thread_ts: Option<String>,
    author_user_id: Option<String>,
    user: Option<String>,
    text: Option<String>,
}

/// A thread's replies, bundled into one item.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct Bundle {
    payload: BundlePayload,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct BundlePayload {
    thread_entry: Option<ThreadEntry>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct ThreadEntry {
    channel_id: String,
    thread_ts: String,
    latest_ts: Option<String>,
}

/// A message the activity names: where it is, why it is there, and what
/// the reference already says about it.
#[derive(Clone, Debug, PartialEq)]
struct Reference {
    reason: Reason,
    channel: String,
    ts: Ts,
    thread: Option<Ts>,
    unread: bool,
    user: Option<String>,
    text: Option<String>,
}

/// Why an item's type puts it in the activity; types the view does not
/// show (reactions, invitations, list edits) are `None`.
fn feed_reason(kind: &str) -> Option<Reason> {
    match kind {
        "at_user" | "at_user_group" => Some(Reason::Mention),
        "at_channel" | "at_everyone" | "at_here" => Some(Reason::Everyone),
        "thread_v2" | "thread_reply" => Some(Reason::Reply),
        _ => None,
    }
}

/// The messages a feed names, in its order.
fn references(feed: Feed) -> Vec<Reference> {
    feed.items
        .into_iter()
        .filter_map(|entry| {
            let reason = feed_reason(&entry.item.kind)?;
            if let Some(message) = entry.item.message.filter(|m| !m.ts.is_empty()) {
                if message.channel.is_empty() {
                    return None;
                }
                return Some(Reference {
                    reason,
                    channel: message.channel,
                    ts: Ts::new(message.ts),
                    thread: message.thread_ts.filter(|t| !t.is_empty()).map(Ts::new),
                    unread: entry.is_unread,
                    user: message.author_user_id.or(message.user),
                    text: message.text,
                });
            }
            let thread = entry.item.bundle_info?.payload.thread_entry?;
            if thread.channel_id.is_empty() || thread.thread_ts.is_empty() {
                return None;
            }
            let ts = thread
                .latest_ts
                .filter(|t| !t.is_empty())
                .unwrap_or_else(|| thread.thread_ts.clone());
            Some(Reference {
                reason,
                channel: thread.channel_id,
                ts: Ts::new(ts),
                thread: Some(Ts::new(thread.thread_ts)),
                unread: entry.is_unread,
                user: None,
                text: None,
            })
        })
        .collect()
}

/// Your activity: Slack's own feed for a browser session, else (or when
/// the feed cannot be read) the messages a search finds naming you.
/// Answers whether it searched.
async fn activity(client: &Client, me: &str) -> Result<(Vec<Activity>, bool), SlackError> {
    if client.token().is_session() {
        match feed(client).await {
            Ok(items) => return Ok((items, false)),
            Err(error) => log::info!("activity.feed: {error}; searching for mentions instead"),
        }
    }
    let answer: MessagesAnswer = client
        .call(
            "search.messages",
            &[
                ("query", format!("<@{me}>")),
                ("count", SEARCH_COUNT.to_string()),
                ("sort", "timestamp".to_owned()),
                ("sort_dir", "desc".to_owned()),
            ],
        )
        .await?;
    Ok((searched_mentions(answer), true))
}

/// The mentions a search found, as activity.
fn searched_mentions(answer: MessagesAnswer) -> Vec<Activity> {
    answer
        .into_page()
        .hits
        .into_iter()
        .filter_map(|hit| {
            let channel = hit.channel?;
            let ts = hit.ts?;
            let mut message = views::bare_message(ts, hit.user, hit.text, hit.thread);
            message.username = hit.username;
            Some(Activity {
                reason: Reason::Mention,
                channel,
                message,
                unread: false,
            })
        })
        .collect()
}

/// Reads `activity.feed` and fills in the messages it names.
async fn feed(client: &Client) -> Result<Vec<Activity>, SlackError> {
    let feed: Feed = client
        .call(
            "activity.feed",
            &[
                ("limit", FEED_LIMIT.to_string()),
                (
                    "types",
                    "thread_v2,at_user,at_user_group,at_channel,at_everyone".to_owned(),
                ),
                ("mode", "chrono_reads_and_unreads".to_owned()),
            ],
        )
        .await?;
    let references = references(feed);
    let filled = futures_util::future::join_all(
        references
            .into_iter()
            .take(FILL_LIMIT)
            .map(|r| fill(client, r)),
    )
    .await;
    Ok(filled.into_iter().flatten().collect())
}

/// The activity item for a reference: the message itself, fetched when the
/// reference does not carry its text. One that cannot be read (deleted, or
/// in a conversation you left) is dropped.
async fn fill(client: &Client, reference: Reference) -> Option<Activity> {
    let message = match &reference.text {
        Some(text) => views::bare_message(
            reference.ts.clone(),
            reference.user.clone(),
            text.clone(),
            reference.thread.clone(),
        ),
        None => match fetch_message(client, &reference.channel, &reference.ts).await {
            Ok(Some(message)) => message,
            Ok(None) => return None,
            Err(error) => {
                log::debug!("could not read a message the activity names: {error}");
                return None;
            }
        },
    };
    Some(Activity {
        reason: reference.reason,
        channel: reference.channel,
        message,
        unread: reference.unread,
    })
}

/// One message of `channel`, in a thread or not. `conversations.replies`
/// takes a reply's own timestamp and answers its thread, or the message
/// alone when it has none; history is asked when that finds nothing.
pub async fn fetch_message(
    client: &Client,
    channel: &str,
    ts: &Ts,
) -> Result<Option<Message>, SlackError> {
    let around = |method: &'static str| {
        let mut params = vec![
            ("channel", channel.to_owned()),
            ("oldest", ts.0.clone()),
            ("latest", ts.0.clone()),
            ("inclusive", "true".to_owned()),
            ("limit", "1".to_owned()),
        ];
        if method == "conversations.replies" {
            params.push(("ts", ts.0.clone()));
        }
        async move { client.call::<types::HistoryPage>(method, &params).await }
    };
    let pick = |page: types::HistoryPage| {
        page.messages
            .into_iter()
            .filter(|m| m.ts == ts.0)
            .find_map(types::Message::into_model)
    };
    match around("conversations.replies").await {
        Ok(page) => {
            if let Some(message) = pick(page) {
                return Ok(Some(message));
            }
        }
        Err(error) if error.is_auth() => return Err(error),
        Err(_) => {}
    }
    Ok(pick(around("conversations.history").await?))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_feed_names_mentions_and_threads() {
        let feed: Feed = serde_json::from_str(
            r#"{"ok":true,"items":[
              {"is_unread":true,"feed_ts":"5.0","item":{"type":"at_user",
                "message":{"ts":"5.000100","channel":"C1","author_user_id":"U2"}}},
              {"is_unread":false,"item":{"type":"at_channel",
                "message":{"ts":"4.000100","channel":"C2","thread_ts":"3.000100"}}},
              {"item":{"type":"message_reaction",
                "message":{"ts":"3.5","channel":"C1"}}},
              {"is_unread":true,"item":{"type":"thread_v2","bundle_info":{"payload":
                {"thread_entry":{"channel_id":"C3","thread_ts":"1.000100","latest_ts":"2.000100"}}}}},
              {"item":{"type":"at_user","message":{"ts":"","channel":"C1"}}},
              {"item":{"type":"something_new","whatever":[1,2]}}
            ]}"#,
        )
        .expect("parses");
        let refs = references(feed);
        assert_eq!(refs.len(), 3, "{refs:#?}");
        assert_eq!(refs[0].reason, Reason::Mention);
        assert_eq!(refs[0].user.as_deref(), Some("U2"));
        assert!(refs[0].unread);
        assert_eq!(refs[1].reason, Reason::Everyone);
        assert_eq!(refs[1].thread, Some(Ts::new("3.000100")));
        assert_eq!(refs[2].reason, Reason::Reply);
        assert_eq!(refs[2].channel, "C3");
        assert_eq!(refs[2].ts, Ts::new("2.000100"), "the newest reply");
        assert_eq!(refs[2].thread, Some(Ts::new("1.000100")));
    }

    #[test]
    fn searched_mentions_keep_their_thread() {
        let answer: MessagesAnswer = serde_json::from_str(
            r#"{"ok":true,"messages":{"total":2,"paging":{"page":1,"pages":1},"matches":[
              {"channel":{"id":"C1","name":"general"},"user":"U2","ts":"9.000100",
               "text":"<@U0> look","permalink":"https://x.slack.com/archives/C1/p9000100?thread_ts=8.000100&cid=C1"},
              {"channel":{"id":"","name":""},"ts":"8.0","text":"lost"}
            ]}}"#,
        )
        .expect("parses");
        let items = searched_mentions(answer);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].channel, "C1");
        assert_eq!(items[0].message.thread_ts, Some(Ts::new("8.000100")));
        assert_eq!(items[0].message.user.as_deref(), Some("U2"));
    }
}
