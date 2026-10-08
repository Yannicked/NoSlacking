//! Which camera, microphone and speaker a huddle uses: the choice the
//! settings remember, the devices there are now, and how one is found
//! among the other.
//!
//! The interface never touches a device. It asks the worker for the
//! devices there are ([`Command::List`], answered by [`Event::Listed`]:
//! the sound devices through cpal, the cameras through the video
//! helper, which lists them without turning one on), shows them in
//! pickers (Settings → Huddles, and the menus beside Mute and Video in
//! a call), and tells the worker what is chosen ([`Command::Use`]),
//! once at start and again on every change; a huddle going on switches
//! to it at once.
//!
//! A choice is remembered by the device's id and its name ([`Choice`]),
//! and found again by [`find`]:
//!
//! - Sound devices go by cpal's id (ALSA's PCM name such as
//!   `sysdefault:CARD=Audio`, Core Audio's device UID, the Windows
//!   endpoint id), which cpal keeps stable across reboots and replugging
//!   where the system does: the id first, then the name.
//! - Cameras go by the helper's id (`v4l2:/dev/video2`, `native:0`),
//!   which is only where the camera is now: plug cameras in another
//!   order, or reboot, and `/dev/video2` is another camera. So a
//!   camera is found by its name (the driver's card name, the system's
//!   camera name), the id telling apart two cameras of the same name
//!   while they stay where they were.
//!
//! A remembered device that is not there now is not forgotten: the
//! system's default (for a camera, the first) is used meanwhile, and the
//! picker shows the remembered one as "not connected", so plugging it
//! back in brings it back.

use crate::app::App;
use crate::backend;
use crate::failure::Failure;
use crate::i18n::{t, tf};

/// What kind of device.
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, serde::Serialize, serde::Deserialize,
)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    /// What you talk into.
    Microphone,
    /// What the huddle plays on: speakers or headphones.
    Speaker,
    /// What the others see you through.
    Camera,
}

impl Kind {
    /// Every kind this build can choose: the camera only with
    /// `huddle-camera`.
    pub fn all() -> &'static [Kind] {
        if cfg!(feature = "huddle-camera") {
            &[Kind::Microphone, Kind::Speaker, Kind::Camera]
        } else {
            &[Kind::Microphone, Kind::Speaker]
        }
    }

    /// Whether the system's id for a device of this kind stays the same
    /// across reboots and replugging (sound devices), or only says where
    /// it is now (cameras).
    pub fn stable_ids(self) -> bool {
        self != Kind::Camera
    }
}

/// A device there is now, as the worker lists it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Device {
    /// How the system finds it: cpal's device id, or the video helper's
    /// camera id.
    pub id: String,
    /// What a picker calls it.
    pub name: String,
}

/// A device chosen in a picker and remembered in the settings: its id
/// when chosen and its name, to find it again (see [`find`]).
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Choice {
    /// The id it had when chosen.
    pub id: String,
    /// Its name when chosen.
    pub name: String,
}

impl From<&Device> for Choice {
    fn from(device: &Device) -> Self {
        Self {
            id: device.id.clone(),
            name: device.name.clone(),
        }
    }
}

/// The devices chosen for huddles; none for a kind means the system's
/// default (for a camera, the first there is).
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct Chosen {
    /// The microphone.
    pub microphone: Option<Choice>,
    /// The speaker.
    pub speaker: Option<Choice>,
    /// The camera.
    pub camera: Option<Choice>,
}

impl Chosen {
    /// What is chosen for `kind`.
    pub fn get(&self, kind: Kind) -> Option<&Choice> {
        match kind {
            Kind::Microphone => self.microphone.as_ref(),
            Kind::Speaker => self.speaker.as_ref(),
            Kind::Camera => self.camera.as_ref(),
        }
    }

    /// Chooses `choice` for `kind`; whether that changed anything.
    pub fn set(&mut self, kind: Kind, choice: Option<Choice>) -> bool {
        let slot = match kind {
            Kind::Microphone => &mut self.microphone,
            Kind::Speaker => &mut self.speaker,
            Kind::Camera => &mut self.camera,
        };
        if *slot == choice {
            return false;
        }
        *slot = choice;
        true
    }
}

/// The device among `present` that `choice` of a `kind` names, if it is
/// there: the one with the same id and name; then, for sound devices,
/// the same id (a name can change with the system's language), and for
/// any device the same name (a camera's id is only where it is now; a
/// sound device's can change when a driver does).
pub fn find<'a>(kind: Kind, choice: &Choice, present: &'a [Device]) -> Option<&'a Device> {
    let exact = present
        .iter()
        .find(|d| d.id == choice.id && d.name == choice.name);
    let same_id = || {
        kind.stable_ids()
            .then(|| present.iter().find(|d| d.id == choice.id))
            .flatten()
    };
    let same_name = || present.iter().find(|d| d.name == choice.name);
    exact.or_else(same_id).or_else(same_name)
}

/// One line of a picker.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    /// What choosing it remembers: none for the system's default.
    pub choice: Option<Choice>,
    /// Its name; none for the system's default.
    pub name: Option<String>,
    /// Remembered but not there now: "(not connected)".
    pub absent: bool,
    /// The one in use.
    pub selected: bool,
}

/// The lines of a picker for `kind`: the system's default first, then
/// each device there is (`present`, none while not yet listed), and the
/// remembered `choice` last if it is known to be missing. The one in
/// use is selected: the remembered device where it is found, the
/// default otherwise, and a missing remembered one shows as chosen
/// still, so a glance tells why the default plays.
pub fn entries(kind: Kind, choice: Option<&Choice>, present: Option<&[Device]>) -> Vec<Entry> {
    let found = match (choice, present) {
        (Some(choice), Some(present)) => find(kind, choice, present),
        _ => None,
    };
    let mut lines = vec![Entry {
        choice: None,
        name: None,
        absent: false,
        selected: choice.is_none(),
    }];
    for device in present.unwrap_or_default() {
        lines.push(Entry {
            choice: Some(Choice::from(device)),
            name: Some(device.name.clone()),
            absent: false,
            selected: found == Some(device),
        });
    }
    if let Some(choice) = choice
        && found.is_none()
    {
        lines.push(Entry {
            choice: Some(choice.clone()),
            name: Some(choice.name.clone()),
            // Not listed yet: neither there nor missing as far as is
            // known.
            absent: present.is_some(),
            selected: true,
        });
    }
    lines
}

/// What the interface asks the worker about devices.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Command {
    /// List the devices of a kind there are now.
    List(Kind),
    /// Use these from now on, in a huddle going on too.
    Use(Chosen),
}

/// What the worker says about devices.
#[derive(Clone, Debug, PartialEq)]
pub enum Event {
    /// The devices of `kind` there are, or why they could not be listed.
    Listed {
        /// Which kind.
        kind: Kind,
        /// The devices, in the system's order.
        result: Result<Vec<Device>, Failure>,
    },
    /// The chosen device of `kind` would not open, so the system's
    /// default is used. Only the speaker does this, as it must play
    /// somewhere; a microphone or camera that will not open stays off.
    FellBack {
        /// Which kind.
        kind: Kind,
        /// Why the chosen one would not open.
        failure: Failure,
    },
}

/// What the views ask about devices.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Action {
    /// List the devices of a kind again (a picker opened).
    Refresh(Kind),
    /// Choose `choice` for `kind` (none: the system's default).
    Choose {
        /// Which kind.
        kind: Kind,
        /// The device, or none for the default.
        choice: Option<Choice>,
    },
}

/// The devices of one kind as the interface last heard of them.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Listing {
    /// The last list, or why there was none; none before the first.
    pub result: Option<Result<Vec<Device>, Failure>>,
    /// Asked for and not answered yet.
    pub asking: bool,
}

impl Listing {
    /// The devices last listed, if any were.
    pub fn devices(&self) -> Option<&[Device]> {
        match &self.result {
            Some(Ok(devices)) => Some(devices),
            _ => None,
        }
    }

    /// Why the last list failed, if it did.
    pub fn failure(&self) -> Option<&Failure> {
        match &self.result {
            Some(Err(failure)) => Some(failure),
            _ => None,
        }
    }
}

/// The interface's side of devices: the lists, by kind.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct State {
    /// The microphones.
    pub microphones: Listing,
    /// The speakers.
    pub speakers: Listing,
    /// The cameras.
    pub cameras: Listing,
}

impl State {
    /// The list of `kind`.
    pub fn listing(&self, kind: Kind) -> &Listing {
        match kind {
            Kind::Microphone => &self.microphones,
            Kind::Speaker => &self.speakers,
            Kind::Camera => &self.cameras,
        }
    }

    fn listing_mut(&mut self, kind: Kind) -> &mut Listing {
        match kind {
            Kind::Microphone => &mut self.microphones,
            Kind::Speaker => &mut self.speakers,
            Kind::Camera => &mut self.cameras,
        }
    }

    /// Notes that `kind` is asked for; whether to ask (not while an
    /// answer is awaited).
    pub fn ask(&mut self, kind: Kind) -> bool {
        let listing = self.listing_mut(kind);
        !std::mem::replace(&mut listing.asking, true)
    }

    /// Takes in a list.
    pub fn listed(&mut self, kind: Kind, result: Result<Vec<Device>, Failure>) {
        *self.listing_mut(kind) = Listing {
            result: Some(result),
            asking: false,
        };
    }
}

/// What a picker's default line says for `kind`.
pub fn default_label(kind: Kind) -> String {
    match kind {
        Kind::Camera => t("First camera"),
        Kind::Microphone | Kind::Speaker => t("System default"),
    }
    .into_owned()
}

/// What a picker line says.
pub fn label(kind: Kind, entry: &Entry) -> String {
    match &entry.name {
        None => default_label(kind),
        Some(name) if entry.absent => tf("{name} (not connected)", &[("name", name)]),
        Some(name) => name.clone(),
    }
}

/// The id a picker's popup in `place` goes by ("settings"), so the demo
/// can open it for its screenshot.
pub fn popup_id(place: &str, kind: Kind) -> egui::Id {
    egui::Id::new(("device-picker", place, kind))
}

/// The id of the menu beside Mute (`Kind::Microphone`) or Video
/// (`Kind::Camera`) in `place` (the call bar's, the call window's), as
/// [`popup_id`].
pub fn menu_id(place: &str, kind: Kind) -> egui::Id {
    egui::Id::new(("device-menu", place, kind))
}

/// Lists every kind again: the settings page opened.
pub fn refresh_all(app: &mut App) {
    for kind in Kind::all() {
        apply(app, Action::Refresh(*kind));
    }
}

/// Tells the worker what is chosen: at start, and after each change.
pub fn tell(app: &App) {
    app.backend.send(backend::Command::Devices(Command::Use(
        app.settings.devices.clone(),
    )));
}

/// Applies a view's request.
pub fn apply(app: &mut App, action: Action) {
    match action {
        Action::Refresh(kind) => {
            if app.devices.ask(kind) {
                app.backend
                    .send(backend::Command::Devices(Command::List(kind)));
            }
        }
        Action::Choose { kind, choice } => {
            if app.settings.devices.set(kind, choice) {
                app.settings_changed();
                tell(app);
            }
        }
    }
}

/// Takes in the worker's news of devices.
pub fn handle(app: &mut App, event: Event) {
    match event {
        Event::Listed { kind, result } => {
            if let Err(failure) = &result {
                log::info!("devices: {kind:?} not listed: {failure:?}");
            }
            app.devices.listed(kind, result);
        }
        Event::FellBack { kind, failure } => {
            let error = failure.message();
            let text = match kind {
                Kind::Speaker => tf(
                    "Your chosen speaker could not be used ({error}); playing on the system default",
                    &[("error", &error)],
                ),
                Kind::Microphone | Kind::Camera => tf(
                    "The device you chose could not be used ({error}); using the default",
                    &[("error", &error)],
                ),
            };
            app.toast(text, true);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn device(id: &str, name: &str) -> Device {
        Device {
            id: id.into(),
            name: name.into(),
        }
    }

    fn choice(id: &str, name: &str) -> Choice {
        Choice {
            id: id.into(),
            name: name.into(),
        }
    }

    #[test]
    fn a_camera_is_found_by_its_name_wherever_it_is_plugged() {
        let brio = choice("v4l2:/dev/video2", "Logitech BRIO");
        // As chosen.
        let present = [
            device("v4l2:/dev/video0", "Integrated Camera"),
            device("v4l2:/dev/video2", "Logitech BRIO"),
        ];
        assert_eq!(find(Kind::Camera, &brio, &present), Some(&present[1]));
        // Plugged in first after a reboot: another node, the same camera.
        let renumbered = [
            device("v4l2:/dev/video0", "Logitech BRIO"),
            device("v4l2:/dev/video2", "Integrated Camera"),
        ];
        assert_eq!(find(Kind::Camera, &brio, &renumbered), Some(&renumbered[0]));
        // Unplugged: its old node is another camera now, which is not it.
        let gone = [device("v4l2:/dev/video2", "Integrated Camera")];
        assert_eq!(find(Kind::Camera, &brio, &gone), None);
    }

    #[test]
    fn two_cameras_of_one_name_are_told_apart_by_where_they_are() {
        let second = choice("native:1", "USB Camera");
        let present = [
            device("native:0", "USB Camera"),
            device("native:1", "USB Camera"),
        ];
        assert_eq!(find(Kind::Camera, &second, &present), Some(&present[1]));
    }

    #[test]
    fn a_sound_device_is_found_by_its_id_then_its_name() {
        let usb = choice("alsa:sysdefault:CARD=Audio", "USB Audio");
        let present = [
            device("alsa:sysdefault:CARD=Generic_1", "HD-Audio Generic"),
            device("alsa:sysdefault:CARD=Audio", "USB-Audio"),
        ];
        // Renamed (another language, another driver): the id holds.
        assert_eq!(find(Kind::Speaker, &usb, &present), Some(&present[1]));
        // The same name under a new id: found by name.
        let moved = [device("alsa:sysdefault:CARD=Audio_1", "USB Audio")];
        assert_eq!(find(Kind::Microphone, &usb, &moved), Some(&moved[0]));
        assert_eq!(find(Kind::Microphone, &usb, &[]), None);
    }

    #[test]
    fn the_picker_lists_the_default_then_the_devices() {
        let present = [device("a", "Desk mic"), device("b", "Headset")];
        let lines = entries(Kind::Microphone, None, Some(&present));
        assert_eq!(lines.len(), 3);
        assert!(lines[0].selected && lines[0].choice.is_none());
        assert_eq!(lines[1].name.as_deref(), Some("Desk mic"));
        assert!(!lines[1].selected && !lines[2].selected);

        let headset = choice("b", "Headset");
        let lines = entries(Kind::Microphone, Some(&headset), Some(&present));
        assert_eq!(lines.len(), 3);
        assert!(!lines[0].selected);
        assert!(lines[2].selected && !lines[2].absent);
        assert_eq!(lines[2].choice.as_ref(), Some(&headset));
    }

    #[test]
    fn a_missing_device_is_shown_not_forgotten() {
        let headset = choice("b", "Headset");
        let present = [device("a", "Desk mic")];
        let lines = entries(Kind::Speaker, Some(&headset), Some(&present));
        assert_eq!(lines.len(), 3);
        let last = &lines[2];
        assert!(last.absent && last.selected);
        assert_eq!(last.choice.as_ref(), Some(&headset));
        assert!(!lines[0].selected, "the choice stays, the default plays");
        // Before the first list: shown as chosen, not yet as missing.
        let lines = entries(Kind::Speaker, Some(&headset), None);
        assert_eq!(lines.len(), 2);
        assert!(lines[1].selected && !lines[1].absent);
    }

    #[test]
    fn a_renumbered_camera_is_selected_in_its_new_place() {
        let brio = choice("v4l2:/dev/video2", "Logitech BRIO");
        let present = [device("v4l2:/dev/video0", "Logitech BRIO")];
        let lines = entries(Kind::Camera, Some(&brio), Some(&present));
        assert_eq!(lines.len(), 2, "no missing line for a camera found");
        assert!(lines[1].selected);
    }

    #[test]
    fn choosing_changes_only_its_kind_and_says_so() {
        let mut chosen = Chosen::default();
        assert!(chosen.set(Kind::Camera, Some(choice("native:0", "FaceTime HD"))));
        assert!(!chosen.set(Kind::Camera, Some(choice("native:0", "FaceTime HD"))));
        assert_eq!(
            chosen.get(Kind::Camera).map(|c| c.id.as_str()),
            Some("native:0")
        );
        assert_eq!(chosen.get(Kind::Microphone), None);
        assert!(chosen.set(Kind::Camera, None));
        assert_eq!(chosen, Chosen::default());
    }

    #[test]
    fn a_list_is_asked_for_once_until_it_comes() {
        let mut state = State::default();
        assert!(state.ask(Kind::Speaker));
        assert!(!state.ask(Kind::Speaker), "already asked");
        assert!(state.ask(Kind::Microphone), "another kind");
        state.listed(Kind::Speaker, Ok(vec![device("a", "Speakers")]));
        assert_eq!(state.speakers.devices().map(<[Device]>::len), Some(1));
        assert!(state.ask(Kind::Speaker), "asked again after the answer");
        state.listed(
            Kind::Camera,
            Err(Failure::Huddle(
                crate::failure::HuddleTrouble::CameraNeedsHelper,
            )),
        );
        assert!(state.cameras.failure().is_some());
        assert!(state.cameras.devices().is_none());
    }

    #[test]
    fn the_camera_is_chosen_only_where_it_can_be_sent() {
        assert_eq!(
            Kind::all().contains(&Kind::Camera),
            cfg!(feature = "huddle-camera")
        );
        assert!(Kind::Speaker.stable_ids() && !Kind::Camera.stable_ids());
    }
}
