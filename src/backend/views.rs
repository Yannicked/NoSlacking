//! The worker's side of [`crate::views`]: the Web API calls behind the
//! views at the top of the sidebar, and the JSON they answer with.
//!
//! A browser session can read what Slack's own web client reads, such as
//! the activity feed; those methods are not documented and may change, so
//! every one of them falls back to documented methods when it fails.

use serde::Deserialize;

use super::api::failure;
use super::{Event, Sink};
use crate::model::{Message, Ts};
use crate::slack::search::MessagesAnswer;
use crate::slack::{Client, SlackError, types};
use crate::views::schedule::Scheduled;
use crate::views::{self, Activity, Command, Followed, Reason, Reminder, Saved};

/// How many items of the activity feed are read.
const FEED_LIMIT: usize = 50;
/// How many mentions the search fallback lists.
const SEARCH_COUNT: usize = 50;
/// The most messages fetched one by one to fill in a list of references.
const FILL_LIMIT: usize = 40;
/// How many unread messages of one conversation are read.
const UNREAD_COUNT: usize = 50;
/// How many messages are shown of a conversation with no read marker.
const UNMARKED_COUNT: usize = 10;
/// How many followed threads are listed.
const THREADS_LIMIT: usize = 25;
/// How many threads found by searching for your replies are read whole.
const SEARCHED_THREADS: usize = 15;
/// The most replies read of one thread found by searching.
const THREAD_PAGE: usize = 200;
/// How many saved messages are listed.
const SAVED_LIMIT: usize = 50;
/// The most pages of scheduled messages read, 100 a page.
const SCHEDULED_PAGES: usize = 5;

/// Runs one command and reports back. Every command is answered, so a view
/// waiting on it never waits for ever.
pub async fn run(client: Client, team: String, command: Command, sink: Sink) {
    let event = match command {
        Command::Activity { me } => {
            let (result, searched) = match activity(&client, &me).await {
                Ok((items, searched)) => (Ok(items), searched),
                Err(error) => (Err(failure(&error)), false),
            };
            views::Event::Activity { result, searched }
        }
        Command::Unread { channel, after } => views::Event::Unread {
            result: unread(&client, &channel, after.as_ref())
                .await
                .map_err(|e| failure(&e)),
            channel,
        },
        Command::Threads { me } => match threads(&client, &me).await {
            Ok((threads, searched)) => views::Event::Threads {
                result: Ok(threads),
                searched,
            },
            Err(error) => views::Event::Threads {
                result: Err(failure(&error)),
                searched: false,
            },
        },
        Command::ReadThread {
            channel,
            thread,
            ts,
        } => {
            // Only the web client keeps a thread's read state.
            if client.token().is_session() {
                let marked = client
                    .act::<serde_json::Value>(
                        "subscriptions.thread.mark",
                        &[
                            ("channel", channel),
                            ("thread_ts", thread.0),
                            ("ts", ts.0),
                            ("read", "1".to_owned()),
                        ],
                    )
                    .await;
                if let Err(error) = marked {
                    log::debug!("subscriptions.thread.mark: {error}");
                }
            }
            views::Event::Nothing
        }
        Command::Saved => match saved(&client).await {
            Ok((list, starred)) => views::Event::Saved {
                result: Ok(list),
                starred,
            },
            Err(error) => views::Event::Saved {
                result: Err(failure(&error)),
                starred: false,
            },
        },
        Command::Reminders => views::Event::Reminders {
            result: client
                .call::<RemindersList>("reminders.list", &[])
                .await
                .map(reminders)
                .map_err(|e| failure(&e)),
        },
        Command::Save { channel, ts, save } => match keep(&client, &channel, &ts, save).await {
            Ok(()) => views::Event::Nothing,
            Err(error) => views::Event::SaveFailed {
                channel,
                ts,
                save,
                error: failure(&error),
            },
        },
        Command::Scheduled => views::Event::ScheduledList {
            result: scheduled(&client).await.map_err(|e| failure(&e)),
        },
        Command::Schedule {
            request,
            channel,
            text,
            thread,
            post_at,
            replace,
        } => {
            let result = schedule(&client, &channel, &text, thread.as_ref(), post_at)
                .await
                .map(|id| Scheduled {
                    id,
                    channel: channel.clone(),
                    post_at,
                    text,
                    thread,
                })
                .map_err(|e| failure(&e));
            // The new one is in: the old one goes. Should that fail, both
            // stay, and the list shows them.
            if result.is_ok()
                && let Some(old) = replace
                && let Err(error) = unschedule(&client, &channel, &old).await
            {
                log::warn!("could not delete the scheduled message it replaced: {error}");
            }
            views::Event::ScheduleDone { request, result }
        }
        Command::CancelScheduled { channel, id } => {
            match unschedule(&client, &channel, &id).await {
                Ok(()) => views::Event::Nothing,
                Err(error) => views::Event::CancelFailed {
                    error: failure(&error),
                },
            }
        }
        Command::Remind { text, time } => views::Event::Reminded {
            time,
            result: client
                .act::<serde_json::Value>("reminders.add", &views::remind::params(&text, time))
                .await
                .map(|_| ())
                .map_err(|e| failure(&e)),
        },
        Command::CompleteReminder { id } => match client
            .act::<serde_json::Value>("reminders.complete", &[("reminder", id.clone())])
            .await
        {
            // Already done elsewhere: nothing to undo.
            Ok(_) => views::Event::Nothing,
            Err(SlackError::Api(code)) if code == "already_complete" => views::Event::Nothing,
            Err(error) => views::Event::CompleteFailed {
                id,
                error: failure(&error),
            },
        },
    };
    reply(&sink, &team, event);
}

fn reply(sink: &Sink, team: &str, event: views::Event) {
    sink.send(Event::Views {
        team: team.to_owned(),
        event,
    });
}

// ---- scheduled --------------------------------------------------------

/// A page of `chat.scheduledMessages.list`.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct ScheduledPage {
    scheduled_messages: Vec<ScheduledItem>,
    response_metadata: types::ResponseMetadata,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct ScheduledItem {
    id: String,
    channel_id: String,
    post_at: i64,
    text: String,
    thread_ts: Option<String>,
}

/// `chat.scheduleMessage`'s answer.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct ScheduleAnswer {
    scheduled_message_id: String,
}

/// The scheduled messages of a page, as the view lists them.
fn scheduled_items(page: Vec<ScheduledItem>) -> Vec<Scheduled> {
    page.into_iter()
        .filter(|item| !item.id.is_empty() && !item.channel_id.is_empty())
        .map(|item| Scheduled {
            id: item.id,
            channel: item.channel_id,
            post_at: item.post_at,
            text: item.text,
            thread: item.thread_ts.filter(|t| !t.is_empty()).map(Ts::new),
        })
        .collect()
}

/// Every message waiting to be sent, soonest first.
async fn scheduled(client: &Client) -> Result<Vec<Scheduled>, SlackError> {
    let mut out = Vec::new();
    let mut cursor: Option<String> = None;
    for _ in 0..SCHEDULED_PAGES {
        let mut params = vec![("limit", "100".to_owned())];
        if let Some(cursor) = cursor.take() {
            params.push(("cursor", cursor));
        }
        let page: ScheduledPage = client.call("chat.scheduledMessages.list", &params).await?;
        out.extend(scheduled_items(page.scheduled_messages));
        match page.response_metadata.cursor() {
            Some(next) => cursor = Some(next),
            None => break,
        }
    }
    out.sort_by_key(|s| s.post_at);
    Ok(out)
}

/// Schedules `text` for `channel` (in `thread`, if any) at `post_at`, and
/// answers the scheduled message's id.
async fn schedule(
    client: &Client,
    channel: &str,
    text: &str,
    thread: Option<&Ts>,
    post_at: i64,
) -> Result<String, SlackError> {
    let mut params = vec![
        ("channel", channel.to_owned()),
        ("text", text.to_owned()),
        ("post_at", post_at.to_string()),
    ];
    if let Some(thread) = thread {
        params.push(("thread_ts", thread.0.clone()));
    }
    let answer: ScheduleAnswer = client.act("chat.scheduleMessage", &params).await?;
    Ok(answer.scheduled_message_id)
}

/// Keeps a scheduled message from being sent. One already gone is no
/// failure.
async fn unschedule(client: &Client, channel: &str, id: &str) -> Result<(), SlackError> {
    match client
        .act::<serde_json::Value>(
            "chat.deleteScheduledMessage",
            &[
                ("channel", channel.to_owned()),
                ("scheduled_message_id", id.to_owned()),
            ],
        )
        .await
    {
        Err(SlackError::Api(code)) if code == "invalid_scheduled_message_id" => Ok(()),
        other => other.map(|_| ()),
    }
}

// ---- later ------------------------------------------------------------

/// `saved.list`, the web client's Later list. Each item names a message.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct SavedList {
    saved_items: Vec<SavedItem>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct SavedItem {
    item_id: String,
    item_type: String,
    ts: String,
    /// `in_progress`, `completed` or `archived`.
    state: String,
    is_archived: bool,
}

/// `stars.list`: the starred items of older Slack, with their messages.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct StarsList {
    items: Vec<StarItem>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct StarItem {
    #[serde(rename = "type")]
    kind: String,
    channel: String,
    message: Option<types::Message>,
}

/// `reminders.list`.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct RemindersList {
    reminders: Vec<ReminderItem>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct ReminderItem {
    id: String,
    text: String,
    time: Option<i64>,
    recurring: bool,
    /// When it was completed; 0 or absent while it is not.
    complete_ts: Option<i64>,
}

/// The messages a Later list names that are still to do, in its order.
fn saved_references(list: SavedList) -> Vec<(String, Ts)> {
    list.saved_items
        .into_iter()
        .filter(|item| item.item_type == "message" && !item.item_id.is_empty())
        .filter(|item| !item.ts.is_empty() && !item.is_archived)
        .filter(|item| item.state != "completed" && item.state != "archived")
        .map(|item| (item.item_id, Ts::new(item.ts)))
        .collect()
}

/// The starred messages of a `stars.list` answer.
fn starred(list: StarsList) -> Vec<Saved> {
    list.items
        .into_iter()
        .filter(|item| item.kind == "message" && !item.channel.is_empty())
        .filter_map(|item| {
            Some(Saved {
                message: item.message?.into_model()?,
                channel: item.channel,
            })
        })
        .collect()
}

/// The reminders not yet complete, soonest first.
fn reminders(list: RemindersList) -> Vec<Reminder> {
    let mut out: Vec<Reminder> = list
        .reminders
        .into_iter()
        .filter(|r| !r.id.is_empty() && r.complete_ts.unwrap_or(0) == 0)
        .map(|r| Reminder {
            id: r.id,
            text: r.text,
            time: r.time.filter(|t| *t > 0),
            recurring: r.recurring,
        })
        .collect();
    out.sort_by_key(|r| (r.time.is_none(), r.time));
    out
}

/// Your saved messages: Later for a browser session, else (or when Later
/// cannot be read) the older starred messages. Answers whether they are
/// the starred ones.
async fn saved(client: &Client) -> Result<(Vec<Saved>, bool), SlackError> {
    if client.token().is_session() {
        match client
            .call::<SavedList>("saved.list", &[("limit", SAVED_LIMIT.to_string())])
            .await
        {
            Ok(list) => {
                let filled = futures_util::future::join_all(
                    saved_references(list).into_iter().take(FILL_LIMIT).map(
                        |(channel, ts)| async move {
                            match fetch_message(client, &channel, &ts).await {
                                Ok(Some(message)) => Some(Saved { channel, message }),
                                Ok(None) => None,
                                Err(error) => {
                                    log::debug!("could not read a saved message: {error}");
                                    None
                                }
                            }
                        },
                    ),
                )
                .await;
                return Ok((filled.into_iter().flatten().collect(), false));
            }
            Err(error) => log::info!("saved.list: {error}; reading starred messages instead"),
        }
    }
    let list: StarsList = client
        .call("stars.list", &[("limit", SAVED_LIMIT.to_string())])
        .await?;
    Ok((starred(list), true))
}

/// Saves a message for later (or takes it off): with Later for a browser
/// session, else (or when Later refuses) with a star. Being already as
/// asked is no failure.
async fn keep(client: &Client, channel: &str, ts: &Ts, save: bool) -> Result<(), SlackError> {
    let settled = |result: Result<serde_json::Value, SlackError>, done: &[&str]| match result {
        Err(SlackError::Api(code)) if done.contains(&code.as_str()) => Ok(()),
        other => other.map(|_| ()),
    };
    if client.token().is_session() {
        let method = if save { "saved.add" } else { "saved.delete" };
        let result = client
            .act::<serde_json::Value>(
                method,
                &[
                    ("item_type", "message".to_owned()),
                    ("item_id", channel.to_owned()),
                    ("ts", ts.0.clone()),
                ],
            )
            .await;
        match settled(result, &["already_saved", "not_saved", "item_not_found"]) {
            Ok(()) => return Ok(()),
            Err(error) if error.is_auth() => return Err(error),
            Err(error) => log::info!("{method}: {error}; starring instead"),
        }
    }
    let method = if save { "stars.add" } else { "stars.remove" };
    let result = client
        .act::<serde_json::Value>(
            method,
            &[("channel", channel.to_owned()), ("timestamp", ts.0.clone())],
        )
        .await;
    settled(result, &["already_starred", "not_starred"])
}

// ---- threads ----------------------------------------------------------

/// `subscriptions.thread.getView`, the web client's Threads list.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct ThreadView {
    threads: Vec<ViewThread>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct ViewThread {
    root_msg: Option<PlacedMessage>,
    latest_replies: Vec<PlacedMessage>,
    unread_replies: Vec<PlacedMessage>,
}

/// A message that says which conversation it is in.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct PlacedMessage {
    #[serde(flatten)]
    message: types::Message,
    channel: String,
}

/// The threads of the web client's list, as the view shows them.
fn viewed_threads(view: ThreadView) -> Vec<Followed> {
    view.threads
        .into_iter()
        .filter_map(|thread| {
            let root = thread.root_msg?;
            let channel = Some(root.channel).filter(|c| !c.is_empty()).or_else(|| {
                thread
                    .latest_replies
                    .iter()
                    .map(|r| r.channel.clone())
                    .find(|c| !c.is_empty())
            })?;
            let parent = root.message.into_model()?;
            let mut replies: Vec<Message> = thread
                .latest_replies
                .into_iter()
                .filter_map(|r| r.message.into_model())
                .filter(|m| m.ts != parent.ts)
                .collect();
            replies.sort_by(|a, b| a.ts.cmp(&b.ts));
            let extra = replies.len().saturating_sub(views::THREAD_REPLIES);
            replies.drain(..extra);
            Some(Followed {
                channel,
                parent,
                replies,
                unread: u32::try_from(thread.unread_replies.len()).unwrap_or(u32::MAX),
            })
        })
        .collect()
}

/// A thread as `conversations.replies` gives it (the parent first), as
/// the view shows it. What you have not read is what came after you last
/// wrote in it: nothing else keeps a thread's read state.
fn replied_thread(
    channel: &str,
    parent: &Ts,
    messages: Vec<Message>,
    me: &str,
) -> Option<Followed> {
    let mut parent_message = None;
    let mut replies = Vec::new();
    for message in messages {
        if message.ts == *parent {
            parent_message = Some(message);
        } else {
            replies.push(message);
        }
    }
    let parent_message = parent_message?;
    if replies.is_empty() {
        return None;
    }
    replies.sort_by(|a, b| a.ts.cmp(&b.ts));
    let mine = |m: &Message| m.user.as_deref() == Some(me);
    let last_mine = replies
        .iter()
        .rposition(mine)
        .map(|at| at + 1)
        .or_else(|| mine(&parent_message).then_some(0));
    let unread = last_mine.map_or(0, |from| {
        replies[from..].iter().filter(|m| !mine(m)).count()
    });
    let extra = replies.len().saturating_sub(views::THREAD_REPLIES);
    replies.drain(..extra);
    Some(Followed {
        channel: channel.to_owned(),
        parent: parent_message,
        replies,
        unread: u32::try_from(unread).unwrap_or(u32::MAX),
    })
}

/// The threads you follow: the web client's own list for a browser
/// session, else (or when that cannot be read) the threads a search finds
/// you replied in. Answers whether it searched.
async fn threads(client: &Client, me: &str) -> Result<(Vec<Followed>, bool), SlackError> {
    if client.token().is_session() {
        match client
            .call::<ThreadView>(
                "subscriptions.thread.getView",
                &[("limit", THREADS_LIMIT.to_string())],
            )
            .await
        {
            Ok(view) => return Ok((viewed_threads(view), false)),
            Err(error) => {
                log::info!("subscriptions.thread.getView: {error}; searching instead");
            }
        }
    }
    let answer: MessagesAnswer = client
        .call(
            "search.messages",
            &[
                ("query", format!("from:<@{me}> is:thread")),
                ("count", SEARCH_COUNT.to_string()),
                ("sort", "timestamp".to_owned()),
                ("sort_dir", "desc".to_owned()),
            ],
        )
        .await?;
    let mut seen = std::collections::HashSet::new();
    let found: Vec<(String, Ts)> = answer
        .into_page()
        .hits
        .into_iter()
        .filter_map(|hit| {
            let channel = hit.channel?;
            // A reply names its parent; a message of yours that started
            // a thread is the parent.
            let parent = hit.thread.or(hit.ts)?;
            Some((channel, parent))
        })
        .filter(|key| seen.insert(key.clone()))
        .take(SEARCHED_THREADS)
        .collect();
    let read = futures_util::future::join_all(found.iter().map(|(channel, parent)| async move {
        let params = [
            ("channel", channel.clone()),
            ("ts", parent.0.clone()),
            ("limit", THREAD_PAGE.to_string()),
        ];
        match client
            .call::<types::HistoryPage>("conversations.replies", &params)
            .await
        {
            Ok(page) => {
                let messages = page
                    .messages
                    .into_iter()
                    .filter_map(types::Message::into_model)
                    .collect();
                replied_thread(channel, parent, messages, me)
            }
            Err(error) => {
                log::debug!("could not read a thread you replied in: {error}");
                None
            }
        }
    }))
    .await;
    Ok((read.into_iter().flatten().collect(), true))
}

// ---- unreads ----------------------------------------------------------

/// The messages of `channel` after `after`, oldest first, and whether there
/// are more.
async fn unread(
    client: &Client,
    channel: &str,
    after: Option<&Ts>,
) -> Result<(Vec<Message>, bool), SlackError> {
    let mut params = vec![("channel", channel.to_owned())];
    match after {
        Some(after) => {
            params.push(("oldest", after.0.clone()));
            params.push(("limit", UNREAD_COUNT.to_string()));
        }
        None => params.push(("limit", UNMARKED_COUNT.to_string())),
    }
    let page: types::HistoryPage = client.call("conversations.history", &params).await?;
    Ok(unread_page(page, after))
}

/// A page of history as the unreads list shows it: oldest first, without
/// the message you last read.
fn unread_page(page: types::HistoryPage, after: Option<&Ts>) -> (Vec<Message>, bool) {
    let mut messages: Vec<Message> = page
        .messages
        .into_iter()
        .filter_map(types::Message::into_model)
        .filter(|m| after.is_none_or(|after| m.ts > *after))
        .collect();
    messages.sort_by(|a, b| a.ts.cmp(&b.ts));
    (messages, page.has_more)
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
    fn unread_messages_come_oldest_first_after_the_read_marker() {
        let page: types::HistoryPage = serde_json::from_str(
            r#"{"ok":true,"has_more":true,"messages":[
              {"type":"message","ts":"3.000100","user":"U1","text":"c"},
              {"type":"message","ts":"2.000100","user":"U1","text":"b"},
              {"type":"message","ts":"1.000100","user":"U1","text":"a"}
            ]}"#,
        )
        .expect("parses");
        let (messages, more) = unread_page(page, Some(&Ts::new("1.000100")));
        let texts: Vec<&str> = messages.iter().map(|m| m.text.as_str()).collect();
        assert_eq!(texts, ["b", "c"]);
        assert!(more);
    }

    #[test]
    fn the_threads_list_reads_with_its_replies() {
        let view: ThreadView = serde_json::from_str(
            r#"{"ok":true,"has_more":false,"threads":[
              {"root_msg":{"type":"message","ts":"1.000100","user":"U0","text":"plan",
                 "thread_ts":"1.000100","reply_count":4,"channel":"C1"},
               "latest_replies":[
                 {"type":"message","ts":"5.000100","user":"U2","text":"e","thread_ts":"1.000100","channel":"C1"},
                 {"type":"message","ts":"2.000100","user":"U1","text":"b","thread_ts":"1.000100","channel":"C1"},
                 {"type":"message","ts":"3.000100","user":"U1","text":"c","thread_ts":"1.000100","channel":"C1"},
                 {"type":"message","ts":"4.000100","user":"U1","text":"d","thread_ts":"1.000100","channel":"C1"}],
               "unread_replies":[{"type":"message","ts":"5.000100","channel":"C1"}]},
              {"latest_replies":[]}
            ]}"#,
        )
        .expect("parses");
        let threads = viewed_threads(view);
        assert_eq!(threads.len(), 1);
        let thread = &threads[0];
        assert_eq!(thread.channel, "C1");
        assert_eq!(thread.parent.text, "plan");
        let texts: Vec<&str> = thread.replies.iter().map(|m| m.text.as_str()).collect();
        assert_eq!(texts, ["c", "d", "e"]);
        assert_eq!(thread.unread, 1);
    }

    #[test]
    fn what_came_after_your_last_reply_is_unread() {
        let message = |ts: &str, user: &str| {
            views::bare_message(
                Ts::new(ts),
                Some(user.into()),
                ts.into(),
                Some(Ts::new("1.0")),
            )
        };
        let parent = Ts::new("1.0");
        let thread = replied_thread(
            "C1",
            &parent,
            vec![
                message("1.0", "U1"),
                message("2.0", "U0"),
                message("3.0", "U1"),
                message("4.0", "U2"),
            ],
            "U0",
        )
        .expect("a thread");
        assert_eq!(thread.unread, 2);
        assert_eq!(thread.replies.len(), 3);
        // You started it and someone answered.
        let started = replied_thread(
            "C1",
            &parent,
            vec![message("1.0", "U0"), message("2.0", "U1")],
            "U0",
        )
        .expect("a thread");
        assert_eq!(started.unread, 1);
        // No replies: not a thread.
        assert_eq!(
            replied_thread("C1", &parent, vec![message("1.0", "U0")], "U0"),
            None
        );
    }

    #[test]
    fn scheduled_messages_read_with_their_thread() {
        let page: ScheduledPage = serde_json::from_str(
            r#"{"ok":true,"scheduled_messages":[
              {"id":"Q1","channel_id":"C1","post_at":1700000000,"date_created":1690000000,"text":"hi"},
              {"id":"Q2","channel_id":"C2","post_at":1700000100,"text":"reply","thread_ts":"1.000100"},
              {"id":"","channel_id":"C3","post_at":1,"text":"broken"}
            ],"response_metadata":{"next_cursor":""}}"#,
        )
        .expect("parses");
        assert_eq!(page.response_metadata.cursor(), None);
        let items = scheduled_items(page.scheduled_messages);
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].post_at, 1_700_000_000);
        assert_eq!(items[1].thread, Some(Ts::new("1.000100")));
    }

    #[test]
    fn later_lists_what_is_still_to_do() {
        let list: SavedList = serde_json::from_str(
            r#"{"ok":true,"saved_items":[
              {"item_id":"C1","item_type":"message","ts":"2.000100","state":"in_progress","date_due":0},
              {"item_id":"C1","item_type":"message","ts":"1.000100","state":"completed"},
              {"item_id":"C2","item_type":"message","ts":"3.000100","state":"in_progress","is_archived":true},
              {"item_id":"F1","item_type":"file","ts":"","state":"in_progress"}
            ],"response_metadata":{"next_cursor":""}}"#,
        )
        .expect("parses");
        assert_eq!(
            saved_references(list),
            [("C1".to_owned(), Ts::new("2.000100"))]
        );
        let stars: StarsList = serde_json::from_str(
            r#"{"ok":true,"items":[
              {"type":"message","channel":"C1","message":{"type":"message","ts":"5.000100","user":"U1","text":"star"}},
              {"type":"file","file":{"id":"F1"}}
            ]}"#,
        )
        .expect("parses");
        let starred = starred(stars);
        assert_eq!(starred.len(), 1);
        assert_eq!(starred[0].message.text, "star");
    }

    #[test]
    fn reminders_leave_out_what_is_done() {
        let list: RemindersList = serde_json::from_str(
            r#"{"ok":true,"reminders":[
              {"id":"Rm2","text":"later","time":200,"complete_ts":0},
              {"id":"Rm1","text":"sooner","time":100,"recurring":false},
              {"id":"Rm3","text":"done","time":50,"complete_ts":60},
              {"id":"Rm4","text":"weekly","recurring":true}
            ]}"#,
        )
        .expect("parses");
        let ids: Vec<String> = reminders(list).into_iter().map(|r| r.id).collect();
        assert_eq!(ids, ["Rm1", "Rm2", "Rm4"]);
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
