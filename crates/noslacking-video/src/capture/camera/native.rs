//! macOS and Windows cameras through `nokhwa` (AVFoundation, Media
//! Foundation), moved here from the app: its native parts (the
//! Objective-C shim on macOS, Media Foundation's bindings on Windows)
//! are the helper's, not the app's.
//!
//! The camera is asked for 640×480 at 30 a second in a raw format if it
//! has one (YUYV or NV12), MJPEG otherwise, and each frame becomes I420.
//! On macOS the system's camera permission is checked first, asking the
//! user the first time (the app bundle's `NSCameraUsageDescription`
//! words the question; the helper runs inside the bundle).

use std::time::{Duration, Instant};

use noslacking_video_ipc::{CaptureProblem, Planes, Source, SourceKind};

use super::convert;
use crate::capture::Trouble;

/// The formats taken from the camera, cheapest to read first.
const FORMATS: [nokhwa::utils::FrameFormat; 5] = [
    nokhwa::utils::FrameFormat::YUYV,
    nokhwa::utils::FrameFormat::NV12,
    nokhwa::utils::FrameFormat::MJPEG,
    nokhwa::utils::FrameFormat::RAWRGB,
    nokhwa::utils::FrameFormat::GRAY,
];

/// What a `nokhwa` error tells the app.
fn trouble(error: &nokhwa::NokhwaError) -> Trouble {
    let text = error.to_string();
    let lower = text.to_lowercase();
    // Windows says E_ACCESSDENIED (0x80070005) when its privacy settings
    // keep apps from the camera; elsewhere "denied" or "permission".
    let problem = if lower.contains("denied")
        || lower.contains("0x80070005")
        || lower.contains("permission")
    {
        CaptureProblem::Denied
    } else if lower.contains("busy") || lower.contains("in use") || lower.contains("0xc00d3704") {
        // MF_E_HW_MFT_FAILED_START_STREAMING: another app has it.
        CaptureProblem::Busy
    } else {
        CaptureProblem::Failed
    };
    Trouble::new(problem, text)
}

/// The id a listed camera goes by.
fn id_of(info: &nokhwa::utils::CameraInfo) -> String {
    format!("native:{}", info.index())
}

/// The cameras there are.
pub fn list() -> Result<Vec<Source>, Trouble> {
    let cameras =
        nokhwa::query(nokhwa::utils::ApiBackend::Auto).map_err(|error| trouble(&error))?;
    Ok(cameras
        .iter()
        .map(|info| Source {
            id: id_of(info),
            name: info.human_name(),
            kind: SourceKind::Camera,
        })
        .collect())
}

/// Asks macOS for the camera, once: the system asks the user the first
/// time. Elsewhere there is nothing to ask before opening.
#[cfg(target_os = "macos")]
fn allowed() -> Result<(), Trouble> {
    if nokhwa::nokhwa_check() {
        return Ok(());
    }
    let (said, answer) = std::sync::mpsc::channel();
    nokhwa::nokhwa_initialize(move |granted| {
        let _ = said.send(granted);
    });
    // The user may take a while to answer the system's question.
    match answer.recv_timeout(Duration::from_secs(60)) {
        Ok(true) => Ok(()),
        Ok(false) | Err(_) => Err(Trouble::new(
            CaptureProblem::Denied,
            "macOS did not allow the camera",
        )),
    }
}

#[cfg(not(target_os = "macos"))]
fn allowed() -> Result<(), Trouble> {
    Ok(())
}

/// An open camera, streaming.
pub struct NativeCamera {
    camera: nokhwa::Camera,
    name: String,
    taken: u64,
    unreadable: u64,
}

impl NativeCamera {
    /// Opens the camera `id` names (the first if none) and starts it.
    pub fn open(id: Option<&str>) -> Result<Self, Trouble> {
        use nokhwa::utils::{
            ApiBackend, CameraFormat, CameraIndex, RequestedFormat, RequestedFormatType, Resolution,
        };
        allowed()?;
        let index = match (id, nokhwa::query(ApiBackend::Auto)) {
            (_, Ok(cameras)) if cameras.is_empty() => {
                return Err(Trouble::new(CaptureProblem::Unavailable, "no camera"));
            }
            (None, Ok(cameras)) => cameras
                .first()
                .map_or(CameraIndex::Index(0), |info| info.index().clone()),
            // Listing failed: the first camera may still open.
            (None, Err(error)) => {
                eprintln!("noslacking-video: camera: could not list cameras: {error}");
                CameraIndex::Index(0)
            }
            (Some(id), Ok(cameras)) => cameras
                .iter()
                .find(|info| id_of(info) == id)
                .map(|info| info.index().clone())
                .ok_or_else(|| Trouble::new(CaptureProblem::Gone, format!("no camera {id}")))?,
            (Some(_), Err(error)) => return Err(trouble(&error)),
        };
        let wanted = CameraFormat::new(
            Resolution::new(640, 480),
            nokhwa::utils::FrameFormat::YUYV,
            30,
        );
        let request = RequestedFormat::with_formats(RequestedFormatType::Closest(wanted), &FORMATS);
        let mut camera = nokhwa::Camera::new(index, request).map_err(|error| trouble(&error))?;
        camera.open_stream().map_err(|error| trouble(&error))?;
        let format = camera.camera_format();
        let name = camera.info().human_name();
        eprintln!(
            "noslacking-video: camera: {name}: {}x{} {} at {} fps",
            format.width(),
            format.height(),
            format.format(),
            format.frame_rate()
        );
        Ok(Self {
            camera,
            name,
            taken: 0,
            unreadable: 0,
        })
    }

    /// Its name, for the log.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The next frame as I420 and when it was taken; none if it did not
    /// read.
    pub fn frame(&mut self) -> Result<Option<(Planes, Instant)>, Trouble> {
        let buffer = self.camera.frame().map_err(|error| trouble(&error))?;
        let at = Instant::now();
        match picture(&buffer) {
            Some(picture) => {
                self.taken += 1;
                Ok(Some((picture, at)))
            }
            None => {
                self.unreadable += 1;
                if self.unreadable == 1 {
                    eprintln!(
                        "noslacking-video: camera: a {} frame of {} bytes did not read",
                        buffer.source_frame_format(),
                        buffer.buffer().len()
                    );
                }
                Ok(None)
            }
        }
    }
}

impl Drop for NativeCamera {
    fn drop(&mut self) {
        // Stopped here rather than in nokhwa's drop, which panics if
        // stopping fails (and a panic aborts the helper).
        if let Err(error) = self.camera.stop_stream() {
            eprintln!("noslacking-video: camera: stopping: {error}");
        }
        eprintln!(
            "noslacking-video: camera: {} closed; {} frames taken, {} unreadable",
            self.name, self.taken, self.unreadable
        );
    }
}

/// One camera frame as I420.
fn picture(buffer: &nokhwa::Buffer) -> Option<Planes> {
    use nokhwa::utils::FrameFormat;
    let resolution = buffer.resolution();
    let (width, height) = (resolution.width(), resolution.height());
    let row = |bytes: u32| usize::try_from(width).ok()?.checked_mul(bytes as usize);
    let data = buffer.buffer();
    match buffer.source_frame_format() {
        FrameFormat::YUYV => convert::from_yuyv(data, width, height, row(2)?),
        FrameFormat::NV12 => convert::from_nv12(data, width, height, row(1)?),
        FrameFormat::MJPEG => convert::from_mjpeg(data),
        FrameFormat::RAWRGB => convert::from_rgb(data, width, height, row(3)?),
        FrameFormat::GRAY => convert::from_gray(data, width, height, row(1)?),
        FrameFormat::RAWBGR => None,
    }
}
