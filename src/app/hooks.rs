//! The app's side of [`crate::hooks`]: deciding which hooks a new message
//! runs, and starting them.

use crate::hooks::{self, Place};
use crate::model::Message;

use super::App;

impl App {
    /// Runs the hooks that match a new message from someone else. Nothing
    /// happens unless hooks are turned on, and never in the demo.
    pub(super) fn run_hooks(&self, team: &str, channel: &str, message: &Message) {
        let settings = &self.settings.hooks;
        if !settings.enabled || settings.list.is_empty() || self.demo {
            return;
        }
        let Some(workspace) = self.workspaces.iter().find(|w| w.info.team_id == team) else {
            return;
        };
        let me = &workspace.info.user_id;
        if message.user.as_deref() == Some(me.as_str()) || message.is_system() {
            return;
        }
        let conversation = workspace.conversation(channel);
        let kind = conversation.map_or_else(|| super::desktop::kind_from_id(channel), |c| c.kind);
        let plain = super::desktop::plain_text(workspace, message);
        let channel_name = conversation.map(|c| workspace.title(c)).unwrap_or_default();
        let place = Place {
            team,
            team_name: &workspace.info.name,
            domain: &workspace.info.domain,
            channel,
            channel_name: &channel_name,
            kind,
        };
        let author = workspace.author(message);
        for hook in &settings.list {
            let argv = hooks::split(&hook.command);
            if argv.is_empty() {
                continue;
            }
            let Some(reason) = hooks::reason(hook, kind, message, &plain, me) else {
                continue;
            };
            let payload = hooks::payload(&place, message, &author, &plain, &reason);
            hooks::run(argv, payload.to_string());
        }
    }
}
