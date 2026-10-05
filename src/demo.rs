//! A pretend Slack for screenshots and offline work on the interface
//! (`--demo`, with the `demo` feature). Nothing here touches the network.

use std::collections::HashMap;

use tokio::sync::mpsc;

mod views;

use crate::backend::{Command, Event, Sink, Socket, UploadGate};
use crate::credentials::AppCredentials;
use crate::model::{
    Attachment, Bot, Conversation, ConversationKind, Delivery, File, Message, Reaction,
    SectionKind, SidebarSection, Ts, User, UserGroup, Workspace,
};
use crate::notice::Notice;

pub const TEAM: &str = "TDEMO";
pub const ME: &str = "U00";
/// The demo's picture, registered with egui under this URI by the app.
pub const PICTURE: &str = "bytes://noslacking-demo-picture.png";
pub const PICTURE_BYTES: &[u8] = include_bytes!("../assets/demo/picture.png");
/// An animated custom emoji, as a workspace's `emoji.list` names a GIF.
pub const PARROT: &str = "bytes://noslacking-demo-parrot.gif";
pub const PARROT_BYTES: &[u8] = include_bytes!("../assets/demo/parrot.gif");

/// Serves `slow://` images after a delay, the way link previews and avatars
/// trickle in from the network after the first layout.
#[derive(Default)]
pub struct SlowImages {
    asked: std::sync::Mutex<HashMap<String, std::time::Instant>>,
}

impl SlowImages {
    const DELAY: std::time::Duration = std::time::Duration::from_millis(900);
}

impl egui::load::BytesLoader for SlowImages {
    fn id(&self) -> &str {
        egui::generate_loader_id!(SlowImages)
    }

    fn load(&self, ctx: &egui::Context, uri: &str) -> egui::load::BytesLoadResult {
        if !uri.starts_with("slow://") {
            return Err(egui::load::LoadError::NotSupported);
        }
        let asked = *self
            .asked
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .entry(uri.to_owned())
            .or_insert_with(std::time::Instant::now);
        if asked.elapsed() < Self::DELAY {
            ctx.request_repaint_after(std::time::Duration::from_millis(50));
            return Ok(egui::load::BytesPoll::Pending { size: None });
        }
        Ok(egui::load::BytesPoll::Ready {
            size: None,
            bytes: egui::load::Bytes::Static(PICTURE_BYTES),
            mime: None,
        })
    }

    fn forget(&self, _uri: &str) {}

    fn forget_all(&self) {}

    fn byte_size(&self) -> usize {
        0
    }
}

/// A fixed "now" so screenshots never change: 2026-09-30 14:00 UTC.
const NOW: u64 = 1_790_172_000;

/// The demo's fixed "now" in Unix seconds, for what the interface
/// measures against the clock, such as hiding quiet conversations.
pub fn now() -> i64 {
    i64::try_from(NOW).unwrap_or_default()
}

/// A day in seconds, for the demo's older conversations.
const DAY: u64 = 24 * 60 * 60;

fn user(id: &str, name: &str, real: &str, title: &str) -> User {
    User {
        id: id.into(),
        name: name.into(),
        real_name: real.into(),
        display_name: real.split(' ').next().unwrap_or(real).into(),
        title: title.into(),
        ..User::default()
    }
}

fn users() -> Vec<User> {
    vec![
        user(ME, "you", "Yannick Example", "Engineer"),
        // Someone in another time zone, for the profile card's clock.
        User {
            tz: Some("America/Sao_Paulo".into()),
            status_text: "Reviewing mockups".into(),
            status_emoji: ":art:".into(),
            ..user("U01", "ana", "Ana Lima", "Design lead")
        },
        user("U02", "bob", "Bob Martens", "Backend"),
        user("U03", "carla", "Carla Rossi", "Product"),
        user("U04", "dev", "Dev Patel", "Infrastructure"),
        User {
            is_bot: true,
            ..user("U05", "ci", "Deploy Bot", "")
        },
        // Someone from a partner company, reached through Slack Connect.
        User {
            team: "TPARTNER".into(),
            ..user("U06", "lee", "Lee Chen", "Partner engineer")
        },
    ]
}

fn conversation(
    id: &str,
    name: &str,
    kind: ConversationKind,
    latest: u64,
    read: u64,
) -> Conversation {
    Conversation {
        id: id.into(),
        name: name.into(),
        kind,
        user: None,
        topic: String::new(),
        purpose: String::new(),
        members: Some(12),
        archived: false,
        last_read: Some(ts(read)),
        latest: Some(ts(latest)),
        unread: 0,
        mentions: 0,
        external: false,
    }
}

fn ts(seconds: u64) -> Ts {
    Ts::new(format!("{seconds}.000100"))
}

fn conversations() -> Vec<Conversation> {
    let mut list = vec![
        Conversation {
            topic: "Company-wide announcements and work-based matters".into(),
            ..conversation(
                "C01",
                "general",
                ConversationKind::Channel,
                NOW - 60,
                NOW - 60,
            )
        },
        Conversation {
            topic: "Shipping the native client :rocket:".into(),
            mentions: 1,
            ..conversation(
                "C02",
                "engineering",
                ConversationKind::Channel,
                NOW - 30,
                NOW - 4000,
            )
        },
        conversation(
            "C03",
            "design",
            ConversationKind::Channel,
            NOW - 9000,
            NOW - 2000,
        ),
        conversation(
            "C04",
            "random",
            ConversationKind::Channel,
            NOW - 500,
            NOW - 9000,
        ),
        conversation(
            "G01",
            "incident-room",
            ConversationKind::Private,
            NOW - 90_000,
            NOW - 90_000,
        ),
        conversation(
            "C05",
            "deploys",
            ConversationKind::Channel,
            NOW - 100,
            NOW - 100,
        ),
        // Shared with a partner company through Slack Connect.
        Conversation {
            external: true,
            ..conversation(
                "C06",
                "acme-partners",
                ConversationKind::Channel,
                NOW - 20_000,
                NOW - 20_000,
            )
        },
    ];
    // Channels quiet for longer than a month, which the sidebar hides
    // behind "N more" by default.
    for (id, name, kind, days) in [
        ("C07", "website-2025", ConversationKind::Channel, 45),
        ("C08", "offsite-planning", ConversationKind::Channel, 120),
        ("C09", "design-archive", ConversationKind::Channel, 60),
        ("G02", "hiring-backend", ConversationKind::Private, 80),
    ] {
        list.push(conversation(
            id,
            name,
            kind,
            NOW - days * DAY,
            NOW - days * DAY,
        ));
    }
    for (id, user, latest, read) in [
        ("D01", "U01", NOW - 200, NOW - 900),
        ("D02", "U02", NOW - 7200, NOW - 7200),
        ("D03", "U03", NOW - 86_000, NOW - 86_000),
        // Quiet for over a month: hidden behind "N more".
        ("D05", "U04", NOW - 40 * DAY, NOW - 40 * DAY),
        // A DM with the deploy bot, which Slack files under Apps.
        ("D04", "U05", NOW - 5000, NOW - 5000),
    ] {
        list.push(Conversation {
            user: Some(user.into()),
            ..conversation(id, user, ConversationKind::Direct, latest, read)
        });
    }
    list.push(conversation(
        "M01",
        "ana, bob, carla",
        ConversationKind::Group,
        NOW - 3000,
        NOW - 3000,
    ));
    list
}

fn message(seconds: u64, user: &str, text: &str) -> Message {
    Message {
        ts: ts(seconds),
        user: Some(user.into()),
        username: None,
        bot_icon: None,
        bot_id: None,
        text: text.into(),
        thread_ts: None,
        reply_count: 0,
        replies_known: false,
        reply_users: Vec::new(),
        latest_reply: None,
        reactions: Vec::new(),
        files: Vec::new(),
        attachments: Vec::new(),
        blocks: Vec::new(),
        edited: false,
        subtype: None,
        delivery: Delivery::Sent,
        broadcast: false,
        pinned: false,
        client_msg_id: None,
    }
}

/// A message as Slack's JSON has it, through the real parser.
fn from_json(json: &str) -> Message {
    serde_json::from_str::<crate::slack::types::Message>(json)
        .ok()
        .and_then(crate::slack::types::Message::into_model)
        .unwrap_or_else(|| message(NOW, "U05", "unreadable demo message"))
}

/// A GlitchTip alert, in the shape GlitchTip's "Slack-compatible webhook"
/// posts it (apps/alerts/webhooks.py) and Slack stores it.
fn glitchtip_alert() -> Message {
    from_json(&format!(
        r##"{{"type":"message","subtype":"bot_message","ts":"{}.000100","bot_id":"B07",
        "text":"GlitchTip Alert",
        "attachments":[{{"id":1,"color":"e52b50","fallback":"[no preview available]",
          "title":"ValueError: invalid literal for int() with base 10: 'abc'",
          "title_link":"https://glitchtip.example.com/acme/issues/4211",
          "text":"apps.orders.views in checkout",
          "mrkdown_in":["text"],
          "fields":[
            {{"title":"Project","value":"backend","short":true}},
            {{"title":"Environment","value":"production","short":true}},
            {{"title":"Server Name","value":"web-3","short":true}},
            {{"title":"Release","value":"2026.10.1","short":false}}
          ]}}]}}"##,
        NOW - 600
    ))
}

/// A Block Kit message: header, a section with fields and an image, a
/// context line, a divider and link buttons.
fn block_kit_release() -> Message {
    from_json(&format!(
        r##"{{"type":"message","subtype":"bot_message","ts":"{}.000100","bot_id":"B08","username":"Release Bot",
        "text":"Release 2026.10.1 is ready",
        "blocks":[
          {{"type":"header","text":{{"type":"plain_text","text":"Release 2026.10.1 is ready :package:","emoji":true}}}},
          {{"type":"section","text":{{"type":"mrkdwn","text":"*14 changes* since the last release, built from `main` by <@U02>."}},
            "fields":[{{"type":"mrkdwn","text":"*Status*\nPassed"}},{{"type":"mrkdwn","text":"*Duration*\n6m 12s"}},
                      {{"type":"mrkdwn","text":"*Platforms*\nLinux, macOS, Windows"}},{{"type":"mrkdwn","text":"*Size*\n9.4 MB"}}],
            "accessory":{{"type":"image","image_url":"{PICTURE}","alt_text":"build preview"}}}},
          {{"type":"context","elements":[{{"type":"mrkdwn","text":"Triggered by a push to `main` · <https://ci.example.com/1288|build #1288>"}}]}},
          {{"type":"divider"}},
          {{"type":"actions","elements":[
            {{"type":"button","text":{{"type":"plain_text","text":"View release"}},"style":"primary","url":"https://github.com/example/noslacking/releases"}},
            {{"type":"button","text":{{"type":"plain_text","text":"Approve"}},"action_id":"approve"}}]}}
        ]}}"##,
        NOW - 100
    ))
}

fn reaction(name: &str, users: &[&str]) -> Reaction {
    Reaction {
        name: name.into(),
        count: users.len() as u32,
        users: users.iter().map(|u| (*u).to_owned()).collect(),
    }
}

const THREAD: u64 = NOW - 3600;

/// How long the pretend network takes for a page of #general.
const LATENCY: std::time::Duration = std::time::Duration::from_millis(400);
const LONG: usize = 130;
const PAGE: usize = 50;

/// #general's long history, oldest first; the last message is the newest.
fn long_history() -> Vec<Message> {
    let people = ["U01", "U02", "U03", "U04"];
    (0..LONG)
        .map(|i| {
            let text = match i % 5 {
                0 => format!("Message {i}: standup notes are in the doc :memo:"),
                1 => format!("Message {i}: can someone review <https://github.com/example/noslacking/pull/{i}|#{i}>?"),
                2 => format!("Message {i}: a longer one that wraps onto a second line, so the list has the uneven row heights a real conversation has"),
                3 => format!("Message {i}:\n• first point\n• second point"),
                _ => format!("Message {i} :+1:"),
            };
            let mut message = message(NOW - 90 * (LONG - i) as u64, people[i % people.len()], &text);
            // A link preview now and then, with a picture whose size is only
            // known once it arrives.
            if i % 4 == 1 {
                message.attachments.push(Attachment {
                    color: None,
                    service: Some("GitHub".into()),
                    pretext: None,
                    title: Some(format!("Pull request #{i}")),
                    title_link: Some(format!("https://github.com/example/noslacking/pull/{i}")),
                    text: "Faster sidebar layout".into(),
                    fields: Vec::new(),
                    image: Some(format!("slow://preview-{i}.png")),
                    thumb: None,
                    footer: None,
                    blocks: Vec::new(),
                    ..Attachment::default()
                });
            }
            message
        })
        .chain(std::iter::once(message(
            NOW - 30,
            "U01",
            "This is the newest message in #general.",
        )))
        .collect()
}

/// A page of [`long_history`] ending before `before` (an index), newest
/// page first, with the cursor for the page before it.
fn long_page(before: Option<usize>) -> (Vec<Message>, Option<String>) {
    let all = long_history();
    let end = before.unwrap_or(all.len()).min(all.len());
    let start = end.saturating_sub(PAGE);
    let cursor = (start > 0).then(|| start.to_string());
    (all[start..end].to_vec(), cursor)
}

fn history(channel: &str) -> Vec<Message> {
    match channel {
        "C02" => vec![
            Message {
                subtype: Some("channel_join".into()),
                ..message(NOW - 86_400 * 2, "U04", "<@U04> has joined the channel")
            },
            message(
                NOW - 86_400 - 5000,
                "U02",
                "Morning! The Socket Mode reconnect fix is in review: <https://github.com/example/noslacking/pull/42|#42>",
            ),
            Message {
                reactions: vec![reaction("eyes", &["U01", "U03"])],
                ..message(
                    NOW - 86_400 - 4900,
                    "U02",
                    "It backs off properly now instead of hammering `apps.connections.open`.",
                )
            },
            Message {
                thread_ts: Some(ts(THREAD)),
                reply_count: 3,
                replies_known: true,
                reply_users: vec!["U01".into(), "U04".into()],
                latest_reply: Some(ts(NOW - 1200)),
                reactions: vec![
                    reaction("tada", &["U01", "U02", "U04"]),
                    reaction("rocket", &[ME]),
                ],
                ..message(
                    THREAD,
                    "U03",
                    "*Release plan for Friday* :calendar:\n• freeze `main` at noon\n• smoke test on Linux, macOS and Windows\n• ship :rocket:",
                )
            },
            Message {
                files: vec![File {
                    id: "F01".into(),
                    name: "sidebar-v2.png".into(),
                    title: "sidebar-v2.png".into(),
                    mimetype: "image/png".into(),
                    size: 48_213,
                    url_private: None,
                    download_url: None,
                    thumb: Some(PICTURE.into()),
                    thumb_size: Some([480.0, 270.0]),
                    permalink: None,
                    original_size: Some([480.0, 270.0]),
                    ..File::default()
                }],
                ..message(
                    NOW - 2400,
                    "U01",
                    "New sidebar spacing, what do you think <!subteam^S01>?",
                )
            },
            Message {
                files: vec![File {
                    id: "F02".into(),
                    name: "sidebar-v2-light.png".into(),
                    title: "sidebar-v2-light.png".into(),
                    mimetype: "image/png".into(),
                    size: 51_002,
                    thumb: Some(PICTURE.into()),
                    thumb_size: Some([480.0, 270.0]),
                    original_size: Some([480.0, 270.0]),
                    permalink: Some(
                        "https://acme.slack.com/files/U01/F02/sidebar-v2-light.png".into(),
                    ),
                    ..File::default()
                }],
                ..message(NOW - 2380, "U01", "And in the light theme.")
            },
            message(
                NOW - 2350,
                "U01",
                "I tightened the rows to 28px and gave unread channels a bolder weight.",
            ),
            Message {
                reactions: vec![reaction("heart", &["U01"])],
                ..message(
                    NOW - 2000,
                    ME,
                    "Looks great! Much easier to scan :+1::skin-tone-3:",
                )
            },
            Message {
                username: Some("Deploy Bot".into()),
                user: None,
                subtype: Some("bot_message".into()),
                attachments: vec![Attachment {
                    color: Some(egui::Color32::from_rgb(0x2e, 0xb6, 0x7d)),
                    service: Some("CI".into()),
                    pretext: None,
                    title: Some("Build #1287 passed".into()),
                    title_link: Some("https://ci.example.com/1287".into()),
                    text: "`noslacking` on `main` in 6m 12s".into(),
                    fields: Vec::new(),
                    image: None,
                    thumb: None,
                    footer: Some("ci.example.com".into()),
                    blocks: Vec::new(),
                    ..Attachment::default()
                }],
                ..message(NOW - 900, "U05", "")
            },
            message(
                NOW - 600,
                "U03",
                "The backoff, for the record:\n```rust\n/// Waits longer after each failure.\nfn backoff(attempt: u32) -> Duration {\n    let ms = 500 * 2u64.pow(attempt.min(5));\n    Duration::from_millis(ms) // at most 16 s\n}\n```",
            ),
            message(
                NOW - 300,
                "U04",
                "Heads up <!here>: staging restarts at 15:00. Ping <@U00> if anything looks off.",
            ),
            message(
                NOW - 30,
                "U02",
                "> It backs off properly now\nConfirmed, survived a night of flaky wifi :sweat_smile:",
            ),
        ],
        "C01" => vec![
            message(NOW - 7200, "U03", "Welcome to the team, <@U04>! :wave:"),
            message(NOW - 60, "U01", "Lunch is in the kitchen today :pizza:"),
        ],
        "C05" => vec![
            Message {
                reactions: vec![
                    reaction("partyparrot", &["U01", ME]),
                    reaction("tada", &["U02"]),
                ],
                ..message(
                    NOW - 4000,
                    "U04",
                    "Deploys and alerts land here :partyparrot: :rocket: :white_check_mark: :fire:",
                )
            },
            glitchtip_alert(),
            block_kit_release(),
        ],
        "D01" => vec![
            message(NOW - 1000, ME, "Can you look at the new reaction picker?"),
            message(
                NOW - 200,
                "U01",
                "Sure, sending notes in a bit :slightly_smiling_face:",
            ),
            // A permalink to an old message in #general, which opens here.
            message(
                NOW - 150,
                "U01",
                &format!(
                    "Same question came up before: <https://acme-inc.slack.com/archives/C01/p{}000100>",
                    NOW - 90 * (LONG - 20) as u64
                ),
            ),
        ],
        // Media: a video link's preview, a blog post's, a screen recording,
        // a voice memo and a PDF.
        "C03" => vec![
            Message {
                attachments: vec![Attachment {
                    service: Some("YouTube".into()),
                    author: Some("Rust Conference".into()),
                    author_link: Some("https://www.youtube.com/@rustconf".into()),
                    title: Some("Immediate mode interfaces in practice".into()),
                    title_link: Some("https://www.youtube.com/watch?v=demo".into()),
                    thumb: Some(PICTURE.into()),
                    thumb_size: Some([480.0, 270.0]),
                    video: Some("https://www.youtube.com/watch?v=demo".into()),
                    color: Some(egui::Color32::from_rgb(0xff, 0x00, 0x33)),
                    ..Attachment::default()
                }],
                ..message(
                    NOW - 9600,
                    "U01",
                    "Worth watching: <https://www.youtube.com/watch?v=demo>",
                )
            },
            Message {
                attachments: vec![Attachment {
                    service: Some("Design Notes".into()),
                    title: Some("Density, revisited".into()),
                    title_link: Some("https://design.example/density".into()),
                    text:
                        "How much should fit on one screen? Notes from a year of compact layouts."
                            .into(),
                    image: Some(PICTURE.into()),
                    image_size: Some([1200.0, 675.0]),
                    ..Attachment::default()
                }],
                ..message(NOW - 9500, "U03", "<https://design.example/density>")
            },
            Message {
                files: vec![
                    File {
                        id: "F10".into(),
                        name: "sidebar-walkthrough.mp4".into(),
                        mimetype: "video/mp4".into(),
                        size: 8_412_000,
                        url_private: Some(
                            "https://files.slack.com/files-pri/TDEMO-F10/walkthrough.mp4".into(),
                        ),
                        poster: Some(PICTURE.into()),
                        poster_size: Some([640.0, 360.0]),
                        ..File::default()
                    },
                    File {
                        id: "F11".into(),
                        name: "voice-memo.m4a".into(),
                        mimetype: "audio/mp4".into(),
                        size: 412_000,
                        url_private: Some(
                            "https://files.slack.com/files-pri/TDEMO-F11/memo.m4a".into(),
                        ),
                        ..File::default()
                    },
                ],
                ..message(NOW - 9200, "U01", "Screen recording and my notes:")
            },
            Message {
                files: vec![File {
                    id: "F12".into(),
                    name: "style-guide.pdf".into(),
                    mimetype: "application/pdf".into(),
                    size: 1_204_000,
                    download_url: Some(
                        "https://files.slack.com/files-pri/TDEMO-F12/download/style-guide.pdf"
                            .into(),
                    ),
                    url_private: Some(
                        "https://files.slack.com/files-pri/TDEMO-F12/style-guide.pdf".into(),
                    ),
                    poster: Some(PICTURE.into()),
                    poster_size: Some([480.0, 270.0]),
                    ..File::default()
                }],
                ..message(NOW - 9000, "U03", "The style guide, updated.")
            },
        ],
        _ => vec![message(
            NOW - 9000,
            "U02",
            "Nothing much happening here yet.",
        )],
    }
}

/// Everything a conversation holds, oldest first.
fn all_history(channel: &str) -> Vec<Message> {
    if channel == "C01" {
        long_history()
    } else {
        history(channel)
    }
}

/// The messages around `ts`, as [`Command::LoadAround`] answers: up to a
/// short page either side, whether there is more each way, and the cursor
/// for older pages (as [`long_page`] reads it).
fn around(channel: &str, ts: &Ts) -> (Vec<Message>, bool, Option<String>, bool) {
    let all = all_history(channel);
    let at = all.partition_point(|m| m.ts < *ts);
    let start = at.saturating_sub(SIDE);
    let end = (at + SIDE + 1).min(all.len());
    let cursor = (start > 0 && channel == "C01").then(|| start.to_string());
    (all[start..end].to_vec(), start > 0, cursor, end < all.len())
}

/// Searches the pretend workspace as Slack would: the words of `query`
/// (its modifiers left out) in every message, matches marked, a page at a
/// time.
fn search(query: &crate::search::Query, page: u32) -> crate::search::Page {
    use crate::search::{FileHit, Hit, MATCH_END, MATCH_START, PAGE_SIZE, Page, Scope, Sort};
    let words: Vec<String> = query
        .text
        .split_whitespace()
        .filter(|w| !w.contains(':'))
        .map(str::to_lowercase)
        .collect();
    let channels = ["C01", "C02", "C05", "D01"];
    let mut hits: Vec<Hit> = Vec::new();
    match query.scope {
        Scope::Messages => {
            let replies = thread().into_iter().skip(1).map(|m| ("C02", m));
            let all = channels
                .iter()
                .flat_map(|c| all_history(c).into_iter().map(move |m| (*c, m)))
                .chain(replies);
            for (channel, message) in all {
                let lower = message.text.to_lowercase();
                if words.is_empty() || !words.iter().all(|w| lower.contains(w.as_str())) {
                    continue;
                }
                // Mark each word where it stands, keeping the text's case.
                let mut text = String::new();
                let mut rest = message.text.as_str();
                while let Some((at, len)) = words
                    .iter()
                    .filter_map(|w| rest.to_lowercase().find(w.as_str()).map(|at| (at, w.len())))
                    .min()
                {
                    if !rest.is_char_boundary(at) || !rest.is_char_boundary(at + len) {
                        break;
                    }
                    text.push_str(&rest[..at]);
                    text.push(MATCH_START);
                    text.push_str(&rest[at..at + len]);
                    text.push(MATCH_END);
                    rest = &rest[at + len..];
                }
                text.push_str(rest);
                hits.push(Hit {
                    key: format!("{channel}/{}", message.ts.as_str()),
                    channel: Some(channel.to_owned()),
                    channel_name: channel.to_owned(),
                    thread: message.thread_ts.clone().filter(|t| *t != message.ts),
                    ts: Some(message.ts.clone()),
                    when: Some(message.ts.clone()),
                    user: message.user.clone(),
                    username: message.username.clone(),
                    text,
                    file: None,
                    permalink: None,
                });
            }
        }
        Scope::Files => hits.push(Hit {
            key: "F01".into(),
            channel: Some("C02".into()),
            channel_name: "engineering".into(),
            ts: Some(ts(NOW - 2400)),
            thread: None,
            when: Some(ts(NOW - 2400)),
            user: Some("U01".into()),
            username: None,
            text: format!("{MATCH_START}sidebar{MATCH_END}-v2.png"),
            file: Some(FileHit {
                name: "sidebar-v2.png".into(),
                title: "sidebar-v2.png".into(),
                mimetype: "image/png".into(),
                size: 48_213,
            }),
            permalink: None,
        }),
    }
    if query.sort == Sort::Newest {
        hits.sort_by(|a, b| b.when.cmp(&a.when));
    }
    let total = hits.len() as u32;
    let pages = total.div_ceil(PAGE_SIZE).max(1);
    let start = ((page.max(1) - 1) * PAGE_SIZE) as usize;
    Page {
        hits: hits
            .into_iter()
            .skip(start)
            .take(PAGE_SIZE as usize)
            .collect(),
        page,
        pages,
        total,
    }
}

/// How many messages [`around`] reads either side.
const SIDE: usize = 15;

/// The timestamp of message `index` of #general's long history, for
/// jumping to it.
pub fn long_history_ts(index: usize) -> Ts {
    long_history()
        .get(index)
        .map(|m| m.ts.clone())
        .unwrap_or_default()
}

fn thread() -> Vec<Message> {
    let mut parent = history("C02")
        .into_iter()
        .find(|m| m.ts == ts(THREAD))
        .unwrap_or_else(|| message(THREAD, "U03", ""));
    parent.thread_ts = Some(ts(THREAD));
    let reply = |seconds, user, text| Message {
        thread_ts: Some(ts(THREAD)),
        ..message(seconds, user, text)
    };
    vec![
        parent,
        reply(NOW - 3000, "U01", "I can own the macOS smoke test."),
        reply(NOW - 2500, "U04", "Windows ARM too? I have the box set up."),
        reply(NOW - 1200, "U01", "Yes please :pray:"),
    ]
}

pub async fn run(sink: Sink, mut commands: mpsc::UnboundedReceiver<Command>) {
    sink.send(Event::AppLoaded(Some(AppCredentials {
        client_id: "1234567890.0987654321".into(),
        client_secret: "demo".into(),
        app_token: "xapp-demo".into(),
    })));
    for (id, name) in [(TEAM, "Acme Inc"), ("TDEMO2", "Open Source")] {
        sink.send(Event::WorkspaceReady(Workspace {
            team_id: id.into(),
            name: name.into(),
            domain: name.to_lowercase().replace(' ', "-"),
            icon: None,
            user_id: ME.into(),
        }));
        sink.send(Event::Users {
            team: id.into(),
            users: users(),
        });
        // What bots.info answers for the webhook that posts GlitchTip alerts.
        sink.send(Event::Bots {
            team: id.into(),
            bots: vec![Bot {
                id: "B07".into(),
                name: "GlitchTip".into(),
                icon: None,
            }],
        });
        sink.send(Event::Emoji {
            team: id.into(),
            emoji: HashMap::from([
                ("partyparrot".to_owned(), PARROT.to_owned()),
                ("parrot".to_owned(), "alias:partyparrot".to_owned()),
            ]),
        });
        // Groups to mention: type `@des` or `@eng` in the composer.
        let group = |id: &str, handle: &str, name: &str, members| UserGroup {
            id: id.into(),
            handle: handle.into(),
            name: name.into(),
            members: Some(members),
        };
        sink.send(Event::UserGroups {
            team: id.into(),
            groups: vec![
                group("S01", "design", "Design team", 3),
                group("S02", "engineering", "Engineering", 5),
            ],
        });
        sink.send(Event::Conversations {
            team: id.into(),
            list: conversations(),
            complete: true,
        });
    }
    // Your sidebar as Slack keeps it: Starred, a section of your own (with a
    // DM in it), then the catch-alls.
    let section =
        |id: &str, kind: SectionKind, name: &str, emoji: &str, ids: &[&str]| SidebarSection {
            id: id.into(),
            kind,
            name: name.into(),
            emoji: emoji.into(),
            channel_ids: ids.iter().map(|s| (*s).to_owned()).collect(),
        };
    sink.send(Event::Sections {
        team: TEAM.into(),
        sections: vec![
            section("L01", SectionKind::Starred, "", "", &["C02"]),
            section(
                "L02",
                SectionKind::Custom,
                "Design team",
                "art",
                &["C03", "G01", "D01", "C09"],
            ),
            section("L03", SectionKind::Channels, "", "", &[]),
            section("L04", SectionKind::DirectMessages, "", "", &[]),
            section("L05", SectionKind::Apps, "", "", &[]),
        ],
    });
    // #deploys is muted, as a browser session's preferences would say.
    sink.send(Event::SlackPrefs {
        team: TEAM.into(),
        prefs: crate::desktop::SlackPrefs {
            muted: std::collections::HashSet::from(["C05".to_owned()]),
            ..crate::desktop::SlackPrefs::default()
        },
    });
    sink.send(Event::Socket(Socket::Connected));
    sink.send(crate::backend::people::demo_huddle(TEAM));
    // Ana keeps typing in her direct message, as Slack repeats it.
    let typing = sink.clone();
    tokio::spawn(async move {
        loop {
            typing.send(Event::People {
                team: TEAM.into(),
                event: crate::people::Event::Typing {
                    channel: "D01".into(),
                    thread: None,
                    user: "U01".into(),
                },
            });
            tokio::time::sleep(std::time::Duration::from_secs(3)).await;
        }
    });
    let mut sent = 0;
    let mut uploads: HashMap<u64, (tokio::task::AbortHandle, UploadGate)> = HashMap::new();
    while let Some(command) = commands.recv().await {
        match command {
            // #general pages like the real API, slowly, to exercise loading
            // and scrolling; the rest arrive at once.
            Command::LoadHistory { team, channel } if channel == "C01" => {
                tokio::time::sleep(LATENCY).await;
                let (messages, cursor) = long_page(None);
                sink.send(Event::History {
                    team,
                    channel,
                    messages,
                    has_more: cursor.is_some(),
                    cursor,
                    older: false,
                    polled: false,
                });
            }
            Command::LoadOlder {
                team,
                channel,
                cursor,
            } if channel == "C01" => {
                tokio::time::sleep(LATENCY).await;
                let (messages, cursor) = long_page(cursor.parse().ok());
                sink.send(Event::History {
                    team,
                    channel,
                    messages,
                    has_more: cursor.is_some(),
                    cursor,
                    older: true,
                    polled: false,
                });
            }
            Command::LoadAround { team, channel, ts } => {
                tokio::time::sleep(LATENCY).await;
                let (messages, has_older, cursor, has_newer) = around(&channel, &ts);
                sink.send(Event::Around {
                    team,
                    channel,
                    ts,
                    messages,
                    has_older,
                    cursor,
                    has_newer,
                });
            }
            Command::LoadNewer {
                team,
                channel,
                after,
            } => {
                tokio::time::sleep(LATENCY).await;
                let all = all_history(&channel);
                let start = all.partition_point(|m| m.ts <= after);
                let end = (start + PAGE).min(all.len());
                sink.send(Event::Newer {
                    team,
                    channel,
                    messages: all[start..end].to_vec(),
                    has_newer: end < all.len(),
                });
            }
            Command::Search {
                query,
                page,
                request,
            } => {
                tokio::time::sleep(LATENCY / 2).await;
                sink.send(Event::Search {
                    team: query.team.clone(),
                    request,
                    result: Ok(search(&query, page)),
                });
            }
            Command::LoadHistory { team, channel } => sink.send(Event::History {
                team,
                messages: history(&channel),
                channel,
                has_more: false,
                cursor: None,
                older: false,
                polled: false,
            }),
            Command::LoadThread { team, channel, ts } => sink.send(Event::Thread {
                team,
                channel,
                ts,
                messages: thread(),
            }),
            Command::Send {
                team,
                channel,
                text,
                thread,
                local,
                ..
            } => {
                sent += 1;
                let mut message = message(NOW + sent, ME, &text);
                message.thread_ts = thread;
                sink.send(Event::Sent {
                    team,
                    channel,
                    local,
                    result: Ok(message),
                });
            }
            // A slow pretend upload, so its progress, Cancel and the last
            // step (when Cancel goes) all show.
            Command::Upload { id, path, .. } => {
                let sink = sink.clone();
                let gate = UploadGate::default();
                let task = {
                    let gate = gate.clone();
                    tokio::spawn(async move {
                        const TOTAL: u64 = 2_400_000;
                        for step in 0..=40 {
                            sink.send(Event::UploadProgress {
                                id,
                                sent: TOTAL * step / 40,
                                total: TOTAL,
                            });
                            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
                        }
                        if !gate.finish() {
                            return;
                        }
                        sink.send(Event::UploadFinishing { id });
                        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                        sink.send(Event::Notice(Notice::DemoUpload {
                            path: path.display().to_string(),
                        }));
                        sink.send(Event::UploadDone { id, shared: true });
                    })
                };
                uploads.insert(id, (task.abort_handle(), gate));
            }
            // Every command works, as far as the demo can tell.
            Command::Slash { id, command, .. } => sink.send(Event::Slash {
                id,
                command,
                result: Ok(None),
            }),
            // Like the worker: too late once the last step began.
            Command::CancelUpload { id } => {
                if let Some((task, gate)) = uploads.remove(&id)
                    && !task.is_finished()
                    && gate.cancel()
                {
                    task.abort();
                    sink.send(Event::UploadCancelled { id });
                }
            }
            Command::Download { name, .. } => {
                sink.send(Event::Notice(Notice::DemoSave { name }));
            }
            Command::OpenFile { name, .. } => {
                sink.send(Event::Notice(Notice::DemoOpen { name }));
            }
            Command::Convos { team, command } => {
                for event in crate::backend::convos::demo(&team, command) {
                    sink.send(event);
                }
            }
            Command::People { team, command } => {
                for event in crate::backend::people::demo(&team, command) {
                    sink.send(event);
                }
            }
            Command::Views { team, command } => {
                for event in views::answer(&team, command) {
                    sink.send(event);
                }
            }
            _ => {}
        }
    }
}
