//! Conversations and the sidebar: the commands of the conversation
//! dialogs and details, the views at the top of the sidebar, and sidebar
//! edits.

use super::{Otherwise, Worker};
use crate::backend::Event;
use crate::backend::api::Call;
use crate::backend::fetch::{conversation_info, edit_sidebar};
use crate::failure::Doing;

impl Worker {
    /// Runs a command of [`crate::convos`]; a Teams workspace has its own.
    pub(super) fn convos(&self, team: String, command: crate::convos::Command) {
        #[cfg(feature = "teams")]
        if self.teams_session(&team).is_some() {
            self.teams_convos(team, command);
            return;
        }
        let what = command.doing();
        self.spawn_slack(
            team,
            Otherwise::Answer(Box::new(move |team, error| Event::Convos {
                team,
                event: crate::convos::Event::Failed { what, error },
            })),
            |client, team, sink| crate::backend::convos::run(client, team, command, sink),
        );
    }

    /// Runs a command of [`crate::views`].
    pub(super) fn views(&self, team: String, command: crate::views::Command) {
        let failed = command.clone();
        self.spawn_slack(
            team,
            Otherwise::Answer(Box::new(move |team, error| Event::Views {
                team,
                event: failed.failed(error),
            })),
            |client, team, sink| crate::backend::views::run(client, team, command, sink),
        );
    }

    /// Closes a direct message; closed already is as asked.
    pub(super) fn close_conversation(&self, team: String, channel: String) {
        self.act(
            team,
            Doing::CloseConversation,
            Call::new(
                "conversations.close",
                vec![("channel", channel)],
                &["channel_not_found", "already_closed"],
            ),
        );
    }

    /// Fetches one conversation's details again, as the interface asks
    /// on its own.
    pub(super) fn fetch_conversation(&self, team: String, channel: String) {
        self.spawn_slack(
            team,
            Otherwise::Skip("fetching a conversation"),
            |client, team, sink| conversation_info(client, team, channel, sink),
        );
    }

    /// Carries out a sidebar edit.
    pub(super) fn edit_sidebar(&self, team: String, calls: Vec<crate::sidebar::SidebarCall>) {
        self.spawn_slack(
            team,
            Otherwise::Refuse(Doing::ChangeSidebar),
            |client, team, sink| edit_sidebar(client, team, calls, sink),
        );
    }
}
