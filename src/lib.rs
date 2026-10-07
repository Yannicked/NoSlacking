//! NoSlacking: a native Slack client built on egui and fastframe.
//!
//! The interface (`ui`, [`app`]) runs on the main thread; Slack's Web API
//! and Socket Mode run on a small tokio runtime in [`backend`]. The two
//! talk through commands and events only.

pub mod app;
pub mod audio;
pub mod auth;
pub mod autostart;
pub mod backend;
pub mod badge;
pub mod convos;
pub mod credentials;
pub mod custom_emoji;
#[cfg(feature = "demo")]
pub mod demo;
pub mod desktop;
pub mod dnd;
pub mod drafts;
pub mod emoji;
pub mod failure;
#[cfg(feature = "highlight")]
pub mod highlight;
pub mod hooks;
#[cfg(feature = "huddle-audio")]
pub mod huddle_audio;
#[cfg(feature = "huddle-audio")]
pub mod huddle_mic;
pub mod huddles;
pub mod i18n;
pub mod images;
pub mod jump;
pub mod lightbox;
pub mod links;
pub mod model;
pub mod mrkdwn;
pub mod notice;
pub mod notify;
pub mod offline;
pub mod palette;
pub mod paste;
pub mod paths;
pub mod people;
pub mod percent;
pub mod quotes;
pub mod redact;
pub mod revision;
pub mod scopes;
pub mod search;
pub mod settings;
pub mod share;
pub mod sidebar;
pub mod single_instance;
pub mod slack;
pub mod slack_links;
pub mod slash;
pub mod spell;
#[cfg(feature = "teams")]
pub mod teams;
pub mod theme;
pub mod tray;
mod ui;
pub mod viewer;
pub mod views;
