//! Slack: the Web API client the gateway posts through and the Socket Mode
//! connection it receives mentions on.

pub mod client;
pub mod socket;

pub use client::{Client, Error, Message, SlackClient, normalize_channel, truncate};
