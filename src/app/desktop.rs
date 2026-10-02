//! The app's side of the desktop integration: deciding on notifications,
//! acting on their clicks, and asking the window to come forward.
//!
//! Kept apart from `app.rs` so the desktop features grow here, with only
//! small hooks in the main loop.

use crate::backend::Waker;
use crate::badge::{self, Launcher, Unread};
use crate::dnd::{Dnd, Snooze};
use crate::model::{ConversationKind, Message};
use crate::notify::{self, Level, Note, Notifier};

use super::{App, WorkspaceState};

/// The desktop's side of the window.
#[derive(Debug, Default)]
pub struct Desktop {
    /// Come forward: a notification was clicked.
    raise: bool,
    /// Ask for attention (flash the taskbar entry, bounce the Dock icon): a
    /// notification arrived while the window was in the background.
    attention: bool,
    /// What the window title last said, to change it only when it changes.
    title: Option<Unread>,
    /// What the launcher icon last showed, likewise.
    badge: Option<Unread>,
    launcher: Option<Launcher>,
}

impl Desktop {
    /// The desktop side of a new app. The demo leaves the real desktop's
    /// launcher alone.
    pub(super) fn new(demo: bool) -> Self {
        Self {
            launcher: if demo { None } else { Launcher::spawn() },
            ..Self::default()
        }
    }
}

/// What is waiting across `workspaces`, for the title and the badge.
pub(super) fn unread(workspaces: &[WorkspaceState]) -> Unread {
    let mut total = Unread::default();
    for conversation in workspaces.iter().flat_map(|w| &w.conversations) {
        total.mentions = total.mentions.saturating_add(conversation.mentions);
        total.unread |= conversation.has_unread();
    }
    total
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

/// The time now in Unix seconds, for Do Not Disturb.
pub(crate) fn now_seconds() -> i64 {
    jiff::Timestamp::now().as_second()
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
        if workspace.desktop.dnd.quiet(now_seconds()) {
            return None;
        }
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
                self.desktop.attention = true;
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
            self.desktop.raise = true;
        }
        // A scheduled quiet stretch is over: Slack knows the next one.
        let now = now_seconds();
        let passed: Vec<String> = self
            .workspaces
            .iter_mut()
            .filter(|w| !w.desktop.dnd_asked && w.desktop.dnd.schedule_passed(now))
            .map(|w| {
                w.desktop.dnd_asked = true;
                w.info.team_id.clone()
            })
            .collect();
        if !self.demo {
            for team in passed {
                self.backend
                    .send(crate::backend::Command::FetchDnd { team });
            }
        }
        if self.desktop.launcher.is_some() {
            let now = unread(&self.workspaces);
            if self.desktop.badge != Some(now) {
                self.desktop.badge = Some(now);
                if let Some(launcher) = &self.desktop.launcher {
                    launcher.set(now);
                }
            }
        }
    }

    /// Passes the desktop's requests to the window.
    pub(super) fn desktop_window(&mut self, ctx: &egui::Context) {
        let now = unread(&self.workspaces);
        if self.desktop.title != Some(now) {
            self.desktop.title = Some(now);
            ctx.send_viewport_cmd(egui::ViewportCommand::Title(badge::window_title(now)));
        }
        if std::mem::take(&mut self.desktop.raise) {
            ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
            ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(false));
            ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
            self.desktop.attention = false;
        }
        if std::mem::take(&mut self.desktop.attention) && !self.window_focused {
            ctx.send_viewport_cmd(egui::ViewportCommand::RequestUserAttention(
                egui::UserAttentionType::Informational,
            ));
        }
    }

    /// Slack's Do Not Disturb state for a workspace.
    pub(super) fn dnd_arrived(&mut self, team: &str, dnd: Dnd) {
        if let Some(workspace) = self.workspace_mut(team) {
            workspace.desktop.dnd = dnd;
            workspace.desktop.dnd_asked = false;
        }
        self.wake_when_quiet_ends(team);
    }

    /// Snoozes notifications in the open workspace for `choice`, or ends
    /// the snooze. It holds here at once, and Slack is told.
    pub(super) fn snooze(&mut self, choice: Option<Snooze>) {
        let Some(team) = self.active_team() else {
            return;
        };
        let now = jiff::Zoned::now();
        let minutes = choice.map(|choice| choice.minutes(&now));
        let until = minutes.map(|m| now.timestamp().as_second() + i64::from(m) * 60);
        if let Some(workspace) = self.workspace_mut(&team) {
            workspace.desktop.dnd.snooze_until = until;
        }
        if !self.demo {
            self.backend.send(crate::backend::Command::Snooze {
                team: team.clone(),
                minutes,
            });
        }
        self.wake_when_quiet_ends(&team);
    }

    /// Draws again when a workspace's quiet time ends, so its bell does
    /// not show it paused for longer than it is.
    fn wake_when_quiet_ends(&self, team: &str) {
        let now = now_seconds();
        let until = self
            .workspaces
            .iter()
            .find(|w| w.info.team_id == team)
            .and_then(|w| w.desktop.dnd.quiet_until(now));
        if let Some(until) = until {
            let seconds = u64::try_from(until - now).unwrap_or(0);
            self.waker
                .wake_after(std::time::Duration::from_secs(seconds + 1));
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
            workspace.desktop.levels = state.levels;
        }
        self.save_settings();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unread_adds_up_every_workspace() {
        let mut one = WorkspaceState::new(crate::model::Workspace {
            team_id: "T1".into(),
            name: "One".into(),
            domain: String::new(),
            icon: None,
            user_id: "U1".into(),
        });
        let mut two = WorkspaceState::new(crate::model::Workspace {
            team_id: "T2".into(),
            ..one.info.clone()
        });
        assert_eq!(unread(&[]), Unread::default());
        let quiet = crate::model::Conversation {
            id: "C1".into(),
            name: "general".into(),
            kind: ConversationKind::Channel,
            user: None,
            topic: String::new(),
            purpose: String::new(),
            members: None,
            archived: false,
            last_read: None,
            latest: None,
            unread: 0,
            mentions: 0,
        };
        one.conversations.push(crate::model::Conversation {
            mentions: 2,
            unread: 2,
            ..quiet.clone()
        });
        two.conversations.push(crate::model::Conversation {
            mentions: 1,
            ..quiet.clone()
        });
        two.conversations.push(quiet);
        assert_eq!(
            unread(&[one, two]),
            Unread {
                mentions: 3,
                unread: true
            }
        );
    }

    #[test]
    fn unknown_direct_messages_are_recognised_by_their_id() {
        assert_eq!(kind_from_id("D012"), ConversationKind::Direct);
        assert_eq!(kind_from_id("C012"), ConversationKind::Channel);
        assert_eq!(kind_from_id("G012"), ConversationKind::Channel);
    }
}
