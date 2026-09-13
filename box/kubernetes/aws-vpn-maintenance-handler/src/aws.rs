//! Wraps the AWS SDK with the narrow set of Site-to-Site VPN operations this
//! controller needs, and centralizes credential resolution.

pub mod client;
pub mod gateways;
pub mod maintenance;
pub mod preflight;
pub mod types;
pub mod vpn;

pub use client::Client;
pub use types::{Connection, DiscoverInput, Maintenance, TagFilter, Tunnel, TunnelStatus};
