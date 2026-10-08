//! Staying up to date: the real-time sockets (Socket Mode, and RTM for
//! browser sessions), the events they bring, and polling while they are
//! down.

use std::collections::HashSet;

use tokio::sync::{mpsc, watch};

use super::{Focus, Internal, Live, Worker};
use crate::backend::api::failure;
use crate::backend::fetch::{conversation_info, conversations, history, sections};
use crate::backend::translate::{Translated, translate};
use crate::backend::{Event, Socket};
use crate::failure::Failure;
use crate::offline::Cache;
use crate::slack::Client;
use crate::slack::socket::{self, SocketEvent};

impl Worker {
    /// Opens (or reopens) the RTM socket for a session workspace.
    pub(super) fn start_rtm(&mut self, team: &str, client: Client) {
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

    /// Closes a workspace's RTM socket, if it has one.
    pub(super) fn stop_rtm(&mut self, team: &str) {
        if let Some(old) = self.rtm.remove(team) {
            let _ = old.stop.send(true);
        }
    }

    pub(super) fn generation(&mut self) -> u64 {
        self.next_generation += 1;
        self.next_generation
    }

    pub(super) fn restart_socket(&mut self) {
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

    pub(super) fn is_session(&self, team: &str) -> bool {
        self.slack_team(team).is_some_and(|t| t.client.is_session())
    }

    /// The real-time status of one workspace: its own RTM socket for a
    /// browser session, Trouter for Teams, the shared Socket Mode
    /// connection otherwise.
    pub(super) fn status(&self, team: &str) -> Socket {
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
    pub(super) fn is_live(&self, team: &str) -> bool {
        self.status(team) == Socket::Connected
    }

    /// Tells the interface the status of the workspace on screen, when it
    /// changed. The interface shows one status, and the one that matters
    /// is the one for what you are looking at.
    pub(super) fn report_socket(&mut self) {
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

    pub(super) fn socket_event(&mut self, event: SocketEvent) {
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

    pub(super) fn rtm_event(&mut self, team: &str, event: crate::slack::rtm::RtmEvent) {
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
    pub(super) fn dispatch_event(&mut self, team: &str, event: &serde_json::Value) {
        let Some(me) = self.slack_team(team).map(|t| t.workspace.user_id.clone()) else {
            log::debug!("event for a workspace not signed in here");
            return;
        };
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
                        tokio::spawn(async move {
                            crate::backend::fetch::emoji(&client, &team, &sink).await
                        });
                    }
                }
                Translated::RefreshPrefs => {
                    if let Some((client, sink)) = self.slack(team)
                        && client.is_session()
                    {
                        tokio::spawn(crate::backend::desktop::prefs(
                            client,
                            team.to_owned(),
                            sink,
                        ));
                    }
                }
            }
        }
    }

    /// Opens every socket afresh and lists every workspace's conversations
    /// again, to catch up on anything missed while offline.
    pub(super) fn reconnect(&mut self) {
        self.restart_socket();
        // These list their conversations as they start.
        let restarted = self.retry_boots(std::time::Instant::now(), true);
        let session_teams: Vec<(String, Client)> = self
            .slack_teams()
            .filter(|(_, team)| team.client.is_session())
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

    /// What runs on each poll tick: the open conversation, then the watch
    /// over every workspace's other conversations.
    pub(super) fn poll(&mut self) {
        self.retry_boots(std::time::Instant::now(), false);
        self.poll_open();
        self.watch_all(std::time::Instant::now());
    }

    /// Starts a round of the watch over every conversation (see
    /// [`crate::backend::poll`]) for each workspace whose socket is down and whose
    /// rest is over, and stops the round of each whose socket is back:
    /// live events tell everything from then on.
    pub(super) fn watch_all(&mut self, now: std::time::Instant) {
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
                crate::backend::poll::round(client, team_id, open, &mut state, sink).await;
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
    pub(super) fn poll_open(&mut self) {
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
