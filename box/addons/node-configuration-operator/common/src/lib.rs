//! What `node-configuration-operator` and `node-configuration-agent` share:
//! the custom resource types, the revision hash, and the process plumbing of
//! a long-running service.

pub mod boot;
pub mod crd;
pub mod payload;
pub mod telemetry;
