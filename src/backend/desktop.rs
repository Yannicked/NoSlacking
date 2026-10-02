//! The worker's part of the desktop integration: Do Not Disturb from
//! Slack's documented `dnd.*` methods.
//!
//! Browser sessions may call them. A sign-in through your own Slack app
//! needs the `dnd:read` and `dnd:write` permissions, which the bundled
//! manifest does not ask for (adding them would break sign-in through apps
//! made from the older manifest); without them these calls fail and the
//! interface keeps your snooze to itself.

use serde_json::Value;

use super::{Event, Sink};
use crate::dnd::Dnd;
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
            sink.send(Event::Notice(format!(
                "Slack did not take the snooze ({}); it holds on this computer only.",
                super::worker::describe(&error)
            )));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
