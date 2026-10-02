//! NoSlacking: a native Slack client built on egui and fastframe.
//!
//! The interface ([`ui`], [`app`]) runs on the main thread; Slack's Web API
//! and Socket Mode run on a small tokio runtime in [`backend`]. The two
//! talk through commands and events only.

pub mod app;
pub mod auth;
pub mod autostart;
pub mod backend;
pub mod badge;
pub mod convos;
pub mod credentials;
#[cfg(feature = "demo")]
pub mod demo;
pub mod desktop;
pub mod dnd;
pub mod drafts;
pub mod emoji;
#[cfg(feature = "highlight")]
pub mod highlight;
pub mod i18n;
pub mod images;
pub mod jump;
pub mod links;
pub mod model;
pub mod mrkdwn;
pub mod notify;
pub mod paste;
pub mod paths;
pub mod redact;
pub mod search;
pub mod settings;
pub mod sidebar;
pub mod single_instance;
pub mod slack;
pub mod slash;
pub mod theme;
pub mod tray;
mod ui;
pub mod views;
