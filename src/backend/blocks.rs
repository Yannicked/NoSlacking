//! Pressing an app's Block Kit button, or choosing from its menu.
//!
//! An app's interactive button is meant for Slack's own clients: the public
//! Web API has no method to press one. Slack's web client calls the
//! internal `blocks.actions`, which only its own sign-in (a browser
//! session) may call, and Slack then tells the app as if the press came
//! from Slack's client. The parameters below are the ones Slack's web
//! client sends, as other clients that press buttons send them
//! (emacs-slack's `slack-block-action-execute`, slack-user-cli's
//! `_dispatch_block_action`): the bot as `service_id`, the press as
//! `actions`, the message as `container`, and a `client_token`.
//!
//! A choice from a menu is sent the same way, its action carrying
//! `selected_option` (the choice's `text` and `value`) in place of a
//! button's `text` and `value`, as emacs-slack
//! (`slack-buffer-execute-static-select-block-action` and its overflow
//! twin), slack-user-cli (`_build_action_payload`) and rho
//! (`run_interaction`) send it. A select also repeats its `placeholder`
//! (emacs-slack), and a select or radio buttons mirror the choice in
//! `state`, as slack-user-cli found the web client doing; an overflow menu
//! keeps no state.
//!
//! An OAuth sign-in never gets this far: [`crate::model::button_use`] does
//! not offer the press there, and the worker refuses it with
//! [`Failure::NeedsSession`] should one arrive.

use serde_json::{Value, json};

use super::api::failure;
use crate::failure::Failure;
use crate::model::{Press, PressKind};
use crate::slack::Client;

/// The form `blocks.actions` takes for `press`, made at `now_ms`
/// (milliseconds since 1970), which dates the press and its token.
pub(super) fn press_params(press: &Press, now_ms: u64) -> Vec<(&'static str, String)> {
    // Slack's own form of a time: seconds, then six digits of fraction.
    let action_ts = format!("{}.{:06}", now_ms / 1000, (now_ms % 1000) * 1000);
    let text = |text: &str| json!({"type": "plain_text", "text": text, "emoji": true});
    let kind = match press.kind {
        PressKind::Button => "button",
        PressKind::Select { .. } => "static_select",
        PressKind::Overflow => "overflow",
        PressKind::Radio => "radio_buttons",
    };
    let mut action = json!({
        "action_id": press.action_id,
        "block_id": press.block_id,
        "type": kind,
        "action_ts": action_ts,
    });
    // What a menu's choice is, as its option object.
    let chosen = json!({
        "text": text(&press.text),
        "value": press.value.as_deref().unwrap_or_default(),
    });
    if let Value::Object(fields) = &mut action {
        match &press.kind {
            PressKind::Button => {
                fields.insert("text".into(), text(&press.text));
                if let Some(value) = &press.value {
                    fields.insert("value".into(), Value::String(value.clone()));
                }
            }
            PressKind::Select { placeholder } => {
                fields.insert("selected_option".into(), chosen.clone());
                if let Some(placeholder) = placeholder {
                    fields.insert("placeholder".into(), text(placeholder));
                }
            }
            PressKind::Overflow | PressKind::Radio => {
                fields.insert("selected_option".into(), chosen.clone());
            }
        }
    }
    let container = json!({
        "type": "message",
        "message_ts": press.ts.as_str(),
        "channel_id": press.channel,
        "is_ephemeral": false,
    });
    let mut params = vec![
        ("service_id", press.bot_id.clone()),
        ("actions", Value::Array(vec![action]).to_string()),
        ("container", container.to_string()),
        // Slack's web client numbers its requests this way; nothing here
        // reads it back.
        ("client_token", format!("web-{now_ms}")),
    ];
    // What the message's inputs now hold, which Slack passes on to the app
    // as `state.values`: the choice made, for the one input that changed.
    if matches!(press.kind, PressKind::Select { .. } | PressKind::Radio) {
        let state = json!({"values": {
            press.block_id.as_str(): {
                press.action_id.as_str(): {"type": kind, "selected_option": chosen},
            },
        }});
        params.push(("state", state.to_string()));
    }
    params
}

/// Sends `press` to Slack. It is never retried after a network failure
/// ([`Client::act`]): a press is not something to do twice.
pub(super) async fn press(client: &Client, press: &Press, now_ms: u64) -> Result<(), Failure> {
    if !client.is_session() {
        return Err(Failure::NeedsSession);
    }
    client
        .act::<Value>("blocks.actions", &press_params(press, now_ms))
        .await
        .map(|_| ())
        .map_err(|error| failure(&error))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Ts;

    fn approve(value: Option<&str>) -> Press {
        Press {
            channel: "C05".into(),
            ts: Ts::new("1790171900.000100"),
            bot_id: "B09".into(),
            block_id: "deploy".into(),
            action_id: "approve".into(),
            text: "Approve".into(),
            value: value.map(str::to_owned),
            kind: PressKind::Button,
        }
    }

    /// The release channel picked from a static select, or from an
    /// overflow menu or radio buttons as `kind` says.
    fn pick(kind: PressKind) -> Press {
        Press {
            block_id: "rollout".into(),
            action_id: "channel".into(),
            text: "Beta :test_tube:".into(),
            value: Some("beta".into()),
            kind,
            ..approve(None)
        }
    }

    fn param<'a>(params: &'a [(&str, String)], name: &str) -> &'a str {
        params
            .iter()
            .find(|(key, _)| *key == name)
            .map(|(_, value)| value.as_str())
            .expect("the parameter is there")
    }

    #[test]
    fn a_press_is_sent_as_the_web_client_sends_it() {
        let params = press_params(&approve(Some("release-1288")), 1_790_172_000_123);
        let names: Vec<&str> = params.iter().map(|(key, _)| *key).collect();
        assert_eq!(
            names,
            ["service_id", "actions", "container", "client_token"]
        );
        assert_eq!(
            param(&params, "service_id"),
            "B09",
            "the bot gets the press"
        );
        assert_eq!(param(&params, "client_token"), "web-1790172000123");
        let actions: Value =
            serde_json::from_str(param(&params, "actions")).expect("actions are JSON");
        assert_eq!(
            actions,
            json!([{
                "action_id": "approve",
                "block_id": "deploy",
                "text": {"type": "plain_text", "text": "Approve", "emoji": true},
                "value": "release-1288",
                "type": "button",
                "action_ts": "1790172000.123000",
            }])
        );
        let container: Value =
            serde_json::from_str(param(&params, "container")).expect("the container is JSON");
        assert_eq!(
            container,
            json!({
                "type": "message",
                "message_ts": "1790171900.000100",
                "channel_id": "C05",
                "is_ephemeral": false,
            })
        );
    }

    #[test]
    fn a_choice_from_a_select_is_sent_with_its_option_and_state() {
        let press = pick(PressKind::Select {
            placeholder: Some("Pick a channel".into()),
        });
        let params = press_params(&press, 1_790_172_000_123);
        let names: Vec<&str> = params.iter().map(|(key, _)| *key).collect();
        assert_eq!(
            names,
            [
                "service_id",
                "actions",
                "container",
                "client_token",
                "state"
            ]
        );
        let actions: Value =
            serde_json::from_str(param(&params, "actions")).expect("actions are JSON");
        assert_eq!(
            actions,
            json!([{
                "action_id": "channel",
                "block_id": "rollout",
                "type": "static_select",
                "selected_option": {
                    "text": {"type": "plain_text", "text": "Beta :test_tube:", "emoji": true},
                    "value": "beta",
                },
                "placeholder": {"type": "plain_text", "text": "Pick a channel", "emoji": true},
                "action_ts": "1790172000.123000",
            }])
        );
        assert!(actions[0].get("text").is_none(), "a select has no label");
        let state: Value = serde_json::from_str(param(&params, "state")).expect("state is JSON");
        assert_eq!(
            state,
            json!({"values": {"rollout": {"channel": {
                "type": "static_select",
                "selected_option": {
                    "text": {"type": "plain_text", "text": "Beta :test_tube:", "emoji": true},
                    "value": "beta",
                },
            }}}})
        );
    }

    #[test]
    fn a_choice_from_an_overflow_menu_keeps_no_state() {
        let params = press_params(&pick(PressKind::Overflow), 5);
        assert!(params.iter().all(|(key, _)| *key != "state"));
        let actions: Value =
            serde_json::from_str(param(&params, "actions")).expect("actions are JSON");
        assert_eq!(
            actions,
            json!([{
                "action_id": "channel",
                "block_id": "rollout",
                "type": "overflow",
                "selected_option": {
                    "text": {"type": "plain_text", "text": "Beta :test_tube:", "emoji": true},
                    "value": "beta",
                },
                "action_ts": "0.005000",
            }])
        );
    }

    #[test]
    fn a_radio_choice_is_sent_like_a_select_without_a_placeholder() {
        let params = press_params(&pick(PressKind::Radio), 5);
        let actions: Value =
            serde_json::from_str(param(&params, "actions")).expect("actions are JSON");
        assert_eq!(actions[0]["type"], "radio_buttons");
        assert_eq!(actions[0]["selected_option"]["value"], "beta");
        assert!(actions[0].get("placeholder").is_none());
        let state: Value = serde_json::from_str(param(&params, "state")).expect("state is JSON");
        assert_eq!(
            state["values"]["rollout"]["channel"]["type"],
            "radio_buttons"
        );
    }

    #[test]
    fn a_button_without_a_value_sends_none() {
        let params = press_params(&approve(None), 5);
        let actions: Value =
            serde_json::from_str(param(&params, "actions")).expect("actions are JSON");
        assert!(actions[0].get("value").is_none());
        assert_eq!(actions[0]["action_ts"], "0.005000");
    }
}
