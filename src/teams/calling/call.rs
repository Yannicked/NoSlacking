//! An outgoing 1:1 Teams call, from ringing to its end: the signalling of
//! `docs/research/teams-calls.md` §A and §C driving a [`MediaSession`].
//!
//! [`outgoing`] runs the whole call on its task. What happens is told
//! through a callback as [`CallEvent`]s, and the caller steers it with
//! [`Control`]s (mute, hang up). It never waits on the network to stop:
//! hanging up closes the media at once and sends `leave` on the way out.

use std::time::Duration;

use tokio::sync::mpsc;

use super::api::{CallApi, CallIds, own_participant, relay_credentials, relay_servers};
use super::codes::{self, Ending};
use super::media::{self, Audio, MediaConfig, MediaEvent, MediaSession, Relay};
use super::types::{CpconvAnswer, Push};
use super::{LocalMedia, RemoteMedia, sdp};
use crate::failure::Failure;
use crate::teams::client::TeamsClient;

/// How long an unanswered call rings before we give up.
const RING_FOR: Duration = Duration::from_secs(60);
/// How long a hang-up's `leave` may take before the call is ended anyway.
const LEAVE_WITHIN: Duration = Duration::from_secs(5);

/// What the caller asks of a running call.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Control {
    /// Mute (or unmute) the microphone, and tell the far end.
    Mute(bool),
    /// End the call.
    HangUp,
}

/// What a call tells as it goes; the last is always [`CallEvent::Ended`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CallEvent {
    /// The call was placed: it rings at the far end.
    Ringing,
    /// Picked up, and the media is up: the call can be heard.
    Live,
    /// The media is connected and audio flows both ways.
    AudioFlowing,
    /// The call is over: normally (`Ok`) or because something failed.
    Ended {
        result: Result<(), Failure>,
        /// What went over the wire, for a probe's summary.
        counts: media::Counts,
    },
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

    // The relay is how a far end behind another network reaches us; a
    // call on the same network still works without it.
    let relay = match relay_credentials(client).await {
        Ok(credentials) => Some(Relay::new(&relay_servers(client).await, credentials)),
        Err(error) => {
            log::warn!("no Teams relay for the call: {error:?}");
            None
        }
    };
    let (mut session, mut local) = MediaSession::start(MediaConfig::offer(relay), audio)
        .await
        .map_err(media_failure)?;
    log::info!(
        "Teams call: media ready with {} candidates",
        local.candidates.len()
    );

    let mut inbox = client.calls().register(&ids.call_agent_id);
    let agent = ids.call_agent_id.clone();
    let call = Call::new(client, ids, me, &surl);
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

/// Waits until the keep-alive is due; never while there is none.
async fn keep_alive_due(keep_alive: Option<&KeepAlive>) {
    match keep_alive {
        Some(keep_alive) => tokio::time::sleep_until(keep_alive.next).await,
        None => std::future::pending().await,
    }
}

impl Call {
    fn new(client: &TeamsClient, ids: CallIds, me: super::types::Participant, surl: &str) -> Self {
        Self {
            api: CallApi::new(client.clone(), ids, me, surl),
            conversation: None,
            accepted: false,
            connected: false,
            keep_alive: None,
        }
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
        let offer = sdp::offer(local);
        self.conversation = Some(self.api.create_call(callee, &offer).await?);
        log::info!("Teams call: placed, ringing");
        tell(CallEvent::Ringing);

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
                        if self.accepted {
                            tell(CallEvent::Live);
                        }
                    }
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

    /// Acts on one push; answers the call's end when it is over.
    async fn on_push(
        &mut self,
        push: Push,
        session: &MediaSession,
        local: &mut LocalMedia,
        tell: &(impl Fn(CallEvent) + Send),
    ) -> Option<Result<(), Failure>> {
        match push {
            Push::MediaAnswer(answer) => {
                log::info!("Teams call: the far end answered the offer");
                apply(session, &answer.media_content.blob);
            }
            Push::Acceptance(acceptance) => {
                log::info!("Teams call: picked up");
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
                if let Some(content) = &acceptance.media_content {
                    apply(session, &content.blob);
                }
                if self.connected {
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
                // Unusable offers are logged by `apply`; the call goes on.
                let remote = apply(session, &negotiation.media_content.blob)?;
                let Some(url) = negotiation.links.media_answer.as_deref() else {
                    log::warn!("Teams call: a renegotiation without an answer link");
                    return None;
                };
                local.session_version += 1;
                let answer = sdp::answer(local, &remote);
                let leg = negotiation.media_content.media_leg_id.clone();
                match self.api.answer_renegotiation(url, &answer, &leg).await {
                    Ok(()) => log::info!("Teams call: renegotiation answered"),
                    Err(error) => log::warn!("Teams call: renegotiation not answered: {error:?}"),
                }
            }
            Push::MediaAcknowledgement(ack) => {
                if let Err(error) = codes::acknowledgement(&ack) {
                    log::warn!("Teams call: an answer of ours was refused: {error:?}");
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
            Push::RosterUpdate(_) => {}
            Push::Other(name) => log::debug!("Teams call: push {name} not acted on"),
        }
        None
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

    async fn mute(&self, session: &MediaSession, muted: bool) {
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
    if let Err(error) = session.apply_remote(&remote) {
        log::warn!("Teams call: the far end's media refused: {error}");
        return None;
    }
    Some(remote)
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

    #[test]
    fn the_keep_alive_goes_a_little_before_it_runs_out() {
        assert_eq!(keep_alive_every(2700), Duration::from_secs(2430));
        assert!(keep_alive_every(0) > Duration::ZERO);
    }
}
