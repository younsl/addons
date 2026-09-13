//! Kubernetes API usage: the Lease behind leader election, the `ConfigMap` that
//! persists controller state, and the Events that form the audit trail.

pub mod events;
pub mod leader;
pub mod state;
pub mod time;

pub use events::Emitter;
pub use state::{Approval, InFlight, Phase, Snapshot, Store};
