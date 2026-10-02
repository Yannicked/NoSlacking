//! The worker's side of [`crate::people`]: keeping the presence of the
//! people on screen up to date.
//!
//! A browser session's RTM socket subscribes to them (`presence_sub`) and
//! then hears every change as it happens. Socket Mode carries no presence
//! at all, so without RTM each person is asked for with
//! `users.getPresence` now and then: one person per call, so the calls go
//! out slowly, one workspace at a time, and only for people not asked
//! about lately.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use serde::Deserialize;
use serde_json::{Value, json};
use tokio::sync::mpsc::UnboundedSender;

use super::{Event, Sink};
use crate::people::{self, Command, Presence};
use crate::slack::{Client, SlackError};

/// How long a polled presence counts as fresh. Polling is the fallback
/// for when nothing tells us of changes, so it lags by up to this much.
const FRESH_FOR: Duration = Duration::from_secs(4 * 60);
/// The most people asked about in one round, and the pause between two
/// of them. `users.getPresence` allows about 50 calls a minute; this stays
/// well under it.
const ROUND: usize = 15;
const BETWEEN_CALLS: Duration = Duration::from_millis(1500);

/// `users.getPresence`'s answer.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct PresenceAnswer {
    presence: String,
}

/// Presence for one workspace.
#[derive(Debug, Default)]
struct Watch {
    /// The people on screen, sorted.
    users: Vec<String>,
    /// When each person was last asked about by polling.
    polled: HashMap<String, Instant>,
    /// The RTM socket's outgoing side, for a browser session.
    rtm: Option<UnboundedSender<Value>>,
    /// Whether that socket is connected, so subscriptions reach Slack and
    /// changes come by themselves.
    live: bool,
    /// The round of polling still running, if any.
    polling: Option<tokio::task::JoinHandle<()>>,
}

impl Watch {
    /// Subscribes the socket to everyone watched, and asks for where they
    /// stand now. A new subscription replaces the old one, so it always
    /// names everyone.
    fn subscribe(&self, added: &[String]) {
        let Some(rtm) = self.rtm.as_ref().filter(|_| self.live) else {
            return;
        };
        let _ = rtm.send(json!({"type": "presence_sub", "ids": self.users}));
        if !added.is_empty() {
            let _ = rtm.send(json!({"type": "presence_query", "ids": added}));
        }
    }
}

/// Presence for every workspace.
#[derive(Debug, Default)]
pub struct Hub {
    teams: HashMap<String, Watch>,
}

impl Hub {
    /// A browser session's RTM socket was started, with this to send on.
    pub fn rtm_started(&mut self, team: &str, sender: UnboundedSender<Value>) {
        let watch = self.teams.entry(team.to_owned()).or_default();
        watch.rtm = Some(sender);
        watch.live = false;
    }

    /// The RTM socket connected or dropped. On connect it is subscribed
    /// afresh: Slack forgets subscriptions with the connection.
    pub fn rtm_live(&mut self, team: &str, live: bool) {
        let Some(watch) = self.teams.get_mut(team) else {
            return;
        };
        watch.live = live && watch.rtm.is_some();
        if watch.live {
            let everyone = watch.users.clone();
            watch.subscribe(&everyone);
        }
    }

    /// Slack will not give this workspace an RTM socket: poll instead.
    pub fn rtm_gone(&mut self, team: &str) {
        if let Some(watch) = self.teams.get_mut(team) {
            watch.rtm = None;
            watch.live = false;
        }
    }

    /// Forgets a workspace that signed out.
    pub fn forget(&mut self, team: &str) {
        if let Some(watch) = self.teams.remove(team)
            && let Some(task) = watch.polling
        {
            task.abort();
        }
    }

    /// Runs one command for `team`.
    pub fn command(&mut self, team: &str, command: Command) {
        match command {
            Command::Watch { mut users } => {
                users.sort();
                users.dedup();
                let watch = self.teams.entry(team.to_owned()).or_default();
                let added: Vec<String> = users
                    .iter()
                    .filter(|u| watch.users.binary_search(u).is_err())
                    .cloned()
                    .collect();
                watch.users = users;
                watch.subscribe(&added);
            }
        }
    }

    /// Starts a round of polling for each workspace that has no live
    /// socket and people not asked about lately. `team` gives a
    /// workspace's client and sink, while it is signed in.
    pub fn poll(&mut self, now: Instant, team: impl Fn(&str) -> Option<(Client, Sink)>) {
        for (id, watch) in &mut self.teams {
            if watch.live || watch.polling.as_ref().is_some_and(|t| !t.is_finished()) {
                continue;
            }
            let due = due(&watch.users, &watch.polled, now);
            if due.is_empty() {
                continue;
            }
            let Some((client, sink)) = team(id) else {
                continue;
            };
            for user in &due {
                watch.polled.insert(user.clone(), now);
            }
            watch
                .polled
                .retain(|user, _| watch.users.binary_search(user).is_ok());
            watch.polling = Some(tokio::spawn(poll(client, id.clone(), due, sink)));
        }
    }
}

/// The watched people to ask about now: those never asked, or not for
/// [`FRESH_FOR`], at most [`ROUND`] of them, the longest unasked first.
fn due(users: &[String], polled: &HashMap<String, Instant>, now: Instant) -> Vec<String> {
    let mut due: Vec<(Option<Instant>, &String)> = users
        .iter()
        .map(|u| (polled.get(u).copied(), u))
        .filter(|(at, _)| at.is_none_or(|at| now.duration_since(at) >= FRESH_FOR))
        .collect();
    // Never asked (`None`) sorts first.
    due.sort();
    due.into_iter()
        .take(ROUND)
        .map(|(_, u)| u.clone())
        .collect()
}

/// Asks for each person's presence in turn, reporting each answer.
async fn poll(client: Client, team: String, users: Vec<String>, sink: Sink) {
    for (n, user) in users.into_iter().enumerate() {
        if n > 0 {
            tokio::time::sleep(BETWEEN_CALLS).await;
        }
        let answer: Result<PresenceAnswer, SlackError> = client
            .call("users.getPresence", &[("user", user.clone())])
            .await;
        match answer {
            Ok(answer) => {
                if let Some(presence) = Presence::parse(&answer.presence) {
                    sink.send(Event::People {
                        team: team.clone(),
                        event: people::Event::Presence {
                            users: vec![(user, presence)],
                        },
                    });
                }
            }
            // Slack is busy: the rest wait for the next round.
            Err(SlackError::RateLimited) => return,
            Err(error) => {
                log::debug!("users.getPresence for {user}: {error}");
                // A refusal of the sign-in or of the method will not change
                // for the next person; an unknown person is only this one.
                let refused = matches!(&error, SlackError::Api(code)
                    if code == "missing_scope" || code == "not_allowed_token_type");
                if error.is_auth() || refused {
                    return;
                }
            }
        }
    }
}

/// Reads a real-time event about people, if it is one.
pub fn translate(event: &Value) -> Option<people::Event> {
    let kind = event.get("type").and_then(Value::as_str)?;
    match kind {
        // One person (`user`), or several at once (`users`) on a socket
        // opened with `batch_presence_aware`.
        "presence_change" => {
            let presence = Presence::parse(event.get("presence")?.as_str()?)?;
            let mut users: Vec<String> = event
                .get("users")
                .and_then(Value::as_array)
                .map(|users| {
                    users
                        .iter()
                        .filter_map(Value::as_str)
                        .map(str::to_owned)
                        .collect()
                })
                .unwrap_or_default();
            if let Some(user) = event.get("user").and_then(Value::as_str) {
                users.push(user.to_owned());
            }
            (!users.is_empty()).then(|| people::Event::Presence {
                users: users.into_iter().map(|u| (u, presence)).collect(),
            })
        }
        _ => None,
    }
}

/// The demo's answer to a command.
#[cfg(feature = "demo")]
pub fn demo(team: &str, command: Command) -> Vec<Event> {
    match command {
        Command::Watch { users } => vec![Event::People {
            team: team.to_owned(),
            event: people::Event::Presence {
                users: users
                    .into_iter()
                    .map(|u| {
                        // Bob and Dev are away; everyone else is around.
                        let away = matches!(u.as_str(), "U02" | "U04");
                        (
                            u,
                            if away {
                                Presence::Away
                            } else {
                                Presence::Active
                            },
                        )
                    })
                    .collect(),
            },
        }],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ids(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| (*s).to_owned()).collect()
    }

    #[test]
    fn polling_asks_about_the_stalest_first_and_not_too_many() {
        let now = Instant::now();
        let later = now + FRESH_FOR + Duration::from_secs(1);
        let users: Vec<String> = (0..40).map(|n| format!("U{n:02}")).collect();
        let first = due(&users, &HashMap::new(), now);
        assert_eq!(first.len(), ROUND);
        let mut polled: HashMap<String, Instant> = first.iter().map(|u| (u.clone(), now)).collect();
        let second = due(&users, &polled, now);
        assert!(second.iter().all(|u| !first.contains(u)), "{second:?}");
        // Everyone asked about: nothing is due until it goes stale.
        for user in &users {
            polled.entry(user.clone()).or_insert(now);
        }
        assert!(due(&users, &polled, now).is_empty());
        assert_eq!(due(&users, &polled, later).len(), ROUND);
    }

    #[test]
    fn presence_changes_name_one_person_or_several() {
        let one = translate(&json!({"type": "presence_change", "user": "U1", "presence": "away"}));
        assert_eq!(
            one,
            Some(people::Event::Presence {
                users: vec![("U1".into(), Presence::Away)]
            })
        );
        let many = translate(&json!({
            "type": "presence_change",
            "users": ["U1", "W2"],
            "presence": "active"
        }));
        assert_eq!(
            many,
            Some(people::Event::Presence {
                users: vec![
                    ("U1".into(), Presence::Active),
                    ("W2".into(), Presence::Active)
                ]
            })
        );
        assert_eq!(
            translate(&json!({"type": "presence_change", "presence": "active"})),
            None
        );
        assert_eq!(translate(&json!({"type": "hello"})), None);
    }

    #[tokio::test]
    async fn subscriptions_follow_the_screen_and_the_socket() {
        let (sender, mut sent) = tokio::sync::mpsc::unbounded_channel();
        let mut hub = Hub::default();
        hub.rtm_started("T1", sender);
        hub.command(
            "T1",
            Command::Watch {
                users: ids(&["U2", "U1"]),
            },
        );
        assert!(sent.try_recv().is_err(), "nothing goes out before hello");
        hub.rtm_live("T1", true);
        let sub = sent.try_recv().expect("a subscription");
        assert_eq!(sub["type"], "presence_sub");
        assert_eq!(sub["ids"], json!(["U1", "U2"]));
        assert_eq!(sent.try_recv().expect("a query")["type"], "presence_query");
        hub.command(
            "T1",
            Command::Watch {
                users: ids(&["U1", "U3"]),
            },
        );
        assert_eq!(
            sent.try_recv().expect("a subscription")["ids"],
            json!(["U1", "U3"])
        );
        let query = sent.try_recv().expect("a query for the new person");
        assert_eq!(query["ids"], json!(["U3"]));
        hub.rtm_live("T1", false);
        hub.command("T1", Command::Watch { users: Vec::new() });
        assert!(sent.try_recv().is_err(), "nothing goes out while down");
    }
}
