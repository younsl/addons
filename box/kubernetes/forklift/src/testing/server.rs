//! Shared server test harness. Crate-internal: it uses private
//! `server::middleware` and `server::pprof_routes` items.

#![allow(dead_code)]

use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use http::{Method, Request};
use prometheus::Registry;
use tower::ServiceExt;

use crate::server::*;

pub(crate) async fn new_test_server() -> (Server, Registry, tempfile::TempDir) {
    let (store, dir) = crate::meta::Store::open_temp().await.expect("open store");
    let cfg = Arc::new(crate::config::Config {
        http_addr: "127.0.0.1:0".into(),
        metrics_addr: "127.0.0.1:0".into(),
        pprof_addr: "127.0.0.1:0".into(),
        shutdown_timeout: Duration::from_secs(1),
        ..Default::default()
    });
    let registry = Registry::new();
    let server = Server::new(cfg, Arc::new(store), &registry);
    (server, registry, dir)
}

/// Issues one request against the composed router.
pub(crate) async fn get_request(server: &Server, path: &str) -> http::Response<Body> {
    server
        .router()
        .oneshot(
            Request::builder()
                .method(Method::GET)
                .uri(path)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap()
}
