//! What the pretend Slack answers for the views at the top of the sidebar.

use super::{ME, NOW, history, message, thread, ts};
use crate::backend::Event;
use crate::model::Message;
use crate::views::{self, Activity, Command, Reason};

/// The demo's answers to one command.
pub fn answer(team: &str, command: Command) -> Vec<Event> {
    let event = match command {
        Command::Activity { .. } => views::Event::Activity {
            result: Ok(activity()),
            searched: false,
        },
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
