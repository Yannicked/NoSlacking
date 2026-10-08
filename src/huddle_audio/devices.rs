//! The microphones and speakers there are, through cpal (rodio's), and
//! the one a [`Choice`] names.
//!
//! Listing opens nothing: cpal reads ALSA's hints on Linux without
//! opening a PCM, and asks Core Audio and WASAPI for their endpoints.
//! It can still take a moment, so it runs on a blocking thread, never
//! the interface's.
//!
//! ALSA lists every way to reach every card: `front:`, `surround51:`,
//! `iec958:`, `hw:`, `plughw:`, `dmix:` and the rest, forty lines on an
//! ordinary laptop, most of them the same card. A picker shows one per
//! card (`sysdefault:CARD=…`, the card as the system set it up) and one
//! per HDMI or DisplayPort output (`hdmi:CARD=…,DEV=…`, named after the
//! screen); the system's default and its sound server's own devices
//! (`default`, `pipewire`, `pulse`) are the picker's "System default".
//! A card opened this way is opened directly, not through PipeWire or
//! PulseAudio: one the sound server is playing on may be busy, which
//! the speaker answers by playing on the default instead.

use rodio::cpal::traits::{DeviceTrait as _, HostTrait as _};

use crate::devices::{Choice, Device, Kind, find};

/// The ALSA PCMs a picker shows, by their prefix: one per card, and one
/// per digital screen output.
const ALSA_SHOWN: [&str; 2] = ["sysdefault:CARD=", "hdmi:CARD="];
/// What a picker shows of ALSA when none of [`ALSA_SHOWN`] is there (an
/// unusual configuration): each card's own device.
const ALSA_FALLBACK: &str = "plughw:CARD=";

/// Whether an ALSA PCM (its id without the `alsa:` host) is one of the
/// picker's: see [`ALSA_SHOWN`].
fn alsa_shown(pcm: &str) -> bool {
    ALSA_SHOWN.iter().any(|prefix| pcm.starts_with(prefix))
}

/// The devices of `all` a picker shows, by their cpal ids: every one
/// outside ALSA; of ALSA's, see [`ALSA_SHOWN`] and [`ALSA_FALLBACK`].
fn shown(all: &[String]) -> Vec<bool> {
    let pcm = |id: &String| id.strip_prefix("alsa:").map(str::to_owned);
    let any_shown = all.iter().filter_map(pcm).any(|p| alsa_shown(&p));
    all.iter()
        .map(|id| match pcm(id) {
            None => true,
            Some(p) if any_shown => alsa_shown(&p),
            Some(p) => p.starts_with(ALSA_FALLBACK),
        })
        .collect()
}

/// A device's name as a picker shows it: ALSA's "Card, Device" with the
/// device's part dropped when it only says the card's again ("USB Audio,
/// USB Audio").
fn tidy(name: &str) -> String {
    match name.split_once(", ") {
        Some((card, device)) if card == device => card.to_owned(),
        _ => name.to_owned(),
    }
}

/// `names`, made different where two are the same (two screens of one
/// model): "U27B3A", "U27B3A (2)".
fn numbered(names: Vec<String>) -> Vec<String> {
    let mut out: Vec<String> = Vec::with_capacity(names.len());
    for name in names {
        let before = out
            .iter()
            .filter(|n| **n == name || n.starts_with(&format!("{name} (")))
            .count();
        out.push(if before == 0 {
            name
        } else {
            format!("{name} ({})", before + 1)
        });
    }
    out
}

/// The devices of `kind` a picker shows, each with cpal's handle.
fn present(kind: Kind) -> Result<Vec<(Device, rodio::cpal::Device)>, String> {
    let host = rodio::cpal::default_host();
    let devices: Vec<rodio::cpal::Device> = match kind {
        Kind::Microphone => host.input_devices().map_err(|e| e.to_string())?.collect(),
        Kind::Speaker => host.output_devices().map_err(|e| e.to_string())?.collect(),
        Kind::Camera => return Err("cameras are the video helper's".into()),
    };
    let mut found = Vec::new();
    for device in devices {
        let Ok(id) = device.id() else {
            continue;
        };
        let name = device
            .description()
            .map(|d| tidy(d.name()))
            .unwrap_or_else(|_| id.1.clone());
        found.push((id.to_string(), name, device));
    }
    let ids: Vec<String> = found.iter().map(|(id, _, _)| id.clone()).collect();
    let keep = shown(&ids);
    let kept: Vec<_> = found
        .into_iter()
        .zip(keep)
        .filter_map(|(found, keep)| keep.then_some(found))
        .collect();
    let names = numbered(kept.iter().map(|(_, name, _)| name.clone()).collect());
    Ok(kept
        .into_iter()
        .zip(names)
        .map(|((id, _, device), name)| (Device { id, name }, device))
        .collect())
}

/// The microphones (`Kind::Microphone`) or speakers (`Kind::Speaker`)
/// there are, in the system's order. Blocks.
pub fn list(kind: Kind) -> Result<Vec<Device>, String> {
    present(kind).map(|devices| devices.into_iter().map(|(d, _)| d).collect())
}

/// The device of `kind` that `choice` names, with its name for the log;
/// none for the system's default: no choice, or a chosen device that is
/// not there now (logged). Blocks.
pub fn chosen(kind: Kind, choice: Option<&Choice>) -> Option<(rodio::cpal::Device, String)> {
    let choice = choice?;
    let present = match present(kind) {
        Ok(present) => present,
        Err(error) => {
            log::warn!("huddle audio: no {kind:?} list ({error}); using the default");
            return None;
        }
    };
    let devices: Vec<Device> = present.iter().map(|(d, _)| d.clone()).collect();
    let Some(device) = find(kind, choice, &devices) else {
        log::info!(
            "huddle audio: the chosen {kind:?} {:?} is not connected; using the default",
            choice.name
        );
        return None;
    };
    present
        .into_iter()
        .find(|(d, _)| d == device)
        .map(|(d, handle)| (handle, d.name))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn alsa_shows_one_line_per_card_and_screen() {
        let ids: Vec<String> = [
            "alsa:null",
            "alsa:sysdefault",
            "alsa:pipewire",
            "alsa:default",
            "alsa:sysdefault:CARD=Audio",
            "alsa:front:CARD=Audio,DEV=0",
            "alsa:surround51:CARD=Audio,DEV=0",
            "alsa:iec958:CARD=Audio,DEV=0",
            "alsa:hdmi:CARD=Generic,DEV=0",
            "alsa:hdmi:CARD=Generic,DEV=1",
            "alsa:sysdefault:CARD=Generic_1",
            "alsa:dmix:CARD=Generic_1,DEV=0",
            "alsa:hw:CARD=Generic_1,DEV=0",
            "alsa:plughw:CARD=Generic_1,DEV=0",
        ]
        .map(String::from)
        .into();
        let kept: Vec<&str> = ids
            .iter()
            .zip(shown(&ids))
            .filter_map(|(id, keep)| keep.then_some(id.as_str()))
            .collect();
        assert_eq!(
            kept,
            [
                "alsa:sysdefault:CARD=Audio",
                "alsa:hdmi:CARD=Generic,DEV=0",
                "alsa:hdmi:CARD=Generic,DEV=1",
                "alsa:sysdefault:CARD=Generic_1",
            ]
        );
    }

    #[test]
    fn without_sysdefault_alsa_shows_each_cards_own_device() {
        let ids: Vec<String> = [
            "alsa:default",
            "alsa:hw:CARD=0,DEV=0",
            "alsa:plughw:CARD=0,DEV=0",
        ]
        .map(String::from)
        .into();
        assert_eq!(shown(&ids), [false, false, true]);
    }

    #[test]
    fn other_systems_show_every_device() {
        let ids: Vec<String> = [
            "coreaudio:BuiltInSpeakerDevice",
            "wasapi:{0.0.0.00000000}.{abc}",
        ]
        .map(String::from)
        .into();
        assert_eq!(shown(&ids), [true, true]);
    }

    #[test]
    fn names_say_the_card_once_and_tell_twins_apart() {
        assert_eq!(tidy("USB Audio, USB Audio"), "USB Audio");
        assert_eq!(
            tidy("HD-Audio Generic, ALC287 Analog"),
            "HD-Audio Generic, ALC287 Analog"
        );
        assert_eq!(
            numbered(vec!["U27B3A".into(), "Analog".into(), "U27B3A".into()]),
            ["U27B3A", "Analog", "U27B3A (2)"]
        );
    }
}
