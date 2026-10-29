//! The worker's side of [`crate::devices`]: listing the microphones and
//! speakers (cpal) and the cameras (the video helper's camera lane,
//! which lists them without opening one), each on a blocking thread, so
//! neither the interface nor the worker's loop waits for the system.

use super::{Event, Sink};
use crate::devices::{Device, Kind};
use crate::failure::{Failure, HuddleTrouble};

/// Lists the devices of `kind` and sends them to the interface.
pub async fn list(kind: Kind, sink: Sink) {
    let (default_name, result) = match tokio::task::spawn_blocking(move || listed(kind)).await {
        Ok(result) => result,
        Err(error) => {
            log::warn!("devices: listing {kind:?} failed: {error}");
            (None, Err(Failure::Huddle(HuddleTrouble::NoDeviceList)))
        }
    };
    sink.send(Event::Devices(crate::devices::Event::Listed {
        kind,
        default_name,
        result,
    }));
}

/// The devices of `kind` there are, or why not, and what the default
/// device is called now. Blocks.
fn listed(kind: Kind) -> (Option<String>, Result<Vec<Device>, Failure>) {
    let default_name = default_device_name(kind);
    let result = match kind {
        Kind::Microphone | Kind::Speaker => {
            crate::huddle_audio::devices::list(kind).map_err(|error| {
                log::warn!("devices: no {kind:?} list: {error}");
                Failure::Huddle(HuddleTrouble::NoDeviceList)
            })
        }
        Kind::Camera => cameras(),
    };
    (default_name, result)
}

/// What the system default device of `kind` is called now, if known.
#[cfg(target_os = "linux")]
fn default_device_name(kind: Kind) -> Option<String> {
    if let Some(name) = pulseaudio_default_description(kind) {
        return Some(name);
    }
    // Fall back to cpal (ALSA) when PulseAudio / PipeWire is unavailable.
    cpal_default(kind)
}

/// What the system default device of `kind` is called now, if known.
#[cfg(not(target_os = "linux"))]
fn default_device_name(kind: Kind) -> Option<String> {
    cpal_default(kind)
}

/// Queries cpal for the system default device name.
fn cpal_default(kind: Kind) -> Option<String> {
    use rodio::cpal::traits::{DeviceTrait as _, HostTrait as _};
    let host = rodio::cpal::default_host();
    let device = match kind {
        Kind::Microphone => host.default_input_device()?,
        Kind::Speaker => host.default_output_device()?,
        Kind::Camera => return None,
    };
    device
        .description()
        .map(|d| d.name().to_owned())
        .or_else(|_| device.id().map(|id| id.1))
        .ok()
}

/// Connects directly to PulseAudio or `pipewire-pulse` via its UNIX domain socket
/// to query the user-friendly description of the current default device.
#[cfg(target_os = "linux")]
fn pulseaudio_default_description(kind: Kind) -> Option<String> {
    use pulseaudio::protocol::{self, Command};
    use std::ffi::CString;
    use std::io::BufReader;
    use std::os::unix::net::UnixStream;
    use std::time::Duration;

    let socket_path = pulse_socket_path()?;
    let stream = UnixStream::connect(socket_path).ok()?;
    let timeout = Some(Duration::from_millis(500));
    let _ = stream.set_read_timeout(timeout);
    let _ = stream.set_write_timeout(timeout);
    let mut sock = BufReader::new(stream);

    let cookie = pulse_cookie().unwrap_or_default();
    let auth = protocol::AuthParams {
        version: protocol::MAX_VERSION,
        supports_shm: false,
        supports_memfd: false,
        cookie,
    };

    let mut seq = 0u32;
    protocol::write_command_message(
        sock.get_mut(),
        seq,
        &Command::Auth(auth),
        protocol::MAX_VERSION,
    )
    .ok()?;
    let (_, auth_reply) =
        protocol::read_reply_message::<protocol::AuthReply>(&mut sock, protocol::MAX_VERSION)
            .ok()?;
    let version = std::cmp::min(protocol::MAX_VERSION, auth_reply.version);

    seq += 1;
    let mut props = protocol::Props::new();
    props.set(
        protocol::Prop::ApplicationName,
        CString::new("noslacking").ok()?,
    );
    protocol::write_command_message(sock.get_mut(), seq, &Command::SetClientName(props), version)
        .ok()?;
    let _ =
        protocol::read_reply_message::<protocol::SetClientNameReply>(&mut sock, version).ok()?;

    seq += 1;
    protocol::write_command_message(sock.get_mut(), seq, &Command::GetServerInfo, version).ok()?;
    let (_, server_info) =
        protocol::read_reply_message::<protocol::ServerInfo>(&mut sock, version).ok()?;

    match kind {
        Kind::Microphone => {
            let target_name = server_info.default_source_name?;
            seq += 1;
            let cmd = Command::GetSourceInfo(protocol::GetSourceInfo {
                index: None,
                name: Some(target_name.clone()),
            });
            protocol::write_command_message(sock.get_mut(), seq, &cmd, version).ok()?;
            let (_, source_info) =
                protocol::read_reply_message::<protocol::SourceInfo>(&mut sock, version).ok()?;
            source_info
                .description
                .and_then(|c| c.into_string().ok())
                .or_else(|| target_name.into_string().ok())
        }
        Kind::Speaker => {
            let target_name = server_info.default_sink_name?;
            seq += 1;
            let cmd = Command::GetSinkInfo(protocol::GetSinkInfo {
                index: None,
                name: Some(target_name.clone()),
            });
            protocol::write_command_message(sock.get_mut(), seq, &cmd, version).ok()?;
            let (_, sink_info) =
                protocol::read_reply_message::<protocol::SinkInfo>(&mut sock, version).ok()?;
            sink_info
                .description
                .and_then(|c| c.into_string().ok())
                .or_else(|| target_name.into_string().ok())
        }
        Kind::Camera => None,
    }
}

/// Resolves the PulseAudio native protocol UNIX domain socket path.
#[cfg(target_os = "linux")]
fn pulse_socket_path() -> Option<std::path::PathBuf> {
    use std::path::PathBuf;
    let candidates = [
        std::env::var("PULSE_SERVER")
            .ok()
            .and_then(|s| s.strip_prefix("unix:").map(PathBuf::from)),
        std::env::var("PULSE_RUNTIME_PATH")
            .ok()
            .map(|s| PathBuf::from(s).join("pulse/native")),
        std::env::var("XDG_RUNTIME_DIR")
            .ok()
            .map(|s| PathBuf::from(s).join("pulse/native")),
    ];
    candidates.into_iter().flatten().find(|path| path.exists())
}

/// Loads the PulseAudio client authentication cookie if available.
#[cfg(target_os = "linux")]
fn pulse_cookie() -> Option<Vec<u8>> {
    use std::path::PathBuf;
    let dirs = directories::BaseDirs::new();
    let candidates = [
        std::env::var("PULSE_COOKIE").ok().map(PathBuf::from),
        dirs.as_ref().map(|d| d.config_dir().join("pulse/cookie")),
        dirs.as_ref()
            .map(|d| d.home_dir().join(".config/pulse/cookie")),
        dirs.as_ref().map(|d| d.home_dir().join(".pulse-cookie")),
    ];
    candidates
        .into_iter()
        .flatten()
        .find_map(|path| std::fs::read(&path).ok().filter(|bytes| !bytes.is_empty()))
}

/// The cameras the video helper finds; none without it, saying so as
/// turning the camera on would.
#[cfg(feature = "huddle-camera")]
fn cameras() -> Result<Vec<Device>, Failure> {
    use crate::huddle_audio::camera_send::{camera_failure, devices_of};
    use crate::huddle_audio::helper::{self, Lane};
    let Some(helper) = helper::shared(Lane::Camera).filter(|h| !h.given_up()) else {
        return Err(Failure::Huddle(HuddleTrouble::CameraNeedsHelper));
    };
    helper
        .cameras()
        .map(|cameras| devices_of(&cameras))
        .map_err(|trouble| {
            log::warn!("devices: no camera list: {trouble:?}");
            camera_failure(&trouble, helper.given_up())
        })
}

/// Without `huddle-camera` there is no camera to choose.
#[cfg(not(feature = "huddle-camera"))]
fn cameras() -> Result<Vec<Device>, Failure> {
    Err(Failure::Huddle(HuddleTrouble::CameraNeedsHelper))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The camera lane's helper in tests is the helper's own code on a
    /// thread, whose camera is a pretend one: none is opened to list it.
    #[cfg(feature = "huddle-camera")]
    #[test]
    fn cameras_are_listed_by_the_helper() {
        assert_eq!(
            cameras(),
            Ok(vec![Device {
                id: "pretend:camera".into(),
                name: "A pretend camera".into(),
            }])
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn socket_path_candidate_detection() {
        // Checks that candidate detection doesn't crash or panic.
        let _ = pulse_socket_path();
        let _ = pulse_cookie();
    }

    #[cfg(target_os = "linux")]
    #[test]
    #[ignore = "touches the live system audio server"]
    fn query_live_pulseaudio() {
        let mic = pulseaudio_default_description(Kind::Microphone);
        let speaker = pulseaudio_default_description(Kind::Speaker);
        assert!(mic.is_some() || speaker.is_some());
    }
}
