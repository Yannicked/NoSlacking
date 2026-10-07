//! Microsoft Teams behind the worker: a signed-in Teams workspace, its
//! live connection (Trouter), and the work each command asks of it.
//!
//! The worker routes by workspace (see `worker::Backend`); everything here
//! is the Teams side of that `match`, shaped like the Slack functions in
//! `backend::fetch` so a worker arm is one call. Like them, these report
//! to the workspace's gated sink and never hold the worker.

use std::sync::Arc;

use futures_util::{SinkExt, StreamExt};
use tokio_tungstenite::tungstenite::Message as WsMessage;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;

use super::teams_translate::{
    teams_id_to_ts, translate_conversation, translate_message, translate_team, translate_user,
    ts_to_teams_id,
};
use super::{Change, Event, Gate, SignIn, Sink, Socket};
use crate::credentials::Credentials;
use crate::failure::Failure;
use crate::model::{Service, Ts, Workspace};
use crate::teams::auth::{self, TeamsCredentials};
use crate::teams::client::TeamsClient;
use crate::teams::socket::{TrouterEvent, handle_frame_control, parse_frame};

/// How many messages a page of history asks for.
const PAGE: usize = 50;
/// How long to wait before connecting again after Trouter failed.
const RETRY: std::time::Duration = std::time::Duration::from_secs(5);

/// Where the Trouter task reports its status: to the worker, which shows
/// it like a Slack socket's.
pub type Report = Arc<dyn Fn(Socket) + Send + Sync>;

/// A signed-in Microsoft Teams workspace.
pub struct Session {
    pub client: TeamsClient,
    pub workspace: Workspace,
    /// What this workspace's tasks report through; closed on sign-out.
    pub sink: Sink,
    gate: Gate,
    boot: tokio::task::AbortHandle,
    trouter: tokio::task::AbortHandle,
    /// The Trouter start this session's status reports come from, so a
    /// replaced session's last words are ignored.
    pub generation: u64,
    /// What Trouter last reported.
    pub status: Socket,
}

impl Session {
    /// Starts a workspace: its first lists, and its live connection.
    pub fn start(
        workspace: Workspace,
        client: TeamsClient,
        (sink, gate): (Sink, Gate),
        generation: u64,
        report: Report,
    ) -> Self {
        let team = workspace.team_id.clone();
        let boot = tokio::spawn(boot(client.clone(), team.clone(), sink.clone())).abort_handle();
        let trouter =
            tokio::spawn(trouter(team, client.clone(), sink.clone(), report)).abort_handle();
        Self {
            client,
            workspace,
            sink,
            gate,
            boot,
            trouter,
            generation,
            status: Socket::Connecting,
        }
    }

    /// A session with nothing running, for tests: they never reach the
    /// network.
    #[cfg(test)]
    pub fn idle(
        workspace: Workspace,
        client: TeamsClient,
        (sink, gate): (Sink, Gate),
        generation: u64,
    ) -> Self {
        Self {
            client,
            workspace,
            sink,
            gate,
            boot: tokio::spawn(async {}).abort_handle(),
            trouter: tokio::spawn(async {}).abort_handle(),
            generation,
            status: Socket::Connecting,
        }
    }

    /// Starts the lists and live connection again, on the same client and
    /// sink: the client's renewals report through that sink, so it has to
    /// stay open. Reports from the old connection carry the old
    /// generation and are ignored.
    pub fn restart(&mut self, generation: u64, report: Report) {
        self.boot.abort();
        self.trouter.abort();
        let team = self.workspace.team_id.clone();
        self.boot =
            tokio::spawn(boot(self.client.clone(), team.clone(), self.sink.clone())).abort_handle();
        self.trouter = tokio::spawn(trouter(
            team,
            self.client.clone(),
            self.sink.clone(),
            report,
        ))
        .abort_handle();
        self.generation = generation;
        self.status = Socket::Connecting;
    }

    /// Stops everything still running for this workspace.
    pub fn shut(&self) {
        self.gate.close();
        self.boot.abort();
        self.trouter.abort();
    }
}

/// A client for `creds` that saves every renewal to the keyring, in
/// order, and signs the workspace out when Microsoft no longer takes the
/// refresh token, as the Slack client does for its own.
pub fn client(
    creds: TeamsCredentials,
    team: &str,
    credentials: Credentials,
    sink: Sink,
) -> TeamsClient {
    let team = team.to_owned();
    TeamsClient::new(creds).with_save(move |result| {
        let credentials = credentials.clone();
        let sink = sink.clone();
        let team = team.clone();
        async move {
            match result {
                Ok(creds) => {
                    if let Err(error) = credentials.save_teams_token(&team, &creds).await {
                        log::warn!("could not store the renewed Teams token: {error}");
                    }
                }
                Err(Failure::SignedOut) => sink.send(Event::SignedOut {
                    team,
                    reason: Some(Failure::SignedOut),
                }),
                Err(_) => {}
            }
        }
    })
}

/// Lists the chats, then the teams and their channels, then you.
async fn boot(client: TeamsClient, team: String, sink: Sink) {
    if let Err(err) = client.ensure_fresh_tokens().await {
        log::warn!("could not renew the Teams tokens of {team}: {err:?}");
    }

    match client.get_conversations(PAGE).await {
        Ok(chats) => sink.send(Event::Conversations {
            team: team.clone(),
            list: chats.iter().map(translate_conversation).collect(),
            complete: true,
        }),
        Err(err) => log::warn!("failed to get Teams conversations of {team}: {err:?}"),
    }

    match client.get_teams().await {
        Ok(teams) => {
            let (sections, channels): (Vec<_>, Vec<_>) = teams.iter().map(translate_team).unzip();
            let channels: Vec<_> = channels.into_iter().flatten().collect();
            if !channels.is_empty() {
                sink.send(Event::Conversations {
                    team: team.clone(),
                    list: channels,
                    complete: false,
                });
            }
            if !sections.is_empty() {
                sink.send(Event::Sections {
                    team: team.clone(),
                    sections,
                });
            }
        }
        Err(err) => log::warn!("failed to get the teams list of {team}: {err:?}"),
    }

    match client.get_me() {
        Ok(me) => sink.send(Event::Users {
            team,
            users: vec![translate_user(&me)],
        }),
        Err(err) => log::warn!("failed to read who is signed in to {team}: {err:?}"),
    }
}

/// The newest page of a conversation, or the one at `cursor`.
pub async fn history(
    client: TeamsClient,
    team: String,
    channel: String,
    cursor: Option<String>,
    sink: Sink,
) {
    let older = cursor.is_some();
    match client.get_messages(&channel, cursor.as_deref(), PAGE).await {
        Ok(page) => sink.send(Event::History {
            team,
            channel,
            messages: page.messages.iter().map(translate_message).collect(),
            has_more: page.older.is_some(),
            cursor: page.older,
            older,
            polled: false,
        }),
        Err(error) => sink.send(Event::HistoryFailed {
            team,
            channel,
            error,
        }),
    }
}

/// A message to post to a Teams conversation.
pub struct Post {
    pub team: String,
    pub channel: String,
    pub text: String,
    /// The interface's id for its optimistic copy.
    pub local: Ts,
    pub client_msg_id: Option<String>,
    /// Your own user id, as the posted message's author.
    pub me: String,
}

/// Posts a message; the answer settles the interface's optimistic copy.
pub async fn send(client: TeamsClient, post: Post, sink: Sink) {
    let html = crate::teams::html::text_to_teams_html(&post.text);
    let result = client
        .send_message(&post.channel, &html, post.client_msg_id.as_deref())
        .await
        .map(|id| posted(id, &post, html));
    sink.send(Event::Sent {
        team: post.team,
        channel: post.channel,
        local: post.local,
        result,
    });
}

/// The message as posted: Teams answers with its id only, so the message
/// is read back through the same translation as any other, which keeps
/// it whole as the model grows.
fn posted(id: Option<String>, post: &Post, html: String) -> crate::model::Message {
    let id = id.unwrap_or_else(|| ts_to_teams_id(&now()));
    translate_message(&crate::teams::types::Message {
        id,
        from: Some(format!("8:orgid:{}", post.me)),
        content: html,
        message_type: Some("RichText/Html".into()),
        client_message_id: post.client_msg_id.clone(),
        ..Default::default()
    })
}

/// Now, as a message time stamp.
fn now() -> Ts {
    let millis = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or_default();
    teams_id_to_ts(&millis.to_string())
}

/// Carries out a change to a message. Teams takes deletions here so far;
/// the interface offers nothing else for a Teams workspace, and anything
/// else that arrives is refused so its optimistic copy is undone.
pub async fn change(
    client: TeamsClient,
    team: String,
    channel: String,
    change: Change,
    sink: Sink,
) {
    let result = match &change {
        Change::Delete { ts, .. } => client.delete_message(&channel, &ts_to_teams_id(ts)).await,
        Change::Edit { .. } | Change::React { .. } => Err(Failure::Unsupported),
    };
    sink.send(Event::Settled {
        team,
        channel,
        change,
        result,
    });
}

/// Moves your read marker to `ts`, a message you have seen.
pub async fn mark(client: TeamsClient, team: String, channel: String, ts: Ts) {
    if let Err(error) = client
        .set_consumption_horizon(&channel, &ts_to_teams_id(&ts))
        .await
    {
        // As for Slack's quiet marks: a failed one is retried by the next.
        log::info!("could not mark {channel} read in {team}: {error:?}");
    }
}

/// Signs in with a device code: shows the code, waits for you to enter
/// it, and trades what Microsoft hands back for a skype token. Answers the
/// workspace and its credentials, which the worker starts and saves.
pub async fn sign_in(
    tenant: Option<String>,
    sink: Sink,
) -> Result<(Workspace, TeamsCredentials), Failure> {
    let http = crate::slack::net::api();
    let device = auth::start_device_code_flow(&http, tenant.as_deref()).await?;
    sink.send(Event::SignIn(SignIn::TeamsDeviceCode {
        user_code: device.user_code.clone(),
        verification_uri: device.verification_uri.clone(),
        message: device.message.clone(),
    }));
    if let Err(error) = open::that_detached(&device.verification_uri) {
        log::info!("could not open the Microsoft sign-in page: {error}");
    }
    let token = auth::poll_device_code_token(
        &http,
        &device.device_code,
        device.interval,
        device.expires_in,
        tenant.as_deref(),
    )
    .await?;
    sink.send(Event::SignIn(SignIn::Exchanging));

    let claims = auth::parse_jwt_claims(&token.access_token);
    let claim = |name: &str| {
        claims
            .as_ref()
            .and_then(|c| c.get(name))
            .and_then(|v| v.as_str())
            .map(str::to_owned)
    };
    let authz = auth::exchange_skype_token(&http, &token.access_token, false).await?;
    let creds = TeamsCredentials {
        expires_at: token.expires_in.map(|s| auth::now_secs() + s),
        skype_token: authz.tokens.and_then(|t| t.skype_token),
        tenant_id: tenant
            .or_else(|| claim("tid"))
            .or_else(|| Some(auth::DEFAULT_TENANT.to_owned())),
        region_gtms: authz.region_gtms,
        access_token: token.access_token,
        refresh_token: token.refresh_token,
        ..TeamsCredentials::default()
    };
    let me = TeamsClient::new(creds.clone()).get_me()?;
    log::info!(
        "signed in to Microsoft Teams: tenant {}",
        claim("tid").as_deref().unwrap_or("?")
    );
    let workspace = Workspace {
        service: Service::Teams,
        team_id: format!("teams_{}", me.id),
        name: me
            .display_name
            .clone()
            .unwrap_or_else(|| Service::Teams.name().to_owned()),
        domain: "teams.microsoft.com".into(),
        icon: None,
        user_id: me.id,
        sign_in: Default::default(),
        scopes: None,
    };
    Ok((workspace, creds))
}

/// Keeps the Trouter connection open, reconnecting when it drops, and
/// passes live messages on. Reports each change of status.
async fn trouter(team: String, client: TeamsClient, sink: Sink, report: Report) {
    let http = crate::slack::net::api();
    loop {
        report(Socket::Connecting);
        match trouter_once(&team, &client, &http, &sink, &report).await {
            Ok(()) => log::info!("Trouter closed for {team}, reconnecting"),
            Err(error) => {
                log::warn!("Trouter failed for {team}: {error:?}, retrying");
                report(Socket::Disconnected(error));
            }
        }
        tokio::time::sleep(RETRY).await;
    }
}

/// One Trouter connection, from negotiation until it closes.
async fn trouter_once(
    team: &str,
    client: &TeamsClient,
    http: &reqwest::Client,
    sink: &Sink,
    report: &Report,
) -> Result<(), Failure> {
    use crate::teams::socket;

    let skype_token = client
        .ensure_fresh_tokens()
        .await?
        .skype_token
        .filter(|t| !t.is_empty())
        .ok_or(Failure::SignedOut)?;
    let renew_on_401 = async |error: Failure| {
        if error == Failure::Http(401)
            && let Err(renewal) = client.force_refresh(&skype_token).await
        {
            return renewal;
        }
        error
    };

    let epid = crate::model::new_client_msg_id();
    let session = match socket::negotiate_trouter(http, &skype_token, &epid).await {
        Ok(session) => session,
        Err(error) => return Err(renew_on_401(error).await),
    };
    let session_id = match socket::obtain_session_id(http, &session, &skype_token, &epid).await {
        Ok(id) => id,
        Err(error) => return Err(renew_on_401(error).await),
    };
    if let Some(registrar) = &session.registrar_url
        && let Err(error) =
            socket::register_endpoint(http, &skype_token, registrar, &session.surl).await
    {
        log::info!("Teams registrar did not take {team}: {error:?}");
    }

    let request = session
        .ws_url(&session_id, &epid)
        .into_client_request()
        .map_err(|error| Failure::Unexpected(error.to_string()))?;
    // Through the proxy, as Slack's sockets go. The error is logged by
    // its kind only: the URL carries the session's signature.
    let (stream, _) = crate::slack::net::websocket(request)
        .await
        .map_err(|error| Failure::Network(error.to_string()))?;

    log::info!("Trouter connected for {team}");
    report(Socket::Connected);
    let (mut write, mut read) = stream.split();
    let mut heartbeat = tokio::time::interval(std::time::Duration::from_secs(30));
    heartbeat.tick().await;

    loop {
        tokio::select! {
            _ = heartbeat.tick() => {
                write
                    .send(WsMessage::Text("2::".into()))
                    .await
                    .map_err(|error| Failure::Network(error.to_string()))?;
            }
            frame = read.next() => match frame {
                Some(Ok(WsMessage::Text(text))) => {
                    if let Some(answer) = handle_frame_control(&text) {
                        let _ = write.send(WsMessage::Text(answer.into())).await;
                    }
                    if let Some(TrouterEvent::Message(message)) = parse_frame(&text) {
                        live_message(team, &message, sink);
                    }
                }
                Some(Ok(WsMessage::Ping(payload))) => {
                    let _ = write.send(WsMessage::Pong(payload)).await;
                }
                Some(Ok(WsMessage::Close(_))) | None => return Ok(()),
                Some(Err(error)) => return Err(Failure::Network(error.to_string())),
                Some(Ok(_)) => {}
            },
        }
    }
}

/// A message Trouter pushed: new, edited, or deleted.
fn live_message(team: &str, message: &crate::teams::types::Message, sink: &Sink) {
    let channel = message.conversation_id.clone().unwrap_or_default();
    let translated = translate_message(message);
    if message.properties.as_ref().is_some_and(|p| p.is_deleted()) {
        sink.send(Event::Deleted {
            team: team.to_owned(),
            channel,
            ts: translated.ts,
        });
    } else {
        sink.send(Event::Message {
            team: team.to_owned(),
            channel,
            changed: message.properties.as_ref().is_some_and(|p| p.is_edited()),
            message: translated,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn post(client_msg_id: Option<&str>) -> Post {
        Post {
            team: "teams_me".into(),
            channel: "19:abc@thread.v2".into(),
            text: "hello".into(),
            local: Ts::new("1.000000"),
            client_msg_id: client_msg_id.map(str::to_owned),
            me: "me".into(),
        }
    }

    #[test]
    fn a_posted_message_is_yours_and_carries_its_ids() {
        let message = posted(
            Some("1700000000123".into()),
            &post(Some("c-1")),
            "<p>hello</p>".into(),
        );
        assert_eq!(message.ts, teams_id_to_ts("1700000000123"));
        assert_eq!(message.user.as_deref(), Some("me"));
        assert_eq!(message.client_msg_id.as_deref(), Some("c-1"));
        assert!(message.text.contains("hello"), "{}", message.text);
    }

    #[test]
    fn a_post_without_an_id_gets_a_time_of_its_own() {
        let message = posted(None, &post(None), "<p>hi</p>".into());
        assert_ne!(message.ts, Ts::new(""));
        assert_eq!(message.user.as_deref(), Some("me"));
    }

    #[test]
    fn marks_and_deletes_use_the_teams_message_id() {
        let ts = teams_id_to_ts("1700000000123");
        assert_eq!(ts_to_teams_id(&ts), "1700000000123");
    }
}
