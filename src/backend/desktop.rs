//! The worker's part of the desktop integration: Do Not Disturb from
//! Slack's documented `dnd.*` methods, and mutes and notification levels
//! from the web client's `users.prefs.*`, which only browser sessions may
//! call.
//!
//! Browser sessions may call them. A sign-in through your own Slack app
//! needs the `dnd:read` and `dnd:write` permissions, which the bundled
//! manifest does not ask for (adding them would break sign-in through apps
//! made from the older manifest); without them these calls fail and the
//! interface keeps your snooze to itself.

use serde_json::Value;

use super::{Event, Sink};
use crate::desktop::SlackPrefs;
use crate::dnd::Dnd;
use crate::failure::{Doing, Problem};
use crate::notify::Level;
use crate::slack::Client;

/// `dnd.info`, and the `dnd_status` of a `dnd_updated` event.
#[derive(Debug, Default, serde::Deserialize)]
#[serde(default)]
pub struct DndInfo {
    pub dnd_enabled: bool,
    pub next_dnd_start_ts: Option<i64>,
    pub next_dnd_end_ts: Option<i64>,
    pub snooze_enabled: bool,
    pub snooze_endtime: Option<i64>,
}

impl DndInfo {
    /// The state as the interface keeps it: a schedule only while it is
    /// on, a snooze only while it runs.
    pub fn into_model(self) -> Dnd {
        Dnd {
            schedule: match (
                self.dnd_enabled,
                self.next_dnd_start_ts,
                self.next_dnd_end_ts,
            ) {
                (true, Some(start), Some(end)) if end > start => Some((start, end)),
                _ => None,
            },
            snooze_until: self
                .snooze_endtime
                .filter(|end| self.snooze_enabled && *end > 0),
        }
    }
}

/// The Do Not Disturb state a `dnd_updated` event carries, if it is one.
pub fn dnd_event(event: &Value) -> Option<Dnd> {
    let status = event.get("dnd_status")?;
    serde_json::from_value::<DndInfo>(status.clone())
        .ok()
        .map(DndInfo::into_model)
}

/// Asks Slack for your Do Not Disturb state. A failure is only logged: the
/// interface keeps what it has.
pub async fn dnd_info(client: Client, team: String, sink: Sink) {
    match client.call::<DndInfo>("dnd.info", &[]).await {
        Ok(info) => sink.send(Event::Dnd {
            team,
            dnd: info.into_model(),
        }),
        Err(error) => log::info!("dnd.info: {error}"),
    }
}

/// Snoozes notifications for `minutes`, or with `None` ends the snooze,
/// then reads the state back.
pub async fn snooze(client: Client, team: String, minutes: Option<u32>, sink: Sink) {
    let result = match minutes {
        Some(minutes) => {
            client
                .act::<Value>("dnd.setSnooze", &[("num_minutes", minutes.to_string())])
                .await
        }
        None => client.act::<Value>("dnd.endSnooze", &[]).await,
    };
    match result {
        Ok(_) => dnd_info(client, team, sink).await,
        // Ending a snooze that already ended is what was asked for.
        Err(crate::slack::SlackError::Api(code)) if code == "snooze_not_active" => {
            dnd_info(client, team, sink).await;
        }
        Err(error) => {
            log::info!("could not change the snooze in Slack: {error}");
            sink.send(Event::Error(Problem::new(
                Doing::Snooze,
                super::api::failure(&error),
            )));
        }
    }
}

/// The preferences whose change (a `pref_change` event) alters mutes,
/// notification levels or keywords.
pub fn is_notification_pref(name: &str) -> bool {
    matches!(
        name,
        "muted_channels" | "all_notifications_prefs" | "highlight_words"
    )
}

/// A Slack desktop notification setting as a level; "default" and
/// anything unknown leave the choice to the next rule.
fn level_of(value: &str) -> Option<Level> {
    match value {
        "everything" | "all" => Some(Level::All),
        "nothing" | "none" => Some(Level::Nothing),
        value if value.starts_with("mention") => Some(Level::Mentions),
        _ => None,
    }
}

/// A comma-separated preference as a list, without empty entries.
fn list(value: Option<&Value>) -> Vec<String> {
    value
        .and_then(Value::as_str)
        .unwrap_or_default()
        .split(',')
        .map(str::trim)
        .filter(|item| !item.is_empty())
        .map(str::to_owned)
        .collect()
}

/// Reads `users.prefs.get`'s `prefs`. Slack's web client keeps mutes and
/// per-conversation levels in `all_notifications_prefs`, a JSON object
/// sent as a string; older workspaces also list mutes in
/// `muted_channels`. Both are read, leniently, since none of it is
/// documented.
pub fn parse_prefs(prefs: &Value) -> SlackPrefs {
    let mut out = SlackPrefs {
        muted: list(prefs.get("muted_channels")).into_iter().collect(),
        keywords: list(prefs.get("highlight_words")),
        ..SlackPrefs::default()
    };
    let all = match prefs.get("all_notifications_prefs") {
        Some(Value::String(text)) => serde_json::from_str(text).unwrap_or(Value::Null),
        Some(value) => value.clone(),
        None => Value::Null,
    };
    if let Some(channels) = all.get("channels").and_then(Value::as_object) {
        for (id, channel) in channels {
            if channel.get("muted").and_then(Value::as_bool) == Some(true) {
                out.muted.insert(id.clone());
            }
            if let Some(level) = channel
                .get("desktop")
                .and_then(Value::as_str)
                .and_then(level_of)
            {
                out.levels.insert(id.clone(), level);
            }
        }
    }
    out.channel_default = all
        .get("global")
        .and_then(|global| global.get("global_desktop"))
        .and_then(Value::as_str)
        .and_then(level_of);
    out
}

/// Reads your notification preferences, for a browser session. Sign-ins
/// through your own app cannot call `users.prefs.get`; they keep mutes on
/// this computer.
pub async fn prefs(client: Client, team: String, sink: Sink) {
    // The web client sends the token in the form; do the same.
    let token = client.token().access;
    match client
        .call::<Value>("users.prefs.get", &[("token", token)])
        .await
    {
        Ok(answer) => {
            let prefs = answer.get("prefs").cloned().unwrap_or(Value::Null);
            sink.send(Event::SlackPrefs {
                team,
                prefs: parse_prefs(&prefs),
            });
        }
        Err(error) => log::info!("users.prefs.get: {error}"),
    }
}

/// Mutes or unmutes a conversation in your Slack preferences, then reads
/// them back so the interface shows what Slack kept. The web client's
/// `users.prefs.setNotifications` comes first; workspaces that lack it
/// take the older `muted_channels` list, `all`.
pub async fn mute(
    client: Client,
    team: String,
    channel: String,
    muted: bool,
    all: Vec<String>,
    sink: Sink,
) {
    let token = client.token().access;
    let first = client
        .act::<Value>(
            "users.prefs.setNotifications",
            &[
                ("token", token.clone()),
                ("name", "muted".to_owned()),
                ("value", muted.to_string()),
                ("channel_id", channel),
                ("global", "false".to_owned()),
            ],
        )
        .await;
    let result = match first {
        Ok(answer) => Ok(answer),
        Err(error) => {
            log::info!("users.prefs.setNotifications: {error}; trying users.prefs.set");
            client
                .act::<Value>(
                    "users.prefs.set",
                    &[
                        ("token", token),
                        ("name", "muted_channels".to_owned()),
                        ("value", all.join(",")),
                    ],
                )
                .await
        }
    };
    if let Err(error) = result {
        sink.send(Event::Error(Problem::new(
            Doing::ChangeMute,
            super::api::failure(&error),
        )));
    }
    prefs(client, team, sink).await;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prefs_read_mutes_levels_and_keywords() {
        let all = serde_json::json!({
            "channels": {
                "C1": {"muted": true, "desktop": "default"},
                "C2": {"muted": false, "desktop": "everything"},
                "C3": {"desktop": "nothing"},
                "C4": {"desktop": "mention"}
            },
            "global": {"global_desktop": "mentions_dms"}
        });
        let prefs = serde_json::json!({
            "muted_channels": "C9, ,C8",
            "highlight_words": "deploy,outage",
            "all_notifications_prefs": all.to_string(),
        });
        let parsed = parse_prefs(&prefs);
        let mut muted: Vec<&str> = parsed.muted.iter().map(String::as_str).collect();
        muted.sort_unstable();
        assert_eq!(muted, ["C1", "C8", "C9"]);
        assert_eq!(parsed.levels.get("C1"), None);
        assert_eq!(parsed.levels.get("C2"), Some(&Level::All));
        assert_eq!(parsed.levels.get("C3"), Some(&Level::Nothing));
        assert_eq!(parsed.levels.get("C4"), Some(&Level::Mentions));
        assert_eq!(parsed.channel_default, Some(Level::Mentions));
        assert_eq!(parsed.keywords, ["deploy", "outage"]);
        assert_eq!(parse_prefs(&Value::Null), SlackPrefs::default());
        assert!(is_notification_pref("muted_channels"));
        assert!(!is_notification_pref("theme"));
    }

    #[test]
    fn dnd_info_reads_the_schedule_and_the_snooze() {
        let info: DndInfo = serde_json::from_str(
            r#"{"ok":true,"dnd_enabled":true,"next_dnd_start_ts":100,"next_dnd_end_ts":200,
                "snooze_enabled":true,"snooze_endtime":150,"snooze_remaining":30}"#,
        )
        .expect("parses");
        assert_eq!(
            info.into_model(),
            Dnd {
                schedule: Some((100, 200)),
                snooze_until: Some(150),
            }
        );
        let off: DndInfo =
            serde_json::from_str(r#"{"ok":true,"dnd_enabled":false,"next_dnd_start_ts":1,"next_dnd_end_ts":2,"snooze_enabled":false}"#)
                .expect("parses");
        assert_eq!(off.into_model(), Dnd::default());
    }

    #[test]
    fn dnd_events_carry_their_status() {
        let event = serde_json::json!({
            "type": "dnd_updated",
            "user": "U1",
            "dnd_status": {"dnd_enabled": false, "snooze_enabled": true, "snooze_endtime": 500}
        });
        assert_eq!(
            dnd_event(&event),
            Some(Dnd {
                schedule: None,
                snooze_until: Some(500)
            })
        );
        assert_eq!(dnd_event(&serde_json::json!({"type": "dnd_updated"})), None);
    }
}
