//! `noslacking --huddle-probe TEAM CHANNEL [--seconds N] [--send-tone]`:
//! the whole listening path from the command line, for trying it against
//! a real huddle and sending the log back.
//!
//! It signs in as the app does at start (the saved browser sign-in, from
//! the keyring), joins the huddle in the channel (starting one if none is
//! going on), plays what it hears for N seconds (30 unless told), leaves,
//! also on Ctrl+C, and ends with a summary and a line that names the step
//! that failed, if one did. Every step logs at info level; secrets never
//! do.
//!
//! With `--send-tone` it joins unmuted and sends a quiet 440 Hz tone the
//! whole time, through the same encoder as the microphone but never the
//! microphone itself, so whether Chime takes our audio can be heard in
//! Slack without anyone talking. Every five seconds the log says what was
//! sent and what Chime's RTCP receiver reports say of it.
//!
//! Video (see [`super::watch`]): every INDEX that changes is logged with
//! its sources, as are PAUSE, RESUME, BITRATES (every 20 s), the topics
//! and sizes of DATA_MESSAGEs and any REMOTE_VIDEO_UPDATE. `--video N`
//! receives up to N streams once the audio is live and logs what comes
//! on each; `--video-h264-only` offers no VP8; `--video-dump DIR` keeps
//! each stream's first 300 frames. The summary ends with each stream and
//! whether audio kept flowing through every re-SUBSCRIBE.
//!
//! With `--send-test-video` (a build with the `huddle-camera` feature) it
//! also sends a moving test picture with a clock as its camera, from the
//! start, never a real camera: once the audio is live it re-SUBSCRIBEs
//! both ways and sends H.264 on its first video m-line, so whether Slack
//! shows our video can be seen without anyone's camera. The log says
//! what was sent, the keyframes asked for and the bandwidth estimate.
//!
//! With `--send-test-share` (a build with the `huddle-share` feature) it
//! also shares a generated 1080p test screen (colour bars, a moving
//! clock) once the audio is live, never anyone's screen: as Slack's own
//! clients share, a second Chime attendee (`…#content`, with the join
//! token's `#content`), its own connection, sending only that picture.
//! So whether Slack shows our share can be seen without sharing a real
//! screen. The log says how the share's session went (did Chime take
//! the `#content` join, did it refuse a third share), what it sent, and
//! whether it was encoded on the GPU.

use std::path::PathBuf;
use std::time::Duration;

use super::join::{self, JoinFailure};
use super::media::{self, Stage};
use super::speaker::Speaker;

/// What the probe was asked to do.
#[derive(Clone, Debug)]
pub struct Options {
    /// The workspace's team id (`T…`), as saved at sign-in.
    pub team: String,
    /// The channel or conversation whose huddle to join (`C…`, `D…`).
    pub channel: String,
    /// How long to listen.
    pub seconds: u64,
    /// The media region `rooms.join` is asked for, if given; otherwise
    /// the nearest is looked up (see [`super::region`]).
    pub region: Option<String>,
    /// The app's settings file, for its proxy setting.
    pub settings: PathBuf,
    /// Join unmuted and send a quiet tone (never the microphone).
    pub send_tone: bool,
    /// Send a moving test picture as our camera (never a real one).
    pub send_test_video: bool,
    /// Share a generated test screen (never a real one).
    pub send_test_share: bool,
    /// What to look at of video (`--video`, `--video-h264-only`,
    /// `--video-dump`); Chime's video signaling is logged either way.
    pub video: super::video::Options,
}

/// The steps, by the name the last line gives a failure.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Step {
    /// Reading the saved sign-in.
    Keyring,
    /// The sign-in is not one that can join.
    SignIn,
    /// `rooms.join`.
    SlackJoin,
    /// Chime, by its own stages.
    Chime(Stage),
}

impl std::fmt::Display for Step {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Keyring => f.write_str("keyring (reading the saved sign-in)"),
            Self::SignIn => f.write_str("sign-in"),
            Self::SlackJoin => f.write_str("rooms.join (Slack)"),
            Self::Chime(Stage::Signaling) => f.write_str("Chime signaling socket"),
            Self::Chime(Stage::Join) => f.write_str("Chime JOIN"),
            Self::Chime(Stage::Relay) => f.write_str("TURN relay"),
            Self::Chime(Stage::Subscribe) => f.write_str("SUBSCRIBE (SDP offer and answer)"),
            Self::Chime(Stage::Connect) => f.write_str("ICE and DTLS through the relay"),
            Self::Chime(Stage::Media) => f.write_str("listening"),
        }
    }
}

/// The probe's last line and exit code for `outcome`.
pub fn verdict(outcome: &Result<(), (Step, String)>, audio_frames: u64) -> (String, i32) {
    match outcome {
        Ok(()) if audio_frames > 0 => (
            format!("probe: OK, {audio_frames} audio frames received and played"),
            0,
        ),
        Ok(()) => (
            "probe: connected and left cleanly, but no audio came (was anyone talking?)".into(),
            0,
        ),
        Err((step, why)) => (format!("probe: FAILED at {step}: {why}"), 1),
    }
}

/// Runs the probe; returns the process's exit code.
pub fn run(options: &Options) -> i32 {
    log::info!(
        "probe: NoSlacking {} on {}/{}; team {}, channel {}, {} s, region asked: {}, {}",
        env!("CARGO_PKG_VERSION"),
        std::env::consts::OS,
        std::env::consts::ARCH,
        options.team,
        options.channel,
        options.seconds,
        options.region.as_deref().unwrap_or("none"),
        if options.send_tone {
            "sending a 440 Hz tone"
        } else {
            "muted"
        }
    );
    log::info!(
        "probe: video: {}{}{}",
        match options.video.streams {
            0 => "logging Chime's video signaling only".to_owned(),
            n => format!("receiving up to {n} streams"),
        },
        if options.video.h264_only {
            ", offering H.264 only"
        } else {
            ", offering VP8 and H.264"
        },
        options
            .video
            .dump
            .as_ref()
            .map_or_else(String::new, |dir| format!(
                ", dumping the first frames to {}",
                dir.display()
            ))
    );
    if options.send_test_video && !cfg!(feature = "huddle-camera") {
        log::error!(
            "probe: FAILED: --send-test-video needs a build with the huddle-camera feature \
             (cargo run --release --features huddle-camera -- --huddle-probe ...)"
        );
        return 1;
    }
    if options.send_test_share && !cfg!(feature = "huddle-share") {
        log::error!(
            "probe: FAILED: --send-test-share needs a build with the huddle-share feature \
             (cargo run --release --features huddle-share,huddle-video -- --huddle-probe ...)"
        );
        return 1;
    }
    if let Some(dir) = &options.video.dump
        && let Err(error) = std::fs::create_dir_all(dir)
    {
        log::error!("probe: FAILED to make {}: {error}", dir.display());
        return 1;
    }
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            log::error!("probe: FAILED to start: {error}");
            return 1;
        }
    };
    let (outcome, frames) = runtime.block_on(probe(options, runtime.handle().clone()));
    let (line, code) = verdict(&outcome, frames);
    if code == 0 {
        log::info!("{line}");
    } else {
        log::error!("{line}");
    }
    log::logger().flush();
    code
}

async fn probe(
    options: &Options,
    handle: tokio::runtime::Handle,
) -> (Result<(), (Step, String)>, u64) {
    // The app's proxy setting applies here too.
    let settings = crate::settings::Settings::load(&options.settings);
    if let Err(error) = crate::slack::net::configure(&settings.proxy) {
        log::warn!("probe: the proxy setting does not work ({error:?}); going without");
    }
    // The app's "Use the graphics card for video" too.
    #[cfg(any(feature = "huddle-video", feature = "huddle-camera"))]
    {
        super::helper::set_gpu(settings.hardware_video);
        log::info!(
            "probe: video on the graphics card {}",
            if settings.hardware_video {
                "when it can (the setting)"
            } else {
                "off (the setting)"
            }
        );
    }

    log::info!("keyring: reading the sign-in for {}", options.team);
    let credentials = crate::credentials::Credentials::native(Some(handle));
    let token = match credentials.load_token(&options.team).await {
        Ok(Some(token)) => token,
        Ok(None) => {
            return (
                Err((
                    Step::Keyring,
                    format!(
                        "no saved sign-in for {}; sign in to that workspace in the app first",
                        options.team
                    ),
                )),
                0,
            );
        }
        Err(error) => return (Err((Step::Keyring, format!("{error:?}"))), 0),
    };
    if !token.is_session() {
        return (
            Err((
                Step::SignIn,
                "this workspace is signed in with the Slack app (OAuth); huddles need a browser \
                 sign-in"
                    .into(),
            )),
            0,
        );
    }
    log::info!("keyring: a browser sign-in");
    let client = crate::slack::Client::shared(token);

    // The device first, so a missing one shows before joining anything.
    let speaker = match Speaker::open(None) {
        Ok((speaker, feed)) => {
            log::info!("sound: the default output device is open");
            Some((speaker, feed))
        }
        Err(why) => {
            log::warn!("sound: {why}; listening without playing");
            None
        }
    };

    let region = super::region::for_join(options.region.as_deref()).await;
    log::info!(
        "slack: rooms.join in {} (regions {region})",
        options.channel
    );
    let joined = match join::join(&client, &options.channel, &region).await {
        Ok(joined) => joined,
        Err(JoinFailure::Slack(error)) => {
            return (Err((Step::SlackJoin, error.to_string())), 0);
        }
        Err(error) => return (Err((Step::SlackJoin, error.to_string())), 0),
    };
    log::info!(
        "slack: joined call {}; meeting {} in {}; attendee {} ({}); signaling {}, audio host {}, \
         TURN control {}",
        joined.call_id.as_deref().unwrap_or("?"),
        joined.meeting_id.as_deref().unwrap_or("?"),
        joined.media_region.as_deref().unwrap_or("?"),
        joined.attendee_id,
        joined.external_user_id.as_deref().unwrap_or("?"),
        super::host_of(&joined.signaling_url),
        super::host_of(&joined.audio_host_url),
        joined
            .turn_control_url
            .as_deref()
            .map_or("none", super::host_of),
    );

    let (stop, stopped) = tokio::sync::watch::channel(false);
    let seconds = options.seconds;
    let timer = tokio::spawn(async move {
        tokio::select! {
            () = tokio::time::sleep(Duration::from_secs(seconds)) => {
                log::info!("probe: {seconds} s are up; leaving");
            }
            interrupted = tokio::signal::ctrl_c() => {
                if interrupted.is_ok() {
                    log::info!("probe: Ctrl+C; leaving");
                }
            }
        }
        let _ = stop.send(true);
    });
    let feed = speaker.as_ref().map(|(_, feed)| feed.clone());
    // The tone, unmuted from the start; its sender lives as long as the
    // session, or the session would take its end for a mute.
    let (_unmuted, muted) = tokio::sync::watch::channel(false);
    let (uplink, tone) = if options.send_tone {
        let (frames, frames_in) = tokio::sync::mpsc::channel(25);
        match super::microphone::ToneSource::start(frames) {
            Ok(tone) => (
                Some(media::Uplink {
                    frames: frames_in,
                    muted,
                }),
                Some(tone),
            ),
            Err(why) => {
                log::warn!("probe: no tone ({why}); joining muted");
                (None, None)
            }
        }
    } else {
        (None, None)
    };
    #[cfg(feature = "huddle-camera")]
    let (camera, test_video) = if options.send_test_video {
        match test_video() {
            Ok((uplink, running)) => (Some(uplink), Some(running)),
            Err(why) => {
                log::warn!("probe: no test video ({why}); sending none");
                (None, None)
            }
        }
    } else {
        (None, None)
    };
    // The test share starts once the audio is live, as a share is started
    // in a huddle one is in.
    #[cfg(feature = "huddle-share")]
    let (live, share) = if options.send_test_share {
        let (live, up) = tokio::sync::oneshot::channel();
        let share = tokio::spawn(test_share(joined.content(), up, stopped.clone()));
        (Some(live), Some(share))
    } else {
        (None, None)
    };
    #[cfg(not(feature = "huddle-share"))]
    let live = None;
    let (report, result) = media::listen(
        &joined,
        feed,
        uplink,
        stopped,
        live,
        None,
        media::Video {
            options: Some(options.video.clone()),
            viewer: None,
            #[cfg(feature = "huddle-camera")]
            camera,
        },
    )
    .await;
    drop(tone);
    #[cfg(feature = "huddle-camera")]
    drop(test_video);
    timer.abort();
    #[cfg(feature = "huddle-share")]
    if let Some(share) = share {
        match share.await {
            Ok(lines) => {
                for line in lines {
                    log::info!("{line}");
                }
            }
            Err(error) => log::warn!("summary: share: its task failed: {error}"),
        }
    }

    log::info!(
        "summary: ended: {}",
        report.ending.as_deref().unwrap_or("-")
    );
    log::info!(
        "summary: relay: {}",
        report.relay.as_deref().unwrap_or("none")
    );
    log::info!("summary: frames by type: {:?}", report.frames);
    log::info!(
        "summary: ICE connected after {:?}, DTLS after {:?}, first audio after {:?}",
        report.ice_connected,
        report.dtls_up,
        report.first_audio
    );
    log::info!(
        "summary: {} audio frames, {} bytes; at most {} attendees listed",
        report.audio_frames,
        report.audio_bytes,
        report.most_attendees
    );
    log::info!(
        "summary: sent {} frames ({} bytes) of tone, {} of silence",
        report.sent_frames,
        report.sent_bytes,
        report.silent_frames
    );
    if let Some((speaker, feed)) = speaker {
        log::info!("summary: played {:?}", feed.played());
        drop(speaker);
    }
    if let Some(video) = &report.video {
        for line in video_summary(video) {
            log::info!("{line}");
        }
    }
    let frames = report.audio_frames;
    (
        result.map_err(|failure| (Step::Chime(failure.stage), failure.why)),
        frames,
    )
}

/// The test video's threads and the ends of its channels the session
/// does not hold; dropped, the threads stop.
#[cfg(feature = "huddle-camera")]
struct TestVideo {
    _pattern: super::camera::Capturing,
    _encoding: super::camera_send::Encoding,
    _on: tokio::sync::watch::Sender<bool>,
    _refusals: tokio::sync::mpsc::Receiver<()>,
}

/// The test picture as an always-on camera, encoded on its own thread:
/// the session's side of it, and the threads.
#[cfg(feature = "huddle-camera")]
fn test_video() -> Result<(super::camera_send::CameraUplink, TestVideo), String> {
    use super::camera::Latest;
    use super::camera_send::{self, CameraUplink, Encoding, SendControl};
    let latest = Latest::default();
    let (frames, frames_in) = tokio::sync::mpsc::channel(camera_send::QUEUE);
    let control = SendControl::default();
    let encoding = Encoding::spawn(latest.clone(), frames, control.clone(), None)?;
    let pattern = camera_send::test_pattern(latest)?;
    // On from the start; the sender lives as long as the session, or the
    // session would take its end for "off".
    let (on, on_rx) = tokio::sync::watch::channel(true);
    let (refused, refusals) = tokio::sync::mpsc::channel(1);
    log::info!(
        "probe: sending a test picture as our camera, {}x{} at {} fps",
        super::camera::MAX_WIDTH,
        super::camera::MAX_HEIGHT,
        super::camera::FPS
    );
    Ok((
        CameraUplink {
            frames: frames_in,
            on: on_rx,
            control,
            refused,
            descriptor: super::camera_send::DESCRIPTOR,
        },
        TestVideo {
            _pattern: pattern,
            _encoding: encoding,
            _on: on,
            _refusals: refusals,
        },
    ))
}

/// The probe's test share: waits for the audio to be live (`up`), then
/// has the video helper share its 1080p test screen (encoded on the GPU
/// if the setting and the helper allow) and sends it as the `content`
/// attendee until `stopped`. Returns the summary's lines.
#[cfg(feature = "huddle-share")]
async fn test_share(
    content: super::join::ChimeJoin,
    up: tokio::sync::oneshot::Receiver<()>,
    stopped: tokio::sync::watch::Receiver<bool>,
) -> Vec<String> {
    use super::camera_send::{QUEUE, SendControl};
    use super::helper::{self, Lane};
    use super::share_send::{self, Encoding};
    use super::video_encoder::{Limits, START_BITRATE};
    if up.await.is_err() {
        return vec!["summary: share: not started, the audio never came up".into()];
    }
    log::info!(
        "probe: sharing a 1080p test screen as {} (the JS SDK's content share: attendee and \
         join token with #content); NoSlacking tells Slack nothing else about it",
        content.attendee_id
    );
    let Some(helper) = helper::shared(Lane::Screen) else {
        return vec!["summary: share: no video helper (noslacking-video) to share with".into()];
    };
    let started = tokio::task::spawn_blocking(move || {
        helper.start_share(
            noslacking_video_ipc::ShareChoice::Test,
            helper::gpu(),
            Limits::SHARE.bitrate(START_BITRATE),
            "",
        )
    })
    .await;
    let share = match started {
        Ok(Ok((share, _))) => share,
        Ok(Err(trouble)) => {
            return vec![format!(
                "summary: share: the test screen did not start: {trouble:?}"
            )];
        }
        Err(error) => return vec![format!("summary: share: its start failed: {error}")],
    };
    let (encoded, encoded_in) = tokio::sync::mpsc::channel(QUEUE);
    let control = SendControl::new(Limits::SHARE);
    let (ending, _ended) = tokio::sync::watch::channel(None);
    let encoding = match Encoding::spawn(share, encoded, control.clone(), ending) {
        Ok(encoding) => encoding,
        Err(why) => return vec![format!("summary: share: no sending thread: {why}")],
    };
    let (_on, on) = tokio::sync::watch::channel(true);
    let (refused, mut refusals) = tokio::sync::mpsc::channel(1);
    let uplink = share_send::uplink(encoded_in, on, control, refused);
    let (report, result) = media::listen(
        &content,
        None,
        None,
        stopped,
        None,
        None,
        media::Video {
            options: None,
            viewer: None,
            camera: Some(uplink),
        },
    )
    .await;
    // Stopping waits on the helper: not on the async threads.
    let _ = tokio::task::spawn_blocking(move || drop(encoding)).await;
    let mut lines = vec![
        format!(
            "summary: share: ended {}; {}",
            report.ending.as_deref().unwrap_or("-"),
            match &result {
                Ok(()) => "left cleanly".to_owned(),
                Err(failure) =>
                    format!("FAILED at {}: {}", Step::Chime(failure.stage), failure.why),
            }
        ),
        format!(
            "summary: share: ICE after {:?}, DTLS after {:?}; {} frames by type {:?}",
            report.ice_connected,
            report.dtls_up,
            report.frames.values().sum::<u64>(),
            report.frames
        ),
    ];
    if refusals.try_recv().is_ok() || matches!(report.refused_status, Some(206 | 509)) {
        lines.push(
            "summary: share: Chime refused it (view only or at capacity): two people may \
             already be sharing"
                .into(),
        );
    }
    if matches!(result, Err(ref f) if matches!(f.stage, Stage::Signaling | Stage::Join)) {
        lines.push(
            "summary: share: the #content join failed: Slack's join token may not take the \
             #content suffix"
                .into(),
        );
    }
    lines
}

/// The summary's video lines: what INDEX showed, each stream received,
/// and whether audio flowed through every re-SUBSCRIBE.
pub fn video_summary(video: &super::video::Summary) -> Vec<String> {
    let mut lines = vec![format!(
        "summary: video: {} INDEX changes logged; a #content source (screen share) {}; last codec \
         intersection [{}]",
        video.indexes,
        if video.saw_share {
            "was listed"
        } else {
            "never listed"
        },
        video.codecs.join(", ")
    )];
    if video.streams.is_empty() {
        lines.push("summary: video: no stream received".into());
    }
    for stream in &video.streams {
        lines.push(format!("summary: video: {}", stream.line()));
    }
    for resubscribe in &video.resubscribes {
        lines.push(format!("summary: video: {}", resubscribe.line()));
    }
    if !video.resubscribes.is_empty() {
        let kept = video
            .resubscribes
            .iter()
            .all(|r| r.audio_frames.is_some_and(|n| n > 0));
        lines.push(format!(
            "summary: video: audio {} through all {} re-SUBSCRIBEs",
            if kept {
                "stayed live"
            } else {
                "did NOT stay live"
            },
            video.resubscribes.len()
        ));
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_video_summary_says_what_came_and_whether_audio_flowed() {
        let summary = super::super::video::Summary {
            streams: vec![super::super::video::StreamStats {
                stream_id: 6,
                attendee: "abcdef12-content".into(),
                share: true,
                mid: "2".into(),
                codec: Some(("H264".into(), 108)),
                frames: 90,
                keyframes: 2,
                ..Default::default()
            }],
            resubscribes: vec![super::super::video::Resubscribe {
                n: 1,
                stream_ids: vec![0, 6],
                answered_ms: Some(80),
                audio_frames: Some(104),
                window_ms: 2080,
            }],
            indexes: 3,
            codecs: vec!["H264_CONSTRAINED_BASELINE_PROFILE".into()],
            saw_share: true,
        };
        let lines = video_summary(&summary);
        assert!(
            lines[0].contains("screen share) was listed"),
            "{}",
            lines[0]
        );
        assert!(
            lines[1].contains("stream 6 (abcdef12-content, a share"),
            "{}",
            lines[1]
        );
        assert!(lines[2].contains("104 audio frames"), "{}", lines[2]);
        assert_eq!(
            lines[3],
            "summary: video: audio stayed live through all 1 re-SUBSCRIBEs"
        );
        let empty = video_summary(&super::super::video::Summary::default());
        assert_eq!(empty[1], "summary: video: no stream received");
    }

    #[test]
    fn the_last_line_names_the_failed_step() {
        let (line, code) = verdict(
            &Err((
                Step::Chime(Stage::Relay),
                "no TURN server gave a relay".into(),
            )),
            0,
        );
        assert_eq!(code, 1);
        assert_eq!(
            line,
            "probe: FAILED at TURN relay: no TURN server gave a relay"
        );
        let (line, code) = verdict(&Ok(()), 120);
        assert_eq!(code, 0);
        assert!(line.contains("120 audio frames"));
        assert!(verdict(&Ok(()), 0).0.contains("no audio came"));
    }
}
