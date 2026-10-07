//! The worker's side of sharing your screen (the `huddle-share` feature):
//! the capture, its encoder and the share's own session, started when the
//! interface asks and stopped when it asks, when the huddle is left, when
//! the share's session fails or Chime refuses it, or when the capture
//! ends by itself.
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

use std::time::Duration;

use tokio::sync::{mpsc, oneshot, watch};

use crate::failure::{Failure, HuddleTrouble};
use crate::huddle_audio::camera_send::SendControl;
use crate::huddle_audio::join::ChimeJoin;
use crate::huddle_audio::media::{self, Stage};
use crate::huddle_audio::share::{Choice, ShareControl, ShareError, Source, System, may_share};
use crate::huddle_audio::share_send::{self, Encoding};
use crate::huddle_audio::video::Share;
use crate::huddle_audio::video_encoder::Limits;
use crate::huddle_share::{ShareNews, ShareRequest};

/// How long the share's session gets to leave (LEAVE, then LEAVE_ACK)
/// once told to stop.
const LEAVE_WAIT: Duration = Duration::from_secs(4);

/// What a capture that did not start tells the interface.
pub fn capture_failure(error: &ShareError) -> Failure {
    Failure::Huddle(match error {
        ShareError::Cancelled => HuddleTrouble::ShareCancelled,
        ShareError::Denied => HuddleTrouble::ShareDenied,
        ShareError::Unavailable => HuddleTrouble::NoScreenCapture,
        ShareError::Gone => HuddleTrouble::ShareGone,
        ShareError::Failed(_) => HuddleTrouble::ShareCapture,
    })
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

/// A share running: its session, its encoder, what ends it.
struct Live {
    halt: watch::Sender<bool>,
    session: tokio::task::JoinHandle<(media::Report, Result<(), media::Failure>)>,
    encoding: Option<Encoding>,
    /// Told when the capture ends by itself.
    ended: watch::Receiver<bool>,
    /// Told when Chime takes no video from the share.
    refusals: mpsc::Receiver<()>,
    /// Told once the share's connection is up.
    up: Option<oneshot::Receiver<()>>,
    /// The share is on for as long as it runs (the session's "camera").
    _on: watch::Sender<bool>,
}

/// What a start on the blocking thread came to.
enum Step {
    /// No dialog of the system's: these can be picked.
    Choose(Vec<Source>),
    /// The capture started, or why not.
    Started(Result<(), ShareError>),
}

type Starting = tokio::task::JoinHandle<(ShareControl<System>, Step)>;

/// How a share's session ended, or its task failing.
type Ended = Result<(media::Report, Result<(), media::Failure>), tokio::task::JoinError>;

/// What a running share has to say.
enum LiveEvent {
    /// Its connection came up (or its session went before it did).
    Up(bool),
    /// The capture ended by itself.
    CaptureEnded,
    /// Chime takes no video from it.
    Refused,
    /// Its session ended by itself.
    Over(Box<Ended>),
}

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
        () = async {
            loop {
                if *ended.borrow_and_update() {
                    return;
                }
                if ended.changed().await.is_err() {
                    return std::future::pending().await;
                }
            }
        } => LiveEvent::CaptureEnded,
        () = async {
            if refusals.recv().await.is_none() {
                std::future::pending::<()>().await;
            }
        } => LiveEvent::Refused,
        over = session => LiveEvent::Over(Box::new(over)),
    }
}

/// A start under way finishing, or never.
async fn started(starting: &mut Option<Starting>) -> Option<(ShareControl<System>, Step)> {
    match starting {
        Some(job) => {
            let done = job.await.ok();
            *starting = None;
            done
        }
        None => std::future::pending().await,
    }
}

/// Starts the share's session and encoder for a capture that started.
fn go_live(
    content: &ChimeJoin,
    frames: crate::huddle_audio::share::Frames,
) -> Result<Live, String> {
    let (encoded, encoded_in) = mpsc::channel(crate::huddle_audio::camera_send::QUEUE);
    let control = SendControl::new(Limits::SHARE);
    let encoding = Encoding::spawn(frames, encoded, control.clone())?;
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
        ended: watch::channel(false).1,
        refusals,
        up: Some(up_rx),
        _on: on,
    })
}

/// Stops a running share: its session leaves (waited for a moment), its
/// encoder stops, then its capture.
async fn stop_live(live: Option<Live>, control: &mut Option<ShareControl<System>>) {
    if let Some(mut live) = live {
        let _ = live.halt.send(true);
        match tokio::time::timeout(LEAVE_WAIT, &mut live.session).await {
            Ok(Ok((report, result))) => log_ending(&report, &result),
            Ok(Err(error)) => log::warn!("huddle share: the session's task failed: {error}"),
            Err(_) => {
                log::warn!("huddle share: the session did not leave in time");
                live.session.abort();
            }
        }
        let encoding = live.encoding.take();
        // Joining the encoder's thread waits on it: not on this one.
        let _ = tokio::task::spawn_blocking(move || drop(encoding)).await;
    }
    stop_capture(control).await;
}

/// Stops the capture (joining its threads, closing the portal's
/// session) off the async threads.
async fn stop_capture(control: &mut Option<ShareControl<System>>) {
    let Some(mut held) = control.take().filter(ShareControl::is_capturing) else {
        return;
    };
    let stopped = tokio::task::spawn_blocking(move || {
        held.stop();
        held
    })
    .await;
    // A thread that failed took the control with it; the next share makes
    // a fresh one.
    *control = stopped.ok();
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
    let frames = crate::huddle_audio::share::Frames::default();
    let fresh = || {
        let system = System::new(frames.clone());
        log::debug!(
            "huddle share: screens are captured through {}",
            system.name()
        );
        ShareControl::new(system)
    };
    let mut control = None;
    let mut starting: Option<Starting> = None;
    // Asked to stop while a start was under way.
    let mut stop_after_start = false;
    let mut live: Option<Live> = None;
    loop {
        tokio::select! {
            _ = &mut done => break,
            request = requests.recv() => {
                let Some(request) = request else { break };
                let choice = match request {
                    ShareRequest::Stop => {
                        stop_after_start = starting.is_some();
                        stop_live(live.take(), &mut control).await;
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
                        Err(again)
                    }
                    ShareRequest::Pick(id) => {
                        if starting.is_some() {
                            continue;
                        }
                        Ok(Choice::Source(id))
                    }
                };
                stop_live(live.take(), &mut control).await;
                let mut held = control.take().unwrap_or_else(fresh);
                stop_after_start = false;
                // Asking the system (its dialog) and starting the capture
                // wait on other threads and on the user: not on this one.
                starting = Some(tokio::task::spawn_blocking(move || {
                    let step = match choice {
                        Ok(choice) => Step::Started(held.start(&choice)),
                        Err(again) => match held.sources() {
                            Ok(sources) if !sources.is_empty() => Step::Choose(sources),
                            Ok(_) => Step::Started(held.start(&Choice::System { again })),
                            Err(error) => Step::Started(Err(error)),
                        },
                    };
                    (held, step)
                }));
            }
            finished = started(&mut starting) => {
                let Some((held, step)) = finished else {
                    log::warn!("huddle share: the capture's thread failed");
                    tell(ShareNews::Failed(Failure::Huddle(HuddleTrouble::ShareCapture)));
                    continue;
                };
                control = Some(held);
                if std::mem::take(&mut stop_after_start) {
                    stop_capture(&mut control).await;
                    continue;
                }
                match step {
                    Step::Choose(sources) => tell(ShareNews::Choose(sources)),
                    Step::Started(Err(error)) => {
                        log::warn!("huddle share: {error}");
                        tell(ShareNews::Failed(capture_failure(&error)));
                    }
                    Step::Started(Ok(())) => match go_live(&content, frames.clone()) {
                        Ok(mut running) => {
                            if let Some(ended) = control.as_ref().and_then(ShareControl::ended) {
                                running.ended = ended;
                            }
                            live = Some(running);
                        }
                        Err(why) => {
                            log::warn!("huddle share: {why}");
                            stop_capture(&mut control).await;
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
                LiveEvent::CaptureEnded => {
                    log::info!("huddle share: the capture ended; stopping the share");
                    stop_live(live.take(), &mut control).await;
                    tell(ShareNews::Ended);
                }
                LiveEvent::Refused => {
                    log::warn!("huddle share: Chime takes no video from the share (view only)");
                    stop_live(live.take(), &mut control).await;
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
                        let encoding = finished.encoding.take();
                        let _ = tokio::task::spawn_blocking(move || drop(encoding)).await;
                    }
                    stop_capture(&mut control).await;
                    tell(failure.map_or(ShareNews::Off, ShareNews::Failed));
                }
            },
        }
    }
    // The huddle was left: everything stops. A start still waiting on the
    // system's dialog stops its capture itself when it returns, as its
    // control is dropped with it.
    drop(starting.take());
    stop_live(live.take(), &mut control).await;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capture_failures_have_their_words() {
        for (error, trouble) in [
            (ShareError::Cancelled, HuddleTrouble::ShareCancelled),
            (ShareError::Denied, HuddleTrouble::ShareDenied),
            (ShareError::Unavailable, HuddleTrouble::NoScreenCapture),
            (ShareError::Gone, HuddleTrouble::ShareGone),
            (ShareError::Failed("x".into()), HuddleTrouble::ShareCapture),
        ] {
            assert_eq!(capture_failure(&error), Failure::Huddle(trouble));
        }
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
