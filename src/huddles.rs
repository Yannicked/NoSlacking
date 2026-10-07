//! Huddle invitations and keeping a huddle's participants right.
//!
//! A browser session's real-time socket says when someone rings you
//! (`huddle_invite`) and, now and then, who joins and leaves a huddle
//! (`sh_room_*`). NoSlacking cannot carry the call itself, so an
//! invitation offers to join in Slack (its web page or app) or to
//! decline, and goes away once answered, once the huddle ends, or once
//! Slack would have stopped ringing.
//!
//! Join and leave events go missing (a reconnect, a busy socket), so the
//! participants of a huddle on screen are asked of Slack
//! (`screenhero.rooms.info`) after a reconnect and every few minutes.
//! Socket Mode, for your own Slack app, carries none of these events and
//! those calls need a browser session, so all of this is for browser
//! sign-ins only.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use crate::app::App;
use crate::backend;
use crate::i18n::{t, tf};
use crate::people::{self, Huddle};

/// How long an invitation shows. Slack rings for about 30 seconds and the
/// invitation says nothing of it, so a little longer, for a late look.
pub const RING_FOR: Duration = Duration::from_secs(45);
/// How often the participants of a huddle on screen are asked of Slack.
pub const CHECK_EVERY: Duration = Duration::from_secs(3 * 60);
/// The longest wait between two checks after failures.
const CHECK_AT_MOST: Duration = Duration::from_secs(30 * 60);

/// A change to a huddle known only by its room, as some `sh_room_*`
/// events say it: without the conversations it is in.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RoomChange {
    Joined(String),
    Left(String),
    /// Who is in it now, all of them.
    Participants(Vec<String>),
    Ended,
}

/// Someone ringing you into a huddle.
#[derive(Clone, Debug, PartialEq)]
pub struct Invite {
    pub team: String,
    /// The conversation the huddle is in.
    pub channel: String,
    /// Slack's id for the huddle (`R…`).
    pub room: String,
    /// Who rang.
    pub from: String,
    /// When it stops showing.
    pub until: Instant,
}

/// The invitations showing, oldest first.
#[derive(Clone, Debug, Default)]
pub struct Invites {
    list: Vec<Invite>,
}

impl Invites {
    /// Takes in an invitation received at `now`. Returns whether it is new:
    /// Slack may ring twice for one huddle, which shows once.
    pub fn add(&mut self, team: &str, channel: &str, room: &str, from: &str, now: Instant) -> bool {
        let until = now + RING_FOR;
        if let Some(known) = self
            .list
            .iter_mut()
            .find(|i| i.team == team && i.room == room)
        {
            known.until = until;
            return false;
        }
        self.list.push(Invite {
            team: team.to_owned(),
            channel: channel.to_owned(),
            room: room.to_owned(),
            from: from.to_owned(),
            until,
        });
        true
    }

    /// Takes away the invitation to a room, answered here or elsewhere.
    /// Returns it, if it was showing.
    pub fn answered(&mut self, team: &str, room: &str) -> Option<Invite> {
        let at = self
            .list
            .iter()
            .position(|i| i.team == team && i.room == room)?;
        Some(self.list.remove(at))
    }

    /// A huddle in `channel` changed or ended (`None`): its invitation
    /// goes once it is over, or once `me` is in it.
    pub fn huddle_changed(&mut self, team: &str, channel: &str, huddle: Option<&Huddle>, me: &str) {
        self.list.retain(|i| {
            if i.team != team || i.channel != channel {
                return true;
            }
            match huddle {
                None => false,
                // Another huddle there now: the one you were rung for is over.
                Some(h) if h.room != i.room => false,
                Some(h) => !h.participants.iter().any(|p| p == me),
            }
        });
    }

    /// A huddle known only by its room changed.
    pub fn room_changed(&mut self, team: &str, room: &str, change: &RoomChange, me: &str) {
        let gone = match change {
            RoomChange::Ended => true,
            RoomChange::Joined(user) => user == me,
            RoomChange::Participants(who) => who.iter().any(|p| p == me),
            RoomChange::Left(_) => false,
        };
        if gone {
            self.answered(team, room);
        }
    }

    /// Takes away what stopped ringing: the invitation to `room`, or, when
    /// only the conversation is known, those in `channel`. Returns them.
    pub fn cancelled(
        &mut self,
        team: &str,
        channel: Option<&str>,
        room: Option<&str>,
    ) -> Vec<Invite> {
        let (gone, kept) = std::mem::take(&mut self.list).into_iter().partition(|i| {
            i.team == team
                && match (room, channel) {
                    (Some(room), _) => i.room == room,
                    (None, Some(channel)) => i.channel == channel,
                    (None, None) => false,
                }
        });
        self.list = kept;
        gone
    }

    /// Drops what has rung long enough at `now`, and says when the next
    /// one does.
    pub fn expire(&mut self, now: Instant) -> Option<Instant> {
        self.list.retain(|i| i.until > now);
        self.list.iter().map(|i| i.until).min()
    }

    /// Forgets a workspace's invitations, when it signs out.
    pub fn forget(&mut self, team: &str) {
        self.list.retain(|i| i.team != team);
    }

    /// The invitations showing, oldest first.
    pub fn list(&self) -> &[Invite] {
        &self.list
    }
}

/// When a huddle's participants were last asked of Slack, and how many
/// times in a row that failed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Check {
    pub at: Instant,
    pub failures: u32,
}

/// How long to wait after a check that came after `failures` failures in
/// a row: [`CHECK_EVERY`], doubling with each failure up to
/// `CHECK_AT_MOST`, so a refusing Slack is never asked in a loop.
pub fn check_wait(failures: u32) -> Duration {
    CHECK_EVERY
        .saturating_mul(1 << failures.min(8))
        .min(CHECK_AT_MOST)
}

/// When a huddle last checked as `last` is next due: at once if never.
pub fn check_due(last: Option<&Check>, now: Instant) -> Instant {
    last.map_or(now, |last| last.at + check_wait(last.failures))
}

mod listen;
pub use listen::{
    ALONE_FOR, FAILED_FOR, Left, Listen, Listening, Person, Phase, Place, Roster, alone_since,
    clock, faces, leave_alone, quit, status_text, title_text,
};
#[cfg(feature = "huddle-video")]
pub use listen::{
    Camera, Gallery, MAX_TILES, Picture, Screen, Share, Wish, cameras_text, open_call,
    sharing_text, tell_wish, watch,
};

/// The call window's picture: the watched share's newest, uploaded.
#[cfg(feature = "huddle-video")]
#[derive(Default)]
pub struct CallPicture {
    /// The texture it is drawn from, set again for each new picture.
    pub texture: Option<egui::TextureHandle>,
    /// The share's own size, for its shape.
    pub source: [usize; 2],
    /// Which share it is of.
    pub of: Option<String>,
    /// Each camera tile's newest picture, uploaded, and the camera's own
    /// size, by camera.
    pub tiles: std::collections::BTreeMap<String, (egui::TextureHandle, [usize; 2])>,
    /// The demo draws the window inside the main one, to be in its
    /// screenshot.
    #[cfg(feature = "demo")]
    pub embed: bool,
}

#[cfg(feature = "huddle-video")]
impl std::fmt::Debug for CallPicture {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CallPicture")
            .field("source", &self.source)
            .field("of", &self.of)
            .field("tiles", &self.tiles.keys().collect::<Vec<_>>())
            .finish_non_exhaustive()
    }
}

/// The self-preview's picture, uploaded (the `huddle-camera` feature).
#[cfg(feature = "huddle-camera")]
#[derive(Default)]
pub struct PreviewPicture {
    /// The texture it is drawn from, set again for each new picture.
    pub texture: Option<egui::TextureHandle>,
    /// Its size in pixels.
    pub size: [usize; 2],
}

#[cfg(feature = "huddle-camera")]
impl std::fmt::Debug for PreviewPicture {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PreviewPicture")
            .field("size", &self.size)
            .finish_non_exhaustive()
    }
}

/// The app's side of huddles.
#[derive(Debug, Default)]
pub struct State {
    pub invites: Invites,
    /// The checks made, by workspace and room.
    checks: HashMap<(String, String), Check>,
    /// The huddle being listened to, if any.
    pub listening: Option<Listening>,
    /// The call window's picture of the share watched.
    #[cfg(feature = "huddle-video")]
    pub picture: CallPicture,
    /// Your camera's self-preview.
    #[cfg(feature = "huddle-camera")]
    pub preview: PreviewPicture,
}

impl State {
    /// Notes a check of `room` sent at `now`. It counts from when it was
    /// sent, so a call that never answers is not repeated sooner.
    fn checking(&mut self, team: &str, room: &str, now: Instant) {
        let failures = self
            .checks
            .get(&(team.to_owned(), room.to_owned()))
            .map_or(0, |c| c.failures);
        self.checks.insert(
            (team.to_owned(), room.to_owned()),
            Check { at: now, failures },
        );
    }

    /// Notes how a check of `room` came out.
    fn checked(&mut self, team: &str, room: &str, ok: bool) {
        if let Some(check) = self.checks.get_mut(&(team.to_owned(), room.to_owned())) {
            check.failures = if ok { 0 } else { check.failures + 1 };
        }
    }
}

/// What the views ask for about huddles.
#[derive(Clone, Debug, PartialEq)]
pub enum Action {
    /// Joins the huddle you were rung for, in Slack's web page or app.
    Join { team: String, room: String },
    /// Declines it.
    Decline { team: String, room: String },
    /// Listens to the huddle in `channel` here, muted, leaving any other.
    Listen { team: String, channel: String },
    /// Answers an invitation by listening here: opens the huddle's
    /// conversation, where its Leave button is, and listens.
    ListenInvite { team: String, room: String },
    /// Leaves the huddle being listened to.
    Leave,
    /// Mutes or unmutes the microphone in the huddle being listened to.
    Microphone(crate::huddle_mic::MicAction),
    /// Opens the call window on a share (by key), or closes it (none).
    #[cfg(feature = "huddle-video")]
    Watch(Option<String>),
    /// Opens the call window on the cameras (and a share, if any).
    #[cfg(feature = "huddle-video")]
    OpenCall,
    /// Turns the camera on or off in the huddle being listened to.
    #[cfg(feature = "huddle-camera")]
    Camera(crate::huddle_camera::CamAction),
}

/// Applies a view's request.
pub fn apply(app: &mut App, action: Action) {
    match action {
        Action::Join { team, room } => {
            // A browser sign-in joins here; only the others hand the
            // huddle to Slack.
            if is_session(app, &team) {
                apply(app, Action::ListenInvite { team, room });
                return;
            }
            if let Some(invite) = app.huddles.invites.answered(&team, &room) {
                app.open_url(&people::huddle_url(&team, &invite.channel));
            }
        }
        Action::Decline { team, room } => {
            if let Some(invite) = app.huddles.invites.answered(&team, &room) {
                app.backend.send(backend::Command::People {
                    team,
                    command: people::Command::DeclineHuddle {
                        channel: invite.channel,
                        room,
                    },
                });
            }
        }
        Action::ListenInvite { team, room } => {
            if let Some(invite) = app.huddles.invites.answered(&team, &room) {
                app.actions
                    .push(crate::model::Action::SelectWorkspace(team.clone()));
                app.actions.push(crate::model::Action::OpenConversation(
                    invite.channel.clone(),
                ));
                apply(
                    app,
                    Action::Listen {
                        team,
                        channel: invite.channel,
                    },
                );
            }
        }
        Action::Listen { team, channel } => listen::listen(app, team, channel),
        Action::Microphone(action) => crate::huddle_mic::apply(app, action),
        Action::Leave => listen::leave(app),
        #[cfg(feature = "huddle-video")]
        Action::Watch(share) => listen::watch(app, share),
        #[cfg(feature = "huddle-video")]
        Action::OpenCall => listen::open_call(app),
        #[cfg(feature = "huddle-camera")]
        Action::Camera(action) => crate::huddle_camera::apply(app, action),
    }
}

/// Whether `team` is a browser session, which alone can check huddles.
fn is_session(app: &App, team: &str) -> bool {
    app.workspaces.iter().any(|w| {
        w.info.team_id == team
            && w.signed_out.is_none()
            && w.info.sign_in == crate::model::SignInKind::Session
    })
}

/// Asks Slack who is in `huddle`, shown in `channel`, at `now`.
fn check(app: &mut App, team: &str, channel: &str, room: &str, now: Instant) {
    app.huddles.checking(team, room, now);
    app.backend.send(backend::Command::People {
        team: team.to_owned(),
        command: people::Command::CheckHuddle {
            channel: channel.to_owned(),
            room: room.to_owned(),
        },
    });
}

/// Runs every frame: drops invitations that rang long enough, and checks
/// the huddle in the open conversation when due.
pub fn frame(app: &mut App, now: Instant) {
    listen::frame(app, now);
    if let Some(next) = app.huddles.invites.expire(now) {
        app.waker.wake_after(next.saturating_duration_since(now));
    }
    let open = app
        .active_conversation()
        .and_then(|(workspace, conversation)| {
            let huddle = workspace.people.huddles.get(&conversation.id)?;
            Some((
                workspace.info.team_id.clone(),
                conversation.id.clone(),
                huddle.room.clone(),
            ))
        });
    let Some((team, channel, room)) = open else {
        return;
    };
    if !is_session(app, &team) {
        return;
    }
    let due = check_due(app.huddles.checks.get(&(team.clone(), room.clone())), now);
    if due <= now {
        check(app, &team, &channel, &room, now);
        app.waker.wake_after(CHECK_EVERY);
    } else {
        app.waker.wake_after(due - now);
    }
}

/// Takes in one of the worker's answers about huddles for `team`.
/// Returns the event back when it is not about huddles.
pub fn handle(app: &mut App, team: &str, event: people::Event) -> Option<people::Event> {
    let now = Instant::now();
    let me = app
        .workspaces
        .iter()
        .find(|w| w.info.team_id == team)
        .map(|w| w.info.user_id.clone())?;
    match event {
        people::Event::Huddles { changes } => {
            for (channel, huddle) in &changes {
                app.huddles
                    .invites
                    .huddle_changed(team, channel, huddle.as_ref(), &me);
            }
            Some(people::Event::Huddles { changes })
        }
        people::Event::HuddleInvite {
            channel,
            room,
            from,
        } => {
            invited(app, team, &channel, &room, &from, now);
            None
        }
        people::Event::HuddleInviteCancelled { channel, room } => {
            let gone = app
                .huddles
                .invites
                .cancelled(team, channel.as_deref(), room.as_deref());
            for invite in gone {
                app.withdraw_invite_note(team, &invite.channel);
            }
            None
        }
        people::Event::HuddleRoom { room, change } => {
            app.huddles.invites.room_changed(team, &room, &change, &me);
            if let Some(workspace) = app.workspaces.iter_mut().find(|w| w.info.team_id == team) {
                apply_room(&mut workspace.people.huddles, &room, &change);
            }
            None
        }
        people::Event::HuddleChecked {
            channel,
            room,
            result,
        } => {
            app.huddles.checked(team, &room, result.is_ok());
            match result {
                Ok(huddle) => Some(people::Event::Huddles {
                    changes: vec![(channel, huddle)],
                }),
                Err(error) => {
                    log::debug!("could not check huddle {room}: {error:?}");
                    None
                }
            }
        }
        people::Event::Reconnected => {
            // Joins and leaves while the socket was down are lost: ask about
            // every huddle known, once.
            if is_session(app, team) {
                let known: Vec<(String, String)> = app
                    .workspaces
                    .iter()
                    .find(|w| w.info.team_id == team)
                    .map(|w| {
                        w.people
                            .huddles
                            .iter()
                            .map(|(channel, h)| (channel.clone(), h.room.clone()))
                            .collect()
                    })
                    .unwrap_or_default();
                for (channel, room) in known {
                    check(app, team, &channel, &room, now);
                }
            }
            None
        }
        people::Event::Listening { channel, state } => {
            listen::heard(app, team, &channel, state);
            None
        }
        people::Event::Microphone { channel, news } => {
            crate::huddle_mic::news(app, team, &channel, news);
            None
        }
        #[cfg(feature = "huddle-camera")]
        people::Event::Camera { channel, news } => {
            crate::huddle_camera::news(app, team, &channel, news);
            None
        }
        people::Event::InviteDeclined { result } => {
            if let Err(error) = result {
                app.toast(
                    tf(
                        "Could not decline the huddle: {error}",
                        &[("error", &error.message())],
                    ),
                    true,
                );
            }
            None
        }
        other => Some(other),
    }
}

/// Applies a change known only by its room to the huddles of a workspace.
pub fn apply_room(huddles: &mut HashMap<String, Huddle>, room: &str, change: &RoomChange) {
    let channels: Vec<String> = huddles
        .iter()
        .filter(|(_, h)| h.room == room)
        .map(|(c, _)| c.clone())
        .collect();
    for channel in channels {
        let Some(huddle) = huddles.get_mut(&channel) else {
            continue;
        };
        match change {
            RoomChange::Joined(user) => {
                if !huddle.participants.contains(user) {
                    huddle.participants.push(user.clone());
                }
            }
            RoomChange::Left(user) => huddle.participants.retain(|p| p != user),
            RoomChange::Participants(who) => huddle.participants.clone_from(who),
            RoomChange::Ended => huddle.participants.clear(),
        }
        if huddle.participants.is_empty() {
            huddles.remove(&channel);
        }
    }
}

/// Shows a new invitation, in the window and on the desktop.
fn invited(app: &mut App, team: &str, channel: &str, room: &str, from: &str, now: Instant) {
    let Some(workspace) = app.workspaces.iter().find(|w| w.info.team_id == team) else {
        return;
    };
    let me = &workspace.info.user_id;
    // Your own ring, or a huddle you are in already.
    if from == me
        || workspace
            .people
            .huddles
            .get(channel)
            .is_some_and(|h| h.room == room && h.participants.contains(me))
    {
        return;
    }
    if !app.huddles.invites.add(team, channel, room, from, now) {
        return;
    }
    app.waker.wake_after(RING_FOR);
    if let Some(note) = app.invite_note(team, channel, from) {
        app.notify(note);
    }
}

/// What an invitation says: who rings, and where.
pub fn invite_text(name: &str, place: Option<&str>) -> (String, String) {
    let title = tf("{name} invites you to a huddle", &[("name", name)]);
    let body = match place {
        Some(place) => tf("In {place}", &[("place", place)]),
        None => t("In a direct message").into_owned(),
    };
    (title, body)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn huddle(room: &str, participants: &[&str]) -> Huddle {
        Huddle {
            room: room.into(),
            participants: participants.iter().map(|p| (*p).to_owned()).collect(),
        }
    }

    #[test]
    fn an_invitation_goes_when_answered() {
        let now = Instant::now();
        let mut invites = Invites::default();
        assert!(invites.add("T1", "C1", "R1", "U2", now));
        // Rung again: still one.
        assert!(!invites.add("T1", "C1", "R1", "U2", now));
        assert_eq!(invites.list().len(), 1);
        assert_eq!(invites.answered("T1", "R9"), None);
        assert_eq!(
            invites.answered("T1", "R1").map(|i| i.channel),
            Some("C1".to_owned())
        );
        assert!(invites.list().is_empty());
    }

    #[test]
    fn an_invitation_goes_when_the_huddle_ends_or_you_join() {
        let now = Instant::now();
        let mut invites = Invites::default();
        invites.add("T1", "C1", "R1", "U2", now);
        invites.add("T1", "C2", "R2", "U2", now);
        invites.add("T1", "C3", "R3", "U2", now);
        invites.add("T2", "C1", "R4", "U2", now);
        // Someone else joins: still ringing.
        invites.huddle_changed("T1", "C1", Some(&huddle("R1", &["U2", "U3"])), "U0");
        assert_eq!(invites.list().len(), 4);
        // You joined, from another device.
        invites.huddle_changed("T1", "C1", Some(&huddle("R1", &["U2", "U0"])), "U0");
        // It ended.
        invites.huddle_changed("T1", "C2", None, "U0");
        // Known by its room only.
        invites.room_changed("T1", "R3", &RoomChange::Left("U2".into()), "U0");
        assert_eq!(invites.list().len(), 2);
        invites.room_changed("T1", "R3", &RoomChange::Ended, "U0");
        let left: Vec<&str> = invites.list().iter().map(|i| i.room.as_str()).collect();
        assert_eq!(left, ["R4"]);
        invites.forget("T2");
        assert!(invites.list().is_empty());
    }

    #[test]
    fn an_invitation_stops_ringing_after_a_while() {
        let now = Instant::now();
        let mut invites = Invites::default();
        invites.add("T1", "C1", "R1", "U2", now);
        invites.add("T1", "C2", "R2", "U2", now + Duration::from_secs(10));
        assert_eq!(invites.expire(now), Some(now + RING_FOR));
        let later = now + RING_FOR;
        assert_eq!(
            invites.expire(later),
            Some(now + Duration::from_secs(10) + RING_FOR)
        );
        assert_eq!(invites.list().len(), 1);
        assert_eq!(invites.expire(later + Duration::from_secs(10)), None);
        assert!(invites.list().is_empty());
    }

    #[test]
    fn huddles_are_checked_every_few_minutes_and_less_after_failures() {
        let now = Instant::now();
        assert_eq!(check_due(None, now), now);
        let ok = Check {
            at: now,
            failures: 0,
        };
        assert_eq!(check_due(Some(&ok), now), now + CHECK_EVERY);
        let failed = Check {
            at: now,
            failures: 1,
        };
        assert_eq!(check_due(Some(&failed), now), now + CHECK_EVERY * 2);
        let refused = Check {
            at: now,
            failures: 40,
        };
        assert_eq!(check_due(Some(&refused), now), now + CHECK_AT_MOST);
        // A check counts from when it was sent, answered or not.
        let mut state = State::default();
        state.checking("T1", "R1", now);
        let key = ("T1".to_owned(), "R1".to_owned());
        assert_eq!(check_due(state.checks.get(&key), now), now + CHECK_EVERY);
        state.checked("T1", "R1", false);
        state.checking("T1", "R1", now + CHECK_EVERY);
        assert_eq!(
            check_due(state.checks.get(&key), now),
            now + CHECK_EVERY * 3
        );
        state.checked("T1", "R1", true);
        assert_eq!(state.checks.get(&key).map(|c| c.failures), Some(0));
    }

    #[test]
    fn a_call_that_stops_ringing_takes_its_invitation_away() {
        let now = Instant::now();
        let mut invites = Invites::default();
        invites.add("T1", "C1", "R1", "U2", now);
        invites.add("T1", "C2", "R2", "U2", now);
        invites.add("T1", "D1", "R3", "U2", now);
        invites.add("T2", "C1", "R4", "U2", now);
        // By room, whatever the conversation said.
        let gone = invites.cancelled("T1", Some("C9"), Some("R1"));
        assert_eq!(
            gone.iter().map(|i| i.room.as_str()).collect::<Vec<_>>(),
            ["R1"]
        );
        // By conversation, when the room is not named.
        let gone = invites.cancelled("T1", Some("D1"), None);
        assert_eq!(
            gone.iter().map(|i| i.channel.as_str()).collect::<Vec<_>>(),
            ["D1"]
        );
        // Nothing named, nothing taken; another workspace's stays.
        assert!(invites.cancelled("T1", None, None).is_empty());
        assert!(invites.cancelled("T1", Some("C1"), None).is_empty());
        let left: Vec<&str> = invites.list().iter().map(|i| i.room.as_str()).collect();
        assert_eq!(left, ["R2", "R4"]);
        // Everyone in it, you among them, from another device.
        invites.room_changed(
            "T1",
            "R2",
            &RoomChange::Participants(vec!["U2".into(), "U0".into()]),
            "U0",
        );
        assert_eq!(invites.list().len(), 1);
    }

    #[test]
    fn room_changes_reach_the_huddle_they_name() {
        let mut huddles = HashMap::from([
            ("C1".to_owned(), huddle("R1", &["U1"])),
            ("C2".to_owned(), huddle("R2", &["U1", "U2"])),
        ]);
        apply_room(&mut huddles, "R1", &RoomChange::Joined("U3".into()));
        apply_room(&mut huddles, "R1", &RoomChange::Joined("U3".into()));
        assert_eq!(huddles["C1"].participants, ["U1", "U3"]);
        apply_room(&mut huddles, "R2", &RoomChange::Left("U2".into()));
        assert_eq!(huddles["C2"].participants, ["U1"]);
        apply_room(
            &mut huddles,
            "R2",
            &RoomChange::Participants(vec!["U1".into(), "U4".into()]),
        );
        assert_eq!(huddles["C2"].participants, ["U1", "U4"]);
        apply_room(&mut huddles, "R2", &RoomChange::Left("U4".into()));
        apply_room(&mut huddles, "R2", &RoomChange::Left("U1".into()));
        assert!(!huddles.contains_key("C2"), "the last one left");
        apply_room(&mut huddles, "R1", &RoomChange::Ended);
        assert!(huddles.is_empty());
        apply_room(&mut huddles, "R9", &RoomChange::Ended);
    }

    #[test]
    fn an_invitation_says_who_and_where() {
        assert_eq!(
            invite_text("Ana", Some("#design")),
            (
                "Ana invites you to a huddle".to_owned(),
                "In #design".to_owned()
            )
        );
        assert_eq!(invite_text("Ana", None).1, "In a direct message");
    }
}
