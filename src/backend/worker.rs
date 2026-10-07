//! The worker: owns every workspace's API client, the Socket Mode
//! connection and sign-in, turns commands into API calls and API answers
//! and events into [`Event`]s.
//!
//! Its loop only dispatches. Anything that waits on the network runs as a
//! task of its own and reports back through `Internal` or straight to the
//! interface, so one slow call never holds up another.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use serde_json::Value;
use tokio::sync::{mpsc, watch};

use super::api::{failure, worth_retrying};
use super::fetch::{
    Boot, boot, conversation_info, conversations, edit_sidebar, history, sections, thread,
    workspace_details,
};
use super::files::{
    Attachment, Destination, UploadGate, download, fetch_bytes, file_name, open_file, upload, view,
};
use super::translate::{Translated, str_of, translate};
use super::{Change, Command, Event, Gate, SignIn, Sink, Socket};
use crate::auth::{Flow, SignedIn};
use crate::credentials::{AppCredentials, Credentials};
use crate::failure::{Doing, Failure, Problem};
use crate::images::ImageLoader;
use crate::model::{Ts, Workspace};
use crate::notice::Notice;
use crate::offline::Cache;
use crate::paths::AppDirs;
use crate::settings::WorkspaceMeta;
use crate::slack::socket::{self, SocketEvent};
use crate::slack::{Client, SlackError, Token, types};

mod signin;

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
    (BOOT_RETRY_FIRST * 2u32.pow(doublings)).min(BOOT_RETRY_MAX)
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

/// An upload still running.
struct Running {
    /// Its task, to stop it.
    task: tokio::task::AbortHandle,
    /// Whether it may still be stopped.
    gate: UploadGate,
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
    client_msg_id: Option<String>,
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
    /// The huddle being listened to (`huddle-audio`).
    #[cfg(feature = "huddle-audio")]
    huddle_audio: super::listen::Listener,
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
            #[cfg(feature = "huddle-audio")]
            huddle_audio: super::listen::Listener::default(),
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
        let session = client.token().is_session();
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

    /// Starts using a signed-in Microsoft Teams workspace.
    #[cfg(feature = "teams")]
    fn add_teams(&mut self, workspace: Workspace, creds: crate::teams::auth::TeamsCredentials) {
        let (sink, gate) = self.sink.gated();
        let client = super::teams::client(
            creds,
            &workspace.team_id,
            self.credentials.clone(),
            sink.clone(),
        );
        self.sink.send(Event::WorkspaceReady(workspace.clone()));
        self.start_teams(workspace, client, (sink, gate));
    }

    /// Starts a Teams workspace's lists and live connection, replacing
    /// whatever ran for it before.
    #[cfg(feature = "teams")]
    fn start_teams(
        &mut self,
        workspace: Workspace,
        client: crate::teams::client::TeamsClient,
        gated: (Sink, Gate),
    ) {
        let generation = self.generation();
        let report = self.trouter_report(&workspace.team_id, generation);
        let session =
            super::teams::Session::start(workspace.clone(), client, gated, generation, report);
        if let Some(old) = self
            .workspaces
            .insert(workspace.team_id, Backend::Teams(session))
        {
            old.shut();
        }
        self.report_socket();
    }

    /// Where the Trouter task started as `generation` for `team` reports.
    #[cfg(feature = "teams")]
    fn trouter_report(&self, team: &str, generation: u64) -> super::teams::Report {
        let internal = self.internal.clone();
        let team = team.to_owned();
        Arc::new(move |status| {
            let _ = internal.send(Internal::Trouter {
                team: team.clone(),
                generation,
                status,
            });
        })
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

    /// Closes a workspace's RTM socket, if it has one.
    pub(super) fn stop_rtm(&mut self, team: &str) {
        if let Some(old) = self.rtm.remove(team) {
            let _ = old.stop.send(true);
        }
    }

    /// Opens (or reopens) the RTM socket for a session workspace.
    fn start_rtm(&mut self, team: &str, client: Client) {
        self.stop_rtm(team);
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
        let (outgoing, frames) = mpsc::unbounded_channel();
        self.people.rtm_started(team, outgoing);
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
            frames,
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
        // Socket Mode serves Slack workspaces only.
        if token.is_empty() || self.slack_teams().next().is_none() {
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
        self.slack_team(team)
            .is_some_and(|t| t.client.token().is_session())
    }

    /// The real-time status of one workspace: its own RTM socket for a
    /// browser session, Trouter for Teams, the shared Socket Mode
    /// connection otherwise.
    fn status(&self, team: &str) -> Socket {
        #[cfg(feature = "teams")]
        if let Some(session) = self.teams_session(team) {
            return session.status.clone();
        }
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
            .map(|focus| focus.team.clone())
            .filter(|team| self.workspaces.contains_key(team))
            .or_else(|| self.workspaces.keys().min().cloned());
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
            Command::LoadAround { team, channel, ts } => match self.slack(&team) {
                Some((client, sink)) => {
                    tokio::spawn(super::around::around(client, team, channel, ts, sink));
                }
                None => self.history_unavailable(team, channel),
            },
            Command::Search {
                query,
                page,
                request,
            } => match self.slack(&query.team) {
                Some((client, sink)) => {
                    tokio::spawn(super::search::search(client, query, page, request, sink));
                }
                None => self.sink.send(Event::Search {
                    result: Err(self.missing(&query.team)),
                    team: query.team,
                    request,
                }),
            },
            Command::LoadNewer {
                team,
                channel,
                after,
            } => match self.slack(&team) {
                Some((client, sink)) => {
                    tokio::spawn(super::around::newer(client, team, channel, after, sink));
                }
                None => self.history_unavailable(team, channel),
            },
            Command::FetchQuote {
                team,
                channel,
                ts,
                thread,
            } => match self.slack(&team) {
                Some((client, sink)) => {
                    tokio::spawn(super::around::quote(
                        client, team, channel, ts, thread, sink,
                    ));
                }
                None => self.sink.send(Event::Quoted {
                    result: Err(self.missing(&team)),
                    team,
                    channel,
                    ts,
                }),
            },
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
            Command::Download { team, url, name } => self.download(&team, url, name),
            Command::OpenFile { team, url, name } => self.open_file(&team, url, name),
            Command::FetchAudio {
                team,
                id,
                url,
                name,
            } => self.fetch_audio(&team, id, url, name),
            Command::ViewFile {
                id,
                team,
                url,
                kind,
                size,
            } => self.view_file(id, &team, url, kind, size),
            Command::Mark { team, channel, ts } => self.mark(&team, channel, ts),
            Command::FetchUsers { team, ids } => self.fetch_users(team, ids),
            Command::FetchBots { team, ids } => self.fetch_bots(team, ids),
            Command::Sidebar { team, calls } => self.edit_sidebar(team, calls),
            Command::CloseConversation { team, channel } => self.act(
                &team,
                "conversations.close",
                vec![("channel", channel)],
                &["channel_not_found", "already_closed"],
            ),
            Command::FetchConversation { team, channel } => {
                if let Some((client, sink)) = self.slack(&team) {
                    tokio::spawn(conversation_info(client, team, channel, sink));
                } else {
                    log::debug!("not fetching {channel} in {team}: signed out");
                }
            }
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
            Command::Snooze { team, minutes } => {
                if let Some((client, sink)) = self.slack(&team) {
                    tokio::spawn(super::desktop::snooze(client, team, minutes, sink));
                }
            }
            Command::Mute {
                team,
                channel,
                muted,
                all,
            } => {
                if let Some((client, sink)) = self.slack(&team) {
                    tokio::spawn(super::desktop::mute(
                        client, team, channel, muted, all, sink,
                    ));
                }
            }
            Command::AddEmoji {
                team,
                name,
                image,
                file_name,
                mime,
            } => self.add_emoji(team, name, image, file_name, mime),
            Command::FetchEmoji { team } => {
                if let Some((client, sink)) = self.slack(&team) {
                    tokio::spawn(async move { super::fetch::emoji(&client, &team, &sink).await });
                }
            }
            Command::FetchDnd { team } => {
                if let Some((client, sink)) = self.slack(&team) {
                    tokio::spawn(super::desktop::dnd_info(client, team, sink));
                }
            }
            Command::People { team, command } => self.people_command(team, command),
            Command::Convos { team, command } => match self.slack(&team) {
                Some((client, sink)) => {
                    tokio::spawn(super::convos::run(client, team, command, sink));
                }
                None => self.sink.send(Event::Convos {
                    event: crate::convos::Event::Failed {
                        what: command.failure(),
                        error: self.missing(&team),
                    },
                    team,
                }),
            },
            Command::Views { team, command } => match self.slack(&team) {
                Some((client, sink)) => {
                    tokio::spawn(super::views::run(client, team, command, sink));
                }
                None => self.sink.send(Event::Views {
                    event: command.failed(self.missing(&team)),
                    team,
                }),
            },
        }
    }

    /// The newest page of history, or the one before `cursor`.
    fn load_history(&self, team: String, channel: String, cursor: Option<String>) {
        match self.workspaces.get(&team) {
            Some(Backend::Slack(slack)) => {
                tokio::spawn(history(
                    slack.client.clone(),
                    team,
                    channel,
                    cursor,
                    self.cache.clone(),
                    false,
                    slack.sink.clone(),
                ));
            }
            #[cfg(feature = "teams")]
            Some(Backend::Teams(session)) => {
                tokio::spawn(super::teams::history(
                    session.client.clone(),
                    team,
                    channel,
                    cursor,
                    session.sink.clone(),
                ));
            }
            None => self.history_unavailable(team, channel),
        }
    }

    fn load_thread(&self, team: String, channel: String, ts: Ts) {
        if let Some((client, sink)) = self.slack(&team) {
            tokio::spawn(thread(client, team, channel, ts, sink));
        } else {
            self.refuse(self.missing(&team), Doing::LoadThread);
        }
    }

    /// Posts a message; the answer settles the interface's optimistic copy.
    fn send(&self, outgoing: Outgoing) {
        match self.workspaces.get(&outgoing.team) {
            Some(Backend::Slack(slack)) => {
                tokio::spawn(post(slack.client.clone(), outgoing, slack.sink.clone()));
            }
            #[cfg(feature = "teams")]
            Some(Backend::Teams(session)) => {
                let post = super::teams::Post {
                    team: outgoing.team,
                    channel: outgoing.channel,
                    text: outgoing.text,
                    local: outgoing.local,
                    client_msg_id: outgoing.client_msg_id,
                    me: session.workspace.user_id.clone(),
                    me_name: super::teams::own_name(&session.workspace),
                };
                tokio::spawn(super::teams::send(
                    session.client.clone(),
                    post,
                    session.sink.clone(),
                ));
            }
            // Fail the optimistic message, or it stays pending.
            None => self.sink.send(Event::Sent {
                team: outgoing.team,
                channel: outgoing.channel,
                local: outgoing.local,
                result: Err(Failure::NotSignedIn),
            }),
        }
    }

    /// Saves an edit, delete or reaction the interface already shows,
    /// and always answers with [`Event::Settled`] so a refused change can
    /// be undone, even for a workspace that is not signed in.
    fn change(&self, team: String, channel: String, change: Change) {
        match self.workspaces.get(&team) {
            Some(Backend::Slack(slack)) => {
                let (client, sink) = (slack.client.clone(), slack.sink.clone());
                tokio::spawn(async move {
                    let (method, params, ignore) = request(&channel, &change);
                    let result = match act_with_blocks::<Value>(&client, method, &params).await {
                        Ok(_) => Ok(()),
                        // Already as asked: nothing to undo.
                        Err(SlackError::Api(code)) if ignore.contains(&code.as_str()) => Ok(()),
                        Err(error) => Err(failure(&error)),
                    };
                    sink.send(Event::Settled {
                        team,
                        channel,
                        change,
                        result,
                    });
                });
            }
            #[cfg(feature = "teams")]
            Some(Backend::Teams(session)) => {
                let me = crate::teams::client::Author {
                    id: session.workspace.user_id.clone(),
                    name: super::teams::own_name(&session.workspace),
                };
                tokio::spawn(super::teams::change(
                    session.client.clone(),
                    team,
                    channel,
                    change,
                    me,
                    session.sink.clone(),
                ));
            }
            None => self.sink.send(Event::Settled {
                team,
                channel,
                change,
                result: Err(Failure::NotSignedIn),
            }),
        }
    }

    /// `files.delete`, answered with [`Event::FileDeleteSettled`] either
    /// way, so a file hidden on screen never stays hidden after a refusal.
    fn delete_file(&self, team: String, file: String, name: String) {
        let Some((client, sink)) = self.slack(&team) else {
            let missing = self.missing(&team);
            self.sink.send(Event::FileDeleteSettled {
                team,
                file,
                name,
                result: Err(missing),
            });
            return;
        };
        tokio::spawn(async move {
            let (method, params, ignore) = delete_file_request(&file);
            let result = match client.act::<Value>(method, &params).await {
                Ok(_) => Ok(()),
                // Gone already, which is what was asked.
                Err(SlackError::Api(code)) if ignore.contains(&code.as_str()) => Ok(()),
                Err(error) => Err(failure(&error)),
            };
            sink.send(Event::FileDeleteSettled {
                team,
                file,
                name,
                result,
            });
        });
    }

    /// `emoji.add` as the web client sends it; only a browser session
    /// may, so any other sign-in is told so without asking Slack.
    fn add_emoji(
        &self,
        team: String,
        name: String,
        image: Vec<u8>,
        file_name: String,
        mime: String,
    ) {
        let answer = |sink: &Sink, team, name, result| {
            sink.send(Event::EmojiAdded { team, name, result });
        };
        let Some((client, sink)) = self.slack(&team) else {
            let missing = self.missing(&team);
            answer(&self.sink, team, name, Err(missing));
            return;
        };
        if !client.token().is_session() {
            answer(&sink, team, name, Err(Failure::NeedsSession));
            return;
        }
        tokio::spawn(async move {
            let result = client
                .add_emoji(&name, image, &file_name, &mime)
                .await
                .map_err(|e| failure(&e));
            answer(&sink, team, name, result);
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
        let Some((client, sink)) = self.slack(&team) else {
            let missing = self.missing(&team);
            self.refuse(
                missing,
                Doing::Upload {
                    name: file_name(&path),
                },
            );
            self.sink.send(Event::UploadDone { id, shared: false });
            return;
        };
        let poll_after = !self.is_live(&team);
        self.uploads
            .retain(|_, running| !running.task.is_finished());
        let gate = UploadGate::default();
        let task = {
            let gate = gate.clone();
            tokio::spawn(async move {
                let to = Destination {
                    team,
                    channel,
                    thread,
                };
                let file = Attachment { path, comment };
                let shared = upload(id, client, to, file, poll_after, gate, &sink).await;
                sink.send(Event::UploadDone { id, shared });
            })
        };
        self.uploads.insert(
            id,
            Running {
                task: task.abort_handle(),
                gate,
            },
        );
    }

    /// Stops an upload only while its gate still allows it. Once Slack is
    /// being told to share the file the cancel is ignored, and the upload
    /// ends with its own [`Event::UploadDone`], so the interface never
    /// says "cancelled" about a file that was posted.
    fn cancel_upload(&mut self, id: u64) {
        let Some(Running { task, gate }) = self.uploads.remove(&id) else {
            // Already over: its own UploadDone was sent.
            return;
        };
        if task.is_finished() {
            return;
        }
        if gate.cancel() {
            task.abort();
            self.sink.send(Event::UploadCancelled { id });
        } else {
            log::debug!("upload {id} is already being shared; not cancelled");
            self.uploads.insert(id, Running { task, gate });
        }
    }

    /// Runs a slash command: through its own Web API method where it has
    /// one, so it works with any sign-in, and otherwise through
    /// `chat.command`, Slack's own runner, which only sessions may call.
    fn slash(&self, id: u64, team: String, channel: String, command: String, text: String) {
        let Some((client, sink)) = self.slack(&team) else {
            let missing = self.missing(&team);
            self.sink.send(Event::Slash {
                id,
                command,
                result: Err(missing),
            });
            return;
        };
        tokio::spawn(async move {
            let result = run_slash(&client, &channel, &command, &text).await;
            sink.send(Event::Slash {
                id,
                command,
                result,
            });
        });
    }

    /// Presses an app's button through `blocks.actions` (see
    /// [`super::blocks`]), which only a browser session may call.
    fn press_button(&self, team: String, press: crate::model::Press) {
        let Some((client, sink)) = self.slack(&team) else {
            let missing = self.missing(&team);
            self.sink.send(Event::Pressed {
                team,
                press,
                result: Err(missing),
            });
            return;
        };
        // The press is dated like Slack's own; a clock before 1970 only
        // makes the date wrong, which Slack does not check.
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |since| {
                u64::try_from(since.as_millis()).unwrap_or(u64::MAX)
            });
        tokio::spawn(async move {
            let result = super::blocks::press(&client, &press, now_ms).await;
            if let Err(error) = &result {
                log::warn!("blocks.actions: {error:?}");
            }
            sink.send(Event::Pressed {
                team,
                press,
                result,
            });
        });
    }

    fn download(&self, team: &str, url: String, name: String) {
        let Some((client, sink)) = self.slack(team) else {
            let missing = self.missing(team);
            self.refuse(missing, Doing::Download { name });
            return;
        };
        tokio::spawn(async move {
            match download(&client, &url, &name).await {
                Ok(path) => sink.send(Event::Notice(Notice::Saved {
                    path: path.display().to_string(),
                })),
                Err(error) => sink.send(Event::Error(error)),
            }
        });
    }

    fn open_file(&self, team: &str, url: String, name: String) {
        let Some((client, sink)) = self.slack(team) else {
            let missing = self.missing(team);
            self.refuse(missing, Doing::Open { name });
            return;
        };
        let dir = self.images.open_dir(team, &url);
        tokio::spawn(async move {
            if let Err(error) = open_file(&client, dir, &url, &name).await {
                sink.send(Event::Error(error));
            }
        });
    }

    /// Fetches a sound whole, for playing in the app. Its answer always
    /// comes, so the card never waits for ever.
    fn fetch_audio(&self, team: &str, id: u64, url: String, name: String) {
        let Some((client, sink)) = self.slack(team) else {
            let missing = self.missing(team);
            let doing = Doing::Download { name };
            self.sink.send(Event::AudioFetched {
                id,
                result: Err(Problem::new(doing, missing)),
            });
            return;
        };
        tokio::spawn(async move {
            let result = fetch_bytes(&client, &url, &name, crate::audio::MAX_BYTES)
                .await
                .map(crate::audio::Bytes::from);
            sink.send(Event::AudioFetched { id, result });
        });
    }

    /// Fetches a file for the viewer and reads it off the runtime's
    /// threads.
    fn view_file(&self, id: u64, team: &str, url: String, kind: crate::viewer::Kind, size: u64) {
        let Some((client, sink)) = self.slack(team) else {
            let missing = self.missing(team);
            self.sink.send(Event::FileView {
                id,
                result: Err(missing),
            });
            return;
        };
        tokio::spawn(async move {
            let result = view(&client, &url, kind, size).await;
            sink.send(Event::FileView { id, result });
        });
    }

    /// Moves your read marker. The interface sends this on its own as you
    /// read; a workspace that is signed out has nothing to mark, and
    /// saying so on every click would only be noise.
    fn mark(&self, team: &str, channel: String, ts: Ts) {
        match self.workspaces.get(team) {
            Some(Backend::Slack(_)) => self.act(
                team,
                "conversations.mark",
                vec![("channel", channel), ("ts", ts.0)],
                &["not_in_channel", "channel_not_found"],
            ),
            #[cfg(feature = "teams")]
            Some(Backend::Teams(session)) => {
                tokio::spawn(super::teams::mark(
                    session.client.clone(),
                    team.to_owned(),
                    channel,
                    ts,
                ));
            }
            None => log::debug!("not marking read in {team}: signed out"),
        }
    }

    fn edit_sidebar(&self, team: String, calls: Vec<crate::sidebar::SidebarCall>) {
        if let Some((client, sink)) = self.slack(&team) {
            tokio::spawn(edit_sidebar(client, team, calls, sink));
        } else {
            self.refuse(self.missing(&team), Doing::ChangeSidebar);
        }
    }

    /// Opens every socket afresh and lists every workspace's conversations
    /// again, to catch up on anything missed while offline.
    fn reconnect(&mut self) {
        self.restart_socket();
        // These list their conversations as they start.
        let restarted = self.retry_boots(std::time::Instant::now(), true);
        let session_teams: Vec<(String, Client)> = self
            .slack_teams()
            .filter(|(_, team)| team.client.token().is_session())
            .map(|(id, team)| (id.clone(), team.client.clone()))
            .collect();
        for (id, client) in session_teams {
            self.start_rtm(&id, client);
        }
        for (id, team) in self
            .slack_teams()
            .filter(|(id, _)| !restarted.contains(*id))
        {
            tokio::spawn(conversations(
                team.client.clone(),
                id.clone(),
                self.cache.clone(),
                team.sink.clone(),
            ));
        }
        #[cfg(feature = "teams")]
        self.restart_teams();
    }

    /// Starts every Teams workspace again: lists it afresh and reconnects
    /// Trouter, through the proxy as now set.
    #[cfg(feature = "teams")]
    fn restart_teams(&mut self) {
        let ids: Vec<String> = self
            .workspaces
            .iter()
            .filter(|(_, backend)| matches!(backend, Backend::Teams(_)))
            .map(|(id, _)| id.clone())
            .collect();
        for id in ids {
            let generation = self.generation();
            let report = self.trouter_report(&id, generation);
            if let Some(Backend::Teams(session)) = self.workspaces.get_mut(&id) {
                session.restart(generation, report);
            }
        }
        self.report_socket();
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
        let Some((client, sink)) = self.slack(team) else {
            let missing = self.missing(team);
            self.refuse(
                missing,
                Doing::Call {
                    method: method.to_owned(),
                },
            );
            return;
        };
        tokio::spawn(async move {
            match client.act::<Value>(method, &params).await {
                Ok(_) => {}
                Err(SlackError::Api(code)) if ignore.contains(&code.as_str()) => {}
                Err(error) => sink.send(Event::Error(Problem::new(
                    Doing::Call {
                        method: method.to_owned(),
                    },
                    failure(&error),
                ))),
            }
        });
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

    fn fetch_users(&mut self, team: String, ids: Vec<String>) {
        #[cfg(feature = "teams")]
        if let Some(session) = self.teams_session(&team) {
            let (client, sink) = (session.client.clone(), session.sink.clone());
            let ids: Vec<String> = ids
                .into_iter()
                .filter(|id| self.users_requested.insert((team.clone(), id.clone())))
                .collect();
            if !ids.is_empty() {
                tokio::spawn(super::teams::fetch_users(client, team, ids, sink));
            }
            return;
        }
        let Some((client, sink)) = self.slack(&team) else {
            self.skipped("fetching people", &team);
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
        let Some((client, sink)) = self.slack(&team) else {
            self.skipped("fetching apps", &team);
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
            SocketEvent::Connected => {
                // The network is back: no need to wait out a retry.
                self.retry_boots(std::time::Instant::now(), true);
                Socket::Connected
            }
            SocketEvent::Disconnected(error) => Socket::Disconnected(failure(&error)),
            // Slack's own code, shown as it is: the usual words for a
            // refused token speak of signing in again, which is not what an
            // app-level token needs.
            SocketEvent::Rejected(code) => Socket::Rejected(Failure::Slack(code)),
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
            RtmEvent::Connected => {
                self.retry_boots(std::time::Instant::now(), true);
                self.people.rtm_live(team, true);
                // Huddles may have changed unheard while it was down.
                self.sink.send(Event::People {
                    team: team.to_owned(),
                    event: crate::people::Event::Reconnected,
                });
                Socket::Connected
            }
            RtmEvent::Disconnected(error) => {
                self.people.rtm_live(team, false);
                Socket::Disconnected(failure(&error))
            }
            RtmEvent::Unavailable(reason) => {
                // Slack will not give this session a socket. Not an outage:
                // poll the open conversation and say so calmly.
                log::info!("RTM unavailable for {team}, polling instead: {reason}");
                self.people.rtm_gone(team);
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
        let Some(me) = self.slack_team(team).map(|t| t.workspace.user_id.clone()) else {
            log::debug!("event for a workspace not signed in here");
            return;
        };
        // A huddle's message, besides the message itself.
        if let Some(event) = super::people::huddle_in_message(event) {
            self.sink.send(Event::People {
                team: team.to_owned(),
                event,
            });
        }
        for translated in translate(team, &me, event) {
            match translated {
                Translated::Event(event) => self.sink.send(event),
                Translated::Refresh(channel) => {
                    if let Some((client, sink)) = self.slack(team) {
                        tokio::spawn(conversation_info(client, team.to_owned(), channel, sink));
                    }
                }
                Translated::RefreshSections => {
                    if let Some((client, sink)) = self.slack(team) {
                        tokio::spawn(sections(client, team.to_owned(), sink));
                    }
                }
                Translated::RefreshEmoji => {
                    if let Some((client, sink)) = self.slack(team) {
                        let team = team.to_owned();
                        tokio::spawn(
                            async move { super::fetch::emoji(&client, &team, &sink).await },
                        );
                    }
                }
                Translated::RefreshPrefs => {
                    if let Some((client, sink)) = self.slack(team)
                        && client.token().is_session()
                    {
                        tokio::spawn(super::desktop::prefs(client, team.to_owned(), sink));
                    }
                }
            }
        }
    }

    /// Runs a command about people (see [`crate::people`]).
    fn people_command(&mut self, team: String, command: crate::people::Command) {
        let Some((client, sink)) = self.slack(&team) else {
            self.skipped("acting on people", &team);
            return;
        };
        #[cfg(feature = "huddle-audio")]
        let command = match command {
            crate::people::Command::ListenHuddle { channel } => {
                self.huddle_audio.start(client, team, channel, sink);
                return;
            }
            crate::people::Command::LeaveHuddle => {
                self.huddle_audio.stop();
                return;
            }
            crate::people::Command::MuteHuddle { muted } => {
                self.huddle_audio.set_muted(muted);
                return;
            }
            #[cfg(feature = "huddle-video")]
            crate::people::Command::WatchShare { share } => {
                self.huddle_audio.watch_share(share);
                return;
            }
            other => other,
        };
        if let Some(command) = super::people::call(client, team.clone(), command, sink) {
            self.people.command(&team, command);
        }
    }

    /// Asks about the presence of people on screen where nothing tells us
    /// when it changes.
    fn poll_presence(&mut self) {
        let teams: HashMap<String, (Client, Sink)> = self
            .slack_teams()
            .map(|(id, t)| (id.clone(), (t.client.clone(), t.sink.clone())))
            .collect();
        self.people
            .poll(std::time::Instant::now(), |team| teams.get(team).cloned());
    }

    /// What runs on each poll tick: the open conversation, then the watch
    /// over every workspace's other conversations.
    fn poll(&mut self) {
        self.retry_boots(std::time::Instant::now(), false);
        self.poll_open();
        self.watch_all(std::time::Instant::now());
    }

    /// Starts a round of the watch over every conversation (see
    /// [`super::poll`]) for each workspace whose socket is down and whose
    /// rest is over, and stops the round of each whose socket is back:
    /// live events tell everything from then on.
    fn watch_all(&mut self, now: std::time::Instant) {
        let live: HashSet<String> = self
            .slack_teams()
            .map(|(id, _)| id)
            .filter(|team| self.is_live(team))
            .cloned()
            .collect();
        let slack = self
            .workspaces
            .iter_mut()
            .filter_map(|(id, backend)| backend.as_slack_mut().map(|team| (id, team)));
        for (id, team) in slack {
            let running = team.watching.as_ref().is_some_and(|r| !r.is_finished());
            if live.contains(id) {
                if let Some(round) = team.watching.take() {
                    round.abort();
                }
                continue;
            }
            if running {
                continue;
            }
            // Held by a round that was stopped but has not let go yet.
            let Ok(mut state) = team.watch.clone().try_lock_owned() else {
                continue;
            };
            if !state.due(now) {
                continue;
            }
            let open = self
                .focus
                .as_ref()
                .filter(|focus| focus.team == *id)
                .and_then(|focus| focus.channel.clone());
            let (client, sink, team_id) = (team.client.clone(), team.sink.clone(), id.clone());
            let round = tokio::spawn(async move {
                super::poll::round(client, team_id, open, &mut state, sink).await;
            });
            team.watching = Some(round.abort_handle());
        }
    }

    /// Without a live socket for its workspace, the open conversation is
    /// fetched again now and then, so new messages still show up.
    ///
    /// Only one poll runs at a time: under a rate limit one call can take
    /// longer than the poll interval, and stacking more on top would only
    /// deepen the limit.
    fn poll_open(&mut self) {
        if self
            .polling
            .as_ref()
            .is_some_and(|task| !task.is_finished())
        {
            return;
        }
        let Some(Focus {
            team,
            channel: Some(channel),
        }) = &self.focus
        else {
            return;
        };
        if self.is_live(team) {
            return;
        }
        if let Some((client, sink)) = self.slack(team) {
            self.polling = Some(tokio::spawn(history(
                client,
                team.clone(),
                channel.clone(),
                None,
                Cache::disabled(),
                true,
                sink,
            )));
        }
    }
}

/// Adds a message's text as Slack's own composer sends it: the mrkdwn
/// `text`, which notifications and older clients show, and the same
/// message as a `rich_text` block, which Slack draws. When no block can be
/// made (see [`crate::slack::rich_out`]), the text goes alone.
pub(crate) fn with_text(params: &mut Vec<(&'static str, String)>, text: String) {
    let blocks = crate::slack::rich_out::blocks_param(&text);
    params.push(("text", text));
    if let Some(blocks) = blocks {
        params.push(("blocks", blocks));
    }
}

/// Slack's answers when it will not take a message's `blocks`.
const BLOCKS_REFUSED: [&str; 3] = [
    "invalid_blocks",
    "invalid_blocks_format",
    "msg_blocks_too_long",
];

/// Makes a call that may carry `blocks`; should Slack refuse them, the
/// same call goes again with the text alone, so a message is never lost
/// to its layout.
pub(crate) async fn act_with_blocks<T: serde::de::DeserializeOwned>(
    client: &Client,
    method: &str,
    params: &[(&'static str, String)],
) -> Result<T, SlackError> {
    match client.act::<T>(method, params).await {
        Err(SlackError::Api(code))
            if BLOCKS_REFUSED.contains(&code.as_str())
                && params.iter().any(|(name, _)| *name == "blocks") =>
        {
            log::warn!("Slack refused a message's blocks ({code}); sending its text alone");
            client.act(method, &without_blocks(params)).await
        }
        other => other,
    }
}

/// The same parameters without `blocks`.
fn without_blocks(params: &[(&'static str, String)]) -> Vec<(&'static str, String)> {
    params
        .iter()
        .filter(|(name, _)| *name != "blocks")
        .cloned()
        .collect()
}

/// Posts `outgoing` to Slack and settles the optimistic copy with the
/// answer.
async fn post(client: Client, outgoing: Outgoing, sink: Sink) {
    let Outgoing {
        team,
        channel,
        text,
        thread,
        broadcast,
        local,
        client_msg_id,
    } = outgoing;
    let params = post_params(&channel, text, thread.as_ref(), broadcast, client_msg_id);
    let result = act_with_blocks::<types::Posted>(&client, "chat.postMessage", &params)
        .await
        .map_err(|e| failure(&e))
        .and_then(|posted| {
            let mut message = posted
                .message
                .and_then(types::Message::into_model)
                .ok_or(Failure::NoMessage)?;
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
}

/// What `chat.postMessage` is given for a message.
///
/// Without `unfurl_links`, whether Slack unfurls a text-based link in a
/// post through the API depends on the token (an app's posts are not
/// unfurled unless asked). A shared message is only a link to the message
/// it quotes, so a text with a link to a Slack message asks outright.
fn post_params(
    channel: &str,
    text: String,
    thread: Option<&Ts>,
    broadcast: bool,
    client_msg_id: Option<String>,
) -> Vec<(&'static str, String)> {
    let unfurl = crate::links::has_message_link(&text);
    let mut params = vec![("channel", channel.to_owned())];
    with_text(&mut params, text);
    if let Some(id) = client_msg_id {
        params.push(("client_msg_id", id));
    }
    if unfurl {
        params.push(("unfurl_links", "true".into()));
    }
    if let Some(thread) = thread {
        params.push(("thread_ts", thread.0.clone()));
        if broadcast {
            params.push(("reply_broadcast", "true".into()));
        }
    }
    params
}

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
        Change::Edit { ts, text, .. } => {
            let mut params = vec![channel, ("ts", ts.0.clone())];
            with_text(&mut params, text.clone());
            ("chat.update", params, &[])
        }
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

/// The call that deletes file `file`, and the refusals that mean it is
/// gone already.
fn delete_file_request(
    file: &str,
) -> (
    &'static str,
    Vec<(&'static str, String)>,
    &'static [&'static str],
) {
    (
        "files.delete",
        vec![("file", file.to_owned())],
        &["file_not_found", "file_deleted"],
    )
}

/// What [`Worker::slash`] runs: the command's own method, or
/// `chat.command`. `Ok` carries Slack's reply text, when it has one.
async fn run_slash(
    client: &Client,
    channel: &str,
    command: &str,
    text: &str,
) -> Result<Option<String>, Failure> {
    let act = |method: &'static str, params: Vec<(&'static str, String)>| async move {
        client
            .act::<Value>(method, &params)
            .await
            .map(|_| None)
            .map_err(|e| failure(&e))
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
        "away" | "active" => super::people::set_away(client, command == "away")
            .await
            .map(|()| None)
            .map_err(|e| failure(&e)),
        "status" => {
            let (emoji, status) = crate::slash::status(&crate::mrkdwn::unescape(text));
            super::people::set_status(client, &emoji, &status, 0)
                .await
                .map(|()| None)
                .map_err(|e| failure(&e))
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
                return Err(Failure::NoInvitee);
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
                .map_err(|e| failure(&e))
        }
        _ => Err(Failure::NeedsSession),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deleting_a_file_names_only_the_file_and_takes_gone_as_done() {
        let (method, params, ignore) = delete_file_request("F1");
        assert_eq!(method, "files.delete");
        assert_eq!(params, vec![("file", "F1".to_owned())]);
        assert!(ignore.contains(&"file_not_found"));
        assert!(ignore.contains(&"file_deleted"));
        assert!(!ignore.contains(&"cant_delete_file"), "a refusal is undone");
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

    #[test]
    fn a_shared_message_asks_slack_to_unfurl_its_link() {
        let shared = "Look\n<https://acme.slack.com/archives/C1/p1700000000000100>";
        let blocks = crate::slack::rich_out::blocks_param(shared).expect("blocks");
        assert_eq!(
            post_params("C2", shared.into(), None, false, None),
            [
                ("channel", "C2".to_owned()),
                ("text", shared.to_owned()),
                ("blocks", blocks),
                ("unfurl_links", "true".to_owned()),
            ]
        );
        let plain = post_params(
            "C2",
            "hi".into(),
            Some(&Ts::new("1.000100")),
            true,
            Some("4f1e6b2a-0c3d-4e5f-8a9b-1c2d3e4f5a6b".into()),
        );
        assert_eq!(
            plain,
            [
                ("channel", "C2".to_owned()),
                ("text", "hi".to_owned()),
                (
                    "blocks",
                    r#"[{"elements":[{"elements":[{"text":"hi","type":"text"}],"type":"rich_text_section"}],"type":"rich_text"}]"#
                        .to_owned()
                ),
                (
                    "client_msg_id",
                    "4f1e6b2a-0c3d-4e5f-8a9b-1c2d3e4f5a6b".to_owned()
                ),
                ("thread_ts", "1.000100".to_owned()),
                ("reply_broadcast", "true".to_owned()),
            ],
            "other messages are sent as before"
        );
    }

    /// The `blocks` parameter of `params`, read as JSON.
    fn blocks_of(params: &[(&'static str, String)]) -> Option<Value> {
        params
            .iter()
            .find(|(name, _)| *name == "blocks")
            .map(|(_, json)| serde_json::from_str(json).expect("blocks are JSON"))
    }

    #[test]
    fn messages_go_with_their_rich_text_beside_the_text() {
        let wire = "*hi* <@U1>\n• one";
        let block = crate::slack::rich_out::rich_text(wire).expect("a block");
        let post = post_params("C1", wire.into(), None, false, None);
        assert!(post.contains(&("text", wire.to_owned())));
        assert_eq!(blocks_of(&post), Some(serde_json::json!([block])));
        let edit = Change::Edit {
            ts: Ts::new("1.0"),
            text: wire.into(),
            before: None,
        };
        let (method, params, _) = request("C1", &edit);
        assert_eq!(method, "chat.update");
        assert_eq!(
            params[..3],
            [
                ("channel", "C1".to_owned()),
                ("ts", "1.0".to_owned()),
                ("text", wire.to_owned())
            ]
        );
        assert_eq!(blocks_of(&params), Some(serde_json::json!([block])));
    }

    #[test]
    fn text_that_makes_no_block_goes_alone() {
        // A date has no element to keep it; blank text has nothing to lay
        // out.
        for wire in ["due <!date^1700000000^{date}|Nov 14>", "  "] {
            let post = post_params("C1", wire.into(), None, false, None);
            assert_eq!(
                post,
                [("channel", "C1".to_owned()), ("text", wire.to_owned())]
            );
        }
        let params = vec![
            ("channel", "C1".to_owned()),
            ("text", "hi".to_owned()),
            ("blocks", "[]".to_owned()),
        ];
        assert_eq!(without_blocks(&params), params[..2]);
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
