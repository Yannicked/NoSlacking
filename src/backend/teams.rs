//! Microsoft Teams behind the worker: a signed-in Teams workspace, its
//! live connection (Trouter), and the work each command asks of it.
//!
//! The worker routes by workspace (see `worker::Backend`); everything here
//! is the Teams side of that `match`, shaped like the Slack functions in
//! `backend::fetch` so a worker arm is one call. Like them, these report
//! to the workspace's gated sink and never hold the worker.

use std::sync::Arc;

use futures_util::{SinkExt, StreamExt};
use tokio_tungstenite::tungstenite::Message as WsMessage;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;

use super::teams_translate::{
    clean_teams_user_id, other_in_pair, reaction_key, teams_id_to_ts, translate_conversation,
    translate_message, translate_team, translate_user, ts_to_teams_id,
};
use super::{Change, Event, Gate, SignIn, Sink, Socket};
use crate::credentials::Credentials;
use crate::failure::Failure;
use crate::model::{Service, Ts, Workspace};
use crate::teams::auth::{self, TeamsCredentials};
use crate::teams::client::TeamsClient;
use crate::teams::socket::{TrouterEvent, handle_frame_control, parse_frame};

/// How many messages a page of history asks for.
const PAGE: usize = 50;
/// How many channel posts a channel's first page holds, as the web client
/// asks (recorded).
const POSTS_PAGE: usize = 20;
/// How long to wait before connecting again after Trouter failed.
const RETRY: std::time::Duration = std::time::Duration::from_secs(5);

/// Where the Trouter task reports its status: to the worker, which shows
/// it like a Slack socket's.
pub type Report = Arc<dyn Fn(Socket) + Send + Sync>;

/// A signed-in Microsoft Teams workspace.
pub struct Session {
    pub client: TeamsClient,
    pub workspace: Workspace,
    /// What this workspace's tasks report through; closed on sign-out.
    pub sink: Sink,
    gate: Gate,
    boot: tokio::task::AbortHandle,
    trouter: tokio::task::AbortHandle,
    /// The Trouter start this session's status reports come from, so a
    /// replaced session's last words are ignored.
    pub generation: u64,
    /// What Trouter last reported.
    pub status: Socket,
}

impl Session {
    /// Starts a workspace: its first lists, and its live connection.
    pub fn start(
        workspace: Workspace,
        client: TeamsClient,
        (sink, gate): (Sink, Gate),
        generation: u64,
        report: Report,
    ) -> Self {
        let team = workspace.team_id.clone();
        if let Some(name) = own_name(&workspace) {
            client.set_own_name(name);
        }
        let boot =
            tokio::spawn(boot(client.clone(), workspace.clone(), sink.clone())).abort_handle();
        let trouter =
            tokio::spawn(trouter(team, client.clone(), sink.clone(), report)).abort_handle();
        Self {
            client,
            workspace,
            sink,
            gate,
            boot,
            trouter,
            generation,
            status: Socket::Connecting,
        }
    }

    /// A session with nothing running, for tests: they never reach the
    /// network.
    #[cfg(test)]
    pub fn idle(
        workspace: Workspace,
        client: TeamsClient,
        (sink, gate): (Sink, Gate),
        generation: u64,
    ) -> Self {
        Self {
            client,
            workspace,
            sink,
            gate,
            boot: tokio::spawn(async {}).abort_handle(),
            trouter: tokio::spawn(async {}).abort_handle(),
            generation,
            status: Socket::Connecting,
        }
    }

    /// Starts the lists and live connection again, on the same client and
    /// sink: the client's renewals report through that sink, so it has to
    /// stay open. Reports from the old connection carry the old
    /// generation and are ignored.
    pub fn restart(&mut self, generation: u64, report: Report) {
        self.boot.abort();
        self.trouter.abort();
        let team = self.workspace.team_id.clone();
        self.boot = tokio::spawn(boot(
            self.client.clone(),
            self.workspace.clone(),
            self.sink.clone(),
        ))
        .abort_handle();
        self.trouter = tokio::spawn(trouter(
            team,
            self.client.clone(),
            self.sink.clone(),
            report,
        ))
        .abort_handle();
        self.generation = generation;
        self.status = Socket::Connecting;
    }

    /// Stops everything still running for this workspace.
    pub fn shut(&self) {
        self.gate.close();
        self.boot.abort();
        self.trouter.abort();
    }
}

/// A client for `creds` that saves every renewal to the keyring, in
/// order, and signs the workspace out when Microsoft no longer takes the
/// refresh token, as the Slack client does for its own.
pub fn client(
    creds: TeamsCredentials,
    team: &str,
    credentials: Credentials,
    sink: Sink,
) -> TeamsClient {
    let team = team.to_owned();
    TeamsClient::new(creds).with_save(move |result| {
        let credentials = credentials.clone();
        let sink = sink.clone();
        let team = team.clone();
        async move {
            match result {
                Ok(creds) => {
                    if let Err(error) = credentials.save_teams_token(&team, &creds).await {
                        log::warn!("could not store the renewed Teams token: {error}");
                    }
                }
                Err(Failure::SignedOut) => sink.send(Event::SignedOut {
                    team,
                    reason: Some(Failure::SignedOut),
                }),
                Err(_) => {}
            }
        }
    })
}

/// Lists the chats, then the teams and their channels, then you.
async fn boot(client: TeamsClient, workspace: Workspace, sink: Sink) {
    let team = workspace.team_id.clone();
    let me = workspace.user_id.clone();
    if let Err(err) = client.ensure_fresh_tokens().await {
        log::warn!("could not renew the Teams tokens of {team}: {err:?}");
    }
    name_yourself(&client, workspace, &sink).await;

    match client.get_conversations(PAGE).await {
        Ok(chats) => {
            let (list, users, profiles) = chat_list(&client, &chats, &me).await;
            if !users.is_empty() {
                sink.send(people_event(&client, &team, users, &profiles));
            }
            sink.send(Event::Conversations {
                team: team.clone(),
                list,
                complete: true,
            });
        }
        Err(err) => log::warn!("failed to get Teams conversations of {team}: {err:?}"),
    }

    // Chat first, as the Teams app opens on it, then a section per team
    // with its channels. A personal account has chats only.
    let mut sections = vec![chat_section()];
    match client.get_teams().await {
        Ok(teams) => {
            let (teams, channels): (Vec<_>, Vec<_>) = teams
                .iter()
                .map(|t| {
                    let (mut section, channels) = translate_team(t);
                    section.icon = team_picture(&client, &team, t);
                    (section, channels)
                })
                .unzip();
            let channels: Vec<_> = channels.into_iter().flatten().collect();
            if !channels.is_empty() {
                sink.send(Event::Conversations {
                    team: team.clone(),
                    list: channels,
                    complete: false,
                });
            }
            sections.extend(teams);
        }
        Err(err) => log::warn!("failed to get the teams list of {team}: {err:?}"),
    }
    sink.send(Event::Sections {
        team: team.clone(),
        sections,
    });

    match client.get_me() {
        Ok(me) => sink.send(people_event(
            &client,
            &team,
            vec![translate_user(&me)],
            std::slice::from_ref(&me),
        )),
        Err(err) => log::warn!("failed to read who is signed in to {team}: {err:?}"),
    }
}

/// The chat list as the sidebar should show it. Teams names a chat by its
/// topic, if it has one; otherwise by who is in it, which the list does not
/// say. So for each chat without a topic its members are asked for: a
/// one-to-one chat becomes the other person's (named as a direct message
/// is), a group is named after its people, and a chat with nobody else
/// and no name, such as Teams' own streams, is left out. Answers the people
/// it learned the names of too, and the profiles read of them.
async fn chat_list(
    client: &TeamsClient,
    chats: &[crate::teams::types::Conversation],
    me: &str,
) -> (
    Vec<crate::model::Conversation>,
    Vec<crate::model::User>,
    Vec<crate::teams::types::UserDetails>,
) {
    use crate::model::ConversationKind;
    let named = |chat: &crate::teams::types::Conversation| {
        chat.thread_properties
            .as_ref()
            .and_then(|p| p.topic.as_deref())
            .is_some_and(|topic| !topic.trim().is_empty())
    };
    let mut list: Vec<crate::model::Conversation> =
        chats.iter().map(translate_conversation).collect();
    // Who else is in each chat that needs it, asked all at once.
    let wanted: Vec<usize> = chats
        .iter()
        .enumerate()
        // Only chat threads have members to list; Teams' own streams
        // (`48:…`) are refused.
        .filter(|(i, chat)| {
            chat.id.starts_with("19:") && list[*i].kind != ConversationKind::Channel && !named(chat)
        })
        .map(|(i, _)| i)
        .collect();
    let members: Vec<Vec<String>> = futures_util::future::join_all(wanted.iter().map(|&i| {
        let id = chats[i].id.clone();
        async move {
            if let Some(other) = other_in_pair(&id, me) {
                return vec![other];
            }
            match client.get_members(&id).await {
                Ok(mris) => mris
                    .iter()
                    .filter_map(|mri| clean_teams_user_id(mri))
                    .filter(|id| id != me)
                    .collect(),
                Err(error) => {
                    log::info!("could not list a Teams chat's members: {error:?}");
                    Vec::new()
                }
            }
        }
    }))
    .await;
    let mut everyone: Vec<String> = members.iter().flatten().cloned().collect();
    everyone.sort();
    everyone.dedup();
    let found = if everyone.is_empty() {
        Vec::new()
    } else {
        client.get_users(&everyone).await.unwrap_or_else(|error| {
            log::info!("could not look up the people in Teams chats: {error:?}");
            Vec::new()
        })
    };
    let mut names: std::collections::HashMap<String, String> = found
        .iter()
        .filter_map(|u| u.display_name.clone().map(|name| (u.id.clone(), name)))
        .collect();
    let mut users: Vec<crate::model::User> = found.iter().map(translate_user).collect();

    // Where the people service gave no names (it refuses personal
    // accounts), the chats' own messages do: each carries its sender's
    // name. So a chat whose people are still unnamed has its newest page
    // read, and its recent writers stand in for its members if those are
    // not known either.
    let unnamed: Vec<usize> = wanted
        .iter()
        .zip(&members)
        .filter(|(_, others)| others.is_empty() || others.iter().any(|id| !names.contains_key(id)))
        .map(|(&i, _)| i)
        .collect();
    let pages = futures_util::future::join_all(
        unnamed
            .iter()
            .map(|&i| client.get_messages(&chats[i].id, None, PAGE)),
    )
    .await;
    let mut writers: std::collections::HashMap<usize, Vec<String>> =
        std::collections::HashMap::new();
    for (&i, page) in unnamed.iter().zip(pages) {
        let Ok(page) = page else {
            continue;
        };
        let people = senders(&page.messages);
        // Newest first, as a chat's name lists who is active in it.
        let ids: Vec<String> = people
            .iter()
            .rev()
            .map(|u| u.id.clone())
            .filter(|id| id != me)
            .collect();
        for person in people {
            names
                .entry(person.id.clone())
                .or_insert_with(|| person.display_name.clone());
            users.push(person);
        }
        writers.insert(i, ids);
    }

    let mut drop = Vec::new();
    for (&i, others) in wanted.iter().zip(&members) {
        let recent = writers.get(&i).map_or(&[][..], Vec::as_slice);
        let people = if others.is_empty() {
            recent
        } else {
            others.as_slice()
        };
        let conversation = &mut list[i];
        match (conversation.kind, people) {
            (ConversationKind::Direct, [other, ..]) => conversation.user = Some(other.clone()),
            (_, []) if conversation.name == conversation.id => drop.push(i),
            (_, []) => {}
            (_, people) => {
                if let Some(name) = group_name(people, &names) {
                    conversation.name = name;
                }
            }
        }
    }
    for i in drop.into_iter().rev() {
        log::debug!("leaving out a Teams chat with no name and nobody else in it");
        list.remove(i);
    }
    users.sort_by(|a, b| a.id.cmp(&b.id));
    users.dedup_by(|a, b| a.id == b.id);
    (list, users, found)
}

/// What Teams calls a group chat without a topic: its people's names, the
/// first three and how many more, or `None` while none is known.
fn group_name(
    people: &[String],
    names: &std::collections::HashMap<String, String>,
) -> Option<String> {
    let known: Vec<&str> = people
        .iter()
        .filter_map(|id| names.get(id))
        .map(String::as_str)
        .filter(|name| !name.trim().is_empty())
        .collect();
    let (first, rest) = known.split_at(known.len().min(3));
    if first.is_empty() {
        return None;
    }
    let more = rest.len() + (people.len() - known.len());
    Some(match more {
        0 => first.join(", "),
        more => format!("{} +{more}", first.join(", ")),
    })
}

/// The newest page of a conversation, or the one at `cursor`.
pub async fn history(
    client: TeamsClient,
    team: String,
    channel: String,
    cursor: Option<String>,
    sink: Sink,
) {
    let older = cursor.is_some();
    // A work channel reads as the web client reads it: its posts with
    // their replies, without the membership lines that fill its history.
    if !older && client.team_of_channel(&channel).is_some() {
        match client.get_channel_posts(&channel, POSTS_PAGE).await {
            Ok(posts) => {
                let raw: Vec<crate::teams::types::Message> = posts
                    .iter()
                    .flat_map(|p| {
                        std::iter::once(p.message.clone()).chain(p.replies.iter().cloned())
                    })
                    .collect();
                let senders = senders(&raw);
                let named: std::collections::HashSet<String> =
                    senders.iter().map(|u| u.id.clone()).collect();
                if !senders.is_empty() {
                    sink.send(people_event(&client, &team, senders, &[]));
                }
                let messages = crate::backend::teams_translate::channel_posts(&posts);
                name_the_rest(&client, &team, &messages, &named, &sink);
                sink.send(Event::History {
                    team,
                    channel,
                    messages,
                    has_more: false,
                    cursor: None,
                    older,
                    polled: false,
                });
                return;
            }
            Err(error) => {
                log::info!(
                    "could not read a Teams channel's posts ({error:?}); reading its history"
                );
            }
        }
    }
    match client.get_messages(&channel, cursor.as_deref(), PAGE).await {
        Ok(page) => {
            let senders = senders(&page.messages);
            let named: std::collections::HashSet<String> =
                senders.iter().map(|u| u.id.clone()).collect();
            if !senders.is_empty() {
                sink.send(people_event(&client, &team, senders, &[]));
            }
            let mut translated: Vec<_> =
                page.messages.iter().filter_map(translate_message).collect();
            crate::backend::teams_translate::thread_posts(&mut translated);
            name_the_rest(&client, &team, &translated, &named, &sink);
            sink.send(Event::History {
                team,
                channel,
                messages: translated,
                has_more: page.older.is_some(),
                cursor: page.older,
                older,
                polled: false,
            });
        }
        Err(error) => sink.send(Event::HistoryFailed {
            team,
            channel,
            error,
        }),
    }
}

/// The people who wrote `messages`, by the name each message carries:
/// Teams sends no user list with history, and this names most authors
/// without a lookup.
fn senders(messages: &[crate::teams::types::Message]) -> Vec<crate::model::User> {
    let mut seen = std::collections::HashSet::new();
    messages
        .iter()
        .filter_map(|m| {
            let id = m.from.as_deref().and_then(clean_teams_user_id)?;
            let name = m.im_display_name.as_deref()?.trim();
            (!name.is_empty() && seen.insert(id.clone())).then(|| {
                translate_user(&crate::teams::types::UserDetails {
                    id,
                    display_name: Some(name.to_owned()),
                    ..Default::default()
                })
            })
        })
        .collect()
}

/// Looks up the people the interface has no name for yet (who reacted,
/// who was added) through Graph. A failure is logged and dropped: the
/// interface then shows their id, as it did before, and asks again later.
pub async fn fetch_users(client: TeamsClient, team: String, ids: Vec<String>, sink: Sink) {
    match client.get_users(&ids).await {
        Ok(found) => {
            let users: Vec<_> = found.iter().map(translate_user).collect();
            if !users.is_empty() {
                sink.send(people_event(&client, &team, users, &found));
            }
        }
        Err(error) => log::warn!(
            "could not look up people in {team} ({} asked): {error:?}",
            ids.len()
        ),
    }
}

/// Finds people for the New message dialog; they arrive among the
/// workspace's people, where its suggestions look.
pub async fn find_people(client: TeamsClient, team: String, query: String, sink: Sink) {
    match client.search_people(&query).await {
        Ok(found) => {
            let users: Vec<_> = found.iter().map(translate_user).collect();
            if !users.is_empty() {
                sink.send(people_event(&client, &team, users, &found));
            }
        }
        Err(error) => sink.send(Event::Convos {
            team,
            event: crate::convos::Event::Failed {
                what: crate::convos::Doing::Open,
                error,
            },
        }),
    }
}

/// Starts a chat with `users` (the dialog has already looked for one with
/// that one person) and opens it, as Slack's `conversations.open` does.
pub async fn open(client: TeamsClient, team: String, me: String, users: Vec<String>, sink: Sink) {
    let chat = match client.create_chat(&me, &users).await {
        Ok(chat) => chat,
        Err(error) => {
            sink.send(Event::Convos {
                team,
                event: crate::convos::Event::Failed {
                    what: crate::convos::Doing::Open,
                    error,
                },
            });
            return;
        }
    };
    let mut conversation = translate_conversation(&crate::teams::types::Conversation {
        id: chat.clone(),
        ..Default::default()
    });
    match users.as_slice() {
        [other] => {
            conversation.kind = crate::model::ConversationKind::Direct;
            conversation.user = Some(other.clone());
        }
        others => {
            conversation.kind = crate::model::ConversationKind::Group;
            let names: std::collections::HashMap<String, String> = client
                .get_users(others)
                .await
                .unwrap_or_default()
                .into_iter()
                .filter_map(|u| u.display_name.map(|name| (u.id, name)))
                .collect();
            if let Some(name) = group_name(others, &names) {
                conversation.name = name;
            }
        }
    }
    conversation.empty = true;
    sink.send(Event::Conversation {
        team: team.clone(),
        conversation,
    });
    sink.send(Event::Convos {
        team,
        event: crate::convos::Event::Opened { channel: chat },
    });
}

/// A channel post's thread, `post` in `channel`: its replies from the
/// post's reply chain, the post first, every reply's thread set to it,
/// as Slack answers for a thread. A reply chain that cannot be read (the
/// address is the web client's, not yet seen in a recording) falls back
/// to the replies the channel's newest messages hold.
pub async fn thread(client: TeamsClient, team: String, channel: String, post: Ts, sink: Sink) {
    let post_id = ts_to_teams_id(&post);
    // The web client's own address for a post's replies first.
    if client.team_of_channel(&channel).is_some() {
        match client.get_post_replies(&channel, &post_id, PAGE).await {
            Ok(replies) if !replies.is_empty() => {
                send_thread(&client, team, channel, post, &replies, &sink);
                return;
            }
            Ok(_) => {}
            Err(error) => log::info!(
                "could not read a Teams post's replies from the teams service ({error:?})"
            ),
        }
    }
    let chain = match client.get_replies(&channel, &post_id, PAGE).await {
        Ok(page) => Ok(page),
        Err(error) => {
            log::info!("could not read a Teams post's replies ({error:?}); looking in the channel");
            client.get_messages(&channel, None, PAGE).await
        }
    };
    let page = match chain {
        Ok(page) => page,
        Err(error) => {
            log::warn!("could not read a Teams post's thread: {error:?}");
            sink.send(Event::Error(crate::failure::Problem {
                doing: crate::failure::Doing::LoadThread,
                failure: error,
            }));
            return;
        }
    };
    send_thread(&client, team, channel, post, &page.messages, &sink);
}

/// Tells the interface a post's thread from `raw`, its replies (and the
/// post, if among them): the post first, every reply's thread set to it.
fn send_thread(
    client: &TeamsClient,
    team: String,
    channel: String,
    post: Ts,
    raw: &[crate::teams::types::Message],
    sink: &Sink,
) {
    let senders = senders(raw);
    let named: std::collections::HashSet<String> = senders.iter().map(|u| u.id.clone()).collect();
    if !senders.is_empty() {
        sink.send(people_event(client, &team, senders, &[]));
    }
    let mut messages: Vec<crate::model::Message> = raw
        .iter()
        .filter_map(translate_message)
        .filter(|m| m.ts == post || m.thread_ts.as_ref() == Some(&post))
        .collect();
    messages.sort_by(|a, b| a.ts.cmp(&b.ts));
    for message in &mut messages {
        message.thread_ts = Some(post.clone());
    }
    crate::backend::teams_translate::thread_posts(&mut messages);
    name_the_rest(client, &team, &messages, &named, sink);
    sink.send(Event::Thread {
        team,
        channel,
        ts: post,
        messages,
    });
}

/// Looks up, in the background, the people `messages` name without
/// having written them: who reacted, and who a system line mentions
/// (`<@id>`). The interface asks only about authors, which a Teams
/// history names itself; these it would show as ids. `named` are known
/// already; each person is asked about once (see
/// [`TeamsClient::not_yet_asked`]).
fn name_the_rest(
    client: &TeamsClient,
    team: &str,
    messages: &[crate::model::Message],
    named: &std::collections::HashSet<String>,
    sink: &Sink,
) {
    let mentioned = messages.iter().flat_map(|m| {
        m.reactions
            .iter()
            .flat_map(|r| r.users.iter().cloned())
            .chain(mentions(&m.text))
    });
    let ids = client.not_yet_asked(mentioned.filter(|id| !named.contains(id)));
    if ids.is_empty() {
        return;
    }
    tokio::spawn(fetch_users(
        client.clone(),
        team.to_owned(),
        ids,
        sink.clone(),
    ));
}

/// The people a message's text mentions as `<@id>`, as system lines do.
fn mentions(text: &str) -> Vec<String> {
    text.split("<@")
        .skip(1)
        .filter_map(|rest| rest.split_once('>'))
        .map(|(id, _)| id.split('|').next().unwrap_or(id).to_owned())
        .filter(|id| !id.is_empty())
        .collect()
}

/// People for the interface, each with their picture: Teams serves it
/// from the middle tier by the person's MRI, fetched with the workspace's
/// own sign-in (see [`crate::images`]). `profiles` are what was read of
/// them, whose photo address (`imageUri`) the picture service needs for a
/// personal account's own photo.
fn people_event(
    client: &TeamsClient,
    team: &str,
    mut users: Vec<crate::model::User>,
    profiles: &[crate::teams::types::UserDetails],
) -> Event {
    // Someone whose profile had no name would be named by their id, over
    // a name their messages gave: leave them as they were.
    users.retain(|user| user.display_name != user.id);
    client.remember_photos(profiles);
    if let Some(base) = client.credentials().middle_tier_url() {
        for user in users.iter_mut().filter(|u| u.avatar.is_none()) {
            let photo = client.photo(&user.id);
            let url = avatar_url(base, &user.id, Some(&user.real_name), photo.as_deref());
            user.avatar = Some(crate::images::authed(team, &url));
        }
    }
    Event::Users {
        team: team.to_owned(),
        users,
    }
}

/// Where the middle tier at `base` serves the picture of the person with
/// id `id`, at the size the sidebar and messages draw it, asked as the web
/// client asks: with their `name` (for the initials when there is no
/// photo) and their `photo`'s address when the profile gave one.
fn avatar_url(base: &str, id: &str, name: Option<&str>, photo: Option<&str>) -> String {
    let path = format!(
        "{}/beta/users/{}/profilepicturev2",
        base.trim_end_matches('/'),
        crate::teams::client::user_mri(id)
    );
    let Ok(mut url) = reqwest::Url::parse(&path) else {
        return format!("{path}?size=HR64x64");
    };
    {
        let mut query = url.query_pairs_mut();
        if let Some(name) = name.filter(|n| !n.is_empty()) {
            query.append_pair("displayname", name);
        }
        if let Some(photo) = photo {
            query.append_pair("imageUri", photo);
        }
        query.append_pair("size", "HR64x64");
    }
    url.into()
}

/// Asks for the presence of `users` at once and reports it, Teams'
/// availability made the interface's two states: there (available, busy,
/// in a call or meeting, not to be disturbed) or away.
pub async fn presence(client: TeamsClient, team: String, users: Vec<String>, sink: Sink) {
    match client.get_presence(&users).await {
        Ok(found) => {
            log::info!(
                "Teams presence in {team}: {} asked, {} answered",
                users.len(),
                found.len()
            );
            let users: Vec<(String, crate::people::Presence)> = found
                .into_iter()
                .map(|(id, availability)| (id, presence_of(&availability)))
                .collect();
            if !users.is_empty() {
                sink.send(Event::People {
                    team,
                    event: crate::people::Event::Presence { users },
                });
            }
        }
        Err(error) => log::warn!("could not read presence in {team}: {error:?}"),
    }
}

/// The interface's presence for Teams' availability.
fn presence_of(availability: &str) -> crate::people::Presence {
    match availability {
        "Available" | "Busy" | "DoNotDisturb" | "InACall" | "InAConferenceCall" | "InAMeeting"
        | "Presenting" | "Focusing" => crate::people::Presence::Active,
        _ => crate::people::Presence::Away,
    }
}

/// A message to post to a Teams conversation.
pub struct Post {
    pub team: String,
    pub channel: String,
    pub text: String,
    /// The interface's id for its optimistic copy.
    pub local: Ts,
    pub client_msg_id: Option<String>,
    /// Your own user id, as the posted message's author.
    pub me: String,
    /// Your name, which Teams shows with the message, if known.
    pub me_name: Option<String>,
    /// The channel post this replies to, if it is a reply.
    pub thread: Option<Ts>,
}

impl Post {
    fn author(&self) -> crate::teams::client::Author {
        crate::teams::client::Author {
            id: self.me.clone(),
            name: self.me_name.clone(),
        }
    }
}

/// Where a team's picture is, as the work web client asks for it
/// (recorded): the middle tier's `profilepicturev2/teams/{groupId}` under
/// your own id, with the picture's etag, quotes and all.
fn team_picture(
    client: &TeamsClient,
    workspace: &str,
    team: &crate::teams::types::Team,
) -> Option<String> {
    let group = team.site.as_ref()?.group_id.as_deref()?;
    let etag = team.picture_etag.as_deref()?;
    let base = client
        .credentials()
        .middle_tier_url()?
        .trim_end_matches('/')
        .to_owned();
    let me = client.user_from_token()?.id;
    let mut url = reqwest::Url::parse(&format!(
        "{base}/beta/users/{me}/profilepicturev2/teams/{group}"
    ))
    .ok()?;
    url.query_pairs_mut()
        .append_pair("etag", &format!("\"{etag}\""))
        .append_pair("displayName", &team.display_name);
    Some(crate::images::authed(workspace, url.as_str()))
}

/// A Teams workspace's chat section: every 1:1, group and meeting chat
/// not placed elsewhere, most recent first, with the button to start
/// one.
fn chat_section() -> crate::model::SidebarSection {
    crate::model::SidebarSection {
        id: crate::model::TEAMS_CHAT_SECTION.to_owned(),
        kind: crate::model::SectionKind::DirectMessages,
        name: String::new(),
        emoji: String::new(),
        channel_ids: Vec::new(),
        icon: None,
    }
}

/// Learns your own name and picture, for a personal account, whose token
/// carries neither: renames the workspace after you (the interface keeps
/// the new name) and tells the interface who you are.
async fn name_yourself(client: &TeamsClient, mut workspace: Workspace, sink: &Sink) {
    if client.credentials().account != auth::Account::Personal {
        return;
    }
    match client.own_profile().await {
        Ok(me) => {
            if let Some(name) = me.display_name.clone() {
                client.set_own_name(name.clone());
                if workspace.name != name {
                    workspace.name = name;
                    sink.send(Event::WorkspaceReady(workspace.clone()));
                }
            }
            // Messages name you by your MRI (`live:yourname`), the
            // sign-in by your `live:.cid.…`: you are both, and your
            // picture is asked by the MRI, as the web client asks it.
            log::debug!(
                "your Teams profile: {} photo address; messages name you {} the sign-in",
                if me.image_uri.is_some() { "a" } else { "no" },
                if me.id == workspace.user_id {
                    "as"
                } else {
                    "otherwise than"
                },
            );
            client.remember_photos(std::slice::from_ref(&me));
            let mut you = translate_user(&me);
            if let Some(base) = client.credentials().middle_tier_url() {
                let url = avatar_url(
                    base,
                    &me.id,
                    me.display_name.as_deref(),
                    me.image_uri.as_deref(),
                );
                you.avatar = Some(crate::images::authed(&workspace.team_id, &url));
            }
            let mut also = you.clone();
            also.id.clone_from(&workspace.user_id);
            let people = if also.id == you.id {
                vec![you]
            } else {
                vec![you, also]
            };
            sink.send(people_event(client, &workspace.team_id, people, &[]));
        }
        Err(error) => log::info!("could not read your own Teams profile: {error:?}"),
    }
}

/// Your name in a Teams workspace, if the workspace's name is it: a
/// personal sign-in whose name could not be found is called after the
/// service instead, which is no one's name.
fn own_name(workspace: &Workspace) -> Option<String> {
    let fallbacks = [Service::Teams.name(), PERSONAL_FALLBACK];
    (!fallbacks.contains(&workspace.name.as_str())).then(|| workspace.name.clone())
}

/// What a personal workspace is called when your name is not known.
const PERSONAL_FALLBACK: &str = "Teams (personal)";

/// Posts a message; the answer settles the interface's optimistic copy.
pub async fn send(client: TeamsClient, post: Post, sink: Sink) {
    let content = crate::teams::html::wire_to_teams(&post.text);
    let result = client
        .send_message(
            &post.channel,
            post.thread.as_ref().map(ts_to_teams_id).as_deref(),
            &content,
            post.client_msg_id.as_deref(),
            &post.author(),
        )
        .await
        .and_then(|id| posted(id, &post, content));
    sink.send(Event::Sent {
        team: post.team,
        channel: post.channel,
        local: post.local,
        result,
    });
}

/// The message as posted: Teams answers with its id only, so the message
/// is read back through the same translation as any other, which keeps
/// it whole as the model grows.
fn posted(
    id: Option<String>,
    post: &Post,
    content: crate::teams::html::Outgoing,
) -> Result<crate::model::Message, Failure> {
    let id = id.unwrap_or_else(|| ts_to_teams_id(&now()));
    translate_message(&crate::teams::types::Message {
        id,
        from: Some(crate::teams::client::user_mri(&post.me)),
        content: content.html,
        message_type: Some("RichText/Html".into()),
        // Its mentions, so the copy shows them as the sent one will.
        properties: Some(crate::teams::types::MessageProperties {
            mentions: Some(
                content
                    .mentions
                    .into_iter()
                    .map(|m| crate::teams::types::Mention {
                        itemid: m.item.into(),
                        mri: m.mri,
                        display_name: Some(m.name),
                    })
                    .collect(),
            ),
            ..Default::default()
        }),
        client_message_id: post.client_msg_id.clone(),
        ..Default::default()
    })
    .map(|mut message| {
        // A reply shows in its thread at once.
        message.thread_ts.clone_from(&post.thread);
        message
    })
    .ok_or(Failure::NoMessage)
}

/// Now, as a message time stamp.
fn now() -> Ts {
    let millis = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or_default();
    teams_id_to_ts(&millis.to_string())
}

/// Carries out an edit, delete or reaction the interface already shows,
/// and settles it either way.
pub async fn change(
    client: TeamsClient,
    team: String,
    channel: String,
    change: Change,
    me: crate::teams::client::Author,
    sink: Sink,
) {
    let result = match &change {
        Change::Delete { ts, .. } => client.delete_message(&channel, &ts_to_teams_id(ts)).await,
        Change::Edit { ts, text, .. } => {
            let content = crate::teams::html::wire_to_teams(text);
            client
                .edit_message(&channel, &ts_to_teams_id(ts), &content, &me)
                .await
        }
        Change::React { ts, name, added } => {
            client
                .react(&channel, &ts_to_teams_id(ts), &reaction_key(name), *added)
                .await
        }
    };
    sink.send(Event::Settled {
        team,
        channel,
        change,
        result,
    });
}

/// Moves your read marker to `ts`, a message you have seen.
pub async fn mark(client: TeamsClient, team: String, channel: String, ts: Ts) {
    if let Err(error) = client
        .set_consumption_horizon(&channel, &ts_to_teams_id(&ts))
        .await
    {
        // As for Slack's quiet marks: a failed one is retried by the next.
        log::warn!("could not mark {channel} read in {team}: {error:?}");
    }
}

/// Signs in with a device code: shows the code, waits for you to enter
/// it, and trades what Microsoft hands back for a skype token. Answers the
/// workspace and its credentials, which the worker starts and saves.
pub async fn sign_in(
    account: auth::Account,
    tenant: Option<String>,
    sink: Sink,
) -> Result<(Workspace, TeamsCredentials), Failure> {
    let http = crate::slack::net::api();
    // A personal account signs in through its own tenant only.
    let tenant = match account {
        auth::Account::Work => tenant,
        auth::Account::Personal => None,
    };
    let device = auth::start_device_code_flow(&http, account, tenant.as_deref()).await?;
    sink.send(Event::SignIn(SignIn::TeamsDeviceCode {
        user_code: device.user_code.clone(),
        verification_uri: device.verification_uri.clone(),
        message: device.message.clone(),
    }));
    if let Err(error) = open::that_detached(&device.verification_uri) {
        log::info!("could not open the Microsoft sign-in page: {error}");
    }
    let token = auth::poll_device_code_token(
        &http,
        account,
        &device.device_code,
        device.interval,
        device.expires_in,
        tenant.as_deref(),
    )
    .await?;
    sink.send(Event::SignIn(SignIn::Exchanging));

    let claims = auth::parse_jwt_claims(&token.access_token);
    let claim = |name: &str| {
        claims
            .as_ref()
            .and_then(|c| c.get(name))
            .and_then(|v| v.as_str())
            .map(str::to_owned)
    };
    let authz = auth::exchange_skype_token(&http, &token.access_token, account).await?;
    let creds = TeamsCredentials {
        expires_at: token.expires_in.map(|s| auth::now_secs() + s),
        skype_token: authz.skype_token(),
        tenant_id: tenant
            .or_else(|| claim("tid"))
            .or_else(|| Some(account.default_tenant().to_owned())),
        region_gtms: authz.region_gtms,
        access_token: token.access_token,
        refresh_token: token.refresh_token,
        account,
        ..TeamsCredentials::default()
    };
    let client = TeamsClient::new(creds.clone());
    let mut me = client.get_me()?;
    // A personal token says who you are but not your name; your profile
    // does.
    if me.display_name.is_none()
        && let Ok(profile) = client.own_profile().await
    {
        me.display_name = profile.display_name;
    }
    log::info!(
        "signed in to Microsoft Teams ({account:?}): tenant {}",
        claim("tid").as_deref().unwrap_or("-")
    );
    let workspace = Workspace {
        service: Service::Teams,
        team_id: workspace_id(&me.id),
        name: me.display_name.clone().unwrap_or_else(|| match account {
            auth::Account::Work => Service::Teams.name().to_owned(),
            auth::Account::Personal => PERSONAL_FALLBACK.to_owned(),
        }),
        domain: match account {
            auth::Account::Work => "teams.microsoft.com",
            auth::Account::Personal => "teams.live.com",
        }
        .into(),
        icon: None,
        user_id: me.id,
        sign_in: Default::default(),
        scopes: None,
    };
    Ok((workspace, creds))
}

/// The workspace id for the person `me`: `teams_` and their id, with
/// anything but letters, digits and `-` made `_`, since a personal id
/// (`live:.cid.…`) holds characters that other places would not take.
fn workspace_id(me: &str) -> String {
    let id: String = me
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect();
    format!("teams_{id}")
}

/// Keeps the Trouter connection open, reconnecting when it drops, and
/// passes live messages on. Reports each change of status.
pub(crate) async fn trouter(team: String, client: TeamsClient, sink: Sink, report: Report) {
    let http = crate::slack::net::api();
    loop {
        report(Socket::Connecting);
        let ended = trouter_once(&team, &client, &http, &sink, &report).await;
        // A call's callbacks named the connection that just closed.
        client.calls().set_surl(None);
        match ended {
            Ok(()) => log::info!("Trouter closed for {team}, reconnecting"),
            Err(error) => {
                log::warn!("Trouter failed for {team}: {error:?}, retrying");
                report(Socket::Disconnected(error));
            }
        }
        tokio::time::sleep(RETRY).await;
    }
}

/// Registers a personal account's live connection for chat events and
/// incoming calls, with a token fresh enough for it.
async fn register_personal(team: &str, client: &TeamsClient, http: &reqwest::Client, surl: &str) {
    use crate::teams::socket;

    let token = match client.ensure_fresh_tokens().await {
        Ok(tokens) => tokens.skype_token.filter(|t| !t.is_empty()),
        Err(error) => {
            log::info!("no token to register {team} with: {error:?}");
            return;
        }
    };
    let Some(token) = token else {
        return;
    };
    if let Err(error) = socket::register_personal(http, &token, surl).await {
        log::warn!("Skype's registrar did not take {team}: {error:?}");
    }
    if let Err(error) = socket::register_personal_calls(http, &token, surl).await {
        log::warn!("Teams' registrar did not take {team} for calls: {error:?}");
    }
}

/// One Trouter connection, from negotiation until it closes.
async fn trouter_once(
    team: &str,
    client: &TeamsClient,
    http: &reqwest::Client,
    sink: &Sink,
    report: &Report,
) -> Result<(), Failure> {
    use crate::teams::socket;

    let skype_token = client
        .ensure_fresh_tokens()
        .await?
        .skype_token
        .filter(|t| !t.is_empty())
        .ok_or(Failure::SignedOut)?;
    let renew_on_401 = async |error: Failure| {
        if error == Failure::Http(401)
            && let Err(renewal) = client.force_refresh(&skype_token).await
        {
            return renewal;
        }
        error
    };

    let personal = client.credentials().account == auth::Account::Personal;
    let host = if personal {
        socket::PERSONAL_TROUTER
    } else {
        socket::WORK_TROUTER
    };
    let epid = crate::model::new_client_msg_id();
    let session = match socket::negotiate_trouter(http, host, &skype_token, &epid).await {
        Ok(session) => session,
        Err(error) => return Err(renew_on_401(error).await),
    };
    let session_id = match socket::obtain_session_id(http, &session, &skype_token, &epid).await {
        Ok(id) => id,
        Err(error) => return Err(renew_on_401(error).await),
    };
    if personal {
        register_personal(team, client, http, &session.surl).await;
    } else if let Some(registrar) = &session.registrar_url
        && let Err(error) =
            socket::register_endpoint(http, &skype_token, registrar, &session.surl).await
    {
        log::info!("Teams registrar did not take {team}: {error:?}");
    }

    let request = session
        .ws_url(&session_id, &epid)
        .into_client_request()
        .map_err(|error| Failure::Unexpected(error.to_string()))?;
    // Through the proxy, as Slack's sockets go. The error is logged by
    // its kind only: the URL carries the session's signature.
    let (stream, _) = crate::slack::net::websocket(request)
        .await
        .map_err(|error| Failure::Network(error.to_string()))?;

    log::info!("Trouter connected for {team}");
    report(Socket::Connected);
    // Calls name this connection in their callbacks.
    client.calls().set_surl(Some(session.surl.clone()));
    // Teams shows you offline unless an endpoint of yours says otherwise.
    if let Err(error) = client.publish_presence(&epid).await {
        log::info!("could not say you are here in {team}: {error:?}");
    }
    let (mut write, mut read) = stream.split();
    let mut heartbeat = tokio::time::interval(std::time::Duration::from_secs(30));
    heartbeat.tick().await;
    // Said at once, as the web client does, then again and again: without
    // it Teams shows you away a while after the presence above. Each
    // event this connection sends is numbered.
    let mut activity = tokio::time::interval(socket::ACTIVITY_EVERY);
    let mut events: u64 = 0;
    let cv = socket::correlation_vector();
    // A personal account's registrations lapse; renewed well before.
    let mut renew = tokio::time::interval(socket::PERSONAL_REGISTRATION_TTL * 5 / 6);
    renew.tick().await;

    loop {
        tokio::select! {
            _ = renew.tick(), if personal => {
                register_personal(team, client, http, &session.surl).await;
            }
            _ = heartbeat.tick() => {
                write
                    .send(WsMessage::Text("2::".into()))
                    .await
                    .map_err(|error| Failure::Network(error.to_string()))?;
            }
            _ = activity.tick() => {
                events += 1;
                write
                    .send(WsMessage::Text(socket::user_activity(events, &cv).into()))
                    .await
                    .map_err(|error| Failure::Network(error.to_string()))?;
            }
            frame = read.next() => match frame {
                Some(Ok(WsMessage::Text(text))) => {
                    if let Some(answer) = handle_frame_control(&text) {
                        let _ = write.send(WsMessage::Text(answer.into())).await;
                    }
                    match parse_frame(&text) {
                        Some(TrouterEvent::Message { message, changed }) => {
                            live_message(team, client, &message, changed, sink);
                        }
                        Some(TrouterEvent::Call { path, body }) => {
                            client.calls().deliver(&path, &body);
                        }
                        Some(TrouterEvent::CallNotification(body)) => {
                            client.calls().ring(&body);
                        }
                        _ => {}
                    }
                }
                Some(Ok(WsMessage::Ping(payload))) => {
                    let _ = write.send(WsMessage::Pong(payload)).await;
                }
                Some(Ok(WsMessage::Close(_))) | None => return Ok(()),
                Some(Err(error)) => return Err(Failure::Network(error.to_string())),
                Some(Ok(_)) => {}
            },
        }
    }
}

/// A message Trouter pushed: new, edited, or deleted.
fn live_message(
    team: &str,
    client: &TeamsClient,
    message: &crate::teams::types::Message,
    changed: bool,
    sink: &Sink,
) {
    let channel = message.conversation_id.clone().unwrap_or_default();
    if let [sender] = senders(std::slice::from_ref(message)).as_slice() {
        sink.send(people_event(client, team, vec![sender.clone()], &[]));
    }
    let Some(translated) = translate_message(message) else {
        return;
    };
    name_the_rest(
        client,
        team,
        std::slice::from_ref(&translated),
        &Default::default(),
        sink,
    );
    if message.properties.as_ref().is_some_and(|p| p.is_deleted()) {
        sink.send(Event::Deleted {
            team: team.to_owned(),
            channel,
            ts: translated.ts,
        });
    } else {
        sink.send(Event::Message {
            team: team.to_owned(),
            channel,
            changed: changed || message.properties.as_ref().is_some_and(|p| p.is_edited()),
            message: translated,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn post(client_msg_id: Option<&str>) -> Post {
        Post {
            team: "teams_me".into(),
            channel: "19:abc@thread.v2".into(),
            text: "hello".into(),
            local: Ts::new("1.000000"),
            client_msg_id: client_msg_id.map(str::to_owned),
            me: "me".into(),
            me_name: None,
            thread: None,
        }
    }

    #[test]
    fn a_reply_shows_in_its_thread_at_once() {
        let mut reply = post(None);
        reply.thread = Some(teams_id_to_ts("1700000000000"));
        let message = posted(
            Some("1700000000999".into()),
            &reply,
            crate::teams::html::wire_to_teams("me too"),
        )
        .expect("a message");
        assert!(message.is_reply());
        assert_eq!(message.thread_ts, Some(teams_id_to_ts("1700000000000")));
    }

    #[test]
    fn a_posted_message_is_yours_and_carries_its_ids() {
        let message = posted(
            Some("1700000000123".into()),
            &post(Some("c-1")),
            crate::teams::html::wire_to_teams("hello"),
        )
        .expect("a message");
        assert_eq!(message.ts, teams_id_to_ts("1700000000123"));
        assert_eq!(message.user.as_deref(), Some("me"));
        assert_eq!(message.client_msg_id.as_deref(), Some("c-1"));
        assert!(message.text.contains("hello"), "{}", message.text);
    }

    #[test]
    fn a_posted_mention_shows_the_person_at_once() {
        let message = posted(
            Some("1".into()),
            &post(None),
            crate::teams::html::wire_to_teams("hi <@live:ana|Ana>"),
        )
        .expect("a message");
        let shown = format!("{:?}", message.blocks);
        assert!(shown.contains("live:ana"), "{shown}");
        assert!(shown.contains("Ana"), "{shown}");
    }

    #[test]
    fn a_post_without_an_id_gets_a_time_of_its_own() {
        let message =
            posted(None, &post(None), crate::teams::html::wire_to_teams("hi")).expect("a message");
        assert_ne!(message.ts, Ts::new(""));
        assert_eq!(message.user.as_deref(), Some("me"));
    }

    #[test]
    fn history_names_its_authors_once_each() {
        let message = |from: &str, name: &str| crate::teams::types::Message {
            id: "1".into(),
            from: Some(from.into()),
            im_display_name: Some(name.into()),
            ..Default::default()
        };
        let users = senders(&[
            message("8:orgid:a", "Alice"),
            message("8:orgid:a", "Alice"),
            message("8:orgid:b", " "),
            message("19:x@thread.v2", "A thread"),
        ]);
        let names: Vec<(&str, &str)> = users
            .iter()
            .map(|u| (u.id.as_str(), u.display_name.as_str()))
            .collect();
        assert_eq!(names, [("a", "Alice")]);
    }

    #[test]
    fn group_chats_are_named_after_their_people() {
        let names: std::collections::HashMap<String, String> =
            [("a", "Ann"), ("b", "Bob"), ("c", "Cas"), ("d", "Dee")]
                .map(|(id, name)| (id.to_owned(), name.to_owned()))
                .into();
        let ids = |list: &[&str]| list.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>();
        assert_eq!(
            group_name(&ids(&["a", "b"]), &names).as_deref(),
            Some("Ann, Bob")
        );
        assert_eq!(
            group_name(&ids(&["a", "b", "c", "d"]), &names).as_deref(),
            Some("Ann, Bob, Cas +1")
        );
        // Someone with no known name still counts.
        assert_eq!(
            group_name(&ids(&["a", "x"]), &names).as_deref(),
            Some("Ann +1")
        );
        assert_eq!(group_name(&ids(&["x", "y"]), &names), None);
    }

    #[test]
    fn system_lines_name_their_people() {
        assert_eq!(
            mentions("<@a-1> added <@live:.cid.2> and <@b|Bob>"),
            ["a-1", "live:.cid.2", "b"]
        );
        assert!(mentions("no one here").is_empty());
    }

    #[tokio::test]
    async fn each_person_is_asked_about_once() {
        let client = TeamsClient::new(TeamsCredentials::default());
        let first = client.not_yet_asked(["a".to_owned(), "b".to_owned()]);
        let again = client.not_yet_asked(["b".to_owned(), "c".to_owned()]);
        assert_eq!(first, ["a", "b"]);
        assert_eq!(again, ["c"]);
    }

    #[test]
    fn availability_is_there_or_away() {
        use crate::people::Presence;
        for there in ["Available", "Busy", "DoNotDisturb", "InACall"] {
            assert_eq!(presence_of(there), Presence::Active, "{there}");
        }
        for away in ["Away", "BeRightBack", "Offline", "PresenceUnknown", ""] {
            assert_eq!(presence_of(away), Presence::Away, "{away}");
        }
    }

    #[test]
    fn avatars_are_asked_by_mri() {
        assert_eq!(
            avatar_url("https://teams.live.com/api/mt/", "live:ana", None, None),
            "https://teams.live.com/api/mt/beta/users/8:live:ana/profilepicturev2?size=HR64x64"
        );
        assert!(crate::teams::client::is_media_url(&avatar_url(
            "https://teams.live.com/api/mt",
            "a-1",
            None,
            None
        )));
    }

    #[test]
    fn a_photo_is_asked_with_its_address_as_the_web_client_asks() {
        let url = avatar_url(
            "https://teams.live.com/api/mt",
            "live:ana",
            Some("Ana de Wit"),
            Some(
                "https://substrate.office.com/profile/v1.0/users/cid:4A5B/image/$value?hashKey='x'",
            ),
        );
        let url = reqwest::Url::parse(&url).expect("a URL");
        let query: Vec<(String, String)> = url.query_pairs().into_owned().collect();
        assert_eq!(url.path(), "/api/mt/beta/users/8:live:ana/profilepicturev2");
        assert_eq!(
            query,
            [
                ("displayname".to_owned(), "Ana de Wit".to_owned()),
                (
                    "imageUri".to_owned(),
                    "https://substrate.office.com/profile/v1.0/users/cid:4A5B/image/$value?hashKey='x'"
                        .to_owned()
                ),
                ("size".to_owned(), "HR64x64".to_owned()),
            ]
        );
    }

    #[test]
    fn workspace_ids_keep_to_plain_characters() {
        assert_eq!(
            workspace_id("094b41dd-eef6-4efd-8013-465e39c83d5a"),
            "teams_094b41dd-eef6-4efd-8013-465e39c83d5a"
        );
        assert_eq!(workspace_id("live:.cid.4a5b"), "teams_live__cid_4a5b");
    }

    #[test]
    fn a_personal_post_is_from_a_live_mri() {
        let mut post = post(None);
        post.me = "live:.cid.4a5b".into();
        let message = posted(
            Some("1".into()),
            &post,
            crate::teams::html::wire_to_teams("hi"),
        )
        .expect("a message");
        assert_eq!(message.user.as_deref(), Some("live:.cid.4a5b"));
    }

    #[test]
    fn marks_and_deletes_use_the_teams_message_id() {
        let ts = teams_id_to_ts("1700000000123");
        assert_eq!(ts_to_teams_id(&ts), "1700000000123");
    }
}
