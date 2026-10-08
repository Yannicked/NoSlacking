//! Microsoft Teams workspaces: starting them, their Trouter connection,
//! and the conversation commands Teams can carry out.

use std::sync::Arc;

use super::{Backend, Internal, Worker};
use crate::backend::{Event, Gate, Sink};
use crate::failure::Failure;
use crate::model::Workspace;

impl Worker {
    /// Starts using a signed-in Microsoft Teams workspace.
    #[cfg(feature = "teams")]
    pub(super) fn add_teams(
        &mut self,
        workspace: Workspace,
        creds: crate::teams::auth::TeamsCredentials,
    ) {
        let (sink, gate) = self.sink.gated();
        let client = crate::backend::teams::client(
            creds,
            &workspace.team_id,
            self.credentials.clone(),
            sink.clone(),
        );
        self.images
            .set_teams_client(&workspace.team_id, client.clone());
        self.sink.send(Event::WorkspaceReady(workspace.clone()));
        self.start_teams(workspace, client, (sink, gate));
    }

    /// Starts a Teams workspace's lists and live connection, replacing
    /// whatever ran for it before.
    #[cfg(feature = "teams")]
    pub(super) fn start_teams(
        &mut self,
        workspace: Workspace,
        client: crate::teams::client::TeamsClient,
        gated: (Sink, Gate),
    ) {
        let generation = self.generation();
        let report = self.trouter_report(&workspace.team_id, generation);
        // Incoming calls ring through the worker, which has the one call.
        let internal = self.internal.clone();
        let team = workspace.team_id.clone();
        client.calls().set_ringer(Arc::new(move |call| {
            let _ = internal.send(Internal::IncomingCall {
                team: team.clone(),
                call,
            });
        }));
        let session = crate::backend::teams::Session::start(
            workspace.clone(),
            client,
            gated,
            generation,
            report,
        );
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
    pub(super) fn trouter_report(
        &self,
        team: &str,
        generation: u64,
    ) -> crate::backend::teams::Report {
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

    /// Runs a command about conversations in a Teams workspace: finding
    /// people and starting chats; the rest is Slack's.
    #[cfg(feature = "teams")]
    pub(super) fn teams_convos(&self, team: String, command: crate::convos::Command) {
        let Some(session) = self.teams_session(&team) else {
            // Only called for a Teams workspace; answered all the same.
            self.sink.send(Event::Convos {
                event: crate::convos::Event::Failed {
                    what: command.doing(),
                    error: self.missing(&team),
                },
                team,
            });
            return;
        };
        let (client, sink) = (session.client.clone(), session.sink.clone());
        match command {
            crate::convos::Command::FindPeople { query } => {
                tokio::spawn(crate::backend::teams::find_people(
                    client, team, query, sink,
                ));
            }
            crate::convos::Command::Open { users } => {
                let me = session.workspace.user_id.clone();
                tokio::spawn(crate::backend::teams::open(client, team, me, users, sink));
            }
            other => sink.send(Event::Convos {
                team,
                event: crate::convos::Event::Failed {
                    what: other.doing(),
                    error: Failure::Unsupported,
                },
            }),
        }
    }

    /// Starts every Teams workspace again: lists it afresh and reconnects
    /// Trouter, through the proxy as now set.
    #[cfg(feature = "teams")]
    pub(super) fn restart_teams(&mut self) {
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
}
