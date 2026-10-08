//! A 1:1 Teams call, from ringing to its end: the signalling of
//! `docs/research/teams-calls.md` §A (outgoing), §B (incoming) and §C
//! driving a [`MediaSession`].
//!
//! [`outgoing`], [`incoming`] and [`meeting`] run the whole call on its
//! task. What
//! happens is told through a callback as [`CallEvent`]s, and the caller
//! steers it with [`Control`]s (mute, hang up); an incoming call also
//! waits for an [`Answer`]. It never waits on the network to stop:
//! hanging up closes the media at once and sends `leave` on the way out.

use std::collections::BTreeMap;
use std::time::Duration;

use tokio::sync::{mpsc, oneshot};

use super::api::{CallApi, CallIds, own_participant, relay_credentials, relay_servers};
use super::codes::{self, Ending};
use super::media::{self, Audio, MediaConfig, MediaEvent, MediaSession, Relay};
use super::types::{
    CpconvAnswer, IncomingInvitation, IncomingNotification, MediaContent, MediaNegotiation,
    Participant, Push, RosterUpdate,
};
use super::{LocalMedia, RemoteMedia, sdp};
use crate::failure::Failure;
use crate::meetings::Meeting;
use crate::teams::client::TeamsClient;

/// How long an unanswered call rings before we give up.
const RING_FOR: Duration = Duration::from_secs(60);
/// How long an incoming call rings here before it counts as missed; the
/// caller usually gives up first.
const RINGS_HERE_FOR: Duration = Duration::from_secs(60);
/// How long a hang-up's `leave` may take before the call is ended anyway.
const LEAVE_WITHIN: Duration = Duration::from_secs(5);

/// What the caller asks of a running call.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Control {
    /// Mute (or unmute) the microphone, and tell the far end.
    Mute(bool),
    /// Start (or stop) sending our screen share: a renegotiation of
    /// ours turns the share line on or off (§C.6).
    Share(bool),
    /// Let someone in from the meeting's lobby: by their MRI, or the id
    /// the interface knows them by (the MRI's last part, as for a guest's
    /// `8:teamsvisitor:…`).
    Admit(String),
    /// Our camera went on (or off). The media sends it by itself; a
    /// meeting must be told it is on before it forwards it (§H.8).
    Camera(bool),
    /// End the call.
    HangUp,
}

/// What a call tells as it goes; the last is always [`CallEvent::Ended`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CallEvent {
    /// The call was placed: it rings at the far end (or, incoming, here).
    Ringing,
    /// Picked up, and the media is up: the call can be heard.
    Live,
    /// The media is connected and audio flows both ways.
    AudioFlowing,
    /// The one called muted (`true`) or unmuted their microphone.
    FarEndMuted(bool),
    /// The far end's camera started (`true`) or stopped showing.
    FarEndVideo(bool),
    /// The far end's screen share started (`true`) or stopped showing.
    FarEndShare(bool),
    /// A meeting keeps you in its lobby until someone lets you in.
    Lobby,
    /// Let in from the lobby: the call is joined, and [`Self::Live`]
    /// follows once its media is up.
    Admitted,
    /// Who else is in the meeting, or waits in its lobby, now.
    People(Vec<Attendee>),
    /// An incoming call stopped ringing because another device of yours
    /// (or another delivery of the same call here) picked it up: not a
    /// missed call.
    AnsweredElsewhere,
    /// The call is over: normally (`Ok`) or because something failed.
    Ended {
        result: Result<(), Failure>,
        /// What went over the wire, for a probe's summary.
        counts: media::Counts,
    },
}

/// Someone in a meeting, or waiting to be let in, as its roster says.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Attendee {
    /// `8:live:…`, `8:orgid:…`, or a guest's id.
    pub mri: String,
    /// The name the meeting shows; guests are known by nothing else.
    pub name: Option<String>,
    /// Waiting in the lobby, not yet in the call.
    pub waiting: bool,
    pub muted: bool,
}

/// Who is in a meeting, from its roster's deltas.
#[derive(Debug, Default)]
struct People {
    /// By MRI: the roster version last taken, and the attendee.
    by_mri: BTreeMap<String, (u64, Attendee)>,
}

impl People {
    /// Takes in a roster delta, leaving out `me`; answers whether anyone
    /// shown changed.
    fn take(&mut self, update: &RosterUpdate, me: &str) -> bool {
        let mut changed = false;
        for (mri, them) in &update.participants {
            if mri.eq_ignore_ascii_case(me) {
                continue;
            }
            if self
                .by_mri
                .get(mri)
                .is_some_and(|(version, _)| them.version < *version)
            {
                continue;
            }
            if !them.is_active() {
                changed |= self.by_mri.remove(mri).is_some();
                continue;
            }
            let attendee = Attendee {
                mri: mri.clone(),
                name: them.name().map(str::to_owned),
                waiting: them.is_waiting(),
                muted: them.is_muted(),
            };
            let was = self
                .by_mri
                .insert(mri.clone(), (them.version, attendee.clone()));
            changed |= was.map(|(_, a)| a).as_ref() != Some(&attendee);
        }
        changed
    }

    /// The MRI of `user`: an MRI listed, or one whose last part is it.
    fn mri_of(&self, user: &str) -> Option<String> {
        self.by_mri
            .keys()
            .find(|mri| mri.as_str() == user || mri.ends_with(&format!(":{user}")))
            .cloned()
    }

    /// Everyone, by MRI.
    fn list(&self) -> Vec<Attendee> {
        self.by_mri.values().map(|(_, a)| a.clone()).collect()
    }
}

/// The relay for a call, if Teams hands out credentials for it.
async fn relay_for(client: &TeamsClient) -> Option<Relay> {
    // The relay is how a far end behind another network reaches us; a
    // call on the same network still works without it.
    match relay_credentials(client).await {
        Ok(credentials) => Some(Relay::new(&relay_servers(client).await, credentials)),
        Err(error) => {
            log::warn!("no Teams relay for the call: {error:?}");
            None
        }
    }
}

/// Joins `meeting` with `audio`, and runs the call until it ends: through
/// its lobby if it has you wait, and onto the meeting's own media server
/// once you are let in (§H). Every step is told through `tell`; `control`
/// steers it. Needs the account's live connection up.
pub async fn meeting(
    client: TeamsClient,
    meeting: Meeting,
    audio: Audio,
    control: mpsc::UnboundedReceiver<Control>,
    tell: impl Fn(CallEvent) + Send,
) {
    let mut counts = media::Counts::default();
    let result = run_meeting(&client, &meeting, audio, control, &tell, &mut counts).await;
    if let Err(error) = &result {
        log::warn!("Teams meeting ended with a failure: {error:?}");
    }
    tell(CallEvent::Ended { result, counts });
}

async fn run_meeting(
    client: &TeamsClient,
    meeting: &Meeting,
    audio: Audio,
    mut control: mpsc::UnboundedReceiver<Control>,
    tell: &(impl Fn(CallEvent) + Send),
    counts: &mut media::Counts,
) -> Result<(), Failure> {
    let surl = client
        .calls()
        .surl()
        .ok_or_else(|| Failure::CallFailed("the live connection is not up".into()))?;
    let endpoint = client
        .endpoint_id()
        .unwrap_or_else(crate::model::new_client_msg_id);
    let ids = CallIds::new(&endpoint);
    let me = own_participant(client, &ids)
        .ok_or_else(|| Failure::CallFailed("who you are is not known".into()))?;
    let mut inbox = client.calls().register(&ids.call_agent_id);
    let agent = ids.call_agent_id.clone();
    let mut call = Call::with(CallApi::new(client.clone(), ids, me.clone(), &surl).meeting());
    call.meeting = Some(InMeeting {
        me: me.id,
        lobby: false,
        people: People::default(),
        camera_on: false,
        // The join's descriptions are the first.
        request: 1,
        changes: 0,
        update_descriptions: None,
        camera_capabilities: None,
    });
    call.relay = relay_for(client).await;
    let ended = call
        .join(meeting, audio, &mut inbox, &mut control, tell, counts)
        .await;
    client.calls().forget(&agent);
    ended
}

/// Calls `callee` (an MRI, `8:live:…` or `8:orgid:…`, or the id the
/// interface knows a person by, `live:…` or an object id) with `audio`, and
/// runs the call until it ends. Every step is told through `tell`;
/// `control` steers it. Needs the account's live connection up: the
/// call's callbacks name it.
pub async fn outgoing(
    client: TeamsClient,
    callee: String,
    audio: Audio,
    control: mpsc::UnboundedReceiver<Control>,
    tell: impl Fn(CallEvent) + Send,
) {
    // Teams rings only a full MRI; the interface's ids drop the `8:`.
    let callee = crate::teams::client::user_mri(&callee);
    let mut counts = media::Counts::default();
    let result = run(&client, &callee, audio, control, &tell, &mut counts).await;
    if let Err(error) = &result {
        log::warn!("Teams call ended with a failure: {error:?}");
    }
    tell(CallEvent::Ended { result, counts });
}

async fn run(
    client: &TeamsClient,
    callee: &str,
    audio: Audio,
    mut control: mpsc::UnboundedReceiver<Control>,
    tell: &(impl Fn(CallEvent) + Send),
    counts: &mut media::Counts,
) -> Result<(), Failure> {
    let surl = client
        .calls()
        .surl()
        .ok_or_else(|| Failure::CallFailed("the live connection is not up".into()))?;
    let endpoint = client
        .endpoint_id()
        .unwrap_or_else(crate::model::new_client_msg_id);
    let ids = CallIds::new(&endpoint);
    let me = own_participant(client, &ids)
        .ok_or_else(|| Failure::CallFailed("who you are is not known".into()))?;

    let relay = relay_for(client).await;
    let (mut session, mut local) = MediaSession::start(MediaConfig::offer(relay.clone()), audio)
        .await
        .map_err(media_failure)?;
    log::info!(
        "Teams call: media ready with {} candidates",
        local.candidates.len()
    );

    let mut inbox = client.calls().register(&ids.call_agent_id);
    let agent = ids.call_agent_id.clone();
    let mut call = Call::new(client, ids, me, &surl);
    call.relay = relay;
    let ended = call
        .ring_and_talk(
            callee,
            &mut session,
            &mut local,
            &mut inbox,
            &mut control,
            tell,
        )
        .await;
    client.calls().forget(&agent);
    session.stop();
    *counts = session.counts();
    ended
}

/// An incoming call, as its notification tells it. Its links let anyone
/// act on the call, so `Debug` shows its id only.
#[derive(Clone)]
pub struct Incoming {
    /// The chain id of every request for this call.
    pub call_id: String,
    /// Who calls.
    pub caller: Participant,
    /// Our id in this call, which the notification gave.
    participant_id: String,
    attach: String,
    /// The conversation to join.
    conversation: String,
    /// The caller's offer.
    offer: MediaContent,
}

impl std::fmt::Debug for Incoming {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Incoming")
            .field("call_id", &self.call_id)
            .finish_non_exhaustive()
    }
}

impl Incoming {
    /// Reads a call notification (B.1); `None` for one that is not a 1:1
    /// call we could answer.
    pub fn read(note: &IncomingNotification) -> Option<Self> {
        let gp = note.gp.as_ref()?;
        let call = gp.call_notification.as_ref()?;
        let invitation = gp.conversation_invitation.as_ref()?;
        if invitation.is_multi_party {
            return None;
        }
        Some(Self {
            call_id: gp.debug_content.as_ref()?.call_id.clone()?,
            caller: call.from.clone()?,
            participant_id: call.to.as_ref()?.participant_id.clone()?,
            attach: call.links.attach.clone()?,
            conversation: invitation.conversation_controller.clone()?,
            offer: call.media_content.clone().filter(|m| !m.blob.is_empty())?,
        })
    }

    /// Who calls, by the name the notification gave.
    pub fn caller_name(&self) -> Option<&str> {
        self.caller
            .display_name
            .as_deref()
            .filter(|n| !n.is_empty())
    }
}

/// What the person decided about an incoming call.
#[derive(Debug)]
pub enum Answer {
    /// Pick up, with this sound.
    Accept(Audio),
    /// Decline.
    Decline,
}

/// Rings for the incoming `call` until `answer` says what to do, then
/// runs it until it ends. Every step is told through `tell`; `control`
/// steers it once picked up. Declined, or given up on by the caller, it
/// ends `Ok` without ever being live.
pub async fn incoming(
    client: TeamsClient,
    call: Incoming,
    answer: oneshot::Receiver<Answer>,
    control: mpsc::UnboundedReceiver<Control>,
    tell: impl Fn(CallEvent) + Send,
) {
    let mut counts = media::Counts::default();
    let result = run_incoming(&client, call, answer, control, &tell, &mut counts).await;
    if let Err(error) = &result {
        log::warn!("Teams call ended with a failure: {error:?}");
    }
    tell(CallEvent::Ended { result, counts });
}

async fn run_incoming(
    client: &TeamsClient,
    call: Incoming,
    answer: oneshot::Receiver<Answer>,
    mut control: mpsc::UnboundedReceiver<Control>,
    tell: &(impl Fn(CallEvent) + Send),
    counts: &mut media::Counts,
) -> Result<(), Failure> {
    let surl = client
        .calls()
        .surl()
        .ok_or_else(|| Failure::CallFailed("the live connection is not up".into()))?;
    let endpoint = client
        .endpoint_id()
        .unwrap_or_else(crate::model::new_client_msg_id);
    let ids = CallIds::incoming(&endpoint, &call.call_id, &call.participant_id);
    let me = own_participant(client, &ids)
        .ok_or_else(|| Failure::CallFailed("who you are is not known".into()))?;
    let mut inbox = client.calls().register(&ids.call_agent_id);
    let agent = ids.call_agent_id.clone();
    let mut this = Call::with(CallApi::new(client.clone(), ids, me, &surl).incoming());
    this.callee = call.caller.id.clone();
    let ended = this
        .ring_here(client, call, answer, &mut inbox, &mut control, tell, counts)
        .await;
    client.calls().forget(&agent);
    ended
}

/// The signalling side of one call.
struct Call {
    api: CallApi,
    /// The conversation's links, once the call is placed.
    conversation: Option<CpconvAnswer>,
    /// Whether the far end picked up.
    accepted: bool,
    /// Whether the media is up.
    connected: bool,
    /// Where and how often to say the call leg is still here, once
    /// picked up.
    keep_alive: Option<KeepAlive>,
    /// The one called, as the roster names them.
    callee: String,
    /// What the roster last said of them: its version, and whether
    /// they were muted.
    far_end: Option<(u64, bool)>,
    /// The far end's latest description: the shape our own offers take.
    last_remote: Option<RemoteMedia>,
    /// Where our renegotiations go: the far end's `mediaRenegotiation`.
    renegotiation: Option<String>,
    /// The call's media leg id, which every SDP of ours names.
    leg: String,
    /// Our screen share as renegotiated.
    share: ShareState,
    /// Whether we asked to be muted: a new media session of the call
    /// starts so too.
    muted: bool,
    /// The relay the media was started with, for a new session of the
    /// call.
    relay: Option<Relay>,
    /// A meeting's side of the call, when it is one.
    meeting: Option<InMeeting>,
}

/// What a call that is a meeting keeps.
#[derive(Debug)]
struct InMeeting {
    /// Our MRI, which the roster lists too.
    me: String,
    /// Whether the meeting keeps us in its lobby.
    lobby: bool,
    people: People,
    /// Whether our camera is on, as the meeting was last told.
    camera_on: bool,
    /// The last `mediaDescriptions` number sent.
    request: u32,
    /// How many times the video lines' use was changed, for the tag.
    changes: u32,
    /// The call leg's links for changing the video lines' use and saying
    /// what our camera sends.
    update_descriptions: Option<String>,
    camera_capabilities: Option<String>,
}

impl InMeeting {
    /// Takes in the video links of a call leg, as an acceptance or a
    /// move to another media server hands them out.
    fn take_links(&mut self, links: &super::types::AcceptanceLinks) {
        if let Some(url) = &links.update_media_descriptions {
            self.update_descriptions = Some(url.clone());
        }
        if let Some(url) = &links.apply_channel_parameters {
            self.camera_capabilities = Some(url.clone());
        }
    }
}

/// Where our screen share's renegotiation stands.
#[derive(Debug, Default)]
struct ShareState {
    /// Whether we want to share.
    wanted: bool,
    /// Whether we share as last agreed.
    agreed: bool,
    /// The sharing an offer of ours asked for, while it waits for its
    /// answer.
    offered: Option<bool>,
    /// Whether a refused offer was sent again already.
    retried: bool,
    /// The number of the share started last, for the offers' tag: odd,
    /// rising by two per share, as the web client's (`ss_1`, `ss_3`).
    number: u32,
}

/// The call leg's keep-alive: its link, and when it is next due.
struct KeepAlive {
    call_leg: String,
    every: Duration,
    next: tokio::time::Instant,
}

/// How long before Teams' keep-alive interval runs out to send one: at
/// nine tenths of it, as the web client does.
fn keep_alive_every(interval_secs: u64) -> Duration {
    Duration::from_secs(interval_secs.max(1)) * 9 / 10
}

/// What `update` says of `callee`: its version and whether they are
/// muted, when it names them in a version no older than `last`'s. A
/// roster is a delta: one that leaves them out says nothing of them.
fn far_end(update: &RosterUpdate, callee: &str, last: Option<(u64, bool)>) -> Option<(u64, bool)> {
    let (_, them) = update
        .participants
        .iter()
        .find(|(mri, _)| mri.eq_ignore_ascii_case(callee))?;
    if last.is_some_and(|(version, _)| them.version < version) {
        return None;
    }
    Some((them.version, them.is_muted()))
}

/// Waits until the keep-alive is due; never while there is none.
async fn keep_alive_due(keep_alive: Option<&KeepAlive>) {
    match keep_alive {
        Some(keep_alive) => tokio::time::sleep_until(keep_alive.next).await,
        None => std::future::pending().await,
    }
}

impl Call {
    fn new(client: &TeamsClient, ids: CallIds, me: Participant, surl: &str) -> Self {
        Self::with(CallApi::new(client.clone(), ids, me, surl))
    }

    fn with(api: CallApi) -> Self {
        Self {
            api,
            conversation: None,
            accepted: false,
            connected: false,
            keep_alive: None,
            callee: String::new(),
            far_end: None,
            last_remote: None,
            renegotiation: None,
            leg: String::new(),
            share: ShareState::default(),
            muted: false,
            relay: None,
            meeting: None,
        }
    }

    /// Finds the meeting, starts the media and joins with our offer
    /// (§H.2, §H.3), then talks until the end.
    async fn join(
        &mut self,
        meeting: &Meeting,
        audio: Audio,
        inbox: &mut mpsc::UnboundedReceiver<Push>,
        control: &mut mpsc::UnboundedReceiver<Control>,
        tell: &(impl Fn(CallEvent) + Send),
        counts: &mut media::Counts,
    ) -> Result<(), Failure> {
        let preheated = self
            .api
            .preheat(meeting)
            .await
            .map_err(|error| match error {
                // A link or passcode the meeting service does not know.
                Failure::Http(status) if (400..500).contains(&status) && status != 401 => {
                    Failure::MeetingNotFound
                }
                other => other,
            })?;
        log::info!("Teams meeting: found");
        let (mut session, mut local) =
            MediaSession::start(MediaConfig::meeting(self.relay.clone()), audio)
                .await
                .map_err(media_failure)?;
        self.leg = self.api.ids().media_leg_id.clone();
        let offer = sdp::offer(&local);
        let joined = match self.api.join_meeting(&preheated, &offer).await {
            Ok(joined) => joined,
            Err(error) => {
                session.stop();
                return Err(error);
            }
        };
        let links = preheated.links.clone().merged(joined.links);
        if let Some(url) = &links.update_endpoint_state
            && let Err(error) = self.api.preheat_done(url).await
        {
            log::info!("Teams meeting: the preheat not ended: {error:?}");
        }
        self.conversation = Some(CpconvAnswer {
            conversation_controller: preheated.conversation_controller,
            links,
            meeting_data: None,
        });
        log::info!("Teams meeting: joined, waiting for its answer");
        let ended = self
            .talk(&mut session, &mut local, inbox, control, tell)
            .await;
        session.stop();
        *counts = session.counts();
        ended
    }

    /// Whether we wait in a meeting's lobby.
    fn in_lobby(&self) -> bool {
        self.meeting.as_ref().is_some_and(|m| m.lobby)
    }

    async fn ring_and_talk(
        mut self,
        callee: &str,
        session: &mut MediaSession,
        local: &mut LocalMedia,
        inbox: &mut mpsc::UnboundedReceiver<Push>,
        control: &mut mpsc::UnboundedReceiver<Control>,
        tell: &(impl Fn(CallEvent) + Send),
    ) -> Result<(), Failure> {
        self.callee = callee.to_owned();
        self.leg = self.api.ids().media_leg_id.clone();
        let offer = sdp::offer(local);
        self.conversation = Some(self.api.create_call(callee, &offer).await?);
        log::info!("Teams call: placed, ringing");
        tell(CallEvent::Ringing);
        self.talk(session, local, inbox, control, tell).await
    }

    /// Runs the call until it ends: rings until picked up (for a call we
    /// placed), then talks.
    async fn talk(
        &mut self,
        session: &mut MediaSession,
        local: &mut LocalMedia,
        inbox: &mut mpsc::UnboundedReceiver<Push>,
        control: &mut mpsc::UnboundedReceiver<Control>,
        tell: &(impl Fn(CallEvent) + Send),
    ) -> Result<(), Failure> {
        let ringing = tokio::time::sleep(RING_FOR);
        tokio::pin!(ringing);
        loop {
            tokio::select! {
                push = inbox.recv() => {
                    let Some(push) = push else {
                        return Err(Failure::CallFailed("the call's pushes stopped".into()));
                    };
                    if let Some(ended) = self.on_push(push, session, local, tell).await {
                        return ended;
                    }
                }
                event = session.next_event() => match event {
                    Some(MediaEvent::Connected) => {
                        log::info!("Teams call: media connected");
                        self.connected = true;
                        if self.accepted && !self.in_lobby() {
                            tell(CallEvent::Live);
                        }
                    }
                    Some(MediaEvent::FarVideo(on)) => tell(CallEvent::FarEndVideo(on)),
                    Some(MediaEvent::FarShare(on)) => tell(CallEvent::FarEndShare(on)),
                    Some(MediaEvent::AudioFlowing) => {
                        log::info!("Teams call: audio flows both ways");
                        tell(CallEvent::AudioFlowing);
                    }
                    Some(MediaEvent::Failed(failure)) => {
                        self.leave().await;
                        return Err(media_failure(failure));
                    }
                    Some(MediaEvent::Stopped) | None => return Ok(()),
                },
                asked = control.recv() => match asked {
                    Some(Control::Mute(muted)) => self.mute(session, muted).await,
                    Some(Control::Share(on)) => {
                        self.share.wanted = on;
                        self.share.retried = false;
                        self.offer_share(local).await;
                    }
                    Some(Control::Admit(mri)) => self.admit(&mri).await,
                    Some(Control::Camera(on)) => self.camera(on).await,
                    // Asked to stop, or whoever steered the call is gone.
                    Some(Control::HangUp) | None => {
                        session.stop();
                        self.leave().await;
                        return Ok(());
                    }
                },
                () = keep_alive_due(self.keep_alive.as_ref()) => self.send_keep_alive().await,
                () = &mut ringing, if !self.accepted => {
                    log::info!("Teams call: nobody answered");
                    session.stop();
                    self.leave().await;
                    return Err(Failure::CallNotAnswered);
                }
            }
        }
    }

    /// Attaches to an incoming call and rings until the person answers
    /// or the caller gives up; picked up, talks until the end.
    #[expect(
        clippy::too_many_arguments,
        reason = "the call's parts, as `run` has them"
    )]
    async fn ring_here(
        &mut self,
        client: &TeamsClient,
        call: Incoming,
        mut answer: oneshot::Receiver<Answer>,
        inbox: &mut mpsc::UnboundedReceiver<Push>,
        control: &mut mpsc::UnboundedReceiver<Control>,
        tell: &(impl Fn(CallEvent) + Send),
        counts: &mut media::Counts,
    ) -> Result<(), Failure> {
        let attached = self.api.attach(&call.attach, &call.conversation).await?;
        let invitation = attached.call_invitation.clone().unwrap_or_default();
        self.conversation = Some(CpconvAnswer {
            conversation_controller: call.conversation.clone(),
            links: attached.conversation().cloned().unwrap_or_default(),
            meeting_data: None,
        });
        if let Some(url) = &invitation.links.progress
            && let Err(error) = self.api.ringing(url).await
        {
            log::info!("Teams call: ringing not told to the caller: {error:?}");
        }
        log::info!("Teams call: an incoming call rings");
        tell(CallEvent::Ringing);

        let ringing = tokio::time::sleep(RINGS_HERE_FOR);
        tokio::pin!(ringing);
        let audio = loop {
            tokio::select! {
                decided = &mut answer => match decided {
                    Ok(Answer::Accept(audio)) => break audio,
                    // Declined, or whoever would answer is gone.
                    Ok(Answer::Decline) | Err(_) => {
                        self.decline(&invitation).await;
                        return Ok(());
                    }
                },
                push = inbox.recv() => match push {
                    Some(Push::CallEnd(end)) => {
                        if answered_elsewhere(&end) {
                            log::info!("Teams call: picked up elsewhere ({}, {})", end.code, end.phrase);
                            tell(CallEvent::AnsweredElsewhere);
                        } else {
                            log::info!("Teams call: the caller gave up ({}, {})", end.code, end.phrase);
                        }
                        return Ok(());
                    }
                    Some(Push::ConversationEnd(_)) => return Ok(()),
                    Some(_) => {}
                    None => return Err(Failure::CallFailed("the call's pushes stopped".into())),
                },
                asked = control.recv() => {
                    if matches!(asked, Some(Control::HangUp) | None) {
                        self.decline(&invitation).await;
                        return Ok(());
                    }
                }
                () = &mut ringing => {
                    log::info!("Teams call: nobody picked up here");
                    return Ok(());
                }
            }
        };

        log::info!("Teams call: picking up");
        let remote = sdp::read(&call.offer.blob).map_err(|error| {
            log::warn!("Teams call: unreadable SDP from the caller: {error}");
            Failure::CallFailed("the caller's media could not be used".into())
        })?;
        let relay = relay_for(client).await;
        self.relay.clone_from(&relay);
        let (mut session, mut local) =
            MediaSession::start(MediaConfig::answer(relay, &remote), audio)
                .await
                .map_err(media_failure)?;
        let ended = self
            .pick_up(
                &call,
                &invitation,
                &remote,
                &mut session,
                &mut local,
                inbox,
                control,
                tell,
            )
            .await;
        session.stop();
        *counts = session.counts();
        ended
    }

    /// Answers the caller's offer and talks until the end.
    #[expect(
        clippy::too_many_arguments,
        reason = "the call's parts, as `run` has them"
    )]
    async fn pick_up(
        &mut self,
        call: &Incoming,
        invitation: &IncomingInvitation,
        remote: &RemoteMedia,
        session: &mut MediaSession,
        local: &mut LocalMedia,
        inbox: &mut mpsc::UnboundedReceiver<Push>,
        control: &mut mpsc::UnboundedReceiver<Control>,
        tell: &(impl Fn(CallEvent) + Send),
    ) -> Result<(), Failure> {
        session.apply_remote(remote).map_err(media_failure)?;
        self.last_remote = Some(remote.clone());
        self.leg = call.offer.media_leg_id.clone();
        let answer = sdp::answer(local, remote);
        // As the web client does before picking up: not muted.
        if let Some(url) = self
            .conversation
            .as_ref()
            .and_then(|c| c.links.update_endpoint_state.clone())
            && let Err(error) = self.api.update_endpoint_state(&url, false).await
        {
            log::info!("Teams call: mute state not told: {error:?}");
        }
        let url = invitation
            .links
            .acceptance
            .as_deref()
            .ok_or_else(|| Failure::CallFailed("the call cannot be picked up".into()))?;
        let acknowledged = self
            .api
            .accept(url, &answer, &call.offer.media_leg_id)
            .await?;
        log::info!("Teams call: picked up here");
        self.renegotiation = acknowledged.links.media_renegotiation.clone();
        self.accepted = true;
        if let (Some(call_leg), Some(interval)) = (
            acknowledged.links.call_leg.clone(),
            acknowledged.call_keep_alive_interval,
        ) {
            let every = keep_alive_every(interval);
            self.keep_alive = Some(KeepAlive {
                call_leg,
                every,
                next: tokio::time::Instant::now() + every,
            });
        }
        if let Some(url) = self
            .conversation
            .as_ref()
            .and_then(|c| c.links.update_endpoint_metadata.clone())
            && let Err(error) = self.api.update_endpoint_metadata(&url).await
        {
            log::info!("Teams call: endpoint metadata not taken: {error:?}");
        }
        self.talk(session, local, inbox, control, tell).await
    }

    /// Declines an incoming call, without waiting long.
    async fn decline(&self, invitation: &IncomingInvitation) {
        let Some(url) = invitation.links.decline() else {
            return;
        };
        match tokio::time::timeout(LEAVE_WITHIN, self.api.decline(url)).await {
            Ok(Ok(())) => log::info!("Teams call: declined"),
            Ok(Err(error)) => log::info!("Teams call: decline refused: {error:?}"),
            Err(_) => log::info!("Teams call: decline took too long"),
        }
    }

    /// Acts on one push; answers the call's end when it is over.
    async fn on_push(
        &mut self,
        push: Push,
        session: &mut MediaSession,
        local: &mut LocalMedia,
        tell: &(impl Fn(CallEvent) + Send),
    ) -> Option<Result<(), Failure>> {
        match push {
            Push::MediaAnswer(answer) => {
                log::info!("Teams call: the far end answered the offer");
                if let Some(remote) = apply(session, &answer.media_content.blob) {
                    self.last_remote = Some(remote);
                }
                // The answer to a renegotiation of ours: acknowledged, and
                // the next one sent if the wish changed meanwhile.
                if let Some(offered) = self.share.offered.take() {
                    self.share.agreed = offered;
                    if let Some(url) = &answer.links.media_acknowledgement
                        && let Err(error) = self.api.acknowledge_answer(url).await
                    {
                        log::info!("Teams call: answer not acknowledged: {error:?}");
                    }
                    log::info!(
                        "Teams call: our screen share is {}",
                        if offered { "on" } else { "off" }
                    );
                    self.offer_share(local).await;
                }
            }
            Push::Acceptance(acceptance) => {
                let lobby = acceptance.is_lobby();
                if let Some(meeting) = &mut self.meeting {
                    meeting.lobby = lobby;
                    meeting.take_links(&acceptance.links);
                }
                log::info!(
                    "Teams call: {}",
                    if lobby {
                        "in the meeting's lobby"
                    } else {
                        "picked up"
                    }
                );
                self.accepted = true;
                if let Some(url) = &acceptance.links.acknowledgement
                    && let Err(error) = self.api.acknowledge_acceptance(url).await
                {
                    log::warn!("Teams call: the pickup was not acknowledged: {error:?}");
                }
                if let (Some(call_leg), Some(interval)) = (
                    acceptance.links.call_leg.clone(),
                    acceptance.call_keep_alive_interval,
                ) {
                    let every = keep_alive_every(interval);
                    self.keep_alive = Some(KeepAlive {
                        call_leg,
                        every,
                        next: tokio::time::Instant::now() + every,
                    });
                }
                if let Some(content) = &acceptance.media_content
                    && let Some(remote) = apply(session, &content.blob)
                {
                    self.last_remote = Some(remote);
                }
                self.renegotiation = acceptance.links.media_renegotiation.clone();
                if lobby {
                    tell(CallEvent::Lobby);
                } else if self.connected {
                    tell(CallEvent::Live);
                }
                if let Some(url) = self
                    .conversation
                    .as_ref()
                    .and_then(|c| c.links.update_endpoint_metadata.clone())
                    && let Err(error) = self.api.update_endpoint_metadata(&url).await
                {
                    log::info!("Teams call: endpoint metadata not taken: {error:?}");
                }
            }
            Push::MediaNegotiation(negotiation) => {
                // An offer from another media server (a meeting letting
                // us in from its lobby) needs a session of its own.
                if let Ok(remote) = sdp::read(&negotiation.media_content.blob)
                    && moves(self.last_remote.as_ref(), &remote)
                {
                    return self
                        .move_media(&negotiation, &remote, session, local, tell)
                        .await
                        .err()
                        .map(Err);
                }
                // Unusable offers are logged by `apply`; the call goes on.
                let remote = apply(session, &negotiation.media_content.blob)?;
                self.last_remote = Some(remote.clone());
                let Some(url) = negotiation.links.media_answer.as_deref() else {
                    log::warn!("Teams call: a renegotiation without an answer link");
                    return None;
                };
                local.session_version += 1;
                let answer = sdp::answer(local, &remote);
                let leg = negotiation.media_content.media_leg_id.clone();
                let descriptions = self.next_descriptions(&answer);
                match self
                    .api
                    .answer_renegotiation(url, &answer, &leg, descriptions)
                    .await
                {
                    Ok(()) => log::info!("Teams call: renegotiation answered"),
                    Err(error) => log::warn!("Teams call: renegotiation not answered: {error:?}"),
                }
                // Microsoft answers the start of our share and then offers
                // again at once, raising its limits (recorded): answered
                // above, sending. Crossing an offer of ours, this one is
                // answered all the same; ours, if refused, says so on its
                // rejection link.
            }
            Push::MediaAcknowledgement(ack, links) => {
                if let Err(error) = codes::acknowledgement(&ack) {
                    log::warn!("Teams call: an answer of ours was refused: {error:?}");
                }
                // Moved to another media server: its leg from now on.
                if let Some(meeting) = &mut self.meeting {
                    meeting.take_links(&links);
                }
                if let Some(url) = links.media_renegotiation {
                    log::info!("Teams call: on a new call leg");
                    self.renegotiation = Some(url);
                }
                if let (Some(call_leg), Some(keep_alive)) = (links.call_leg, &mut self.keep_alive) {
                    keep_alive.call_leg = call_leg;
                }
            }
            Push::ConversationUpdate(update) => {
                if self.in_lobby() && update.in_call() {
                    log::info!("Teams meeting: let in from the lobby");
                }
                if let Some(conversation) = &mut self.conversation {
                    conversation.links =
                        std::mem::take(&mut conversation.links).merged(update.links);
                }
            }
            Push::CallEnd(end) => {
                log::info!("Teams call: ended by Teams ({}, {})", end.code, end.phrase);
                return Some(match codes::call_end(&end) {
                    Ending::Normal => Ok(()),
                    Ending::Failed(failure) => Err(failure),
                });
            }
            Push::ConversationEnd(end) => {
                return Some(match codes::conversation_end(&end) {
                    Ending::Normal => Ok(()),
                    Ending::Failed(failure) => Err(failure),
                });
            }
            Push::RosterUpdate(update) if self.meeting.is_some() => {
                if let Some(meeting) = &mut self.meeting
                    && meeting.people.take(&update, &meeting.me)
                {
                    let people = meeting.people.list();
                    let waiting = people.iter().filter(|p| p.waiting).count();
                    log::info!(
                        "Teams meeting: {} others in, {waiting} waiting",
                        people.len() - waiting
                    );
                    tell(CallEvent::People(people));
                }
            }
            Push::RosterUpdate(update) => {
                if let Some(muted) = self.far_end_change(&update) {
                    log::info!(
                        "Teams call: the far end is {}",
                        if muted { "muted" } else { "unmuted" }
                    );
                    tell(CallEvent::FarEndMuted(muted));
                }
            }
            Push::Other(name) if name == "rejection" || name == "mediaNegotiationFailure" => {
                log::info!("Teams call: our renegotiation was refused ({name})");
                if self.share.offered.take().is_some() && !self.share.retried {
                    self.share.retried = true;
                    self.offer_share(local).await;
                }
            }
            Push::Other(name) => log::debug!("Teams call: push {name} not acted on"),
        }
        None
    }

    /// Whether the far end's mute changed with `update`: answers the new
    /// state.
    fn far_end_change(&mut self, update: &RosterUpdate) -> Option<bool> {
        let was = self.far_end.map(|(_, muted)| muted);
        self.far_end = Some(far_end(update, &self.callee, self.far_end)?);
        let muted = self.far_end.is_some_and(|(_, muted)| muted);
        // Unmuted is what the bar shows until told otherwise.
        (was.unwrap_or(false) != muted).then_some(muted)
    }

    /// Says the call leg is still here, and when to say it next.
    async fn send_keep_alive(&mut self) {
        let Some(keep_alive) = self.keep_alive.as_mut() else {
            return;
        };
        keep_alive.next = tokio::time::Instant::now() + keep_alive.every;
        let call_leg = keep_alive.call_leg.clone();
        if let Err(error) = self.api.keep_alive(&call_leg).await {
            log::warn!("Teams call: keep-alive refused: {error:?}");
        }
    }

    /// Offers the screen share as wished, if that is not what was last
    /// agreed and no offer of ours waits for its answer: our own offer in
    /// the shape of the far end's latest description.
    async fn offer_share(&mut self, local: &mut LocalMedia) {
        if self.share.offered.is_some() || self.share.wanted == self.share.agreed {
            return;
        }
        let (Some(url), Some(last)) = (self.renegotiation.clone(), self.last_remote.as_ref())
        else {
            log::warn!("Teams call: no way to renegotiate the screen share yet");
            return;
        };
        if self.share.wanted {
            self.share.number = if self.share.number == 0 {
                1
            } else {
                self.share.number + 2
            };
        }
        local.sharing = self.share.wanted;
        local.session_version += 1;
        let offer = sdp::reoffer(local, last);
        match self
            .api
            .renegotiate(&url, &offer, &self.leg, self.share.number)
            .await
        {
            Ok(()) => {
                log::info!(
                    "Teams call: asked to {} sharing our screen",
                    if self.share.wanted { "start" } else { "stop" }
                );
                self.share.offered = Some(self.share.wanted);
            }
            Err(error) => {
                log::warn!("Teams call: the screen share was not renegotiated: {error:?}")
            }
        }
    }

    async fn mute(&mut self, session: &MediaSession, muted: bool) {
        self.muted = muted;
        session.set_muted(muted);
        if let Some(url) = self
            .conversation
            .as_ref()
            .and_then(|c| c.links.update_endpoint_state.clone())
            && let Err(error) = self.api.update_endpoint_state(&url, muted).await
        {
            log::info!("Teams call: mute not told to the far end: {error:?}");
        }
    }

    /// Moves the call to the media server whose `remote` offer came
    /// with `negotiation`: the old session hands its sound and pictures
    /// to a new one, which answers the offer (§H.4: the web client, let
    /// in from a meeting's lobby, builds a new transport too). Fails the
    /// call when the new session cannot start.
    async fn move_media(
        &mut self,
        negotiation: &MediaNegotiation,
        remote: &RemoteMedia,
        session: &mut MediaSession,
        local: &mut LocalMedia,
        tell: &(impl Fn(CallEvent) + Send),
    ) -> Result<(), Failure> {
        log::info!(
            "Teams call: moving to another media server: {}",
            remote.summary()
        );
        let Some(url) = negotiation.links.media_answer.as_deref() else {
            log::warn!("Teams call: a new media server's offer without an answer link");
            return Ok(());
        };
        // Without them (the session had ended) the call goes on silent.
        let held = session.release().await.unwrap_or_default();
        let started =
            MediaSession::resume(MediaConfig::answer(self.relay.clone(), remote), held).await;
        let (fresh, fresh_local) = match started {
            Ok(started) => started,
            Err(failure) => {
                self.leave().await;
                return Err(media_failure(failure));
            }
        };
        *session = fresh;
        *local = fresh_local;
        session.set_muted(self.muted);
        if let Err(failure) = session.apply_remote(remote) {
            self.leave().await;
            return Err(media_failure(failure));
        }
        self.connected = false;
        self.last_remote = Some(remote.clone());
        self.leg = negotiation.media_content.media_leg_id.clone();
        // Whatever was shared went with the old session.
        self.share.agreed = false;
        self.share.offered = None;
        if let Some(meeting) = &mut self.meeting
            && std::mem::take(&mut meeting.lobby)
        {
            log::info!("Teams meeting: in the call");
            tell(CallEvent::Admitted);
        }
        let answer = sdp::answer(local, remote);
        let descriptions = self.next_descriptions(&answer);
        match self
            .api
            .answer_renegotiation(url, &answer, &self.leg, descriptions)
            .await
        {
            Ok(()) => log::info!("Teams call: the new media server's offer answered"),
            Err(error) => {
                log::warn!("Teams call: the new media server's offer not answered: {error:?}");
            }
        }
        Ok(())
    }

    /// The next `mediaDescriptions` of a meeting, for our SDP `sdp`;
    /// none outside a meeting.
    fn next_descriptions(&mut self, sdp: &str) -> Option<serde_json::Value> {
        let meeting = self.meeting.as_mut()?;
        meeting.request += 1;
        Some(super::api::descriptions_for(
            sdp,
            meeting.camera_on,
            meeting.request,
        ))
    }

    /// Tells a meeting our camera went on or off (recorded: what it can
    /// send first, then the camera's line sending and receiving).
    async fn camera(&mut self, on: bool) {
        let camera = self
            .last_remote
            .as_ref()
            .and_then(|r| r.camera())
            .map(|l| l.mid.clone());
        let share = self
            .last_remote
            .as_ref()
            .and_then(|r| r.share())
            .map(|l| l.mid.clone());
        let Some(meeting) = self.meeting.as_mut().filter(|m| m.camera_on != on) else {
            return;
        };
        let Some(mid) = camera else {
            log::info!("Teams meeting: no camera line to turn on");
            return;
        };
        meeting.camera_on = on;
        meeting.request += 1;
        meeting.changes += 1;
        let descriptions =
            super::api::media_descriptions(Some(&mid), on, share.as_deref(), meeting.request);
        let number = meeting.changes;
        let (update, capabilities) = (
            meeting.update_descriptions.clone(),
            meeting.camera_capabilities.clone(),
        );
        if on
            && let Some(url) = capabilities
            && let Err(error) = self.api.camera_capabilities(&url, &mid).await
        {
            log::info!("Teams meeting: the camera's capabilities not taken: {error:?}");
        }
        let Some(url) = update else {
            log::info!("Teams meeting: no way to say the camera is on");
            return;
        };
        match self
            .api
            .update_media_descriptions(&url, descriptions, number)
            .await
        {
            Ok(()) => log::info!(
                "Teams meeting: told our camera is {}",
                if on { "on" } else { "off" }
            ),
            Err(error) => log::warn!("Teams meeting: the camera's change not taken: {error:?}"),
        }
    }

    /// Lets `user` (see [`Control::Admit`]) in from the meeting's lobby.
    async fn admit(&self, user: &str) {
        let mri = self
            .meeting
            .as_ref()
            .and_then(|m| m.people.mri_of(user))
            .unwrap_or_else(|| crate::teams::client::user_mri(user));
        let mri = mri.as_str();
        let Some(url) = self
            .conversation
            .as_ref()
            .and_then(|c| c.links.admit.clone())
        else {
            log::info!("Teams meeting: no way to let anyone in");
            return;
        };
        match self.api.admit(&url, mri).await {
            Ok(()) => log::info!("Teams meeting: asked to let someone in"),
            Err(error) => log::warn!("Teams meeting: could not let someone in: {error:?}"),
        }
    }

    /// Leaves the call, without waiting long: the call is over here
    /// whatever Teams answers.
    async fn leave(&self) {
        let Some(url) = self
            .conversation
            .as_ref()
            .and_then(|c| c.links.leave.clone())
        else {
            return;
        };
        let connected = self.accepted;
        match tokio::time::timeout(LEAVE_WITHIN, self.api.hang_up(&url, connected)).await {
            Ok(Ok(())) => log::info!("Teams call: left"),
            Ok(Err(error)) => log::info!("Teams call: leave refused: {error:?}"),
            Err(_) => log::info!("Teams call: leave took too long"),
        }
    }
}

/// Whether the far end's `remote` description comes from another media
/// server than `last`: a DTLS certificate of its own, which takes a new
/// session (str0m keeps one per peer).
fn moves(last: Option<&RemoteMedia>, remote: &RemoteMedia) -> bool {
    last.is_some_and(|last| last.fingerprint.is_some() && last.fingerprint != remote.fingerprint)
}

/// Reads the far end's SDP and hands it to the media; answers what was
/// read, or nothing when it could not be used (logged, the call goes on).
fn apply(session: &MediaSession, blob: &str) -> Option<RemoteMedia> {
    let remote = match sdp::read(blob) {
        Ok(remote) => remote,
        Err(error) => {
            log::warn!("Teams call: unreadable SDP from the far end: {error}");
            return None;
        }
    };
    log::info!("Teams call: the far end's media: {}", remote.summary());
    if let Err(error) = session.apply_remote(&remote) {
        log::warn!("Teams call: the far end's media refused: {error}");
        return None;
    }
    Some(remote)
}

/// Whether a ringing call ended because it was picked up elsewhere:
/// another device says so in `acceptedElsewhereBy`; a second delivery of
/// the call to this same connection (it is registered for chat and for
/// calls) only says so in words.
fn answered_elsewhere(end: &super::types::Outcome) -> bool {
    end.accepted_elsewhere_by.is_some() || end.phrase.contains("accepted by another")
}

/// The interface's failure for the media's: its stage, never its detail
/// (which may name addresses).
fn media_failure(failure: media::Failure) -> Failure {
    log::warn!("Teams call media failed: {failure}");
    Failure::CallFailed(match failure.stage {
        media::Stage::Gather => "no network path could be found".into(),
        media::Stage::Remote => "the far end's media could not be used".into(),
        media::Stage::Connect => "the media did not connect".into(),
        media::Stage::Media => "the connection broke".into(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn muted_roster() -> RosterUpdate {
        serde_json::from_str(include_str!("fixtures/roster_update_muted.json")).expect("reads")
    }

    #[test]
    fn the_roster_says_whether_the_far_end_is_muted() {
        let update = muted_roster();
        assert_eq!(far_end(&update, "8:live:me", None), Some((4, true)));
        assert_eq!(far_end(&update, "8:LIVE:ME", None), Some((4, true)));
        // Someone the delta leaves out, and an older version, say nothing.
        assert_eq!(far_end(&update, "8:live:ana", None), None);
        assert_eq!(far_end(&update, "8:live:me", Some((5, false))), None);
    }

    #[test]
    fn a_call_notification_reads_as_an_incoming_call() {
        let note: IncomingNotification =
            serde_json::from_str(include_str!("fixtures/call_notification.json")).expect("reads");
        let call = Incoming::read(&note).expect("a 1:1 call");
        assert!(!call.call_id.is_empty());
        assert!(call.caller.id.starts_with("8:"));
        assert!(call.offer.blob.starts_with("v=0"));
        // Its links stay out of the log.
        assert!(!format!("{call:?}").contains("http"));
    }

    #[test]
    fn a_call_picked_up_elsewhere_is_not_missed() {
        use super::super::types::Outcome;
        // A second delivery of the call to this connection, as logged.
        let fork = Outcome {
            code: 487,
            phrase: "Call cancelled as it was accepted by another fork.".into(),
            ..Outcome::default()
        };
        assert!(answered_elsewhere(&fork));
        // Another device of yours.
        let device = Outcome {
            accepted_elsewhere_by: Some(Participant::default()),
            ..Outcome::default()
        };
        assert!(answered_elsewhere(&device));
        // The caller gave up.
        let gave_up = Outcome {
            code: 487,
            phrase: "CallEndReasonLocalUserInitiated".into(),
            ..Outcome::default()
        };
        assert!(!answered_elsewhere(&gave_up));
    }

    #[test]
    fn the_keep_alive_goes_a_little_before_it_runs_out() {
        assert_eq!(keep_alive_every(2700), Duration::from_secs(2430));
        assert!(keep_alive_every(0) > Duration::ZERO);
    }

    #[test]
    fn a_new_media_servers_certificate_moves_the_call() {
        let read = |text: &str| sdp::read(text).expect("reads");
        let lobby = read(include_str!("fixtures/meeting_lobby.sdp"));
        let meeting = read(include_str!("fixtures/meeting_retarget.sdp"));
        let confirm = read(include_str!("fixtures/meeting_confirm.sdp"));
        assert!(moves(Some(&lobby), &meeting));
        // The same server again, in BUNDLE: a renegotiation like any.
        assert!(!moves(Some(&meeting), &confirm));
        assert!(!moves(None, &meeting));
    }

    #[test]
    fn a_meetings_roster_keeps_who_is_in_and_who_waits() {
        let update: RosterUpdate =
            serde_json::from_str(include_str!("fixtures/roster_lobby.json")).expect("reads");
        let mut people = People::default();
        assert!(people.take(&update, "8:live:ORGANIZER"));
        // You are not among the others; the one gone is not shown.
        let shown = people.list();
        assert_eq!(shown.len(), 1);
        assert_eq!(shown[0].mri, "8:live:waiting");
        assert!(shown[0].waiting);
        assert_eq!(shown[0].name.as_deref(), Some("Wim Waiting"));
        // The same again changes nothing; an older version is ignored.
        assert!(!people.take(&update, "8:live:organizer"));
        let mut older = update.clone();
        if let Some(them) = older.participants.get_mut("8:live:waiting") {
            them.version = 1;
            them.endpoints.clear();
        }
        assert!(!people.take(&older, "8:live:organizer"));
        // Let in: in the call, no longer waiting.
        let mut admitted = update;
        if let Some(them) = admitted.participants.get_mut("8:live:waiting") {
            them.version = 6;
            for endpoint in them.endpoints.values_mut() {
                endpoint.call = endpoint.lobby.take();
            }
        }
        assert!(people.take(&admitted, "8:live:organizer"));
        assert!(!people.list()[0].waiting);
    }

    #[test]
    fn a_guest_is_admitted_by_the_mri_the_roster_gave() {
        let mut update: RosterUpdate =
            serde_json::from_str(include_str!("fixtures/roster_lobby.json")).expect("reads");
        let waiting = update
            .participants
            .remove("8:live:waiting")
            .expect("in the fixture");
        update
            .participants
            .insert("8:teamsvisitor:a1b2c3".into(), waiting);
        let mut people = People::default();
        people.take(&update, "8:live:organizer");
        // The interface knows them by the MRI's last part.
        assert_eq!(
            people.mri_of("a1b2c3").as_deref(),
            Some("8:teamsvisitor:a1b2c3")
        );
        assert_eq!(
            people.mri_of("8:teamsvisitor:a1b2c3").as_deref(),
            Some("8:teamsvisitor:a1b2c3")
        );
        assert_eq!(people.mri_of("someone"), None);
    }
}
