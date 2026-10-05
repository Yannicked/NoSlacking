//! The worker's side of [`crate::convos`]: the Web API calls that start,
//! find, join, leave and create conversations, and the JSON they answer
//! with.

use serde::Deserialize;

use super::api::describe;
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

/// The most pages of `conversations.members` read, 200 people a page.
const MEMBER_PAGES: usize = 50;
/// How many of a conversation's newest files its Files tab lists.
const FILES: usize = 100;

/// The parts of `conversations.info` the details panel adds to the
/// sidebar's copy.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct Made {
    channel: MadeChannel,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct MadeChannel {
    created: Option<i64>,
    creator: Option<String>,
}

/// A page of `conversations.members`.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct MembersPage {
    members: Vec<String>,
    response_metadata: types::ResponseMetadata,
}

/// `files.list`'s answer.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct FilesPage {
    files: Vec<ListedFile>,
}

/// A file in `files.list`: a message's file, and who shared it when.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct ListedFile {
    #[serde(flatten)]
    file: types::File,
    user: Option<String>,
    created: Option<i64>,
}

/// `pins.list`'s answer.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct PinsPage {
    items: Vec<PinItem>,
}

/// One pinned item; only messages are shown.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct PinItem {
    #[serde(rename = "type")]
    kind: String,
    created_by: Option<String>,
    message: Option<types::Message>,
}

/// `bookmarks.list`'s answer.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct BookmarksPage {
    bookmarks: Vec<BookmarkItem>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct BookmarkItem {
    id: String,
    title: String,
    link: String,
    emoji: Option<String>,
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
        Command::About { channel } => {
            let result = about(&client, &channel).await.map_err(|e| explain(&e));
            reply(&sink, &team, convos::Event::About { channel, result });
            Ok(())
        }
        Command::Members { channel } => {
            let result = members(&client, &channel).await.map_err(|e| explain(&e));
            reply(&sink, &team, convos::Event::Members { channel, result });
            Ok(())
        }
        Command::Files { channel } => {
            let result = files(&client, &channel).await.map_err(|e| explain(&e));
            reply(&sink, &team, convos::Event::Files { channel, result });
            Ok(())
        }
        Command::Describe {
            channel,
            field,
            text,
        } => describe_channel(&client, &team, &channel, field, text, &sink).await,
        Command::Pins { channel } => {
            let result = client
                .call::<PinsPage>("pins.list", &[("channel", channel.clone())])
                .await
                .map(pins)
                .map_err(|e| explain(&e));
            reply(&sink, &team, convos::Event::Pins { channel, result });
            Ok(())
        }
        Command::Bookmarks { channel } => {
            let result = client
                .call::<BookmarksPage>("bookmarks.list", &[("channel_id", channel.clone())])
                .await
                .map(bookmarks)
                .map_err(|e| explain(&e));
            reply(&sink, &team, convos::Event::Bookmarks { channel, result });
            Ok(())
        }
        Command::Pin { channel, ts, pin } => {
            let method = if pin { "pins.add" } else { "pins.remove" };
            match client
                .act::<serde_json::Value>(method, &[("channel", channel), ("timestamp", ts.0)])
                .await
            {
                // Already as asked: nothing to undo.
                Err(SlackError::Api(code)) if code == "already_pinned" || code == "no_pin" => {
                    Ok(())
                }
                other => other.map(|_| ()),
            }
        }
    };
    if let Err(error) = result {
        fail(&sink, team, what, &error);
    }
}

/// The pinned messages of a `pins.list` answer, newest pin first as Slack
/// lists them.
fn pins(page: PinsPage) -> Vec<convos::Pin> {
    page.items
        .into_iter()
        .filter(|item| item.kind == "message")
        .filter_map(|item| {
            let mut message = item.message?.into_model()?;
            message.pinned = true;
            Some(convos::Pin {
                message,
                by: item.created_by.filter(|u| !u.is_empty()),
            })
        })
        .collect()
}

/// The links of a `bookmarks.list` answer, with their emoji as shortcodes.
fn bookmarks(page: BookmarksPage) -> Vec<convos::Bookmark> {
    page.bookmarks
        .into_iter()
        .filter(|b| !b.link.is_empty())
        .map(|b| convos::Bookmark {
            title: if b.title.is_empty() {
                b.link.clone()
            } else {
                b.title
            },
            id: b.id,
            link: b.link,
            emoji: b
                .emoji
                .map(|e| e.trim_matches(':').to_owned())
                .filter(|e| !e.is_empty()),
        })
        .collect()
}

/// A `pin_added` or `pin_removed` event, as the interface takes it.
pub fn pin_event(kind: &str, event: &serde_json::Value) -> Option<convos::Event> {
    let item = event.get("item")?;
    if item.get("type").and_then(serde_json::Value::as_str) != Some("message") {
        return None;
    }
    let str_of = |value: &serde_json::Value, key: &str| {
        value
            .get(key)
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned)
    };
    let channel = str_of(event, "channel_id").or_else(|| str_of(item, "channel"))?;
    let ts = item.get("message").and_then(|m| str_of(m, "ts"))?;
    Some(convos::Event::Pinned {
        channel,
        ts: crate::model::Ts::new(ts),
        pinned: kind == "pin_added",
        by: str_of(event, "user"),
    })
}

/// Sends one of [`convos::Event`]s.
fn reply(sink: &Sink, team: &str, event: convos::Event) {
    sink.send(Event::Convos {
        team: team.to_owned(),
        event,
    });
}

/// When and by whom a conversation was made.
async fn about(client: &Client, channel: &str) -> Result<convos::About, SlackError> {
    let made: Made = client
        .call("conversations.info", &[("channel", channel.to_owned())])
        .await?;
    Ok(convos::About {
        created: made.channel.created.filter(|c| *c > 0),
        creator: made.channel.creator.filter(|c| !c.is_empty()),
    })
}

/// Everyone in a conversation.
async fn members(client: &Client, channel: &str) -> Result<Vec<String>, SlackError> {
    let mut all = Vec::new();
    let mut cursor: Option<String> = None;
    let mut seen = std::collections::HashSet::new();
    for _ in 0..MEMBER_PAGES {
        let mut params = vec![("channel", channel.to_owned()), ("limit", "200".to_owned())];
        if let Some(cursor) = cursor.take() {
            params.push(("cursor", cursor));
        }
        let page: MembersPage = client.call("conversations.members", &params).await?;
        all.extend(page.members);
        match page
            .response_metadata
            .cursor()
            .filter(|next| seen.insert(next.clone()))
        {
            Some(next) => cursor = Some(next),
            None => break,
        }
    }
    Ok(all)
}

/// The newest files shared in a conversation.
async fn files(client: &Client, channel: &str) -> Result<Vec<convos::SharedFile>, SlackError> {
    let page: FilesPage = client
        .call(
            "files.list",
            &[
                ("channel", channel.to_owned()),
                ("count", FILES.to_string()),
            ],
        )
        .await?;
    Ok(shared(page))
}

/// The files of a `files.list` page that can still be shown.
fn shared(page: FilesPage) -> Vec<convos::SharedFile> {
    page.files
        .into_iter()
        .filter_map(|listed| {
            Some(convos::SharedFile {
                file: listed.file.into_model()?,
                user: listed.user.filter(|u| !u.is_empty()),
                created: listed.created,
            })
        })
        .collect()
}

/// Sets a topic or purpose, then fetches the conversation so every view
/// shows what Slack kept (it trims and may shorten).
async fn describe_channel(
    client: &Client,
    team: &str,
    channel: &str,
    field: convos::Field,
    text: String,
    sink: &Sink,
) -> Result<(), SlackError> {
    let (method, key) = match field {
        convos::Field::Topic => ("conversations.setTopic", "topic"),
        convos::Field::Purpose => ("conversations.setPurpose", "purpose"),
    };
    client
        .act::<serde_json::Value>(method, &[("channel", channel.to_owned()), (key, text)])
        .await?;
    if let Ok(info) = client
        .call::<types::ChannelInfo>("conversations.info", &[("channel", channel.to_owned())])
        .await
    {
        sink.send(Event::Conversation {
            team: team.to_owned(),
            conversation: info.channel.into_model(),
        });
    }
    Ok(())
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
        Command::About { channel } => vec![Event::Convos {
            team: team.to_owned(),
            event: convos::Event::About {
                channel,
                result: Ok(convos::About {
                    created: Some(1_700_000_000),
                    creator: Some("U01".into()),
                }),
            },
        }],
        Command::Members { channel } => vec![Event::Convos {
            team: team.to_owned(),
            event: convos::Event::Members {
                channel,
                result: Ok(["U00", "U01", "U02", "U03", "U04", "U05", "U06"]
                    .map(str::to_owned)
                    .to_vec()),
            },
        }],
        Command::Files { channel } => {
            let file = |id: &str, name: &str, mimetype: &str, size: u64| crate::model::File {
                id: id.into(),
                name: name.into(),
                title: name.into(),
                mimetype: mimetype.into(),
                size,
                download_url: Some(format!("https://files.example/{name}")),
                ..crate::model::File::default()
            };
            vec![Event::Convos {
                team: team.to_owned(),
                event: convos::Event::Files {
                    channel,
                    result: Ok(vec![
                        convos::SharedFile {
                            file: file("F1", "roadmap-q4.pdf", "application/pdf", 482_113),
                            user: Some("U03".into()),
                            created: Some(1_790_100_000),
                        },
                        convos::SharedFile {
                            file: file("F2", "sidebar-spacing.png", "image/png", 91_034),
                            user: Some("U01".into()),
                            created: Some(1_790_000_000),
                        },
                    ]),
                },
            }]
        }
        // The interface already shows the new text, and the pin.
        Command::Describe { .. } | Command::Pin { .. } => Vec::new(),
        Command::Pins { channel } => {
            let mut message = crate::model::Message {
                ts: crate::model::Ts::new("1790168400.000100"),
                user: Some("U03".into()),
                username: None,
                bot_icon: None,
                bot_id: None,
                text: "Release checklist: tag, build, notarize, announce in #general.".into(),
                thread_ts: None,
                reply_count: 0,
                replies_known: true,
                reply_users: Vec::new(),
                latest_reply: None,
                reactions: Vec::new(),
                files: Vec::new(),
                attachments: Vec::new(),
                blocks: Vec::new(),
                edited: false,
                subtype: None,
                delivery: crate::model::Delivery::Sent,
                broadcast: false,
                pinned: true,
            };
            let first = convos::Pin {
                message: message.clone(),
                by: Some("U03".into()),
            };
            message.ts = crate::model::Ts::new("1790000000.000100");
            message.user = Some("U01".into());
            message.text = "Design review every Thursday at 14:00 :calendar:".into();
            vec![Event::Convos {
                team: team.to_owned(),
                event: convos::Event::Pins {
                    channel,
                    result: Ok(vec![
                        first,
                        convos::Pin {
                            message,
                            by: Some("U00".into()),
                        },
                    ]),
                },
            }]
        }
        Command::Bookmarks { channel } => {
            let bookmark = |id: &str, title: &str, link: &str, emoji: &str| convos::Bookmark {
                id: id.into(),
                title: title.into(),
                link: link.into(),
                emoji: Some(emoji.into()),
            };
            vec![Event::Convos {
                team: team.to_owned(),
                event: convos::Event::Bookmarks {
                    channel,
                    result: Ok(vec![
                        bookmark("Bk1", "Roadmap", "https://example.com/roadmap", "world_map"),
                        bookmark("Bk2", "CI dashboard", "https://ci.example.com", "rocket"),
                        bookmark("Bk3", "Style guide", "https://example.com/style", "art"),
                    ]),
                },
            }]
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
    fn files_say_who_shared_them_and_when() {
        let page: FilesPage = serde_json::from_str(
            r#"{"ok":true,"files":[
                {"id":"F1","name":"a.pdf","mimetype":"application/pdf","size":10,
                 "user":"U1","created":1700000000},
                {"id":"F2","name":"gone","mode":"tombstone"}
            ]}"#,
        )
        .expect("json");
        let files = shared(page);
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].file.name, "a.pdf");
        assert_eq!(files[0].user.as_deref(), Some("U1"));
        assert_eq!(files[0].created, Some(1_700_000_000));
    }

    #[test]
    fn members_and_creation_come_from_their_own_answers() {
        let page: MembersPage = serde_json::from_str(
            r#"{"ok":true,"members":["U1","U2"],"response_metadata":{"next_cursor":""}}"#,
        )
        .expect("json");
        assert_eq!(page.members, ["U1", "U2"]);
        assert_eq!(page.response_metadata.cursor(), None);
        let made: Made = serde_json::from_str(
            r#"{"ok":true,"channel":{"id":"C1","created":1600000000,"creator":"U9"}}"#,
        )
        .expect("json");
        assert_eq!(made.channel.created, Some(1_600_000_000));
        assert_eq!(made.channel.creator.as_deref(), Some("U9"));
    }

    #[test]
    fn pins_keep_only_messages_and_bookmarks_their_links() {
        let page: PinsPage = serde_json::from_str(
            r#"{"ok":true,"items":[
                {"type":"message","created_by":"U2","channel":"C1",
                 "message":{"type":"message","ts":"1.000100","user":"U1","text":"hi"}},
                {"type":"file","file":{"id":"F1"}}
            ]}"#,
        )
        .expect("json");
        let pins = pins(page);
        assert_eq!(pins.len(), 1);
        assert!(pins[0].message.pinned);
        assert_eq!(pins[0].message.text, "hi");
        assert_eq!(pins[0].by.as_deref(), Some("U2"));
        let page: BookmarksPage = serde_json::from_str(
            r#"{"ok":true,"bookmarks":[
                {"id":"Bk1","title":"","link":"https://a.example","emoji":":rocket:"},
                {"id":"Bk2","title":"Folder","type":"folder"}
            ]}"#,
        )
        .expect("json");
        let marks = bookmarks(page);
        assert_eq!(marks.len(), 1);
        assert_eq!(marks[0].title, "https://a.example");
        assert_eq!(marks[0].emoji.as_deref(), Some("rocket"));
    }

    #[test]
    fn pin_events_name_the_message() {
        let event: serde_json::Value = serde_json::from_str(
            r#"{"type":"pin_removed","user":"U3","channel_id":"C1",
                "item":{"type":"message","channel":"C1","message":{"ts":"5.000100"}}}"#,
        )
        .expect("json");
        assert_eq!(
            pin_event("pin_removed", &event),
            Some(convos::Event::Pinned {
                channel: "C1".into(),
                ts: crate::model::Ts::new("5.000100"),
                pinned: false,
                by: Some("U3".into()),
            })
        );
        let file: serde_json::Value =
            serde_json::from_str(r#"{"type":"pin_added","item":{"type":"file"}}"#).expect("json");
        assert_eq!(pin_event("pin_added", &file), None);
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
