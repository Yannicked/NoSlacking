//! How the worker's events change the app: sign-in, workspaces, and
//! the conversations, messages and people that arrive.
//!
//! A second `impl App`, kept apart so `app.rs` stays readable. What an
//! event changes in one workspace lives in [`super::workspace`]; this
//! side adds what needs the backend: fetching, toasts and scrolling.

use super::workspace::Arrived;
use super::{App, Page, WorkspaceState};
use crate::backend::{Change, Command, Event, SignIn, Socket};
use crate::credentials::AppCredentials;
use crate::emoji::EmojiSet;
use crate::i18n::{t, tf};
use crate::model::{Conversation, Message, Ts, Workspace};
use crate::settings::WorkspaceMeta;

impl App {
    pub(super) fn handle(&mut self, event: Event) {
        match event {
            // The account: the app, the keyring, sign-in and the socket.
            Event::AppLoaded(app) => self.app_loaded(app),
            Event::KeyringError(error) => {
                self.toast(tf("Keyring: {error}", &[("error", &error)]), true);
                self.keyring_error = Some(error);
            }
            Event::SignIn(state) => self.sign_in_changed(state),
            Event::WorkspaceReady(info) => self.workspace_ready(info),
            Event::SignedOut { team, reason } => self.signed_out(&team, reason),
            Event::Socket(socket) => self.socket_changed(socket),
            Event::Error(error) => self.toast(error, true),
            Event::UploadProgress { id, sent, total } => {
                if let Some(upload) = self.transfers.iter_mut().find(|u| u.id == id) {
                    upload.sent = sent;
                    upload.total = total;
                }
            }
            Event::UploadDone { id } => self.upload_done(id),
            Event::Slash { command, result } => self.slash_done(&command, result),
            Event::Notice(text) => self.toast(text, false),
            Event::Dnd { team, dnd } => self.dnd_arrived(&team, dnd),
            Event::SlackPrefs { team, prefs } => self.prefs_arrived(&team, prefs),
            Event::DeepLink(link) => {
                if !self.follow(&link) {
                    self.toast(t("That conversation is not open to you here"), true);
                }
            }
            // A workspace's conversations, people, apps and sidebar.
            Event::Conversations {
                team,
                list,
                complete,
            } => self.conversations(&team, list, complete),
            Event::Conversation { team, conversation } => {
                if let Some(workspace) = self.workspace_mut(&team) {
                    let users = workspace.conversation_arrived(conversation);
                    self.fetch_users(&team, users);
                }
            }
            Event::ConversationGone { team, channel } => self.conversation_gone(&team, &channel),
            Event::Users { team, users } => {
                if let Some(workspace) = self.workspace_mut(&team) {
                    workspace.users_arrived(users);
                }
            }
            Event::Bots { team, bots } => {
                if let Some(workspace) = self.workspace_mut(&team) {
                    workspace.bots_arrived(bots);
                }
            }
            Event::Sections { team, sections } => {
                if let Some(workspace) = self.workspace_mut(&team) {
                    workspace.sections = Some(sections);
                }
            }
            Event::Emoji { team, emoji } => {
                if let Some(workspace) = self.workspace_mut(&team) {
                    workspace.emoji = EmojiSet::new(emoji);
                }
            }
            // Messages and read state.
            Event::History {
                team,
                channel,
                messages,
                has_more,
                cursor,
                older,
            } => self.history(&team, &channel, messages, has_more, cursor, older),
            Event::CachedHistory {
                team,
                channel,
                messages,
                has_more,
                cursor,
            } => {
                if let Some(workspace) = self.workspace_mut(&team)
                    && let Some(arrived) =
                        workspace.cached_history_arrived(&channel, messages, has_more, cursor)
                {
                    self.scroll_to_bottom
                        .insert(Self::draft_key(&team, &channel, None));
                    self.fetch_arrived(&team, arrived);
                }
            }
            Event::HistoryFailed {
                team,
                channel,
                error,
            } => {
                if let Some(workspace) = self.workspace_mut(&team) {
                    workspace.history_failed(&channel);
                }
                self.toast(
                    tf("Could not load messages: {error}", &[("error", &error)]),
                    true,
                );
            }
            Event::Around {
                team,
                channel,
                ts,
                messages,
                has_older,
                cursor,
                has_newer,
            } => {
                if let Some(workspace) = self.workspace_mut(&team) {
                    let arrived = workspace.around_arrived(
                        &channel,
                        messages,
                        (has_older, cursor),
                        has_newer,
                    );
                    log::debug!("loaded the messages around {} in {channel}", ts.as_str());
                    self.fetch_arrived(&team, arrived);
                }
                // An anchor kept for the list as it was is stale now.
                self.prepended = None;
            }
            Event::Search {
                team,
                request,
                result,
            } => {
                if self.search.query.as_ref().is_some_and(|q| q.team == team) {
                    self.search.arrived(request, result);
                }
            }
            Event::Newer {
                team,
                channel,
                messages,
                has_newer,
            } => {
                if let Some(workspace) = self.workspace_mut(&team) {
                    let arrived = workspace.newer_arrived(&channel, messages, has_newer);
                    self.fetch_arrived(&team, arrived);
                }
                self.mark_if_viewing(&team, &channel);
            }
            Event::Thread {
                team,
                channel,
                ts,
                messages,
            } => {
                if let Some(workspace) = self.workspace_mut(&team) {
                    let arrived = workspace.thread_arrived(&channel, ts, messages);
                    self.fetch_arrived(&team, arrived);
                }
            }
            Event::Message {
                team,
                channel,
                message,
                changed,
            } => self.message(&team, &channel, message, changed),
            Event::Deleted { team, channel, ts } => self.remove_message(&team, &channel, &ts),
            Event::Reaction {
                team,
                channel,
                ts,
                name,
                user,
                added,
            } => {
                if let Some(workspace) = self.workspace_mut(&team) {
                    workspace.reaction_changed(&channel, &ts, &name, &user, added);
                }
            }
            Event::Sent {
                team,
                channel,
                local,
                result,
            } => self.sent(&team, &channel, &local, result),
            Event::Read { team, channel, ts } => {
                if let Some(workspace) = self.workspace_mut(&team) {
                    workspace.read_elsewhere(&channel, ts);
                }
            }
            Event::Settled {
                team,
                channel,
                change,
                result,
            } => self.settled(&team, &channel, change, result),
            Event::Convos { team, event } => crate::convos::handle(self, &team, event),
            Event::People { team, event } => crate::people::handle(self, &team, event),
            Event::Views { team, event } => crate::views::handle(self, &team, event),
        }
    }

    /// Slack answered an edit, delete or reaction; a refused one is
    /// undone on screen, and you are told.
    fn settled(&mut self, team: &str, channel: &str, change: Change, result: Result<(), String>) {
        let Err(error) = result else { return };
        let what = match &change {
            Change::Edit { .. } => t("Could not edit the message"),
            Change::Delete { .. } => t("Could not delete the message"),
            Change::React { .. } => t("Could not change the reaction"),
        };
        if let Some(workspace) = self.workspace_mut(team) {
            workspace.undo(channel, change);
        }
        self.toast(format!("{what}: {error}"), true);
    }

    fn app_loaded(&mut self, app: Option<AppCredentials>) {
        if let Some(app) = &app {
            self.setup.client_id = app.client_id.clone();
            self.setup.client_secret = app.client_secret.clone();
            self.setup.app_token = app.app_token.clone();
        }
        self.app_credentials = app;
        self.app_loaded = true;
    }

    fn sign_in_changed(&mut self, state: SignIn) {
        if let SignIn::Done(name) = &state {
            self.toast(tf("Signed in to {name}.", &[("name", name)]), false);
            self.page = Page::Main;
            self.setup.user_token.clear();
        }
        self.sign_in = Some(state);
    }

    fn workspace_ready(&mut self, info: Workspace) {
        let team = info.team_id.clone();
        self.settings.upsert_workspace(WorkspaceMeta {
            team_id: info.team_id.clone(),
            name: info.name.clone(),
            domain: info.domain.clone(),
            icon: info.icon.clone(),
            user_id: info.user_id.clone(),
        });
        match self.workspace_mut(&team) {
            Some(workspace) => {
                workspace.info = info;
                workspace.signed_out = None;
            }
            None => {
                let mut state = WorkspaceState::new(info);
                state.active = self.settings.last_conversation.get(&team).cloned();
                state.desktop = self.settings.desktop.team_state(&team);
                self.workspaces.push(state);
            }
        }
        if self.settings.active_workspace.is_none() {
            self.settings.active_workspace = Some(team);
        }
        self.save_settings();
    }

    /// A workspace needs signing in again (`reason`), or was signed out.
    fn signed_out(&mut self, team: &str, reason: Option<String>) {
        match reason {
            Some(reason) => {
                if let Some(workspace) = self.workspace_mut(team) {
                    workspace.signed_out = Some(reason);
                }
            }
            None => {
                // What you were writing there goes with the sign-in.
                let prefix = format!("{team}/");
                self.drafts.retain(|key, _| !key.starts_with(&prefix));
                self.workspaces.retain(|w| w.info.team_id != team);
                self.settings.remove_workspace(team);
                self.save_settings();
                if self.workspaces.is_empty() {
                    self.page = Page::SignIn;
                }
            }
        }
    }

    fn socket_changed(&mut self, socket: Socket) {
        if let Socket::Rejected(reason) = &socket {
            self.toast(
                tf(
                    "Slack refused the app-level token ({reason})",
                    &[("reason", reason)],
                ),
                true,
            );
        }
        self.socket = socket;
    }

    fn conversations(&mut self, team: &str, list: Vec<Conversation>, complete: bool) {
        let active_team = self.active_team();
        let Some(workspace) = self.workspace_mut(team) else {
            return;
        };
        let users = workspace.conversations_arrived(list, complete);
        let open = workspace.active.clone();
        self.fetch_users(team, users);
        if active_team.as_deref() == Some(team)
            && let Some(open) = open
        {
            self.ensure_loaded(team, &open);
        }
    }

    /// A conversation is gone, and with it any thread open from it.
    fn conversation_gone(&mut self, team: &str, channel: &str) {
        if let Some(workspace) = self.workspace_mut(team) {
            workspace.conversation_gone(channel);
        }
        if self.active_team().as_deref() == Some(team)
            && self.thread.as_ref().is_some_and(|(c, _)| c == channel)
        {
            self.thread = None;
        }
    }

    fn history(
        &mut self,
        team: &str,
        channel: &str,
        messages: Vec<Message>,
        has_more: bool,
        cursor: Option<String>,
        older: bool,
    ) {
        let Some(workspace) = self.workspace_mut(team) else {
            return;
        };
        let (arrived, first) =
            workspace.history_arrived(channel, messages, has_more, cursor, older);
        if older {
            self.prepended = Some((format!("{team}/{channel}"), 0.0));
        }
        if first {
            self.scroll_to_bottom
                .insert(Self::draft_key(team, channel, None));
        }
        self.fetch_arrived(team, arrived);
        if !older {
            self.mark_if_viewing(team, channel);
        }
    }

    fn message(&mut self, team: &str, channel: &str, message: Message, changed: bool) {
        let viewing = self.is_viewing(team, channel);
        let note = if changed {
            None
        } else {
            self.note_for(team, channel, &message, viewing)
        };
        if !changed {
            self.run_hooks(team, channel, &message);
            crate::views::arrived(self, team, channel, &message);
        }
        let Some(workspace) = self.workspace_mut(team) else {
            return;
        };
        if changed {
            let arrived = workspace.message_changed(channel, message);
            self.fetch_arrived(team, arrived);
            if viewing {
                self.waker.wake();
            }
            return;
        }
        let from_me = message.user.as_deref() == Some(workspace.info.user_id.as_str());

        if let Some(user) = &message.user {
            workspace.people.stopped_typing(channel, user);
        }
        let (arrived, fetch_conversation) = workspace.message_arrived(channel, message, viewing);
        if fetch_conversation {
            self.backend.send(Command::FetchConversation {
                team: team.to_owned(),
                channel: channel.to_owned(),
            });
        }
        self.fetch_arrived(team, arrived);
        if viewing && !from_me {
            self.mark_if_viewing(team, channel);
        }
        if viewing {
            self.waker.wake();
        }
        if let Some(note) = note {
            self.notify(note);
        }
    }

    fn sent(&mut self, team: &str, channel: &str, local: &Ts, result: Result<Message, String>) {
        let Some(workspace) = self.workspace_mut(team) else {
            return;
        };
        workspace.sent(channel, local, &result);
        if let Err(error) = result {
            self.toast(tf("Message not sent: {error}", &[("error", &error)]), true);
        }
    }

    /// A message is gone; an open thread it started closes with it.
    pub(super) fn remove_message(&mut self, team: &str, channel: &str, ts: &Ts) {
        if let Some(workspace) = self.workspace_mut(team) {
            workspace.remove_message(channel, ts);
        }
        if self.active_team().as_deref() == Some(team)
            && self
                .thread
                .as_ref()
                .is_some_and(|(c, parent)| c == channel && parent == ts)
        {
            self.thread = None;
        }
    }

    fn fetch_arrived(&mut self, team: &str, arrived: Arrived) {
        self.fetch_users(team, arrived.users);
        self.fetch_bots(team, arrived.bots);
    }

    fn fetch_users(&mut self, team: &str, ids: Vec<String>) {
        if ids.is_empty() || self.demo {
            return;
        }
        if let Some(workspace) = self.workspace_mut(team) {
            workspace.requested_users.extend(ids.iter().cloned());
        }
        self.backend.send(Command::FetchUsers {
            team: team.to_owned(),
            ids,
        });
    }

    fn fetch_bots(&mut self, team: &str, ids: Vec<String>) {
        if ids.is_empty() || self.demo {
            return;
        }
        if let Some(workspace) = self.workspace_mut(team) {
            workspace.requested_bots.extend(ids.iter().cloned());
        }
        self.backend.send(Command::FetchBots {
            team: team.to_owned(),
            ids,
        });
    }
}
