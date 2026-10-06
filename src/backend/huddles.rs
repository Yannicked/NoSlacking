//! The worker's side of [`crate::huddles`]: reading huddle invitations
//! and room changes off a browser session's socket, declining an
//! invitation, and asking Slack who is in a huddle.
//!
//! None of this is in Slack's public API. The events come on the socket
//! Slack's own clients use, and the two methods are the ones its web
//! client calls, so they only work with a browser session's token and
//! cookie. Socket Mode carries none of it.

use serde_json::Value;

use super::{Event, Sink};
use crate::huddles::RoomChange;
use crate::people::{self, Command};
use crate::slack::{Client, SlackError};

/// Reads `huddle_invite`: someone rings you into a huddle.
pub fn invite(event: &Value) -> Option<people::Event> {
    let text = |key: &str| {
        event
            .get(key)
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
    };
    Some(people::Event::HuddleInvite {
        channel: text("channel_id")?,
        room: text("call_id")?,
        from: text("sender_user_id")?,
    })
}

/// Reads an `sh_room_*` event that names its huddle only by its room: a
/// join or leave with `user`, or an update saying it ended. The room's
/// id is in `call_id`, or in the `room` or `huddle` object.
pub fn room_change(kind: &str, event: &Value) -> Option<people::Event> {
    let room = event.get("room");
    let huddle = event.get("huddle");
    let id = [
        event.get("call_id"),
        room.and_then(|r| r.get("call_id")),
        room.and_then(|r| r.get("id")),
        huddle.and_then(|h| h.get("id")),
    ]
    .into_iter()
    .flatten()
    .find_map(Value::as_str)
    .filter(|id| !id.is_empty())?
    .to_owned();
    let user = || {
        event
            .get("user")
            .and_then(Value::as_str)
            .filter(|u| !u.is_empty())
            .map(str::to_owned)
    };
    let change = match kind {
        "sh_room_join" => RoomChange::Joined(user()?),
        "sh_room_leave" => RoomChange::Left(user()?),
        "sh_room_update" => {
            let ended = [room, huddle].into_iter().flatten().any(|r| {
                r.get("has_ended").and_then(Value::as_bool) == Some(true)
                    || r.get("date_end")
                        .and_then(Value::as_i64)
                        .is_some_and(|end| end > 0)
            });
            if !ended {
                return None;
            }
            RoomChange::Ended
        }
        _ => return None,
    };
    Some(people::Event::HuddleRoom { room: id, change })
}

/// What `rooms.inviteResponse` takes to decline the invitation to `room`
/// in `channel`, as Slack's web client sends it.
pub fn decline_params(channel: &str, room: &str) -> Vec<(&'static str, String)> {
    vec![
        ("response", "decline".to_owned()),
        ("channel_id", channel.to_owned()),
        ("room_id", room.to_owned()),
        ("_x_reason", "respond-to-huddle-invite".to_owned()),
    ]
}

/// What `screenhero.rooms.info` takes to describe `room`, as Slack's web
/// client sends it.
pub fn check_params(room: &str) -> Vec<(&'static str, String)> {
    vec![
        ("room", room.to_owned()),
        ("_x_reason", "all-calls-store/conditional-fetch".to_owned()),
        ("_x_mode", "online".to_owned()),
        ("_x_sonic", "true".to_owned()),
        ("_x_app_name", "client".to_owned()),
    ]
}

/// The huddle `screenhero.rooms.info` describes, `None` once it ended.
/// A room without a list of participants is a shape not understood, and
/// fails: taking it for an empty huddle would end one still going on.
pub fn checked(answer: &Value) -> Result<Option<people::Huddle>, SlackError> {
    let room = answer
        .get("room")
        .filter(|r| r.get("participants").is_some_and(Value::is_array))
        .ok_or_else(|| SlackError::Decode("no room participants".into()))?;
    super::people::room(room)
        .map(|(_, huddle)| huddle)
        .ok_or_else(|| SlackError::Decode("no room id".into()))
}

/// Runs a huddle command that calls Slack, answering with its result.
/// Returns the command back when it is not one.
pub fn call(client: Client, team: String, command: Command, sink: Sink) -> Option<Command> {
    match command {
        Command::DeclineHuddle { channel, room } => {
            tokio::spawn(async move {
                let result = if client.token().is_session() {
                    client
                        .act::<Value>("rooms.inviteResponse", &decline_params(&channel, &room))
                        .await
                        .map(|_| ())
                        .map_err(|e| super::api::failure(&e))
                } else {
                    Err(crate::failure::Failure::MissingPermission)
                };
                sink.send(Event::People {
                    team,
                    event: people::Event::InviteDeclined { result },
                });
            });
            None
        }
        Command::CheckHuddle { channel, room } => {
            if !client.token().is_session() {
                return None;
            }
            tokio::spawn(async move {
                let result = client
                    .call::<Value>("screenhero.rooms.info", &check_params(&room))
                    .await
                    .and_then(|answer| checked(&answer))
                    .map_err(|e| super::api::failure(&e));
                sink.send(Event::People {
                    team,
                    event: people::Event::HuddleChecked {
                        channel,
                        room,
                        result,
                    },
                });
            });
            None
        }
        other => Some(other),
    }
}

/// The demo's invitation: Bob rings you into a huddle in #general.
#[cfg(feature = "demo")]
pub fn demo_invite(team: &str) -> Event {
    Event::People {
        team: team.to_owned(),
        event: people::Event::HuddleInvite {
            channel: "C01".into(),
            room: "R02".into(),
            from: "U02".into(),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn invitations_name_the_huddle_and_who_rang() {
        let event = json!({
            "type": "huddle_invite",
            "channel_id": "C1",
            "call_id": "R1",
            "sender_user_id": "U2",
            "free_willy": {"meeting": {}}
        });
        assert_eq!(
            super::super::people::translate(&event),
            Some(people::Event::HuddleInvite {
                channel: "C1".into(),
                room: "R1".into(),
                from: "U2".into(),
            })
        );
        assert_eq!(
            invite(&json!({"type": "huddle_invite", "channel_id": "C1"})),
            None
        );
    }

    #[test]
    fn joins_and_leaves_known_by_their_room() {
        let translate = super::super::people::translate;
        assert_eq!(
            translate(&json!({"type": "sh_room_join", "user": "U3", "call_id": "R1"})),
            Some(people::Event::HuddleRoom {
                room: "R1".into(),
                change: RoomChange::Joined("U3".into())
            })
        );
        assert_eq!(
            translate(&json!({"type": "sh_room_leave", "user": "U3", "huddle": {"id": "R2"}})),
            Some(people::Event::HuddleRoom {
                room: "R2".into(),
                change: RoomChange::Left("U3".into())
            })
        );
        assert_eq!(
            translate(
                &json!({"type": "sh_room_update", "room": {"call_id": "R3"}, "huddle": {"has_ended": true}})
            ),
            Some(people::Event::HuddleRoom {
                room: "R3".into(),
                change: RoomChange::Ended
            })
        );
        // Still going on, and nothing else said: nothing to do.
        assert_eq!(
            translate(&json!({"type": "sh_room_update", "huddle": {"id": "R3", "date_end": 0}})),
            None
        );
        assert_eq!(
            translate(&json!({"type": "sh_room_join", "call_id": "R1"})),
            None
        );
    }

    #[test]
    fn declining_sends_what_slacks_client_sends() {
        assert_eq!(
            decline_params("C1", "R1"),
            [
                ("response", "decline".to_owned()),
                ("channel_id", "C1".to_owned()),
                ("room_id", "R1".to_owned()),
                ("_x_reason", "respond-to-huddle-invite".to_owned()),
            ]
        );
        let check = check_params("R1");
        assert_eq!(check[0], ("room", "R1".to_owned()));
        assert!(check.contains(&("_x_app_name", "client".to_owned())));
    }

    #[test]
    fn room_info_gives_the_participants() {
        let going = json!({"ok": true, "room": {
            "id": "R1", "channels": ["C1"], "date_end": 0,
            "participants": ["U1", {"user_id": "U2"}]
        }});
        assert_eq!(
            checked(&going).ok(),
            Some(Some(people::Huddle {
                room: "R1".into(),
                participants: vec!["U1".into(), "U2".into()],
            }))
        );
        let ended = json!({"ok": true, "room": {
            "id": "R1", "has_ended": true, "participants": []
        }});
        assert_eq!(checked(&ended).ok(), Some(None));
        // A shape not understood fails rather than ending the huddle.
        assert!(checked(&json!({"ok": true, "room": {"id": "R1"}})).is_err());
        assert!(checked(&json!({"ok": true})).is_err());
    }
}
