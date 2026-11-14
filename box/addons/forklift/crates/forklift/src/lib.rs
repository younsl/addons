//! Forklift is a lightweight, Kubernetes-native artifact repository.
//!
//! Domain modules share one crate. `bin/forklift` wires the server. The
//! `forklift-mcp` crate serves the management API over the Model Context
//! Protocol.

pub mod api;
pub mod audit;
pub mod auth;
pub mod cluster;
pub mod config;
pub mod coverage;
pub mod insights;
pub mod license;
pub mod memlimit;
pub mod meta;
pub mod metrics;
pub mod migrate;
pub mod notify;
pub mod objstore;
pub mod openapi;
pub mod outbound;
pub mod replication;
pub mod repo;
pub mod repoconfig;
pub mod server;
pub mod storage;
#[cfg(any(test, feature = "testing"))]
pub mod testing;
pub use forklift_common::version;
pub mod vuln;
pub mod webui;
