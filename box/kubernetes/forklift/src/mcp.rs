//! `forklift-mcp` exposes the forklift management API as a Model Context
//! Protocol server, so MCP clients (Claude, kagent, and other agent runtimes)
//! can operate a forklift instance through typed tools.
//!
//! The server is a thin proxy: every tool call becomes one HTTP request to the
//! upstream forklift management API, authenticated with the caller's own
//! credential. The Authorization header on the incoming MCP HTTP request is
//! forwarded verbatim, so RBAC is enforced by forklift per caller, not by a
//! shared service identity. `FORKLIFT_MCP_TOKEN` provides an optional fallback
//! credential for clients that cannot attach headers.

pub mod client;
pub mod metrics;
pub mod server;

pub use client::{Client, Error};
pub use metrics::{Metrics, OUTCOME_ERROR, OUTCOME_OK, OUTCOME_TOOL_ERROR};
pub use server::{Body, Query, Server};
