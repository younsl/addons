//! Direct-messages the approvers, collects their approve/deny decision, and
//! streams progress back into the same thread. Clicks arrive over Socket Mode,
//! an outbound WebSocket, so nothing inbound is exposed.

pub mod blocks;
pub mod client;
pub mod detected;
pub mod level;
pub mod socket;

pub use blocks::{Proposal, approval_blocks, resolved_blocks};
pub use client::{Approver, Client, MessageRef};
pub use detected::{Detected, DetectedTunnel, detected_blocks};
pub use level::{Level, Notice, label};
pub use socket::Interaction;
