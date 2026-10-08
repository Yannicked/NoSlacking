//! Capture on macOS and Windows through `xcap`: screens and windows by
//! name for the app's picker, each picture asked for at the share's rate
//! (CoreGraphics through `objc2` on macOS, GDI and DXGI through the
//! `windows` crate on Windows; neither compiles C).
//!
//! On macOS the app must be allowed to record the screen (System
//! Settings → Privacy & Security → Screen & System Audio Recording); the
//! helper, started by the app from inside its bundle, is covered by the
//! app's permission. Until it is allowed, captures fail or come back
//! empty, which is reported as [`CaptureProblem::Denied`].

use noslacking_video_ipc::{CaptureProblem, ShareChoice, Source, SourceKind};

use super::{Frame, Order, Packed, Trouble};
use crate::pipeline::{Capture, Settings};

/// The screens, then the windows that have a title and are not
/// minimised.
pub fn sources() -> Result<Vec<Source>, Trouble> {
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
        return Err(Trouble::new(
            CaptureProblem::Unavailable,
            "xcap found no screen or window",
        ));
    }
    Ok(sources)
}

/// Captures the screen or window `choice` names.
pub fn start(choice: &ShareChoice, settings: Settings) -> Result<Capture, Trouble> {
    let id = match choice {
        ShareChoice::Source(id) => id.clone(),
        _ => sources()?
            .first()
            .map(|s| s.id.clone())
            .ok_or_else(|| Trouble::new(CaptureProblem::Gone, "nothing to share"))?,
    };
    allowed()?;
    super::spawn(
        "noslacking-share-xcap",
        settings,
        move |pipeline, inbox, _feed, started| {
            // Found again on this thread: the handles need not cross it.
            let target = match find(&id) {
                Ok(target) => target,
                Err(trouble) => {
                    let _ = started.send(Err(trouble));
                    return;
                }
            };
            // The first picture says whether capture works at all.
            if let Err(error) = target.grab() {
                let _ = started.send(Err(error_of(&error)));
                return;
            }
            let _ = started.send(Ok(()));
            super::poll(
                pipeline,
                &inbox,
                || target.grab().map_err(|e| e.to_string()),
                |pipeline, (width, height, data), at| {
                    pipeline.put(
                        &Frame::Packed(Packed {
                            width: *width,
                            height: *height,
                            stride: *width as usize * 4,
                            order: Order::Rgba,
                            data,
                        }),
                        at,
                    );
                },
            );
        },
    )
}

/// What is captured.
enum Target {
    Monitor(xcap::Monitor),
    Window(xcap::Window),
}

impl Target {
    fn grab(&self) -> Result<(u32, u32, Vec<u8>), xcap::XCapError> {
        let image = match self {
            Self::Monitor(monitor) => monitor.capture_image()?,
            Self::Window(window) => window.capture_image()?,
        };
        let (width, height) = (image.width(), image.height());
        Ok((width, height, image.into_raw()))
    }
}

/// The screen or window a picker's id names.
fn find(id: &str) -> Result<Target, Trouble> {
    let gone = || Trouble::new(CaptureProblem::Gone, format!("no {id}"));
    if let Some(wanted) = id
        .strip_prefix("screen:")
        .and_then(|n| n.parse::<u32>().ok())
    {
        let monitors = xcap::Monitor::all().map_err(|e| failure(&e))?;
        return monitors
            .into_iter()
            .find(|m| m.id().is_ok_and(|id| id == wanted))
            .map(Target::Monitor)
            .ok_or_else(gone);
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
            .ok_or_else(gone);
    }
    Err(gone())
}

fn failure(error: &xcap::XCapError) -> Trouble {
    Trouble::failed(error.to_string())
}

/// A failed first capture: on macOS, most likely the permission.
fn error_of(error: &xcap::XCapError) -> Trouble {
    eprintln!("noslacking-video: share: the first capture failed: {error}");
    if cfg!(target_os = "macos") {
        Trouble::new(CaptureProblem::Denied, error.to_string())
    } else {
        failure(error)
    }
}

/// Whether macOS lets the app record the screen; asks once if it was
/// never asked (the system's own prompt).
#[cfg(target_os = "macos")]
fn allowed() -> Result<(), Trouble> {
    use objc2_core_graphics::{CGPreflightScreenCaptureAccess, CGRequestScreenCaptureAccess};
    if CGPreflightScreenCaptureAccess() {
        return Ok(());
    }
    // Shows the system's prompt the first time; later it only says no
    // until the user allows the app in System Settings and restarts it.
    if CGRequestScreenCaptureAccess() {
        Ok(())
    } else {
        Err(Trouble::new(
            CaptureProblem::Denied,
            "Screen Recording is not allowed",
        ))
    }
}

#[cfg(not(target_os = "macos"))]
fn allowed() -> Result<(), Trouble> {
    Ok(())
}
