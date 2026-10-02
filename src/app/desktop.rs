//! The app's side of the desktop integration: deciding on notifications,
//! acting on their clicks, and asking the window to come forward.
//!
//! Kept apart from `app.rs` so the desktop features grow here, with only
//! small hooks in the main loop.

use crate::backend::Waker;
use crate::model::{ConversationKind, Message};
use crate::notify::{self, Level, Note, Notifier};

use super::{App, WorkspaceState};

/// What the window should do for the desktop on its next frame.
#[derive(Debug, Default)]
pub struct WindowRequests {
    /// Come forward: a notification was clicked.
    raise: bool,
    /// Ask for attention (flash the taskbar entry, bounce the Dock icon): a
    /// notification arrived while the window was in the background.
    attention: bool,
}

/// The notification thread, except in the demo, whose pretend messages
/// would otherwise pop up on a real desktop.
pub(super) fn notifier(waker: &Waker, demo: bool) -> Option<Notifier> {
    if demo {
        return None;
    }
    let waker = waker.clone();
    Notifier::spawn(move || waker.wake())
}

/// The kind of a conversation not loaded yet, from its id: Slack starts
/// direct messages with `D`. Others may be channels or group messages;
/// channel rules are the stricter guess.
fn kind_from_id(channel: &str) -> ConversationKind {
    if channel.starts_with('D') {
        ConversationKind::Direct
    } else {
        ConversationKind::Channel
    }
}

/// A message's text as plain words, with people and channels by name.
fn plain_text(workspace: &WorkspaceState, message: &Message) -> String {
    let text = crate::mrkdwn::plain(&message.text, |inline| match inline {
        crate::mrkdwn::Inline::User { id, .. } => Some(format!("@{}", workspace.user_label(id))),
        crate::mrkdwn::Inline::Channel { id, label } => Some(format!(
            "#{}",
            workspace
                .conversation(id)
                .map(|c| c.name.clone())
                .or_else(|| label.clone())
                .unwrap_or_else(|| id.clone())
        )),
        crate::mrkdwn::Inline::Group { label, .. } => label.clone(),
        _ => None,
    });
    if !text.trim().is_empty() {
        return text;
    }
    // A file with no comment, or an app's message that is all layout.
    match message.files.first() {
        Some(file) => crate::i18n::tf("Shared {name}", &[("name", &file.name)]),
        None => message
            .attachments
            .iter()
            .find_map(|a| a.title.clone().or_else(|| Some(a.text.clone())))
            .unwrap_or_default(),
    }
}

impl App {
    /// The notification a new message deserves, if any. `viewing` says
    /// whether you are looking at its conversation right now.
    pub(super) fn note_for(
        &self,
        team: &str,
        channel: &str,
        message: &Message,
        viewing: bool,
    ) -> Option<Note> {
        let settings = &self.settings.desktop;
        if viewing || !settings.notifications || self.notifier.is_none() {
            return None;
        }
        let workspace = self.workspaces.iter().find(|w| w.info.team_id == team)?;
        let conversation = workspace.conversation(channel);
        // Read on another device already, or the conversation is open in
        // Slack's own client there.
        if conversation
            .and_then(|c| c.last_read.as_ref())
            .is_some_and(|read| *read >= message.ts)
        {
            return None;
        }
        let kind = conversation.map_or_else(|| kind_from_id(channel), |c| c.kind);
        let level = workspace.desktop.level(channel, kind);
        let plain = plain_text(workspace, message);
        notify::reason(
            kind,
            message,
            &plain,
            &workspace.info.user_id,
            level,
            &settings.keywords,
        )?;
        let author = workspace.author(message);
        let place = match conversation {
            Some(c) if !c.kind.is_dm() => format!("#{}", workspace.title(c)),
            Some(c) => workspace.title(c),
            None => author.clone(),
        };
        let (title, body) = notify::compose(kind, &place, &author, &plain);
        Some(Note {
            team: team.to_owned(),
            channel: channel.to_owned(),
            title,
            body,
            sound: settings.sound,
        })
    }

    /// Shows a notification, and asks for attention if the window is in
    /// the background.
    pub(super) fn notify(&mut self, note: Note) {
        if let Some(notifier) = &self.notifier {
            notifier.show(note);
            if !self.window_focused {
                self.window_requests.attention = true;
                self.waker.wake();
            }
        }
    }

    /// Opens the conversations of clicked notifications. Runs with or
    /// without a window.
    pub(super) fn desktop_frame(&mut self) {
        let clicks = self
            .notifier
            .as_ref()
            .map(Notifier::clicks)
            .unwrap_or_default();
        for click in clicks {
            if !self.workspaces.iter().any(|w| w.info.team_id == click.team) {
                continue;
            }
            if self.active_team().as_deref() != Some(click.team.as_str()) {
                self.select_workspace(click.team.clone());
            }
            self.open_conversation(&click.channel);
            self.window_requests.raise = true;
        }
    }

    /// Passes the desktop's requests to the window.
    pub(super) fn desktop_window(&mut self, ctx: &egui::Context) {
        if std::mem::take(&mut self.window_requests.raise) {
            ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
            ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(false));
            ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
            self.window_requests.attention = false;
        }
        if std::mem::take(&mut self.window_requests.attention) && !self.window_focused {
            ctx.send_viewport_cmd(egui::ViewportCommand::RequestUserAttention(
                egui::UserAttentionType::Informational,
            ));
        }
    }

    /// Sets how much of a conversation in the open workspace notifies.
    pub(super) fn set_notify_level(&mut self, channel: &str, level: Option<Level>) {
        let Some(team) = self.active_team() else {
            return;
        };
        self.settings.desktop.set_level(&team, channel, level);
        let state = self.settings.desktop.team_state(&team);
        if let Some(workspace) = self.workspace_mut(&team) {
            workspace.desktop = state;
        }
        self.save_settings();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unknown_direct_messages_are_recognised_by_their_id() {
        assert_eq!(kind_from_id("D012"), ConversationKind::Direct);
        assert_eq!(kind_from_id("C012"), ConversationKind::Channel);
        assert_eq!(kind_from_id("G012"), ConversationKind::Channel);
    }
}
