//! Ward: a Kubernetes-native antivirus and EDR. The operator turns a `Ward`
//! object into a node agent `DaemonSet` and an optional manager
//! `StatefulSet`, and drives signature database rollouts.

#![allow(
    clippy::redundant_pub_crate,
    clippy::significant_drop_tightening,
    clippy::significant_drop_in_scrutinee
)]

mod agent;
mod app;
mod content;
pub mod crd;
mod feed;
mod k8s;
mod manager;
mod observability;
mod operator;
mod policy;
mod rollout;
mod signature;
mod tls;

pub use app::start;
