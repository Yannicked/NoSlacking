//! The worker: owns every workspace's API client, the Socket Mode
//! connection and sign-in, turns commands into API calls and API answers
//! and events into [`Event`]s.
//!
//! Its loop only dispatches. Anything that waits on the network runs as a
//! task of its own and reports back through [`Internal`] or straight to the
//! interface, so one slow call never holds up another.

use std::collections::{HashMap, HashSet};
use std::time::Duration;

use serde_json::Value;
use tokio::sync::{mpsc, watch};

use super::{Change, Command, Event, Gate, SignIn, Sink, Socket};
use crate::auth::{self, Flow, SignedIn};
use crate::credentials::{AppCredentials, Credentials};
use crate::images::ImageLoader;
use crate::model::{Conversation, ConversationKind, Message, Ts, User, Workspace};
use crate::offline::Cache;
use crate::paths::AppDirs;
use crate::settings::{Redirect, WorkspaceMeta};
use crate::slack::magic::TeamResult;
use crate::slack::socket::{self, SocketEvent};
use crate::slack::{Client, SlackError, Token, types};

const HISTORY_PAGE: u32 = 50;
/// How often the open conversation is polled while Socket Mode is down.
const POLL_EVERY: Duration = Duration::from_secs(6);
/// The most pages read from each listing. Each is far beyond what a
/// workspace normally has; they only stop a cursor that never ends, and
/// hitting one is logged.
/// `users.list`, 200 people a page.
const USER_PAGES: usize = 40;
/// `users.conversations`, 200 conversations a page.
const CONVERSATION_PAGES: usize = 100;
/// `conversations.replies`, 200 replies a page.
const THREAD_PAGES: usize = 50;
/// `users.channelSections.list`.
const SECTION_PAGES: usize = 10;
/// `stars.list`, 200 stars a page.
const STAR_PAGES: usize = 20;
const MAX_UPLOAD: u64 = 1024 * 1024 * 1024;
/// How long after "Sign in with your browser" a handed-over link is used.
const BROWSER_SIGN_IN_WINDOW: Duration = Duration::from_secs(15 * 60);
/// Why a command for a workspace the worker does not have cannot run.
const NOT_SIGNED_IN: &str = "that workspace is not signed in";

/// One workspace's saved sign-in, as read from the keyring at start-up.
enum Stored {
    Token(Token),
    Missing,
    /// The keyring failed while reading this one.
    Failed(crate::credentials::Error),
    /// Not read, because the keyring had already failed: asking again would
    /// only repeat the failure, or the unlock prompt.
    Skipped,
}

/// What tasks report back to the loop.
enum Internal {
    /// The keyring answered at start-up: the app, and each workspace's
    /// sign-in, in the order of the settings.
    Loaded {
        app: Result<Option<AppCredentials>, crate::credentials::Error>,
        workspaces: Vec<(WorkspaceMeta, Stored)>,
        /// The offline cache's key, unless the keyring failed.
        cache_key: Option<crate::offline::CacheKey>,
    },
    Callback(String),
    SignedIn(Result<SignedIn, String>),
    TeamAdded {
        meta: Workspace,
        token: Token,
    },
    /// From the Socket Mode task started as `generation`.
    Socket {
        generation: u64,
        event: SocketEvent,
    },
    /// From the RTM task started for `team` as `generation`.
    Rtm {
        team: String,
        generation: u64,
        event: crate::slack::rtm::RtmEvent,
    },
    SignInListenerFailed(String),
    /// These people and apps could not be fetched for a passing reason;
    /// the next request for them should try again.
    FetchFailed {
        team: String,
        users: Vec<String>,
        bots: Vec<String>,
    },
}

struct Team {
    client: Client,
    user_id: String,
    /// What this workspace's tasks report through. Signing out closes
    /// `gate`, so nothing they send afterwards reaches the interface.
    sink: Sink,
    gate: Gate,
    /// The start-up work (lists, people, sections, the unread sweep),
    /// stopped on sign-out rather than left calling Slack for nothing.
    boot: tokio::task::AbortHandle,
}

impl Team {
    /// Stops everything still running for this workspace.
    fn shut(&self) {
        self.gate.close();
        self.boot.abort();
    }
}

/// A message to post, as `Command::Send` carries it.
struct Outgoing {
    team: String,
    channel: String,
    text: String,
    thread: Option<Ts>,
    broadcast: bool,
    /// The interface's id for its optimistic copy.
    local: Ts,
}

/// A real-time socket the worker started, and what it last reported.
///
/// Each start gets a fresh `generation`. A socket that was replaced can
/// still report on its way out; its reports carry the old generation and
/// are ignored, so they cannot mark the new socket down or remove it.
struct Live {
    stop: watch::Sender<bool>,
    generation: u64,
    status: Socket,
}

pub struct Worker {
    /// The encrypted offline cache, once the keyring gave its key.
    cache: Cache,
    credentials: Credentials,
    dirs: AppDirs,
    sink: Sink,
    images: ImageLoader,
    app: Option<AppCredentials>,
    teams: HashMap<String, Team>,
    flow: Option<Flow>,
    /// When the user started a browser sign-in: until it is
    /// [`BROWSER_SIGN_IN_WINDOW`] old, a `slack://` sign-in link handed over
    /// by the desktop is used. Any other time such a link is not ours to act
    /// on, so it is ignored.
    browser_sign_in: Option<std::time::Instant>,
    listener: Option<tokio::task::JoinHandle<()>>,
    /// The Socket Mode connection, which serves every workspace signed in
    /// through the app.
    socket: Option<Live>,
    /// Per-session-workspace RTM sockets.
    rtm: HashMap<String, Live>,
    /// The generation the next socket gets.
    next_generation: u64,
    /// The workspace on screen, and its open conversation if any.
    focus: Option<(String, Option<String>)>,
    /// The status last sent to the interface, so it hears only changes.
    reported: Option<Socket>,
    /// The poll of the open conversation that is still running, if any.
    polling: Option<tokio::task::JoinHandle<()>>,
    users_requested: HashSet<(String, String)>,
    bots_requested: HashSet<(String, String)>,
    internal: mpsc::UnboundedSender<Internal>,
    internal_rx: Option<mpsc::UnboundedReceiver<Internal>>,
    /// Commands that arrived before the keyring answered at start-up. They
    /// wait, so a command for a saved workspace is not refused only
    /// because its token is still being read. `None` once started.
    waiting: Option<Vec<Command>>,
    /// Uploads still running, by the interface's id, so they can be
    /// cancelled.
    uploads: HashMap<u64, tokio::task::AbortHandle>,
}

impl Worker {
    pub fn new(credentials: Credentials, dirs: AppDirs, sink: Sink, images: ImageLoader) -> Self {
        let (internal, internal_rx) = mpsc::unbounded_channel();
        Self {
            cache: Cache::disabled(),
            credentials,
            dirs,
            sink,
            images,
            app: None,
            teams: HashMap::new(),
            flow: None,
            browser_sign_in: None,
            listener: None,
            socket: None,
            rtm: HashMap::new(),
            next_generation: 0,
            focus: None,
            reported: None,
            polling: None,
            users_requested: HashSet::new(),
            bots_requested: HashSet::new(),
            internal,
            internal_rx: Some(internal_rx),
            waiting: Some(Vec::new()),
            uploads: HashMap::new(),
        }
    }

    pub async fn run(
        mut self,
        workspaces: Vec<WorkspaceMeta>,
        mut commands: mpsc::UnboundedReceiver<Command>,
    ) {
        let Some(mut internal) = self.internal_rx.take() else {
            return;
        };
        self.start(workspaces);
        let mut poll = tokio::time::interval(POLL_EVERY);
        poll.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                command = commands.recv() => match command {
                    Some(command) => match &mut self.waiting {
                        Some(waiting) => waiting.push(command),
                        None => self.command(command),
                    },
                    None => break,
                },
                Some(message) = internal.recv() => self.internal(message),
                _ = poll.tick() => self.poll(),
            }
        }
        if let Some(live) = self.socket.take() {
            let _ = live.stop.send(true);
        }
        for (_, live) in self.rtm.drain() {
            let _ = live.stop.send(true);
        }
    }

    /// Reads the saved app and sign-ins in a task of its own: the keyring
    /// can sit behind an unlock prompt for as long as the user leaves it.
    fn start(&mut self, workspaces: Vec<WorkspaceMeta>) {
        let credentials = self.credentials.clone();
        let internal = self.internal.clone();
        tokio::spawn(async move {
            let app = credentials.load_app().await;
            let mut failed = false;
            let mut stored = Vec::with_capacity(workspaces.len());
            for meta in workspaces {
                let token = if failed {
                    Stored::Skipped
                } else {
                    match credentials.load_token(&meta.team_id).await {
                        Ok(Some(token)) => Stored::Token(token),
                        Ok(None) => Stored::Missing,
                        Err(error) => {
                            failed = true;
                            Stored::Failed(error)
                        }
                    }
                };
                stored.push((meta, token));
            }
            let cache_key = if failed {
                None
            } else {
                match credentials.cache_key().await {
                    Ok(key) => Some(key),
                    Err(error) => {
                        log::warn!("no offline cache: {error}");
                        None
                    }
                }
            };
            let _ = internal.send(Internal::Loaded {
                app,
                workspaces: stored,
                cache_key,
            });
        });
    }

    /// Opens what the keyring held, then the commands that waited for it.
    fn loaded(
        &mut self,
        app: Result<Option<AppCredentials>, crate::credentials::Error>,
        workspaces: Vec<(WorkspaceMeta, Stored)>,
    ) {
        match app {
            Ok(app) => {
                self.app = app.clone();
                self.sink.send(Event::AppLoaded(app));
            }
            Err(error) => {
                self.sink.send(Event::AppLoaded(None));
                self.sink.send(Event::KeyringError(error.to_string()));
            }
        }
        for (meta, stored) in workspaces {
            let reason = match stored {
                Stored::Token(token) => {
                    let workspace = Workspace {
                        team_id: meta.team_id,
                        name: meta.name,
                        domain: meta.domain,
                        icon: meta.icon,
                        user_id: meta.user_id,
                    };
                    self.add_team(workspace, token);
                    continue;
                }
                Stored::Missing => "No saved sign-in for this workspace.".to_owned(),
                Stored::Failed(error) => {
                    self.sink.send(Event::KeyringError(error.to_string()));
                    format!("Could not read this workspace's sign-in: {error}.")
                }
                Stored::Skipped => {
                    "Could not read this workspace's sign-in: the keyring failed.".to_owned()
                }
            };
            // Every workspace gets an answer, so none is left waiting.
            self.sink.send(Event::SignedOut {
                team: meta.team_id,
                reason: Some(reason),
            });
        }
        self.restart_socket();
        for command in self.waiting.take().unwrap_or_default() {
            self.command(command);
        }
    }

    /// The gated sink of a signed-in workspace, or a closed one.
    fn sink_for(&self, team: &str) -> Sink {
        match self.teams.get(team) {
            Some(t) => t.sink.clone(),
            None => {
                let (sink, gate) = self.sink.gated();
                gate.close();
                sink
            }
        }
    }

    /// A workspace's client and the sink for its tasks.
    fn team(&self, team: &str) -> Option<(Client, Sink)> {
        self.teams
            .get(team)
            .map(|t| (t.client.clone(), t.sink.clone()))
    }

    fn make_client(&self, team: &str, token: Token, sink: Sink) -> Client {
        let credentials = self.credentials.clone();
        let team = team.to_owned();
        Client::shared(token).with_refresh(
            self.app.as_ref().and_then(AppCredentials::oauth),
            move |result| {
                let credentials = credentials.clone();
                let sink = sink.clone();
                let team = team.clone();
                // The client waits for this before its next refresh, so
                // the newest token is always the one saved last.
                async move {
                    match result {
                        Ok(token) => {
                            if let Err(error) = credentials.save_token(&team, &token).await {
                                log::warn!("could not store the renewed token: {error}");
                            }
                        }
                        Err(error) if error.is_auth() => sink.send(Event::SignedOut {
                            team,
                            reason: Some(describe(&error)),
                        }),
                        Err(_) => {}
                    }
                }
            },
        )
    }

    /// Starts using a signed-in workspace.
    fn add_team(&mut self, workspace: Workspace, token: Token) {
        let (sink, gate) = self.sink.gated();
        let client = self.make_client(&workspace.team_id, token, sink.clone());
        self.images.set_client(&workspace.team_id, client.clone());
        let session = client.token().is_session();
        self.sink.send(Event::WorkspaceReady(workspace.clone()));
        let boot = tokio::spawn(boot(
            client.clone(),
            workspace.clone(),
            self.cache.clone(),
            sink.clone(),
        ))
        .abort_handle();
        let replaced = self.teams.insert(
            workspace.team_id.clone(),
            Team {
                client: client.clone(),
                user_id: workspace.user_id.clone(),
                sink,
                gate,
                boot,
            },
        );
        tokio::spawn(super::desktop::dnd_info(
            client.clone(),
            workspace.team_id.clone(),
            self.sink_for(&workspace.team_id),
        ));
        if session {
            tokio::spawn(super::desktop::prefs(
                client.clone(),
                workspace.team_id.clone(),
                self.sink_for(&workspace.team_id),
            ));
        }
        // Signing in again replaces the old sign-in and its tasks.
        if let Some(old) = replaced {
            old.shut();
        }
        if session {
            self.start_rtm(&workspace.team_id, client);
        }
        self.report_socket();
    }

    /// Opens (or reopens) the RTM socket for a session workspace.
    fn start_rtm(&mut self, team: &str, client: Client) {
        if let Some(old) = self.rtm.remove(team) {
            let _ = old.stop.send(true);
        }
        let (stop, stopped) = watch::channel(false);
        let generation = self.generation();
        self.rtm.insert(
            team.to_owned(),
            Live {
                stop,
                generation,
                status: Socket::Connecting,
            },
        );
        self.report_socket();
        let internal = self.internal.clone();
        let team = team.to_owned();
        tokio::spawn(crate::slack::rtm::run(
            client,
            move |event| {
                let _ = internal.send(Internal::Rtm {
                    team: team.clone(),
                    generation,
                    event,
                });
            },
            stopped,
        ));
    }

    fn generation(&mut self) -> u64 {
        self.next_generation += 1;
        self.next_generation
    }

    fn restart_socket(&mut self) {
        if let Some(old) = self.socket.take() {
            let _ = old.stop.send(true);
        }
        let token = self
            .app
            .as_ref()
            .map(|app| app.app_token.trim().to_owned())
            .unwrap_or_default();
        if token.is_empty() || self.teams.is_empty() {
            self.report_socket();
            return;
        }
        let (stop, stopped) = watch::channel(false);
        let generation = self.generation();
        self.socket = Some(Live {
            stop,
            generation,
            status: Socket::Connecting,
        });
        self.report_socket();
        let internal = self.internal.clone();
        tokio::spawn(socket::run(
            crate::slack::net::api(),
            token,
            move |event| {
                let _ = internal.send(Internal::Socket { generation, event });
            },
            stopped,
        ));
    }

    fn is_session(&self, team: &str) -> bool {
        self.teams
            .get(team)
            .is_some_and(|t| t.client.token().is_session())
    }

    /// The real-time status of one workspace: its own RTM socket for a
    /// browser session, the shared Socket Mode connection otherwise.
    fn status(&self, team: &str) -> Socket {
        if self.is_session(team) {
            return self
                .rtm
                .get(team)
                .map_or(Socket::Off, |live| live.status.clone());
        }
        self.socket
            .as_ref()
            .map_or(Socket::Off, |live| live.status.clone())
    }

    /// Whether events for `team` arrive live, so polling it is not needed.
    fn is_live(&self, team: &str) -> bool {
        self.status(team) == Socket::Connected
    }

    /// Tells the interface the status of the workspace on screen, when it
    /// changed. The interface shows one status, and the one that matters
    /// is the one for what you are looking at.
    fn report_socket(&mut self) {
        let team = self
            .focus
            .as_ref()
            .map(|(team, _)| team.clone())
            .filter(|team| self.teams.contains_key(team))
            .or_else(|| self.teams.keys().min().cloned());
        let status = team.map_or(Socket::Off, |team| self.status(&team));
        if self.reported.as_ref() != Some(&status) {
            self.reported = Some(status.clone());
            self.sink.send(Event::Socket(status));
        }
    }

    /// Dispatches one command to its handler. Nothing here waits on the
    /// network: each handler starts a task and returns.
    fn command(&mut self, command: Command) {
        match command {
            Command::SaveApp(app) => self.save_app(app),
            Command::StartSignIn { redirect, port } => self.start_sign_in(redirect, port),
            Command::CancelSignIn => self.cancel_sign_in(),
            Command::Callback(url) => self.callback(url),
            Command::PasteToken(token) => self.paste_token(token),
            Command::SignInSession {
                cookie,
                workspace_url,
            } => self.sign_in_session(cookie, &workspace_url),
            Command::SignInLink(link) => self.sign_in_link(&link),
            Command::StartBrowserSignIn => self.start_browser_sign_in(),
            Command::SignOut(team) => self.sign_out(&team),
            Command::Focus { team, channel } => {
                self.focus = Some((team, channel));
                self.report_socket();
            }
            Command::LoadHistory { team, channel } => self.load_history(team, channel, None),
            Command::LoadOlder {
                team,
                channel,
                cursor,
            } => self.load_history(team, channel, Some(cursor)),
            Command::LoadThread { team, channel, ts } => self.load_thread(team, channel, ts),
            Command::LoadAround { team, channel, ts } => match self.team(&team) {
                Some((client, sink)) => {
                    tokio::spawn(super::around::around(client, team, channel, ts, sink));
                }
                None => self.history_unavailable(team, channel),
            },
            Command::Search {
                query,
                page,
                request,
            } => match self.team(&query.team) {
                Some((client, sink)) => {
                    tokio::spawn(super::search::search(client, query, page, request, sink));
                }
                None => self.sink.send(Event::Search {
                    team: query.team,
                    request,
                    result: Err(crate::search::Failure::Other(NOT_SIGNED_IN.to_owned())),
                }),
            },
            Command::LoadNewer {
                team,
                channel,
                after,
            } => match self.team(&team) {
                Some((client, sink)) => {
                    tokio::spawn(super::around::newer(client, team, channel, after, sink));
                }
                None => self.history_unavailable(team, channel),
            },
            Command::Send {
                team,
                channel,
                text,
                thread,
                broadcast,
                local,
            } => self.send(Outgoing {
                team,
                channel,
                text,
                thread,
                broadcast,
                local,
            }),
            Command::Edit {
                team,
                channel,
                ts,
                text,
                before,
            } => self.change(team, channel, Change::Edit { ts, text, before }),
            Command::Delete {
                team,
                channel,
                ts,
                removed,
            } => self.change(team, channel, Change::Delete { ts, removed }),
            Command::React {
                team,
                channel,
                ts,
                name,
                add,
            } => self.change(
                team,
                channel,
                Change::React {
                    ts,
                    name,
                    added: add,
                },
            ),
            Command::Upload {
                id,
                team,
                channel,
                thread,
                path,
                comment,
            } => self.upload(id, team, channel, thread, path, comment),
            Command::Slash {
                team,
                channel,
                command,
                text,
            } => self.slash(team, channel, command, text),
            Command::CancelUpload { id } => {
                if let Some(task) = self.uploads.remove(&id) {
                    task.abort();
                }
                self.sink.send(Event::UploadDone { id });
            }
            Command::Download { team, url, name } => self.download(&team, url, name),
            Command::Mark { team, channel, ts } => self.mark(&team, channel, ts),
            Command::FetchUsers { team, ids } => self.fetch_users(team, ids),
            Command::FetchBots { team, ids } => self.fetch_bots(team, ids),
            Command::Sidebar { team, calls } => self.edit_sidebar(team, calls),
            Command::FetchConversation { team, channel } => {
                if let Some((client, sink)) = self.team(&team) {
                    tokio::spawn(conversation_info(client, team, channel, sink));
                } else {
                    log::debug!("not fetching {channel} in {team}: signed out");
                }
            }
            Command::Reconnect => self.reconnect(),
            Command::SetProxy(proxy) => match crate::slack::net::configure(&proxy) {
                // New clients only help once the sockets reconnect on them.
                Ok(()) => self.reconnect(),
                Err(error) => self
                    .sink
                    .send(Event::Error(format!("Could not use the proxy: {error}"))),
            },
            Command::Snooze { team, minutes } => {
                if let Some((client, sink)) = self.team(&team) {
                    tokio::spawn(super::desktop::snooze(client, team, minutes, sink));
                }
            }
            Command::Mute {
                team,
                channel,
                muted,
                all,
            } => {
                if let Some((client, sink)) = self.team(&team) {
                    tokio::spawn(super::desktop::mute(
                        client, team, channel, muted, all, sink,
                    ));
                }
            }
            Command::FetchDnd { team } => {
                if let Some((client, sink)) = self.team(&team) {
                    tokio::spawn(super::desktop::dnd_info(client, team, sink));
                }
            }
            Command::Convos { team, command } => match self.team(&team) {
                Some((client, sink)) => {
                    tokio::spawn(super::convos::run(client, team, command, sink));
                }
                None => self.sink.send(Event::Convos {
                    team,
                    event: crate::convos::Event::Failed {
                        what: command.failure(),
                        error: NOT_SIGNED_IN.to_owned(),
                    },
                }),
            },
        }
    }

    fn save_app(&mut self, app: AppCredentials) {
        // Saved in the background; the new app is used at once.
        let credentials = self.credentials.clone();
        let sink = self.sink.clone();
        let saved = app.clone();
        tokio::spawn(async move {
            if let Err(error) = credentials.save_app(&saved).await {
                sink.send(Event::KeyringError(error.to_string()));
            }
        });
        // Clients pick up the new client secret for refreshes. They keep
        // their token and refresh lock, which every clone shares, so a
        // refresh in flight cannot race a second one.
        let oauth = app.oauth();
        for team in self.teams.values() {
            team.client.set_app(oauth.clone());
        }
        self.app = Some(app);
        self.restart_socket();
    }

    fn cancel_sign_in(&mut self) {
        self.flow = None;
        if let Some(listener) = self.listener.take() {
            listener.abort();
        }
    }

    fn paste_token(&mut self, token: String) {
        let http = crate::slack::net::api();
        let internal = self.internal.clone();
        self.sink.send(Event::SignIn(SignIn::Exchanging));
        tokio::spawn(async move {
            let result = validate(&http, Token::plain(token.trim())).await;
            let _ = internal.send(Internal::SignedIn(result));
        });
    }

    fn sign_in_session(&mut self, cookie: String, workspace_url: &str) {
        let Some(workspace_url) = crate::slack::session::normalize_workspace(workspace_url) else {
            self.sink.send(Event::SignIn(SignIn::Failed(
                "Enter your workspace's Slack address, such as acme.slack.com.".into(),
            )));
            return;
        };
        let internal = self.internal.clone();
        self.sink.send(Event::SignIn(SignIn::Exchanging));
        tokio::spawn(async move {
            let result = crate::slack::session::derive(cookie.trim(), &workspace_url)
                .await
                .map(|signed| SignedIn {
                    team_id: signed.team_id,
                    user_id: signed.user_id,
                    token: signed.token,
                })
                .map_err(|e| describe(&e));
            let _ = internal.send(Internal::SignedIn(result));
        });
    }

    /// Opens Slack's sign-in page in the browser and starts
    /// accepting the link it hands back.
    fn start_browser_sign_in(&mut self) {
        self.browser_sign_in = Some(std::time::Instant::now());
        if let Err(error) = open::that_detached(crate::slack::magic::SIGN_IN_URL) {
            log::warn!("could not open the browser: {error}");
            self.sink.send(Event::SignIn(SignIn::Failed(
                "Could not open the browser. Open app.slack.com/ssb/signin yourself.".into(),
            )));
        }
    }

    /// Whether a browser sign-in the user started is still waiting for its
    /// link.
    fn browser_sign_in_pending(&self) -> bool {
        self.browser_sign_in
            .is_some_and(|started| started.elapsed() < BROWSER_SIGN_IN_WINDOW)
    }

    /// Signs in to every workspace a pasted `slack://` sign-in link names:
    /// redeems its tokens for the account's session cookie, then signs in to
    /// each team with that cookie like [`Self::sign_in_session`].
    fn sign_in_link(&mut self, link: &str) {
        let Some(sets) = crate::slack::magic::parse_link(link) else {
            self.sink.send(Event::SignIn(SignIn::Failed(
                "That is not a Slack sign-in link. Copy the slack:// link the browser offers to open."
                    .into(),
            )));
            return;
        };
        let internal = self.internal.clone();
        let sink = self.sink.clone();
        self.sink.send(Event::SignIn(SignIn::Exchanging));
        tokio::spawn(async move {
            let mut signed = 0;
            let mut problems = Vec::new();
            for set in sets {
                let redeemed = match crate::slack::magic::redeem(&set).await {
                    Ok(redeemed) => redeemed,
                    Err(error) => {
                        problems.push(describe(&error));
                        continue;
                    }
                };
                for team in redeemed.teams {
                    match team {
                        TeamResult::SignedIn { url } => {
                            let Some(cookie) = redeemed.cookie.as_deref() else {
                                problems.push("Slack signed in but set no session cookie".into());
                                continue;
                            };
                            let result = crate::slack::session::derive(cookie, &url)
                                .await
                                .map(|session| SignedIn {
                                    team_id: session.team_id,
                                    user_id: session.user_id,
                                    token: session.token,
                                })
                                .map_err(|e| describe(&e));
                            match result {
                                Ok(session) => {
                                    signed += 1;
                                    let _ = internal.send(Internal::SignedIn(Ok(session)));
                                }
                                Err(error) => problems.push(error),
                            }
                        }
                        // SSO and similar: the workspace wants the browser
                        // again; its page then offers a new link to paste.
                        TeamResult::Browser { url } => {
                            if crate::mrkdwn::is_openable(&url)
                                && let Err(error) = open::that_detached(&url)
                            {
                                log::warn!("could not open the browser: {error}");
                            }
                            sink.send(Event::Notice(
                                "A workspace needs one more step in the browser; paste the new link it offers."
                                    .into(),
                            ));
                        }
                        TeamResult::Failed { reason } => problems.push(reason),
                    }
                }
            }
            if signed == 0 {
                let error = if problems.is_empty() {
                    "Slack signed in to no workspace with that link.".to_owned()
                } else {
                    format!("Could not sign in: {}", problems.join("; "))
                };
                let _ = internal.send(Internal::SignedIn(Err(error)));
            } else if !problems.is_empty() {
                log::warn!("some workspaces did not sign in: {}", problems.join("; "));
            }
        });
    }

    /// The newest page of history, or the one before `cursor`.
    fn load_history(&self, team: String, channel: String, cursor: Option<String>) {
        if let Some((client, sink)) = self.team(&team) {
            tokio::spawn(history(
                client,
                team,
                channel,
                cursor,
                self.cache.clone(),
                sink,
            ));
        } else {
            self.history_unavailable(team, channel);
        }
    }

    fn load_thread(&self, team: String, channel: String, ts: Ts) {
        if let Some((client, sink)) = self.team(&team) {
            tokio::spawn(thread(client, team, channel, ts, sink));
        } else {
            self.not_signed_in("load the thread");
        }
    }

    /// Posts a message; the answer settles the interface's optimistic copy.
    fn send(&self, outgoing: Outgoing) {
        let Some((client, sink)) = self.team(&outgoing.team) else {
            // Fail the optimistic message, or it stays pending.
            self.sink.send(Event::Sent {
                team: outgoing.team,
                channel: outgoing.channel,
                local: outgoing.local,
                result: Err(NOT_SIGNED_IN.to_owned()),
            });
            return;
        };
        tokio::spawn(async move {
            let Outgoing {
                team,
                channel,
                text,
                thread,
                broadcast,
                local,
            } = outgoing;
            let mut params = vec![("channel", channel.clone()), ("text", text)];
            if let Some(thread) = &thread {
                params.push(("thread_ts", thread.0.clone()));
                if broadcast {
                    params.push(("reply_broadcast", "true".into()));
                }
            }
            let result = client
                .act::<types::Posted>("chat.postMessage", &params)
                .await
                .map_err(|e| describe(&e))
                .and_then(|posted| {
                    let mut message = posted
                        .message
                        .and_then(types::Message::into_model)
                        .ok_or_else(|| "Slack did not return the message".to_owned())?;
                    if message.ts.as_str().is_empty() {
                        message.ts = Ts::new(posted.ts);
                    }
                    Ok(message)
                });
            sink.send(Event::Sent {
                team,
                channel,
                local,
                result,
            });
        });
    }

    /// Saves an edit, delete or reaction the interface already shows,
    /// and always answers with [`Event::Settled`] so a refused change can
    /// be undone, even for a workspace that is not signed in.
    fn change(&self, team: String, channel: String, change: Change) {
        let Some((client, sink)) = self.team(&team) else {
            self.sink.send(Event::Settled {
                team,
                channel,
                change,
                result: Err(NOT_SIGNED_IN.to_owned()),
            });
            return;
        };
        tokio::spawn(async move {
            let (method, params, ignore) = request(&channel, &change);
            let result = match client.act::<Value>(method, &params).await {
                Ok(_) => Ok(()),
                // Already as asked: nothing to undo.
                Err(SlackError::Api(code)) if ignore.contains(&code.as_str()) => Ok(()),
                Err(error) => Err(describe(&error)),
            };
            sink.send(Event::Settled {
                team,
                channel,
                change,
                result,
            });
        });
    }

    fn upload(
        &mut self,
        id: u64,
        team: String,
        channel: String,
        thread: Option<Ts>,
        path: std::path::PathBuf,
        comment: String,
    ) {
        let Some((client, sink)) = self.team(&team) else {
            self.not_signed_in("upload the file");
            self.sink.send(Event::UploadDone { id });
            return;
        };
        let poll_after = !self.is_live(&team);
        self.uploads.retain(|_, task| !task.is_finished());
        let task = tokio::spawn(async move {
            upload(
                id, client, team, channel, thread, path, comment, poll_after, &sink,
            )
            .await;
            sink.send(Event::UploadDone { id });
        });
        self.uploads.insert(id, task.abort_handle());
    }

    /// Runs a slash command: through its own Web API method where it has
    /// one, so it works with any sign-in, and otherwise through
    /// `chat.command`, Slack's own runner, which only sessions may call.
    fn slash(&self, team: String, channel: String, command: String, text: String) {
        let Some((client, sink)) = self.team(&team) else {
            self.sink.send(Event::Slash {
                command,
                result: Err(NOT_SIGNED_IN.to_owned()),
            });
            return;
        };
        tokio::spawn(async move {
            let result = run_slash(&client, &channel, &command, &text).await;
            sink.send(Event::Slash { command, result });
        });
    }

    fn download(&self, team: &str, url: String, name: String) {
        let Some((client, sink)) = self.team(team) else {
            self.not_signed_in(&format!("download {name}"));
            return;
        };
        tokio::spawn(async move {
            match download(&client, &url, &name).await {
                Ok(path) => sink.send(Event::Notice(format!("Saved {}", path.display()))),
                Err(error) => sink.send(Event::Error(error)),
            }
        });
    }

    /// Moves your read marker. The interface sends this on its own as you
    /// read; a workspace that is signed out has nothing to mark, and
    /// saying so on every click would only be noise.
    fn mark(&self, team: &str, channel: String, ts: Ts) {
        if !self.teams.contains_key(team) {
            log::debug!("not marking read in {team}: signed out");
            return;
        }
        self.act(
            team,
            "conversations.mark",
            vec![("channel", channel), ("ts", ts.0)],
            &["not_in_channel", "channel_not_found"],
        );
    }

    fn edit_sidebar(&self, team: String, calls: Vec<crate::sidebar::SidebarCall>) {
        if let Some((client, sink)) = self.team(&team) {
            tokio::spawn(edit_sidebar(client, team, calls, sink));
        } else {
            self.not_signed_in("change the sidebar");
        }
    }

    /// Opens every socket afresh and lists every workspace's conversations
    /// again, to catch up on anything missed while offline.
    fn reconnect(&mut self) {
        self.restart_socket();
        let session_teams: Vec<(String, Client)> = self
            .teams
            .iter()
            .filter(|(_, team)| team.client.token().is_session())
            .map(|(id, team)| (id.clone(), team.client.clone()))
            .collect();
        for (id, client) in session_teams {
            self.start_rtm(&id, client);
        }
        for (id, team) in &self.teams {
            tokio::spawn(conversations(
                team.client.clone(),
                id.clone(),
                self.cache.clone(),
                team.sink.clone(),
            ));
        }
    }

    /// Calls a method for its effect, reporting failures (except `ignore`d
    /// codes) as errors.
    fn act(
        &self,
        team: &str,
        method: &'static str,
        params: Vec<(&'static str, String)>,
        ignore: &'static [&'static str],
    ) {
        let Some((client, sink)) = self.team(team) else {
            self.sink
                .send(Event::Error(format!("{method} failed: {NOT_SIGNED_IN}")));
            return;
        };
        tokio::spawn(async move {
            match client.act::<Value>(method, &params).await {
                Ok(_) => {}
                Err(SlackError::Api(code)) if ignore.contains(&code.as_str()) => {}
                Err(error) => sink.send(Event::Error(format!(
                    "{method} failed: {}",
                    describe(&error)
                ))),
            }
        });
    }

    /// Says that `what` cannot be done because the workspace is not signed
    /// in here, rather than dropping the command without a word.
    fn not_signed_in(&self, what: &str) {
        self.sink
            .send(Event::Error(format!("Could not {what}: {NOT_SIGNED_IN}")));
    }

    /// Ends a history load for a workspace that is not signed in, so the
    /// conversation does not show as loading for ever.
    fn history_unavailable(&self, team: String, channel: String) {
        self.sink.send(Event::HistoryFailed {
            team,
            channel,
            error: NOT_SIGNED_IN.to_owned(),
        });
    }

    fn fetch_users(&mut self, team: String, ids: Vec<String>) {
        let Some((client, sink)) = self.team(&team) else {
            log::debug!("not fetching people in {team}: signed out");
            return;
        };
        let ids: Vec<String> = ids
            .into_iter()
            .filter(|id| self.users_requested.insert((team.clone(), id.clone())))
            .collect();
        if ids.is_empty() {
            return;
        }
        let internal = self.internal.clone();
        tokio::spawn(async move {
            let mut users = Vec::new();
            let mut retry = Vec::new();
            for id in ids {
                match client
                    .call::<types::UserInfo>("users.info", &[("user", id.clone())])
                    .await
                {
                    Ok(info) => users.push(info.user.into_model()),
                    Err(error) => {
                        log::debug!("users.info {id}: {error}");
                        if worth_retrying(&error) {
                            retry.push(id);
                        }
                    }
                }
            }
            if !retry.is_empty() {
                let _ = internal.send(Internal::FetchFailed {
                    team: team.clone(),
                    users: retry,
                    bots: Vec::new(),
                });
            }
            if !users.is_empty() {
                sink.send(Event::Users { team, users });
            }
        });
    }

    fn fetch_bots(&mut self, team: String, ids: Vec<String>) {
        let Some((client, sink)) = self.team(&team) else {
            log::debug!("not fetching apps in {team}: signed out");
            return;
        };
        let ids: Vec<String> = ids
            .into_iter()
            .filter(|id| self.bots_requested.insert((team.clone(), id.clone())))
            .collect();
        if ids.is_empty() {
            return;
        }
        let internal = self.internal.clone();
        tokio::spawn(async move {
            let mut bots = Vec::new();
            let mut retry = Vec::new();
            for id in ids {
                match client
                    .call::<types::BotInfo>("bots.info", &[("bot", id.clone())])
                    .await
                {
                    Ok(info) => {
                        let mut bot = info.bot.into_model();
                        if bot.id.is_empty() {
                            bot.id = id;
                        }
                        bots.push(bot);
                    }
                    Err(error) => {
                        log::debug!("bots.info {id}: {error}");
                        if worth_retrying(&error) {
                            retry.push(id);
                        }
                    }
                }
            }
            if !retry.is_empty() {
                let _ = internal.send(Internal::FetchFailed {
                    team: team.clone(),
                    users: Vec::new(),
                    bots: retry,
                });
            }
            if !bots.is_empty() {
                sink.send(Event::Bots { team, bots });
            }
        });
    }

    fn start_sign_in(&mut self, redirect: Redirect, port: u16) {
        let Some(app) = self.app.clone().filter(AppCredentials::can_sign_in) else {
            self.sink.send(Event::SignIn(SignIn::Failed(
                "Enter the Slack app's client ID first.".into(),
            )));
            return;
        };
        if let Some(listener) = self.listener.take() {
            listener.abort();
        }
        let flow = Flow::start(&app, redirect, port);
        match redirect {
            Redirect::Scheme => {
                if let Err(error) = auth::register_scheme() {
                    log::warn!("could not register noslacking:// links: {error}");
                    self.sink.send(Event::Error(format!(
                        "Could not register noslacking:// links ({error}). Try the loopback redirect in Settings."
                    )));
                }
            }
            Redirect::Loopback => {
                let internal = self.internal.clone();
                let state = flow.state.clone();
                self.listener = Some(tokio::spawn(async move {
                    match auth::loopback(port, &state).await {
                        Ok(url) => {
                            let _ = internal.send(Internal::Callback(url));
                        }
                        Err(error) => {
                            let _ =
                                internal.send(Internal::SignInListenerFailed(error.to_string()));
                        }
                    }
                }));
            }
        }
        if let Err(error) = open::that_detached(&flow.url) {
            log::warn!("could not open the browser: {error}");
        }
        self.sink
            .send(Event::SignIn(SignIn::Waiting(flow.url.clone())));
        self.flow = Some(flow);
    }

    fn callback(&mut self, url: String) {
        if url.starts_with("slack:") {
            // Only the link of a sign-in the user started here counts; any
            // other slack:// link handed over is not a sign-in for us.
            if self.browser_sign_in_pending() && crate::slack::magic::parse_link(&url).is_some() {
                self.browser_sign_in = None;
                self.sign_in_link(&url);
            } else if let Some(link) = crate::links::parse_deep(&url).filter(|link| {
                link.team
                    .as_ref()
                    .is_some_and(|t| self.teams.contains_key(t))
            }) {
                // A link to a conversation of a workspace signed in here.
                self.sink.send(Event::DeepLink(link));
            } else {
                log::info!("ignoring a slack:// link with no browser sign-in in progress");
            }
            return;
        }
        let Some(flow) = self.flow.clone() else {
            log::info!("ignoring a sign-in link with no sign-in in progress");
            return;
        };
        let code = match auth::parse_callback(&url, &flow.state) {
            Ok(code) => code,
            Err(error) => {
                self.sink.send(Event::SignIn(SignIn::Failed(error)));
                return;
            }
        };
        self.flow = None;
        let Some(app) = self.app.clone() else {
            return;
        };
        self.sink.send(Event::SignIn(SignIn::Exchanging));
        let http = crate::slack::net::api();
        let internal = self.internal.clone();
        tokio::spawn(async move {
            let result = auth::exchange(&http, &app, &flow, &code)
                .await
                .map_err(|e| describe(&e));
            let _ = internal.send(Internal::SignedIn(result));
        });
    }

    /// Removes the unencrypted lists older builds kept for `team`.
    fn remove_plain_cache(&self, team: &str) {
        for path in [
            self.dirs.users_cache(team),
            self.dirs.conversations_cache(team),
        ] {
            match std::fs::remove_file(&path) {
                Ok(()) => log::info!("removed the unencrypted {}", path.display()),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => log::warn!("could not remove {}: {error}", path.display()),
            }
        }
    }

    fn sign_out(&mut self, team: &str) {
        if let Some(live) = self.rtm.remove(team) {
            let _ = live.stop.send(true);
        }
        if let Some(removed) = self.teams.remove(team) {
            // Before SignedOut goes out: nothing from a task still running
            // for this workspace can follow it and bring the workspace back.
            removed.shut();
            let credentials = self.credentials.clone();
            let team = team.to_owned();
            // A session token belongs to the browser login; revoking it would
            // sign the browser out too, so only OAuth tokens are revoked.
            let revoke = !removed.client.token().is_session();
            tokio::spawn(async move {
                if revoke && let Err(error) = removed.client.act::<Value>("auth.revoke", &[]).await
                {
                    log::info!("auth.revoke: {error}");
                }
                if let Err(error) = credentials.delete_token(&team).await {
                    log::warn!("could not delete the token: {error}");
                }
            });
        }
        self.images.remove_client(team);
        // Nothing read in the workspace stays on disk after signing out.
        self.cache.wipe(team);
        self.remove_plain_cache(team);
        // A later sign-in to the same workspace fetches everyone afresh.
        self.users_requested.retain(|(t, _)| t != team);
        self.bots_requested.retain(|(t, _)| t != team);
        self.sink.send(Event::SignedOut {
            team: team.to_owned(),
            reason: None,
        });
        if self.teams.is_empty() {
            self.restart_socket();
        }
        self.report_socket();
    }

    fn internal(&mut self, message: Internal) {
        match message {
            Internal::Loaded {
                app,
                workspaces,
                cache_key,
            } => {
                if let Some(key) = cache_key {
                    self.cache = Cache::new(self.dirs.offline(), &key);
                }
                for (meta, _) in &workspaces {
                    self.remove_plain_cache(&meta.team_id);
                }
                self.loaded(app, workspaces);
            }
            // Signed out since: its entries are gone already.
            Internal::FetchFailed { team, .. } if !self.teams.contains_key(&team) => {}
            Internal::FetchFailed { team, users, bots } => {
                for id in users {
                    self.users_requested.remove(&(team.clone(), id));
                }
                for id in bots {
                    self.bots_requested.remove(&(team.clone(), id));
                }
            }
            Internal::Callback(url) => self.callback(url),
            Internal::SignInListenerFailed(error) => {
                self.sink.send(Event::SignIn(SignIn::Failed(format!(
                    "Could not listen for Slack's redirect: {error}. Is the port in use?"
                ))));
            }
            Internal::SignedIn(Err(error)) => self.sink.send(Event::SignIn(SignIn::Failed(error))),
            Internal::SignedIn(Ok(signed)) => {
                let http = crate::slack::net::api();
                let credentials = self.credentials.clone();
                let internal = self.internal.clone();
                let sink = self.sink.clone();
                tokio::spawn(async move {
                    if let Err(error) = credentials.save_token(&signed.team_id, &signed.token).await
                    {
                        sink.send(Event::KeyringError(error.to_string()));
                    }
                    let client = Client::new(http, signed.token.clone());
                    let meta = workspace_details(&client, &signed.team_id, &signed.user_id).await;
                    let _ = internal.send(Internal::TeamAdded {
                        meta,
                        token: signed.token,
                    });
                });
            }
            Internal::TeamAdded { meta, token } => {
                let name = meta.name.clone();
                let had_socket = self.socket.is_some();
                self.add_team(meta, token);
                self.sink.send(Event::SignIn(SignIn::Done(name)));
                if !had_socket {
                    self.restart_socket();
                }
            }
            Internal::Socket { generation, event } => {
                if self.socket.as_ref().map(|live| live.generation) == Some(generation) {
                    self.socket_event(event);
                } else {
                    log::debug!("ignoring a report from a replaced Socket Mode connection");
                }
            }
            Internal::Rtm {
                team,
                generation,
                event,
            } => {
                if self.rtm.get(&team).map(|live| live.generation) == Some(generation) {
                    self.rtm_event(&team, event);
                } else {
                    log::debug!("ignoring a report from a replaced RTM socket for {team}");
                }
            }
        }
    }

    fn socket_event(&mut self, event: SocketEvent) {
        let status = match event {
            SocketEvent::Connected => Socket::Connected,
            SocketEvent::Disconnected(reason) => Socket::Disconnected(reason),
            SocketEvent::Rejected(reason) => Socket::Rejected(reason),
            SocketEvent::Event { team, event } => {
                self.dispatch_event(&team, &event);
                return;
            }
        };
        if let Some(live) = &mut self.socket {
            live.status = status;
        }
        self.report_socket();
    }

    fn rtm_event(&mut self, team: &str, event: crate::slack::rtm::RtmEvent) {
        use crate::slack::rtm::RtmEvent;
        let status = match event {
            RtmEvent::Connected => Socket::Connected,
            RtmEvent::Disconnected(reason) => Socket::Disconnected(reason),
            RtmEvent::Unavailable(reason) => {
                // Slack will not give this session a socket. Not an outage:
                // poll the open conversation and say so calmly.
                log::info!("RTM unavailable for {team}, polling instead: {reason}");
                self.rtm.remove(team);
                self.report_socket();
                return;
            }
            RtmEvent::Event(event) => {
                self.dispatch_event(team, &event);
                return;
            }
        };
        if let Some(live) = self.rtm.get_mut(team) {
            live.status = status;
        }
        self.report_socket();
    }

    /// Routes one real-time event (from Socket Mode or RTM) to the interface.
    fn dispatch_event(&mut self, team: &str, event: &serde_json::Value) {
        let Some(me) = self.teams.get(team).map(|t| t.user_id.clone()) else {
            log::debug!("event for a workspace not signed in here");
            return;
        };
        for translated in translate(team, &me, event) {
            match translated {
                Translated::Event(event) => self.sink.send(event),
                Translated::Refresh(channel) => {
                    if let Some((client, sink)) = self.team(team) {
                        tokio::spawn(conversation_info(client, team.to_owned(), channel, sink));
                    }
                }
                Translated::RefreshSections => {
                    if let Some((client, sink)) = self.team(team) {
                        tokio::spawn(sections(client, team.to_owned(), sink));
                    }
                }
                Translated::RefreshPrefs => {
                    if let Some((client, sink)) = self.team(team)
                        && client.token().is_session()
                    {
                        tokio::spawn(super::desktop::prefs(client, team.to_owned(), sink));
                    }
                }
            }
        }
    }

    /// Without a live socket for its workspace, the open conversation is
    /// fetched again now and then, so new messages still show up.
    ///
    /// Only one poll runs at a time: under a rate limit one call can take
    /// longer than the poll interval, and stacking more on top would only
    /// deepen the limit.
    fn poll(&mut self) {
        if self
            .polling
            .as_ref()
            .is_some_and(|task| !task.is_finished())
        {
            return;
        }
        let Some((team, Some(channel))) = &self.focus else {
            return;
        };
        if self.is_live(team) {
            return;
        }
        if let Some((client, sink)) = self.team(team) {
            self.polling = Some(tokio::spawn(history(
                client,
                team.clone(),
                channel.clone(),
                None,
                Cache::disabled(),
                sink,
            )));
        }
    }
}

/// A user-facing description of an API failure.
/// The Web API call that makes a [`Change`], and the error codes that
/// mean it is already made.
fn request(
    channel: &str,
    change: &Change,
) -> (
    &'static str,
    Vec<(&'static str, String)>,
    &'static [&'static str],
) {
    let channel = ("channel", channel.to_owned());
    match change {
        Change::Edit { ts, text, .. } => (
            "chat.update",
            vec![channel, ("ts", ts.0.clone()), ("text", text.clone())],
            &[],
        ),
        Change::Delete { ts, .. } => (
            "chat.delete",
            vec![channel, ("ts", ts.0.clone())],
            &["message_not_found"],
        ),
        Change::React { ts, name, added } => (
            if *added {
                "reactions.add"
            } else {
                "reactions.remove"
            },
            vec![channel, ("timestamp", ts.0.clone()), ("name", name.clone())],
            &["already_reacted", "no_reaction"],
        ),
    }
}

pub fn describe(error: &SlackError) -> String {
    if error.is_auth() {
        return "the sign-in is no longer valid; sign in again".into();
    }
    match error {
        SlackError::Api(code) => match code.as_str() {
            "missing_scope" => {
                "the Slack app lacks a permission; reinstall it from the manifest".into()
            }
            "channel_not_found" => "the conversation no longer exists".into(),
            "not_in_channel" => "you are not in that channel".into(),
            "is_archived" => "the channel is archived".into(),
            "msg_too_long" => "the message is too long".into(),
            "cant_update_message" | "edit_window_closed" => {
                "that message can no longer be edited".into()
            }
            "cant_delete_message" => "you cannot delete that message".into(),
            "invalid_code" | "code_already_used" => "the sign-in link expired; try again".into(),
            "bad_redirect_uri" => {
                "the redirect URL does not match the Slack app; check its OAuth settings".into()
            }
            "invalid_client_id" | "bad_client_secret" => "the client ID or secret is wrong".into(),
            other => other.replace('_', " "),
        },
        other => other.to_string(),
    }
}

/// Walks a cursor-paged listing.
///
/// `page` fetches the page at a cursor (none for the first) and answers its
/// items and the next cursor; `each` takes every page's items as they come,
/// so a caller can show them early and keeps what arrived before a failure,
/// and answers whether to go on. The walk ends at an empty cursor, at a
/// cursor Slack already gave (some undocumented listings answer every page
/// with the same one), when `each` says stop, at the first error, or after
/// `max_pages`, which is logged: a listing cut short looks complete
/// otherwise.
async fn paginate<T, Fut>(
    what: &str,
    max_pages: usize,
    mut page: impl FnMut(Option<String>) -> Fut,
    mut each: impl FnMut(Vec<T>) -> bool,
) -> Result<(), SlackError>
where
    Fut: std::future::Future<Output = Result<(Vec<T>, Option<String>), SlackError>>,
{
    let mut cursor = None;
    let mut given = HashSet::new();
    for _ in 0..max_pages {
        let (items, next) = page(cursor.take()).await?;
        if !each(items) {
            return Ok(());
        }
        match next.filter(|next| !next.is_empty()) {
            Some(next) if !given.insert(next.clone()) => {
                log::debug!("{what}: Slack repeated a cursor; that was the last page");
                return Ok(());
            }
            Some(next) => cursor = Some(next),
            None => return Ok(()),
        }
    }
    log::warn!("{what}: stopped after {max_pages} pages; the rest is left out");
    Ok(())
}

/// `params`, plus the cursor when there is one.
fn with_cursor(
    mut params: Vec<(&'static str, String)>,
    cursor: Option<String>,
) -> Vec<(&'static str, String)> {
    if let Some(cursor) = cursor {
        params.push(("cursor", cursor));
    }
    params
}

/// Whether a failed fetch may work later: an outage or a rate limit, not
/// Slack saying no (an unknown id stays unknown).
fn worth_retrying(error: &SlackError) -> bool {
    !matches!(error, SlackError::Api(_))
}

/// Checks a pasted token and finds out whose it is.
async fn validate(http: &reqwest::Client, token: Token) -> Result<SignedIn, String> {
    if !token.access.starts_with("xox") {
        return Err("That does not look like a Slack token (it should start with xoxp-).".into());
    }
    if token.access.starts_with("xoxb-") {
        return Err("That is a bot token. NoSlacking needs the User OAuth Token (xoxp-).".into());
    }
    let client = Client::new(http.clone(), token.clone());
    let test: types::AuthTest = client
        .call("auth.test", &[])
        .await
        .map_err(|e| describe(&e))?;
    Ok(SignedIn {
        team_id: test.team_id,
        user_id: test.user_id,
        token,
    })
}

/// A workspace's name, domain and icon.
async fn workspace_details(client: &Client, team: &str, user: &str) -> Workspace {
    let mut workspace = Workspace {
        team_id: team.to_owned(),
        name: team.to_owned(),
        domain: String::new(),
        icon: None,
        user_id: user.to_owned(),
    };
    match client.call::<types::TeamInfo>("team.info", &[]).await {
        Ok(info) => {
            workspace.name = info.team.name;
            workspace.domain = info.team.domain;
            if !info.team.icon.image_default {
                workspace.icon = info
                    .team
                    .icon
                    .image_132
                    .or(info.team.icon.image_88)
                    .or(info.team.icon.image_68);
            }
        }
        Err(error) => {
            log::warn!("team.info: {error}");
            if let Ok(test) = client.call::<types::AuthTest>("auth.test", &[]).await {
                workspace.name = test.team;
            }
        }
    }
    workspace
}

/// Everything a workspace needs after signing in: cached lists first, then
/// a check of the token, fresh lists, custom emoji and unread state.
async fn boot(client: Client, workspace: Workspace, cache: Cache, sink: Sink) {
    let team = workspace.team_id.clone();
    if let Some(list) = cache.read::<Vec<Conversation>>(&team, "conversations") {
        sink.send(Event::Conversations {
            team: team.clone(),
            list,
            complete: false,
        });
    }
    if let Some(users) = cache.read::<Vec<User>>(&team, "users") {
        sink.send(Event::Users {
            team: team.clone(),
            users,
        });
    }
    match client.call::<types::AuthTest>("auth.test", &[]).await {
        Ok(_) => {}
        Err(error) if error.is_auth() => {
            sink.send(Event::SignedOut {
                team,
                reason: Some(describe(&error)),
            });
            return;
        }
        Err(error) => {
            sink.send(Event::Error(format!(
                "Could not reach {}: {}",
                workspace.name,
                describe(&error)
            )));
            return;
        }
    }
    let details = workspace_details(&client, &team, &workspace.user_id).await;
    if details != workspace {
        sink.send(Event::WorkspaceReady(details));
    }
    let list = conversations(client.clone(), team.clone(), cache.clone(), sink.clone()).await;
    match client.call::<types::EmojiList>("emoji.list", &[]).await {
        Ok(list) => sink.send(Event::Emoji {
            team: team.clone(),
            emoji: list.emoji,
        }),
        Err(error) => log::info!("emoji.list: {error}"),
    }
    // Side by side, but inside this task, so stopping the boot on sign-out
    // stops them too.
    let sweep = async {
        if let Some(list) = list {
            unread_sweep(client.clone(), team.clone(), list, sink.clone()).await;
        }
    };
    tokio::join!(
        users(client.clone(), team.clone(), cache, sink.clone()),
        sections(client.clone(), team.clone(), sink.clone()),
        sweep,
    );
}

/// Every conversation you are in.
async fn conversations(
    client: Client,
    team: String,
    cache: Cache,
    sink: Sink,
) -> Option<Vec<Conversation>> {
    let mut list = Vec::new();
    let walked = paginate(
        "users.conversations",
        CONVERSATION_PAGES,
        |cursor| {
            let params = with_cursor(
                vec![
                    ("types", "public_channel,private_channel,mpim,im".to_owned()),
                    ("exclude_archived", "true".to_owned()),
                    ("limit", "200".to_owned()),
                ],
                cursor,
            );
            let client = &client;
            async move {
                let page: types::ConversationsPage =
                    client.call("users.conversations", &params).await?;
                let next = page.response_metadata.cursor();
                Ok((page.channels, next))
            }
        },
        |channels| {
            list.extend(channels.into_iter().map(types::Channel::into_model));
            true
        },
    )
    .await;
    if let Err(error) = walked {
        sink.send(Event::Error(format!(
            "Could not list conversations: {}",
            describe(&error)
        )));
        return None;
    }
    cache.write(&team, "conversations", &list);
    sink.send(Event::Conversations {
        team,
        list: list.clone(),
        complete: true,
    });
    Some(list)
}

/// Your sidebar sections and starred conversations, as Slack's own client
/// gets them. `users.channelSections.list` is undocumented and only answers
/// browser sessions; anything else keeps the plain sidebar.
/// Workspaces whose unsent section channels were logged already.
static SECTIONS_NOTED: std::sync::Mutex<Vec<String>> = std::sync::Mutex::new(Vec::new());

/// Whether `key` is new to `seen`, remembering it.
fn first_time(seen: &std::sync::Mutex<Vec<String>>, key: &str) -> bool {
    let mut seen = seen
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if seen.iter().any(|k| k == key) {
        return false;
    }
    seen.push(key.to_owned());
    true
}

async fn sections(client: Client, team: String, sink: Sink) {
    // The web client sends the token in the form; do the same.
    let token = client.token().access;
    let mut all = Vec::new();
    let walked = paginate(
        "users.channelSections.list",
        SECTION_PAGES,
        |cursor| {
            let params = with_cursor(vec![("token", token.clone())], cursor);
            let client = &client;
            async move {
                let page: types::ChannelSectionsPage =
                    client.call("users.channelSections.list", &params).await?;
                Ok((page.channel_sections, page.cursor))
            }
        },
        // Slack has been seen answering every page with the same sections
        // and a fresh cursor: keep each section once, and stop at a page
        // that brings nothing new.
        |sections| {
            let before = all.len();
            for section in sections {
                if !all.iter().any(|s: &types::ChannelSection| {
                    s.channel_section_id == section.channel_section_id
                }) {
                    all.push(section);
                }
            }
            all.len() > before
        },
    )
    .await;
    if let Err(error) = walked {
        log::info!("no sidebar sections ({error}); using the plain sidebar");
        return;
    }
    // Slack files more channels in a section than it sends (archived and
    // left ones) and offers no call for the rest (see
    // `types::ChannelIdsPage`). Say so once per workspace and run; a
    // channel missing from its section still shows under Channels.
    let unsent: usize = all
        .iter()
        .filter_map(|section| section.channel_ids_page.unsent())
        .sum();
    if unsent > 0 && first_time(&SECTIONS_NOTED, &team) {
        log::info!(
            "sidebar sections of {team}: {unsent} filed channels not sent (archived or left, most likely)"
        );
    }
    let mut ordered = types::order_sections(all);
    // Slack leaves Starred empty in the section list; stars.list fills it.
    if let Some(starred) = ordered
        .iter_mut()
        .find(|s| s.kind == crate::model::SectionKind::Starred)
    {
        let mut items = Vec::new();
        let walked = paginate(
            "stars.list",
            STAR_PAGES,
            |cursor| {
                let params = with_cursor(
                    vec![("token", token.clone()), ("limit", "200".into())],
                    cursor,
                );
                let client = &client;
                async move {
                    let page: types::StarsList = client.call("stars.list", &params).await?;
                    let next = page.response_metadata.cursor();
                    Ok((page.items, next))
                }
            },
            |page| {
                items.extend(page);
                true
            },
        )
        .await;
        match walked {
            Ok(()) => {
                starred.channel_ids = types::StarsList {
                    items,
                    ..Default::default()
                }
                .conversations();
            }
            Err(error) => log::info!("stars.list: {error}"),
        }
    }
    if !ordered.is_empty() {
        sink.send(Event::Sections {
            team,
            sections: ordered,
        });
    }
}

/// Carries out a sidebar edit, in order, stopping at the first failure; then
/// fetches the sections again so the sidebar shows what Slack really has.
async fn edit_sidebar(
    client: Client,
    team: String,
    calls: Vec<crate::sidebar::SidebarCall>,
    sink: Sink,
) {
    use crate::sidebar::SidebarCall;
    let token = client.token().access;
    let section_channels = |section: &str, channel: &str| {
        serde_json::json!([{ "channel_section_id": section, "channel_ids": [channel] }]).to_string()
    };
    for call in calls {
        let result = match &call {
            SidebarCall::Create {
                name,
                channel,
                remove_from,
            } => {
                let created = client
                    .act::<Value>(
                        "users.channelSections.create",
                        &[
                            ("token", token.clone()),
                            ("name", name.clone()),
                            ("emoji", String::new()),
                            ("type", "standard".into()),
                        ],
                    )
                    .await;
                match (created, channel) {
                    (Ok(answer), Some(channel)) => {
                        let id = answer
                            .pointer("/channel_section/channel_section_id")
                            .and_then(Value::as_str)
                            .map(str::to_owned);
                        match id {
                            Some(id) => {
                                let mut params = vec![
                                    ("token", token.clone()),
                                    ("insert", section_channels(&id, channel)),
                                ];
                                if let Some(from) = remove_from {
                                    params.push(("remove", section_channels(from, channel)));
                                }
                                client
                                    .act::<Value>(
                                        "users.channelSections.channels.bulkUpdate",
                                        &params,
                                    )
                                    .await
                                    .map(|_| ())
                            }
                            None => Err(SlackError::Decode("no id for the new section".into())),
                        }
                    }
                    (Ok(_), None) => Ok(()),
                    (Err(error), _) => Err(error),
                }
            }
            SidebarCall::Set {
                section,
                name,
                next,
            } => {
                let mut params = vec![
                    ("token", token.clone()),
                    ("channel_section_id", section.clone()),
                ];
                if let Some(name) = name {
                    params.push(("name", name.clone()));
                }
                if let Some(next) = next {
                    params.push(("next_channel_section_id", next.clone()));
                }
                client
                    .act::<Value>("users.channelSections.set", &params)
                    .await
                    .map(|_| ())
            }
            SidebarCall::Delete { section } => client
                .act::<Value>(
                    "users.channelSections.delete",
                    &[
                        ("token", token.clone()),
                        ("channel_section_id", section.clone()),
                    ],
                )
                .await
                .map(|_| ()),
            SidebarCall::Channels {
                channel,
                insert,
                remove,
            } => {
                let mut params = vec![("token", token.clone())];
                params.push((
                    "insert",
                    insert
                        .as_deref()
                        .map_or_else(|| "[]".to_owned(), |to| section_channels(to, channel)),
                ));
                params.push((
                    "remove",
                    remove
                        .as_deref()
                        .map_or_else(|| "[]".to_owned(), |from| section_channels(from, channel)),
                ));
                client
                    .act::<Value>("users.channelSections.channels.bulkUpdate", &params)
                    .await
                    .map(|_| ())
            }
            SidebarCall::Star { channel, starred } => {
                let method = if *starred {
                    "stars.add"
                } else {
                    "stars.remove"
                };
                match client
                    .act::<Value>(method, &[("channel", channel.clone())])
                    .await
                {
                    // Already as asked.
                    Err(SlackError::Api(code))
                        if code == "already_starred" || code == "not_starred" =>
                    {
                        Ok(())
                    }
                    other => other.map(|_| ()),
                }
            }
        };
        if let Err(error) = result {
            log::warn!("sidebar edit {call:?} failed: {error}");
            sink.send(Event::Error(format!(
                "Could not change the sidebar: {}",
                describe(&error)
            )));
            break;
        }
    }
    sections(client, team, sink).await;
}

/// The workspace's people, page by page, each page shown as it arrives.
async fn users(client: Client, team: String, cache: Cache, sink: Sink) {
    let mut all = Vec::new();
    let walked = paginate(
        "users.list",
        USER_PAGES,
        |cursor| {
            let params = with_cursor(vec![("limit", "200".to_owned())], cursor);
            let client = &client;
            async move {
                let page: types::UsersPage = client.call("users.list", &params).await?;
                let next = page.response_metadata.cursor();
                Ok((page.members, next))
            }
        },
        |members| {
            let users: Vec<User> = members.into_iter().map(types::User::into_model).collect();
            all.extend(users.iter().cloned());
            sink.send(Event::Users {
                team: team.clone(),
                users,
            });
            true
        },
    )
    .await;
    // What arrived before a failure is still worth keeping.
    if let Err(error) = walked {
        log::info!("users.list: {error}");
    }
    if !all.is_empty() {
        cache.write(&team, "users", &all);
    }
}

/// Reads each conversation's read marker and newest message.
///
/// A browser session asks `client.counts`, the web client's own call, for
/// all of them at once. Anything it does not cover, and every OAuth
/// workspace, falls back to one or two calls per conversation, direct
/// messages first, skipping conversations whose state is already known.
/// A rate limit pauses the sweep rather than skipping conversations.
async fn unread_sweep(client: Client, team: String, list: Vec<Conversation>, sink: Sink) {
    let mut list = if client.token().is_session() {
        match client
            .call::<types::ClientCounts>("client.counts", &[])
            .await
        {
            Ok(counts) => {
                let counts = counts.by_id();
                let mut rest = Vec::new();
                for mut conversation in list {
                    match counts.get(&conversation.id) {
                        Some(count) => {
                            apply_count(&mut conversation, count);
                            sink.send(Event::Conversation {
                                team: team.clone(),
                                conversation,
                            });
                        }
                        None => rest.push(conversation),
                    }
                }
                rest
            }
            Err(error) => {
                log::info!("client.counts unavailable ({error}); reading each conversation");
                list
            }
        }
    } else {
        list
    };
    list.retain(|c| c.latest.is_none() || c.last_read.is_none());
    list.sort_by_key(|c| match c.kind {
        ConversationKind::Direct | ConversationKind::Group => 0,
        ConversationKind::Private => 1,
        ConversationKind::Channel => 2,
    });
    for conversation in list {
        let mut pause = SWEEP_PAUSE;
        for attempt in 1.. {
            match fetch_conversation(&client, &team, &conversation.id, &sink).await {
                Err(SlackError::RateLimited) if attempt < SWEEP_ATTEMPTS => {
                    log::debug!("unread sweep rate limited; pausing for {pause:?}");
                    tokio::time::sleep(pause).await;
                    pause *= 2;
                }
                Err(error) => {
                    log_fetch_failure("conversations.info", &conversation.id, &error);
                    break;
                }
                Ok(()) => break,
            }
        }
    }
}

/// How long the unread sweep first waits out a rate limit, and how many
/// times it tries one conversation.
const SWEEP_PAUSE: Duration = Duration::from_secs(15);
const SWEEP_ATTEMPTS: u32 = 4;

/// Puts `client.counts`' read state for one conversation onto it.
fn apply_count(conversation: &mut Conversation, count: &types::CountEntry) {
    if let Some(last_read) = types::real_ts(&count.last_read) {
        conversation.last_read = Some(last_read);
    }
    if let Some(latest) = types::real_ts(&count.latest) {
        conversation.latest = Some(latest);
    }
    conversation.mentions = count.mention_count;
}

/// Logs a failed background fetch: quietly for a passing failure, more
/// loudly when Slack refused, which points at something to fix.
fn log_fetch_failure(method: &str, id: &str, error: &SlackError) {
    if worth_retrying(error) {
        log::debug!("{method} {id}: {error}");
    } else {
        log::info!("{method} {id}: {error}");
    }
}

/// Fetches one conversation's details and sends them on.
async fn conversation_info(client: Client, team: String, channel: String, sink: Sink) {
    if let Err(error) = fetch_conversation(&client, &team, &channel, &sink).await {
        log_fetch_failure("conversations.info", &channel, &error);
    }
}

/// One conversation's details, with its newest message when the details
/// lack it; a conversation that is gone is reported as gone.
async fn fetch_conversation(
    client: &Client,
    team: &str,
    channel: &str,
    sink: &Sink,
) -> Result<(), SlackError> {
    match client
        .call::<types::ChannelInfo>("conversations.info", &[("channel", channel.to_owned())])
        .await
    {
        Ok(info) => {
            let mut conversation = info.channel.into_model();
            if conversation.latest.is_none()
                && let Ok(page) = client
                    .call::<types::HistoryPage>(
                        "conversations.history",
                        &[("channel", channel.to_owned()), ("limit", "1".into())],
                    )
                    .await
            {
                conversation.latest = page.messages.first().map(|m| Ts::new(m.ts.clone()));
            }
            sink.send(Event::Conversation {
                team: team.to_owned(),
                conversation,
            });
            Ok(())
        }
        Err(SlackError::Api(code)) if code == "channel_not_found" => {
            sink.send(Event::ConversationGone {
                team: team.to_owned(),
                channel: channel.to_owned(),
            });
            Ok(())
        }
        Err(error) => Err(error),
    }
}

/// A page of history: the newest one, or the one before `cursor`.
///
/// For the newest page, the copy in the offline cache goes out first, so
/// the conversation shows at once (and without a network); Slack's answer
/// then replaces it and is kept for next time.
async fn history(
    client: Client,
    team: String,
    channel: String,
    cursor: Option<String>,
    cache: Cache,
    sink: Sink,
) {
    let newest = cursor.is_none() && cache.is_enabled();
    if newest {
        let (cached_team, cached_channel, reader) = (team.clone(), channel.clone(), cache.clone());
        let cached =
            tokio::task::spawn_blocking(move || reader.read_history(&cached_team, &cached_channel))
                .await
                .ok()
                .flatten()
                .and_then(|value| serde_json::from_value::<types::HistoryPage>(value).ok());
        if let Some(page) = cached {
            let (messages, has_more, cursor) = history_page(page);
            sink.send(Event::CachedHistory {
                team: team.clone(),
                channel: channel.clone(),
                messages,
                has_more,
                cursor,
            });
        }
    }
    let mut params = vec![
        ("channel", channel.clone()),
        ("limit", HISTORY_PAGE.to_string()),
        ("include_all_metadata", "false".to_owned()),
    ];
    let older = cursor.is_some();
    if let Some(cursor) = cursor {
        params.push(("cursor", cursor));
    }
    // As Slack sent it, so the cache keeps the page exactly and reads it
    // back the same way.
    let answer = client
        .call::<Value>("conversations.history", &params)
        .await
        .and_then(|value| {
            serde_json::from_value::<types::HistoryPage>(value.clone())
                .map(|page| (page, value))
                .map_err(|error| SlackError::Decode(error.to_string()))
        });
    match answer {
        Ok((page, value)) => {
            let (messages, has_more, cursor) = history_page(page);
            sink.send(Event::History {
                team: team.clone(),
                channel: channel.clone(),
                messages,
                has_more,
                cursor,
                older,
            });
            if newest {
                let _ = tokio::task::spawn_blocking(move || {
                    cache.write_history(&team, &channel, &value);
                })
                .await;
            }
        }
        Err(error) => sink.send(Event::HistoryFailed {
            team,
            channel,
            error: describe(&error),
        }),
    }
}

/// A history page's messages, oldest first, and whether and how to read
/// on.
fn history_page(page: types::HistoryPage) -> (Vec<Message>, bool, Option<String>) {
    let cursor = page.response_metadata.cursor();
    let mut messages: Vec<Message> = page
        .messages
        .into_iter()
        .filter_map(types::Message::into_model)
        .collect();
    messages.reverse();
    (messages, page.has_more, cursor)
}

async fn thread(client: Client, team: String, channel: String, ts: Ts, sink: Sink) {
    let mut messages = Vec::new();
    let walked = paginate(
        "conversations.replies",
        THREAD_PAGES,
        |cursor| {
            let params = with_cursor(
                vec![
                    ("channel", channel.clone()),
                    ("ts", ts.0.clone()),
                    ("limit", "200".to_owned()),
                ],
                cursor,
            );
            let client = &client;
            async move {
                let page: types::HistoryPage =
                    client.call("conversations.replies", &params).await?;
                let next = page.response_metadata.cursor();
                Ok((page.messages, next))
            }
        },
        |page| {
            messages.extend(page.into_iter().filter_map(types::Message::into_model));
            true
        },
    )
    .await;
    if let Err(error) = walked {
        sink.send(Event::Error(format!(
            "Could not load the thread: {}",
            describe(&error)
        )));
        return;
    }
    sink.send(Event::Thread {
        team,
        channel,
        ts,
        messages,
    });
}

/// Saves the file at `url` in the downloads folder as `name`, or says in
/// a sentence why not.
///
/// The body streams into a hidden temporary file next to its final place,
/// which is renamed once complete, so a large file never sits in memory
/// and a failed download never appears under the real name.
/// What [`Worker::slash`] runs: the command's own method, or
/// `chat.command`. `Ok` carries Slack's reply text, when it has one.
async fn run_slash(
    client: &Client,
    channel: &str,
    command: &str,
    text: &str,
) -> Result<Option<String>, String> {
    let act = |method: &'static str, params: Vec<(&'static str, String)>| async move {
        client
            .act::<Value>(method, &params)
            .await
            .map(|_| None)
            .map_err(|e| describe(&e))
    };
    let channel = channel.to_owned();
    match command {
        "me" => {
            act(
                "chat.meMessage",
                vec![("channel", channel), ("text", text.to_owned())],
            )
            .await
        }
        "away" => act("users.setPresence", vec![("presence", "away".to_owned())]).await,
        "active" => act("users.setPresence", vec![("presence", "auto".to_owned())]).await,
        "status" => {
            let (emoji, status) = crate::slash::status(&crate::mrkdwn::unescape(text));
            let profile = serde_json::json!({
                "status_text": status,
                "status_emoji": emoji,
                "status_expiration": 0,
            });
            act("users.profile.set", vec![("profile", profile.to_string())]).await
        }
        "topic" => {
            act(
                "conversations.setTopic",
                vec![("channel", channel), ("topic", text.to_owned())],
            )
            .await
        }
        "invite" => {
            let people = crate::slash::mentioned(text);
            if people.is_empty() {
                return Err("name someone to invite with @".to_owned());
            }
            act(
                "conversations.invite",
                vec![("channel", channel), ("users", people.join(","))],
            )
            .await
        }
        "leave" => act("conversations.leave", vec![("channel", channel)]).await,
        _ if client.token().is_session() => {
            let params = [
                ("channel", channel),
                ("command", format!("/{command}")),
                ("text", text.to_owned()),
            ];
            client
                .act::<Value>("chat.command", &params)
                .await
                .map(|answer| {
                    str_of(&answer, "response")
                        .filter(|r| !r.is_empty())
                        .map(str::to_owned)
                })
                .map_err(|e| describe(&e))
        }
        _ => Err(super::SLASH_NEEDS_SESSION.to_owned()),
    }
}

/// Uploads one file for [`Worker::upload`], telling the interface how far
/// it got along the way.
#[allow(clippy::too_many_arguments)]
async fn upload(
    id: u64,
    client: Client,
    team: String,
    channel: String,
    thread: Option<Ts>,
    path: std::path::PathBuf,
    comment: String,
    poll_after: bool,
    sink: &Sink,
) {
    let name = path
        .file_name()
        .map_or_else(|| "file".to_owned(), |n| n.to_string_lossy().into_owned());
    // The size comes from the open file, so it is the size of what
    // gets streamed, not of whatever the path named a moment
    // earlier.
    let opened = match tokio::fs::File::open(&path).await {
        Ok(file) => file.metadata().await.map(|meta| (file, meta)),
        Err(error) => Err(error),
    };
    let (file, size) = match opened {
        Ok((_, meta)) if !meta.is_file() => {
            sink.send(Event::Error(format!("{name} is not a file.")));
            return;
        }
        Ok((file, meta)) => (file, meta.len()),
        Err(error) => {
            sink.send(Event::Error(format!("Could not read {name}: {error}")));
            return;
        }
    };
    if size > MAX_UPLOAD {
        sink.send(Event::Error(format!(
            "{name} is larger than Slack's 1 GB limit."
        )));
        return;
    }
    sink.send(Event::UploadProgress {
        id,
        sent: 0,
        total: size,
    });
    let thread = thread.as_ref().map(Ts::as_str);
    let progress = {
        let sink = sink.clone();
        // Told only at each further hundredth, so a big file does not
        // wake the window for every chunk.
        let reported = std::sync::atomic::AtomicU64::new(0);
        let step = (size / 100).max(1);
        move |sent: u64| {
            let last = reported.load(std::sync::atomic::Ordering::Relaxed);
            if sent / step > last / step || sent == size {
                reported.store(sent, std::sync::atomic::Ordering::Relaxed);
                sink.send(Event::UploadProgress {
                    id,
                    sent,
                    total: size,
                });
            }
        }
    };
    match client
        .upload(&channel, thread, &name, file, size, &comment, progress)
        .await
    {
        Ok(()) => {
            sink.send(Event::Notice(format!("Uploaded {name}")));
            // Without a live socket the new file would only show
            // at the next poll.
            if poll_after {
                history(client, team, channel, None, Cache::disabled(), sink.clone()).await;
            }
        }
        Err(error) => sink.send(Event::Error(format!(
            "Could not upload {name}: {}",
            describe(&error)
        ))),
    }
}

async fn download(client: &Client, url: &str, name: &str) -> Result<std::path::PathBuf, String> {
    use tokio::io::AsyncWriteExt as _;
    let mut response = client
        .download(url)
        .await
        .map_err(|e| format!("Could not download {name}: {}", describe(&e)))?;
    let saving = |error: std::io::Error| format!("Could not save {name}: {error}");
    // Finding the folder can read a config file; keep it off the runtime.
    let dir = tokio::task::spawn_blocking(downloads_dir)
        .await
        .ok()
        .flatten()
        .ok_or_else(|| saving(std::io::Error::other("no downloads folder")))?;
    let safe = safe_name(name);
    let (part, mut file) = create_unique(&dir, |n| format!(".{}.part", numbered(&safe, n)))
        .await
        .map_err(saving)?;
    let written: Result<(), String> = async {
        let mut size = 0u64;
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|e| format!("Could not download {name}: {}", SlackError::from(e)))?
        {
            size += chunk.len() as u64;
            if size > MAX_UPLOAD {
                return Err(format!("{name} is larger than Slack's 1 GB limit."));
            }
            file.write_all(&chunk).await.map_err(saving)?;
        }
        file.flush().await.map_err(saving)?;
        file.sync_all().await.map_err(saving)
    }
    .await;
    drop(file);
    if let Err(error) = written {
        let _ = tokio::fs::remove_file(&part).await;
        return Err(error);
    }
    // Claim the final name with create_new, so no other file can take it
    // between the check and the rename, then move the download onto it.
    let claimed = create_unique(&dir, |n| numbered(&safe, n)).await;
    let renamed = match claimed {
        Ok((path, reserved)) => {
            drop(reserved);
            match tokio::fs::rename(&part, &path).await {
                Ok(()) => Ok(path),
                Err(error) => {
                    let _ = tokio::fs::remove_file(&path).await;
                    Err(error)
                }
            }
        }
        Err(error) => Err(error),
    };
    if renamed.is_err() {
        let _ = tokio::fs::remove_file(&part).await;
    }
    renamed.map_err(saving)
}

fn downloads_dir() -> Option<std::path::PathBuf> {
    directories::UserDirs::new()
        .and_then(|dirs| dirs.download_dir().map(std::path::Path::to_path_buf))
        .or_else(|| directories::BaseDirs::new().map(|dirs| dirs.home_dir().to_path_buf()))
}

/// Creates the first of `name(0)`, `name(1)`, … that does not exist yet in
/// `dir`. `create_new` makes taking the name and creating the file one
/// step, so two downloads of the same name cannot both get it.
async fn create_unique(
    dir: &std::path::Path,
    name: impl Fn(u32) -> String,
) -> std::io::Result<(std::path::PathBuf, tokio::fs::File)> {
    for n in 0..10_000 {
        let path = dir.join(name(n));
        match tokio::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .await
        {
            Ok(file) => return Ok((path, file)),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error),
        }
    }
    Err(std::io::Error::other("too many files with that name"))
}

/// The longest file name written, in bytes: room under the 255 most file
/// systems allow for " (n)" and the temporary ".part".
const MAX_NAME: usize = 200;

/// An extension worth keeping when a name is cut short: short and real.
fn split_extension(name: &str) -> (&str, Option<&str>) {
    match name.rsplit_once('.') {
        Some((stem, ext)) if !stem.is_empty() && !ext.is_empty() && ext.len() <= 16 => {
            (stem, Some(ext))
        }
        _ => (name, None),
    }
}

/// `name`, or for `n > 0` the same with " (n)" before its extension.
fn numbered(name: &str, n: u32) -> String {
    if n == 0 {
        return name.to_owned();
    }
    match split_extension(name) {
        (stem, Some(ext)) => format!("{stem} ({n}).{ext}"),
        (stem, None) => format!("{stem} ({n})"),
    }
}

/// Cuts `name` to at most `max` bytes on a character boundary, keeping
/// its extension.
fn truncate_name(name: &str, max: usize) -> String {
    if name.len() <= max {
        return name.to_owned();
    }
    let (stem, ext) = split_extension(name);
    let room = max.saturating_sub(ext.map_or(0, |ext| ext.len() + 1));
    let mut end = room.min(stem.len());
    while !stem.is_char_boundary(end) {
        end -= 1;
    }
    let stem = stem[..end].trim_end_matches(['.', ' ']);
    match ext {
        Some(ext) => format!("{stem}.{ext}"),
        None => stem.to_owned(),
    }
}

/// Names Windows keeps for devices, whatever the extension: `nul.txt`
/// opens the null device, not a file.
fn is_reserved(name: &str) -> bool {
    let stem = name.split('.').next().unwrap_or(name).trim_end();
    let upper = stem.to_ascii_uppercase();
    matches!(
        upper.as_str(),
        "CON" | "PRN" | "AUX" | "NUL" | "CONIN$" | "CONOUT$"
    ) || ((upper.starts_with("COM") || upper.starts_with("LPT"))
        && upper.len() == 4
        && upper[3..].chars().all(|c| matches!(c, '1'..='9')))
}

/// A file name that cannot climb out of the downloads folder, hide, or
/// break on any of the systems the app runs on: no separators or
/// characters Windows refuses, no control characters, no leading dots,
/// no trailing dots or spaces (Windows drops them), no device names, and
/// not too long.
fn safe_name(name: &str) -> String {
    let replaced: String = name
        .chars()
        .map(|c| {
            if c.is_control() || matches!(c, '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*') {
                '_'
            } else {
                c
            }
        })
        .collect();
    let trimmed = replaced
        .trim_start_matches(['.', ' '])
        .trim_end_matches(['.', ' ']);
    let mut safe = truncate_name(trimmed, MAX_NAME);
    if safe.is_empty() {
        return "download".to_owned();
    }
    if is_reserved(&safe) {
        safe.insert(0, '_');
    }
    safe
}

/// What one Socket Mode event means here. Short-lived, so its size does
/// not matter.
#[derive(Debug)]
#[allow(clippy::large_enum_variant)]
enum Translated {
    Event(Event),
    /// Fetch this conversation's details again.
    Refresh(String),
    /// The sidebar's sections changed in another Slack client.
    RefreshSections,
    /// Your notification preferences changed in another Slack client.
    RefreshPrefs,
}

fn str_of<'a>(value: &'a Value, key: &str) -> Option<&'a str> {
    value.get(key).and_then(Value::as_str)
}

fn message_of(value: &Value) -> Option<Message> {
    serde_json::from_value::<types::Message>(value.clone())
        .ok()?
        .into_model()
}

/// Turns an Events API event into interface events.
fn translate(team: &str, me: &str, event: &Value) -> Vec<Translated> {
    let team = team.to_owned();
    let kind = str_of(event, "type").unwrap_or("");
    let channel = str_of(event, "channel").map(str::to_owned);
    let mut out = Vec::new();
    match kind {
        "message" => {
            let Some(channel) = channel else {
                return out;
            };
            match str_of(event, "subtype") {
                Some("message_changed" | "message_replied") => {
                    if let Some(message) = event.get("message").and_then(message_of) {
                        out.push(Translated::Event(Event::Message {
                            team,
                            channel,
                            message,
                            changed: true,
                        }));
                    }
                }
                Some("message_deleted") => {
                    if let Some(ts) = str_of(event, "deleted_ts") {
                        out.push(Translated::Event(Event::Deleted {
                            team,
                            channel,
                            ts: Ts::new(ts),
                        }));
                    }
                }
                Some("channel_name" | "group_name" | "channel_topic" | "channel_purpose") => {
                    if let Some(message) = message_of(event) {
                        out.push(Translated::Event(Event::Message {
                            team,
                            channel: channel.clone(),
                            message,
                            changed: false,
                        }));
                    }
                    out.push(Translated::Refresh(channel));
                }
                _ => {
                    if let Some(message) = message_of(event) {
                        out.push(Translated::Event(Event::Message {
                            team,
                            channel,
                            message,
                            changed: false,
                        }));
                    }
                }
            }
        }
        "reaction_added" | "reaction_removed" => {
            let item = event.get("item");
            let channel = item.and_then(|i| str_of(i, "channel"));
            let ts = item.and_then(|i| str_of(i, "ts"));
            let name = str_of(event, "reaction");
            let user = str_of(event, "user");
            if let (Some(channel), Some(ts), Some(name), Some(user)) = (channel, ts, name, user) {
                out.push(Translated::Event(Event::Reaction {
                    team,
                    channel: channel.to_owned(),
                    ts: Ts::new(ts),
                    name: name.to_owned(),
                    user: user.to_owned(),
                    added: kind == "reaction_added",
                }));
            }
        }
        "member_joined_channel" | "member_left_channel" => {
            let user = str_of(event, "user");
            if user == Some(me)
                && let Some(channel) = channel
            {
                if kind == "member_joined_channel" {
                    out.push(Translated::Refresh(channel));
                } else {
                    out.push(Translated::Event(Event::ConversationGone { team, channel }));
                }
            }
        }
        "channel_left" | "group_left" | "channel_deleted" | "group_deleted" | "channel_archive"
        | "group_archive" => {
            if let Some(channel) = channel {
                out.push(Translated::Event(Event::ConversationGone { team, channel }));
            }
        }
        "channel_rename" | "group_rename" | "channel_created" | "channel_unarchive"
        | "im_created" => {
            let id = channel.or_else(|| {
                event
                    .get("channel")
                    .and_then(|c| str_of(c, "id"))
                    .map(str::to_owned)
            });
            // Only conversations you are in belong in the sidebar; a fresh
            // channel someone else made is not one of them.
            if let Some(id) = id
                && kind != "channel_created"
            {
                out.push(Translated::Refresh(id));
            }
        }
        "user_change" | "team_join" => {
            if let Some(user) = event
                .get("user")
                .and_then(|u| serde_json::from_value::<types::User>(u.clone()).ok())
            {
                out.push(Translated::Event(Event::Users {
                    team,
                    users: vec![user.into_model()],
                }));
            }
        }
        // Sections made, renamed, moved, deleted, or channels moved between
        // them, and stars, in Slack's own client.
        "channel_section_upserted"
        | "channel_section_deleted"
        | "channel_sections_channels_upserted"
        | "channel_sections_channels_removed"
        | "star_added"
        | "star_removed" => out.push(Translated::RefreshSections),
        // Mutes, notification levels or keywords changed in another client.
        "pref_change" => {
            if super::desktop::is_notification_pref(str_of(event, "name").unwrap_or("")) {
                out.push(Translated::RefreshPrefs);
            }
        }
        // Your own Do Not Disturb changed, here or in another client.
        "dnd_updated" => {
            if let Some(dnd) = super::desktop::dnd_event(event) {
                out.push(Translated::Event(Event::Dnd { team, dnd }));
            }
        }
        "pin_added" | "pin_removed" => {
            if let Some(event) = super::convos::pin_event(kind, event) {
                out.push(Translated::Event(Event::Convos { team, event }));
            }
        }
        // Read on another device (or in another window): the read marker
        // moves, so unread counts here follow. The interface only ever moves
        // a marker forward, so an older mark arriving late changes nothing.
        "channel_marked" | "group_marked" | "im_marked" | "mpim_marked" => {
            if let (Some(channel), Some(ts)) = (channel, str_of(event, "ts")) {
                out.push(Translated::Event(Event::Read {
                    team,
                    channel,
                    ts: Ts::new(ts),
                }));
            }
        }
        _ => log::debug!("unhandled event {kind}"),
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn events(value: &str) -> Vec<Translated> {
        translate("T1", "U1", &serde_json::from_str(value).expect("json"))
    }

    #[test]
    fn reads_on_other_devices_move_the_read_marker() {
        for kind in ["channel_marked", "group_marked", "im_marked", "mpim_marked"] {
            let read = events(&format!(
                r#"{{"type":"{kind}","channel":"C1","ts":"1700000000.000200"}}"#
            ));
            assert!(
                matches!(&read[..], [Translated::Event(Event::Read { team, channel, ts })]
                    if team == "T1" && channel == "C1" && ts.0 == "1700000000.000200"),
                "{kind}"
            );
        }
        assert!(events(r#"{"type":"channel_marked","channel":"C1"}"#).is_empty());
    }

    #[test]
    fn new_edited_and_deleted_messages() {
        match &events(r#"{"type":"message","channel":"C1","user":"U2","text":"hi","ts":"1.0"}"#)[..]
        {
            [
                Translated::Event(Event::Message {
                    channel,
                    message,
                    changed: false,
                    ..
                }),
            ] => {
                assert_eq!(channel, "C1");
                assert_eq!(message.text, "hi");
            }
            other => panic!("{other:?}"),
        }
        match &events(
            r#"{"type":"message","subtype":"message_changed","channel":"C1","message":{"user":"U2","text":"edited","ts":"1.0","edited":{"user":"U2","ts":"2.0"}}}"#,
        )[..]
        {
            [
                Translated::Event(Event::Message {
                    message, changed, ..
                }),
            ] => assert!(message.edited && *changed),
            other => panic!("{other:?}"),
        }
        match &events(
            r#"{"type":"message","subtype":"message_deleted","channel":"C1","deleted_ts":"1.0"}"#,
        )[..]
        {
            [Translated::Event(Event::Deleted { ts, .. })] => assert_eq!(ts.as_str(), "1.0"),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn reactions_and_membership() {
        match &events(
            r#"{"type":"reaction_added","user":"U2","reaction":"tada","item":{"type":"message","channel":"C1","ts":"1.0"}}"#,
        )[..]
        {
            [
                Translated::Event(Event::Reaction {
                    added: true, name, ..
                }),
            ] => assert_eq!(name, "tada"),
            other => panic!("{other:?}"),
        }
        assert!(matches!(
            &events(r#"{"type":"member_joined_channel","user":"U1","channel":"C9"}"#)[..],
            [Translated::Refresh(c)] if c == "C9"
        ));
        assert!(
            events(r#"{"type":"member_joined_channel","user":"U2","channel":"C9"}"#).is_empty()
        );
        assert!(
            events(r#"{"type":"channel_created","channel":{"id":"C5","name":"x"}}"#).is_empty()
        );
    }

    /// A worker with no network behind it: an in-memory keyring, a
    /// throwaway folder, and the events it sends.
    fn worker() -> (Worker, std::sync::mpsc::Receiver<Event>) {
        let (sender, events) = std::sync::mpsc::channel();
        let sink = Sink {
            sender,
            waker: super::super::Waker::default(),
            gate: None,
        };
        let root = std::env::temp_dir().join(format!("noslacking-test-{}", std::process::id()));
        let images = ImageLoader::new(tokio::runtime::Handle::current(), root.join("images"));
        let worker = Worker::new(Credentials::memory(), AppDirs::under(&root), sink, images);
        (worker, events)
    }

    fn team(worker: &mut Worker, id: &str, token: Token) {
        let client = Client::new(reqwest::Client::new(), token);
        let (sink, gate) = worker.sink.gated();
        let boot = tokio::spawn(async {}).abort_handle();
        worker.teams.insert(
            id.to_owned(),
            Team {
                client,
                user_id: "U1".into(),
                sink,
                gate,
                boot,
            },
        );
    }

    fn live(worker: &mut Worker, status: Socket) -> Live {
        Live {
            stop: watch::channel(false).0,
            generation: worker.generation(),
            status,
        }
    }

    fn session() -> Token {
        Token::session("xoxc-1", "xoxd-1", "https://a.slack.com")
    }

    #[tokio::test]
    async fn each_workspace_has_its_own_liveness() {
        let (mut worker, _events) = worker();
        team(&mut worker, "TA", session());
        team(&mut worker, "TB", session());
        team(&mut worker, "TC", Token::plain("xoxp-1"));
        let up = live(&mut worker, Socket::Connected);
        worker.rtm.insert("TA".into(), up);
        // RTM for A says nothing about B, nor about Socket Mode for C.
        assert!(worker.is_live("TA"));
        assert!(!worker.is_live("TB"));
        assert!(!worker.is_live("TC"));
        let up = live(&mut worker, Socket::Connected);
        worker.socket = Some(up);
        assert!(worker.is_live("TC"));
        assert!(!worker.is_live("TB"));
    }

    #[tokio::test]
    async fn the_interface_hears_the_focused_workspace() {
        let (mut worker, events) = worker();
        team(&mut worker, "TA", session());
        team(&mut worker, "TB", session());
        let up = live(&mut worker, Socket::Connected);
        worker.rtm.insert("TA".into(), up);
        worker.focus = Some(("TB".into(), None));
        worker.report_socket();
        worker.focus = Some(("TA".into(), Some("C1".into())));
        worker.report_socket();
        // No change, no event.
        worker.report_socket();
        let heard: Vec<Socket> = events
            .try_iter()
            .filter_map(|event| match event {
                Event::Socket(socket) => Some(socket),
                _ => None,
            })
            .collect();
        assert_eq!(heard, [Socket::Off, Socket::Connected]);
    }

    #[tokio::test]
    async fn a_replaced_socket_cannot_remove_its_successor() {
        use crate::slack::rtm::RtmEvent;
        let (mut worker, _events) = worker();
        team(&mut worker, "TA", session());
        let old = live(&mut worker, Socket::Connecting).generation;
        let current = live(&mut worker, Socket::Connected);
        let generation = current.generation;
        worker.rtm.insert("TA".into(), current);
        worker.internal(Internal::Rtm {
            team: "TA".into(),
            generation: old,
            event: RtmEvent::Unavailable("gone".into()),
        });
        assert!(worker.is_live("TA"));
        worker.internal(Internal::Rtm {
            team: "TA".into(),
            generation,
            event: RtmEvent::Disconnected("drop".into()),
        });
        assert!(!worker.is_live("TA"));
        assert!(worker.rtm.contains_key("TA"));
        // A stale Socket Mode report is ignored the same way.
        let socket = live(&mut worker, Socket::Connected);
        let generation = socket.generation;
        worker.socket = Some(socket);
        worker.internal(Internal::Socket {
            generation: generation - 1,
            event: SocketEvent::Disconnected("old".into()),
        });
        assert_eq!(
            worker.socket.as_ref().map(|s| s.status.clone()),
            Some(Socket::Connected)
        );
    }

    #[tokio::test]
    async fn a_keyring_failure_leaves_no_workspace_waiting() {
        let (mut worker, events) = worker();
        let meta = |id: &str| WorkspaceMeta {
            team_id: id.into(),
            name: id.into(),
            domain: String::new(),
            icon: None,
            user_id: "U1".into(),
        };
        worker.internal(Internal::Loaded {
            app: Ok(None),
            workspaces: vec![
                (meta("TA"), Stored::Missing),
                (
                    meta("TB"),
                    Stored::Failed(crate::credentials::Error::Locked),
                ),
                (meta("TC"), Stored::Skipped),
            ],
            cache_key: None,
        });
        assert!(worker.waiting.is_none());
        let signed_out: Vec<String> = events
            .try_iter()
            .filter_map(|event| match event {
                Event::SignedOut {
                    team,
                    reason: Some(_),
                } => Some(team),
                _ => None,
            })
            .collect();
        assert_eq!(signed_out, ["TA", "TB", "TC"]);
    }

    #[tokio::test]
    async fn failed_fetches_can_be_asked_for_again() {
        let (mut worker, _events) = worker();
        // A session token, so signing out revokes nothing over the network.
        team(&mut worker, "TA", session());
        for id in ["U1", "U2"] {
            worker
                .users_requested
                .insert(("TA".to_owned(), id.to_owned()));
        }
        worker.bots_requested.insert(("TA".into(), "B1".into()));
        worker.internal(Internal::FetchFailed {
            team: "TA".into(),
            users: vec!["U1".into()],
            bots: vec!["B1".into()],
        });
        assert!(!worker.users_requested.contains(&("TA".into(), "U1".into())));
        assert!(worker.users_requested.contains(&("TA".into(), "U2".into())));
        assert!(worker.bots_requested.is_empty());
        worker.sign_out("TA");
        assert!(worker.users_requested.is_empty());
        assert!(worth_retrying(&SlackError::RateLimited));
        assert!(!worth_retrying(&SlackError::Api("user_not_found".into())));
    }

    #[tokio::test]
    async fn slack_links_count_only_during_a_browser_sign_in() {
        let link = "slack://T0123ABCD/magic-login/abc?host=acme.slack.com";
        let (mut worker, events) = worker();
        worker.waiting = None;
        // Nothing started here: a handed-over link is not ours to use.
        worker.command(Command::Callback(link.into()));
        assert!(events.try_iter().next().is_none());
        // Started too long ago: still ignored.
        worker.browser_sign_in = std::time::Instant::now().checked_sub(BROWSER_SIGN_IN_WINDOW);
        assert!(!worker.browser_sign_in_pending());
        worker.command(Command::Callback(link.into()));
        assert!(events.try_iter().next().is_none());
        // Just started: pending, and a link that is not a sign-in link is
        // still ignored without ending the wait.
        worker.browser_sign_in = Some(std::time::Instant::now());
        assert!(worker.browser_sign_in_pending());
        worker.command(Command::Callback("slack://channel?team=T1&id=C1".into()));
        assert!(events.try_iter().next().is_none());
        assert!(worker.browser_sign_in_pending());
    }

    #[tokio::test]
    async fn deep_links_reach_the_interface_only_for_signed_in_workspaces() {
        let (mut worker, events) = worker();
        worker.waiting = None;
        team(&mut worker, "TA", session());
        worker.command(Command::Callback("slack://channel?team=TB&id=C1".into()));
        assert!(events.try_iter().next().is_none(), "not signed in here");
        worker.command(Command::Callback("slack://channel?team=TA&id=C1".into()));
        let events: Vec<Event> = events.try_iter().collect();
        assert!(
            matches!(&events[..], [Event::DeepLink(link)] if link.team.as_deref() == Some("TA")),
            "{events:?}"
        );
    }

    #[tokio::test]
    async fn commands_for_an_unknown_workspace_get_an_answer() {
        let (mut worker, events) = worker();
        worker.waiting = None;
        worker.command(Command::Send {
            team: "TX".into(),
            channel: "C1".into(),
            text: "hi".into(),
            thread: None,
            broadcast: false,
            local: Ts::new("local-1"),
        });
        worker.command(Command::Delete {
            team: "TX".into(),
            channel: "C1".into(),
            ts: Ts::new("1.0"),
            removed: None,
        });
        worker.command(Command::LoadHistory {
            team: "TX".into(),
            channel: "C1".into(),
        });
        worker.command(Command::LoadAround {
            team: "TX".into(),
            channel: "C1".into(),
            ts: Ts::new("1.0"),
        });
        let events: Vec<Event> = events.try_iter().collect();
        assert!(
            matches!(&events[..], [
                Event::Sent { local, result: Err(_), .. },
                Event::Settled { change: Change::Delete { .. }, result: Err(_), .. },
                Event::HistoryFailed { .. },
                Event::HistoryFailed { .. },
            ] if local.as_str() == "local-1"),
            "{events:?}"
        );
    }

    #[tokio::test]
    async fn nothing_from_a_signed_out_workspace_gets_through() {
        let (mut worker, events) = worker();
        team(&mut worker, "TA", session());
        let (_, sink) = worker.team("TA").expect("signed in");
        let pending = tokio::spawn(std::future::pending::<()>());
        if let Some(team) = worker.teams.get_mut("TA") {
            team.boot = pending.abort_handle();
        }
        worker.sign_out("TA");
        // A task that outlived the sign-out reports a late WorkspaceReady.
        sink.send(Event::WorkspaceReady(Workspace {
            team_id: "TA".into(),
            name: "A".into(),
            domain: String::new(),
            icon: None,
            user_id: "U1".into(),
        }));
        let events: Vec<Event> = events.try_iter().collect();
        assert!(
            events
                .iter()
                .any(|e| matches!(e, Event::SignedOut { team, reason: None } if team == "TA")),
            "{events:?}"
        );
        assert!(
            !events.iter().any(|e| matches!(e, Event::WorkspaceReady(_))),
            "{events:?}"
        );
        assert!(pending.await.is_err_and(|e| e.is_cancelled()));
    }

    type Page = std::future::Ready<Result<(Vec<u32>, Option<String>), SlackError>>;

    /// A pretend listing: page `n` holds `n`, and the cursor for page `n+1`
    /// is `"n+1"` until `last`.
    fn pages(
        last: u32,
        fail_at: Option<u32>,
        asked: &mut Vec<Option<String>>,
    ) -> impl FnMut(Option<String>) -> Page {
        move |cursor| {
            asked.push(cursor.clone());
            let n = cursor.map_or(0, |c| c.parse().unwrap_or(0));
            if fail_at == Some(n) {
                return std::future::ready(Err(SlackError::RateLimited));
            }
            let next = (n < last).then(|| (n + 1).to_string());
            std::future::ready(Ok((vec![n], next)))
        }
    }

    #[tokio::test]
    async fn paginate_stops_when_slack_repeats_itself() {
        // The same cursor every time: the second page is the last.
        let mut asked = 0;
        let walked = paginate(
            "t",
            10,
            |_| {
                asked += 1;
                std::future::ready(Ok((vec![1], Some("same".to_owned()))))
            },
            |_| true,
        )
        .await;
        assert_eq!(walked, Ok(()));
        assert_eq!(asked, 2);
        // A caller that has seen it all before says stop, even though the
        // cursors keep changing.
        let asked = std::cell::Cell::new(0);
        let mut taken = 0;
        let walked = paginate(
            "t",
            10,
            |_| {
                asked.set(asked.get() + 1);
                std::future::ready(Ok((vec![0], Some(asked.get().to_string()))))
            },
            |_| {
                taken += 1;
                taken < 2
            },
        )
        .await;
        assert_eq!(walked, Ok(()));
        assert_eq!(asked.get(), 2);
    }

    #[tokio::test]
    async fn paginate_follows_cursors_to_the_end() {
        let mut asked = Vec::new();
        let mut seen = Vec::new();
        let walked = paginate("t", 10, pages(3, None, &mut asked), |p| {
            seen.extend(p);
            true
        })
        .await;
        assert_eq!(walked, Ok(()));
        assert_eq!(seen, [0, 1, 2, 3]);
        assert_eq!(
            asked,
            [None, Some("1".into()), Some("2".into()), Some("3".into())]
        );
    }

    #[tokio::test]
    async fn paginate_stops_at_the_cap_and_at_errors() {
        let mut asked = Vec::new();
        let mut seen = Vec::new();
        let walked = paginate("t", 2, pages(100, None, &mut asked), |p| {
            seen.extend(p);
            true
        })
        .await;
        assert_eq!(walked, Ok(()));
        assert_eq!(seen, [0, 1]);
        let mut asked = Vec::new();
        let mut seen = Vec::new();
        let walked = paginate("t", 10, pages(5, Some(2), &mut asked), |p| {
            seen.extend(p);
            true
        })
        .await;
        assert_eq!(walked, Err(SlackError::RateLimited));
        // What came before the failure was handed over.
        assert_eq!(seen, [0, 1]);
    }

    #[test]
    fn counts_fill_in_the_read_state() {
        let counts: types::ClientCounts = serde_json::from_str(
            r#"{"ok":true,
                "channels":[{"id":"C1","last_read":"1.0","latest":"2.0","mention_count":3,"has_unreads":true}],
                "mpims":[{"id":"G1","last_read":"0000000000.000000","latest":"","mention_count":0}],
                "ims":[{"id":"D1","last_read":"5.0","latest":"5.0"}]}"#,
        )
        .expect("parses");
        let counts = counts.by_id();
        assert_eq!(counts.len(), 3);
        let mut channel = serde_json::from_str::<types::Channel>(r#"{"id":"C1","name":"general"}"#)
            .expect("parses")
            .into_model();
        apply_count(&mut channel, &counts["C1"]);
        assert_eq!(channel.last_read, Some(Ts::new("1.0")));
        assert_eq!(channel.latest, Some(Ts::new("2.0")));
        assert_eq!(channel.mentions, 3);
        assert!(channel.has_unread());
        // Slack's "never" markers are no read state at all.
        let mut group = Conversation {
            id: "G1".into(),
            ..channel.clone()
        };
        group.last_read = None;
        group.latest = None;
        apply_count(&mut group, &counts["G1"]);
        assert_eq!((group.last_read, group.latest), (None, None));
    }

    #[test]
    fn failures_read_as_plain_sentences() {
        for code in crate::slack::client::AUTH_ERRORS {
            assert_eq!(
                describe(&SlackError::Api((*code).to_owned())),
                "the sign-in is no longer valid; sign in again",
                "{code}"
            );
        }
        assert_eq!(
            describe(&SlackError::Api("channel_not_found".into())),
            "the conversation no longer exists"
        );
        assert_eq!(
            describe(&SlackError::Api("some_new_code".into())),
            "some new code"
        );
        assert_eq!(describe(&SlackError::Http(502)), "HTTP 502");
    }

    #[test]
    fn download_names_stay_in_the_folder() {
        assert_eq!(safe_name("../../.bashrc"), "_.._.bashrc");
        assert_eq!(safe_name("report.pdf"), "report.pdf");
        assert_eq!(safe_name(".."), "download");
        assert_eq!(safe_name("C:\\Windows\\x.exe"), "C__Windows_x.exe");
    }

    #[test]
    fn download_names_work_on_windows() {
        assert_eq!(safe_name("a<b>c:d\"e|f?g*h.txt"), "a_b_c_d_e_f_g_h.txt");
        assert_eq!(safe_name("tab\there\u{7}.txt"), "tab_here_.txt");
        assert_eq!(safe_name("notes. . ."), "notes");
        assert_eq!(safe_name("  spaced  "), "spaced");
        for reserved in [
            "CON",
            "nul.txt",
            "Com1.log",
            "LPT9",
            "aux.tar.gz",
            "conout$",
        ] {
            assert_eq!(safe_name(reserved), format!("_{reserved}"), "{reserved}");
        }
        for fine in ["console.txt", "COM10", "COM0", "nullish", "lpt.txt"] {
            assert_eq!(safe_name(fine), fine, "{fine}");
        }
    }

    #[test]
    fn long_download_names_keep_their_extension() {
        let long = format!("{}.pdf", "a".repeat(300));
        let safe = safe_name(&long);
        assert_eq!(safe.len(), MAX_NAME);
        assert!(safe.ends_with("a.pdf"));
        // Cut on a character boundary, never inside one.
        let wide = format!("{}.txt", "é".repeat(150));
        let safe = safe_name(&wide);
        assert!(safe.len() <= MAX_NAME && safe.ends_with(".txt"), "{safe}");
        // No real extension: cut the whole name.
        assert_eq!(safe_name(&"b".repeat(300)).len(), MAX_NAME);
    }

    #[tokio::test]
    async fn a_taken_name_is_never_reused() {
        let dir = std::env::temp_dir().join(format!("noslacking-names-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let first = create_unique(&dir, |n| numbered("a.txt", n))
            .await
            .expect("first");
        let second = create_unique(&dir, |n| numbered("a.txt", n))
            .await
            .expect("second");
        assert_eq!(first.0, dir.join("a.txt"));
        assert_eq!(second.0, dir.join("a (1).txt"));
        drop((first, second));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn taken_names_get_a_number_before_the_extension() {
        assert_eq!(numbered("report.pdf", 0), "report.pdf");
        assert_eq!(numbered("report.pdf", 2), "report (2).pdf");
        assert_eq!(numbered("archive.tar.gz", 1), "archive.tar (1).gz");
        assert_eq!(numbered("README", 3), "README (3)");
    }
}
