//! Loading a workspace from Slack: its details, conversations, sidebar,
//! people, unread counts, history and threads.
//!
//! Each runs as a task of its own and reports to the interface, so the
//! worker's loop never waits on the network.

use std::time::Duration;

use serde_json::Value;

use super::api::{failure, paginate, with_cursor, worth_retrying};
use super::{Event, Sink};
use crate::failure::{Doing, Problem};
use crate::model::{Conversation, ConversationKind, Message, Ts, User, Workspace};
use crate::offline::Cache;
use crate::slack::{Client, SlackError, types};

const HISTORY_PAGE: u32 = 50;
/// The most pages read from each listing. Each is far beyond what a
/// workspace normally has; they only stop a cursor that never ends, and
/// hitting one is logged.
/// `users.list`, 200 people a page.
const USER_PAGES: usize = 40;
/// `users.conversations`, 200 conversations a page.
const CONVERSATION_PAGES: usize = 100;
/// `conversations.replies`, 200 replies a page.
const THREAD_PAGES: usize = 50;
/// `users.channelSections.list`.
const SECTION_PAGES: usize = 10;
/// `stars.list`, 200 stars a page.
const STAR_PAGES: usize = 20;

/// A workspace's name, domain and icon.
pub(super) async fn workspace_details(client: &Client, team: &str, user: &str) -> Workspace {
    let mut workspace = Workspace {
        team_id: team.to_owned(),
        name: team.to_owned(),
        domain: String::new(),
        icon: None,
        user_id: user.to_owned(),
    };
    match client.call::<types::TeamInfo>("team.info", &[]).await {
        Ok(info) => {
            workspace.name = info.team.name;
            workspace.domain = info.team.domain;
            if !info.team.icon.image_default {
                workspace.icon = info
                    .team
                    .icon
                    .image_132
                    .or(info.team.icon.image_88)
                    .or(info.team.icon.image_68);
            }
        }
        Err(error) => {
            log::warn!("team.info: {error}");
            if let Ok(test) = client.call::<types::AuthTest>("auth.test", &[]).await {
                workspace.name = test.team;
            }
        }
    }
    workspace
}

/// Everything a workspace needs after signing in: cached lists first, then
/// a check of the token, fresh lists, custom emoji and unread state.
pub(super) async fn boot(client: Client, workspace: Workspace, cache: Cache, sink: Sink) {
    let team = workspace.team_id.clone();
    if let Some(list) = cache.read::<Vec<Conversation>>(&team, "conversations") {
        sink.send(Event::Conversations {
            team: team.clone(),
            list,
            complete: false,
        });
    }
    if let Some(users) = cache.read::<Vec<User>>(&team, "users") {
        sink.send(Event::Users {
            team: team.clone(),
            users,
        });
    }
    match client.call::<types::AuthTest>("auth.test", &[]).await {
        Ok(_) => {}
        Err(error) if error.is_auth() => {
            sink.send(Event::SignedOut {
                team,
                reason: Some(failure(&error)),
            });
            return;
        }
        Err(error) => {
            sink.send(Event::Error(Problem::new(
                Doing::Reach {
                    workspace: workspace.name.clone(),
                },
                failure(&error),
            )));
            return;
        }
    }
    let details = workspace_details(&client, &team, &workspace.user_id).await;
    if details != workspace {
        sink.send(Event::WorkspaceReady(details));
    }
    let list = conversations(client.clone(), team.clone(), cache.clone(), sink.clone()).await;
    match client.call::<types::EmojiList>("emoji.list", &[]).await {
        Ok(list) => sink.send(Event::Emoji {
            team: team.clone(),
            emoji: list.emoji,
        }),
        Err(error) => log::info!("emoji.list: {error}"),
    }
    // Side by side, but inside this task, so stopping the boot on sign-out
    // stops them too.
    let sweep = async {
        if let Some(list) = list {
            unread_sweep(client.clone(), team.clone(), list, sink.clone()).await;
        }
    };
    tokio::join!(
        users(client.clone(), team.clone(), cache, sink.clone()),
        sections(client.clone(), team.clone(), sink.clone()),
        sweep,
    );
}

/// Every conversation you are in.
pub(super) async fn conversations(
    client: Client,
    team: String,
    cache: Cache,
    sink: Sink,
) -> Option<Vec<Conversation>> {
    let mut list = Vec::new();
    let walked = paginate(
        "users.conversations",
        CONVERSATION_PAGES,
        |cursor| {
            let params = with_cursor(
                vec![
                    ("types", "public_channel,private_channel,mpim,im".to_owned()),
                    ("exclude_archived", "true".to_owned()),
                    ("limit", "200".to_owned()),
                ],
                cursor,
            );
            let client = &client;
            async move {
                let page: types::ConversationsPage =
                    client.call("users.conversations", &params).await?;
                let next = page.response_metadata.cursor();
                Ok((page.channels, next))
            }
        },
        |channels| {
            list.extend(channels.into_iter().map(types::Channel::into_model));
            true
        },
    )
    .await;
    if let Err(error) = walked {
        sink.send(Event::Error(Problem::new(
            Doing::ListConversations,
            failure(&error),
        )));
        return None;
    }
    cache.write(&team, "conversations", &list);
    sink.send(Event::Conversations {
        team,
        list: list.clone(),
        complete: true,
    });
    Some(list)
}

/// Your sidebar sections and starred conversations, as Slack's own client
/// gets them. `users.channelSections.list` is undocumented and only answers
/// browser sessions; anything else keeps the plain sidebar.
/// Workspaces whose unsent section channels were logged already.
static SECTIONS_NOTED: std::sync::Mutex<Vec<String>> = std::sync::Mutex::new(Vec::new());

/// Whether `key` is new to `seen`, remembering it.
fn first_time(seen: &std::sync::Mutex<Vec<String>>, key: &str) -> bool {
    let mut seen = seen
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if seen.iter().any(|k| k == key) {
        return false;
    }
    seen.push(key.to_owned());
    true
}

pub(super) async fn sections(client: Client, team: String, sink: Sink) {
    // The web client sends the token in the form; do the same.
    let token = client.token().access;
    let mut all = Vec::new();
    let walked = paginate(
        "users.channelSections.list",
        SECTION_PAGES,
        |cursor| {
            let params = with_cursor(vec![("token", token.clone())], cursor);
            let client = &client;
            async move {
                let page: types::ChannelSectionsPage =
                    client.call("users.channelSections.list", &params).await?;
                Ok((page.channel_sections, page.cursor))
            }
        },
        // Slack has been seen answering every page with the same sections
        // and a fresh cursor: keep each section once, and stop at a page
        // that brings nothing new.
        |sections| {
            let before = all.len();
            for section in sections {
                if !all.iter().any(|s: &types::ChannelSection| {
                    s.channel_section_id == section.channel_section_id
                }) {
                    all.push(section);
                }
            }
            all.len() > before
        },
    )
    .await;
    if let Err(error) = walked {
        log::info!("no sidebar sections ({error}); using the plain sidebar");
        return;
    }
    // Slack files more channels in a section than it sends (archived and
    // left ones) and offers no call for the rest (see
    // `types::ChannelIdsPage`). Say so once per workspace and run; a
    // channel missing from its section still shows under Channels.
    let unsent: usize = all
        .iter()
        .filter_map(|section| section.channel_ids_page.unsent())
        .sum();
    if unsent > 0 && first_time(&SECTIONS_NOTED, &team) {
        log::info!(
            "sidebar sections of {team}: {unsent} filed channels not sent (archived or left, most likely)"
        );
    }
    let mut ordered = types::order_sections(all);
    // Slack leaves Starred empty in the section list; stars.list fills it.
    if let Some(starred) = ordered
        .iter_mut()
        .find(|s| s.kind == crate::model::SectionKind::Starred)
    {
        let mut items = Vec::new();
        let walked = paginate(
            "stars.list",
            STAR_PAGES,
            |cursor| {
                let params = with_cursor(
                    vec![("token", token.clone()), ("limit", "200".into())],
                    cursor,
                );
                let client = &client;
                async move {
                    let page: types::StarsList = client.call("stars.list", &params).await?;
                    let next = page.response_metadata.cursor();
                    Ok((page.items, next))
                }
            },
            |page| {
                items.extend(page);
                true
            },
        )
        .await;
        match walked {
            Ok(()) => {
                starred.channel_ids = types::StarsList {
                    items,
                    ..Default::default()
                }
                .conversations();
            }
            Err(error) => log::info!("stars.list: {error}"),
        }
    }
    if !ordered.is_empty() {
        sink.send(Event::Sections {
            team,
            sections: ordered,
        });
    }
}

/// Carries out a sidebar edit, in order, stopping at the first failure; then
/// fetches the sections again so the sidebar shows what Slack really has.
pub(super) async fn edit_sidebar(
    client: Client,
    team: String,
    calls: Vec<crate::sidebar::SidebarCall>,
    sink: Sink,
) {
    use crate::sidebar::SidebarCall;
    let token = client.token().access;
    let section_channels = |section: &str, channel: &str| {
        serde_json::json!([{ "channel_section_id": section, "channel_ids": [channel] }]).to_string()
    };
    for call in calls {
        let result = match &call {
            SidebarCall::Create {
                name,
                channel,
                remove_from,
            } => {
                let created = client
                    .act::<Value>(
                        "users.channelSections.create",
                        &[
                            ("token", token.clone()),
                            ("name", name.clone()),
                            ("emoji", String::new()),
                            ("type", "standard".into()),
                        ],
                    )
                    .await;
                match (created, channel) {
                    (Ok(answer), Some(channel)) => {
                        let id = answer
                            .pointer("/channel_section/channel_section_id")
                            .and_then(Value::as_str)
                            .map(str::to_owned);
                        match id {
                            Some(id) => {
                                let mut params = vec![
                                    ("token", token.clone()),
                                    ("insert", section_channels(&id, channel)),
                                ];
                                if let Some(from) = remove_from {
                                    params.push(("remove", section_channels(from, channel)));
                                }
                                client
                                    .act::<Value>(
                                        "users.channelSections.channels.bulkUpdate",
                                        &params,
                                    )
                                    .await
                                    .map(|_| ())
                            }
                            None => Err(SlackError::Decode("no id for the new section".into())),
                        }
                    }
                    (Ok(_), None) => Ok(()),
                    (Err(error), _) => Err(error),
                }
            }
            SidebarCall::Set {
                section,
                name,
                next,
            } => {
                let mut params = vec![
                    ("token", token.clone()),
                    ("channel_section_id", section.clone()),
                ];
                if let Some(name) = name {
                    params.push(("name", name.clone()));
                }
                if let Some(next) = next {
                    params.push(("next_channel_section_id", next.clone()));
                }
                client
                    .act::<Value>("users.channelSections.set", &params)
                    .await
                    .map(|_| ())
            }
            SidebarCall::Delete { section } => client
                .act::<Value>(
                    "users.channelSections.delete",
                    &[
                        ("token", token.clone()),
                        ("channel_section_id", section.clone()),
                    ],
                )
                .await
                .map(|_| ()),
            SidebarCall::Channels {
                channel,
                insert,
                remove,
            } => {
                let mut params = vec![("token", token.clone())];
                params.push((
                    "insert",
                    insert
                        .as_deref()
                        .map_or_else(|| "[]".to_owned(), |to| section_channels(to, channel)),
                ));
                params.push((
                    "remove",
                    remove
                        .as_deref()
                        .map_or_else(|| "[]".to_owned(), |from| section_channels(from, channel)),
                ));
                client
                    .act::<Value>("users.channelSections.channels.bulkUpdate", &params)
                    .await
                    .map(|_| ())
            }
            SidebarCall::Star { channel, starred } => {
                let method = if *starred {
                    "stars.add"
                } else {
                    "stars.remove"
                };
                match client
                    .act::<Value>(method, &[("channel", channel.clone())])
                    .await
                {
                    // Already as asked.
                    Err(SlackError::Api(code))
                        if code == "already_starred" || code == "not_starred" =>
                    {
                        Ok(())
                    }
                    other => other.map(|_| ()),
                }
            }
        };
        if let Err(error) = result {
            log::warn!("sidebar edit {call:?} failed: {error}");
            sink.send(Event::Error(Problem::new(
                Doing::ChangeSidebar,
                failure(&error),
            )));
            break;
        }
    }
    sections(client, team, sink).await;
}

/// The workspace's people, page by page, each page shown as it arrives.
async fn users(client: Client, team: String, cache: Cache, sink: Sink) {
    let mut all = Vec::new();
    let walked = paginate(
        "users.list",
        USER_PAGES,
        |cursor| {
            let params = with_cursor(vec![("limit", "200".to_owned())], cursor);
            let client = &client;
            async move {
                let page: types::UsersPage = client.call("users.list", &params).await?;
                let next = page.response_metadata.cursor();
                Ok((page.members, next))
            }
        },
        |members| {
            let users: Vec<User> = members.into_iter().map(types::User::into_model).collect();
            all.extend(users.iter().cloned());
            sink.send(Event::Users {
                team: team.clone(),
                users,
            });
            true
        },
    )
    .await;
    // What arrived before a failure is still worth keeping.
    if let Err(error) = walked {
        log::info!("users.list: {error}");
    }
    if !all.is_empty() {
        cache.write(&team, "users", &all);
    }
}

/// Reads each conversation's read marker and newest message.
///
/// A browser session asks `client.counts`, the web client's own call, for
/// all of them at once. Anything it does not cover, and every OAuth
/// workspace, falls back to one or two calls per conversation, direct
/// messages first, skipping conversations whose state is already known.
/// A rate limit pauses the sweep rather than skipping conversations.
async fn unread_sweep(client: Client, team: String, list: Vec<Conversation>, sink: Sink) {
    let mut list = if client.token().is_session() {
        match client
            .call::<types::ClientCounts>("client.counts", &[])
            .await
        {
            Ok(counts) => {
                let counts = counts.by_id();
                let mut rest = Vec::new();
                for mut conversation in list {
                    match counts.get(&conversation.id) {
                        Some(count) => {
                            apply_count(&mut conversation, count);
                            sink.send(Event::Conversation {
                                team: team.clone(),
                                conversation,
                            });
                        }
                        None => rest.push(conversation),
                    }
                }
                rest
            }
            Err(error) => {
                log::info!("client.counts unavailable ({error}); reading each conversation");
                list
            }
        }
    } else {
        list
    };
    list.retain(|c| c.latest.is_none() || c.last_read.is_none());
    list.sort_by_key(|c| match c.kind {
        ConversationKind::Direct | ConversationKind::Group => 0,
        ConversationKind::Private => 1,
        ConversationKind::Channel => 2,
    });
    for conversation in list {
        let mut pause = SWEEP_PAUSE;
        for attempt in 1.. {
            match fetch_conversation(&client, &team, &conversation.id, &sink).await {
                Err(SlackError::RateLimited) if attempt < SWEEP_ATTEMPTS => {
                    log::debug!("unread sweep rate limited; pausing for {pause:?}");
                    tokio::time::sleep(pause).await;
                    pause *= 2;
                }
                Err(error) => {
                    log_fetch_failure("conversations.info", &conversation.id, &error);
                    break;
                }
                Ok(()) => break,
            }
        }
    }
}

/// How long the unread sweep first waits out a rate limit, and how many
/// times it tries one conversation.
const SWEEP_PAUSE: Duration = Duration::from_secs(15);
const SWEEP_ATTEMPTS: u32 = 4;

/// Puts `client.counts`' read state for one conversation onto it.
fn apply_count(conversation: &mut Conversation, count: &types::CountEntry) {
    if let Some(last_read) = types::real_ts(&count.last_read) {
        conversation.last_read = Some(last_read);
    }
    if let Some(latest) = types::real_ts(&count.latest) {
        conversation.latest = Some(latest);
    }
    conversation.mentions = count.mention_count;
}

/// Logs a failed background fetch: quietly for a passing failure, more
/// loudly when Slack refused, which points at something to fix.
fn log_fetch_failure(method: &str, id: &str, error: &SlackError) {
    if worth_retrying(error) {
        log::debug!("{method} {id}: {error}");
    } else {
        log::info!("{method} {id}: {error}");
    }
}

/// Fetches one conversation's details and sends them on.
pub(super) async fn conversation_info(client: Client, team: String, channel: String, sink: Sink) {
    if let Err(error) = fetch_conversation(&client, &team, &channel, &sink).await {
        log_fetch_failure("conversations.info", &channel, &error);
    }
}

/// One conversation's details, with its newest message when the details
/// lack it; a conversation that is gone is reported as gone.
async fn fetch_conversation(
    client: &Client,
    team: &str,
    channel: &str,
    sink: &Sink,
) -> Result<(), SlackError> {
    match client
        .call::<types::ChannelInfo>("conversations.info", &[("channel", channel.to_owned())])
        .await
    {
        Ok(info) => {
            let mut conversation = info.channel.into_model();
            if conversation.latest.is_none()
                && let Ok(page) = client
                    .call::<types::HistoryPage>(
                        "conversations.history",
                        &[("channel", channel.to_owned()), ("limit", "1".into())],
                    )
                    .await
            {
                conversation.latest = page.messages.first().map(|m| Ts::new(m.ts.clone()));
            }
            sink.send(Event::Conversation {
                team: team.to_owned(),
                conversation,
            });
            Ok(())
        }
        Err(SlackError::Api(code)) if code == "channel_not_found" => {
            sink.send(Event::ConversationGone {
                team: team.to_owned(),
                channel: channel.to_owned(),
            });
            Ok(())
        }
        Err(error) => Err(error),
    }
}

/// A page of history: the newest one, or the one before `cursor`.
///
/// For the newest page, the copy in the offline cache goes out first, so
/// the conversation shows at once (and without a network); Slack's answer
/// then replaces it and is kept for next time.
pub(super) async fn history(
    client: Client,
    team: String,
    channel: String,
    cursor: Option<String>,
    cache: Cache,
    sink: Sink,
) {
    let newest = cursor.is_none() && cache.is_enabled();
    if newest {
        let (cached_team, cached_channel, reader) = (team.clone(), channel.clone(), cache.clone());
        let cached =
            tokio::task::spawn_blocking(move || reader.read_history(&cached_team, &cached_channel))
                .await
                .ok()
                .flatten()
                .and_then(|value| serde_json::from_value::<types::HistoryPage>(value).ok());
        if let Some(page) = cached {
            let (messages, has_more, cursor) = history_page(page);
            sink.send(Event::CachedHistory {
                team: team.clone(),
                channel: channel.clone(),
                messages,
                has_more,
                cursor,
            });
        }
    }
    let mut params = vec![
        ("channel", channel.clone()),
        ("limit", HISTORY_PAGE.to_string()),
        ("include_all_metadata", "false".to_owned()),
    ];
    let older = cursor.is_some();
    if let Some(cursor) = cursor {
        params.push(("cursor", cursor));
    }
    // As Slack sent it, so the cache keeps the page exactly and reads it
    // back the same way.
    let answer = client
        .call::<Value>("conversations.history", &params)
        .await
        .and_then(|value| {
            serde_json::from_value::<types::HistoryPage>(value.clone())
                .map(|page| (page, value))
                .map_err(|error| SlackError::Decode(error.to_string()))
        });
    match answer {
        Ok((page, value)) => {
            // A huddle still going on shows on its conversation. Only the
            // newest page can hold one.
            if !older
                && let Some(event) = super::people::huddle_in_history(&channel, &page.messages)
            {
                sink.send(Event::People {
                    team: team.clone(),
                    event,
                });
            }
            let (messages, has_more, cursor) = history_page(page);
            sink.send(Event::History {
                team: team.clone(),
                channel: channel.clone(),
                messages,
                has_more,
                cursor,
                older,
            });
            if newest {
                let _ = tokio::task::spawn_blocking(move || {
                    cache.write_history(&team, &channel, &value);
                })
                .await;
            }
        }
        Err(error) => sink.send(Event::HistoryFailed {
            team,
            channel,
            error: failure(&error),
        }),
    }
}

/// A history page's messages, oldest first, and whether and how to read
/// on.
fn history_page(page: types::HistoryPage) -> (Vec<Message>, bool, Option<String>) {
    let cursor = page.response_metadata.cursor();
    let mut messages: Vec<Message> = page
        .messages
        .into_iter()
        .filter_map(types::Message::into_model)
        .collect();
    messages.reverse();
    (messages, page.has_more, cursor)
}

pub(super) async fn thread(client: Client, team: String, channel: String, ts: Ts, sink: Sink) {
    let mut messages = Vec::new();
    let walked = paginate(
        "conversations.replies",
        THREAD_PAGES,
        |cursor| {
            let params = with_cursor(
                vec![
                    ("channel", channel.clone()),
                    ("ts", ts.0.clone()),
                    ("limit", "200".to_owned()),
                ],
                cursor,
            );
            let client = &client;
            async move {
                let page: types::HistoryPage =
                    client.call("conversations.replies", &params).await?;
                let next = page.response_metadata.cursor();
                Ok((page.messages, next))
            }
        },
        |page| {
            messages.extend(page.into_iter().filter_map(types::Message::into_model));
            true
        },
    )
    .await;
    if let Err(error) = walked {
        sink.send(Event::Error(Problem::new(
            Doing::LoadThread,
            failure(&error),
        )));
        return;
    }
    sink.send(Event::Thread {
        team,
        channel,
        ts,
        messages,
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counts_fill_in_the_read_state() {
        let counts: types::ClientCounts = serde_json::from_str(
            r#"{"ok":true,
                "channels":[{"id":"C1","last_read":"1.0","latest":"2.0","mention_count":3,"has_unreads":true}],
                "mpims":[{"id":"G1","last_read":"0000000000.000000","latest":"","mention_count":0}],
                "ims":[{"id":"D1","last_read":"5.0","latest":"5.0"}]}"#,
        )
        .expect("parses");
        let counts = counts.by_id();
        assert_eq!(counts.len(), 3);
        let mut channel = serde_json::from_str::<types::Channel>(r#"{"id":"C1","name":"general"}"#)
            .expect("parses")
            .into_model();
        apply_count(&mut channel, &counts["C1"]);
        assert_eq!(channel.last_read, Some(Ts::new("1.0")));
        assert_eq!(channel.latest, Some(Ts::new("2.0")));
        assert_eq!(channel.mentions, 3);
        assert!(channel.has_unread());
        // Slack's "never" markers are no read state at all.
        let mut group = Conversation {
            id: "G1".into(),
            ..channel.clone()
        };
        group.last_read = None;
        group.latest = None;
        apply_count(&mut group, &counts["G1"]);
        assert_eq!((group.last_read, group.latest), (None, None));
    }
}
