//! The worker: owns every workspace's API client, the Socket Mode
//! connection and sign-in, turns commands into API calls and API answers
//! and events into [`Event`]s.
//!
//! Its loop only dispatches. Anything that waits on the network runs as a
//! task of its own and reports back through `Internal` or straight to the
//! interface, so one slow call never holds up another.
//!
//! Here are the workspaces, the loop and the dispatch; the handlers live
//! by domain in the submodules, each a further `impl Worker`.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use tokio::sync::{mpsc, watch};

use super::api::{Call, failure};
use super::fetch::{Boot, boot, workspace_details};
use super::{Change, Command, Event, Gate, SignIn, Sink, Socket};
use crate::auth::{Flow, SignedIn};
use crate::credentials::{AppCredentials, Credentials};
use crate::failure::{Doing, Failure, Problem};
use crate::images::ImageLoader;
use crate::model::Workspace;
use crate::offline::Cache;
use crate::paths::AppDirs;
use crate::settings::WorkspaceMeta;
use crate::slack::socket::SocketEvent;
use crate::slack::{Client, Token};

mod convos;
mod files;
mod live;
mod messages;
mod people;
mod signin;
#[cfg(feature = "teams")]
mod teams;

use files::Running;
use messages::Outgoing;

/// How often the open conversation is polled while Socket Mode is down.
const POLL_EVERY: Duration = Duration::from_secs(6);
/// How often presence polling looks for people to ask about.
const PRESENCE_EVERY: Duration = Duration::from_secs(5);
/// How long after "Sign in with your browser" a handed-over link is used.
const BROWSER_SIGN_IN_WINDOW: Duration = Duration::from_secs(15 * 60);

/// One workspace's saved sign-in, as read from the keyring at start-up.
enum Stored {
    Token(Token),
    #[cfg(feature = "teams")]
    TeamsCreds(crate::teams::auth::TeamsCredentials),
    Missing,
    /// The keyring failed while reading this one.
    Failed(crate::credentials::Error),
    /// Not read, because the keyring had already failed: asking again would
    /// only repeat the failure, or the unlock prompt.
    Skipped,
    /// Not read: a Microsoft Teams sign-in, in a build without Teams.
    #[cfg(not(feature = "teams"))]
    Unsupported,
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
    SignedIn(Result<SignedIn, Failure>),
    TeamAdded {
        meta: Workspace,
        token: Token,
    },
    /// A Microsoft Teams sign-in finished.
    #[cfg(feature = "teams")]
    TeamsSignedIn(Result<(Workspace, crate::teams::auth::TeamsCredentials), Failure>),
    /// From the Trouter task of the Teams workspace `team` started as
    /// `generation`.
    #[cfg(feature = "teams")]
    Trouter {
        team: String,
        generation: u64,
        status: Socket,
    },
    /// A call rings for the Teams workspace `team`.
    #[cfg(feature = "teams")]
    IncomingCall {
        team: String,
        call: crate::teams::calling::call::Incoming,
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
    /// The browser sign-in started at this moment has had its
    /// [`BROWSER_SIGN_IN_WINDOW`].
    BrowserSignInOver(std::time::Instant),
    /// These people and apps could not be fetched for a passing reason;
    /// the next request for them should try again.
    FetchFailed {
        team: String,
        users: Vec<String>,
        bots: Vec<String>,
    },
}

/// A workspace's start-up task, and how it went once it is over.
struct Booting {
    task: tokio::task::AbortHandle,
    /// Set by the task as it ends; never set if it was stopped.
    outcome: Arc<OnceLock<Boot>>,
}

/// The first wait before starting a workspace that could not be reached
/// again, and the longest; each failure doubles it.
const BOOT_RETRY_FIRST: Duration = Duration::from_secs(10);
const BOOT_RETRY_MAX: Duration = Duration::from_secs(5 * 60);

/// When to try starting a workspace again after its start-up could not
/// reach Slack.
#[derive(Debug, Default)]
struct BootRetry {
    /// Start-ups in a row that could not reach Slack.
    failures: u32,
    /// When the next one is due, from when the last was seen to fail.
    at: Option<std::time::Instant>,
}

impl BootRetry {
    /// Whether to start again now, after a start-up that could not reach
    /// Slack: when the wait is over, or `at_once` when there is news that
    /// the network is back.
    fn due(&mut self, now: std::time::Instant, at_once: bool) -> bool {
        let failures = &mut self.failures;
        let at = *self.at.get_or_insert_with(|| {
            *failures = failures.saturating_add(1);
            now + boot_wait(*failures)
        });
        if at_once || now >= at {
            self.at = None;
            true
        } else {
            false
        }
    }
}

/// How long to wait after `failures` start-ups in a row that could not
/// reach Slack.
fn boot_wait(failures: u32) -> Duration {
    let doublings = failures.saturating_sub(1).min(10);
    crate::retry::backoff(BOOT_RETRY_FIRST, BOOT_RETRY_MAX, doublings)
}

struct Team {
    client: Client,
    /// Who and what the workspace is, as it was signed in.
    workspace: Workspace,
    /// What this workspace's tasks report through. Signing out closes
    /// `gate`, so nothing they send afterwards reaches the interface.
    sink: Sink,
    gate: Gate,
    /// The start-up work (lists, people, sections, the unread sweep),
    /// stopped on sign-out rather than left calling Slack for nothing.
    boot: Booting,
    /// When to start again if the start-up could not reach Slack.
    retry: BootRetry,
    /// The watch over every conversation while the socket is down (see
    /// [`super::poll`]). A round holds the lock while it runs, so two
    /// never overlap.
    watch: Arc<tokio::sync::Mutex<super::poll::State>>,
    /// The round running now, stopped when the socket comes back or the
    /// workspace signs out.
    watching: Option<tokio::task::AbortHandle>,
}

impl Team {
    /// A signed-in workspace with nothing watched yet.
    fn new(client: Client, workspace: Workspace, sink: Sink, gate: Gate, boot: Booting) -> Self {
        Self {
            client,
            workspace,
            sink,
            gate,
            boot,
            retry: BootRetry::default(),
            watch: Arc::new(tokio::sync::Mutex::new(super::poll::State::starting(
                std::time::Instant::now(),
            ))),
            watching: None,
        }
    }

    /// Stops everything still running for this workspace.
    fn shut(&self) {
        self.gate.close();
        self.boot.task.abort();
        if let Some(round) = &self.watching {
            round.abort();
        }
    }
}

/// A signed-in workspace, by the service behind it. An enum rather than
/// a trait object, so `match` finds every place that has to decide for
/// each service.
enum Backend {
    Slack(Team),
    #[cfg(feature = "teams")]
    Teams(super::teams::Session),
}

impl Backend {
    /// Stops everything still running for this workspace.
    fn shut(&self) {
        match self {
            Self::Slack(team) => team.shut(),
            #[cfg(feature = "teams")]
            Self::Teams(session) => session.shut(),
        }
    }

    /// What this workspace's tasks report through.
    fn sink(&self) -> &Sink {
        match self {
            Self::Slack(team) => &team.sink,
            #[cfg(feature = "teams")]
            Self::Teams(session) => &session.sink,
        }
    }

    fn as_slack(&self) -> Option<&Team> {
        match self {
            Self::Slack(team) => Some(team),
            #[cfg(feature = "teams")]
            Self::Teams(_) => None,
        }
    }

    fn as_slack_mut(&mut self) -> Option<&mut Team> {
        match self {
            Self::Slack(team) => Some(team),
            #[cfg(feature = "teams")]
            Self::Teams(_) => None,
        }
    }
}

/// What the interface has on screen.
struct Focus {
    /// The workspace.
    team: String,
    /// Its open conversation, if any.
    channel: Option<String>,
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

/// What a Slack-only command does in a workspace where it cannot run: a
/// Teams one, or one not signed in (see [`Worker::missing`]).
enum Otherwise {
    /// Says so, as a problem with what it was doing: for what the user
    /// asked for.
    Refuse(Doing),
    /// Logs it, as "not `what`": for what the interface asks on its own.
    Skip(&'static str),
    /// Answers with the event made from the workspace and why, for a
    /// command whose view waits for its answer.
    Answer(Box<dyn FnOnce(String, Failure) -> Event>),
}

/// Ends a history load of `channel`, so it does not show as loading for
/// ever.
fn history_failed(channel: String) -> Otherwise {
    Otherwise::Answer(Box::new(move |team, error| Event::HistoryFailed {
        team,
        channel,
        error,
    }))
}

pub struct Worker {
    /// The encrypted offline cache, once the keyring gave its key.
    cache: Cache,
    credentials: Credentials,
    dirs: AppDirs,
    sink: Sink,
    images: ImageLoader,
    app: Option<AppCredentials>,
    /// Every signed-in workspace, Slack or Teams, by id.
    workspaces: HashMap<String, Backend>,
    flow: Option<Flow>,
    /// When the user started a browser sign-in: until it is
    /// [`BROWSER_SIGN_IN_WINDOW`] old, a `slack://` sign-in link handed over
    /// by the desktop is used. Any other time such a link is not ours to act
    /// on, so it is ignored.
    browser_sign_in: Option<std::time::Instant>,
    /// The task borrowing the `slack://` links for the browser sign-in,
    /// which giving them back waits for, so it cannot come first.
    claiming: Option<tokio::task::JoinHandle<()>>,
    listener: Option<tokio::task::JoinHandle<()>>,
    /// The Socket Mode connection, which serves every workspace signed in
    /// through the app.
    socket: Option<Live>,
    /// Per-session-workspace RTM sockets.
    rtm: HashMap<String, Live>,
    /// The generation the next socket gets.
    next_generation: u64,
    /// What is on screen.
    focus: Option<Focus>,
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
    /// cancelled while their gate allows it.
    uploads: HashMap<u64, Running>,
    /// Presence and the like for the people on screen.
    people: super::people::Hub,
    /// The huddle being listened to.
    huddle_audio: super::listen::Listener,
    /// The Teams call going on.
    #[cfg(feature = "teams")]
    teams_call: super::teams_call::Caller,
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
            workspaces: HashMap::new(),
            flow: None,
            browser_sign_in: None,
            claiming: None,
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
            people: super::people::Hub::default(),
            huddle_audio: super::listen::Listener::default(),
            #[cfg(feature = "teams")]
            teams_call: super::teams_call::Caller::default(),
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
        let mut presence = tokio::time::interval(PRESENCE_EVERY);
        presence.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
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
                _ = presence.tick() => self.poll_presence(),
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
                } else if meta.service == crate::model::Service::Teams {
                    #[cfg(feature = "teams")]
                    match credentials.load_teams_token(&meta.team_id).await {
                        Ok(Some(creds)) => Stored::TeamsCreds(creds),
                        Ok(None) => Stored::Missing,
                        Err(error) => {
                            failed = true;
                            Stored::Failed(error)
                        }
                    }
                    // Built without Teams: the sign-in stays in the keyring
                    // for a build that has it.
                    #[cfg(not(feature = "teams"))]
                    Stored::Unsupported
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
                self.sink.send(Event::KeyringError(error.into()));
            }
        }
        for (meta, stored) in workspaces {
            let reason = match stored {
                Stored::Token(token) => {
                    let workspace = Workspace {
                        service: meta.service,
                        team_id: meta.team_id,
                        name: meta.name,
                        domain: meta.domain,
                        icon: meta.icon,
                        user_id: meta.user_id,
                        sign_in: Default::default(),
                        scopes: meta.scopes,
                    };
                    self.add_team(workspace, token);
                    continue;
                }
                #[cfg(feature = "teams")]
                Stored::TeamsCreds(creds) => {
                    let workspace = Workspace {
                        service: meta.service,
                        team_id: meta.team_id,
                        name: meta.name,
                        domain: meta.domain,
                        icon: meta.icon,
                        user_id: meta.user_id,
                        sign_in: Default::default(),
                        scopes: None,
                    };
                    self.add_teams(workspace, creds);
                    continue;
                }
                #[cfg(not(feature = "teams"))]
                Stored::Unsupported => Failure::Unsupported,
                Stored::Missing => Failure::NoSavedSignIn,
                Stored::Failed(error) => {
                    self.sink.send(Event::KeyringError(error.into()));
                    Failure::KeyringUnread(Some(error.into()))
                }
                Stored::Skipped => Failure::KeyringUnread(None),
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
        match self.workspaces.get(team) {
            Some(backend) => backend.sink().clone(),
            None => {
                let (sink, gate) = self.sink.gated();
                gate.close();
                sink
            }
        }
    }

    /// A Slack workspace's client and the sink for its tasks; `None` for
    /// a Teams workspace or one not signed in (see [`Self::missing`]).
    fn slack(&self, team: &str) -> Option<(Client, Sink)> {
        self.slack_team(team)
            .map(|t| (t.client.clone(), t.sink.clone()))
    }

    /// A signed-in Slack workspace.
    fn slack_team(&self, team: &str) -> Option<&Team> {
        self.workspaces.get(team).and_then(Backend::as_slack)
    }

    /// Every signed-in Slack workspace.
    fn slack_teams(&self) -> impl Iterator<Item = (&String, &Team)> {
        self.workspaces
            .iter()
            .filter_map(|(id, backend)| backend.as_slack().map(|team| (id, team)))
    }

    /// A signed-in Teams workspace.
    #[cfg(feature = "teams")]
    fn teams_session(&self, team: &str) -> Option<&super::teams::Session> {
        match self.workspaces.get(team) {
            Some(Backend::Teams(session)) => Some(session),
            _ => None,
        }
    }

    /// Why a Slack-only command cannot run in `team`: it is a Teams
    /// workspace, or it is not signed in here.
    fn missing(&self, team: &str) -> Failure {
        match self.workspaces.get(team) {
            Some(Backend::Slack(_)) | None => Failure::NotSignedIn,
            #[cfg(feature = "teams")]
            Some(Backend::Teams(_)) => Failure::Unsupported,
        }
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
                            reason: Some(failure(&error)),
                        }),
                        Err(_) => {}
                    }
                }
            },
        )
    }

    /// Starts using a signed-in workspace.
    fn add_team(&mut self, mut workspace: Workspace, token: Token) {
        let (sink, gate) = self.sink.gated();
        let client = self.make_client(&workspace.team_id, token, sink.clone());
        self.images.set_client(&workspace.team_id, client.clone());
        let session = client.is_session();
        // The token decides it, whatever the saved details said.
        workspace.sign_in = crate::model::SignInKind::of(session);
        // What Slack granted, until its answers say otherwise; a session
        // has no list to keep.
        if session {
            workspace.scopes = None;
        }
        client.set_scopes(workspace.scopes.clone());
        self.sink.send(Event::WorkspaceReady(workspace.clone()));
        let boot = self.spawn_boot(&client, &workspace, &sink, false);
        let replaced = self.workspaces.insert(
            workspace.team_id.clone(),
            Backend::Slack(Team::new(
                client.clone(),
                workspace.clone(),
                sink,
                gate,
                boot,
            )),
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
        } else {
            // An OAuth sign-in over a browser session: the session's socket
            // would go on with the old token, so it stops, and people's
            // presence goes back to polling.
            self.stop_rtm(&workspace.team_id);
            self.people.rtm_gone(&workspace.team_id);
        }
        self.report_socket();
    }

    /// Starts a workspace's start-up work; see [`boot`] for `retry`.
    fn spawn_boot(
        &self,
        client: &Client,
        workspace: &Workspace,
        sink: &Sink,
        retry: bool,
    ) -> Booting {
        let outcome = Arc::new(OnceLock::new());
        let set = outcome.clone();
        let started = boot(
            client.clone(),
            workspace.clone(),
            self.cache.clone(),
            sink.clone(),
            retry,
        );
        let task = tokio::spawn(async move {
            let _ = set.set(started.await);
        })
        .abort_handle();
        Booting { task, outcome }
    }

    /// Starts again each workspace whose start-up could not reach Slack,
    /// once its wait is over or, `at_once`, now. Without this a workspace
    /// that started offline would never load its emoji, people, sections
    /// or unread state. Answers the workspaces started again.
    fn retry_boots(&mut self, now: std::time::Instant, at_once: bool) -> HashSet<String> {
        let due: Vec<String> = self
            .workspaces
            .iter_mut()
            .filter_map(|(id, backend)| backend.as_slack_mut().map(|team| (id, team)))
            .filter(|(_, team)| team.boot.outcome.get() == Some(&Boot::Unreached))
            .filter_map(|(id, team)| team.retry.due(now, at_once).then(|| id.clone()))
            .collect();
        for id in &due {
            let Some(team) = self.slack_team(id) else {
                continue;
            };
            log::info!("starting {id} again: Slack could not be reached before");
            let boot = self.spawn_boot(&team.client, &team.workspace, &team.sink, true);
            if let Some(team) = self.workspaces.get_mut(id).and_then(Backend::as_slack_mut) {
                team.boot = boot;
            }
        }
        due.into_iter().collect()
    }

    /// Dispatches one command to its handler. Nothing here waits on the
    /// network: each handler starts a task and returns.
    fn command(&mut self, command: Command) {
        match command {
            Command::Devices(crate::devices::Command::List(kind)) => {
                tokio::spawn(super::devices::list(kind, self.sink.clone()));
            }
            Command::Devices(crate::devices::Command::Use(chosen)) => {
                #[cfg(feature = "teams")]
                self.teams_call.use_devices(chosen.clone());
                self.huddle_audio.use_devices(chosen);
            }
            Command::SaveApp(app) => self.save_app(app),
            Command::StartSignIn {
                redirect,
                port,
                request,
            } => self.start_sign_in(redirect, port, request),
            Command::CancelSignIn => self.cancel_sign_in(),
            Command::Callback(url) => self.callback(url),
            Command::PasteToken(token) => self.paste_token(token),
            Command::SignInLink(link) => self.sign_in_link(&link),
            Command::StartBrowserSignIn => self.start_browser_sign_in(),
            Command::StartTeamsSignIn { tenant, personal } => {
                self.start_teams_sign_in(tenant, personal);
            }
            Command::SignOut(team) => self.sign_out(&team),
            Command::Focus { team, channel } => {
                self.focus = Some(Focus { team, channel });
                self.report_socket();
            }
            Command::LoadHistory { team, channel } => self.load_history(team, channel, None),
            Command::LoadOlder {
                team,
                channel,
                cursor,
            } => self.load_history(team, channel, Some(cursor)),
            Command::LoadThread { team, channel, ts } => self.load_thread(team, channel, ts),
            Command::LoadAround { team, channel, ts } => self.load_around(team, channel, ts),
            Command::Search {
                query,
                page,
                request,
            } => self.search(query, page, request),
            Command::LoadNewer {
                team,
                channel,
                after,
            } => self.load_newer(team, channel, after),
            Command::FetchQuote {
                team,
                channel,
                ts,
                thread,
            } => self.fetch_quote(team, channel, ts, thread),
            Command::Send {
                team,
                channel,
                text,
                thread,
                broadcast,
                local,
                client_msg_id,
            } => self.send(Outgoing {
                team,
                channel,
                text,
                thread,
                broadcast,
                local,
                client_msg_id,
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
            Command::DeleteFile { team, file, name } => self.delete_file(team, file, name),
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
                id,
                team,
                channel,
                command,
                text,
            } => self.slash(id, team, channel, command, text),
            Command::PressButton { team, press } => self.press_button(team, press),
            Command::CancelUpload { id } => self.cancel_upload(id),
            Command::Download { team, url, name } => self.download(team, url, name),
            Command::OpenFile { team, url, name } => self.open_file(team, url, name),
            Command::FetchAudio {
                team,
                id,
                url,
                name,
            } => self.fetch_audio(team, id, url, name),
            Command::ViewFile {
                id,
                team,
                url,
                kind,
                size,
            } => self.view_file(id, team, url, kind, size),
            Command::Mark { team, channel, ts } => self.mark(team, channel, ts),
            Command::FetchUsers { team, ids } => self.fetch_users(team, ids),
            Command::FetchBots { team, ids } => self.fetch_bots(team, ids),
            Command::Sidebar { team, calls } => self.edit_sidebar(team, calls),
            Command::CloseConversation { team, channel } => self.close_conversation(team, channel),
            Command::FetchConversation { team, channel } => self.fetch_conversation(team, channel),
            Command::Reconnect => self.reconnect(),
            Command::SetProxy(proxy) => match crate::slack::net::configure(&proxy) {
                // New clients only help once the sockets reconnect on them.
                Ok(()) => self.reconnect(),
                Err(error) => {
                    log::info!("proxy not used: {error}");
                    self.sink.send(Event::Error(Problem::new(
                        Doing::UseProxy,
                        Failure::BadProxy,
                    )));
                }
            },
            Command::Snooze { team, minutes } => self.spawn_slack(
                team,
                Otherwise::Refuse(Doing::Snooze),
                move |client, team, sink| super::desktop::snooze(client, team, minutes, sink),
            ),
            Command::Mute {
                team,
                channel,
                muted,
                all,
            } => self.spawn_slack(
                team,
                Otherwise::Refuse(Doing::ChangeMute),
                move |client, team, sink| {
                    super::desktop::mute(client, team, channel, muted, all, sink)
                },
            ),
            Command::AddEmoji {
                team,
                name,
                image,
                file_name,
                mime,
            } => self.add_emoji(team, name, image, file_name, mime),
            Command::FetchEmoji { team } => self.spawn_slack(
                team,
                Otherwise::Skip("fetching emoji"),
                |client, team, sink| async move { super::fetch::emoji(&client, &team, &sink).await },
            ),
            Command::FetchDnd { team } => self.spawn_slack(
                team,
                Otherwise::Skip("fetching Do Not Disturb"),
                super::desktop::dnd_info,
            ),
            Command::People { team, command } => self.people_command(team, command),
            Command::Convos { team, command } => self.convos(team, command),
            Command::Views { team, command } => self.views(team, command),
        }
    }

    /// Makes a call for its effect, reporting failures (but those that
    /// mean it is done already) as a problem with `doing`.
    fn act(&self, team: String, doing: Doing, call: Call) {
        self.spawn_slack(
            team,
            Otherwise::Refuse(doing.clone()),
            |client, _, sink| async move {
                if let Err(error) = call.run(&client).await {
                    sink.send(Event::Error(Problem::new(doing, failure(&error))));
                }
            },
        );
    }

    /// Starts `work` with `team`'s Slack client, or, for a Teams
    /// workspace or one not signed in, does what `otherwise` says.
    fn spawn_slack<Fut>(
        &self,
        team: String,
        otherwise: Otherwise,
        work: impl FnOnce(Client, String, Sink) -> Fut,
    ) where
        Fut: std::future::Future<Output = ()> + Send + 'static,
    {
        match self.slack(&team) {
            Some((client, sink)) => {
                tokio::spawn(work(client, team, sink));
            }
            None => {
                let why = self.missing(&team);
                match otherwise {
                    Otherwise::Refuse(doing) => self.refuse(why, doing),
                    Otherwise::Skip(what) => self.skipped(what, &team),
                    Otherwise::Answer(answer) => self.sink.send(answer(team, why)),
                }
            }
        }
    }

    /// Runs `work` with `team`'s Slack client and sends `answer` with its
    /// result; for a Teams workspace or one not signed in, `answer` says
    /// why it could not run, so the interface is never left waiting.
    fn answer_slack<T, Fut>(
        &self,
        team: String,
        work: impl FnOnce(Client) -> Fut,
        answer: impl FnOnce(String, Result<T, Failure>) -> Event + Send + 'static,
    ) where
        Fut: std::future::Future<Output = Result<T, Failure>> + Send + 'static,
        T: Send + 'static,
    {
        match self.slack(&team) {
            Some((client, sink)) => {
                let work = work(client);
                tokio::spawn(async move {
                    let result = work.await;
                    sink.send(answer(team, result));
                });
            }
            None => {
                let why = self.missing(&team);
                self.sink.send(answer(team, Err(why)));
            }
        }
    }

    /// Says that `doing` cannot be done, and why (see [`Self::missing`]),
    /// rather than dropping the command without a word.
    fn refuse(&self, why: Failure, doing: Doing) {
        self.sink.send(Event::Error(Problem::new(doing, why)));
    }

    /// Ends a history load that cannot run here, so the conversation does
    /// not show as loading for ever.
    fn history_unavailable(&self, team: String, channel: String) {
        self.sink.send(Event::HistoryFailed {
            error: self.missing(&team),
            team,
            channel,
        });
    }

    /// Logs that a command the interface sends on its own (people, apps)
    /// was skipped. A Teams workspace has no Slack people to fetch, which
    /// is expected and not worth a line each time.
    fn skipped(&self, what: &str, team: &str) {
        if self.missing(team) == Failure::NotSignedIn {
            log::debug!("not {what} in {team}: signed out");
        }
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
            Internal::FetchFailed { team, .. } if !self.workspaces.contains_key(&team) => {}
            Internal::FetchFailed { team, users, bots } => {
                for id in users {
                    self.users_requested.remove(&(team.clone(), id));
                }
                for id in bots {
                    self.bots_requested.remove(&(team.clone(), id));
                }
            }
            Internal::Callback(url) => self.callback(url),
            Internal::BrowserSignInOver(started) => {
                if self.browser_sign_in == Some(started) {
                    log::info!("the browser sign-in ran out of time");
                    self.end_browser_sign_in();
                }
            }
            Internal::SignInListenerFailed(error) => {
                self.sink
                    .send(Event::SignIn(SignIn::Failed(Failure::NoListener(error))));
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
                        sink.send(Event::KeyringError(error.into()));
                    }
                    let client = Client::new(http, signed.token.clone());
                    let mut meta =
                        workspace_details(&client, &signed.team_id, &signed.user_id).await;
                    // The OAuth answer's list, or else the one Slack's
                    // answers to the calls just made carried.
                    if signed.scopes.is_some() {
                        meta.scopes = signed.scopes;
                    }
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
            #[cfg(feature = "teams")]
            Internal::TeamsSignedIn(result) => self.teams_signed_in(result),
            #[cfg(feature = "teams")]
            Internal::Trouter {
                team,
                generation,
                status,
            } => {
                match self.workspaces.get_mut(&team) {
                    Some(Backend::Teams(session)) if session.generation == generation => {
                        session.status = status;
                    }
                    _ => log::debug!("ignoring a report from a replaced Trouter connection"),
                }
                self.report_socket();
            }
            #[cfg(feature = "teams")]
            Internal::IncomingCall { team, call } => {
                if let Some(session) = self.teams_session(&team) {
                    let (client, sink) = (session.client.clone(), session.sink.clone());
                    self.teams_call.ring(client, team, call, sink);
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::api::worth_retrying;
    use crate::model::Ts;
    use crate::slack::SlackError;

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

    /// Signs a Teams workspace in, with nothing running behind it.
    /// Answers the generation its Trouter reports carry.
    #[cfg(feature = "teams")]
    fn teams(worker: &mut Worker, id: &str) -> u64 {
        let workspace = Workspace {
            service: crate::model::Service::Teams,
            team_id: id.to_owned(),
            name: id.to_owned(),
            domain: String::new(),
            icon: None,
            user_id: "me".into(),
            sign_in: Default::default(),
            scopes: None,
        };
        let generation = worker.generation();
        let session = super::super::teams::Session::idle(
            workspace,
            crate::teams::client::TeamsClient::new(Default::default()),
            worker.sink.gated(),
            generation,
        );
        worker
            .workspaces
            .insert(id.to_owned(), Backend::Teams(session));
        generation
    }

    /// Closing a Teams chat is the sidebar's alone: nothing is asked of
    /// Microsoft, and no error comes back.
    #[cfg(feature = "teams")]
    #[tokio::test]
    async fn closing_a_teams_chat_says_nothing() {
        let (mut worker, events) = worker();
        worker.waiting = None;
        teams(&mut worker, "TT");
        worker.command(Command::CloseConversation {
            team: "TT".into(),
            channel: "19:abc@unq.gbl.spaces".into(),
        });
        tokio::task::yield_now().await;
        let events: Vec<Event> = events.try_iter().collect();
        assert!(events.is_empty(), "{events:?}");
    }

    /// The next event, waiting for tasks the worker started to send it.
    #[cfg(feature = "teams")]
    async fn next_event(events: &std::sync::mpsc::Receiver<Event>) -> Option<Event> {
        for _ in 0..100 {
            if let Ok(event) = events.try_recv() {
                return Some(event);
            }
            tokio::task::yield_now().await;
        }
        None
    }

    #[cfg(feature = "teams")]
    #[tokio::test]
    async fn slack_only_commands_say_teams_cannot_do_them() {
        let (mut worker, events) = worker();
        teams(&mut worker, "teams_me");
        for team in ["teams_me", "T404"] {
            worker.command(Command::DeleteFile {
                team: team.into(),
                file: "F1".into(),
                name: "a.txt".into(),
            });
        }
        let answers: Vec<(String, Result<(), Failure>)> = events
            .try_iter()
            .filter_map(|event| match event {
                Event::FileDeleteSettled { team, result, .. } => Some((team, result)),
                _ => None,
            })
            .collect();
        assert_eq!(
            answers,
            vec![
                ("teams_me".to_owned(), Err(Failure::Unsupported)),
                ("T404".to_owned(), Err(Failure::NotSignedIn)),
            ]
        );
    }

    #[cfg(feature = "teams")]
    #[tokio::test]
    async fn slack_only_people_commands_are_answered_in_teams() {
        let (mut worker, events) = worker();
        teams(&mut worker, "teams_me");
        worker.command(Command::People {
            team: "teams_me".into(),
            command: crate::people::Command::SetStatus {
                emoji: ":palm_tree:".into(),
                text: "Away".into(),
                expiration: 0,
            },
        });
        worker.command(Command::People {
            team: "teams_me".into(),
            command: crate::people::Command::Typing {
                channel: "19:a@thread.v2".into(),
                thread: None,
            },
        });
        let events: Vec<Event> = events.try_iter().collect();
        assert!(
            matches!(
                &events[..],
                [Event::People {
                    event: crate::people::Event::StatusSet {
                        result: Err(Failure::Unsupported)
                    },
                    ..
                }]
            ),
            "{events:?}"
        );
    }

    #[cfg(feature = "teams")]
    #[tokio::test]
    async fn a_failed_teams_edit_is_settled_so_it_is_undone() {
        let (mut worker, events) = worker();
        teams(&mut worker, "teams_me");
        worker.command(Command::Edit {
            team: "teams_me".into(),
            channel: "19:a@thread.v2".into(),
            ts: Ts::new("1700000000.123000"),
            text: "changed".into(),
            before: None,
        });
        match next_event(&events).await {
            Some(Event::Settled { team, result, .. }) => {
                assert_eq!(team, "teams_me");
                // Signed in with no skype token: refused before any request.
                assert_eq!(result, Err(Failure::NoSavedSignIn));
            }
            other => panic!("expected Settled, got {other:?}"),
        }
    }

    #[cfg(feature = "teams")]
    #[tokio::test]
    async fn trouter_reports_show_as_the_socket_status() {
        let (mut worker, events) = worker();
        let generation = teams(&mut worker, "teams_me");
        worker.command(Command::Focus {
            team: "teams_me".into(),
            channel: None,
        });
        worker.internal(Internal::Trouter {
            team: "teams_me".into(),
            generation,
            status: Socket::Connected,
        });
        let reported: Vec<Socket> = events
            .try_iter()
            .filter_map(|event| match event {
                Event::Socket(status) => Some(status),
                _ => None,
            })
            .collect();
        assert_eq!(reported.last(), Some(&Socket::Connected));

        // A replaced connection's last words change nothing.
        worker.internal(Internal::Trouter {
            team: "teams_me".into(),
            generation: generation - 1,
            status: Socket::Disconnected(Failure::Http(500)),
        });
        assert_eq!(worker.status("teams_me"), Socket::Connected);
        assert!(
            !events
                .try_iter()
                .any(|event| matches!(event, Event::Socket(_)))
        );
    }

    #[cfg(feature = "teams")]
    #[tokio::test]
    async fn signing_out_of_teams_forgets_the_workspace() {
        let (mut worker, events) = worker();
        teams(&mut worker, "teams_me");
        worker.sign_out("teams_me");
        assert!(!worker.workspaces.contains_key("teams_me"));
        assert!(events.try_iter().any(|event| matches!(
            event,
            Event::SignedOut { team, reason: None } if team == "teams_me"
        )));
        // Gone means not signed in, no longer "Teams cannot".
        assert_eq!(worker.missing("teams_me"), Failure::NotSignedIn);
    }

    fn team(worker: &mut Worker, id: &str, token: Token) {
        let client = Client::new(reqwest::Client::new(), token);
        let (sink, gate) = worker.sink.gated();
        let boot = Booting {
            task: tokio::spawn(async {}).abort_handle(),
            outcome: Arc::new(OnceLock::from(Boot::Done)),
        };
        let workspace = Workspace {
            service: crate::model::Service::Slack,
            team_id: id.to_owned(),
            name: id.to_owned(),
            domain: String::new(),
            icon: None,
            user_id: "U1".into(),
            sign_in: Default::default(),
            scopes: None,
        };
        worker.workspaces.insert(
            id.to_owned(),
            Backend::Slack(Team::new(client, workspace, sink, gate, boot)),
        );
    }

    #[test]
    fn an_unreached_start_up_is_tried_again_later_and_later() {
        let start = std::time::Instant::now();
        let mut retry = BootRetry::default();
        // Seen failing: the first wait starts.
        assert!(!retry.due(start, false));
        assert!(!retry.due(start + BOOT_RETRY_FIRST / 2, false));
        assert!(retry.due(start + BOOT_RETRY_FIRST, false));
        // Failed again: twice as long.
        let later = start + BOOT_RETRY_FIRST;
        assert!(!retry.due(later, false));
        assert!(!retry.due(later + BOOT_RETRY_FIRST, false));
        assert!(retry.due(later + BOOT_RETRY_FIRST * 2, false));
        assert_eq!(boot_wait(100), BOOT_RETRY_MAX);
    }

    #[test]
    fn news_of_the_network_tries_again_at_once() {
        let start = std::time::Instant::now();
        let mut retry = BootRetry::default();
        assert!(retry.due(start, true));
        // The count still grows, so the next wait is longer.
        assert!(!retry.due(start, false));
        assert_eq!(retry.failures, 2);
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
        worker.focus = Some(Focus {
            team: "TB".into(),
            channel: None,
        });
        worker.report_socket();
        worker.focus = Some(Focus {
            team: "TA".into(),
            channel: Some("C1".into()),
        });
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
            event: RtmEvent::Disconnected(SlackError::Network("drop".into())),
        });
        assert!(!worker.is_live("TA"));
        assert!(worker.rtm.contains_key("TA"));
        // A stale Socket Mode report is ignored the same way.
        let socket = live(&mut worker, Socket::Connected);
        let generation = socket.generation;
        worker.socket = Some(socket);
        worker.internal(Internal::Socket {
            generation: generation - 1,
            event: SocketEvent::Disconnected(SlackError::Network("old".into())),
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
            service: crate::model::Service::Slack,
            team_id: id.into(),
            name: id.into(),
            domain: String::new(),
            icon: None,
            user_id: "U1".into(),
            scopes: None,
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
    async fn the_browser_sign_in_ends_on_a_cancel_or_its_time() {
        // Ending one gives the slack:// links back; nothing here claimed
        // them, so that finds nothing to do and touches nothing.
        let (mut worker, _events) = worker();
        worker.waiting = None;
        let started = std::time::Instant::now();
        worker.browser_sign_in = Some(started);
        // A pasted link that is no sign-in link keeps the wait going.
        worker.command(Command::SignInLink("slack://channel?team=T1".into()));
        assert!(worker.browser_sign_in_pending());
        // An earlier sign-in's time running out is not this one's.
        let earlier = started
            .checked_sub(std::time::Duration::from_secs(1))
            .expect("a moment earlier");
        worker.internal(Internal::BrowserSignInOver(earlier));
        assert!(worker.browser_sign_in_pending());
        worker.internal(Internal::BrowserSignInOver(started));
        assert!(!worker.browser_sign_in_pending());

        worker.browser_sign_in = Some(std::time::Instant::now());
        worker.command(Command::CancelSignIn);
        assert!(worker.browser_sign_in.is_none());
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
    async fn user_actions_in_an_unknown_workspace_say_so() {
        let (mut worker, events) = worker();
        worker.waiting = None;
        worker.command(Command::Snooze {
            team: "TX".into(),
            minutes: Some(30),
        });
        worker.command(Command::People {
            team: "TX".into(),
            command: crate::people::Command::SetAway(true),
        });
        // Asked for by the interface on its own: only logged.
        worker.command(Command::FetchEmoji { team: "TX".into() });
        worker.command(Command::People {
            team: "TX".into(),
            command: crate::people::Command::Active,
        });
        worker.command(Command::CloseConversation {
            team: "TX".into(),
            channel: "D1".into(),
        });
        let events: Vec<Event> = events.try_iter().collect();
        assert!(
            matches!(
                &events[..],
                [
                    Event::Error(Problem {
                        doing: Doing::Snooze,
                        failure: Failure::NotSignedIn
                    }),
                    Event::People {
                        event: crate::people::Event::AwaySet {
                            away: true,
                            result: Err(Failure::NotSignedIn)
                        },
                        ..
                    },
                    Event::Error(Problem {
                        doing: Doing::CloseConversation,
                        ..
                    }),
                ]
            ),
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
            client_msg_id: None,
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
        let (_, sink) = worker.slack("TA").expect("signed in");
        let pending = tokio::spawn(std::future::pending::<()>());
        if let Some(team) = worker
            .workspaces
            .get_mut("TA")
            .and_then(Backend::as_slack_mut)
        {
            team.boot.task = pending.abort_handle();
        }
        worker.sign_out("TA");
        // A task that outlived the sign-out reports a late WorkspaceReady.
        sink.send(Event::WorkspaceReady(Workspace {
            service: crate::model::Service::Slack,
            team_id: "TA".into(),
            name: "A".into(),
            domain: String::new(),
            icon: None,
            user_id: "U1".into(),
            sign_in: Default::default(),
            scopes: None,
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
}
