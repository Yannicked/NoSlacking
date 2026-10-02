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

use super::{Command, Event, SignIn, Sink, Socket};
use crate::auth::{self, Flow, SignedIn};
use crate::credentials::{AppCredentials, Credentials};
use crate::images::ImageLoader;
use crate::model::{Conversation, ConversationKind, Message, Ts, User, Workspace};
use crate::paths::AppDirs;
use crate::settings::{Redirect, WorkspaceMeta};
use crate::slack::socket::{self, SocketEvent};
use crate::slack::{Client, SlackError, Token, types};

const HISTORY_PAGE: u32 = 50;
/// How often the open conversation is polled while Socket Mode is down.
const POLL_EVERY: Duration = Duration::from_secs(6);
/// The most users fetched with `users.list` (200 per page).
const USER_PAGES: usize = 40;
const MAX_UPLOAD: u64 = 1024 * 1024 * 1024;

/// What tasks report back to the loop.
enum Internal {
    Callback(String),
    SignedIn(Result<SignedIn, String>),
    TeamAdded {
        meta: Workspace,
        token: Token,
    },
    Socket(SocketEvent),
    Rtm {
        team: String,
        event: crate::slack::rtm::RtmEvent,
    },
    SignInListenerFailed(String),
}

struct Team {
    client: Client,
    user_id: String,
}

pub struct Worker {
    http: reqwest::Client,
    credentials: Credentials,
    dirs: AppDirs,
    sink: Sink,
    images: ImageLoader,
    app: Option<AppCredentials>,
    teams: HashMap<String, Team>,
    flow: Option<Flow>,
    listener: Option<tokio::task::JoinHandle<()>>,
    socket_stop: Option<watch::Sender<bool>>,
    /// Per-session-workspace RTM sockets.
    rtm_stops: HashMap<String, watch::Sender<bool>>,
    socket_up: bool,
    focus: Option<(String, String)>,
    users_requested: HashSet<(String, String)>,
    bots_requested: HashSet<(String, String)>,
    internal: mpsc::UnboundedSender<Internal>,
    internal_rx: Option<mpsc::UnboundedReceiver<Internal>>,
}

impl Worker {
    pub fn new(
        http: reqwest::Client,
        credentials: Credentials,
        dirs: AppDirs,
        sink: Sink,
        images: ImageLoader,
    ) -> Self {
        let (internal, internal_rx) = mpsc::unbounded_channel();
        Self {
            http,
            credentials,
            dirs,
            sink,
            images,
            app: None,
            teams: HashMap::new(),
            flow: None,
            listener: None,
            socket_stop: None,
            rtm_stops: HashMap::new(),
            socket_up: false,
            focus: None,
            users_requested: HashSet::new(),
            bots_requested: HashSet::new(),
            internal,
            internal_rx: Some(internal_rx),
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
        self.start(workspaces).await;
        let mut poll = tokio::time::interval(POLL_EVERY);
        poll.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                command = commands.recv() => match command {
                    Some(command) => self.command(command).await,
                    None => break,
                },
                Some(message) = internal.recv() => self.internal(message).await,
                _ = poll.tick() => self.poll(),
            }
        }
        if let Some(stop) = self.socket_stop.take() {
            let _ = stop.send(true);
        }
        for (_, stop) in self.rtm_stops.drain() {
            let _ = stop.send(true);
        }
    }

    async fn start(&mut self, workspaces: Vec<WorkspaceMeta>) {
        match self.credentials.load_app().await {
            Ok(app) => {
                self.app = app.clone();
                self.sink.send(Event::AppLoaded(app));
            }
            Err(error) => {
                self.sink.send(Event::AppLoaded(None));
                self.sink.send(Event::KeyringError(error.to_string()));
            }
        }
        for meta in workspaces {
            match self.credentials.load_token(&meta.team_id).await {
                Ok(Some(token)) => {
                    let workspace = Workspace {
                        team_id: meta.team_id,
                        name: meta.name,
                        domain: meta.domain,
                        icon: meta.icon,
                        user_id: meta.user_id,
                    };
                    self.add_team(workspace, token);
                }
                Ok(None) => self.sink.send(Event::SignedOut {
                    team: meta.team_id,
                    reason: Some("No saved sign-in for this workspace.".into()),
                }),
                Err(error) => {
                    self.sink.send(Event::KeyringError(error.to_string()));
                    break;
                }
            }
        }
        self.restart_socket();
    }

    fn client(&self, team: &str) -> Option<Client> {
        self.teams.get(team).map(|t| t.client.clone())
    }

    fn make_client(&self, team: &str, token: Token) -> Client {
        let credentials = self.credentials.clone();
        let team = team.to_owned();
        Client::new(self.http.clone(), token).with_refresh(
            self.app.as_ref().and_then(AppCredentials::oauth),
            move |token| {
                let credentials = credentials.clone();
                let token = token.clone();
                let team = team.clone();
                tokio::spawn(async move {
                    if let Err(error) = credentials.save_token(&team, &token).await {
                        log::warn!("could not store the renewed token: {error}");
                    }
                });
            },
        )
    }

    /// Starts using a signed-in workspace.
    fn add_team(&mut self, workspace: Workspace, token: Token) {
        let client = self.make_client(&workspace.team_id, token);
        self.images.set_client(&workspace.team_id, client.clone());
        self.teams.insert(
            workspace.team_id.clone(),
            Team {
                client: client.clone(),
                user_id: workspace.user_id.clone(),
            },
        );
        let session = client.token().is_session();
        self.sink.send(Event::WorkspaceReady(workspace.clone()));
        tokio::spawn(boot(
            client.clone(),
            workspace.clone(),
            self.dirs.clone(),
            self.sink.clone(),
        ));
        if session {
            self.start_rtm(&workspace.team_id, client);
        }
    }

    /// Opens (or reopens) the RTM socket for a session workspace.
    fn start_rtm(&mut self, team: &str, client: Client) {
        if let Some(stop) = self.rtm_stops.remove(team) {
            let _ = stop.send(true);
        }
        let (stop, stopped) = watch::channel(false);
        self.rtm_stops.insert(team.to_owned(), stop);
        self.sink.send(Event::Socket(Socket::Connecting));
        let internal = self.internal.clone();
        let team = team.to_owned();
        tokio::spawn(crate::slack::rtm::run(
            client,
            move |event| {
                let _ = internal.send(Internal::Rtm {
                    team: team.clone(),
                    event,
                });
            },
            stopped,
        ));
    }

    fn restart_socket(&mut self) {
        if let Some(stop) = self.socket_stop.take() {
            let _ = stop.send(true);
        }
        self.socket_up = false;
        let token = self
            .app
            .as_ref()
            .map(|app| app.app_token.trim().to_owned())
            .unwrap_or_default();
        if token.is_empty() || self.teams.is_empty() {
            // Session workspaces have their own RTM sockets; leave their
            // status alone and only report "off" when nothing is live.
            if self.rtm_stops.is_empty() {
                self.sink.send(Event::Socket(Socket::Off));
            }
            return;
        }
        let (stop, stopped) = watch::channel(false);
        self.socket_stop = Some(stop);
        self.sink.send(Event::Socket(Socket::Connecting));
        let internal = self.internal.clone();
        tokio::spawn(socket::run(
            self.http.clone(),
            token,
            move |event| {
                let _ = internal.send(Internal::Socket(event));
            },
            stopped,
        ));
    }

    async fn command(&mut self, command: Command) {
        match command {
            Command::SaveApp(app) => {
                if let Err(error) = self.credentials.save_app(&app).await {
                    self.sink.send(Event::KeyringError(error.to_string()));
                }
                self.app = Some(app);
                // Clients pick up the new client secret for refreshes.
                let teams: Vec<_> = self
                    .teams
                    .iter()
                    .map(|(id, team)| (id.clone(), team.client.token()))
                    .collect();
                for (id, token) in teams {
                    let client = self.make_client(&id, token);
                    self.images.set_client(&id, client.clone());
                    if let Some(team) = self.teams.get_mut(&id) {
                        team.client = client;
                    }
                }
                self.restart_socket();
            }
            Command::StartSignIn { redirect, port } => self.start_sign_in(redirect, port),
            Command::CancelSignIn => {
                self.flow = None;
                if let Some(listener) = self.listener.take() {
                    listener.abort();
                }
            }
            Command::Callback(url) => self.callback(url),
            Command::PasteToken(token) => {
                let http = self.http.clone();
                let internal = self.internal.clone();
                self.sink.send(Event::SignIn(SignIn::Exchanging));
                tokio::spawn(async move {
                    let result = validate(&http, Token::plain(token.trim())).await;
                    let _ = internal.send(Internal::SignedIn(result));
                });
            }
            Command::SignInSession {
                cookie,
                workspace_url,
            } => {
                let Some(workspace_url) =
                    crate::slack::session::normalize_workspace(&workspace_url)
                else {
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
            Command::SignOut(team) => self.sign_out(&team),
            Command::Focus { team, channel } => {
                self.focus = channel.map(|channel| (team, channel));
            }
            Command::LoadHistory { team, channel } => {
                if let Some(client) = self.client(&team) {
                    tokio::spawn(history(client, team, channel, None, self.sink.clone()));
                }
            }
            Command::LoadOlder {
                team,
                channel,
                cursor,
            } => {
                if let Some(client) = self.client(&team) {
                    tokio::spawn(history(
                        client,
                        team,
                        channel,
                        Some(cursor),
                        self.sink.clone(),
                    ));
                }
            }
            Command::LoadThread { team, channel, ts } => {
                if let Some(client) = self.client(&team) {
                    tokio::spawn(thread(client, team, channel, ts, self.sink.clone()));
                }
            }
            Command::Send {
                team,
                channel,
                text,
                thread,
                broadcast,
                local,
            } => {
                if let Some(client) = self.client(&team) {
                    let sink = self.sink.clone();
                    tokio::spawn(async move {
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
            }
            Command::Edit {
                team,
                channel,
                ts,
                text,
            } => self.act(
                &team,
                "chat.update",
                vec![("channel", channel), ("ts", ts.0), ("text", text)],
                &[],
            ),
            Command::Delete { team, channel, ts } => self.act(
                &team,
                "chat.delete",
                vec![("channel", channel), ("ts", ts.0)],
                &["message_not_found"],
            ),
            Command::React {
                team,
                channel,
                ts,
                name,
                add,
            } => {
                let Some(client) = self.client(&team) else {
                    return;
                };
                let user = self
                    .teams
                    .get(&team)
                    .map(|t| t.user_id.clone())
                    .unwrap_or_default();
                let sink = self.sink.clone();
                tokio::spawn(async move {
                    let method = if add {
                        "reactions.add"
                    } else {
                        "reactions.remove"
                    };
                    let params = [
                        ("channel", channel.clone()),
                        ("timestamp", ts.0.clone()),
                        ("name", name.clone()),
                    ];
                    match client.act::<Value>(method, &params).await {
                        Ok(_) => {}
                        Err(SlackError::Api(code))
                            if code == "already_reacted" || code == "no_reaction" => {}
                        Err(error) => {
                            // Undo the optimistic change.
                            sink.send(Event::Reaction {
                                team,
                                channel,
                                ts,
                                name,
                                user,
                                added: !add,
                            });
                            sink.send(Event::Error(format!(
                                "Could not change the reaction: {}",
                                describe(&error)
                            )));
                        }
                    }
                });
            }
            Command::Upload {
                team,
                channel,
                thread,
                path,
                comment,
            } => {
                let Some(client) = self.client(&team) else {
                    return;
                };
                let sink = self.sink.clone();
                let poll_after = !self.socket_up;
                tokio::spawn(async move {
                    let name = path
                        .file_name()
                        .map_or_else(|| "file".to_owned(), |n| n.to_string_lossy().into_owned());
                    let size = tokio::fs::metadata(&path)
                        .await
                        .map(|m| m.len())
                        .unwrap_or(0);
                    if size > MAX_UPLOAD {
                        sink.send(Event::Error(format!(
                            "{name} is larger than Slack's 1 GB limit."
                        )));
                        return;
                    }
                    let bytes = match tokio::fs::read(&path).await {
                        Ok(bytes) => bytes,
                        Err(error) => {
                            sink.send(Event::Error(format!("Could not read {name}: {error}")));
                            return;
                        }
                    };
                    sink.send(Event::Notice(format!("Uploading {name}…")));
                    match client
                        .upload(
                            &channel,
                            thread.as_ref().map(Ts::as_str),
                            &name,
                            bytes,
                            &comment,
                        )
                        .await
                    {
                        Ok(()) => {
                            sink.send(Event::Notice(format!("Uploaded {name}")));
                            if poll_after {
                                history(client, team, channel, None, sink).await;
                            }
                        }
                        Err(error) => sink.send(Event::Error(format!(
                            "Could not upload {name}: {}",
                            describe(&error)
                        ))),
                    }
                });
            }
            Command::Download { team, url, name } => {
                let Some(client) = self.client(&team) else {
                    return;
                };
                let sink = self.sink.clone();
                tokio::spawn(async move {
                    match client.get_bytes(&url, MAX_UPLOAD as usize).await {
                        Ok(bytes) => match save_download(&name, &bytes) {
                            Ok(path) => {
                                sink.send(Event::Notice(format!("Saved {}", path.display())))
                            }
                            Err(error) => {
                                sink.send(Event::Error(format!("Could not save {name}: {error}")))
                            }
                        },
                        Err(error) => sink.send(Event::Error(format!(
                            "Could not download {name}: {}",
                            describe(&error)
                        ))),
                    }
                });
            }
            Command::Mark { team, channel, ts } => self.act(
                &team,
                "conversations.mark",
                vec![("channel", channel), ("ts", ts.0)],
                &["not_in_channel", "channel_not_found"],
            ),
            Command::FetchUsers { team, ids } => self.fetch_users(team, ids),
            Command::FetchBots { team, ids } => self.fetch_bots(team, ids),
            Command::Sidebar { team, calls } => {
                if let Some(client) = self.client(&team) {
                    tokio::spawn(edit_sidebar(client, team, calls, self.sink.clone()));
                }
            }
            Command::FetchConversation { team, channel } => {
                if let Some(client) = self.client(&team) {
                    tokio::spawn(conversation_info(client, team, channel, self.sink.clone()));
                }
            }
            Command::Reconnect => {
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
                        self.dirs.clone(),
                        self.sink.clone(),
                    ));
                }
            }
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
        let Some(client) = self.client(team) else {
            return;
        };
        let sink = self.sink.clone();
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

    fn fetch_users(&mut self, team: String, ids: Vec<String>) {
        let Some(client) = self.client(&team) else {
            return;
        };
        let ids: Vec<String> = ids
            .into_iter()
            .filter(|id| self.users_requested.insert((team.clone(), id.clone())))
            .collect();
        if ids.is_empty() {
            return;
        }
        let sink = self.sink.clone();
        tokio::spawn(async move {
            let mut users = Vec::new();
            for id in ids {
                match client
                    .call::<types::UserInfo>("users.info", &[("user", id.clone())])
                    .await
                {
                    Ok(info) => users.push(info.user.into_model()),
                    Err(error) => log::debug!("users.info {id}: {error}"),
                }
            }
            if !users.is_empty() {
                sink.send(Event::Users { team, users });
            }
        });
    }

    fn fetch_bots(&mut self, team: String, ids: Vec<String>) {
        let Some(client) = self.client(&team) else {
            return;
        };
        let ids: Vec<String> = ids
            .into_iter()
            .filter(|id| self.bots_requested.insert((team.clone(), id.clone())))
            .collect();
        if ids.is_empty() {
            return;
        }
        let sink = self.sink.clone();
        tokio::spawn(async move {
            let mut bots = Vec::new();
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
                    Err(error) => log::debug!("bots.info {id}: {error}"),
                }
            }
            if !bots.is_empty() {
                sink.send(Event::Bots { team, bots });
            }
        });
    }

    fn start_sign_in(&mut self, redirect: Redirect, port: u16) {
        let Some(app) = self.app.clone().filter(AppCredentials::can_sign_in) else {
            self.sink.send(Event::SignIn(SignIn::Failed(
                "Enter the Slack app's client ID and client secret first.".into(),
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
        let http = self.http.clone();
        let internal = self.internal.clone();
        tokio::spawn(async move {
            let result = auth::exchange(&http, &app, &flow, &code)
                .await
                .map_err(|e| describe(&e));
            let _ = internal.send(Internal::SignedIn(result));
        });
    }

    fn sign_out(&mut self, team: &str) {
        if let Some(stop) = self.rtm_stops.remove(team) {
            let _ = stop.send(true);
        }
        if let Some(removed) = self.teams.remove(team) {
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
        self.sink.send(Event::SignedOut {
            team: team.to_owned(),
            reason: None,
        });
        if self.teams.is_empty() {
            self.restart_socket();
        }
    }

    async fn internal(&mut self, message: Internal) {
        match message {
            Internal::Callback(url) => self.callback(url),
            Internal::SignInListenerFailed(error) => {
                self.sink.send(Event::SignIn(SignIn::Failed(format!(
                    "Could not listen for Slack's redirect: {error}. Is the port in use?"
                ))));
            }
            Internal::SignedIn(Err(error)) => self.sink.send(Event::SignIn(SignIn::Failed(error))),
            Internal::SignedIn(Ok(signed)) => {
                let http = self.http.clone();
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
                let had_socket = self.socket_stop.is_some();
                self.add_team(meta, token);
                self.sink.send(Event::SignIn(SignIn::Done(name)));
                if !had_socket {
                    self.restart_socket();
                }
            }
            Internal::Socket(event) => self.socket_event(event),
            Internal::Rtm { team, event } => self.rtm_event(&team, event),
        }
    }

    fn socket_event(&mut self, event: SocketEvent) {
        match event {
            SocketEvent::Connected => {
                self.socket_up = true;
                self.sink.send(Event::Socket(Socket::Connected));
            }
            SocketEvent::Disconnected(reason) => {
                self.socket_up = false;
                self.sink.send(Event::Socket(Socket::Disconnected(reason)));
            }
            SocketEvent::Rejected(reason) => {
                self.socket_up = false;
                self.sink.send(Event::Socket(Socket::Rejected(reason)));
            }
            SocketEvent::Event { team, event } => self.dispatch_event(&team, &event),
        }
    }

    fn rtm_event(&mut self, team: &str, event: crate::slack::rtm::RtmEvent) {
        use crate::slack::rtm::RtmEvent;
        match event {
            RtmEvent::Connected => {
                self.socket_up = true;
                self.sink.send(Event::Socket(Socket::Connected));
            }
            RtmEvent::Disconnected(reason) => {
                self.socket_up = false;
                self.sink.send(Event::Socket(Socket::Disconnected(reason)));
            }
            RtmEvent::Unavailable(reason) => {
                // Slack will not give this session a socket. Not an outage:
                // poll the open conversation and say so calmly.
                log::info!("RTM unavailable for {team}, polling instead: {reason}");
                self.socket_up = false;
                self.rtm_stops.remove(team);
                if self.rtm_stops.is_empty() && self.socket_stop.is_none() {
                    self.sink.send(Event::Socket(Socket::Off));
                }
            }
            RtmEvent::Event(event) => self.dispatch_event(team, &event),
        }
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
                    if let Some(client) = self.client(team) {
                        tokio::spawn(conversation_info(
                            client,
                            team.to_owned(),
                            channel,
                            self.sink.clone(),
                        ));
                    }
                }
                Translated::RefreshSections => {
                    if let Some(client) = self.client(team) {
                        tokio::spawn(sections(client, team.to_owned(), self.sink.clone()));
                    }
                }
            }
        }
    }

    /// Without Socket Mode, the open conversation is fetched again now and
    /// then, so new messages still show up.
    fn poll(&self) {
        if self.socket_up {
            return;
        }
        let Some((team, channel)) = &self.focus else {
            return;
        };
        if let Some(client) = self.client(team) {
            tokio::spawn(history(
                client,
                team.clone(),
                channel.clone(),
                None,
                self.sink.clone(),
            ));
        }
    }
}

/// A user-facing description of an API failure.
pub fn describe(error: &SlackError) -> String {
    match error {
        SlackError::Api(code) => match code.as_str() {
            "invalid_auth" | "not_authed" | "token_revoked" => {
                "the sign-in is no longer valid; sign in again".into()
            }
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

fn read_cache<T: serde::de::DeserializeOwned>(path: &std::path::Path) -> Option<T> {
    let bytes = std::fs::read(path).ok()?;
    serde_json::from_slice(&bytes).ok()
}

fn write_cache<T: serde::Serialize>(path: &std::path::Path, value: &T) {
    match serde_json::to_vec(value) {
        Ok(bytes) => {
            if let Err(error) = crate::paths::write_atomic(path, &bytes) {
                log::debug!("cache not written: {error}");
            }
        }
        Err(error) => log::debug!("cache not encoded: {error}"),
    }
}

/// Everything a workspace needs after signing in: cached lists first, then
/// a check of the token, fresh lists, custom emoji and unread state.
async fn boot(client: Client, workspace: Workspace, dirs: AppDirs, sink: Sink) {
    let team = workspace.team_id.clone();
    if let Some(list) = read_cache::<Vec<Conversation>>(&dirs.conversations_cache(&team)) {
        sink.send(Event::Conversations {
            team: team.clone(),
            list,
            complete: false,
        });
    }
    if let Some(users) = read_cache::<Vec<User>>(&dirs.users_cache(&team)) {
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
    let list = conversations(client.clone(), team.clone(), dirs.clone(), sink.clone()).await;
    match client.call::<types::EmojiList>("emoji.list", &[]).await {
        Ok(list) => sink.send(Event::Emoji {
            team: team.clone(),
            emoji: list.emoji,
        }),
        Err(error) => log::info!("emoji.list: {error}"),
    }
    tokio::spawn(users(client.clone(), team.clone(), dirs, sink.clone()));
    tokio::spawn(sections(client.clone(), team.clone(), sink.clone()));
    if let Some(list) = list {
        unread_sweep(client, team, list, sink).await;
    }
}

/// Every conversation you are in.
async fn conversations(
    client: Client,
    team: String,
    dirs: AppDirs,
    sink: Sink,
) -> Option<Vec<Conversation>> {
    let mut list = Vec::new();
    let mut cursor: Option<String> = None;
    loop {
        let mut params = vec![
            ("types", "public_channel,private_channel,mpim,im".to_owned()),
            ("exclude_archived", "true".to_owned()),
            ("limit", "200".to_owned()),
        ];
        if let Some(cursor) = &cursor {
            params.push(("cursor", cursor.clone()));
        }
        match client
            .call::<types::ConversationsPage>("users.conversations", &params)
            .await
        {
            Ok(page) => {
                list.extend(page.channels.into_iter().map(types::Channel::into_model));
                cursor = page.response_metadata.cursor();
                if cursor.is_none() {
                    break;
                }
            }
            Err(error) => {
                sink.send(Event::Error(format!(
                    "Could not list conversations: {}",
                    describe(&error)
                )));
                return None;
            }
        }
    }
    write_cache(&dirs.conversations_cache(&team), &list);
    sink.send(Event::Conversations {
        team,
        list: list.clone(),
        complete: true,
    });
    Some(list)
}

/// The workspace's people, page by page.
/// Your sidebar sections and starred conversations, as Slack's own client
/// gets them. `users.channelSections.list` is undocumented and only answers
/// browser sessions; anything else keeps the plain sidebar.
async fn sections(client: Client, team: String, sink: Sink) {
    // The web client sends the token in the form; do the same.
    let token = client.token().access;
    let mut all = Vec::new();
    let mut cursor: Option<String> = None;
    for _ in 0..10 {
        let mut params = vec![("token", token.clone())];
        if let Some(cursor) = &cursor {
            params.push(("cursor", cursor.clone()));
        }
        match client
            .call::<types::ChannelSectionsPage>("users.channelSections.list", &params)
            .await
        {
            Ok(page) => {
                all.extend(page.channel_sections);
                cursor = page.cursor.filter(|c| !c.is_empty());
                if cursor.is_none() {
                    break;
                }
            }
            Err(error) => {
                log::info!("no sidebar sections ({error}); using the plain sidebar");
                return;
            }
        }
    }
    let mut ordered = types::order_sections(all);
    // Slack leaves Starred empty in the section list; stars.list fills it.
    if let Some(starred) = ordered
        .iter_mut()
        .find(|s| s.kind == crate::model::SectionKind::Starred)
    {
        match client
            .call::<types::StarsList>(
                "stars.list",
                &[("token", token.clone()), ("limit", "1000".into())],
            )
            .await
        {
            Ok(stars) => starred.channel_ids = stars.conversations(),
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

async fn users(client: Client, team: String, dirs: AppDirs, sink: Sink) {
    let mut all = Vec::new();
    let mut cursor: Option<String> = None;
    for _ in 0..USER_PAGES {
        let mut params = vec![("limit", "200".to_owned())];
        if let Some(cursor) = &cursor {
            params.push(("cursor", cursor.clone()));
        }
        match client.call::<types::UsersPage>("users.list", &params).await {
            Ok(page) => {
                let users: Vec<User> = page
                    .members
                    .into_iter()
                    .map(types::User::into_model)
                    .collect();
                all.extend(users.iter().cloned());
                sink.send(Event::Users {
                    team: team.clone(),
                    users,
                });
                cursor = page.response_metadata.cursor();
                if cursor.is_none() {
                    break;
                }
            }
            Err(error) => {
                log::info!("users.list: {error}");
                break;
            }
        }
    }
    if !all.is_empty() {
        write_cache(&dirs.users_cache(&team), &all);
    }
}

/// Reads each conversation's read marker and newest message, direct
/// messages first. Slack has no single call for this, so it trickles in.
async fn unread_sweep(client: Client, team: String, mut list: Vec<Conversation>, sink: Sink) {
    list.sort_by_key(|c| match c.kind {
        ConversationKind::Direct | ConversationKind::Group => 0,
        ConversationKind::Private => 1,
        ConversationKind::Channel => 2,
    });
    for conversation in list {
        conversation_info(client.clone(), team.clone(), conversation.id, sink.clone()).await;
    }
}

async fn conversation_info(client: Client, team: String, channel: String, sink: Sink) {
    match client
        .call::<types::ChannelInfo>("conversations.info", &[("channel", channel.clone())])
        .await
    {
        Ok(info) => {
            let mut conversation = info.channel.into_model();
            if conversation.latest.is_none()
                && let Ok(page) = client
                    .call::<types::HistoryPage>(
                        "conversations.history",
                        &[("channel", channel.clone()), ("limit", "1".into())],
                    )
                    .await
            {
                conversation.latest = page.messages.first().map(|m| Ts::new(m.ts.clone()));
            }
            sink.send(Event::Conversation { team, conversation });
        }
        Err(SlackError::Api(code)) if code == "channel_not_found" => {
            sink.send(Event::ConversationGone { team, channel });
        }
        Err(error) => log::debug!("conversations.info {channel}: {error}"),
    }
}

/// A page of history: the newest one, or the one before `cursor`.
async fn history(
    client: Client,
    team: String,
    channel: String,
    cursor: Option<String>,
    sink: Sink,
) {
    let mut params = vec![
        ("channel", channel.clone()),
        ("limit", HISTORY_PAGE.to_string()),
        ("include_all_metadata", "false".to_owned()),
    ];
    let older = cursor.is_some();
    if let Some(cursor) = cursor {
        params.push(("cursor", cursor));
    }
    match client
        .call::<types::HistoryPage>("conversations.history", &params)
        .await
    {
        Ok(page) => {
            let mut messages: Vec<Message> = page
                .messages
                .into_iter()
                .filter_map(types::Message::into_model)
                .collect();
            messages.reverse();
            sink.send(Event::History {
                team,
                channel,
                messages,
                has_more: page.has_more,
                cursor: page.response_metadata.cursor(),
                older,
            });
        }
        Err(error) => sink.send(Event::HistoryFailed {
            team,
            channel,
            error: describe(&error),
        }),
    }
}

async fn thread(client: Client, team: String, channel: String, ts: Ts, sink: Sink) {
    let mut messages = Vec::new();
    let mut cursor: Option<String> = None;
    for _ in 0..10 {
        let mut params = vec![
            ("channel", channel.clone()),
            ("ts", ts.0.clone()),
            ("limit", "200".to_owned()),
        ];
        if let Some(cursor) = &cursor {
            params.push(("cursor", cursor.clone()));
        }
        match client
            .call::<types::HistoryPage>("conversations.replies", &params)
            .await
        {
            Ok(page) => {
                messages.extend(
                    page.messages
                        .into_iter()
                        .filter_map(types::Message::into_model),
                );
                cursor = page.response_metadata.cursor();
                if cursor.is_none() {
                    break;
                }
            }
            Err(error) => {
                sink.send(Event::Error(format!(
                    "Could not load the thread: {}",
                    describe(&error)
                )));
                return;
            }
        }
    }
    sink.send(Event::Thread {
        team,
        channel,
        ts,
        messages,
    });
}

fn save_download(name: &str, bytes: &[u8]) -> std::io::Result<std::path::PathBuf> {
    let dir = directories::UserDirs::new()
        .and_then(|dirs| dirs.download_dir().map(std::path::Path::to_path_buf))
        .or_else(|| directories::BaseDirs::new().map(|dirs| dirs.home_dir().to_path_buf()))
        .ok_or_else(|| std::io::Error::other("no downloads folder"))?;
    let safe = safe_name(name);
    let path = std::path::Path::new(&safe);
    let stem = path
        .file_stem()
        .map_or_else(|| safe.clone(), |s| s.to_string_lossy().into_owned());
    let extension = path.extension().map(|e| e.to_string_lossy().into_owned());
    let mut candidate = dir.join(&safe);
    let mut n = 1;
    while candidate.exists() {
        let name = match &extension {
            Some(ext) => format!("{stem} ({n}).{ext}"),
            None => format!("{stem} ({n})"),
        };
        candidate = dir.join(name);
        n += 1;
    }
    std::fs::write(&candidate, bytes)?;
    Ok(candidate)
}

/// A file name that cannot climb out of the downloads folder or hide.
fn safe_name(name: &str) -> String {
    let safe: String = name
        .chars()
        .map(|c| {
            if matches!(c, '/' | '\\' | ':' | '\0') {
                '_'
            } else {
                c
            }
        })
        .collect();
    let safe = safe.trim_start_matches('.').to_owned();
    if safe.is_empty() {
        "download".to_owned()
    } else {
        safe
    }
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
    fn new_edited_and_deleted_messages() {
        match &events(r#"{"type":"message","channel":"C1","user":"U2","text":"hi","ts":"1.0"}"#)[..]
        {
            [
                Translated::Event(Event::Message {
                    channel, message, ..
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
            [Translated::Event(Event::Message { message, .. })] => assert!(message.edited),
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

    #[test]
    fn download_names_stay_in_the_folder() {
        assert_eq!(safe_name("../../.bashrc"), "_.._.bashrc");
        assert_eq!(safe_name("report.pdf"), "report.pdf");
        assert_eq!(safe_name(".."), "download");
    }
}
