//! Microsoft Teams client, types, authentication, and protocols.
//!
//! Provides the data structures, HTTP client, Trouter WebSocket handling,
//! and HTML message translation for interacting with Microsoft Teams.

pub mod auth;
pub mod client;
pub mod html;
pub mod probe;
pub mod socket;
pub mod types;

pub use auth::{TEAMS_CLIENT_ID, TeamsCredentials};
pub use client::TeamsClient;
