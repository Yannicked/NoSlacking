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
    let target = match kind {
        Kind::Microphone => "@DEFAULT_AUDIO_SOURCE@",
        Kind::Speaker => "@DEFAULT_AUDIO_SINK@",
        Kind::Camera => return None,
    };
    if let Some(name) = pipewire_default(target) {
        return Some(name);
    }
    pulseaudio_default(kind)
}

/// What the system default device of `kind` is called now, if known.
#[cfg(not(target_os = "linux"))]
fn default_device_name(kind: Kind) -> Option<String> {
    use rodio::cpal::traits::{DeviceTrait as _, HostTrait as _};
    let host = rodio::cpal::default_host();
    match kind {
        Kind::Microphone => host.default_input_device().and_then(|d| d.name().ok()),
        Kind::Speaker => host.default_output_device().and_then(|d| d.name().ok()),
        Kind::Camera => None,
    }
}

/// Queries PipeWire (`wpctl`) for the default node's description.
#[cfg(target_os = "linux")]
fn pipewire_default(target: &str) -> Option<String> {
    let output = std::process::Command::new("wpctl")
        .args(["inspect", target])
        .stdin(std::process::Stdio::null())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&output.stdout);
    parse_pipewire_description(&text)
}

/// Parses the `node.description` line from `wpctl inspect` output.
#[cfg(any(test, target_os = "linux"))]
fn parse_pipewire_description(text: &str) -> Option<String> {
    for line in text.lines() {
        let trimmed = line
            .trim()
            .trim_start_matches(|c: char| c == '*' || c.is_whitespace());
        if let Some(desc) = trimmed.strip_prefix("node.description = \"")
            && let Some(name) = desc.strip_suffix('"')
        {
            let name = name.trim();
            if !name.is_empty() {
                return Some(name.to_owned());
            }
        }
    }
    None
}

/// Queries PulseAudio (`pactl`) for the default source/sink description.
#[cfg(target_os = "linux")]
fn pulseaudio_default(kind: Kind) -> Option<String> {
    let (get_cmd, list_cmd) = match kind {
        Kind::Microphone => ("get-default-source", "sources"),
        Kind::Speaker => ("get-default-sink", "sinks"),
        Kind::Camera => return None,
    };
    let default_output = std::process::Command::new("pactl")
        .arg(get_cmd)
        .stdin(std::process::Stdio::null())
        .output()
        .ok()?;
    if !default_output.status.success() {
        return None;
    }
    let default_id = String::from_utf8_lossy(&default_output.stdout)
        .trim()
        .to_owned();
    if default_id.is_empty() {
        return None;
    }
    let list_output = std::process::Command::new("pactl")
        .args(["list", list_cmd])
        .stdin(std::process::Stdio::null())
        .output()
        .ok()?;
    if !list_output.status.success() {
        return None;
    }
    let list_text = String::from_utf8_lossy(&list_output.stdout);
    parse_pactl_description(&list_text, &default_id)
}

/// Parses the human-friendly description of `target_name` from `pactl list` output.
#[cfg(any(test, target_os = "linux"))]
fn parse_pactl_description(text: &str, target_name: &str) -> Option<String> {
    let mut current_name = None;
    let mut current_desc = None;
    for line in text.lines() {
        let line = line.trim();
        if let Some(name) = line.strip_prefix("Name: ") {
            if current_name.as_deref() == Some(target_name) && current_desc.is_some() {
                return current_desc;
            }
            current_name = Some(name.to_owned());
            current_desc = None;
        } else if let Some(desc) = line.strip_prefix("Description: ") {
            current_desc = Some(desc.to_owned());
        }
    }
    if current_name.as_deref() == Some(target_name) {
        current_desc
    } else {
        None
    }
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

    #[test]
    fn parses_pipewire_description() {
        let sample = "\
id 138, type PipeWire:Interface:Node
  * media.class = \"Audio/Source\"
  * node.description = \"Jabra Evolve2 65\"
  * node.name = \"bluez_input.50:C2:ED:B7:4A:23\"
";
        assert_eq!(
            parse_pipewire_description(sample).as_deref(),
            Some("Jabra Evolve2 65")
        );
        assert_eq!(parse_pipewire_description("no description here"), None);
    }

    #[test]
    fn parses_pactl_description() {
        let sample = "\
Source #50
\tState: RUNNING
\tName: alsa_input.pci-0000_c1_00.6.HiFi__Mic2__source
\tDescription: Ryzen HD Audio Controller Stereo Microphone
Source #190
\tState: IDLE
\tName: bluez_input.50:C2:ED:B7:4A:23
\tDescription: Jabra Evolve2 65
";
        assert_eq!(
            parse_pactl_description(sample, "bluez_input.50:C2:ED:B7:4A:23").as_deref(),
            Some("Jabra Evolve2 65")
        );
        assert_eq!(
            parse_pactl_description(sample, "alsa_input.pci-0000_c1_00.6.HiFi__Mic2__source")
                .as_deref(),
            Some("Ryzen HD Audio Controller Stereo Microphone")
        );
        assert_eq!(parse_pactl_description(sample, "non_existent"), None);
    }
}
