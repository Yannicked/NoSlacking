//! NoSlacking's hardware video helper.
//!
//! The app starts `noslacking-video` the first time it would decode a
//! video stream on the GPU and talks to it over its standard input and
//! output in the messages of `noslacking-video-ipc`. The helper drives
//! the platform's video API through a [`backend::Backend`]: VA-API on
//! Linux ([`vaapi`]); on other systems, and where VA-API has no H.264
//! decoder, it reports no capabilities and the app decodes in software.
//!
//! Why a separate process: the platform APIs are C, so calling them
//! takes `unsafe` code the app forbids, and a GPU driver handed a
//! stranger's malformed stream may crash or hang. Here that costs only
//! the helper, which the app restarts a few times and then does without.

pub mod backend;
pub mod fake;
#[cfg(target_os = "linux")]
pub mod h264;
#[cfg(target_os = "linux")]
#[allow(unsafe_code)]
pub mod pipe;
pub mod server;
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
    // until then the app decodes in software there.
    Box::new(backend::Nothing::new(
        "none: no back end for this system yet",
    ))
}
