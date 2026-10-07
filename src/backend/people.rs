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
            Command::SetStatus { .. }
            | Command::SetAway(_)
            | Command::DeclineHuddle { .. }
            | Command::CheckHuddle { .. } => {
                log::debug!("{command:?} needs the workspace's client");
            }
            Command::ListenHuddle { .. } | Command::LeaveHuddle | Command::MuteHuddle { .. } => {
                log::debug!("{command:?} is the worker's");
            }
            #[cfg(feature = "huddle-video")]
            Command::WatchCall { .. } => log::debug!("{command:?} is the worker's"),
            #[cfg(feature = "huddle-camera")]
            Command::CameraHuddle { .. } => log::debug!("{command:?} is the worker's"),
            #[cfg(feature = "huddle-share")]
            Command::ShareHuddle { .. } => log::debug!("{command:?} is the worker's"),
            Command::Active => {
                if let Some(rtm) = self
                    .teams
                    .get(team)
                    .filter(|w| w.live)
                    .and_then(|w| w.rtm.as_ref())
                {
                    // Numbered on the way out, like every frame we send;
                    // Slack answers a tickle with nothing.
                    let _ = rtm.send(json!({"type": "tickle"}));
                }
            }
            Command::Typing { channel, thread } => {
                let Some(rtm) = self
                    .teams
                    .get(team)
                    .filter(|w| w.live)
                    .and_then(|w| w.rtm.as_ref())
                else {
                    return;
                };
                let mut frame = json!({"type": "typing", "channel": channel});
                if let Some(thread) = thread {
                    frame["thread_ts"] = thread.as_str().into();
                }
                let _ = rtm.send(frame);
            }
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

/// Runs a command that calls Slack, answering with its result. Returns
/// the command back when it is one [`Hub::command`] handles.
pub fn call(client: Client, team: String, command: Command, sink: Sink) -> Option<Command> {
    match command {
        Command::SetStatus {
            emoji,
            text,
            expiration,
        } => {
            tokio::spawn(async move {
                let result = set_status(&client, &emoji, &text, expiration).await;
                sink.send(Event::People {
                    team,
                    event: people::Event::StatusSet {
                        result: result.map_err(|e| super::api::failure(&e)),
                    },
                });
            });
            None
        }
        Command::SetAway(away) => {
            tokio::spawn(async move {
                let result = set_away(&client, away).await;
                sink.send(Event::People {
                    team,
                    event: people::Event::AwaySet {
                        away,
                        result: result.map_err(|e| super::api::failure(&e)),
                    },
                });
            });
            None
        }
        other => super::huddles::call(client, team, other, sink),
    }
}

/// Sets your status: `emoji` as `:name:`, both empty to clear it, and
/// when it clears by itself in Unix seconds (0 for never). `/status`
/// goes through here too.
pub async fn set_status(
    client: &Client,
    emoji: &str,
    text: &str,
    expiration: i64,
) -> Result<(), SlackError> {
    let profile = json!({
        "status_text": text,
        "status_emoji": emoji,
        "status_expiration": expiration,
    });
    client
        .act::<Value>("users.profile.set", &[("profile", profile.to_string())])
        .await
        .map(|_| ())
}

/// Shows you as away, or lets Slack decide again (`auto`), which shows
/// you active while you use it. `/away` and `/active` go through here.
pub async fn set_away(client: &Client, away: bool) -> Result<(), SlackError> {
    let presence = if away { "away" } else { "auto" };
    client
        .act::<Value>("users.setPresence", &[("presence", presence.to_owned())])
        .await
        .map(|_| ())
}

/// Reads a huddle's room, as Slack sends it on `huddle_thread` messages
/// and `sh_room_*` events: the conversations it is in, and the huddle,
/// or `None` once it has ended or emptied.
pub(super) fn room(room: &Value) -> Option<(Vec<String>, Option<people::Huddle>)> {
    let id = room.get("id").and_then(Value::as_str)?.to_owned();
    let strings = |key: &str| -> Vec<String> {
        room.get(key)
            .and_then(Value::as_array)
            .map(|list| {
                list.iter()
                    .filter_map(Value::as_str)
                    .map(str::to_owned)
                    .collect()
            })
            .unwrap_or_default()
    };
    // Slack lists participants by id, or as objects with a `user_id`.
    let participants: Vec<String> = room
        .get("participants")
        .and_then(Value::as_array)
        .map(|list| {
            list.iter()
                .filter_map(|p| p.as_str().or_else(|| p.get("user_id")?.as_str()))
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default();
    let ended = room.get("has_ended").and_then(Value::as_bool) == Some(true)
        || room
            .get("date_end")
            .and_then(Value::as_i64)
            .is_some_and(|end| end > 0)
        || participants.is_empty();
    let huddle = (!ended).then_some(people::Huddle {
        room: id,
        participants,
    });
    Some((strings("channels"), huddle))
}

/// Whether a room object says who is in it, or that it ended: anything
/// less is a partial update, which must not be read as an empty huddle.
pub(super) fn says_who_or_ended(room: &Value) -> bool {
    room.get("participants").is_some_and(Value::is_array)
        || room.get("has_ended").and_then(Value::as_bool) == Some(true)
        || room
            .get("date_end")
            .and_then(Value::as_i64)
            .is_some_and(|end| end > 0)
}

/// The huddle a message stands for, live: a new `huddle_thread` message,
/// or a change to one (someone joined, left, or it ended).
pub fn huddle_in_message(event: &Value) -> Option<people::Event> {
    if event.get("type").and_then(Value::as_str) != Some("message") {
        return None;
    }
    let channel = event.get("channel").and_then(Value::as_str)?;
    let message = match event.get("subtype").and_then(Value::as_str) {
        Some("message_changed") => event.get("message")?,
        _ => event,
    };
    if message.get("subtype").and_then(Value::as_str) != Some("huddle_thread") {
        return None;
    }
    let (_, huddle) = room(message.get("room")?)?;
    Some(people::Event::Huddles {
        changes: vec![(channel.to_owned(), huddle)],
    })
}

/// Whether a huddle goes on in a conversation, from its newest messages:
/// the newest `huddle_thread` message among them decides. Without one the
/// page says nothing, since a busy huddle can push its message further
/// back than one page.
pub fn huddle_in_history(
    channel: &str,
    messages: &[crate::slack::types::Message],
) -> Option<people::Event> {
    // Slack lists history newest first.
    let newest = messages
        .iter()
        .find(|m| m.subtype.as_deref() == Some("huddle_thread"))?;
    let (_, huddle) = room(newest.room.as_ref()?)?;
    Some(people::Event::Huddles {
        changes: vec![(channel.to_owned(), huddle)],
    })
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
        // A huddle started, someone joined or left, or it ended (browser
        // sessions, over RTM). Without the conversations, only the room
        // is known.
        kind if kind.starts_with("sh_room_") => {
            // The `huddle` object may be a short one without the
            // conversations (as in `rooms.join`'s answer) beside a full
            // `room`: each is tried, and only one that says who is in it
            // or that it ended counts, so a partial one ends nothing.
            let whole = ["room", "huddle"]
                .into_iter()
                .filter_map(|key| event.get(key))
                .filter(|r| says_who_or_ended(r))
                .filter_map(room)
                .find(|(channels, _)| !channels.is_empty());
            match whole {
                Some((channels, huddle)) => Some(people::Event::Huddles {
                    changes: channels.into_iter().map(|c| (c, huddle.clone())).collect(),
                }),
                None => super::huddles::room_change(kind, event),
            }
        }
        // Someone rings you into a huddle (browser sessions), or stopped.
        "huddle_invite" => super::huddles::invite(event),
        "huddle_invite_cancel" => super::huddles::invite_cancel(event),
        // You set yourself away or active, here or in another client.
        "manual_presence_change" => Some(people::Event::ManualPresence {
            away: event.get("presence")?.as_str()? == "away",
        }),
        // Someone typing, over RTM. Socket Mode never sends these.
        "user_typing" => Some(people::Event::Typing {
            channel: event.get("channel")?.as_str()?.to_owned(),
            thread: event
                .get("thread_ts")
                .and_then(Value::as_str)
                .filter(|ts| !ts.is_empty())
                .map(crate::model::Ts::new),
            user: event.get("user")?.as_str()?.to_owned(),
        }),
        _ => None,
    }
}

/// The demo's huddle: Ana and Carla, in #design.
#[cfg(feature = "demo")]
pub fn demo_huddle(team: &str) -> Event {
    Event::People {
        team: team.to_owned(),
        event: people::Event::Huddles {
            changes: vec![(
                "C03".into(),
                Some(people::Huddle {
                    room: "R01".into(),
                    participants: vec!["U01".into(), "U03".into()],
                }),
            )],
        },
    }
}

/// The demo's answer to a command.
#[cfg(feature = "demo")]
pub fn demo(team: &str, command: Command) -> Vec<Event> {
    match command {
        Command::Typing { .. } | Command::Active | Command::CheckHuddle { .. } => Vec::new(),
        // Listening plays nothing in the demo, but the call bar shows as
        // it would: joined, live, and who is in #design's huddle.
        Command::ListenHuddle { channel } => {
            #[cfg(not(feature = "huddle-video"))]
            let roster = crate::demo::listening().roster;
            #[cfg(feature = "huddle-video")]
            let roster = crate::demo::sharing().roster;
            let states = vec![
                crate::huddles::Listen::Joining,
                crate::huddles::Listen::Live,
                crate::huddles::Listen::Roster(roster),
            ];
            // Ana and Carla share their screens and five have a camera
            // on, which play when watched.
            #[cfg(feature = "huddle-video")]
            let states = {
                let mut states = states;
                if let Some(screen) = crate::demo::share_screen() {
                    states.push(crate::huddles::Listen::Screen(screen));
                }
                if let Some(gallery) = crate::demo::camera_gallery() {
                    states.push(crate::huddles::Listen::Gallery(gallery));
                }
                states.push(crate::huddles::Listen::Shares(crate::demo::shares()));
                states.push(crate::huddles::Listen::Cameras(crate::demo::watch_call(
                    &crate::huddles::Wish::closed(),
                )));
                states
            };
            // Your camera's self-preview, the test picture when on.
            #[cfg(feature = "huddle-camera")]
            let states = {
                let mut states = states;
                if let Some(preview) = crate::demo::camera_preview() {
                    states.push(crate::huddles::Listen::Preview(preview));
                }
                states
            };
            states
                .into_iter()
                .map(|state| Event::People {
                    team: team.to_owned(),
                    event: people::Event::Listening {
                        channel: channel.clone(),
                        state,
                    },
                })
                .collect()
        }
        // Left at once; the demo has one huddle to leave, in #design.
        Command::LeaveHuddle => vec![Event::People {
            team: team.to_owned(),
            event: people::Event::Listening {
                channel: "C03".into(),
                state: crate::huddles::Listen::Ended(Ok(crate::huddles::Left::Asked)),
            },
        }],
        // The pretend share and cameras play while watched.
        #[cfg(feature = "huddle-video")]
        Command::WatchCall { wish } => vec![Event::People {
            team: team.to_owned(),
            event: people::Event::Listening {
                channel: "C03".into(),
                state: crate::huddles::Listen::Cameras(crate::demo::watch_call(&wish)),
            },
        }],
        // The demo's camera is the test picture; it opens and closes as
        // asked.
        #[cfg(feature = "huddle-camera")]
        Command::CameraHuddle { on } => {
            crate::demo::camera(on);
            vec![Event::People {
                team: team.to_owned(),
                event: people::Event::Camera {
                    channel: "C03".into(),
                    news: if on {
                        crate::huddle_camera::CamNews::On
                    } else {
                        crate::huddle_camera::CamNews::Off
                    },
                },
            }]
        }
        // The demo shares nobody's screen: sharing turns on at once, as
        // after the system's dialog; choosing again lists pretend
        // screens and windows, as without one.
        #[cfg(feature = "huddle-share")]
        Command::ShareHuddle { request } => {
            use crate::huddle_share::{ShareNews, ShareRequest};
            let news = match request {
                ShareRequest::Start { again: true } => {
                    ShareNews::Choose(crate::demo::share_sources())
                }
                ShareRequest::Start { again: false } | ShareRequest::Pick(_) => ShareNews::On,
                ShareRequest::Stop => ShareNews::Off,
            };
            vec![Event::People {
                team: team.to_owned(),
                event: people::Event::Share {
                    channel: "C03".into(),
                    news,
                },
            }]
        }
        // The demo has no microphone; it opens and closes as asked.
        Command::MuteHuddle { muted } => vec![Event::People {
            team: team.to_owned(),
            event: people::Event::Microphone {
                channel: "C03".into(),
                news: if muted {
                    crate::huddle_mic::MicNews::Muted
                } else {
                    crate::huddle_mic::MicNews::Live
                },
            },
        }],
        Command::DeclineHuddle { .. } => vec![Event::People {
            team: team.to_owned(),
            event: people::Event::InviteDeclined { result: Ok(()) },
        }],
        Command::SetStatus { .. } => vec![Event::People {
            team: team.to_owned(),
            event: people::Event::StatusSet { result: Ok(()) },
        }],
        Command::SetAway(away) => vec![Event::People {
            team: team.to_owned(),
            event: people::Event::AwaySet {
                away,
                result: Ok(()),
            },
        }],
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

    #[test]
    fn huddles_come_from_rooms_and_their_messages() {
        let going = json!({
            "type": "sh_room_join",
            "user": "U2",
            "huddle": {"id": "R1", "channels": ["C1"], "participants": ["U1", "U2"], "date_end": 0}
        });
        let huddle = people::Huddle {
            room: "R1".into(),
            participants: vec!["U1".into(), "U2".into()],
        };
        assert_eq!(
            translate(&going),
            Some(people::Event::Huddles {
                changes: vec![("C1".into(), Some(huddle.clone()))]
            })
        );
        let emptied = json!({
            "type": "sh_room_leave",
            "huddle": {"id": "R1", "channels": ["C1"], "participants": []}
        });
        assert_eq!(
            translate(&emptied),
            Some(people::Event::Huddles {
                changes: vec![("C1".into(), None)]
            })
        );
        let started = json!({
            "type": "message",
            "subtype": "huddle_thread",
            "channel": "C1",
            "ts": "1.0",
            "room": {"id": "R1", "participants": ["U1", "U2"], "has_ended": false}
        });
        assert_eq!(
            huddle_in_message(&started),
            Some(people::Event::Huddles {
                changes: vec![("C1".into(), Some(huddle))]
            })
        );
        let ended = json!({
            "type": "message",
            "subtype": "message_changed",
            "channel": "C1",
            "message": {
                "subtype": "huddle_thread",
                "ts": "1.0",
                "room": {"id": "R1", "participants": [], "has_ended": true}
            }
        });
        assert_eq!(
            huddle_in_message(&ended),
            Some(people::Event::Huddles {
                changes: vec![("C1".into(), None)]
            })
        );
        let plain = json!({"type": "message", "channel": "C1", "ts": "2.0", "text": "hi"});
        assert_eq!(huddle_in_message(&plain), None);
    }

    #[test]
    fn history_says_whether_a_huddle_goes_on() {
        let page: crate::slack::types::HistoryPage = serde_json::from_value(json!({
            "messages": [
                {"ts": "3.0", "text": "after"},
                {"ts": "2.0", "subtype": "huddle_thread", "room": {
                    "id": "R2", "participants": ["U3"], "has_ended": false
                }},
                {"ts": "1.0", "subtype": "huddle_thread", "room": {
                    "id": "R1", "participants": [], "has_ended": true
                }}
            ]
        }))
        .expect("a page");
        assert_eq!(
            huddle_in_history("C1", &page.messages),
            Some(people::Event::Huddles {
                changes: vec![(
                    "C1".into(),
                    Some(people::Huddle {
                        room: "R2".into(),
                        participants: vec!["U3".into()]
                    })
                )]
            })
        );
        assert_eq!(
            huddle_in_history("C1", &page.messages[2..]),
            Some(people::Event::Huddles {
                changes: vec![("C1".into(), None)]
            })
        );
        assert_eq!(huddle_in_history("C1", &page.messages[..1]), None);
    }

    #[test]
    fn typing_names_the_place_and_the_person() {
        assert_eq!(
            translate(&json!({"type": "user_typing", "channel": "C1", "user": "U1"})),
            Some(people::Event::Typing {
                channel: "C1".into(),
                thread: None,
                user: "U1".into()
            })
        );
        assert_eq!(
            translate(&json!({
                "type": "user_typing",
                "channel": "C1",
                "thread_ts": "1.0",
                "user": "U1"
            })),
            Some(people::Event::Typing {
                channel: "C1".into(),
                thread: Some(crate::model::Ts::new("1.0")),
                user: "U1".into()
            })
        );
        assert_eq!(
            translate(&json!({"type": "user_typing", "channel": "C1"})),
            None
        );
    }

    #[tokio::test]
    async fn typing_goes_out_only_over_a_live_socket() {
        let (sender, mut sent) = tokio::sync::mpsc::unbounded_channel();
        let mut hub = Hub::default();
        let typing = || Command::Typing {
            channel: "C1".into(),
            thread: Some(crate::model::Ts::new("1.0")),
        };
        hub.command("T1", typing());
        hub.rtm_started("T1", sender);
        hub.command("T1", typing());
        assert!(sent.try_recv().is_err());
        hub.rtm_live("T1", true);
        // The subscription to nobody, then the notice.
        assert_eq!(
            sent.try_recv().expect("a subscription")["type"],
            "presence_sub"
        );
        hub.command("T1", typing());
        let frame = sent.try_recv().expect("typing");
        assert_eq!(frame["type"], "typing");
        assert_eq!(frame["channel"], "C1");
        assert_eq!(frame["thread_ts"], "1.0");
    }

    #[tokio::test]
    async fn a_tickle_goes_out_only_over_a_live_socket() {
        let (sender, mut sent) = tokio::sync::mpsc::unbounded_channel();
        let mut hub = Hub::default();
        hub.command("T1", Command::Active);
        hub.rtm_started("T1", sender);
        hub.command("T1", Command::Active);
        assert!(
            sent.try_recv().is_err(),
            "nothing before the socket is live"
        );
        hub.rtm_live("T1", true);
        assert_eq!(
            sent.try_recv().expect("a subscription")["type"],
            "presence_sub"
        );
        hub.command("T1", Command::Active);
        assert_eq!(
            sent.try_recv().expect("a tickle"),
            json!({"type": "tickle"})
        );
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
