//! Slack: the Web API, Socket Mode and their JSON.

pub mod client;
pub mod magic;
pub mod rtm;
pub mod session;
pub mod socket;
pub mod types;

pub use client::{Client, OauthApp, SlackError, Token};
