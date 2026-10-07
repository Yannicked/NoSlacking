//! Capture on macOS and Windows through `xcap`: screens and windows by
//! name for the picker, each picture asked for at [`super::FPS`] (CoreGraphics
//! through `objc2` on macOS, GDI and DXGI through the `windows` crate on
//! Windows; neither compiles C).
//!
//! On macOS the app must be allowed to record the screen (System
//! Settings → Privacy & Security → Screen & System Audio Recording); until
//! it is, captures fail or come back empty, which is reported as
//! [`ShareError::Denied`].

use super::{
    Capturing, Choice, Ended, Frames, Order, Packed, Picture, ShareError, Source, SourceKind,
};

/// `xcap`.
#[derive(Debug)]
pub struct Xcap {
    frames: Frames,
}

impl Xcap {
    /// The system's screens and windows, their frames going to `frames`.
    pub fn new(frames: Frames) -> Self {
        Self { frames }
    }

    /// The screens, then the windows that have a title and are not
    /// minimised.
    pub fn sources(&mut self) -> Result<Vec<Source>, ShareError> {
        let monitors = xcap::Monitor::all().map_err(|e| failure(&e))?;
        let mut sources = Vec::new();
        for (n, monitor) in monitors.iter().enumerate() {
            let Ok(id) = monitor.id() else {
                continue;
            };
            // On macOS xcap's friendly name asks AppKit's NSScreen, which
            // belongs to the main thread; this runs on another.
            let named = if cfg!(target_os = "macos") {
                monitor.name()
            } else {
                monitor.friendly_name().or_else(|_| monitor.name())
            };
            let size = match (monitor.width(), monitor.height()) {
                (Ok(w), Ok(h)) => format!(" ({w}×{h})"),
                _ => String::new(),
            };
            let name = named
                .ok()
                .filter(|n| !n.is_empty())
                .unwrap_or_else(|| format!("Screen {}", n + 1))
                + &size;
            sources.push(Source {
                id: format!("screen:{id}"),
                name,
                kind: SourceKind::Screen,
            });
        }
        for window in xcap::Window::all().unwrap_or_default() {
            if window.is_minimized().unwrap_or(true) {
                continue;
            }
            let (Ok(id), Ok(title)) = (window.id(), window.title()) else {
                continue;
            };
            if title.trim().is_empty() {
                continue;
            }
            let app = window.app_name().unwrap_or_default();
            let name = if app.is_empty() || title.contains(&app) {
                title
            } else {
                format!("{title} — {app}")
            };
            sources.push(Source {
                id: format!("window:{id}"),
                name,
                kind: SourceKind::Window,
            });
        }
        if sources.is_empty() {
            return Err(ShareError::Unavailable);
        }
        Ok(sources)
    }

    /// Captures the screen or window `choice` names.
    pub fn start(&mut self, choice: &Choice, ended: Ended) -> Result<Capturing, ShareError> {
        let id = match choice {
            Choice::Source(id) => id.clone(),
            Choice::System { .. } => self
                .sources()?
                .first()
                .map(|s| s.id.clone())
                .ok_or(ShareError::Gone)?,
        };
        allowed()?;
        let frames = self.frames.clone();
        let running = super::spawn_capture("noslacking-share-xcap", move |stop, started| {
            // Found again on this thread: the handles need not cross it.
            let target = match find(&id) {
                Ok(target) => target,
                Err(error) => {
                    let _ = started.send(Err(error));
                    return;
                }
            };
            // The first picture says whether capture works at all.
            if let Err(error) = target.grab() {
                let _ = started.send(Err(error_of(&error)));
                return;
            }
            let _ = started.send(Ok(()));
            super::poll_frames(stop, &frames, &ended, || {
                target.grab().map_err(|e| e.to_string())
            });
        })?;
        Ok(Capturing::new(running, None))
    }
}

/// What is captured.
enum Target {
    Monitor(xcap::Monitor),
    Window(xcap::Window),
}

impl Target {
    fn grab(&self) -> Result<Picture, xcap::XCapError> {
        let image = match self {
            Self::Monitor(monitor) => monitor.capture_image()?,
            Self::Window(window) => window.capture_image()?,
        };
        let (width, height) = (image.width() as usize, image.height() as usize);
        Ok(Picture::Packed(Packed {
            width,
            height,
            stride: width * 4,
            order: Order::Rgba,
            data: image.into_raw(),
        }))
    }
}

/// The screen or window a picker's id names.
fn find(id: &str) -> Result<Target, ShareError> {
    if let Some(wanted) = id
        .strip_prefix("screen:")
        .and_then(|n| n.parse::<u32>().ok())
    {
        let monitors = xcap::Monitor::all().map_err(|e| failure(&e))?;
        return monitors
            .into_iter()
            .find(|m| m.id().is_ok_and(|id| id == wanted))
            .map(Target::Monitor)
            .ok_or(ShareError::Gone);
    }
    if let Some(wanted) = id
        .strip_prefix("window:")
        .and_then(|n| n.parse::<u32>().ok())
    {
        let windows = xcap::Window::all().map_err(|e| failure(&e))?;
        return windows
            .into_iter()
            .find(|w| w.id().is_ok_and(|id| id == wanted))
            .map(Target::Window)
            .ok_or(ShareError::Gone);
    }
    Err(ShareError::Gone)
}

fn failure(error: &xcap::XCapError) -> ShareError {
    ShareError::Failed(error.to_string())
}

/// A failed first capture: on macOS, most likely the permission.
fn error_of(error: &xcap::XCapError) -> ShareError {
    log::warn!("huddle share: the first capture failed: {error}");
    if cfg!(target_os = "macos") {
        ShareError::Denied
    } else {
        failure(error)
    }
}

/// Whether macOS lets the app record the screen; asks once if it was
/// never asked (the system's own prompt).
#[cfg(target_os = "macos")]
fn allowed() -> Result<(), ShareError> {
    use objc2_core_graphics::{CGPreflightScreenCaptureAccess, CGRequestScreenCaptureAccess};
    if CGPreflightScreenCaptureAccess() {
        return Ok(());
    }
    // Shows the system's prompt the first time; later it only says no
    // until the user allows the app in System Settings and restarts it.
    if CGRequestScreenCaptureAccess() {
        Ok(())
    } else {
        Err(ShareError::Denied)
    }
}

#[cfg(not(target_os = "macos"))]
fn allowed() -> Result<(), ShareError> {
    Ok(())
}
