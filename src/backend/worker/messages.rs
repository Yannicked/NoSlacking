//! Messages: loading history and threads, sending, editing, deleting
//! and reacting, slash commands and app buttons, and the read marker.

use serde_json::Value;

use super::{Backend, Otherwise, Worker, history_failed};
use crate::backend::api::{Call, act_with_blocks, failure, with_text};
use crate::backend::around;
use crate::backend::fetch::{history, thread};
use crate::backend::translate::str_of;
use crate::backend::{Change, Event, Sink};
use crate::failure::{Doing, Failure};
use crate::model::Ts;
use crate::slack::{Client, types};

impl Worker {
    /// The newest page of history, or the one before `cursor`.
    pub(super) fn load_history(&self, team: String, channel: String, cursor: Option<String>) {
        match self.workspaces.get(&team) {
            Some(Backend::Slack(slack)) => {
                tokio::spawn(history(
                    slack.client.clone(),
                    team,
                    channel,
                    cursor,
                    self.cache.clone(),
                    false,
                    slack.sink.clone(),
                ));
            }
            #[cfg(feature = "teams")]
            Some(Backend::Teams(session)) => {
                tokio::spawn(crate::backend::teams::history(
                    session.client.clone(),
                    team,
                    channel,
                    cursor,
                    session.sink.clone(),
                ));
            }
            None => self.history_unavailable(team, channel),
        }
    }

    pub(super) fn load_thread(&self, team: String, channel: String, ts: Ts) {
        #[cfg(feature = "teams")]
        if let Some(session) = self.teams_session(&team) {
            tokio::spawn(crate::backend::teams::thread(
                session.client.clone(),
                team,
                channel,
                ts,
                session.sink.clone(),
            ));
            return;
        }
        self.spawn_slack(
            team,
            Otherwise::Refuse(Doing::LoadThread),
            |client, team, sink| thread(client, team, channel, ts, sink),
        );
    }

    /// The messages around `ts`, for jumping to it.
    pub(super) fn load_around(&self, team: String, channel: String, ts: Ts) {
        self.spawn_slack(
            team,
            history_failed(channel.clone()),
            |client, team, sink| around::around(client, team, channel, ts, sink),
        );
    }

    /// The page of messages right after `after`.
    pub(super) fn load_newer(&self, team: String, channel: String, after: Ts) {
        self.spawn_slack(
            team,
            history_failed(channel.clone()),
            |client, team, sink| around::newer(client, team, channel, after, sink),
        );
    }

    /// Message `ts` by itself, to quote; always answered.
    pub(super) fn fetch_quote(&self, team: String, channel: String, ts: Ts, thread: Option<Ts>) {
        let (to, at) = (channel.clone(), ts.clone());
        self.spawn_slack(
            team,
            Otherwise::Answer(Box::new(move |team, error| Event::Quoted {
                team,
                channel: to,
                ts: at,
                result: Err(error),
            })),
            |client, team, sink| around::quote(client, team, channel, ts, thread, sink),
        );
    }

    /// Page `page` of a search; always answered as `request`.
    pub(super) fn search(&self, query: crate::search::Query, page: u32, request: u64) {
        self.spawn_slack(
            query.team.clone(),
            Otherwise::Answer(Box::new(move |team, error| Event::Search {
                team,
                request,
                result: Err(error),
            })),
            move |client, _, sink| {
                crate::backend::search::search(client, query, page, request, sink)
            },
        );
    }

    /// Posts a message; the answer settles the interface's optimistic copy.
    pub(super) fn send(&self, outgoing: Outgoing) {
        match self.workspaces.get(&outgoing.team) {
            Some(Backend::Slack(slack)) => {
                tokio::spawn(post(slack.client.clone(), outgoing, slack.sink.clone()));
            }
            #[cfg(feature = "teams")]
            Some(Backend::Teams(session)) => {
                let post = crate::backend::teams::Post {
                    team: outgoing.team,
                    channel: outgoing.channel,
                    text: outgoing.text,
                    local: outgoing.local,
                    client_msg_id: outgoing.client_msg_id,
                    me: session.workspace.user_id.clone(),
                    me_name: session.client.own_name(),
                    thread: outgoing.thread,
                };
                tokio::spawn(crate::backend::teams::send(
                    session.client.clone(),
                    post,
                    session.sink.clone(),
                ));
            }
            // Fail the optimistic message, or it stays pending.
            None => self.sink.send(Event::Sent {
                team: outgoing.team,
                channel: outgoing.channel,
                local: outgoing.local,
                result: Err(Failure::NotSignedIn),
            }),
        }
    }

    /// Saves an edit, delete or reaction the interface already shows,
    /// and always answers with [`Event::Settled`] so a refused change can
    /// be undone, even for a workspace that is not signed in.
    pub(super) fn change(&self, team: String, channel: String, change: Change) {
        match self.workspaces.get(&team) {
            Some(Backend::Slack(slack)) => {
                let (client, sink) = (slack.client.clone(), slack.sink.clone());
                tokio::spawn(async move {
                    // Already as asked is no failure: nothing to undo.
                    let result = request(&channel, &change)
                        .run(&client)
                        .await
                        .map_err(|e| failure(&e));
                    sink.send(Event::Settled {
                        team,
                        channel,
                        change,
                        result,
                    });
                });
            }
            #[cfg(feature = "teams")]
            Some(Backend::Teams(session)) => {
                let me = crate::teams::client::Author {
                    id: session.workspace.user_id.clone(),
                    name: session.client.own_name(),
                };
                tokio::spawn(crate::backend::teams::change(
                    session.client.clone(),
                    team,
                    channel,
                    change,
                    me,
                    session.sink.clone(),
                ));
            }
            None => self.sink.send(Event::Settled {
                team,
                channel,
                change,
                result: Err(Failure::NotSignedIn),
            }),
        }
    }

    /// Runs a slash command: through its own Web API method where it has
    /// one, so it works with any sign-in, and otherwise through
    /// `chat.command`, Slack's own runner, which only sessions may call.
    pub(super) fn slash(
        &self,
        id: u64,
        team: String,
        channel: String,
        command: String,
        text: String,
    ) {
        let named = command.clone();
        self.answer_slack(
            team,
            |client| async move { run_slash(&client, &channel, &named, &text).await },
            move |_, result| Event::Slash {
                id,
                command,
                result,
            },
        );
    }

    /// Presses an app's button through `blocks.actions` (see
    /// [`crate::backend::blocks`]), which only a browser session may call.
    pub(super) fn press_button(&self, team: String, press: crate::model::Press) {
        // The press is dated like Slack's own; a clock before 1970 only
        // makes the date wrong, which Slack does not check.
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |since| {
                u64::try_from(since.as_millis()).unwrap_or(u64::MAX)
            });
        let pressed = press.clone();
        self.answer_slack(
            team,
            |client| async move {
                let result = crate::backend::blocks::press(&client, &pressed, now_ms).await;
                if let Err(error) = &result {
                    log::warn!("blocks.actions: {error:?}");
                }
                result
            },
            |team, result| Event::Pressed {
                team,
                press,
                result,
            },
        );
    }

    /// Moves your read marker. The interface sends this on its own as you
    /// read; a workspace that is signed out has nothing to mark, and
    /// saying so on every click would only be noise.
    pub(super) fn mark(&self, team: String, channel: String, ts: Ts) {
        match self.workspaces.get(&team) {
            Some(Backend::Slack(_)) => self.act(
                team,
                Doing::MarkRead,
                Call::new(
                    "conversations.mark",
                    vec![("channel", channel), ("ts", ts.0)],
                    &["not_in_channel", "channel_not_found"],
                ),
            ),
            #[cfg(feature = "teams")]
            Some(Backend::Teams(session)) => {
                tokio::spawn(crate::backend::teams::mark(
                    session.client.clone(),
                    team,
                    channel,
                    ts,
                ));
            }
            None => log::debug!("not marking read in {team}: signed out"),
        }
    }
}

/// A message to post, as `Command::Send` carries it.
pub(super) struct Outgoing {
    pub(super) team: String,
    pub(super) channel: String,
    pub(super) text: String,
    pub(super) thread: Option<Ts>,
    pub(super) broadcast: bool,
    /// The interface's id for its optimistic copy.
    pub(super) local: Ts,
    pub(super) client_msg_id: Option<String>,
}

/// Posts `outgoing` to Slack and settles the optimistic copy with the
/// answer.
async fn post(client: Client, outgoing: Outgoing, sink: Sink) {
    let Outgoing {
        team,
        channel,
        text,
        thread,
        broadcast,
        local,
        client_msg_id,
    } = outgoing;
    let params = post_params(&channel, text, thread.as_ref(), broadcast, client_msg_id);
    let result = act_with_blocks::<types::Posted>(&client, "chat.postMessage", &params)
        .await
        .map_err(|e| failure(&e))
        .and_then(|posted| {
            let mut message = posted
                .message
                .and_then(types::Message::into_model)
                .ok_or(Failure::NoMessage)?;
            if message.ts.as_str().is_empty() {
                message.ts = Ts::new(posted.ts);
            }
            Ok(message)
        });
    sink.send(Event::Sent {
        team,
        channel,
        local,
        result,
    });
}

/// What `chat.postMessage` is given for a message.
///
/// Without `unfurl_links`, whether Slack unfurls a text-based link in a
/// post through the API depends on the token (an app's posts are not
/// unfurled unless asked). A shared message is only a link to the message
/// it quotes, so a text with a link to a Slack message asks outright.
fn post_params(
    channel: &str,
    text: String,
    thread: Option<&Ts>,
    broadcast: bool,
    client_msg_id: Option<String>,
) -> Vec<(&'static str, String)> {
    let unfurl = crate::links::has_message_link(&text);
    let mut params = vec![("channel", channel.to_owned())];
    with_text(&mut params, text);
    if let Some(id) = client_msg_id {
        params.push(("client_msg_id", id));
    }
    if unfurl {
        params.push(("unfurl_links", "true".into()));
    }
    if let Some(thread) = thread {
        params.push(("thread_ts", thread.0.clone()));
        if broadcast {
            params.push(("reply_broadcast", "true".into()));
        }
    }
    params
}

/// The Web API call that makes a [`Change`], with the error codes that
/// mean it is already made.
fn request(channel: &str, change: &Change) -> Call {
    let channel = ("channel", channel.to_owned());
    match change {
        Change::Edit { ts, text, .. } => {
            let mut params = vec![channel, ("ts", ts.0.clone())];
            with_text(&mut params, text.clone());
            Call::new("chat.update", params, &[])
        }
        Change::Delete { ts, .. } => Call::new(
            "chat.delete",
            vec![channel, ("ts", ts.0.clone())],
            &["message_not_found"],
        ),
        Change::React { ts, name, added } => Call::new(
            if *added {
                "reactions.add"
            } else {
                "reactions.remove"
            },
            vec![channel, ("timestamp", ts.0.clone()), ("name", name.clone())],
            &["already_reacted", "no_reaction"],
        ),
    }
}

/// What [`Worker::slash`] runs: the command's own method, or
/// `chat.command`. `Ok` carries Slack's reply text, when it has one.
async fn run_slash(
    client: &Client,
    channel: &str,
    command: &str,
    text: &str,
) -> Result<Option<String>, Failure> {
    let act = |method: &'static str, params: Vec<(&'static str, String)>| async move {
        client
            .act::<Value>(method, &params)
            .await
            .map(|_| None)
            .map_err(|e| failure(&e))
    };
    let channel = channel.to_owned();
    match command {
        "me" => {
            act(
                "chat.meMessage",
                vec![("channel", channel), ("text", text.to_owned())],
            )
            .await
        }
        "away" | "active" => crate::backend::people::set_away(client, command == "away")
            .await
            .map(|()| None)
            .map_err(|e| failure(&e)),
        "status" => {
            let (emoji, status) = crate::slash::status(&crate::mrkdwn::unescape(text));
            crate::backend::people::set_status(client, &emoji, &status, 0)
                .await
                .map(|()| None)
                .map_err(|e| failure(&e))
        }
        "topic" => {
            act(
                "conversations.setTopic",
                vec![("channel", channel), ("topic", text.to_owned())],
            )
            .await
        }
        "invite" => {
            let people = crate::slash::mentioned(text);
            if people.is_empty() {
                return Err(Failure::NoInvitee);
            }
            act(
                "conversations.invite",
                vec![("channel", channel), ("users", people.join(","))],
            )
            .await
        }
        "leave" => act("conversations.leave", vec![("channel", channel)]).await,
        _ if client.is_session() => {
            let params = [
                ("channel", channel),
                ("command", format!("/{command}")),
                ("text", text.to_owned()),
            ];
            client
                .act::<Value>("chat.command", &params)
                .await
                .map(|answer| {
                    str_of(&answer, "response")
                        .filter(|r| !r.is_empty())
                        .map(str::to_owned)
                })
                .map_err(|e| failure(&e))
        }
        _ => Err(Failure::NeedsSession),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_shared_message_asks_slack_to_unfurl_its_link() {
        let shared = "Look\n<https://acme.slack.com/archives/C1/p1700000000000100>";
        let blocks = crate::slack::rich_out::blocks_param(shared).expect("blocks");
        assert_eq!(
            post_params("C2", shared.into(), None, false, None),
            [
                ("channel", "C2".to_owned()),
                ("text", shared.to_owned()),
                ("blocks", blocks),
                ("unfurl_links", "true".to_owned()),
            ]
        );
        let plain = post_params(
            "C2",
            "hi".into(),
            Some(&Ts::new("1.000100")),
            true,
            Some("4f1e6b2a-0c3d-4e5f-8a9b-1c2d3e4f5a6b".into()),
        );
        assert_eq!(
            plain,
            [
                ("channel", "C2".to_owned()),
                ("text", "hi".to_owned()),
                (
                    "blocks",
                    r#"[{"elements":[{"elements":[{"text":"hi","type":"text"}],"type":"rich_text_section"}],"type":"rich_text"}]"#
                        .to_owned()
                ),
                (
                    "client_msg_id",
                    "4f1e6b2a-0c3d-4e5f-8a9b-1c2d3e4f5a6b".to_owned()
                ),
                ("thread_ts", "1.000100".to_owned()),
                ("reply_broadcast", "true".to_owned()),
            ],
            "other messages are sent as before"
        );
    }

    /// The `blocks` parameter of `params`, read as JSON.
    fn blocks_of(params: &[(&'static str, String)]) -> Option<Value> {
        params
            .iter()
            .find(|(name, _)| *name == "blocks")
            .map(|(_, json)| serde_json::from_str(json).expect("blocks are JSON"))
    }

    #[test]
    fn messages_go_with_their_rich_text_beside_the_text() {
        let wire = "*hi* <@U1>\n• one";
        let block = crate::slack::rich_out::rich_text(wire).expect("a block");
        let post = post_params("C1", wire.into(), None, false, None);
        assert!(post.contains(&("text", wire.to_owned())));
        assert_eq!(blocks_of(&post), Some(serde_json::json!([block])));
        let edit = Change::Edit {
            ts: Ts::new("1.0"),
            text: wire.into(),
            before: None,
        };
        let Call { method, params, .. } = request("C1", &edit);
        assert_eq!(method, "chat.update");
        assert_eq!(
            params[..3],
            [
                ("channel", "C1".to_owned()),
                ("ts", "1.0".to_owned()),
                ("text", wire.to_owned())
            ]
        );
        assert_eq!(blocks_of(&params), Some(serde_json::json!([block])));
    }

    #[test]
    fn text_that_makes_no_block_goes_alone() {
        // A date has no element to keep it; blank text has nothing to lay
        // out.
        for wire in ["due <!date^1700000000^{date}|Nov 14>", "  "] {
            let post = post_params("C1", wire.into(), None, false, None);
            assert_eq!(
                post,
                [("channel", "C1".to_owned()), ("text", wire.to_owned())]
            );
        }
    }
}
