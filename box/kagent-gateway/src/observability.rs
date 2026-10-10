//! The Prometheus registry, the metric set, and the HTTP servers that expose
//! `/metrics` and the health endpoints.

pub mod metrics;
pub mod server;

pub use metrics::Metrics;
