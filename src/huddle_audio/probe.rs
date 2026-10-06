//! `noslacking --huddle-probe TEAM CHANNEL [--seconds N]`: the whole
//! listening path from the command line, for trying it against a real
//! huddle and sending the log back.
//!
//! It signs in as the app does at start (the saved browser sign-in, from
//! the keyring), joins the huddle in the channel (starting one if none is
//! going on), plays what it hears for N seconds (30 unless told), leaves,
//! also on Ctrl+C, and ends with a summary and a line that names the step
//! that failed, if one did. Every step logs at info level; secrets never
//! do.

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
        "probe: NoSlacking {} on {}/{}; team {}, channel {}, {} s, region asked: {}",
        env!("CARGO_PKG_VERSION"),
        std::env::consts::OS,
        std::env::consts::ARCH,
        options.team,
        options.channel,
        options.seconds,
        options.region.as_deref().unwrap_or("none")
    );
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
    let speaker = match Speaker::open() {
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
    let (report, result) = media::listen(&joined, feed, stopped, None).await;
    timer.abort();

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
    if let Some((speaker, feed)) = speaker {
        log::info!("summary: played {:?}", feed.played());
        drop(speaker);
    }
    let frames = report.audio_frames;
    (
        result.map_err(|failure| (Step::Chime(failure.stage), failure.why)),
        frames,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

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
