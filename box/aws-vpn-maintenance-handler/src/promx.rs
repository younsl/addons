//! Queries a Prometheus-compatible HTTP API (Prometheus, Mimir, Thanos) so the
//! controller can judge how much traffic a tunnel is actually carrying before
//! it replaces one.
//!
//! The cron window says when maintenance is allowed. This says whether now is
//! actually a quiet moment, which a fixed schedule cannot know: a 02:00 window
//! is only low-impact until a batch job moves.

pub mod client;
pub mod gate;
pub mod profile;
pub mod quiet;

pub use client::{Client, ClientConfig};
pub use gate::{Assessment, Gate, Vars};
