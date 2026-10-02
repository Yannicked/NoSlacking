//! NoSlacking: a native Slack client built on egui and fastframe.
//!
//! The interface ([`ui`], [`app`]) runs on the main thread; Slack's Web API
//! and Socket Mode run on a small tokio runtime in [`backend`]. The two
//! talk through commands and events only.

pub mod app;
pub mod auth;
pub mod backend;
pub mod credentials;
#[cfg(feature = "demo")]
pub mod demo;
pub mod emoji;
#[cfg(feature = "highlight")]
pub mod highlight;
pub mod i18n;
pub mod images;
pub mod model;
pub mod mrkdwn;
pub mod paths;
pub mod redact;
pub mod settings;
pub mod sidebar;
pub mod single_instance;
pub mod slack;
pub mod theme;
mod ui;
