//! Reads the two kinds of state a verdict needs: the live Application status,
//! which the Kubernetes API already holds, and the images a pending sync would
//! deploy, which only Argo CD knows.

pub mod api;
pub mod application;

pub use api::{DesiredImageClient, ImageResolver};
pub use application::{AppReader, KubeReader, snapshot_from_value};
