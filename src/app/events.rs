//! How the worker's events change the app: sign-in, workspaces, and
//! the conversations, messages and people that arrive.
//!
//! A second `impl App`, kept apart so `app.rs` stays readable. What an
//! event changes in one workspace lives in [`super::workspace`]; this
//! side adds what needs the backend: fetching, toasts and scrolling.

use super::workspace::{Arrived, SendOutcome};
use super::{App, Page, WorkspaceState};
use crate::backend::{Change, Command, Event, SignIn, Socket};
use crate::credentials::AppCredentials;
use crate::failure::Failure;
use crate::i18n::{t, tf};
use crate::model::{Conversation, Message, Ts, Workspace};
use crate::settings::WorkspaceMeta;

/// The most notifications one poll shows for one conversation: the newest
/// messages it found. Hooks and the views still see every one.
const POLLED_NOTES: usize = 3;

impl App {
    pub(super) fn handle(&mut self, event: Event) {
        match event {
            // The account: the app, the keyring, sign-in and the socket.
            Event::AppLoaded(app) => self.app_loaded(app),
            Event::KeyringError(error) => {
                self.toast(tf("Keyring: {error}", &[("error", &error.message())]), true);
                self.keyring_error = Some(error);
            }
            Event::OlderApp => {
                self.settings.older_app = true;
                self.settings_changed();
                self.toast(t("Your Slack app was made from an older manifest, so it is asked only for the permissions it has. Update it to unlock Do Not Disturb, @group mentions and bookmark editing.").into_owned(), false);
            }
            Event::SignIn(state) => self.sign_in_changed(state),
            Event::WorkspaceReady(info) => self.workspace_ready(info),
            Event::SignedOut { team, reason } => self.signed_out(&team, reason),
            Event::Socket(socket) => self.socket_changed(socket),
            Event::Error(problem) => self.toast(problem.message(), problem.is_error()),
            Event::UploadProgress { id, sent, total } => {
                if let Some(upload) = self.transfers.iter_mut().find(|u| u.id == id) {
                    upload.sent = sent;
                    upload.total = total;
                }
            }
            Event::UploadFinishing { id } => {
                if let Some(upload) = self.transfers.iter_mut().find(|u| u.id == id) {
                    upload.finishing = true;
                }
            }
            Event::UploadDone { id, shared } => {
                self.upload_done(id, shared);
            }
            // Said only once the worker has really stopped it, so the
            // toast never claims a cancel for a file that was posted.
            Event::UploadCancelled { id } => {
                if self.upload_done(id, false) {
                    self.toast(t("Upload cancelled").into_owned(), false);
                }
            }
            Event::Slash {
                id,
                command,
                result,
            } => self.slash_done(id, &command, result),
            Event::Pressed {
                team,
                press,
                result,
            } => {
                use crate::model::PressKind;
                if let Some(workspace) = self.workspace_mut(&team) {
                    workspace.pressing.remove(&press);
                    // A select or radio buttons keep showing what was
                    // chosen; an overflow menu is only a list of actions.
                    if result.is_ok()
                        && matches!(press.kind, PressKind::Select { .. } | PressKind::Radio)
                    {
                        workspace.choose(press.clone());
                    }
                }
                // Taken: the app answers by changing the message, or by
                // opening a form, which only Slack itself can show.
                if let Err(error) = result {
                    let label = crate::mrkdwn::unescape(&press.text);
                    let error = error.message();
                    let message = if press.kind == PressKind::Button {
                        tf(
                            "Could not press {button}: {error}",
                            &[("button", &label), ("error", &error)],
                        )
                    } else {
                        tf(
                            "Could not choose {choice}: {error}",
                            &[("choice", &label), ("error", &error)],
                        )
                    };
                    self.toast(message, true);
                }
            }
            Event::Notice(notice) => self.toast(notice.message(), false),
            Event::AudioFetched { id, result } => self.audio_fetched(id, result),
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
            Event::Opened {
                team,
                channel,
                open,
            } => {
                let unknown = self
                    .workspace_mut(&team)
                    .is_some_and(|w| w.opened(&channel, open));
                if unknown {
                    self.backend
                        .send(Command::FetchConversation { team, channel });
                }
            }
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
            Event::Emoji {
                team,
                emoji,
                can_add,
            } => {
                if let Some(workspace) = self.workspace_mut(&team) {
                    workspace.emoji_arrived(emoji, can_add);
                }
            }
            Event::EmojiChanged { team, change } => {
                if let Some(workspace) = self.workspace_mut(&team) {
                    workspace.emoji_changed(&change);
                }
            }
            Event::EmojiAdded { team, name, result } => self.emoji_added(&team, name, result),
            Event::UserGroups { team, groups } => {
                if let Some(workspace) = self.workspace_mut(&team) {
                    workspace.groups = groups;
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
                polled,
            } => {
                // Measured before the page joins what is loaded.
                let (fresh, unopened) = self
                    .workspace_mut(&team)
                    .map(|w| {
                        let fresh = w.polled_new(&channel, &messages, older, polled);
                        (fresh, polled && !older && w.unopened(&channel))
                    })
                    .unwrap_or_default();
                if unopened {
                    // A poll of a conversation nobody opened only says what
                    // is new there; opening it loads it properly.
                    if let Some(workspace) = self.workspace_mut(&team) {
                        workspace.polled_unopened(&channel, &messages);
                    }
                } else {
                    self.history(&team, &channel, messages, has_more, cursor, older);
                }
                self.announce_polled(&team, &channel, &fresh);
            }
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
                    tf(
                        "Could not load messages: {error}",
                        &[("error", &error.message())],
                    ),
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
            Event::Quoted {
                team,
                channel,
                ts,
                result,
            } => {
                if let Err(error) = &result {
                    log::debug!("could not quote {} in {channel}: {error:?}", ts.as_str());
                }
                if let Some(workspace) = self.workspace_mut(&team) {
                    let arrived = workspace.quote_arrived(&channel, &ts, result);
                    self.fetch_arrived(&team, arrived);
                }
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
            Event::FileDeleteSettled {
                team,
                file,
                name,
                result,
            } => self.file_delete_settled(&team, &file, &name, result),
            Event::FileGone { team, file } => {
                if let Some(workspace) = self.workspace_mut(&team) {
                    workspace.file_gone(&file);
                }
            }
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
            Event::Activity {
                team,
                channel,
                latest,
                last_read,
                mentions,
            } => {
                let unknown = self
                    .workspace_mut(&team)
                    .is_some_and(|w| w.activity(&channel, latest, last_read, mentions));
                if unknown {
                    self.backend
                        .send(Command::FetchConversation { team, channel });
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
    fn settled(&mut self, team: &str, channel: &str, change: Change, result: Result<(), Failure>) {
        let Err(error) = result else { return };
        let what = match &change {
            Change::Edit { .. } => t("Could not edit the message"),
            Change::Delete { .. } => t("Could not delete the message"),
            Change::React { .. } => t("Could not change the reaction"),
        };
        if let Some(workspace) = self.workspace_mut(team) {
            workspace.undo(channel, change);
        }
        self.toast(format!("{what}: {}", error.message()), true);
    }

    /// Slack answered the deletion of your file; a refused one shows
    /// again, and you are told.
    fn file_delete_settled(
        &mut self,
        team: &str,
        file: &str,
        name: &str,
        result: Result<(), Failure>,
    ) {
        if let Some(workspace) = self.workspace_mut(team) {
            workspace.file_delete_settled(file, result.is_ok());
        }
        if let Err(error) = result {
            self.toast(
                tf(
                    "Could not delete {name}: {error}",
                    &[("name", name), ("error", &error.message())],
                ),
                true,
            );
        }
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
            scopes: info.scopes.clone(),
        });
        // An app sign-in that got every scope shows the app is up to date,
        // whatever an earlier refusal said.
        if info.sign_in == crate::model::SignInKind::App
            && info.scopes.as_ref().is_some_and(|s| s.lacking().is_empty())
            && self.settings.older_app
        {
            self.settings.older_app = false;
            self.settings_changed();
        }
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
        crate::views::warm_threads(self, &team);
        if self.settings.active_workspace.is_none() {
            self.settings.active_workspace = Some(team);
        }
        self.save_settings();
    }

    /// A workspace needs signing in again (`reason`), or was signed out.
    fn signed_out(&mut self, team: &str, reason: Option<Failure>) {
        // Nothing can be joined or declined there any more.
        self.huddles.invites.forget(team);
        match reason {
            Some(reason) => {
                if let Some(workspace) = self.workspace_mut(team) {
                    workspace.signed_out = Some(reason);
                }
            }
            None => {
                let was_active = self.active_team().as_deref() == Some(team);
                self.forget_team(team);
                self.workspaces.retain(|w| w.info.team_id != team);
                self.settings.remove_workspace(team);
                self.save_settings();
                if self.workspaces.is_empty() {
                    self.page = Page::SignIn;
                } else if was_active && let Some(next) = self.active_team() {
                    self.select_workspace(next);
                }
            }
        }
    }

    /// Drops what the interface holds for a workspace signed out of: what
    /// you were writing there goes with the sign-in, and what is open of it
    /// closes, so nothing (a reply in its thread, say) goes to another.
    fn forget_team(&mut self, team: &str) {
        let prefix = format!("{team}/");
        let ours = |key: &str| key.starts_with(&prefix);
        self.drafts.retain(|key, _| !ours(key));
        self.jumps.retain(|j| !ours(&j.list));
        self.scroll_to_bottom.retain(|key| !ours(key));
        if self.read_line.as_ref().is_some_and(|(key, _)| ours(key)) {
            self.read_line = None;
        }
        if self.prepended.as_ref().is_some_and(|(key, _)| ours(key)) {
            self.prepended = None;
        }
        self.marks.retain(|(t, _), _| t != team);
        self.pending_marks.retain(|(t, _), _| t != team);
        self.popouts.retain(|p| p.team != team);
        self.views.teams.remove(team);
        self.convos.data.retain(|(t, _), _| t != team);
        self.audio_signed_out(team);
        if self.active_team().as_deref() == Some(team) {
            // What is open names the workspace on screen.
            self.thread = None;
            self.editing = None;
            self.selected = None;
            self.confirm_delete = None;
            self.confirm_press = None;
            self.confirm_delete_file = None;
            self.add_emoji = None;
            self.picker = None;
            self.share = None;
            self.views.open = None;
            self.convos.details = None;
        }
    }

    fn socket_changed(&mut self, socket: Socket) {
        if let Socket::Rejected(reason) = &socket {
            self.toast(
                tf(
                    "Slack refused the app-level token ({reason})",
                    &[("reason", &reason.message())],
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

    /// Treats the new messages a poll found (see
    /// [`WorkspaceState::polled_new`]) as live ones are: hooks, the views'
    /// lists, and notifications. Only the newest few notify, so a socket
    /// down for a while does not end in a pile of notifications.
    fn announce_polled(&mut self, team: &str, channel: &str, fresh: &[Message]) {
        if fresh.is_empty() {
            return;
        }
        let viewing = self.is_viewing(team, channel);
        let mut notes = Vec::new();
        for message in fresh {
            if !self
                .workspace_mut(team)
                .is_some_and(|w| w.first_sight(channel, &message.ts))
            {
                continue;
            }
            notes.extend(self.note_for(team, channel, message, viewing));
            if let Some(workspace) = self.workspace_mut(team) {
                workspace.count_polled(channel, message, viewing);
            }
            self.run_hooks(team, channel, message);
            crate::views::arrived(self, team, channel, message);
        }
        let older = notes.len().saturating_sub(POLLED_NOTES);
        for note in notes.into_iter().skip(older) {
            self.notify(note);
        }
    }

    fn message(&mut self, team: &str, channel: &str, message: Message, changed: bool) {
        // The echo of a message you deleted while it was sending.
        if self
            .workspace_mut(team)
            .is_some_and(|w| w.is_suppressed(channel, &message.ts) || w.is_cancelled_echo(&message))
        {
            // Settled by the workspace, so its send is still deleted.
            if let Some(workspace) = self.workspace_mut(team) {
                workspace.message_arrived(channel, message, false);
            }
            return;
        }
        let viewing = self.is_viewing(team, channel);
        // A message a poll announced already stays quiet when its live copy
        // comes too.
        let fresh = !changed
            && self
                .workspace_mut(team)
                .is_none_or(|w| w.first_sight(channel, &message.ts));
        let note = if fresh {
            self.note_for(team, channel, &message, viewing)
        } else {
            None
        };
        if fresh {
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

    fn sent(&mut self, team: &str, channel: &str, local: &Ts, result: Result<Message, Failure>) {
        let Some(workspace) = self.workspace_mut(team) else {
            return;
        };
        match workspace.sent(channel, local, &result) {
            // Deleted while it was sending: take it back now it is posted.
            SendOutcome::Cancelled { delete } => {
                if let Some(ts) = delete {
                    self.backend.send(Command::Delete {
                        team: team.to_owned(),
                        channel: channel.to_owned(),
                        ts,
                        removed: None,
                    });
                }
                return;
            }
            SendOutcome::Posted => return,
            SendOutcome::Settled => {}
        }
        if let Err(error) = result {
            self.toast(
                tf("Message not sent: {error}", &[("error", &error.message())]),
                true,
            );
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
