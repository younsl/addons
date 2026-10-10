//! HTTP server: the embedded console, its JSON API, health, and metrics.
//! Handlers never touch the file system. They read [`State`] and wake the
//! controller.

pub mod api;
pub mod guard;
pub mod ui;

use std::net::{Ipv6Addr, SocketAddr};
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Context as _;
use axum::Router;
use axum::middleware;
use axum::routing::{get, post, put};
use serde::Serialize;
use tokio::net::TcpListener;
use tokio::sync::watch;

use crate::State;
use crate::config::{BuildInfo, Config};
use crate::telemetry::{LogBuffer, LogFilterHandle};

/// Static facts about this instance shown in the console.
#[derive(Debug, Clone, Serialize)]
pub struct Info {
    pub version: &'static str,
    pub commit: &'static str,
    pub built: &'static str,
    pub rustc: &'static str,
    pub config_file: PathBuf,
    pub home: PathBuf,
    pub dry_run: bool,
    pub reconcile_interval_secs: u64,
}

impl Info {
    pub fn new(cfg: &Config, build: BuildInfo) -> Self {
        Self {
            version: build.version,
            commit: build.commit,
            built: build.date,
            rustc: build.rustc,
            config_file: cfg.file.clone(),
            home: cfg.home.clone(),
            dry_run: cfg.dry_run,
            reconcile_interval_secs: cfg.reconcile_interval.as_secs(),
        }
    }
}

#[derive(Clone)]
pub struct AppState {
    pub state: State,
    pub info: Arc<Info>,
    pub log_filter: LogFilterHandle,
    pub logs: LogBuffer,
}

pub fn router(app: AppState) -> Router {
    let console = Router::new()
        .route("/", get(ui::index))
        .route("/logs", get(ui::index))
        .route("/assets/app.css", get(ui::css))
        .route("/assets/app.js", get(ui::js))
        .route("/api/info", get(api::info))
        .route("/api/status", get(api::status))
        .route("/api/tree", get(api::tree))
        .route("/api/file", get(api::file))
        .route("/api/reconcile", post(api::reconcile))
        .route("/api/log-level", put(api::log_level))
        .route("/api/logs", get(api::logs))
        .layer(middleware::from_fn(guard::loopback_only));
    Router::new()
        .route("/healthz", get(api::healthz))
        .route("/readyz", get(api::readyz))
        .route("/metrics", get(api::metrics))
        .merge(console)
        .with_state(app)
}

/// Bind a dual-stack listener on `port`. Binding up front surfaces a busy
/// port before the first reconcile touches the file system.
pub async fn bind(port: u16) -> anyhow::Result<TcpListener> {
    let addr = SocketAddr::from((Ipv6Addr::UNSPECIFIED, port));
    TcpListener::bind(addr)
        .await
        .with_context(|| format!("bind {addr}"))
}

/// Serve `router` on `listener` until `shutdown` changes.
pub async fn serve(
    listener: TcpListener,
    router: Router,
    mut shutdown: watch::Receiver<bool>,
) -> anyhow::Result<()> {
    let addr = listener.local_addr().context("listener local addr")?;
    tracing::info!(%addr, "listening");
    axum::serve(listener, router)
        .with_graceful_shutdown(async move {
            let _ = shutdown.changed().await;
        })
        .await
        .context("http server failed")?;
    tracing::info!("http server stopped");
    Ok(())
}

#[cfg(test)]
mod tests {
    use axum::http::StatusCode;

    use super::*;

    #[tokio::test]
    async fn bind_rejects_busy_port() {
        let first = bind(0).await.expect("bind ephemeral");
        let port = first.local_addr().expect("addr").port();
        assert!(bind(port).await.is_err(), "second bind on {port} must fail");
    }

    #[tokio::test]
    async fn serve_stops_on_shutdown() {
        let listener = bind(0).await.expect("bind");
        let addr = listener.local_addr().expect("addr");
        let router = Router::new().route("/ping", get(|| async { StatusCode::OK }));
        let (tx, rx) = watch::channel(false);
        let handle = tokio::spawn(serve(listener, router, rx));
        assert!(tokio::net::TcpStream::connect(addr).await.is_ok());
        tx.send(true).expect("shutdown");
        tokio::time::timeout(std::time::Duration::from_secs(5), handle)
            .await
            .expect("serve exits")
            .expect("no panic")
            .expect("no error");
    }
}
