//! What the pretend Slack answers for the views at the top of the sidebar.

use super::{ME, NOW, history, message, thread, ts};
use crate::backend::Event;
use crate::model::Message;
use crate::views::{self, Activity, Command, Followed, Reason};

/// The demo's answers to one command.
pub fn answer(team: &str, command: Command) -> Vec<Event> {
    let event = match command {
        Command::Activity { .. } => views::Event::Activity {
            result: Ok(activity()),
            searched: false,
        },
        Command::Unread { channel, after } => views::Event::Unread {
            result: Ok((
                history(&channel)
                    .into_iter()
                    .filter(|m| m.in_channel() && after.as_ref().is_none_or(|a| m.ts > *a))
                    .collect(),
                false,
            )),
            channel,
        },
        Command::Threads { .. } => views::Event::Threads {
            result: Ok(threads()),
            searched: false,
        },
        Command::ReadThread { .. } => views::Event::Nothing,
    };
    vec![Event::Views {
        team: team.to_owned(),
        event,
    }]
}

/// A message of the demo's history, by when it was sent.
fn find(channel: &str, seconds: u64) -> Message {
    history(channel)
        .into_iter()
        .chain(thread())
        .find(|m| m.ts == ts(seconds))
        .unwrap_or_else(|| message(seconds, "U01", ""))
}

fn activity() -> Vec<Activity> {
    vec![
        Activity {
            reason: Reason::Mention,
            channel: "C02".into(),
            message: find("C02", NOW - 300),
            unread: true,
        },
        Activity {
            reason: Reason::Reply,
            channel: "C02".into(),
            message: find("C02", NOW - 1200),
            unread: true,
        },
        Activity {
            reason: Reason::Mention,
            channel: "C03".into(),
            message: message(
                NOW - 5400,
                "U01",
                "<@U00> could you check the contrast of the new badge in dark mode?",
            ),
            unread: false,
        },
        Activity {
            reason: Reason::Everyone,
            channel: "C01".into(),
            message: message(
                NOW - 86_400,
                "U03",
                "<!channel> the all-hands moves to Thursday this week.",
            ),
            unread: false,
        },
    ]
    .into_iter()
    .filter(|a| a.message.user.as_deref() != Some(ME))
    .collect()
}

/// The threads the demo's you follow: #engineering's release thread, with
/// replies you have not read, and a quiet one in #design.
fn threads() -> Vec<Followed> {
    let mut release = thread();
    let parent = release.remove(0);
    let reply = |seconds, user, text| Message {
        thread_ts: Some(ts(NOW - 9500)),
        ..message(seconds, user, text)
    };
    let design_parent = Message {
        thread_ts: Some(ts(NOW - 9500)),
        reply_count: 2,
        replies_known: true,
        ..message(NOW - 9500, ME, "Which icon set should the new views use?")
    };
    vec![
        Followed {
            channel: "C02".into(),
            parent,
            replies: release,
            unread: 2,
        },
        Followed {
            channel: "C03".into(),
            parent: design_parent,
            replies: vec![
                reply(NOW - 9400, "U01", "Lucide, like the rest of the app."),
                reply(NOW - 9300, ME, "Lucide it is :+1:"),
            ],
            unread: 0,
        },
    ]
}
