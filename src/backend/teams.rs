//! Microsoft Teams workspace session and background tasks.

use futures_util::{SinkExt, StreamExt};
use tokio_tungstenite::tungstenite::Message as WsMessage;

use crate::backend::{Event, Gate, Sink};
use crate::model::Workspace;
use crate::teams::client::TeamsClient;
use crate::teams::socket::{TrouterEvent, handle_frame_control, parse_frame};

/// A live Microsoft Teams workspace session.
pub struct TeamsSession {
    pub client: TeamsClient,
    pub workspace: Workspace,
    pub sink: Sink,
    pub gate: Gate,
    pub creds: crate::teams::auth::TeamsCredentials,
    pub boot_task: tokio::task::AbortHandle,
    pub trouter_task: Option<tokio::task::AbortHandle>,
}

impl TeamsSession {
    /// Shuts down background tasks and closes the event gate.
    pub fn shut(&self) {
        self.gate.close();
        self.boot_task.abort();
        if let Some(ref task) = self.trouter_task {
            task.abort();
        }
    }
}

/// Runs the real-time Trouter WebSocket connection loop for a Teams workspace.
///
/// Handles session negotiation, registrar registration, auto-responding to ping/acks,
/// and dispatching incoming live messages to the UI sink.
pub async fn trouter_loop(team_id: String, client: TeamsClient, sink: Sink) {
    let http = crate::slack::net::api();

    loop {
        let skype_token = match client.credentials().skype_token {
            Some(ref t) if !t.is_empty() => t.clone(),
            _ => {
                log::info!("No SkypeToken for Trouter in {team_id}, attempting refresh...");
                match client.force_refresh().await {
                    Ok(c) => c.skype_token.unwrap_or_default(),
                    Err(e) => {
                        log::warn!("Failed to refresh SkypeToken for {team_id}: {e:?}");
                        tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                        continue;
                    }
                }
            }
        };

        if skype_token.is_empty() {
            tokio::time::sleep(std::time::Duration::from_secs(5)).await;
            continue;
        }

        let epid = crate::model::new_client_msg_id();

        // 1. Negotiate Trouter session
        let session = match crate::teams::socket::negotiate_trouter(&http, &skype_token, &epid)
            .await
        {
            Ok(s) => s,
            Err(crate::failure::Failure::Http(401)) => {
                log::info!("Trouter negotiation returned 401 for {team_id}, refreshing tokens...");
                if let Err(e) = client.force_refresh().await {
                    log::warn!("Trouter token refresh failed for {team_id}: {e:?}");
                    tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                }
                continue;
            }
            Err(e) => {
                log::warn!("Trouter negotiation failed for {team_id}: {e:?}, retrying in 5s");
                tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                continue;
            }
        };

        // 2. Obtain Socket.IO session ID
        let session_id = match crate::teams::socket::obtain_session_id(
            &http,
            &session,
            &skype_token,
            &epid,
        )
        .await
        {
            Ok(sid) => sid,
            Err(crate::failure::Failure::Http(401)) => {
                log::info!(
                    "Trouter session ID request returned 401 for {team_id}, refreshing tokens..."
                );
                let _ = client.force_refresh().await;
                continue;
            }
            Err(e) => {
                log::warn!(
                    "Trouter session ID request failed for {team_id}: {e:?}, retrying in 5s"
                );
                tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                continue;
            }
        };

        // 3. Register endpoint with registrar if URL provided
        if let Some(ref reg_url) = session.registrar_url {
            let _ = crate::teams::socket::register_endpoint(
                &http,
                &skype_token,
                reg_url,
                &session.surl,
            )
            .await;
        }

        // 4. Connect WebSocket
        let ws_url = session.ws_url(&session_id, &epid);
        let ws_stream = match tokio_tungstenite::connect_async(&ws_url).await {
            Ok((stream, _resp)) => stream,
            Err(e) => {
                log::warn!("Trouter WebSocket connect failed for {team_id}: {e:?}, retrying in 5s");
                tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                continue;
            }
        };

        log::info!("Trouter connected for workspace {team_id}");
        let (mut write, mut read) = ws_stream.split();

        let mut heartbeat = tokio::time::interval(std::time::Duration::from_secs(30));
        heartbeat.tick().await; // skip initial tick

        loop {
            tokio::select! {
                _ = heartbeat.tick() => {
                    if let Err(e) = write.send(WsMessage::Text("2::".into())).await {
                        log::warn!("Trouter heartbeat send failed: {e:?}");
                        break;
                    }
                }
                msg = read.next() => {
                    match msg {
                        Some(Ok(WsMessage::Text(text))) => {
                            if let Some(resp) = handle_frame_control(&text) {
                                let _ = write.send(WsMessage::Text(resp.into())).await;
                            }
                            if let Some(event) = parse_frame(&text) {
                                match event {
                                    TrouterEvent::Message(teams_msg) => {
                                        let conv_id = teams_msg.conversation_id.clone().unwrap_or_default();
                                        let msg = crate::backend::teams_translate::translate_message(&teams_msg);
                                        let is_deleted = teams_msg
                                            .properties
                                            .as_ref()
                                            .is_some_and(|p| p.is_deleted());

                                        if is_deleted {
                                            sink.send(Event::Deleted {
                                                team: team_id.clone(),
                                                channel: conv_id,
                                                ts: msg.ts,
                                            });
                                        } else {
                                            sink.send(Event::Message {
                                                team: team_id.clone(),
                                                channel: conv_id,
                                                message: msg,
                                                changed: false,
                                            });
                                        }
                                    }
                                    TrouterEvent::Ping => {
                                        let _ = write.send(WsMessage::Text("2::".into())).await;
                                    }
                                    TrouterEvent::Raw(_) => {}
                                }
                            }
                        }
                        Some(Ok(WsMessage::Ping(p))) => {
                            let _ = write.send(WsMessage::Pong(p)).await;
                        }
                        Some(Ok(WsMessage::Close(_))) | None => {
                            log::info!("Trouter WebSocket closed for {team_id}, reconnecting...");
                            break;
                        }
                        Some(Err(e)) => {
                            log::warn!("Trouter WebSocket error for {team_id}: {e:?}");
                            break;
                        }
                        _ => {}
                    }
                }
            }
        }

        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
    }
}

/// Fetches initial conversations, teams/channels, and current user info for a Teams workspace.
pub async fn boot_teams(client: TeamsClient, team_id: String, _user_id: String, sink: Sink) {
    // 0. Ensure tokens are fresh before initial requests
    if let Err(err) = client.ensure_fresh_tokens().await {
        log::warn!("could not ensure fresh tokens on boot for {team_id}: {err:?}");
    }

    // 1. Fetch conversations / 1:1 and group chats
    match client.get_conversations(50).await {
        Ok(chats) => {
            let mut convos = Vec::new();
            for c in &chats {
                convos.push(crate::backend::teams_translate::translate_conversation(c));
            }
            sink.send(Event::Conversations {
                team: team_id.clone(),
                list: convos,
                complete: true,
            });
        }
        Err(err) => {
            log::warn!("failed to get teams conversations: {err:?}");
        }
    }

    // 2. Fetch teams and channels (CSA)
    match client.get_teams().await {
        Ok(teams_list) => {
            let mut sections = Vec::new();
            let mut team_channels = Vec::new();
            for t in &teams_list {
                let (section, channels) = crate::backend::teams_translate::translate_team(t);
                sections.push(section);
                team_channels.extend(channels);
            }
            if !team_channels.is_empty() {
                sink.send(Event::Conversations {
                    team: team_id.clone(),
                    list: team_channels,
                    complete: false,
                });
            }
            if !sections.is_empty() {
                sink.send(Event::Sections {
                    team: team_id.clone(),
                    sections,
                });
            }
        }
        Err(err) => {
            log::warn!("failed to get teams list: {err:?}");
        }
    }

    // 3. Fetch user profile
    match client.get_me().await {
        Ok(me) => {
            sink.send(Event::Users {
                team: team_id.clone(),
                users: vec![crate::backend::teams_translate::translate_user(&me)],
            });
        }
        Err(err) => {
            log::warn!("failed to get me: {err:?}");
        }
    }
}
