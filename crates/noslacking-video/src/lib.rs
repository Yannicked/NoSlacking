//! NoSlacking's video helper: every huddle video stream the app shows
//! is decoded here.
//!
//! The app starts `noslacking-video` the first time it decodes a video
//! stream and talks to it over its standard input and output in the
//! messages of `noslacking-video-ipc`. The helper decodes on the GPU
//! through the platform's video API, a [`backend::Backend`] (VA-API on
//! Linux, `vaapi`), when the app asks for it and the GPU can; and
//! otherwise, or once the GPU fails, in software ([`software`]). On
//! other systems, and where VA-API has no H.264 decoder, it reports no
//! hardware and decodes everything in software. It also encodes the
//! app's camera on the GPU when it can (the app encodes in software
//! itself otherwise), and captures and encodes the screen the user
//! shares ([`capture`], [`share`]): the app gets only the H.264 to send.
//!
//! Why a separate process: the platform APIs are C, so calling them
//! takes `unsafe` code the app forbids, and a GPU driver or a decoder
//! handed a stranger's malformed stream may crash or hang (release
//! builds abort on a panic). Here that costs only the helper, which the
//! app restarts a few times and then shows no video without.

pub mod backend;
pub mod capture;
pub mod fake;
#[cfg(target_os = "linux")]
pub mod h264;
pub mod nal;
#[cfg(target_os = "linux")]
#[allow(unsafe_code)]
pub mod pipe;
pub mod server;
pub mod share;
pub mod shrink;
pub mod software;
pub mod software_encoder;
#[cfg(target_os = "linux")]
pub mod vaapi;

/// The back end this system has, or one that can do nothing: the
/// `NOSLACKING_VIDEO_BACKEND` environment variable may name one
/// (`vaapi`, `fake`, `none`), else the platform's is tried.
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

#[cfg(not(target_os = "linux"))]
fn platform_backend() -> Box<dyn backend::Backend> {
    // VideoToolbox (macOS) and Media Foundation (Windows) are planned;
    // until then the helper decodes in software there.
    Box::new(backend::Nothing::new(
        "none: no back end for this system yet",
    ))
}
