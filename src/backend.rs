//! The bridge between the interface and Slack.
//!
//! A dedicated thread runs a small tokio runtime with the [`worker`]. The
//! interface sends [`Command`]s and never waits; the worker answers with
//! [`Event`]s and wakes the window for each one, so egui sleeps when
//! nothing happens.

mod api;
mod around;
mod blocks;
pub mod convos;
pub mod desktop;
mod fetch;
mod files;
pub mod huddles;
pub mod people;
mod poll;
mod search;
mod translate;
pub mod views;
pub mod worker;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, PoisonError, mpsc};

pub use fastframe_shell::Waker;
pub use files::UploadGate;

use crate::credentials::{AppCredentials, Credentials};
use crate::failure::{Failure, Problem};
use crate::images::ImageLoader;
use crate::model::{Bot, Conversation, Message, SidebarSection, Ts, User, UserGroup, Workspace};
use crate::paths::AppDirs;
use crate::settings::{Redirect, WorkspaceMeta};
use crate::sidebar::SidebarCall;

/// What the interface asks the worker to do.
pub enum Command {
    /// Saves the Slack app's credentials and restarts Socket Mode with them.
    SaveApp(AppCredentials),
    /// Starts OAuth in the browser, asking for `request`'s scopes.
    StartSignIn {
        redirect: Redirect,
        port: u16,
        request: crate::scopes::Request,
    },
    CancelSignIn,
    /// A redirect URL handed over by another launch.
    Callback(String),
    /// Signs in with a user token copied from the app's settings page.
    PasteToken(String),
    /// Signs in by reusing the browser session: the `d` cookie and a
    /// workspace URL.
    SignInSession {
        cookie: String,
        workspace_url: String,
    },
    /// Signs in with the `slack://` link Slack's browser sign-in hands over
    /// (see [`crate::slack::magic`]).
    SignInLink(String),
    /// Opens Slack's sign-in page in the browser and, for a while, accepts
    /// the `slack://` link it hands back through the desktop.
    StartBrowserSignIn,
    SignOut(String),
    /// The conversation on screen, for polling when Socket Mode is down.
    Focus {
        team: String,
        channel: Option<String>,
    },
    LoadHistory {
        team: String,
        channel: String,
    },
    LoadOlder {
        team: String,
        channel: String,
        cursor: String,
    },
    LoadThread {
        team: String,
        channel: String,
        ts: Ts,
    },
    /// The messages just before and after `ts`, to show it in context.
    LoadAround {
        team: String,
        channel: String,
        ts: Ts,
    },
    /// Page `page` of a search, answered as request `request`.
    Search {
        query: crate::search::Query,
        page: u32,
        request: u64,
    },
    /// The page of messages right after `after`.
    LoadNewer {
        team: String,
        channel: String,
        after: Ts,
    },
    /// Message `ts` of `channel` by itself, to quote under a link to it;
    /// `thread` is its parent for a reply. Answered by [`Event::Quoted`].
    FetchQuote {
        team: String,
        channel: String,
        ts: Ts,
        thread: Option<Ts>,
    },
    Send {
        team: String,
        channel: String,
        text: String,
        thread: Option<Ts>,
        broadcast: bool,
        local: Ts,
        /// The id of the optimistic copy, sent along so Slack's copy of
        /// the message can be told apart from another with the same text.
        client_msg_id: Option<String>,
    },
    /// Saves an edit already shown on screen.
    Edit {
        team: String,
        channel: String,
        ts: Ts,
        text: String,
        /// The message as it was before the edit, if it was loaded, to
        /// put back if Slack refuses.
        before: Option<Box<Message>>,
    },
    /// Deletes a message already taken off the screen.
    Delete {
        team: String,
        channel: String,
        ts: Ts,
        /// The message as it was, if it was loaded, to show again if
        /// Slack refuses.
        removed: Option<Box<Message>>,
    },
    /// Deletes your file `file` (`files.delete`), already hidden on
    /// screen; `name` is for telling you if Slack refuses.
    DeleteFile {
        team: String,
        file: String,
        name: String,
    },
    /// Adds custom emoji `name` with picture `image` (browser sessions
    /// only), answered with [`Event::EmojiAdded`].
    AddEmoji {
        team: String,
        name: String,
        image: Vec<u8>,
        file_name: String,
        mime: String,
    },
    /// Fetches the workspace's custom emoji again (`emoji.list`).
    FetchEmoji {
        team: String,
    },
    /// Adds or takes back your reaction, already toggled on screen.
    React {
        team: String,
        channel: String,
        ts: Ts,
        name: String,
        add: bool,
    },
    /// Uploads a file; its progress comes back as [`Event::UploadProgress`],
    /// [`Event::UploadFinishing`] and [`Event::UploadDone`] under `id`.
    Upload {
        id: u64,
        team: String,
        channel: String,
        thread: Option<Ts>,
        path: PathBuf,
        comment: String,
    },
    /// Runs a slash command (without its `/`) in `channel`; `text` is in
    /// wire form, mentions as `<@U1>`. Answered by [`Event::Slash`] under
    /// `id`.
    Slash {
        id: u64,
        team: String,
        channel: String,
        command: String,
        text: String,
    },
    /// Presses an app's interactive button, as Slack's web client does
    /// (browser sessions only). Answered by [`Event::Pressed`]; the app
    /// usually answers by changing the message, which arrives as an edit.
    PressButton {
        team: String,
        press: crate::model::Press,
    },
    /// Stops the upload `id` if Slack has not yet been told to share the
    /// file, answered by [`Event::UploadCancelled`]. Later it is ignored
    /// and the upload ends as usual with [`Event::UploadDone`].
    CancelUpload {
        id: u64,
    },
    Download {
        team: String,
        url: String,
        name: String,
    },
    /// Fetches a file into the workspace's private cache and opens it in
    /// the system's own app: videos and sounds, which play there.
    OpenFile {
        team: String,
        url: String,
        name: String,
    },
    Mark {
        team: String,
        channel: String,
        ts: Ts,
    },
    FetchUsers {
        team: String,
        ids: Vec<String>,
    },
    /// Changes your sidebar in Slack, then fetches it again.
    Sidebar {
        team: String,
        calls: Vec<SidebarCall>,
    },
    /// Names and icons of apps that posted without a username.
    FetchBots {
        team: String,
        ids: Vec<String>,
    },
    FetchConversation {
        team: String,
        channel: String,
    },
    /// Closes a direct message or group DM in Slack (`conversations.close`),
    /// as closing it in Slack's own sidebar does.
    CloseConversation {
        team: String,
        channel: String,
    },
    Reconnect,
    /// Switches every connection to this proxy setting and restarts the
    /// live ones.
    SetProxy(crate::slack::net::ProxySettings),
    /// Snoozes notifications in Slack for this many minutes, or with
    /// `None` ends the snooze.
    Snooze {
        team: String,
        minutes: Option<u32>,
    },
    /// Asks Slack for the Do Not Disturb state again.
    FetchDnd {
        team: String,
    },
    /// Mutes or unmutes a conversation in your Slack preferences (browser
    /// sessions). `all` is every muted conversation after the change, for
    /// the older preference that lists them.
    Mute {
        team: String,
        channel: String,
        muted: bool,
        all: Vec<String>,
    },
    /// Starts, finds or looks after a conversation (see [`crate::convos`]).
    Convos {
        team: String,
        command: crate::convos::Command,
    },
    /// Presence and the like for people (see [`crate::people`]).
    People {
        team: String,
        command: crate::people::Command,
    },
    /// Loads or changes what a view at the top of the sidebar lists (see
    /// [`crate::views`]).
    Views {
        team: String,
        command: crate::views::Command,
    },
}

/// Prints every field except the secrets: a pasted token, the session
/// cookie, and the sign-in redirect (its code finishes a sign-in).
impl std::fmt::Debug for Command {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        use crate::redact::REDACTED;
        match self {
            Self::SaveApp(app) => f.debug_tuple("SaveApp").field(app).finish(),
            Self::StartSignIn {
                redirect,
                port,
                request,
            } => f
                .debug_struct("StartSignIn")
                .field("redirect", redirect)
                .field("port", port)
                .field("request", request)
                .finish(),
            Self::CancelSignIn => f.write_str("CancelSignIn"),
            Self::Callback(_) => f.debug_tuple("Callback").field(&REDACTED).finish(),
            Self::PasteToken(_) => f.debug_tuple("PasteToken").field(&REDACTED).finish(),
            Self::SignInLink(_) => f.debug_tuple("SignInLink").field(&REDACTED).finish(),
            Self::StartBrowserSignIn => f.write_str("StartBrowserSignIn"),
            Self::SignInSession { workspace_url, .. } => f
                .debug_struct("SignInSession")
                .field("cookie", &REDACTED)
                .field("workspace_url", workspace_url)
                .finish(),
            Self::SignOut(team) => f.debug_tuple("SignOut").field(team).finish(),
            Self::Focus { team, channel } => f
                .debug_struct("Focus")
                .field("team", team)
                .field("channel", channel)
                .finish(),
            Self::LoadHistory { team, channel } => f
                .debug_struct("LoadHistory")
                .field("team", team)
                .field("channel", channel)
                .finish(),
            Self::LoadOlder {
                team,
                channel,
                cursor,
            } => f
                .debug_struct("LoadOlder")
                .field("team", team)
                .field("channel", channel)
                .field("cursor", cursor)
                .finish(),
            Self::LoadThread { team, channel, ts } => f
                .debug_struct("LoadThread")
                .field("team", team)
                .field("channel", channel)
                .field("ts", ts)
                .finish(),
            Self::LoadAround { team, channel, ts } => f
                .debug_struct("LoadAround")
                .field("team", team)
                .field("channel", channel)
                .field("ts", ts)
                .finish(),
            Self::Search {
                query,
                page,
                request,
            } => f
                .debug_struct("Search")
                .field("query", query)
                .field("page", page)
                .field("request", request)
                .finish(),
            Self::LoadNewer {
                team,
                channel,
                after,
            } => f
                .debug_struct("LoadNewer")
                .field("team", team)
                .field("channel", channel)
                .field("after", after)
                .finish(),
            Self::FetchQuote {
                team,
                channel,
                ts,
                thread,
            } => f
                .debug_struct("FetchQuote")
                .field("team", team)
                .field("channel", channel)
                .field("ts", ts)
                .field("thread", thread)
                .finish(),
            Self::Send {
                team,
                channel,
                text,
                thread,
                broadcast,
                local,
                client_msg_id,
            } => f
                .debug_struct("Send")
                .field("team", team)
                .field("channel", channel)
                .field("text", text)
                .field("thread", thread)
                .field("broadcast", broadcast)
                .field("local", local)
                .field("client_msg_id", client_msg_id)
                .finish(),
            Self::Edit {
                team,
                channel,
                ts,
                text,
                before,
            } => f
                .debug_struct("Edit")
                .field("team", team)
                .field("channel", channel)
                .field("ts", ts)
                .field("text", text)
                .field("before", &before.is_some())
                .finish(),
            Self::Delete {
                team,
                channel,
                ts,
                removed,
            } => f
                .debug_struct("Delete")
                .field("team", team)
                .field("channel", channel)
                .field("ts", ts)
                .field("removed", &removed.is_some())
                .finish(),
            Self::React {
                team,
                channel,
                ts,
                name,
                add,
            } => f
                .debug_struct("React")
                .field("team", team)
                .field("channel", channel)
                .field("ts", ts)
                .field("name", name)
                .field("add", add)
                .finish(),
            Self::Upload {
                id,
                team,
                channel,
                thread,
                path,
                comment,
            } => f
                .debug_struct("Upload")
                .field("id", id)
                .field("team", team)
                .field("channel", channel)
                .field("thread", thread)
                .field("path", path)
                .field("comment", comment)
                .finish(),
            Self::Slash {
                id,
                team,
                channel,
                command,
                text,
            } => f
                .debug_struct("Slash")
                .field("id", id)
                .field("team", team)
                .field("channel", channel)
                .field("command", command)
                .field("text", text)
                .finish(),
            Self::PressButton { team, press } => f
                .debug_struct("PressButton")
                .field("team", team)
                .field("press", press)
                .finish(),
            Self::CancelUpload { id } => f.debug_struct("CancelUpload").field("id", id).finish(),
            Self::Download { team, url, name } => f
                .debug_struct("Download")
                .field("team", team)
                .field("url", url)
                .field("name", name)
                .finish(),
            Self::OpenFile { team, url, name } => f
                .debug_struct("OpenFile")
                .field("team", team)
                .field("url", url)
                .field("name", name)
                .finish(),
            Self::Mark { team, channel, ts } => f
                .debug_struct("Mark")
                .field("team", team)
                .field("channel", channel)
                .field("ts", ts)
                .finish(),
            Self::FetchUsers { team, ids } => f
                .debug_struct("FetchUsers")
                .field("team", team)
                .field("ids", ids)
                .finish(),
            Self::Sidebar { team, calls } => f
                .debug_struct("Sidebar")
                .field("team", team)
                .field("calls", calls)
                .finish(),
            Self::FetchBots { team, ids } => f
                .debug_struct("FetchBots")
                .field("team", team)
                .field("ids", ids)
                .finish(),
            Self::FetchConversation { team, channel } => f
                .debug_struct("FetchConversation")
                .field("team", team)
                .field("channel", channel)
                .finish(),
            Self::CloseConversation { team, channel } => f
                .debug_struct("CloseConversation")
                .field("team", team)
                .field("channel", channel)
                .finish(),
            Self::DeleteFile { team, file, name } => f
                .debug_struct("DeleteFile")
                .field("team", team)
                .field("file", file)
                .field("name", name)
                .finish(),
            Self::AddEmoji {
                team,
                name,
                image,
                file_name,
                mime,
            } => f
                .debug_struct("AddEmoji")
                .field("team", team)
                .field("name", name)
                .field("image", &image.len())
                .field("file_name", file_name)
                .field("mime", mime)
                .finish(),
            Self::FetchEmoji { team } => f.debug_struct("FetchEmoji").field("team", team).finish(),
            Self::Reconnect => f.write_str("Reconnect"),
            Self::SetProxy(proxy) => f.debug_tuple("SetProxy").field(&proxy.mode).finish(),
            Self::Snooze { team, minutes } => f
                .debug_struct("Snooze")
                .field("team", team)
                .field("minutes", minutes)
                .finish(),
            Self::FetchDnd { team } => f.debug_struct("FetchDnd").field("team", team).finish(),
            Self::Mute {
                team,
                channel,
                muted,
                all,
            } => f
                .debug_struct("Mute")
                .field("team", team)
                .field("channel", channel)
                .field("muted", muted)
                .field("all", all)
                .finish(),
            Self::Convos { team, command } => f
                .debug_struct("Convos")
                .field("team", team)
                .field("command", command)
                .finish(),
            Self::People { team, command } => f
                .debug_struct("People")
                .field("team", team)
                .field("command", command)
                .finish(),
            Self::Views { team, command } => f
                .debug_struct("Views")
                .field("team", team)
                .field("command", command)
                .finish(),
        }
    }
}

/// A change to a message that the interface shows before Slack confirms
/// it, with what it takes to undo it if Slack refuses.
#[derive(Clone, Debug, PartialEq)]
pub enum Change {
    /// You edited `ts` to `text`; `before` is the message as it was.
    Edit {
        ts: Ts,
        text: String,
        before: Option<Box<Message>>,
    },
    /// You deleted `ts`; `removed` is the message as it was.
    Delete {
        ts: Ts,
        removed: Option<Box<Message>>,
    },
    /// You added (or took back) your reaction `name` on `ts`.
    React { ts: Ts, name: String, added: bool },
}

/// Where a sign-in stands.
#[derive(Clone, Debug, PartialEq)]
pub enum SignIn {
    /// The browser is open on this URL.
    Waiting(String),
    Exchanging,
    Failed(Failure),
    Done(String),
}

#[derive(Clone, Debug, PartialEq)]
pub enum Socket {
    /// No app-level token: messages arrive by polling the open conversation.
    Off,
    Connecting,
    Connected,
    /// The live connection dropped, for this reason; reconnecting.
    Disconnected(Failure),
    /// Slack refused the app-level token; reconnecting will not help.
    Rejected(Failure),
}

#[derive(Debug)]
pub enum Event {
    /// The keyring answered: the stored app, if any.
    AppLoaded(Option<AppCredentials>),
    /// The keyring failed to read or save a secret.
    KeyringError(crate::failure::Keyring),
    /// Slack would not authorize your Slack app with the newer scopes, so
    /// it was made from an older manifest; the sign-in starts again with
    /// the older set.
    OlderApp,
    SignIn(SignIn),
    /// A workspace is signed in (and these are its current details).
    WorkspaceReady(Workspace),
    /// A workspace has no working token any more.
    SignedOut {
        team: String,
        reason: Option<Failure>,
    },
    Conversations {
        team: String,
        list: Vec<Conversation>,
        complete: bool,
    },
    Conversation {
        team: String,
        conversation: Conversation,
    },
    ConversationGone {
        team: String,
        channel: String,
    },
    /// A direct message or group DM was opened or closed in your sidebar,
    /// perhaps in another Slack client.
    Opened {
        team: String,
        channel: String,
        open: bool,
    },
    Users {
        team: String,
        users: Vec<User>,
    },
    Bots {
        team: String,
        bots: Vec<Bot>,
    },
    /// Your sidebar sections, in your order. Only sessions get them; the
    /// sidebar falls back to Channels and Direct messages without.
    Sections {
        team: String,
        sections: Vec<SidebarSection>,
    },
    /// The workspace's custom emoji; `can_add` says whether this sign-in
    /// can add more (browser sessions, through the web client's call).
    Emoji {
        team: String,
        emoji: HashMap<String, String>,
        can_add: bool,
    },
    /// Slack answered [`Command::AddEmoji`].
    EmojiAdded {
        team: String,
        name: String,
        result: Result<(), Failure>,
    },
    /// The workspace's user groups, for mentioning them. Never sent when
    /// the sign-in may not list them.
    UserGroups {
        team: String,
        groups: Vec<UserGroup>,
    },
    History {
        team: String,
        channel: String,
        messages: Vec<Message>,
        has_more: bool,
        cursor: Option<String>,
        older: bool,
        /// Whether this is a poll of the newest page while the socket is
        /// down, whose new messages are announced as live ones would be.
        /// A first load or a reload is not: its messages are not news.
        polled: bool,
    },
    /// The newest page of a conversation as the offline cache kept it, to
    /// show until [`Event::History`] brings Slack's own.
    CachedHistory {
        team: String,
        channel: String,
        messages: Vec<Message>,
        has_more: bool,
        cursor: Option<String>,
    },
    HistoryFailed {
        team: String,
        channel: String,
        error: Failure,
    },
    /// The messages around `ts`, oldest first, which replace what the
    /// conversation's list held: the stretch asked for by
    /// [`Command::LoadAround`].
    Around {
        team: String,
        channel: String,
        ts: Ts,
        messages: Vec<Message>,
        /// Whether older messages exist, and the cursor for them.
        has_older: bool,
        cursor: Option<String>,
        has_newer: bool,
    },
    /// The message [`Command::FetchQuote`] asked for: it, none when it is
    /// not there (deleted), or why it could not be read.
    Quoted {
        team: String,
        channel: String,
        ts: Ts,
        result: Result<Option<Message>, Failure>,
    },
    /// A page of search results, or why there is none.
    Search {
        team: String,
        request: u64,
        result: Result<crate::search::Page, Failure>,
    },
    /// The page after the newest message loaded, oldest first.
    Newer {
        team: String,
        channel: String,
        messages: Vec<Message>,
        has_newer: bool,
    },
    Thread {
        team: String,
        channel: String,
        ts: Ts,
        messages: Vec<Message>,
    },
    /// A message, live.
    Message {
        team: String,
        channel: String,
        message: Message,
        /// A new copy of a message Slack already sent (an edit, or a
        /// parent's thread details), not a new one: it changes no counts
        /// and only replaces a loaded copy.
        changed: bool,
    },
    Deleted {
        team: String,
        channel: String,
        ts: Ts,
    },
    /// Slack answered [`Command::DeleteFile`]: `Ok` once the file is gone
    /// (or was already).
    FileDeleteSettled {
        team: String,
        file: String,
        name: String,
        result: Result<(), Failure>,
    },
    /// A file was deleted, here or anywhere (`file_deleted`).
    FileGone {
        team: String,
        file: String,
    },
    Reaction {
        team: String,
        channel: String,
        ts: Ts,
        name: String,
        user: String,
        added: bool,
    },
    Sent {
        team: String,
        channel: String,
        local: Ts,
        result: Result<Message, Failure>,
    },
    /// Slack answered an edit, delete or reaction. On an error the
    /// interface undoes the change it already showed.
    Settled {
        team: String,
        channel: String,
        change: Change,
        result: Result<(), Failure>,
    },
    /// Someone else read up to `ts` (you, on another device).
    Read {
        team: String,
        channel: String,
        ts: Ts,
    },
    /// What a poll found about a conversation while the socket is down:
    /// its newest message, your read marker and Slack's count of unread
    /// mentions, each when the poll learnt it. Markers only move forward,
    /// so a stale answer cannot undo anything newer.
    Activity {
        team: String,
        channel: String,
        latest: Option<Ts>,
        last_read: Option<Ts>,
        mentions: Option<u32>,
    },
    Socket(Socket),
    /// A `slack://` link the desktop handed over, for a signed-in
    /// workspace: open what it names.
    DeepLink(crate::links::Link),
    /// Something failed that has no event of its own: shown as a toast.
    Error(Problem),
    /// Something worth a calm toast.
    Notice(crate::notice::Notice),
    /// Your Do Not Disturb state in a workspace.
    Dnd {
        team: String,
        dnd: crate::dnd::Dnd,
    },
    /// Your notification preferences in a workspace (browser sessions).
    SlackPrefs {
        team: String,
        prefs: crate::desktop::SlackPrefs,
    },
    /// `sent` of the `total` bytes of upload `id` are on their way.
    UploadProgress {
        id: u64,
        sent: u64,
        total: u64,
    },
    /// A slash command ran (`Ok`, with Slack's reply if it gave one) or
    /// failed. [`Failure::NeedsSession`] says it can only run through a
    /// browser session's sign-in.
    Slash {
        id: u64,
        command: String,
        result: Result<Option<String>, Failure>,
    },
    /// Slack took a button press (`Ok`) or refused it. Taking it only
    /// means the app was told; what the app does comes as message edits.
    Pressed {
        team: String,
        press: crate::model::Press,
        result: Result<(), Failure>,
    },
    /// Upload `id` has all its bytes up and Slack is being told to share
    /// it. That can't be taken back, so it can no longer be cancelled.
    UploadFinishing {
        id: u64,
    },
    /// Upload `id` ended: `shared`, or failed with its own error event
    /// sent first.
    UploadDone {
        id: u64,
        shared: bool,
    },
    /// Upload `id` was stopped before Slack was told to share it, so
    /// nothing was posted.
    UploadCancelled {
        id: u64,
    },
    /// An answer about starting, finding or looking after a conversation
    /// (see [`crate::convos`]).
    Convos {
        team: String,
        event: crate::convos::Event,
    },
    /// News about people: presence and the like (see [`crate::people`]).
    People {
        team: String,
        event: crate::people::Event,
    },
    /// An answer for a view at the top of the sidebar (see
    /// [`crate::views`]).
    Views {
        team: String,
        event: crate::views::Event,
    },
}

/// The interface's end of the bridge.
pub struct Backend {
    commands: tokio::sync::mpsc::UnboundedSender<Command>,
    events: mpsc::Receiver<Event>,
    pub images: ImageLoader,
}

impl Backend {
    pub fn send(&self, command: Command) {
        if self.commands.send(command).is_err() {
            log::error!("the Slack worker has stopped");
        }
    }

    pub fn try_recv(&self) -> Option<Event> {
        self.events.try_recv().ok()
    }
}

/// Sends events to the interface and wakes it.
#[derive(Clone)]
pub struct Sink {
    sender: mpsc::Sender<Event>,
    waker: Waker,
    /// For one workspace's tasks: whether the workspace is still signed
    /// in. Closed, the sink drops everything.
    gate: Option<Arc<Mutex<bool>>>,
}

impl Sink {
    pub fn send(&self, event: Event) {
        match &self.gate {
            Some(gate) => {
                // The lock is held across the send, so once `close` has
                // returned no event from this sink can still arrive.
                let open = gate.lock().unwrap_or_else(PoisonError::into_inner);
                if !*open {
                    return;
                }
                let _ = self.sender.send(event);
            }
            None => {
                let _ = self.sender.send(event);
            }
        }
        self.waker.wake();
    }

    /// A sink for one workspace's tasks, and the gate that silences it when
    /// the workspace signs out. A task that is still running then cannot
    /// bring the workspace back with a late event.
    pub fn gated(&self) -> (Sink, Gate) {
        let gate = Arc::new(Mutex::new(true));
        let sink = Sink {
            sender: self.sender.clone(),
            waker: self.waker.clone(),
            gate: Some(gate.clone()),
        };
        (sink, Gate(gate))
    }
}

/// Silences the sinks made with it by [`Sink::gated`].
#[derive(Debug)]
pub struct Gate(Arc<Mutex<bool>>);

impl Gate {
    /// Drops every later event from the gated sinks. Events already sent
    /// stay sent.
    pub fn close(&self) {
        *self.0.lock().unwrap_or_else(PoisonError::into_inner) = false;
    }
}

/// Where the worker gets its data.
pub enum Source {
    Slack {
        dirs: AppDirs,
        workspaces: Vec<WorkspaceMeta>,
        credentials_in_memory: bool,
    },
    #[cfg(feature = "demo")]
    Demo,
}

/// Starts the runtime thread and the worker.
pub fn spawn(waker: &Waker, source: Source, cache_dir: PathBuf) -> Backend {
    let (commands, receiver) = tokio::sync::mpsc::unbounded_channel();
    let (sender, events) = mpsc::channel();
    let sink = Sink {
        sender,
        waker: waker.clone(),
        gate: None,
    };
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .thread_name("noslacking-runtime")
        .enable_all()
        .build();
    // Without a runtime there is no app to run.
    let runtime =
        runtime.unwrap_or_else(|error| panic!("could not start the network runtime: {error}"));
    let images = ImageLoader::new(runtime.handle().clone(), cache_dir);
    let worker_images = images.clone();
    let handle = runtime.handle().clone();
    let spawned = std::thread::Builder::new()
        .name("noslacking-backend".into())
        .spawn(move || {
            runtime.block_on(async move {
                match source {
                    Source::Slack {
                        dirs,
                        workspaces,
                        credentials_in_memory,
                    } => {
                        let credentials = if credentials_in_memory {
                            Credentials::memory()
                        } else {
                            Credentials::native(Some(handle))
                        };
                        worker::Worker::new(credentials, dirs, sink, worker_images)
                            .run(workspaces, receiver)
                            .await;
                    }
                    #[cfg(feature = "demo")]
                    Source::Demo => crate::demo::run(sink, receiver).await,
                }
            });
        });
    if let Err(error) = spawned {
        log::error!("could not start the backend thread: {error}");
    }
    Backend {
        commands,
        events,
        images,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn commands_never_print_secrets() {
        let printed = format!(
            "{:?} {:?} {:?} {:?}",
            Command::PasteToken("xoxp-secret".into()),
            Command::SignInSession {
                cookie: "xoxd-secret".into(),
                workspace_url: "https://acme.slack.com".into(),
            },
            Command::Callback("noslacking://oauth/callback?code=c0de&state=s".into()),
            Command::SaveApp(AppCredentials {
                client_id: "1.2".into(),
                client_secret: "hush".into(),
                app_token: "xapp-secret".into(),
            }),
        );
        for secret in ["xoxp-secret", "xoxd-secret", "c0de", "hush", "xapp-secret"] {
            assert!(!printed.contains(secret), "{printed}");
        }
        assert!(printed.contains("acme.slack.com"), "{printed}");
        assert_eq!(
            format!("{:?}", Command::SignOut("T1".into())),
            r#"SignOut("T1")"#
        );
    }
}
