//! People and apps: fetching who they are, presence, status, and the
//! huddle or call going on.

use std::collections::HashSet;

use super::{Backend, Internal, Worker};
use crate::backend::Event;
use crate::backend::api::worth_retrying;
#[cfg(feature = "teams")]
use crate::failure::Failure;
use crate::slack::{Client, SlackError, types};

impl Worker {
    /// Runs a command about people (see [`crate::people`]).
    pub(super) fn people_command(&mut self, team: String, command: crate::people::Command) {
        // A Teams workspace has presence to watch and calls to make; the
        // rest (status, huddles, typing over RTM) is Slack's.
        #[cfg(feature = "teams")]
        if self.teams_session(&team).is_some() {
            let Some(command) = control(&mut self.teams_call, command) else {
                return;
            };
            let Some(session) = self.teams_session(&team) else {
                return;
            };
            match command {
                crate::people::Command::Watch { .. } => self.people.command(&team, command),
                crate::people::Command::Call { channel, user } => {
                    let (client, sink) = (session.client.clone(), session.sink.clone());
                    // One call or huddle at a time.
                    self.huddle_audio.stop();
                    self.teams_call.start(client, team, channel, user, sink);
                }
                crate::people::Command::AnswerCall { channel, call } => {
                    let sink = session.sink.clone();
                    self.huddle_audio.stop();
                    if !self.teams_call.answer(&team, &call, channel.clone()) {
                        // Too late: the interface lets the call go.
                        log::info!("a Teams call stopped ringing before it was picked up");
                        sink.send(Event::People {
                            team,
                            event: crate::people::Event::Listening {
                                channel,
                                state: crate::huddles::Listen::Ended(Ok(
                                    crate::huddles::Left::Ended,
                                )),
                            },
                        });
                    }
                }
                crate::people::Command::JoinMeeting { meeting } => {
                    let (client, sink) = (session.client.clone(), session.sink.clone());
                    self.huddle_audio.stop();
                    let join = crate::backend::teams_call::Join::Meeting(meeting);
                    self.teams_call.join_meeting(client, team, join, sink);
                }
                crate::people::Command::MeetNow { subject } => {
                    let (client, sink) = (session.client.clone(), session.sink.clone());
                    self.huddle_audio.stop();
                    let join = crate::backend::teams_call::Join::Now(subject);
                    self.teams_call.join_meeting(client, team, join, sink);
                }
                crate::people::Command::Admit { user } => self.teams_call.admit(&user),
                crate::people::Command::DeclineHuddle { room, .. } => {
                    self.teams_call.decline(&team, &room);
                }
                // Slack's alone (status, away, typing, huddles): said so
                // where a view waits on it.
                other => match other.refused(Failure::Unsupported) {
                    Some(event) => session.sink.send(Event::People { team, event }),
                    None => log::debug!("{other:?} is not for a Teams workspace"),
                },
            }
            return;
        }
        let Some((client, sink)) = self.slack(&team) else {
            match command.refused(self.missing(&team)) {
                Some(event) => self.sink.send(Event::People { team, event }),
                None => self.skipped("acting on people", &team),
            }
            return;
        };
        let Some(command) = control(&mut self.huddle_audio, command) else {
            return;
        };
        let command = match command {
            crate::people::Command::ListenHuddle { channel } => {
                #[cfg(feature = "teams")]
                self.teams_call.stop();
                self.huddle_audio.start(client, team, channel, sink);
                return;
            }
            other => other,
        };
        if let Some(command) = crate::backend::people::call(client, team.clone(), command, sink) {
            self.people.command(&team, command);
        }
    }

    /// Asks about the presence of people on screen where nothing tells us
    /// when it changes.
    pub(super) fn poll_presence(&mut self) {
        use crate::backend::people::Poller;
        let workspaces = &self.workspaces;
        self.people.poll(std::time::Instant::now(), |team| {
            match workspaces.get(team)? {
                Backend::Slack(t) => Some(Poller::Slack(t.client.clone(), t.sink.clone())),
                #[cfg(feature = "teams")]
                Backend::Teams(s) => Some(Poller::Teams(s.client.clone(), s.sink.clone())),
            }
        });
    }

    pub(super) fn fetch_users(&mut self, team: String, ids: Vec<String>) {
        #[cfg(feature = "teams")]
        if let Some(session) = self.teams_session(&team) {
            let (client, sink) = (session.client.clone(), session.sink.clone());
            let ids = unasked(&mut self.users_requested, &team, ids);
            if !ids.is_empty() {
                tokio::spawn(crate::backend::teams::fetch_users(client, team, ids, sink));
            }
            return;
        }
        self.fetch_each(team, ids, Info::Users, user_info, |team, users| {
            Event::Users { team, users }
        });
    }

    pub(super) fn fetch_bots(&mut self, team: String, ids: Vec<String>) {
        self.fetch_each(team, ids, Info::Bots, bot_info, |team, bots| Event::Bots {
            team,
            bots,
        });
    }

    /// Fetches the people or apps of `ids` not asked for already, one by
    /// one through `info`, and sends those that came with `found`. Those
    /// that failed for a passing reason may be asked for again.
    pub(super) fn fetch_each<T, Fut>(
        &mut self,
        team: String,
        ids: Vec<String>,
        kind: Info,
        info: impl Fn(Client, String) -> Fut + Send + 'static,
        found: impl FnOnce(String, Vec<T>) -> Event + Send + 'static,
    ) where
        Fut: std::future::Future<Output = Result<T, SlackError>> + Send,
        T: Send + 'static,
    {
        let Some((client, sink)) = self.slack(&team) else {
            self.skipped(kind.fetching(), &team);
            return;
        };
        let requested = match kind {
            Info::Users => &mut self.users_requested,
            Info::Bots => &mut self.bots_requested,
        };
        let ids = unasked(requested, &team, ids);
        if ids.is_empty() {
            return;
        }
        let internal = self.internal.clone();
        tokio::spawn(async move {
            let mut got = Vec::new();
            let mut retry = Vec::new();
            for id in ids {
                match info(client.clone(), id.clone()).await {
                    Ok(item) => got.push(item),
                    Err(error) => {
                        log::debug!("{} {id}: {error}", kind.method());
                        if worth_retrying(&error) {
                            retry.push(id);
                        }
                    }
                }
            }
            if !retry.is_empty() {
                let (users, bots) = match kind {
                    Info::Users => (retry, Vec::new()),
                    Info::Bots => (Vec::new(), retry),
                };
                let _ = internal.send(Internal::FetchFailed {
                    team: team.clone(),
                    users,
                    bots,
                });
            }
            if !got.is_empty() {
                sink.send(found(team, got));
            }
        });
    }
}

/// The controls of the call going on, whichever service it is on: a
/// Slack huddle or a Teams call. Each acts only when there is one.
trait CallControl {
    /// Leaves the huddle, or hangs up.
    fn leave(&mut self);
    /// Mutes or unmutes the microphone.
    fn mute(&mut self, muted: bool);
    /// Receives only what the call window wants.
    #[cfg(feature = "huddle-video")]
    fn watch(&mut self, wish: crate::huddle_audio::cameras::Wish);
    /// Turns the camera on or off.
    #[cfg(feature = "huddle-camera")]
    fn camera(&mut self, on: bool);
    /// Starts, picks or stops sharing your screen.
    #[cfg(feature = "huddle-share")]
    fn share(&mut self, request: crate::huddle_share::ShareRequest);
}

impl CallControl for crate::backend::listen::Listener {
    fn leave(&mut self) {
        self.stop();
    }

    fn mute(&mut self, muted: bool) {
        self.set_muted(muted);
    }

    #[cfg(feature = "huddle-video")]
    fn watch(&mut self, wish: crate::huddle_audio::cameras::Wish) {
        self.watch_call(wish);
    }

    #[cfg(feature = "huddle-camera")]
    fn camera(&mut self, on: bool) {
        self.set_camera(on);
    }

    #[cfg(feature = "huddle-share")]
    fn share(&mut self, request: crate::huddle_share::ShareRequest) {
        crate::backend::listen::Listener::share(self, request);
    }
}

#[cfg(feature = "teams")]
impl CallControl for crate::backend::teams_call::Caller {
    fn leave(&mut self) {
        self.stop();
    }

    fn mute(&mut self, muted: bool) {
        self.set_muted(muted);
    }

    #[cfg(feature = "huddle-video")]
    fn watch(&mut self, wish: crate::huddle_audio::cameras::Wish) {
        crate::backend::teams_call::Caller::watch(self, &wish);
    }

    #[cfg(feature = "huddle-camera")]
    fn camera(&mut self, on: bool) {
        self.set_camera(on);
    }

    #[cfg(feature = "huddle-share")]
    fn share(&mut self, request: crate::huddle_share::ShareRequest) {
        crate::backend::teams_call::Caller::share(self, request);
    }
}

/// Carries out `command` on `call` when it is one of the call's
/// controls; any other command comes back.
fn control(
    call: &mut impl CallControl,
    command: crate::people::Command,
) -> Option<crate::people::Command> {
    use crate::people::Command;
    match command {
        Command::LeaveHuddle => call.leave(),
        Command::MuteHuddle { muted } => call.mute(muted),
        #[cfg(feature = "huddle-video")]
        Command::WatchCall { wish } => call.watch(wish),
        #[cfg(feature = "huddle-camera")]
        Command::CameraHuddle { on } => call.camera(on),
        #[cfg(feature = "huddle-share")]
        Command::ShareHuddle { request } => call.share(request),
        other => return Some(other),
    }
    None
}

/// What [`Worker::fetch_each`] fetches.
#[derive(Clone, Copy)]
pub(super) enum Info {
    Users,
    Bots,
}

impl Info {
    /// The method that answers one.
    pub(super) fn method(self) -> &'static str {
        match self {
            Self::Users => "users.info",
            Self::Bots => "bots.info",
        }
    }

    /// What is skipped, for the log.
    pub(super) fn fetching(self) -> &'static str {
        match self {
            Self::Users => "fetching people",
            Self::Bots => "fetching apps",
        }
    }
}

/// The ids of `ids` not asked for in `team` yet, now marked as asked.
fn unasked(requested: &mut HashSet<(String, String)>, team: &str, ids: Vec<String>) -> Vec<String> {
    ids.into_iter()
        .filter(|id| requested.insert((team.to_owned(), id.clone())))
        .collect()
}

/// One person, by id.
async fn user_info(client: Client, id: String) -> Result<crate::model::User, SlackError> {
    let info: types::UserInfo = client.call("users.info", &[("user", id)]).await?;
    Ok(info.user.into_model())
}

/// One app, by id; Slack can leave the id out of its answer.
async fn bot_info(client: Client, id: String) -> Result<crate::model::Bot, SlackError> {
    let info: types::BotInfo = client.call("bots.info", &[("bot", id.clone())]).await?;
    let mut bot = info.bot.into_model();
    if bot.id.is_empty() {
        bot.id = id;
    }
    Ok(bot)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A call that writes down what it was asked.
    #[derive(Default)]
    struct Recorded(Vec<String>);

    impl CallControl for Recorded {
        fn leave(&mut self) {
            self.0.push("leave".into());
        }
        fn mute(&mut self, muted: bool) {
            self.0.push(format!("mute {muted}"));
        }
        #[cfg(feature = "huddle-video")]
        fn watch(&mut self, _: crate::huddle_audio::cameras::Wish) {
            self.0.push("watch".into());
        }
        #[cfg(feature = "huddle-camera")]
        fn camera(&mut self, on: bool) {
            self.0.push(format!("camera {on}"));
        }
        #[cfg(feature = "huddle-share")]
        fn share(&mut self, _: crate::huddle_share::ShareRequest) {
            self.0.push("share".into());
        }
    }

    #[test]
    fn the_call_controls_go_to_the_call_and_the_rest_comes_back() {
        use crate::people::Command;
        let mut call = Recorded::default();
        assert_eq!(
            control(&mut call, Command::MuteHuddle { muted: true }),
            None
        );
        assert_eq!(control(&mut call, Command::LeaveHuddle), None);
        assert_eq!(
            control(&mut call, Command::SetAway(true)),
            Some(Command::SetAway(true))
        );
        assert_eq!(call.0, ["mute true", "leave"]);
    }

    #[test]
    fn each_id_is_asked_for_once_per_workspace() {
        let mut requested = HashSet::new();
        let ids = |list: &[&str]| list.iter().map(|id| (*id).to_owned()).collect();
        assert_eq!(
            unasked(&mut requested, "TA", ids(&["U1", "U2"])),
            ["U1", "U2"]
        );
        assert_eq!(unasked(&mut requested, "TA", ids(&["U2", "U3"])), ["U3"]);
        assert_eq!(unasked(&mut requested, "TB", ids(&["U1"])), ["U1"]);
    }
}
