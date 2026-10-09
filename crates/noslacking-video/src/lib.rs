//! NoSlacking's video helper: everything in a huddle that touches
//! pixels happens here.
//!
//! The app starts `noslacking-video` the first time it needs it and
//! talks to it over its standard input and output in the messages of
//! `noslacking-video-ipc`. The helper decodes every stream the app shows
//! on the GPU through the platform's video API, a [`backend::Backend`]
//! (VA-API on Linux, `vaapi`; VideoToolbox on macOS, [`videotoolbox`];
//! Media Foundation on Windows, [`mediafoundation`]; the last two built but
//! not yet tried on a Mac or a Windows GPU), when the app asks for it and
//! the GPU can; and otherwise, or once the GPU fails, in software
//! ([`software`]). On other systems, and where the platform's API has no
//! H.264 decoder, it reports no hardware and decodes everything in
//! software. It also captures and
//! encodes what the user sends ([`capture`], [`pipeline`]): the screen
//! they share and their camera ([`capture::camera`]), on the GPU when it
//! can and in software otherwise. The app gets only the H.264 to send
//! and, for the camera, a small picture for its self-view.
//!
//! Why a separate process: the platform APIs are C, so calling them
//! takes `unsafe` code the app forbids, and a GPU driver or a decoder
//! handed a stranger's malformed stream may crash or hang (release
//! builds abort on a panic). Here that costs only the helper, which the
//! app restarts a few times and then shows no video without. Capture's
//! native dependencies (V4L2's and PipeWire's calls, the portal's file
//! descriptors, nokhwa's Objective-C shim) stay out of the app's build.

pub mod backend;
pub mod blur;
pub mod capture;
pub mod fake;
#[cfg(target_os = "linux")]
pub mod h264;
pub mod mediafoundation;
pub mod nal;
#[cfg(target_os = "linux")]
#[allow(unsafe_code)]
pub mod pipe;
pub mod pipeline;
pub mod server;
pub mod shrink;
pub mod software;
pub mod software_encoder;
#[cfg(target_os = "linux")]
pub mod vaapi;
pub mod videotoolbox;

/// The back end this system has, or one that can do nothing: the
/// `NOSLACKING_VIDEO_BACKEND` environment variable may name one
/// (`fake`, `none`), else the platform's (`vaapi`, `videotoolbox`,
/// `mediafoundation`) is tried.
pub fn choose_backend() -> Box<dyn backend::Backend> {
    let wanted = std::env::var("NOSLACKING_VIDEO_BACKEND").unwrap_or_default();
    match wanted.as_str() {
        "none" => return Box::new(backend::Nothing::new("none: turned off")),
        "fake" => return Box::new(fake::Fake),
        _ => {}
    }
    platform_backend()
}

#[cfg(target_os = "linux")]
fn platform_backend() -> Box<dyn backend::Backend> {
    match vaapi::Vaapi::open() {
        Ok(backend) => Box::new(backend),
        Err(why) => Box::new(backend::Nothing::new(&format!("none: {why}"))),
    }
}

#[cfg(target_os = "macos")]
fn platform_backend() -> Box<dyn backend::Backend> {
    match videotoolbox::VideoToolbox::open() {
        Ok(backend) => Box::new(backend),
        Err(why) => Box::new(backend::Nothing::new(&format!("none: {why}"))),
    }
}

#[cfg(windows)]
fn platform_backend() -> Box<dyn backend::Backend> {
    match mediafoundation::MediaFoundation::open() {
        Ok(backend) => Box::new(backend),
        Err(why) => Box::new(backend::Nothing::new(&format!("none: {why}"))),
    }
}

#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
fn platform_backend() -> Box<dyn backend::Backend> {
    // No video API is spoken elsewhere: the helper decodes in software.
    Box::new(backend::Nothing::new(
        "none: no back end for this system yet",
    ))
}
