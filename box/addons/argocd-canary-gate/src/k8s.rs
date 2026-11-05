//! Kubernetes reads: the Application from the admission request and the
//! Rollouts from the API.

pub mod application;
pub mod rollout;

pub use application::{AppReader, KubeAppReader, nested, nested_str, snapshot_from_value};
pub use rollout::{KubeRolloutReader, RolloutReader};

// Constructed directly only by the test doubles.
#[cfg(test)]
pub use rollout::ReadError;
