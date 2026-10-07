//! The worker's side of sharing your screen (the `huddle-share` feature):
//! the share in the video helper, its sending thread and the share's own
//! session, started when the interface asks and stopped when it asks,
//! when the huddle is left, when the share's session fails or Chime
//! refuses it, or when the capture ends by itself.
//!
//! The helper (`noslacking-video`, its screen lane) does everything that
//! touches the screen: it says what can be shared (nothing to list where
//! the system has its own dialog), shows that dialog, captures and
//! encodes. No helper, no sharing: the interface says so.
//!
//! A share is a second Chime attendee, as in the JS SDK's
//! `DefaultContentShareController`: the same meeting, `attendeeId#content`
//! and `joinToken#content` ([`ChimeJoin::content`]), its own signaling,
//! TURN relay, DTLS and SUBSCRIBE (a [`media::listen`] of its own) that
//! sends Opus silence (the JS SDK synthesizes a silent track when a share
//! has no sound) and the screen as H.264 on its send line, and receives
//! no video (the JS SDK gives it `NoVideoDownlinkBandwidthPolicy`).
//! NoSlacking says nothing to Slack itself about the share: Slack's own
//! shares were only ever seen as Chime's `#content` sources, and no
//! Slack call announcing one is known (HuddleFM shows none). The log
//! says so when a share starts, so a real run can tell.
//!
//! A helper that crashes while sharing ends the share with "the video
//! helper stopped" rather than starting it again on its own: a new
//! capture may need the system's dialog again, and a crash there could
//! repeat; pressing Share again starts a fresh helper (it is started
//! again a few times, as for decoding).

use std::sync::{Mutex, PoisonError};
use std::time::Duration;

use tokio::sync::{mpsc, oneshot, watch};

use crate::failure::{Failure, HuddleTrouble};
use crate::huddle_audio::camera_send::SendControl;
use crate::huddle_audio::helper::{self, Lane, RemoteShare, ShareTrouble};
use crate::huddle_audio::join::ChimeJoin;
use crate::huddle_audio::media::{self, Stage};
use crate::huddle_audio::share::{Source, may_share, problem_failure};
use crate::huddle_audio::share_send::{self, Encoding, Ending};
use crate::huddle_audio::video::Share;
use crate::huddle_audio::video_encoder::Limits;
use crate::huddle_share::{ShareNews, ShareRequest};
use noslacking_video_ipc::ShareChoice;

/// How long the share's session gets to leave (LEAVE, then LEAVE_ACK)
/// once told to stop.
const LEAVE_WAIT: Duration = Duration::from_secs(4);

/// The portal's restore token from the last share, for this run of the
/// app: sharing again shares the same without its dialog. Kept here
/// rather than in the helper so a restarted helper still has it.
static RESTORE: Mutex<String> = Mutex::new(String::new());

/// What a share the helper could not start or keep tells the interface.
pub fn start_failure(trouble: &ShareTrouble) -> Failure {
    match trouble {
        ShareTrouble::Problem(problem, _) => problem_failure(*problem),
        ShareTrouble::Lost(_) => Failure::Huddle(HuddleTrouble::ShareHelperLost),
    }
}

/// What the share's session ending by itself tells the interface: `None`
/// when it ended well (the huddle over). Chime's 206 ("view only") or
/// 509 ("at source capacity") is the meeting having all the shares it
/// takes; a join or setup refused is the server not taking it.
pub fn session_failure(
    refused: Option<u32>,
    result: &Result<(), media::Failure>,
) -> Option<Failure> {
    if matches!(refused, Some(206 | 509)) {
        return Some(Failure::Huddle(HuddleTrouble::ShareLimit));
    }
    let failure = result.as_ref().err()?;
    Some(Failure::Huddle(match failure.stage {
        Stage::Media => HuddleTrouble::ShareLost,
        Stage::Signaling | Stage::Join | Stage::Relay | Stage::Subscribe | Stage::Connect => {
            HuddleTrouble::ShareRefused
        }
    }))
}

/// A share running: its session, its sending thread, what ends it.
struct Live {
    halt: watch::Sender<bool>,
    session: tokio::task::JoinHandle<(media::Report, Result<(), media::Failure>)>,
    encoding: Option<Encoding>,
    /// Told when the share ends by itself (capture ended, helper failed).
    ended: watch::Receiver<Option<Ending>>,
    /// Told when Chime takes no video from the share.
    refusals: mpsc::Receiver<()>,
    /// Told once the share's connection is up.
    up: Option<oneshot::Receiver<()>>,
    /// The share is on for as long as it runs (the session's "camera").
    _on: watch::Sender<bool>,
}

/// What the interface asked, for the blocking thread that asks the
/// helper.
enum Begin {
    /// Share: the system's dialog, or the list for the call bar.
    Start {
        /// Choose afresh.
        again: bool,
    },
    /// Share this one of the listed sources.
    Pick(String),
}

/// What a start on the blocking thread came to.
enum Step {
    /// No dialog of the system's: these can be picked.
    Choose(Vec<Source>),
    /// The share started in the helper, or why not.
    Started(Result<RemoteShare, Failure>),
}

/// What a running share has to say.
enum LiveEvent {
    /// Its connection came up (or its session went before it did).
    Up(bool),
    /// It ended by itself.
    Ended(Ending),
    /// Chime takes no video from it.
    Refused,
    /// Its session ended by itself.
    Over(Box<Ended>),
}

/// How a share's session ended, or its task failing.
type Ended = Result<(media::Report, Result<(), media::Failure>), tokio::task::JoinError>;

/// The running share's next news, or never while none runs.
async fn live_event(live: &mut Option<Live>) -> LiveEvent {
    let Some(Live {
        up,
        ended,
        refusals,
        session,
        ..
    }) = live
    else {
        return std::future::pending().await;
    };
    tokio::select! {
        ok = async {
            match up.as_mut() {
                Some(up) => up.await.is_ok(),
                None => std::future::pending().await,
            }
        } => {
            *up = None;
            LiveEvent::Up(ok)
        }
        ending = async {
            loop {
                if let Some(ending) = ended.borrow_and_update().clone() {
                    return ending;
                }
                if ended.changed().await.is_err() {
                    return std::future::pending().await;
                }
            }
        } => LiveEvent::Ended(ending),
        () = async {
            if refusals.recv().await.is_none() {
                std::future::pending::<()>().await;
            }
        } => LiveEvent::Refused,
        over = session => LiveEvent::Over(Box::new(over)),
    }
}

/// A start under way finishing, or never.
async fn started(starting: &mut Option<tokio::task::JoinHandle<Step>>) -> Option<Step> {
    match starting {
        Some(job) => {
            let done = job.await.ok();
            *starting = None;
            done
        }
        None => std::future::pending().await,
    }
}

/// Asks the helper for what `begin` says, on a blocking thread: the
/// sources, the system's dialog, the capture all wait on another process
/// and on the user.
fn begin(begin: Begin) -> Step {
    let Some(helper) = helper::shared(Lane::Screen).filter(|h| !h.given_up()) else {
        log::warn!("huddle share: no video helper: no sharing");
        return Step::Started(Err(Failure::Huddle(HuddleTrouble::NoVideoHelper)));
    };
    let lost = |helper: &helper::Helper, trouble: ShareTrouble| {
        log::warn!("huddle share: {trouble:?}");
        if helper.given_up() && matches!(trouble, ShareTrouble::Lost(_)) {
            Step::Started(Err(Failure::Huddle(HuddleTrouble::NoVideoHelper)))
        } else {
            Step::Started(Err(start_failure(&trouble)))
        }
    };
    let choice = match begin {
        Begin::Pick(id) => ShareChoice::Source(id),
        Begin::Start { again } => match helper.sources() {
            Ok((false, sources)) if !sources.is_empty() => return Step::Choose(sources),
            Ok(_) => ShareChoice::System { again },
            Err(trouble) => return lost(&helper, trouble),
        },
    };
    let restore = RESTORE
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .clone();
    let bitrate = Limits::SHARE.bitrate(crate::huddle_audio::video_encoder::START_BITRATE);
    match helper.start_share(choice, helper::gpu(), bitrate, &restore) {
        Ok((share, token)) => {
            if !token.is_empty() {
                *RESTORE.lock().unwrap_or_else(PoisonError::into_inner) = token;
            }
            Step::Started(Ok(share))
        }
        Err(trouble) => lost(&helper, trouble),
    }
}

/// Starts the share's session and sending thread for a share the helper
/// started.
fn go_live(content: &ChimeJoin, share: RemoteShare) -> Result<Live, String> {
    let (encoded, encoded_in) = mpsc::channel(crate::huddle_audio::camera_send::QUEUE);
    let control = SendControl::new(Limits::SHARE);
    let (ending, ended) = watch::channel(None);
    let encoding = Encoding::spawn(share, encoded, control.clone(), ending)?;
    let (on, on_rx) = watch::channel(true);
    let (refused, refusals) = mpsc::channel(1);
    let uplink = share_send::uplink(encoded_in, on_rx, control, refused);
    let (halt, halted) = watch::channel(false);
    let (up, up_rx) = oneshot::channel();
    log::info!(
        "huddle share: joining the huddle as the content attendee ({}), as the JS SDK's \
         content share does; NoSlacking tells Slack nothing else about the share, so if \
         Slack does not show it, Slack may want its own announcement too",
        content.attendee_id
    );
    let content = content.clone();
    let session = tokio::spawn(async move {
        media::listen(
            &content,
            None,
            None,
            halted,
            Some(up),
            None,
            media::Video {
                options: None,
                viewer: None,
                camera: Some(uplink),
            },
        )
        .await
    });
    Ok(Live {
        halt,
        session,
        encoding: Some(encoding),
        ended,
        refusals,
        up: Some(up_rx),
        _on: on,
    })
}

/// Stops the sending thread, and with it the share in the helper, off
/// the async threads (it waits on the helper).
async fn stop_sending(encoding: Option<Encoding>) {
    let _ = tokio::task::spawn_blocking(move || drop(encoding)).await;
}

/// Stops a running share: its session leaves (waited for a moment), then
/// its sending thread and the capture.
async fn stop_live(live: Option<Live>) {
    let Some(mut live) = live else {
        return;
    };
    let _ = live.halt.send(true);
    match tokio::time::timeout(LEAVE_WAIT, &mut live.session).await {
        Ok(Ok((report, result))) => log_ending(&report, &result),
        Ok(Err(error)) => log::warn!("huddle share: the session's task failed: {error}"),
        Err(_) => {
            log::warn!("huddle share: the session did not leave in time");
            live.session.abort();
        }
    }
    stop_sending(live.encoding.take()).await;
}

fn log_ending(report: &media::Report, result: &Result<(), media::Failure>) {
    log::info!(
        "huddle share: the share's session ended ({}): {}; {} frames of silence sent",
        report.ending.as_deref().unwrap_or("-"),
        match result {
            Ok(()) => "left".to_owned(),
            Err(failure) => failure.to_string(),
        },
        report.silent_frames
    );
}

/// Runs the screen share of one huddle until `done`: takes `requests`,
/// tells the interface through `tell`, and never shares while `others`
/// (who share now, with `huddle-video`) are two already.
pub async fn run(
    content: ChimeJoin,
    mut requests: mpsc::Receiver<ShareRequest>,
    others: Option<watch::Receiver<Vec<Share>>>,
    mut done: oneshot::Receiver<()>,
    tell: impl Fn(ShareNews),
) {
    let mut starting: Option<tokio::task::JoinHandle<Step>> = None;
    // Asked to stop while a start was under way.
    let mut stop_after_start = false;
    let mut live: Option<Live> = None;
    loop {
        tokio::select! {
            _ = &mut done => break,
            request = requests.recv() => {
                let Some(request) = request else { break };
                let what = match request {
                    ShareRequest::Stop => {
                        stop_after_start = starting.is_some();
                        stop_live(live.take()).await;
                        tell(ShareNews::Off);
                        continue;
                    }
                    ShareRequest::Start { again } => {
                        if (live.is_some() && !again) || starting.is_some() {
                            continue;
                        }
                        let sharing = others.as_ref().map_or(0, |o| o.borrow().len());
                        if !may_share(sharing) {
                            tell(ShareNews::Failed(Failure::Huddle(HuddleTrouble::ShareLimit)));
                            continue;
                        }
                        Begin::Start { again }
                    }
                    ShareRequest::Pick(id) => {
                        if starting.is_some() {
                            continue;
                        }
                        Begin::Pick(id)
                    }
                };
                stop_live(live.take()).await;
                stop_after_start = false;
                starting = Some(tokio::task::spawn_blocking(move || begin(what)));
            }
            finished = started(&mut starting) => {
                let Some(step) = finished else {
                    log::warn!("huddle share: the start's thread failed");
                    tell(ShareNews::Failed(Failure::Huddle(HuddleTrouble::ShareCapture)));
                    continue;
                };
                if std::mem::take(&mut stop_after_start) {
                    if let Step::Started(Ok(share)) = step {
                        let _ = tokio::task::spawn_blocking(move || drop(share)).await;
                    }
                    continue;
                }
                match step {
                    Step::Choose(sources) => tell(ShareNews::Choose(sources)),
                    Step::Started(Err(failure)) => tell(ShareNews::Failed(failure)),
                    Step::Started(Ok(share)) => match go_live(&content, share) {
                        Ok(running) => live = Some(running),
                        Err(why) => {
                            log::warn!("huddle share: {why}");
                            tell(ShareNews::Failed(Failure::Huddle(HuddleTrouble::ShareCapture)));
                        }
                    },
                }
            }
            event = live_event(&mut live) => match event {
                LiveEvent::Up(true) => {
                    log::info!("huddle share: the share's connection is up; sharing");
                    tell(ShareNews::On);
                }
                LiveEvent::Up(false) => {}
                LiveEvent::Ended(ending) => {
                    log::info!("huddle share: the share ended by itself ({ending:?}); stopping");
                    stop_live(live.take()).await;
                    tell(match ending {
                        Ending::Ended => ShareNews::Ended,
                        Ending::Failed(failure) => ShareNews::Failed(failure),
                    });
                }
                LiveEvent::Refused => {
                    log::warn!("huddle share: Chime takes no video from the share (view only)");
                    stop_live(live.take()).await;
                    tell(ShareNews::Failed(Failure::Huddle(HuddleTrouble::ShareLimit)));
                }
                LiveEvent::Over(over) => {
                    // The session's task is done: not waited for again.
                    let finished = live.take();
                    let failure = match *over {
                        Ok((report, result)) => {
                            log_ending(&report, &result);
                            session_failure(report.refused_status, &result)
                        }
                        Err(error) => {
                            log::warn!("huddle share: the session's task failed: {error}");
                            Some(Failure::Huddle(HuddleTrouble::ShareLost))
                        }
                    };
                    if let Some(mut finished) = finished {
                        stop_sending(finished.encoding.take()).await;
                    }
                    tell(failure.map_or(ShareNews::Off, ShareNews::Failed));
                }
            },
        }
    }
    // The huddle was left: everything stops. A start still waiting on the
    // system's dialog closes its share itself when it returns, as its
    // share is dropped with the task's result.
    drop(starting.take());
    stop_live(live.take()).await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use noslacking_video_ipc::ShareProblem;

    #[test]
    fn the_helpers_troubles_have_their_words() {
        assert_eq!(
            start_failure(&ShareTrouble::Problem(ShareProblem::Cancelled, "x".into())),
            Failure::Huddle(HuddleTrouble::ShareCancelled)
        );
        assert_eq!(
            start_failure(&ShareTrouble::Lost("crashed".into())),
            Failure::Huddle(HuddleTrouble::ShareHelperLost)
        );
    }

    /// Without the setting, a list from the helper (here its own code on
    /// a thread, with pretend sources) goes to the call bar; a pick
    /// starts the share in the helper.
    #[test]
    fn the_helper_lists_what_can_be_shared_and_shares_the_pick() {
        let Step::Choose(sources) = begin(Begin::Start { again: false }) else {
            panic!("a list for the call bar");
        };
        assert_eq!(sources, helper::pretend::sources());
        let Step::Started(Ok(share)) = begin(Begin::Pick(sources[1].id.clone())) else {
            panic!("a share");
        };
        drop(share);
        // A source that is not there (any more).
        let Step::Started(Err(failure)) = begin(Begin::Pick("gone".into())) else {
            panic!("no share");
        };
        assert_eq!(failure, Failure::Huddle(HuddleTrouble::ShareGone));
    }

    /// Chime refusing the share (206 view only, 509 at capacity) is the
    /// two-share limit; a refused join is the server not taking it; a
    /// share that leaves well says nothing.
    #[test]
    fn a_refused_share_is_the_two_share_limit() {
        let failed = |stage| {
            Err(media::Failure {
                stage,
                why: "x".into(),
            })
        };
        let limit = Some(Failure::Huddle(HuddleTrouble::ShareLimit));
        assert_eq!(session_failure(Some(206), &Ok(())), limit);
        assert_eq!(session_failure(Some(509), &failed(Stage::Subscribe)), limit);
        assert_eq!(
            session_failure(Some(403), &failed(Stage::Join)),
            Some(Failure::Huddle(HuddleTrouble::ShareRefused))
        );
        assert_eq!(
            session_failure(None, &failed(Stage::Signaling)),
            Some(Failure::Huddle(HuddleTrouble::ShareRefused))
        );
        assert_eq!(
            session_failure(None, &failed(Stage::Media)),
            Some(Failure::Huddle(HuddleTrouble::ShareLost))
        );
        assert_eq!(session_failure(None, &Ok(())), None);
    }
}
