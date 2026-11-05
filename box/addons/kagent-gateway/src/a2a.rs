//! Minimal client for the kagent A2A JSON-RPC endpoint.

pub mod client;
pub mod error;

pub use client::{AgentClient, Client, Progress, ProgressHook, Request};
pub use error::Error;
