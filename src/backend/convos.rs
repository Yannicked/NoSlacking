//! The worker's side of [`crate::convos`]: the Web API calls that start
//! and find conversations, and the JSON they answer with.

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

/// Runs one command and reports back. Every failure is answered, so a
/// dialog waiting on it never waits for ever.
pub async fn run(client: Client, team: String, command: Command, sink: Sink) {
    let what = command.failure();
    let result = match command {
        Command::Open { users } => open(&client, &team, &users, &sink).await,
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
            error: describe(error),
        },
    });
}

/// Opens the DM or group DM with `users`, sends its details, then asks
/// the interface to show it.
async fn open(
    client: &Client,
    team: &str,
    users: &[String],
    sink: &Sink,
) -> Result<(), SlackError> {
    let opened: Opened = client
        .act(
            "conversations.open",
            &[("users", users.join(",")), ("return_im", "true".into())],
        )
        .await?;
    let id = opened.channel.id.clone();
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
            fallback(opened.channel, users)
        }
    };
    sink.send(Event::Conversation {
        team: team.to_owned(),
        conversation,
    });
    sink.send(Event::Convos {
        team: team.to_owned(),
        event: convos::Event::Opened { channel: id },
    });
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
            vec![
                Event::Conversation {
                    team: team.to_owned(),
                    conversation,
                },
                Event::Convos {
                    team: team.to_owned(),
                    event: convos::Event::Opened { channel: id },
                },
            ]
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::ConversationKind;

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
