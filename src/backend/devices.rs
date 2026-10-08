//! The worker's side of [`crate::devices`]: listing the microphones and
//! speakers (cpal) and the cameras (the video helper's camera lane,
//! which lists them without opening one), each on a blocking thread, so
//! neither the interface nor the worker's loop waits for the system.

use super::{Event, Sink};
use crate::devices::{Device, Kind};
use crate::failure::{Failure, HuddleTrouble};

/// Lists the devices of `kind` and sends them to the interface.
pub async fn list(kind: Kind, sink: Sink) {
    let result = match tokio::task::spawn_blocking(move || listed(kind)).await {
        Ok(result) => result,
        Err(error) => {
            log::warn!("devices: listing {kind:?} failed: {error}");
            Err(Failure::Huddle(HuddleTrouble::NoDeviceList))
        }
    };
    sink.send(Event::Devices(crate::devices::Event::Listed {
        kind,
        result,
    }));
}

/// The devices of `kind` there are, or why not. Blocks.
fn listed(kind: Kind) -> Result<Vec<Device>, Failure> {
    match kind {
        Kind::Microphone | Kind::Speaker => {
            crate::huddle_audio::devices::list(kind).map_err(|error| {
                log::warn!("devices: no {kind:?} list: {error}");
                Failure::Huddle(HuddleTrouble::NoDeviceList)
            })
        }
        Kind::Camera => cameras(),
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

#[cfg(all(test, feature = "huddle-camera"))]
mod tests {
    use super::*;

    /// The camera lane's helper in tests is the helper's own code on a
    /// thread, whose camera is a pretend one: none is opened to list it.
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
}
