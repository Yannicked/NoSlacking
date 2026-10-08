//! Listening to a huddle, the app's side: one huddle at a time, shown in
//! the call bar from joining until it is left, with who is in it and who
//! speaks. Left when asked (the bar's Leave, the conversation header's,
//! Ctrl+Shift+H), when everyone else has been gone a minute, on sign-out
//! and on quit; when the huddle ends Chime closes it and the bar says so.
//!
//! With `huddle-video` the bar also says who shares their screen and
//! offers Watch, which opens the call window on that share (see
//! `ui::call_window`), and how many have a camera on, with Video, which
//! opens it on their tiles. Only what the window shows is received;
//! closing it or leaving stops all of it.

use std::time::{Duration, Instant};

use crate::app::App;
use crate::backend;
use crate::failure::Failure;
use crate::i18n::{t, tf};
use crate::people;

#[cfg(feature = "huddle-video")]
pub use crate::huddle_audio::cameras::{Camera, MAX_TILES, Wish};
#[cfg(feature = "huddle-video")]
pub use crate::huddle_audio::gallery::Gallery;
pub use crate::huddle_audio::roster::{Person, Roster};
#[cfg(feature = "huddle-video")]
pub use crate::huddle_audio::screen::{Picture, Screen};
#[cfg(feature = "huddle-video")]
pub use crate::huddle_audio::video::Share;

/// How long you stay once everyone else has left before leaving too:
/// long enough for someone dropping out and back in.
pub const ALONE_FOR: Duration = Duration::from_secs(60);
/// How long a failure shows in the call bar, unless closed sooner.
pub const FAILED_FOR: Duration = Duration::from_secs(30);

/// How listening ended without failing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Left {
    /// You left: asked here, or by signing out or quitting.
    Asked,
    /// The huddle itself ended.
    Ended,
}

/// Where listening to a huddle got to, as the worker tells it.
#[derive(Clone, Debug, PartialEq)]
pub enum Listen {
    /// Joining: Slack, then Chime, then the audio connection.
    Joining,
    /// A call placed: it rings at the far end.
    Ringing,
    /// A meeting keeps you in its lobby until someone lets you in.
    Lobby,
    /// Let in from a meeting's lobby: joining its call.
    Admitted,
    /// The meeting's join link, to invite others with.
    Invite(crate::meetings::MeetingLink),
    /// The audio is connected and playing.
    Live,
    /// Who is in the huddle and speaking now. Sent when it changes, at
    /// most a few times a second.
    Roster(Roster),
    /// Who shares their screen now, when that changes.
    #[cfg(feature = "huddle-video")]
    Shares(Vec<Share>),
    /// Where the watched share's pictures arrive, once per session.
    #[cfg(feature = "huddle-video")]
    Screen(Screen),
    /// Where your camera's self-preview arrives, once per session.
    #[cfg(feature = "huddle-camera")]
    Preview(crate::huddle_camera::Preview),
    /// Who has a camera on now and who has a tile, when that changes.
    #[cfg(feature = "huddle-video")]
    Cameras(Vec<Camera>),
    /// Where the camera tiles' pictures arrive, once per session.
    #[cfg(feature = "huddle-video")]
    Gallery(Gallery),
    /// Left, the huddle over, or failed.
    Ended(Result<Left, Failure>),
}

/// Where the huddle being listened to is.
#[derive(Clone, Debug, PartialEq)]
pub enum Phase {
    /// Joining it.
    Joining,
    /// Ringing the one called, for a call.
    Ringing,
    /// Waiting in a meeting's lobby to be let in.
    Lobby,
    /// Listening since `since`.
    Live { since: Instant },
    /// It failed at `at`; the bar shows why for a while.
    Failed { error: Failure, at: Instant },
}

/// The huddle being listened to: one at a time.
#[derive(Clone, Debug, PartialEq)]
pub struct Listening {
    /// The workspace.
    pub team: String,
    /// The conversation the huddle is in.
    pub channel: String,
    pub phase: Phase,
    /// Who is in it, as last told.
    pub roster: Roster,
    /// Since when no one else has been in it, while that lasts.
    pub alone_since: Option<Instant>,
    /// The microphone: muted on joining a huddle, opening on a call.
    pub mic: crate::huddle_mic::Mic,
    /// Who was called (or called), when this is a call rather than a
    /// huddle.
    pub callee: Option<String>,
    /// Whether the call was answered here rather than placed.
    pub answered: bool,
    /// Whether this is a Teams meeting rather than a huddle or a call.
    pub meeting: bool,
    /// The meeting's join link, once known.
    pub invite: Option<crate::meetings::MeetingLink>,
    /// Who shares their screen, as last told.
    #[cfg(feature = "huddle-video")]
    pub shares: Vec<Share>,
    /// Where the watched share's pictures arrive.
    #[cfg(feature = "huddle-video")]
    pub screen: Option<Screen>,
    /// The share the call window shows, by key; none while it is closed
    /// or shows only cameras.
    #[cfg(feature = "huddle-video")]
    pub watching: Option<String>,
    /// Your camera: off on joining.
    #[cfg(feature = "huddle-camera")]
    pub camera: crate::huddle_camera::Cam,
    /// Where your camera's self-preview arrives.
    #[cfg(feature = "huddle-camera")]
    pub preview: Option<crate::huddle_camera::Preview>,
    /// Your screen share: off on joining.
    #[cfg(feature = "huddle-share")]
    pub sharing: crate::huddle_share::Sharing,
    /// What can be shared, while the call bar offers the choice.
    #[cfg(feature = "huddle-share")]
    pub share_sources: Vec<crate::huddle_share::Source>,
    /// Who has a camera on, as last told; those with a tile first.
    #[cfg(feature = "huddle-video")]
    pub cameras: Vec<Camera>,
    /// Where the camera tiles' pictures arrive.
    #[cfg(feature = "huddle-video")]
    pub gallery: Option<Gallery>,
    /// Whether the call window is open.
    #[cfg(feature = "huddle-video")]
    pub window: bool,
    /// What the session was last told the window wants.
    #[cfg(feature = "huddle-video")]
    pub wish: Wish,
}

impl Listening {
    /// Joining the huddle in `channel` of `team`.
    pub fn new(team: &str, channel: &str) -> Self {
        Self {
            team: team.to_owned(),
            channel: channel.to_owned(),
            phase: Phase::Joining,
            roster: Roster::default(),
            alone_since: None,
            mic: crate::huddle_mic::Mic::Muted,
            callee: None,
            answered: false,
            meeting: false,
            invite: None,
            #[cfg(feature = "huddle-video")]
            shares: Vec::new(),
            #[cfg(feature = "huddle-video")]
            screen: None,
            #[cfg(feature = "huddle-video")]
            watching: None,
            #[cfg(feature = "huddle-camera")]
            camera: crate::huddle_camera::Cam::Off,
            #[cfg(feature = "huddle-camera")]
            preview: None,
            #[cfg(feature = "huddle-share")]
            sharing: crate::huddle_share::Sharing::Off,
            #[cfg(feature = "huddle-share")]
            share_sources: Vec::new(),
            #[cfg(feature = "huddle-video")]
            cameras: Vec::new(),
            #[cfg(feature = "huddle-video")]
            gallery: None,
            #[cfg(feature = "huddle-video")]
            window: false,
            #[cfg(feature = "huddle-video")]
            wish: Wish::closed(),
        }
    }

    /// Calling `callee` from `channel` of `team`: unmuted, as a call is.
    pub fn call(team: &str, channel: &str, callee: &str) -> Self {
        Self {
            mic: crate::huddle_mic::Mic::Opening,
            callee: Some(callee.to_owned()),
            ..Self::new(team, channel)
        }
    }

    /// Joining a Teams meeting in `team`: unmuted, as a call is.
    pub fn meeting(team: &str) -> Self {
        Self {
            mic: crate::huddle_mic::Mic::Opening,
            meeting: true,
            ..Self::new(team, crate::meetings::MEETING_CHANNEL)
        }
    }

    /// Whether this is a call (a meeting is one) rather than a huddle.
    pub fn is_call(&self) -> bool {
        self.callee.is_some() || self.meeting
    }

    /// Who waits in the meeting's lobby, as last told.
    pub fn waiting(&self) -> impl Iterator<Item = &Person> {
        self.roster.people.iter().filter(|p| p.waiting)
    }

    /// What the call window wants, given room for `tiles` tiles of
    /// `tile` pixels: nothing while it is closed.
    #[cfg(feature = "huddle-video")]
    pub fn wish_for(&self, tiles: usize, tile: [u32; 2]) -> Wish {
        if !self.window {
            return Wish::closed();
        }
        Wish {
            open: true,
            share: self.watching.clone(),
            tiles: tiles.min(MAX_TILES),
            tile,
        }
    }

    /// The share being watched, while it lasts.
    #[cfg(feature = "huddle-video")]
    pub fn watched(&self) -> Option<&Share> {
        let key = self.watching.as_deref()?;
        self.shares.iter().find(|s| s.key == key)
    }

    /// Whether you are in the huddle, joining or listening, rather than
    /// looking at why it failed.
    pub fn in_huddle(&self) -> bool {
        !matches!(self.phase, Phase::Failed { .. })
    }

    /// Whether this is the huddle in `channel` of `team`, and you are in
    /// it.
    pub fn is(&self, team: &str, channel: &str) -> bool {
        self.in_huddle() && self.team == team && self.channel == channel
    }
}

/// Since when you have been alone in the huddle, after `roster` came at
/// `now`: still `was` while you stay alone, from now once you are,
/// `None` while anyone else is there. Only counts once the audio plays.
pub fn alone_since(
    was: Option<Instant>,
    roster: &Roster,
    live: bool,
    now: Instant,
) -> Option<Instant> {
    (live && roster.alone()).then(|| was.unwrap_or(now))
}

/// Whether to leave at `now`, alone since `since`.
pub fn leave_alone(since: Option<Instant>, now: Instant) -> bool {
    since.is_some_and(|since| now.saturating_duration_since(since) >= ALONE_FOR)
}

/// A call's length as a clock: `0:05`, `12:34`, `1:02:03`.
pub fn clock(length: Duration) -> String {
    crate::model::duration_text(length.as_secs().saturating_mul(1000))
}

/// Where the huddle is, for the bar's title.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Place<'a> {
    /// A channel, by name.
    Channel(&'a str),
    /// A direct or group message, by who is in it.
    Direct(&'a str),
}

/// The call bar's title: "Listening in #design", "Listening with Ana";
/// "Talking in #design", "Talking with Ana" while `talking`, the
/// microphone live.
pub fn title_text(place: Place<'_>, talking: bool) -> String {
    match (place, talking) {
        (Place::Channel(name), false) => tf("Listening in #{channel}", &[("channel", name)]),
        (Place::Direct(name), false) => tf("Listening with {name}", &[("name", name)]),
        (Place::Channel(name), true) => tf("Talking in #{channel}", &[("channel", name)]),
        (Place::Direct(name), true) => tf("Talking with {name}", &[("name", name)]),
    }
}

/// The call bar's title for a call with `name`: "Calling Ana" until
/// they pick up, "In a call with Ana" after, and at once for a call
/// `answered` here.
pub fn call_title_text(name: &str, phase: &Phase, answered: bool) -> String {
    match phase {
        Phase::Joining | Phase::Ringing if !answered => tf("Calling {name}", &[("name", name)]),
        _ => tf("In a call with {name}", &[("name", name)]),
    }
}

/// The call bar's title for a meeting: "Joining a meeting", "In the
/// lobby" while waiting to be let in, "In a meeting" once in.
pub fn meeting_title_text(phase: &Phase) -> String {
    match phase {
        Phase::Joining | Phase::Ringing => t("Joining a meeting"),
        Phase::Lobby => t("In the lobby"),
        Phase::Live { .. } | Phase::Failed { .. } => t("In a meeting"),
    }
    .into_owned()
}

/// What the call bar says of where listening, or a `call`, is at `now`.
pub fn status_text(phase: &Phase, call: bool, now: Instant) -> String {
    match phase {
        Phase::Joining => t("Joining…").into_owned(),
        Phase::Ringing => t("Ringing…").into_owned(),
        Phase::Lobby => t("Waiting for someone to let you in…").into_owned(),
        Phase::Live { since } => tf(
            "Live · {time}",
            &[("time", &clock(now.saturating_duration_since(*since)))],
        ),
        Phase::Failed { error, .. } if call => {
            tf("Call ended: {error}", &[("error", &error.message())])
        }
        Phase::Failed { error, .. } => {
            tf("Could not listen: {error}", &[("error", &error.message())])
        }
    }
}

/// The faces the call bar shows: one per person, however many devices
/// they are in on (you in Slack and here, say), the others first in the
/// order they came, you last. Those waiting in a lobby are not in yet.
pub fn faces(roster: &Roster) -> Vec<Person> {
    let mut faces: Vec<Person> = Vec::new();
    for person in roster.people.iter().filter(|p| !p.waiting) {
        let same = faces
            .iter_mut()
            .find(|f| f.user.is_some() && f.user == person.user);
        match same {
            Some(face) => {
                face.me |= person.me;
                face.muted &= person.muted;
                face.speaking |= person.speaking;
            }
            None => faces.push(person.clone()),
        }
    }
    faces.sort_by_key(|f| f.me);
    faces
}

/// Listens to the huddle in `channel` of `team` here, leaving any other.
pub fn listen(app: &mut App, team: String, channel: String) {
    if !super::is_session(app, &team) {
        return;
    }
    // The worker listens to one huddle at a time and leaves the last
    // itself; a different workspace's is told to stop.
    if let Some(last) = app.huddles.listening.take()
        && last.team != team
        && last.in_huddle()
    {
        app.backend.send(backend::Command::People {
            team: last.team,
            command: people::Command::LeaveHuddle,
        });
    }
    app.huddles.listening = Some(Listening::new(&team, &channel));
    app.backend.send(backend::Command::People {
        team,
        command: people::Command::ListenHuddle { channel },
    });
}

/// Calls `user` from the one-to-one chat `channel` of `team`, ending any
/// other call or huddle.
pub fn call(app: &mut App, team: String, channel: String, user: String) {
    if let Some(last) = app.huddles.listening.take()
        && last.team != team
        && last.in_huddle()
    {
        app.backend.send(backend::Command::People {
            team: last.team,
            command: people::Command::LeaveHuddle,
        });
    }
    app.huddles.listening = Some(Listening::call(&team, &channel, &user));
    app.backend.send(backend::Command::People {
        team,
        command: people::Command::Call { channel, user },
    });
}

/// Joins `meeting` (or, with none, starts one now) in `team`, ending any
/// other call or huddle.
pub fn join_meeting(app: &mut App, team: String, meeting: Option<crate::meetings::Meeting>) {
    if let Some(last) = app.huddles.listening.take()
        && last.team != team
        && last.in_huddle()
    {
        app.backend.send(backend::Command::People {
            team: last.team,
            command: people::Command::LeaveHuddle,
        });
    }
    app.huddles.listening = Some(Listening::meeting(&team));
    let command = match meeting {
        Some(meeting) => people::Command::JoinMeeting { meeting },
        None => people::Command::MeetNow {
            subject: tf("Meeting with {name}", &[("name", &own_name(app, &team))]),
        },
    };
    app.backend.send(backend::Command::People { team, command });
}

/// Your name in `team`, as a meeting started here is named after you.
fn own_name(app: &App, team: &str) -> String {
    app.workspaces
        .iter()
        .find(|w| w.info.team_id == team)
        .map(|w| w.user_label(&w.info.user_id))
        .unwrap_or_default()
}

/// Lets `user` (by the id the interface knows them by) in from the
/// meeting's lobby.
pub fn admit(app: &mut App, user: String) {
    let Some(listening) = app.huddles.listening.as_ref().filter(|l| l.meeting) else {
        return;
    };
    let team = listening.team.clone();
    app.backend.send(backend::Command::People {
        team,
        command: people::Command::Admit { user },
    });
}

/// Picks up the incoming call `call` of `team` (its invitation's room):
/// opens the chat it rang in and shows the call there, ending any other
/// call or huddle.
pub fn answer(app: &mut App, team: String, call: String) {
    let Some(invite) = app.huddles.invites.answered(&team, &call) else {
        return;
    };
    app.withdraw_invite_note(&team, &invite.channel);
    if let Some(last) = app.huddles.listening.take()
        && last.team != team
        && last.in_huddle()
    {
        app.backend.send(backend::Command::People {
            team: last.team,
            command: people::Command::LeaveHuddle,
        });
    }
    let known = app
        .workspaces
        .iter()
        .any(|w| w.info.team_id == team && w.conversation(&invite.channel).is_some());
    app.actions
        .push(crate::model::Action::SelectWorkspace(team.clone()));
    if known {
        app.actions.push(crate::model::Action::OpenConversation(
            invite.channel.clone(),
        ));
    }
    app.huddles.listening = Some(Listening {
        answered: true,
        ..Listening::call(&team, &invite.channel, &invite.from)
    });
    app.backend.send(backend::Command::People {
        team,
        command: people::Command::AnswerCall {
            channel: invite.channel,
            call,
        },
    });
}

/// Opens the call window on the share `key` (switching from any other)
/// or, with none, closes it: only what it shows is received.
#[cfg(feature = "huddle-video")]
pub fn watch(app: &mut App, key: Option<String>) {
    let Some(listening) = app.huddles.listening.as_mut().filter(|l| l.in_huddle()) else {
        return;
    };
    if listening.watching == key && listening.window == key.is_some() {
        return;
    }
    if listening.watching != key
        && let Some(screen) = &listening.screen
    {
        // Not the last share's picture in the new one's window.
        screen.clear();
    }
    listening.window = key.is_some();
    listening.watching = key;
    tell_wish(app, None);
}

/// Opens the call window on the cameras, and the first share if someone
/// shares.
#[cfg(feature = "huddle-video")]
pub fn open_call(app: &mut App) {
    let Some(listening) = app.huddles.listening.as_mut().filter(|l| l.in_huddle()) else {
        return;
    };
    if listening.window {
        return;
    }
    listening.window = true;
    if listening.watching.is_none() {
        listening.watching = listening.shares.first().map(|s| s.key.clone());
    }
    tell_wish(app, None);
}

/// Tells the session what the call window wants, if that changed: room
/// for the tiles `room` says (count and size), or as last said when it
/// has not drawn yet (all of them, at their largest layer, until the
/// window's first frame says).
#[cfg(feature = "huddle-video")]
pub fn tell_wish(app: &mut App, room: Option<(usize, [u32; 2])>) {
    let Some(listening) = app.huddles.listening.as_mut().filter(|l| l.in_huddle()) else {
        return;
    };
    let (tiles, tile) = room.unwrap_or(if listening.wish.open {
        (listening.wish.tiles, listening.wish.tile)
    } else {
        (MAX_TILES, [0, 0])
    });
    let wish = listening.wish_for(tiles, tile);
    if wish == listening.wish {
        return;
    }
    listening.wish = wish.clone();
    let team = listening.team.clone();
    app.backend.send(backend::Command::People {
        team,
        command: people::Command::WatchCall { wish },
    });
}

/// What the call bar says of the cameras on: "2 cameras on".
#[cfg(feature = "huddle-video")]
pub fn cameras_text(count: usize) -> String {
    crate::i18n::tn(
        "{count} camera on",
        "{count} cameras on",
        u32::try_from(count).unwrap_or(u32::MAX),
    )
}

/// What the call bar says of someone sharing: "Ana is sharing their
/// screen".
#[cfg(feature = "huddle-video")]
pub fn sharing_text(name: &str) -> String {
    tf("{name} is sharing their screen", &[("name", name)])
}

/// Leaves the huddle being listened to, or closes the failure shown.
pub fn leave(app: &mut App) {
    if let Some(last) = app.huddles.listening.take()
        && last.in_huddle()
    {
        app.backend.send(backend::Command::People {
            team: last.team,
            command: people::Command::LeaveHuddle,
        });
    }
}

/// Takes in where listening to the huddle in `channel` of `team` got to.
/// News of a session already left or replaced is dropped.
pub fn heard(app: &mut App, team: &str, channel: &str, state: Listen) {
    let now = Instant::now();
    let Some(listening) = app
        .huddles
        .listening
        .as_mut()
        .filter(|l| l.is(team, channel))
    else {
        return;
    };
    match state {
        Listen::Joining => {}
        Listen::Ringing => {
            if listening.phase == Phase::Joining {
                listening.phase = Phase::Ringing;
            }
        }
        Listen::Lobby => listening.phase = Phase::Lobby,
        Listen::Admitted => {
            if listening.phase == Phase::Lobby {
                listening.phase = Phase::Joining;
            }
        }
        Listen::Invite(link) => listening.invite = Some(link),
        Listen::Live => {
            if !matches!(listening.phase, Phase::Live { .. }) {
                listening.phase = Phase::Live { since: now };
            }
        }
        Listen::Roster(roster) => {
            let live = matches!(listening.phase, Phase::Live { .. });
            listening.alone_since = alone_since(listening.alone_since, &roster, live, now);
            listening.roster = roster;
        }
        #[cfg(feature = "huddle-video")]
        Listen::Shares(shares) => {
            listening.shares = shares;
            // The share watched has ended: the window goes on with the
            // cameras, or closes if there are none.
            if listening.watching.is_some() && listening.watched().is_none() {
                if listening.cameras.is_empty() {
                    watch(app, None);
                } else {
                    listening.watching = None;
                    tell_wish(app, None);
                }
                app.toast(t("The screen share ended"), false);
            }
        }
        #[cfg(feature = "huddle-video")]
        Listen::Screen(screen) => listening.screen = Some(screen),
        #[cfg(feature = "huddle-camera")]
        Listen::Preview(preview) => listening.preview = Some(preview),
        #[cfg(feature = "huddle-video")]
        Listen::Cameras(cameras) => listening.cameras = cameras,
        #[cfg(feature = "huddle-video")]
        Listen::Gallery(gallery) => listening.gallery = Some(gallery),
        Listen::Ended(Ok(Left::Asked)) => app.huddles.listening = None,
        Listen::Ended(Ok(Left::Ended)) => {
            let ended = if listening.meeting {
                t("The meeting ended")
            } else if listening.is_call() {
                t("The call ended")
            } else {
                t("The huddle ended")
            };
            app.huddles.listening = None;
            app.toast(ended, false);
        }
        Listen::Ended(Err(error)) => {
            listening.phase = Phase::Failed { error, at: now };
            listening.alone_since = None;
            // The camera closed with the session.
            #[cfg(feature = "huddle-camera")]
            {
                listening.camera = crate::huddle_camera::Cam::Off;
                listening.preview = None;
            }
            // And the share with it.
            #[cfg(feature = "huddle-share")]
            {
                listening.sharing = crate::huddle_share::Sharing::Off;
                listening.share_sources.clear();
            }
            // The session is gone, and its shares with it.
            #[cfg(feature = "huddle-video")]
            {
                listening.watching = None;
                listening.window = false;
                listening.wish = Wish::closed();
                listening.shares.clear();
                listening.cameras.clear();
            }
        }
    }
}

/// Runs every frame: forgets the huddle of a workspace signed out, leaves
/// one everyone else left a while ago, and lets a failure go after a
/// while.
pub fn frame(app: &mut App, now: Instant) {
    let Some(listening) = &app.huddles.listening else {
        return;
    };
    // Signed out while listening: the worker stopped it, and its news
    // went with the workspace.
    let signed_in = if listening.is_call() {
        super::is_signed_in(app, &listening.team)
    } else {
        super::is_session(app, &listening.team)
    };
    if !signed_in {
        app.huddles.listening = None;
        return;
    }
    if let Phase::Failed { at, .. } = listening.phase {
        let until = at + FAILED_FOR;
        if until <= now {
            app.huddles.listening = None;
        } else {
            app.waker.wake_after(until - now);
        }
        return;
    }
    // A call ends when the far end hangs up, which Teams says; only a
    // huddle is left for being alone in.
    if !listening.is_call() && leave_alone(listening.alone_since, now) {
        leave(app);
        app.toast(t("Everyone else left the huddle"), false);
    } else if let Some(since) = listening.alone_since {
        app.waker
            .wake_after((since + ALONE_FOR).saturating_duration_since(now));
    }
}

/// Leaves the huddle being listened to before the app quits, waiting a
/// little for Chime to hear it: the worker's thread goes with the app.
pub fn quit(app: &mut App) {
    let Some(last) = app.huddles.listening.take().filter(Listening::in_huddle) else {
        return;
    };
    app.backend.send(backend::Command::People {
        team: last.team,
        command: people::Command::LeaveHuddle,
    });
    let until = Instant::now() + Duration::from_secs(4);
    while Instant::now() < until {
        match app.backend.try_recv() {
            Some(backend::Event::People {
                event:
                    people::Event::Listening {
                        state: Listen::Ended(_),
                        ..
                    },
                ..
            }) => return,
            Some(_) => {}
            None => std::thread::sleep(Duration::from_millis(20)),
        }
    }
    log::warn!("left the huddle without hearing back");
}

#[cfg(test)]
mod tests {
    use super::*;

    fn person(user: &str, me: bool, muted: bool, speaking: bool) -> Person {
        Person {
            user: Some(user.into()),
            me,
            muted,
            speaking,
            name: None,
            waiting: false,
        }
    }

    #[test]
    fn everyone_else_gone_a_minute_means_leaving() {
        let now = Instant::now();
        let alone = Roster {
            people: vec![person("U0", true, true, false)],
            count: Some(1),
        };
        let company = Roster {
            people: vec![
                person("U0", true, true, false),
                person("U2", false, false, false),
            ],
            count: Some(2),
        };
        // Not while joining: the roster may not have come in full.
        assert_eq!(alone_since(None, &alone, false, now), None);
        let since = alone_since(None, &alone, true, now);
        assert_eq!(since, Some(now));
        // Staying alone keeps the first moment.
        let later = now + Duration::from_secs(30);
        assert_eq!(alone_since(since, &alone, true, later), Some(now));
        assert!(!leave_alone(since, later));
        assert!(leave_alone(since, now + ALONE_FOR));
        // Someone back in time: the minute starts over.
        assert_eq!(alone_since(since, &company, true, later), None);
        assert!(!leave_alone(None, now + ALONE_FOR * 10));
    }

    #[test]
    fn the_bar_says_where_and_how_long() {
        assert_eq!(clock(Duration::from_secs(5)), "0:05");
        assert_eq!(clock(Duration::from_secs(12 * 60 + 34)), "12:34");
        assert_eq!(clock(Duration::from_secs(3723)), "1:02:03");
        assert_eq!(
            title_text(Place::Channel("design"), false),
            "Listening in #design"
        );
        assert_eq!(
            title_text(Place::Direct("Ana"), false),
            "Listening with Ana"
        );
        assert_eq!(
            title_text(Place::Channel("design"), true),
            "Talking in #design"
        );
        assert_eq!(title_text(Place::Direct("Ana"), true), "Talking with Ana");
        let now = Instant::now();
        assert_eq!(status_text(&Phase::Joining, false, now), "Joining…");
        assert_eq!(status_text(&Phase::Ringing, true, now), "Ringing…");
        assert_eq!(
            status_text(
                &Phase::Live {
                    since: now - Duration::from_secs(134)
                },
                false,
                now
            ),
            "Live · 2:14"
        );
        let failed = Phase::Failed {
            error: Failure::Huddle(crate::failure::HuddleTrouble::Lost),
            at: now,
        };
        assert!(status_text(&failed, false, now).starts_with("Could not listen: "));
        assert!(status_text(&failed, true, now).starts_with("Call ended: "));
        assert_eq!(
            call_title_text("Ana", &Phase::Ringing, false),
            "Calling Ana"
        );
        assert_eq!(
            call_title_text("Ana", &Phase::Live { since: now }, false),
            "In a call with Ana"
        );
        assert_eq!(
            call_title_text("Ana", &Phase::Joining, true),
            "In a call with Ana"
        );
    }

    #[test]
    fn faces_are_one_per_person_you_last() {
        let roster = Roster {
            people: vec![
                person("U0", true, true, false),
                person("U2", false, false, true),
                // You again, from Slack's own app, unmuted and talking.
                person("U0", false, false, true),
                person("U3", false, true, false),
                Person {
                    user: None,
                    me: false,
                    muted: false,
                    speaking: false,
                    name: None,
                    waiting: false,
                },
            ],
            count: Some(5),
        };
        let faces = faces(&roster);
        let seen: Vec<(Option<&str>, bool, bool, bool)> = faces
            .iter()
            .map(|f| (f.user.as_deref(), f.me, f.muted, f.speaking))
            .collect();
        assert_eq!(
            seen,
            [
                (Some("U2"), false, false, true),
                (Some("U3"), false, true, false),
                (None, false, false, false),
                (Some("U0"), true, false, true),
            ]
        );
    }

    #[cfg(feature = "huddle-video")]
    #[test]
    fn the_watched_share_is_the_one_still_going() {
        let mut listening = Listening::new("T1", "C1");
        assert!(listening.watched().is_none());
        listening.shares = vec![
            Share {
                key: "a#content".into(),
                user: Some("U2".into()),
            },
            Share {
                key: "b#content".into(),
                user: None,
            },
        ];
        assert!(listening.watched().is_none(), "nothing watched");
        listening.watching = Some("b#content".into());
        assert_eq!(listening.watched().map(|s| s.user.clone()), Some(None));
        listening.shares.truncate(1);
        assert!(listening.watched().is_none(), "b stopped sharing");
        assert_eq!(sharing_text("Ana"), "Ana is sharing their screen");
    }

    #[test]
    fn news_of_another_huddle_is_dropped() {
        let mut listening = Listening::new("T1", "C1");
        assert!(listening.is("T1", "C1"));
        assert!(!listening.is("T1", "C2") && !listening.is("T2", "C1"));
        listening.phase = Phase::Failed {
            error: Failure::Huddle(crate::failure::HuddleTrouble::Lost),
            at: Instant::now(),
        };
        assert!(!listening.in_huddle() && !listening.is("T1", "C1"));
    }
}
