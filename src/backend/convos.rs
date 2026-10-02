//! The worker's side of [`crate::convos`]: the Web API calls that start,
//! find, join, leave and create conversations, and the JSON they answer
//! with.

use serde::Deserialize;

use super::worker::describe;
use super::{Event, Sink};
use crate::convos::{self, Command};
use crate::model::Conversation;
use crate::slack::{Client, SlackError, types};

/// `conversations.open`'s answer: only the id is sure to be there.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct Opened {
    channel: types::Channel,
}

/// The most pages of `conversations.list` read for the browser, 200
/// channels a page: far more than a workspace normally has, it only stops
/// a cursor that never ends.
const BROWSE_PAGES: usize = 50;

/// A page of `conversations.list`.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct ListPage {
    channels: Vec<types::Channel>,
    response_metadata: types::ResponseMetadata,
}

/// Runs one command and reports back. Every failure is answered, so a
/// dialog waiting on it never waits for ever.
pub async fn run(client: Client, team: String, command: Command, sink: Sink) {
    let what = command.failure();
    let result = match command {
        Command::Open { users } => open(&client, &team, &users, &sink).await,
        Command::Browse => browse(&client, &team, &sink).await,
        Command::Join { channel } => join(&client, &team, &channel, &sink).await,
        Command::Leave { channel } => leave(&client, &team, &channel, &sink).await,
        Command::Create { name, private } => create(&client, &team, &name, private, &sink).await,
    };
    if let Err(error) = result {
        fail(&sink, team, what, &error);
    }
}

/// Tells the interface that `what` failed.
fn fail(sink: &Sink, team: String, what: convos::Failure, error: &SlackError) {
    sink.send(Event::Convos {
        team,
        event: convos::Event::Failed {
            what,
            error: explain(error),
        },
    });
}

/// [`describe`], with the refusals these calls can meet in plain words.
fn explain(error: &SlackError) -> String {
    match error.code() {
        Some("name_taken") => "a channel by that name exists already".into(),
        Some("invalid_name" | "invalid_name_specials" | "invalid_name_punctuation") => {
            "Slack does not take that name".into()
        }
        Some("invalid_name_maxlength") => "the name is too long".into(),
        Some("cant_leave_general") => "nobody can leave the general channel".into(),
        Some("restricted_action" | "restricted_action_read_only_channel") => {
            "the workspace does not allow you to do that".into()
        }
        Some("method_not_supported_for_channel_type") => {
            "that cannot be done in this kind of conversation".into()
        }
        _ => describe(error),
    }
}

/// Sends a channel you are now in, then asks the interface to open it.
fn opened(sink: &Sink, team: &str, conversation: Conversation) {
    let channel = conversation.id.clone();
    sink.send(Event::Conversation {
        team: team.to_owned(),
        conversation,
    });
    sink.send(Event::Convos {
        team: team.to_owned(),
        event: convos::Event::Opened { channel },
    });
}

/// Lists the public channels you are not in, page by page, so the browser
/// fills as they come.
async fn browse(client: &Client, team: &str, sink: &Sink) -> Result<(), SlackError> {
    let mut cursor: Option<String> = None;
    let mut seen = std::collections::HashSet::new();
    for page in 1..=BROWSE_PAGES {
        let mut params = vec![
            ("types", "public_channel".to_owned()),
            ("exclude_archived", "true".to_owned()),
            ("limit", "200".to_owned()),
        ];
        if let Some(cursor) = cursor.take() {
            params.push(("cursor", cursor));
        }
        let answer: ListPage = client.call("conversations.list", &params).await?;
        // Slack repeating a cursor would page for ever.
        let next = answer
            .response_metadata
            .cursor()
            .filter(|next| seen.insert(next.clone()));
        let done = next.is_none() || page == BROWSE_PAGES;
        sink.send(Event::Convos {
            team: team.to_owned(),
            event: convos::Event::Browsed {
                channels: joinable(answer.channels),
                done,
            },
        });
        if done {
            break;
        }
        cursor = next;
    }
    Ok(())
}

/// The channels of a listing you are not in, as the browser shows them.
fn joinable(channels: Vec<types::Channel>) -> Vec<convos::Listed> {
    channels
        .into_iter()
        .filter(|c| c.is_member != Some(true) && !c.is_archived)
        .map(|c| convos::Listed {
            id: c.id,
            name: c.name,
            topic: c.topic.value,
            purpose: c.purpose.value,
            members: c.num_members.unwrap_or(0),
        })
        .collect()
}

/// Joins a public channel and opens it.
async fn join(client: &Client, team: &str, channel: &str, sink: &Sink) -> Result<(), SlackError> {
    let joined: types::ChannelInfo = client
        .act("conversations.join", &[("channel", channel.to_owned())])
        .await?;
    let mut conversation = joined.channel.into_model();
    if conversation.id.is_empty() {
        conversation.id = channel.to_owned();
    }
    opened(sink, team, conversation);
    Ok(())
}

/// Leaves a channel; the sidebar drops it once Slack agrees.
async fn leave(client: &Client, team: &str, channel: &str, sink: &Sink) -> Result<(), SlackError> {
    match client
        .act::<serde_json::Value>("conversations.leave", &[("channel", channel.to_owned())])
        .await
    {
        Ok(_) => {}
        // Not in it any more: gone all the same.
        Err(SlackError::Api(code)) if code == "not_in_channel" => {}
        Err(error) => return Err(error),
    }
    sink.send(Event::ConversationGone {
        team: team.to_owned(),
        channel: channel.to_owned(),
    });
    Ok(())
}

/// Creates a channel and opens it.
async fn create(
    client: &Client,
    team: &str,
    name: &str,
    private: bool,
    sink: &Sink,
) -> Result<(), SlackError> {
    let created: types::ChannelInfo = client
        .act(
            "conversations.create",
            &[
                ("name", name.to_owned()),
                ("is_private", private.to_string()),
            ],
        )
        .await?;
    opened(sink, team, created.channel.into_model());
    Ok(())
}

/// Opens the DM or group DM with `users`, sends its details, then asks
/// the interface to show it.
async fn open(
    client: &Client,
    team: &str,
    users: &[String],
    sink: &Sink,
) -> Result<(), SlackError> {
    let answer: Opened = client
        .act(
            "conversations.open",
            &[("users", users.join(",")), ("return_im", "true".into())],
        )
        .await?;
    let id = answer.channel.id.clone();
    if id.is_empty() {
        return Err(SlackError::Api("channel_not_found".into()));
    }
    // The open answer can be as thin as an id; the details make the
    // sidebar row.
    let conversation = match client
        .call::<types::ChannelInfo>("conversations.info", &[("channel", id.clone())])
        .await
    {
        Ok(info) => info.channel.into_model(),
        Err(error) => {
            log::debug!("conversations.info {id}: {error}");
            fallback(answer.channel, users)
        }
    };
    opened(sink, team, conversation);
    Ok(())
}

/// A conversation from `conversations.open`'s own answer, when its details
/// cannot be read: it is a DM with one person, or a group DM.
fn fallback(mut channel: types::Channel, users: &[String]) -> Conversation {
    match users {
        [user] => {
            channel.is_im = true;
            channel.user = Some(user.clone());
            if channel.name.is_empty() {
                channel.name.clone_from(user);
            }
        }
        _ => channel.is_mpim = true,
    }
    channel.into_model()
}

/// What the pretend Slack answers (`--demo`): what the real one would, with
/// made-up data and no network.
#[cfg(feature = "demo")]
pub fn demo(team: &str, command: Command) -> Vec<Event> {
    match command {
        Command::Open { users } => {
            let id = format!("D-{}", users.join("-"));
            let mut conversation = fallback(
                types::Channel {
                    id: id.clone(),
                    ..types::Channel::default()
                },
                &users,
            );
            if users.len() > 1 {
                conversation.name = users.join(", ");
            }
            demo_opened(team, conversation)
        }
        Command::Browse => vec![Event::Convos {
            team: team.to_owned(),
            event: convos::Event::Browsed {
                channels: demo_listed(),
                done: true,
            },
        }],
        Command::Join { channel } => {
            let listed = demo_listed()
                .into_iter()
                .find(|c| c.id == channel)
                .unwrap_or_default();
            let conversation = types::Channel {
                id: channel,
                name: listed.name,
                is_channel: true,
                topic: types::TextValue {
                    value: listed.topic,
                },
                num_members: Some(listed.members + 1),
                ..types::Channel::default()
            };
            demo_opened(team, conversation.into_model())
        }
        Command::Leave { channel } => vec![Event::ConversationGone {
            team: team.to_owned(),
            channel,
        }],
        Command::Create { name, private } => {
            let conversation = types::Channel {
                id: format!("C-{name}"),
                name,
                is_channel: !private,
                is_private: private,
                num_members: Some(1),
                ..types::Channel::default()
            };
            demo_opened(team, conversation.into_model())
        }
    }
}

/// The public channels the pretend Slack has that you are not in.
#[cfg(feature = "demo")]
fn demo_listed() -> Vec<convos::Listed> {
    let channel = |id: &str, name: &str, topic: &str, members: u32| convos::Listed {
        id: id.into(),
        name: name.into(),
        topic: topic.into(),
        purpose: String::new(),
        members,
    };
    vec![
        channel("C10", "announcements", "Company news, read-only", 214),
        channel("C11", "frontend", "Web and desktop interfaces", 38),
        channel("C12", "help-it", "Laptops, accounts and VPN", 120),
        channel("C13", "lunch", "Who is going where :fork_and_knife:", 57),
        channel("C14", "release-notes", "", 12),
        channel("C15", "watercooler", "Anything goes", 96),
    ]
}

/// The pretend Slack's [`opened`].
#[cfg(feature = "demo")]
fn demo_opened(team: &str, conversation: Conversation) -> Vec<Event> {
    let channel = conversation.id.clone();
    vec![
        Event::Conversation {
            team: team.to_owned(),
            conversation,
        },
        Event::Convos {
            team: team.to_owned(),
            event: convos::Event::Opened { channel },
        },
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::ConversationKind;

    #[test]
    fn the_browser_lists_only_channels_you_can_join() {
        let page: ListPage = serde_json::from_str(
            r#"{"ok":true,"channels":[
                {"id":"C1","name":"general","is_channel":true,"is_member":true,"num_members":40},
                {"id":"C2","name":"lunch","is_channel":true,"is_member":false,"num_members":7,
                 "topic":{"value":"Food"},"purpose":{"value":"Where to eat"}},
                {"id":"C3","name":"old","is_channel":true,"is_archived":true}
            ],"response_metadata":{"next_cursor":"abc"}}"#,
        )
        .expect("json");
        assert_eq!(page.response_metadata.cursor().as_deref(), Some("abc"));
        let listed = joinable(page.channels);
        assert_eq!(
            listed,
            [convos::Listed {
                id: "C2".into(),
                name: "lunch".into(),
                topic: "Food".into(),
                purpose: "Where to eat".into(),
                members: 7,
            }]
        );
    }

    #[test]
    fn refusals_read_as_plain_sentences() {
        assert_eq!(
            explain(&SlackError::Api("name_taken".into())),
            "a channel by that name exists already"
        );
        assert_eq!(
            explain(&SlackError::Api("not_in_channel".into())),
            "you are not in that channel"
        );
    }

    #[test]
    fn a_thin_open_answer_still_makes_a_dm() {
        let opened: Opened =
            serde_json::from_str(r#"{"ok":true,"channel":{"id":"D1"}}"#).expect("json");
        let dm = fallback(opened.channel, &["U1".into()]);
        assert_eq!(dm.kind, ConversationKind::Direct);
        assert_eq!(dm.user.as_deref(), Some("U1"));
        let opened: Opened =
            serde_json::from_str(r#"{"ok":true,"channel":{"id":"G1","name":"mpdm-a--b-1"}}"#)
                .expect("json");
        let group = fallback(opened.channel, &["U1".into(), "U2".into()]);
        assert_eq!(group.kind, ConversationKind::Group);
        assert_eq!(group.name, "a, b");
    }
}
