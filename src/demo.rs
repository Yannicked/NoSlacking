//! A pretend Slack for screenshots and offline work on the interface
//! (`--demo`, with the `demo` feature). Nothing here touches the network.

use std::collections::HashMap;

use tokio::sync::mpsc;

mod files;
mod views;

use crate::backend::{Command, Event, Sink, Socket, UploadGate};
use crate::credentials::AppCredentials;
use crate::model::{
    Attachment, Bot, Conversation, ConversationKind, Delivery, File, Message, Reaction,
    SectionKind, SidebarSection, SignInKind, Ts, User, UserGroup, Workspace,
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
        is_open: None,
        empty: false,
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
    // Group DMs Slack has closed: out of the sidebar, unless something
    // new came (the second one, unread, shows).
    for (id, name, latest, read) in [
        ("M02", "ana, dev", NOW - 2 * DAY, NOW - 2 * DAY),
        ("M03", "bob, carla, dev", NOW - 600, NOW - 4000),
    ] {
        list.push(Conversation {
            is_open: Some(false),
            ..conversation(id, name, ConversationKind::Group, latest, read)
        });
    }
    // Group DMs nobody ever wrote in, as Slack's counts report them:
    // behind "N more" while hiding quiet conversations.
    for (id, name) in [("M04", "ana, bob, dev"), ("M05", "carla, lee")] {
        list.push(Conversation {
            latest: None,
            last_read: None,
            empty: true,
            ..conversation(id, name, ConversationKind::Group, 0, 0)
        });
    }
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
        subscribed: None,
    }
}

/// Slack's copy of a message you sent: its text, the rich text block it
/// went with (the very `blocks` parameter sent to Slack) and its client
/// id, read through the real parser.
fn echo(seconds: u64, text: &str, client_msg_id: Option<String>) -> Message {
    let mut json = serde_json::json!({
        "type": "message",
        "ts": ts(seconds).as_str(),
        "user": ME,
        "text": text,
    });
    if let Some(blocks) = crate::slack::rich_out::blocks_param(text)
        .and_then(|blocks| serde_json::from_str::<serde_json::Value>(&blocks).ok())
    {
        json["blocks"] = blocks;
    }
    if let Some(id) = client_msg_id {
        json["client_msg_id"] = serde_json::Value::String(id);
    }
    serde_json::from_value::<crate::slack::types::Message>(json)
        .ok()
        .and_then(crate::slack::types::Message::into_model)
        .unwrap_or_else(|| message(seconds, ME, text))
}

/// A message as Slack's JSON has it, through the real parser.
fn from_json(json: &str) -> Message {
    serde_json::from_str::<crate::slack::types::Message>(json)
        .ok()
        .and_then(crate::slack::types::Message::into_model)
        .unwrap_or_else(|| message(NOW, "U05", "unreadable demo message"))
}

/// A link to the release plan in #engineering with Slack's own unfurl of
/// it, an attachment with `is_msg_unfurl`, as Slack stores it.
fn message_unfurl() -> Message {
    let link = format!("https://acme-inc.slack.com/archives/C02/p{THREAD}000100");
    from_json(
        &serde_json::json!({
            "type": "message",
            "ts": format!("{}.000100", NOW - 120),
            "user": ME,
            "text": format!("And the plan itself, for reference:\n<{link}>"),
            "attachments": [{
                "id": 1,
                "ts": format!("{THREAD}.000100"),
                "channel_id": "C02",
                "channel_name": "engineering",
                "is_msg_unfurl": true,
                "author_id": "U03",
                "author_name": "Carla Rossi",
                "author_subname": "Carla Rossi",
                "author_link": "https://acme-inc.slack.com/team/U03",
                "text": "*Release plan for Friday* :calendar:\n• freeze `main` at noon\n• smoke test on Linux, macOS and Windows\n• ship :rocket:",
                "fallback": "[September 30th, 2026 1:00 PM] Carla Rossi: Release plan for Friday",
                "from_url": link,
                "original_url": link,
                "color": "D0D0D0",
                "footer": "Posted in #engineering",
                "mrkdwn_in": ["text"],
            }],
        })
        .to_string(),
    )
}

/// A message by itself, as [`Command::FetchQuote`] answers: from any
/// conversation's history or the thread, none when it is not there.
fn quoted(channel: &str, ts: &Ts) -> Option<Message> {
    all_history(channel)
        .into_iter()
        .chain(if channel == "C02" {
            thread()
        } else {
            Vec::new()
        })
        .find(|m| m.ts == *ts)
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
/// picture with its size, a context line, a divider and link buttons.
fn block_kit_release() -> Message {
    from_json(&format!(
        r##"{{"type":"message","subtype":"bot_message","ts":"{}.000100","bot_id":"B08","username":"Release Bot",
        "text":"Release 2026.10.1 is ready",
        "blocks":[
          {{"type":"header","text":{{"type":"plain_text","text":"Release 2026.10.1 is ready :package:","emoji":true}}}},
          {{"type":"section","text":{{"type":"mrkdwn","text":"*14 changes* since the last release, built from `main` by <@U02>."}},
            "fields":[{{"type":"mrkdwn","text":"*Status*\nPassed"}},{{"type":"mrkdwn","text":"*Duration*\n6m 12s"}},
                      {{"type":"mrkdwn","text":"*Platforms*\nLinux, macOS, Windows"}},{{"type":"mrkdwn","text":"*Size*\n9.4 MB"}}],
            "accessory":{{"type":"image","image_url":"slow://build-preview.png","alt_text":"build preview"}}}},
          {{"type":"image","image_url":"slow://build-times.png","alt_text":"build times",
            "title":{{"type":"plain_text","text":"Build times this week"}},
            "image_width":480,"image_height":270,"image_bytes":48213}},
          {{"type":"context","elements":[{{"type":"mrkdwn","text":"Triggered by a push to `main` · <https://ci.example.com/1288|build #1288>"}}]}},
          {{"type":"divider"}},
          {{"type":"actions","block_id":"release","elements":[
            {{"type":"button","text":{{"type":"plain_text","text":"View release"}},"style":"primary","url":"https://github.com/example/noslacking/releases"}},
            {{"type":"button","text":{{"type":"plain_text","text":"Approve"}},"action_id":"approve"}}]}}
        ]}}"##,
        NOW - 100
    ))
}

/// When the deploy bot asks for an approval.
pub const APPROVAL: u64 = NOW - 50;

/// A deploy waiting for approval, with interactive buttons: Approve asks
/// first, as the app wants. Once `answer` is given, the app has replaced
/// the buttons with who answered, as such apps do.
fn deploy_approval(answer: Option<&str>) -> Message {
    let last = match answer {
        None => r##"{"type":"actions","block_id":"deploy-1288","elements":[
            {"type":"button","action_id":"approve","value":"1288","style":"primary",
             "text":{"type":"plain_text","text":"Approve"},
             "confirm":{"title":{"type":"plain_text","text":"Deploy to production?"},
               "text":{"type":"mrkdwn","text":"Release *2026.10.1* goes to every customer at once."},
               "confirm":{"type":"plain_text","text":"Deploy"},
               "deny":{"type":"plain_text","text":"Not yet"}}},
            {"type":"button","action_id":"reject","value":"1288","style":"danger",
             "text":{"type":"plain_text","text":"Reject"}}]}"##
            .to_owned(),
        Some(answer) => format!(
            r##"{{"type":"context","elements":[{{"type":"mrkdwn","text":":white_check_mark: {answer} by you"}}]}}"##
        ),
    };
    from_json(&format!(
        r##"{{"type":"message","subtype":"bot_message","ts":"{APPROVAL}.000100","bot_id":"B09","username":"Deploy Bot",
        "text":"Deploy 2026.10.1 to production?",
        "blocks":[
          {{"type":"section","block_id":"ask","text":{{"type":"mrkdwn","text":"*Deploy 2026.10.1 to production?*\nRequested by <@U02> · build #1288 passed"}}}},
          {last}
        ]}}"##
    ))
}

/// When the rollout bot asks where a release goes.
pub const ROLLOUT: u64 = NOW - 30;

/// A rollout with an app's menus: a select of release channels in groups,
/// an overflow menu (one choice a link, one asking first) and a select
/// that already has a choice. Once something is `chosen` (its value and
/// label), the app shows it as made, with a `note` of what it did, as
/// such apps answer.
fn rollout(chosen: Option<(&str, &str)>, note: Option<&str>) -> Message {
    let initial = chosen
        .map(|(value, text)| {
            format!(
                r##","initial_option":{{"text":{{"type":"plain_text","text":"{text}"}},"value":"{value}"}}"##
            )
        })
        .unwrap_or_default();
    let note = note
        .map(|note| {
            format!(r##",{{"type":"context","elements":[{{"type":"mrkdwn","text":"{note}"}}]}}"##)
        })
        .unwrap_or_default();
    from_json(&format!(
        r##"{{"type":"message","subtype":"bot_message","ts":"{ROLLOUT}.000100","bot_id":"B10","username":"Rollout Bot",
        "text":"Where should 2026.10.1 go first?",
        "blocks":[
          {{"type":"section","block_id":"rollout","text":{{"type":"mrkdwn","text":"*Where should 2026.10.1 go first?*\nThe rollout starts as soon as you pick."}},
            "accessory":{{"type":"static_select","action_id":"channel",
              "placeholder":{{"type":"plain_text","text":"Pick a channel"}}{initial},
              "option_groups":[
                {{"label":{{"type":"plain_text","text":"Customers"}},"options":[
                  {{"text":{{"type":"plain_text","text":"Stable"}},"value":"stable"}},
                  {{"text":{{"type":"plain_text","text":"Beta"}},"value":"beta",
                    "description":{{"type":"plain_text","text":"About 2,000 workspaces"}}}}]}},
                {{"label":{{"type":"plain_text","text":"Internal"}},"options":[
                  {{"text":{{"type":"plain_text","text":"Canary"}},"value":"canary"}},
                  {{"text":{{"type":"plain_text","text":"Staff only"}},"value":"staff"}}]}}]}}}},
          {{"type":"actions","block_id":"rollout-more","elements":[
            {{"type":"static_select","action_id":"notify",
              "initial_option":{{"text":{{"type":"plain_text","text":"Notify on failure"}},"value":"failure"}},
              "options":[
                {{"text":{{"type":"plain_text","text":"Notify everyone"}},"value":"everyone"}},
                {{"text":{{"type":"plain_text","text":"Notify on failure"}},"value":"failure"}},
                {{"text":{{"type":"plain_text","text":"Notify nobody"}},"value":"nobody"}}]}},
            {{"type":"overflow","action_id":"more","options":[
              {{"text":{{"type":"plain_text","text":"View the rollout plan"}},"value":"plan","url":"https://example.com/rollout"}},
              {{"text":{{"type":"plain_text","text":"Pause the rollout"}},"value":"pause"}},
              {{"text":{{"type":"plain_text","text":"Roll back"}},"value":"rollback"}}],
              "confirm":{{"title":{{"type":"plain_text","text":"Are you sure?"}},
                "text":{{"type":"mrkdwn","text":"The rollout bot acts on this at once."}},
                "confirm":{{"type":"plain_text","text":"Go ahead"}},
                "deny":{{"type":"plain_text","text":"Cancel"}}}}}},
            {{"type":"datepicker","action_id":"when","placeholder":{{"type":"plain_text","text":"Schedule it"}}}}]}}{note}
        ]}}"##
    ))
}

/// What the rollout bot does with a choice from its menus: the message it
/// changes to, if any.
fn rollout_answer(press: &crate::model::Press) -> Option<Message> {
    let value = press.value.as_deref().unwrap_or_default();
    match press.action_id.as_str() {
        "channel" => Some(rollout(
            Some((value, press.text.as_str())),
            Some(&format!(
                ":rocket: Rolling out to *{}*, picked by you",
                press.text
            )),
        )),
        "notify" => Some(rollout(
            None,
            Some(&format!(":bell: {}, set by you", press.text)),
        )),
        "more" => match value {
            "pause" => Some(rollout(None, Some(":pause_button: Rollout paused by you"))),
            "rollback" => Some(rollout(None, Some(":rewind: Rolled back by you"))),
            _ => None,
        },
        _ => None,
    }
}

/// A chart an app posted as a Block Kit image whose size Slack did not
/// give, as older messages have it.
fn block_kit_chart() -> Message {
    from_json(&format!(
        r##"{{"type":"message","subtype":"bot_message","ts":"{}.000100","bot_id":"B08","username":"Release Bot",
        "text":"Error rate after the release",
        "blocks":[
          {{"type":"image","image_url":"slow://error-rate.png","alt_text":"error rate",
            "title":{{"type":"plain_text","text":"Error rate after the release"}}}}
        ]}}"##,
        NOW - 40
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
                    thumb: Some("slow://sidebar-v2.png".into()),
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
                    thumb: Some("slow://sidebar-v2-light.png".into()),
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
            deploy_approval(None),
            rollout(None, None),
            block_kit_chart(),
        ],
        "D01" => vec![
            message(NOW - 1000, ME, "Can you look at the new reaction picker?"),
            message(
                NOW - 200,
                "U01",
                "Sure, sending notes in a bit :slightly_smiling_face:",
            ),
            // A permalink to an old message in #general, which opens here.
            // Posted without an unfurl, so the quote under it is fetched.
            message(
                NOW - 150,
                "U01",
                &format!(
                    "Same question came up before: <https://acme-inc.slack.com/archives/C01/p{}000100>",
                    NOW - 90 * (LONG - 20) as u64
                ),
            ),
            message_unfurl(),
            // A link to a message deleted since.
            message(
                NOW - 100,
                "U01",
                &format!(
                    "There was a third one, but it's gone: <https://acme-inc.slack.com/archives/C02/p{}000100>",
                    NOW - 2200
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
                    thumb: Some("slow://talk-thumb.jpg".into()),
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
                    image: Some("slow://density.png".into()),
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
                        poster: Some("slow://walkthrough.jpg".into()),
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
                    poster: Some("slow://style-guide.png".into()),
                    poster_size: Some([480.0, 270.0]),
                    ..File::default()
                }],
                ..message(NOW - 9000, "U03", "The style guide, updated.")
            },
        ],
        "C04" => shared_files(),
        _ => vec![message(
            NOW - 9000,
            "U02",
            "Nothing much happening here yet.",
        )],
    }
}

/// What every demo sound plays: 14 seconds of soft beeps (as long as the
/// voice clip), a WAV made in memory rather than a file in the repository.
fn demo_sound() -> crate::audio::Bytes {
    const RATE: u32 = 16_000;
    crate::audio::wav(RATE, &crate::audio::beeps(RATE, 14.0)).into()
}

/// The voice clip in #random, for the screenshot that plays it.
pub fn voice_clip() -> Option<File> {
    shared_files()
        .into_iter()
        .flat_map(|message| message.files)
        .find(|file| file.voice)
}

/// #random's files, one of each kind Slack previews: a code snippet, a
/// text file, a PDF, a spreadsheet with Slack's PDF of it, a voice clip
/// and a video.
fn shared_files() -> Vec<Message> {
    let url = |id: &str, name: &str| {
        Some(format!(
            "https://files.slack.com/files-pri/TDEMO-{id}/{name}"
        ))
    };
    let tmb = |id: &str, name: &str| {
        Some(format!(
            "https://files.slack.com/files-tmb/TDEMO-{id}-a1b2c3/{name}"
        ))
    };
    let file = |id: &str, name: &str, mimetype: &str, size: u64| File {
        id: id.into(),
        name: name.into(),
        title: name.into(),
        mimetype: mimetype.into(),
        size,
        url_private: url(id, name),
        download_url: url(id, &format!("download/{name}")),
        ..File::default()
    };
    let with = |files: Vec<File>, seconds: u64, user: &str, text: &str| Message {
        files,
        ..message(NOW - seconds, user, text)
    };
    vec![
        with(
            vec![File {
                filetype: "rust".into(),
                preview: Some(crate::model::TextPreview {
                    text: "/// Waits longer after each failed attempt, up to a minute.\n\
                           fn backoff(attempt: u32) -> Duration {\n\
                           \x20   let base = Duration::from_millis(500);\n\
                           \x20   // Doubling, capped so it never waits for hours.\n\
                           \x20   let factor = 2u32.saturating_pow(attempt.min(7));\n\
                           \x20   (base * factor).min(Duration::from_secs(60))\n\
                           }\n\
                           \n\
                           #[test]\n\
                           fn backoff_is_capped() {"
                        .into(),
                    lines_more: Some(16),
                    lines: Some(26),
                    truncated: false,
                }),
                ..file("F20", "backoff.rs", "text/plain", 742)
            }],
            7200,
            "U02",
            "The reconnect backoff, if anyone wants to check my maths:",
        ),
        with(
            vec![File {
                filetype: "text".into(),
                preview: Some(crate::model::TextPreview {
                    text: "2026-09-30 11:02:14 INFO  socket: connected (wss-primary)\n\
                           2026-09-30 11:47:51 WARN  socket: no pong in 30s, reconnecting\n\
                           2026-09-30 11:47:52 INFO  socket: connected (wss-backup)"
                        .into(),
                    lines_more: Some(0),
                    lines: Some(3),
                    truncated: false,
                }),
                ..file("F21", "reconnect.log", "text/plain", 196)
            }],
            6800,
            "U04",
            "And the log from last night.",
        ),
        with(
            vec![File {
                filetype: "pdf".into(),
                poster: Some("slow://release-notes.png".into()),
                poster_size: Some([909.0, 1286.0]),
                ..file("F22", "release-notes.pdf", "application/pdf", 312_400)
            }],
            6000,
            "U03",
            "Release notes for Friday.",
        ),
        with(
            vec![File {
                filetype: "xlsx".into(),
                poster: Some("slow://budget.png".into()),
                poster_size: Some([1210.0, 935.0]),
                converted_pdf: tmb("F23", "budget_converted.pdf"),
                ..file(
                    "F23",
                    "Q4 budget.xlsx",
                    "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
                    75_813,
                )
            }],
            5000,
            "U01",
            "Budget for next quarter, with the new build machines.",
        ),
        with(
            vec![file("F26", "deploys.csv", "text/csv", 7_412)],
            4_600,
            "U02",
            "Every deploy since July, for the retro.",
        ),
        with(
            vec![File {
                filetype: "zip".into(),
                ..file("F27", "logs.zip", "application/zip", 98_220)
            }],
            4_200,
            "U04",
            "The logs from that night, zipped.",
        ),
        with(
            vec![File {
                filetype: "m4a".into(),
                voice: true,
                duration_ms: Some(13_977),
                wave: vec![
                    0, 0, 2, 34, 75, 57, 53, 45, 46, 48, 66, 89, 78, 54, 68, 68, 61, 48, 51, 47,
                    47, 45, 69, 72, 47, 40, 46, 41, 37, 34, 35, 35, 36, 36, 28, 36, 39, 40, 39, 36,
                    41, 40, 42, 33, 51, 46, 39, 32, 39, 34, 37, 32, 37, 36, 37, 32, 34, 39, 27,
                    41, 43, 48, 68, 72, 56, 66, 52, 53, 53, 43, 39, 42, 41, 46, 49, 34, 37, 39,
                    20, 25, 40, 37, 34, 76, 100, 53, 89, 94, 34, 48, 26, 25, 59, 29, 74, 71, 68,
                    23, 54, 58,
                ],
                transcript: Some(
                    "Quick one: the build machines arrive Tuesday, so let's move the freeze to Wednesday."
                        .into(),
                ),
                ..file(
                    "F24",
                    "Audio clip (2026-09-30_13-50-02).m4a",
                    "audio/mp4",
                    171_020,
                )
            }],
            900,
            "U01",
            "",
        ),
        with(
            vec![File {
                filetype: "mp4".into(),
                poster: Some("slow://onboarding.jpg".into()),
                poster_size: Some([1920.0, 1080.0]),
                duration_ms: Some(279_145),
                mp4_low: tmb("F25", "onboarding_trans.mp4"),
                ..file("F25", "onboarding.mp4", "video/mp4", 30_784_549)
            }],
            500,
            "U03",
            "Onboarding walkthrough for the new folks.",
        ),
    ]
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

/// The demo huddle's pretend screen share: where its pictures go, and
/// whether it is being watched (then the fixture plays into it).
#[cfg(feature = "huddle-video")]
static SHARE: std::sync::OnceLock<(
    crate::huddles::Screen,
    std::sync::Arc<std::sync::atomic::AtomicBool>,
)> = std::sync::OnceLock::new();

/// The demo huddle's pretend cameras: where their pictures go, and which
/// are playing (those with a tile, not paused).
#[cfg(feature = "huddle-video")]
type DemoCameras = (
    crate::huddles::Gallery,
    std::sync::Arc<std::sync::Mutex<Vec<String>>>,
);
#[cfg(feature = "huddle-video")]
static CAMERAS: std::sync::OnceLock<DemoCameras> = std::sync::OnceLock::new();

/// Sets up the pretend share and cameras, their pictures waking the
/// window through `sink`'s waker. Only the first call counts.
#[cfg(feature = "huddle-video")]
fn start_share(sink: &Sink) {
    let waker = sink.waker();
    let screen = crate::huddles::Screen::new(move || waker.wake());
    let watched = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    if let Err(error) = crate::huddle_audio::screen::demo_feed(screen.clone(), watched.clone()) {
        log::warn!("demo: no pretend share: {error}");
        return;
    }
    let _ = SHARE.set((screen, watched));
    let waker = sink.waker();
    let gallery = crate::huddles::Gallery::new(move || waker.wake());
    // One fixture, five people: each tinted their own way.
    for (feed, tint) in camera_feeds().iter().zip(TINTS) {
        gallery.tint(&feed.key, tint);
    }
    let playing = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    if let Err(error) = crate::huddle_audio::gallery::demo_feed(gallery.clone(), playing.clone()) {
        log::warn!("demo: no pretend cameras: {error}");
        return;
    }
    let _ = CAMERAS.set((gallery, playing));
}

/// How the pretend cameras are tinted, to tell them apart: added to the
/// blue and red differences.
#[cfg(feature = "huddle-video")]
const TINTS: [[i16; 2]; 5] = [[0, 0], [50, -30], [-45, 40], [0, 0], [-30, -50]];

/// The pretend cameras' gallery, once the demo has started.
#[cfg(feature = "huddle-video")]
pub fn camera_gallery() -> Option<crate::huddles::Gallery> {
    CAMERAS.get().map(|(gallery, _)| gallery.clone())
}

/// Who has a camera on in #design's huddle: Ana, Bob, Carla, Dev (his
/// paused) and Lee, as INDEX would list them.
#[cfg(feature = "huddle-video")]
pub fn camera_feeds() -> Vec<crate::huddle_audio::cameras::Feed> {
    [
        ("ana-attendee", "U01", false),
        ("bob-attendee", "U02", false),
        ("carla-attendee", "U03", false),
        ("dev-attendee", "U04", true),
        ("lee-attendee", "U06", false),
    ]
    .into_iter()
    .enumerate()
    .map(
        |(n, (key, user, paused))| crate::huddle_audio::cameras::Feed {
            key: key.to_owned(),
            user: Some(user.to_owned()),
            layers: vec![crate::huddle_audio::cameras::Layer {
                stream_id: 100 + u32::try_from(n).unwrap_or(0),
                width: 480,
                height: 480,
                max_kbps: 500,
            }],
            paused,
        },
    )
    .collect()
}

/// The pretend cameras as the call window `wish` would get them, Ana
/// speaking; those with a tile play.
#[cfg(feature = "huddle-video")]
pub fn watch_call(wish: &crate::huddles::Wish) -> Vec<crate::huddles::Camera> {
    use crate::huddle_audio::cameras;
    let now = std::time::Instant::now();
    let feeds = camera_feeds();
    let tiles = if wish.open {
        let spoke = HashMap::from([("ana-attendee".to_owned(), now)]);
        cameras::pick(&feeds, &[], &spoke, wish.tiles, now)
    } else {
        Vec::new()
    };
    if let Some((_, playing)) = CAMERAS.get() {
        let on: Vec<String> = tiles
            .iter()
            .filter(|key| feeds.iter().any(|f| f.key == **key && !f.paused))
            .cloned()
            .collect();
        *playing
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = on;
    }
    if let Some((_, watched)) = SHARE.get() {
        watched.store(
            wish.open && wish.share.is_some(),
            std::sync::atomic::Ordering::Relaxed,
        );
    }
    cameras::cameras(&feeds, &tiles)
}

/// The pretend share's screen, once the demo has started.
#[cfg(feature = "huddle-video")]
pub fn share_screen() -> Option<crate::huddles::Screen> {
    SHARE.get().map(|(screen, _)| screen.clone())
}

/// The demo's camera: the test picture, never a real one, feeding the
/// self-preview while it is on.
#[cfg(feature = "huddle-camera")]
struct DemoCamera {
    preview: crate::huddle_camera::Preview,
    latest: crate::huddle_audio::camera::Latest,
    running: std::sync::Mutex<
        Option<(
            crate::huddle_audio::camera::Capturing,
            crate::huddle_audio::microphone::Running,
        )>,
    >,
}

#[cfg(feature = "huddle-camera")]
static CAMERA: std::sync::OnceLock<DemoCamera> = std::sync::OnceLock::new();

/// Sets up the demo's camera, its preview waking the window through
/// `sink`'s waker. Only the first call counts.
#[cfg(feature = "huddle-camera")]
fn start_camera(sink: &Sink) {
    let waker = sink.waker();
    let _ = CAMERA.set(DemoCamera {
        preview: crate::huddle_camera::Preview::new(move || waker.wake()),
        latest: crate::huddle_audio::camera::Latest::default(),
        running: std::sync::Mutex::new(None),
    });
}

/// The demo camera's self-preview, once the demo has started.
#[cfg(feature = "huddle-camera")]
pub fn camera_preview() -> Option<crate::huddle_camera::Preview> {
    CAMERA.get().map(|c| c.preview.clone())
}

/// Turns the demo's camera (the test picture) on or off.
#[cfg(feature = "huddle-camera")]
pub fn camera(on: bool) {
    use crate::huddle_audio::camera::{Camera as _, TestPattern};
    let Some(camera) = CAMERA.get() else {
        return;
    };
    let mut running = camera
        .running
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if !on {
        *running = None;
        return;
    }
    if running.is_some() {
        return;
    }
    let pattern = TestPattern::new(camera.latest.clone()).open();
    let feed = crate::huddle_audio::camera_send::preview_feed(
        camera.latest.clone(),
        camera.preview.clone(),
    );
    match (pattern, feed) {
        (Ok(pattern), Ok(feed)) => *running = Some((pattern, feed)),
        (Err(error), _) => log::warn!("demo: no camera: {error}"),
        (_, Err(error)) => log::warn!("demo: no camera: {error}"),
    }
}

/// Listening to #design's huddle, talking, with your camera on: the call
/// bar's self-preview (`--demo-view camera`).
#[cfg(feature = "huddle-camera")]
pub fn camera_on() -> crate::huddles::Listening {
    crate::huddles::Listening {
        mic: crate::huddle_mic::Mic::Live,
        camera: crate::huddle_camera::Cam::On,
        preview: camera_preview(),
        ..listening()
    }
}

/// Who shares in #design's huddle: Ana, and Carla too, so the call
/// window has two tabs.
#[cfg(feature = "huddle-video")]
pub fn shares() -> Vec<crate::huddles::Share> {
    vec![
        crate::huddles::Share {
            key: "ana-attendee#content".into(),
            user: Some("U01".into()),
        },
        crate::huddles::Share {
            key: "carla-attendee#content".into(),
            user: Some("U03".into()),
        },
    ]
}

/// Listening to #design's huddle where Ana and Carla share their
/// screens and five people have a camera on, Dev's paused
/// (`--demo-view sharing`, `call-window` and `cameras`).
#[cfg(feature = "huddle-video")]
pub fn sharing() -> crate::huddles::Listening {
    use crate::huddles::Person;
    let mut listening = listening();
    let person = |user: &str, muted| Person {
        user: Some(user.to_owned()),
        me: false,
        muted,
        speaking: false,
    };
    // Bob, Dev and Lee are in it too, Lee muted.
    let me = listening.roster.people.pop();
    listening.roster.people.extend([
        person("U02", false),
        person("U04", false),
        person("U06", true),
    ]);
    listening.roster.people.extend(me);
    listening.roster.count = Some(6);
    crate::huddles::Listening {
        shares: shares(),
        screen: share_screen(),
        cameras: watch_call(&crate::huddles::Wish::closed()),
        gallery: camera_gallery(),
        ..listening
    }
}

/// Listening to the huddle in #design for 2 min 14 s: Ana speaking,
/// Carla muted, and you, for the call bar's screenshot
/// (`--demo-view listening`).
pub fn listening() -> crate::huddles::Listening {
    use crate::huddles::{Person, Phase, Roster};
    let person = |user: &str, me, muted, speaking| Person {
        user: Some(user.to_owned()),
        me,
        muted,
        speaking,
    };
    let since = std::time::Instant::now()
        .checked_sub(std::time::Duration::from_secs(134))
        .unwrap_or_else(std::time::Instant::now);
    crate::huddles::Listening {
        phase: Phase::Live { since },
        roster: Roster {
            people: vec![
                person("U01", false, false, true),
                person("U03", false, true, false),
                person(ME, true, true, false),
            ],
            count: Some(3),
        },
        ..crate::huddles::Listening::new(TEAM, "C03")
    }
}

fn thread() -> Vec<Message> {
    let mut parent = history("C02")
        .into_iter()
        .find(|m| m.ts == ts(THREAD))
        .unwrap_or_else(|| message(THREAD, "U03", ""));
    parent.thread_ts = Some(ts(THREAD));
    // You follow it, as Slack tells a browser session.
    parent.subscribed = Some(true);
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

/// The pretend workspace's custom emoji.
fn demo_emoji() -> HashMap<String, String> {
    HashMap::from([
        ("partyparrot".to_owned(), PARROT.to_owned()),
        ("parrot".to_owned(), "alias:partyparrot".to_owned()),
    ])
}

pub async fn run(sink: Sink, mut commands: mpsc::UnboundedReceiver<Command>) {
    sink.send(Event::AppLoaded(Some(AppCredentials {
        client_id: "1234567890.0987654321".into(),
        client_secret: "demo".into(),
        app_token: "xapp-demo".into(),
    })));
    // Acme is signed in as a browser session, so its app buttons press;
    // Open Source by OAuth, where they only work in Slack, through an app
    // made from the first manifest, so Settings shows how to update it.
    for (id, name, sign_in) in [
        (TEAM, "Acme Inc", SignInKind::Session),
        ("TDEMO2", "Open Source", SignInKind::App),
    ] {
        sink.send(Event::WorkspaceReady(Workspace {
            team_id: id.into(),
            name: name.into(),
            domain: name.to_lowercase().replace(' ', "-"),
            icon: None,
            user_id: ME.into(),
            sign_in,
            scopes: (sign_in == SignInKind::App).then(|| {
                crate::scopes::Scopes::parse(&crate::scopes::Request::Older.scopes().join(","))
            }),
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
            emoji: demo_emoji(),
            can_add: true,
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
    #[cfg(feature = "huddle-video")]
    start_share(&sink);
    #[cfg(feature = "huddle-camera")]
    start_camera(&sink);
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
    // Bob rings you into a huddle in #general the first time you open
    // #design (the huddle view), and not over every other view.
    let mut rang = false;
    while let Some(command) = commands.recv().await {
        match command {
            Command::Focus {
                channel: Some(channel),
                ..
            } if channel == "C03" && !rang => {
                rang = true;
                let ringing = sink.clone();
                tokio::spawn(async move {
                    tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                    ringing.send(crate::backend::huddles::demo_invite(TEAM));
                });
            }
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
                client_msg_id,
                ..
            } => {
                sent += 1;
                let mut message = echo(NOW + sent, &text, client_msg_id);
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
            // The deploy and rollout bots take a moment, then answer the
            // press or choice as such apps do: by changing their message.
            Command::PressButton { team, press } => {
                let sink = sink.clone();
                tokio::spawn(async move {
                    tokio::time::sleep(LATENCY * 3).await;
                    let answer = match press.action_id.as_str() {
                        "approve" => Some(deploy_approval(Some("Approved"))),
                        "reject" => Some(deploy_approval(Some("Rejected"))),
                        _ => rollout_answer(&press),
                    };
                    let channel = press.channel.clone();
                    sink.send(Event::Pressed {
                        team: team.clone(),
                        press,
                        result: Ok(()),
                    });
                    if let Some(message) = answer {
                        sink.send(Event::Message {
                            team,
                            channel,
                            message,
                            changed: true,
                        });
                    }
                });
            }
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
            // Taken after a moment, as Slack would; the list fetched after
            // it does not have it yet, as Slack's may not.
            Command::AddEmoji { team, name, .. } => {
                let sink = sink.clone();
                tokio::spawn(async move {
                    tokio::time::sleep(std::time::Duration::from_millis(600)).await;
                    sink.send(Event::EmojiAdded {
                        team,
                        name,
                        result: Ok(()),
                    });
                });
            }
            Command::FetchEmoji { team } => sink.send(Event::Emoji {
                team,
                emoji: demo_emoji(),
                can_add: true,
            }),
            // Slack takes it, then says so to every client.
            Command::DeleteFile { team, file, name } => {
                sink.send(Event::FileDeleteSettled {
                    team: team.clone(),
                    file: file.clone(),
                    name,
                    result: Ok(()),
                });
                sink.send(Event::FileGone { team, file });
            }
            Command::OpenFile { name, .. } => {
                sink.send(Event::Notice(Notice::DemoOpen { name }));
            }
            // Every sound is the same few seconds of beeps, made here.
            Command::FetchAudio { id, .. } => sink.send(Event::AudioFetched {
                id,
                result: Ok(demo_sound()),
            }),
            // Read for real, from the demo's own bytes, a moment late so the
            // viewer's loading state shows.
            Command::ViewFile { id, url, kind, .. } => {
                let sink = sink.clone();
                tokio::spawn(async move {
                    tokio::time::sleep(LATENCY).await;
                    let result = match files::contents(&url) {
                        Some(bytes) => tokio::task::spawn_blocking(move || {
                            crate::viewer::read(kind, &bytes, false)
                        })
                        .await
                        .unwrap_or_else(|e| {
                            Err(crate::failure::Failure::Unreadable(e.to_string()))
                        }),
                        None => Err(crate::failure::Failure::Http(404)),
                    };
                    sink.send(Event::FileView { id, result });
                });
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
            // A moment late, as from the network, so the quote grows its
            // row after the list is laid out.
            Command::FetchQuote {
                team, channel, ts, ..
            } => {
                let sink = sink.clone();
                tokio::spawn(async move {
                    tokio::time::sleep(LATENCY).await;
                    let result = Ok(quoted(&channel, &ts));
                    sink.send(Event::Quoted {
                        team,
                        channel,
                        ts,
                        result,
                    });
                });
            }
            _ => {}
        }
    }
}
