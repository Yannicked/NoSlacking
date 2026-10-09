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

use super::{App, Tone, WorkspaceState};

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
    /// The tray item, while it is on and the desktop has a tray.
    tray: Option<crate::tray::Tray>,
    /// Whether a window exists now.
    window_open: bool,
    /// Close the window but keep running: the tray asked to hide it.
    hide: bool,
    /// Requests from later launches (show the window, open a link).
    launches: Option<std::sync::mpsc::Receiver<crate::single_instance::Request>>,
    /// Why the login entry could not be changed, from its thread.
    autostart: Option<std::sync::mpsc::Receiver<crate::failure::Failure>>,
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
    for workspace in workspaces {
        for conversation in &workspace.conversations {
            total.mentions = total.mentions.saturating_add(conversation.mentions);
            total.unread |= workspace.is_unread(conversation);
        }
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
pub(super) fn kind_from_id(channel: &str) -> ConversationKind {
    if channel.starts_with('D') {
        ConversationKind::Direct
    } else {
        ConversationKind::Channel
    }
}

/// A message's text as plain words, with people and channels by name.
pub(super) fn plain_text(workspace: &WorkspaceState, message: &Message) -> String {
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
        crate::mrkdwn::Inline::Group { id, label } => {
            Some(workspace.group_label(id, label.as_deref()))
        }
        _ => None,
    });
    if !text.trim().is_empty() {
        return text;
    }
    // A file with no comment, or an app's message that is all layout.
    match message.files.iter().find(|file| !file.deleted) {
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
        if workspace.desktop.dnd.quiet(now_seconds()) || workspace.desktop.is_muted(channel) {
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
        let keywords: Vec<String> = settings
            .keywords
            .iter()
            .chain(workspace.desktop.slack_keywords())
            .cloned()
            .collect();
        notify::reason(
            kind,
            message,
            &plain,
            &workspace.info.user_id,
            level,
            &keywords,
        )?;
        let author = workspace.author(message);
        let place = match conversation {
            Some(c) => workspace.named_place(c),
            None => author.clone(),
        };
        let (title, body) = notify::compose(kind, &place, &author, &plain);
        Some(Note {
            team: team.to_owned(),
            channel: channel.to_owned(),
            title,
            body,
            sound: settings.sound,
            link: None,
        })
    }

    /// The notification a huddle invitation deserves, if any: as for
    /// messages, none with notifications off, during Do Not Disturb or
    /// from a muted conversation, and none while the window is in front,
    /// where the invitation shows anyway. A click joins.
    pub(crate) fn invite_note(&self, team: &str, channel: &str, from: &str) -> Option<Note> {
        let settings = &self.settings.desktop;
        if self.window_focused || !settings.notifications || self.notifier.is_none() {
            return None;
        }
        let workspace = self.workspaces.iter().find(|w| w.info.team_id == team)?;
        if workspace.desktop.dnd.quiet(now_seconds()) || workspace.desktop.is_muted(channel) {
            return None;
        }
        // A Teams call rings: answered here, not in Slack.
        if workspace.info.offers(crate::model::Ability::Calls) {
            let (title, body) = crate::huddles::call_text(&workspace.user_label(from));
            return Some(Note {
                team: team.to_owned(),
                channel: channel.to_owned(),
                title,
                body,
                sound: settings.sound,
                link: None,
            });
        }
        let place = workspace
            .conversation(channel)
            .filter(|c| !c.kind.is_dm())
            .map(|c| workspace.named_place(c));
        let (title, body) =
            crate::huddles::invite_text(&workspace.user_label(from), place.as_deref());
        Some(Note {
            team: team.to_owned(),
            channel: channel.to_owned(),
            title,
            body,
            sound: settings.sound,
            link: Some(crate::people::huddle_url(team, channel)),
        })
    }

    /// Shows a notification, and asks for attention if the window is in
    /// the background.
    pub(crate) fn notify(&mut self, note: Note) {
        if let Some(notifier) = &self.notifier {
            notifier.show(note);
            if !self.window_focused {
                self.desktop.attention = true;
                self.waker.wake();
            }
        }
    }

    /// Takes away the desktop's notification of a huddle invitation in
    /// `channel` that stopped ringing, where the desktop allows it.
    pub(crate) fn withdraw_invite_note(&self, team: &str, channel: &str) {
        if let Some(notifier) = &self.notifier {
            notifier.withdraw(team, channel);
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
            // A huddle invitation: joining answers it.
            if let Some(link) = &click.link {
                if let Some(invite) = self
                    .huddles
                    .invites
                    .list()
                    .iter()
                    .find(|i| i.team == click.team && i.channel == click.channel)
                {
                    let action = crate::huddles::Action::Join {
                        team: invite.team.clone(),
                        room: invite.room.clone(),
                    };
                    crate::huddles::apply(self, action);
                    // Joined here: the call bar is in this window.
                    self.desktop.raise = true;
                } else {
                    self.open_url(link);
                }
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
        self.tray_requests();
        self.launch_requests();
        let failures: Vec<crate::failure::Failure> = self
            .desktop
            .autostart
            .as_ref()
            .map(|r| r.try_iter().collect())
            .unwrap_or_default();
        for error in failures {
            self.toast(
                crate::i18n::tf(
                    "Could not change starting at login: {error}",
                    &[("error", &error.message())],
                ),
                Tone::Error,
            );
        }
        if let Some(tray) = &mut self.desktop.tray {
            tray.set_unread(unread(&self.workspaces));
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
        if self.quit || self.desktop.hide {
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }
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

    /// Shows the tray item if the settings want one. Not in the demo: its
    /// item would sit in the real desktop's tray.
    pub(super) fn start_tray(&mut self) {
        if self.demo || !self.settings.desktop.tray || self.desktop.tray.is_some() {
            return;
        }
        let waker = self.waker.clone();
        self.desktop.tray = crate::tray::Tray::spawn(move || waker.wake());
        if self.desktop.window_open
            && let Some(tray) = &mut self.desktop.tray
        {
            tray.attach();
        }
    }

    /// Writes (or removes) the login entry off this thread: on Windows it
    /// runs reg.exe. Failures come back as a toast.
    fn apply_autostart(&mut self, enabled: bool) {
        if self.demo {
            return;
        }
        let (sender, failures) = std::sync::mpsc::channel();
        self.desktop.autostart = Some(failures);
        let waker = self.waker.clone();
        let spawned = std::thread::Builder::new()
            .name("autostart".into())
            .spawn(move || {
                if let Err(error) = crate::autostart::set(enabled) {
                    log::warn!("could not change starting at login: {error:?}");
                    let _ = sender.send(error);
                    waker.wake();
                }
            });
        if let Err(error) = spawned {
            log::warn!("could not change starting at login: {error}");
        }
    }

    /// Turns starting at login on or off.
    pub fn set_start_on_login(&mut self, on: bool) {
        self.settings.desktop.start_on_login = on;
        self.apply_autostart(on);
        self.save_settings();
    }

    /// Rewrites the login entry at start-up, so it names this executable
    /// even after the app was moved or updated.
    pub(super) fn refresh_autostart(&mut self) {
        if self.settings.desktop.start_on_login {
            self.apply_autostart(true);
        }
    }

    /// Turns the tray item on or off. On macOS a menu-bar item stays until
    /// NoSlacking quits.
    pub fn set_tray(&mut self, on: bool) {
        self.settings.desktop.tray = on;
        if on {
            self.start_tray();
        } else {
            self.desktop.tray = None;
        }
        self.save_settings();
    }

    /// Whether the desktop showed a tray item, so the window can close
    /// into it.
    pub fn has_tray(&self) -> bool {
        self.desktop.tray.is_some()
    }

    /// Hands over the requests later launches send (see
    /// [`crate::single_instance`]), which arrive with or without a window.
    pub fn listen_for_launches(
        &mut self,
        launches: std::sync::mpsc::Receiver<crate::single_instance::Request>,
    ) {
        self.desktop.launches = Some(launches);
    }

    fn launch_requests(&mut self) {
        let requests: Vec<_> = self
            .desktop
            .launches
            .as_ref()
            .map(|r| r.try_iter().collect())
            .unwrap_or_default();
        for request in requests {
            if let crate::single_instance::Request::Open(link) = request {
                if crate::meetings::is_meeting_link(&link) {
                    self.open_meeting_link(&link);
                } else {
                    self.backend.send(crate::backend::Command::Callback(link));
                }
            }
            self.desktop.raise = true;
        }
    }

    fn tray_requests(&mut self) {
        use crate::tray::Request;
        let requests = self
            .desktop
            .tray
            .as_ref()
            .map(crate::tray::Tray::requests)
            .unwrap_or_default();
        for request in requests {
            match request {
                Request::Toggle if self.desktop.window_open => self.desktop.hide = true,
                Request::Toggle | Request::Show => {
                    self.desktop.hide = false;
                    self.desktop.raise = true;
                }
                Request::Quit => self.quit = true,
            }
            self.waker.wake();
        }
    }

    /// A window was made.
    pub(super) fn window_made(&mut self) {
        self.desktop.window_open = true;
        if let Some(tray) = &mut self.desktop.tray {
            tray.attach();
        }
    }

    /// The window is gone and the app runs on without one.
    pub(super) fn window_left(&mut self) {
        self.desktop.window_open = false;
        self.desktop.hide = false;
        self.desktop.title = None;
        self.window_focused = false;
    }

    /// What closing the window means: quit, or keep running in the tray
    /// when there is one and you asked for that (in the settings, or by
    /// hiding the window from the tray).
    pub(super) fn closed_action(&self) -> fastframe_shell::Closed {
        if !self.quit
            && self.desktop.tray.is_some()
            && (self.settings.desktop.close_to_tray || self.desktop.hide)
        {
            fastframe_shell::Closed::Hide
        } else {
            fastframe_shell::Closed::Quit
        }
    }

    /// Whether the window should come back while none is open.
    pub(super) fn wants_window(&self) -> bool {
        self.desktop.raise
    }

    /// Whether a launch asking to start hidden (at login) may: only with
    /// the tray to come back through. macOS makes its menu-bar item with
    /// the first window, so it always opens one.
    pub(super) fn can_start_hidden(&self) -> bool {
        cfg!(not(target_os = "macos"))
            && self.desktop.tray.is_some()
            && self.settings.desktop.close_to_tray
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

    /// Your notification preferences from Slack, for a browser session.
    pub(super) fn prefs_arrived(&mut self, team: &str, prefs: crate::desktop::SlackPrefs) {
        if let Some(workspace) = self.workspace_mut(team) {
            workspace.desktop.slack = Some(prefs);
        }
    }

    /// Mutes or unmutes a conversation in the open workspace: in Slack for
    /// a browser session, on this computer otherwise. It shows at once.
    pub(super) fn mute(&mut self, channel: &str, muted: bool) {
        let Some(team) = self.active_team() else {
            return;
        };
        let demo = self.demo;
        let Some(workspace) = self.workspace_mut(&team) else {
            return;
        };
        if let Some(slack) = &mut workspace.desktop.slack {
            if muted {
                slack.muted.insert(channel.to_owned());
            } else {
                slack.muted.remove(channel);
            }
            let mut all: Vec<String> = slack.muted.iter().cloned().collect();
            all.sort();
            if !demo {
                self.backend.send(crate::backend::Command::Mute {
                    team,
                    channel: channel.to_owned(),
                    muted,
                    all,
                });
            }
            return;
        }
        self.settings.desktop.set_muted(&team, channel, muted);
        let local = self.settings.desktop.team_state(&team).local_muted;
        if let Some(workspace) = self.workspace_mut(&team) {
            workspace.desktop.local_muted = local;
        }
        self.save_settings();
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
            service: crate::model::Service::Slack,
            team_id: "T1".into(),
            name: "One".into(),
            domain: String::new(),
            icon: None,
            user_id: "U1".into(),
            sign_in: Default::default(),
            scopes: None,
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
            external: false,
            is_open: None,
            empty: false,
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
    fn muted_conversations_are_unread_only_for_mentions() {
        let mut w = WorkspaceState::new(crate::model::Workspace {
            service: crate::model::Service::Slack,
            team_id: "T1".into(),
            name: "One".into(),
            domain: String::new(),
            icon: None,
            user_id: "U1".into(),
            sign_in: Default::default(),
            scopes: None,
        });
        let mut c = crate::model::Conversation {
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
            unread: 3,
            mentions: 0,
            external: false,
            is_open: None,
            empty: false,
        };
        assert!(w.is_unread(&c));
        w.desktop.local_muted.insert("C1".into());
        assert!(!w.is_unread(&c));
        assert_eq!(unread(std::slice::from_ref(&w)), Unread::default());
        c.mentions = 1;
        assert!(w.is_unread(&c));
    }

    #[test]
    fn notifications_name_user_groups_even_without_a_label() {
        let mut w = WorkspaceState::new(crate::model::Workspace {
            service: crate::model::Service::Slack,
            team_id: "T1".into(),
            name: "One".into(),
            domain: String::new(),
            icon: None,
            user_id: "U1".into(),
            sign_in: Default::default(),
            scopes: None,
        });
        w.groups.push(crate::model::UserGroup {
            id: "S1".into(),
            handle: "design".into(),
            name: "Design".into(),
            members: None,
        });
        let message = Message {
            ts: crate::model::Ts::new("1.0"),
            user: None,
            username: None,
            bot_icon: None,
            bot_id: None,
            text: "<!subteam^S1> and <!subteam^S9> and <!subteam^S8|@ops>".into(),
            thread_ts: None,
            reply_count: 0,
            replies_known: false,
            reply_users: Vec::new(),
            latest_reply: None,
            reactions: Vec::new(),
            files: Vec::new(),
            attachments: Vec::new(),
            blocks: Vec::new(),
            edited: false,
            subtype: None,
            delivery: crate::model::Delivery::Sent,
            broadcast: false,
            pinned: false,
            client_msg_id: None,
            subscribed: None,
        };
        assert_eq!(plain_text(&w, &message), "@design and @S9 and @ops");
    }

    #[test]
    fn unknown_direct_messages_are_recognised_by_their_id() {
        assert_eq!(kind_from_id("D012"), ConversationKind::Direct);
        assert_eq!(kind_from_id("C012"), ConversationKind::Channel);
        assert_eq!(kind_from_id("G012"), ConversationKind::Channel);
    }
}
