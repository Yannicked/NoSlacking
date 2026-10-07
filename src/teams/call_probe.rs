//! `noslacking --teams-call-probe TEAM MRI [--seconds N]`: an outgoing
//! Teams call from the command line, for trying calls against a real
//! account and sending the log back.
//!
//! It signs in as the app does at start (the saved Teams sign-in of
//! workspace `TEAM`, from the keyring), opens the live connection, calls
//! `MRI` (`8:live:…` or `8:orgid:…`) with a quiet 440 Hz tone in place of
//! the microphone, hangs up after N seconds (30 unless told), or when the
//! far end does, and ends with a summary and a last line naming how it
//! went. Every step logs at info level; tokens, credentials and callback
//! addresses never do.

use std::time::Duration;

use super::calling::call::{CallEvent, Control, outgoing};
use super::calling::media::{Audio, Tone};

/// What the probe was asked to do.
#[derive(Clone, Debug)]
pub struct Options {
    /// The Teams workspace's id, as saved at sign-in (`teams_…`).
    pub team: String,
    /// Whom to call.
    pub callee: String,
    /// How long to stay in the call once it is live.
    pub seconds: u64,
    /// The app's settings file, for its proxy setting.
    pub settings: std::path::PathBuf,
}

/// Runs the probe; answers the exit code: 0 when the call was live.
pub fn run(options: &Options) -> i32 {
    log::info!(
        "teams call probe: NoSlacking {} on {}/{}; calling for {} s",
        env!("CARGO_PKG_VERSION"),
        std::env::consts::OS,
        std::env::consts::ARCH,
        options.seconds
    );
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            log::error!("teams call probe: FAILED to start: {error}");
            return 1;
        }
    };
    let handle = runtime.handle().clone();
    let code = runtime.block_on(probe(options, handle));
    log::logger().flush();
    code
}

async fn probe(options: &Options, handle: tokio::runtime::Handle) -> i32 {
    let settings = crate::settings::Settings::load(&options.settings);
    if let Err(error) = crate::slack::net::configure(&settings.proxy) {
        log::warn!("teams call probe: the proxy setting does not work ({error:?}); going without");
    }
    let credentials = crate::credentials::Credentials::native(Some(handle));
    let creds = match credentials.load_teams_token(&options.team).await {
        Ok(Some(creds)) => creds,
        Ok(None) => {
            log::error!(
                "teams call probe: FAILED: no saved Teams sign-in for {}",
                options.team
            );
            return 1;
        }
        Err(error) => {
            log::error!("teams call probe: FAILED to read the keyring: {error}");
            return 1;
        }
    };
    let sink = crate::backend::Sink::nowhere();
    let client = crate::backend::teams::client(creds, &options.team, credentials, sink.clone());
    let live = tokio::spawn(crate::backend::teams::trouter(
        options.team.clone(),
        client.clone(),
        sink,
        std::sync::Arc::new(|status| log::info!("teams call probe: live connection {status:?}")),
    ));

    // The call's callbacks name the live connection: wait for it.
    let mut waited = Duration::ZERO;
    while client.calls().surl().is_none() {
        if waited >= Duration::from_secs(30) {
            log::error!("teams call probe: FAILED: the live connection did not come up");
            live.abort();
            return 1;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
        waited += Duration::from_millis(250);
    }

    let (tone, uplink) = match Tone::start() {
        Ok(started) => started,
        Err(error) => {
            log::error!("teams call probe: FAILED to make the tone: {error}");
            live.abort();
            return 1;
        }
    };
    let audio = Audio {
        feed: None,
        uplink: Some(uplink),
    };
    let (control, steer) = tokio::sync::mpsc::unbounded_channel();
    let (events, mut told) = tokio::sync::mpsc::unbounded_channel();
    let call = tokio::spawn(outgoing(
        client.clone(),
        options.callee.clone(),
        audio,
        steer,
        move |event| {
            let _ = events.send(event);
        },
    ));

    let mut was_live = false;
    let mut flowing = false;
    let mut hang_up_at: Option<tokio::time::Instant> = None;
    let code = loop {
        let deadline =
            hang_up_at.unwrap_or_else(|| tokio::time::Instant::now() + Duration::from_secs(3600));
        tokio::select! {
            event = told.recv() => match event {
                Some(CallEvent::Ringing) => log::info!("teams call probe: ringing"),
                Some(CallEvent::Live) => {
                    log::info!("teams call probe: live; hanging up in {} s", options.seconds);
                    was_live = true;
                    hang_up_at = Some(tokio::time::Instant::now() + Duration::from_secs(options.seconds));
                }
                Some(CallEvent::AudioFlowing) => {
                    flowing = true;
                    log::info!("teams call probe: audio flows both ways");
                }
                Some(CallEvent::Ended { result, counts }) => {
                    log::info!(
                        "teams call probe: summary: packets in {} out {}, audio frames in {} out {}",
                        counts.packets_in, counts.packets_out, counts.audio_in, counts.audio_out
                    );
                    break match result {
                        Ok(()) if was_live => {
                            log::info!(
                                "teams call probe: OK: the call was live{}",
                                if flowing { " and audio flowed both ways" } else { ", but audio did not flow both ways" }
                            );
                            0
                        }
                        Ok(()) => {
                            log::error!("teams call probe: ended before it was live");
                            1
                        }
                        Err(failure) => {
                            log::error!("teams call probe: FAILED: {failure:?}");
                            1
                        }
                    };
                }
                None => break 1,
            },
            () = tokio::time::sleep_until(deadline), if hang_up_at.is_some() => {
                log::info!("teams call probe: hanging up");
                let _ = control.send(Control::HangUp);
                hang_up_at = None;
            }
        }
    };
    drop(tone);
    let _ = call.await;
    live.abort();
    code
}
