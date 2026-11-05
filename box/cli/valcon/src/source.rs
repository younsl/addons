//! Ports to the outside world. `aws/` and `k8s/` implement them, tests use fakes.

use std::collections::HashMap;
use std::fmt::Display;
use std::future::Future;
use std::net::IpAddr;

use crate::types::{CacheGroup, NodeMetrics, PodRef};

/// Replication groups with their nodes.
pub trait CatalogSource: Send + Sync + 'static {
    type Error: Display + Send;

    fn groups(&self) -> impl Future<Output = Result<Vec<CacheGroup>, Self::Error>> + Send;
}

/// Node id to private IP.
pub trait AddressSource: Send + Sync + 'static {
    type Error: Display + Send;

    fn node_ips(&self)
    -> impl Future<Output = Result<HashMap<String, IpAddr>, Self::Error>> + Send;
}

/// Latest metrics per node id.
pub trait MetricsSource: Send + Sync + 'static {
    type Error: Display + Send;

    fn latest(
        &self,
        nodes: Vec<String>,
    ) -> impl Future<Output = Result<HashMap<String, NodeMetrics>, Self::Error>> + Send;
}

/// Pods to inspect and their TCP tables.
pub trait PodSource: Send + Sync + 'static {
    type Error: Display + Send;

    fn list(&self) -> impl Future<Output = Result<Vec<PodRef>, Self::Error>> + Send;

    fn read_tcp_tables(
        &self,
        pod: &PodRef,
    ) -> impl Future<Output = Result<String, Self::Error>> + Send;
}

/// Sends raw bytes to `host:port` from inside a pod and returns everything the server sent
/// back until it closed the connection.
pub trait Tunnel: Send + Sync + 'static {
    type Error: Display + Send;

    fn roundtrip(
        &self,
        via: &PodRef,
        host: IpAddr,
        port: u16,
        payload: Vec<u8>,
    ) -> impl Future<Output = Result<Vec<u8>, Self::Error>> + Send;
}
