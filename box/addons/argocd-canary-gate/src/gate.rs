//! The pure verdict rules and the domain types they reason about.

pub mod evaluate;
pub mod types;

pub use evaluate::{decide, lookup_failed};
pub use types::{AppSnapshot, Decision, RolloutSnapshot, RolloutState};

// Reached through `Decision.code` in production code, imported by name only in
// tests.
#[cfg(test)]
pub use types::Code;
