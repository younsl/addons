//! Command `forklift-mcp` serves the forklift management API as a Model
//! Context Protocol server over streamable HTTP, for MCP clients such as Claude
//! and kagent. It runs as its own process (in Kubernetes, its own pod) and
//! proxies every tool call to a forklift instance with the caller's credential.

use std::sync::Arc;
use std::time::Duration;

use anyhow::Context as _;
use axum::Router;
use axum::routing::get;
use forklift::mcp;
use forklift::version;
use prometheus::{Encoder, Registry, TextEncoder};
use rmcp::transport::StreamableHttpService;
use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
use tokio_util::sync::CancellationToken;

const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(15);

fn main() -> std::process::ExitCode {
    if std::env::args()
        .skip(1)
        .any(|a| a == "-version" || a == "--version")
    {
        println!("forklift-mcp {}", version::string());
        return std::process::ExitCode::SUCCESS;
    }
    forklift::server::logging::init_logging(
        &env_or("FORKLIFT_MCP_LOG_LEVEL", "info"),
        &env_or("FORKLIFT_MCP_LOG_FORMAT", "json"),
    );
    let runtime = match tokio::runtime::Runtime::new() {
        Ok(runtime) => runtime,
        Err(e) => {
            tracing::error!(error = %e, "fatal");
            return std::process::ExitCode::FAILURE;
        }
    };
    match runtime.block_on(run()) {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(e) => {
            tracing::error!(error = %e, "fatal");
            std::process::ExitCode::FAILURE
        }
    }
}

async fn run() -> anyhow::Result<()> {
    // reqwest is built with `rustls-no-provider`, so the process-wide crypto
    // provider has to be installed before the first HTTPS upstream call.
    let _ = rustls::crypto::ring::default_provider().install_default();

    let addr = env_or("FORKLIFT_MCP_ADDR", ":8080");
    // Metrics are always served on their own listener, never on the MCP
    // traffic port, so the Service can keep /metrics off the client-facing
    // port and the ServiceMonitor scrapes a dedicated endpoint.
    let metrics_addr = env_or("FORKLIFT_MCP_METRICS_ADDR", ":8081");
    let upstream = env_or("FORKLIFT_MCP_UPSTREAM_URL", "http://localhost:8080");
    // Optional shared credential for MCP clients that cannot attach an
    // Authorization header; per-request headers always take precedence.
    let token = std::env::var("FORKLIFT_MCP_TOKEN").unwrap_or_default();

    // Startup lines mirror forklift's shape: one identity line, then one line per listener, so
    // both processes read the same in aggregated logs.
    tracing::info!(
        version = %version::string(),
        rust = %version::rust(),
        upstream = %upstream,
        "starting forklift-mcp"
    );

    let registry = Registry::new();
    // It only exists on Linux.
    #[cfg(target_os = "linux")]
    registry
        .register(Box::new(
            prometheus::process_collector::ProcessCollector::for_self(),
        ))
        .context("register process collector")?;
    let metrics = mcp::Metrics::new(&registry);

    let server = mcp::Server::new(
        mcp::Client::new(&upstream, &token, Some(metrics.clone())),
        &version::string(),
        Some(metrics),
    );

    let cancel = CancellationToken::new();
    let mut config = rmcp::transport::streamable_http_server::StreamableHttpServerConfig::default()
        .disable_allowed_hosts();
    config.cancellation_token = cancel.clone();
    let mcp_service = StreamableHttpService::new(
        {
            let server = server.clone();
            move || Ok(server.clone())
        },
        Arc::new(LocalSessionManager::default()),
        config,
    );

    let app = Router::new()
        .route_service("/mcp", mcp_service)
        .route("/healthz", get(|| async { axum::http::StatusCode::OK }))
        .route("/readyz", get(|| async { axum::http::StatusCode::OK }));

    let metrics_app = Router::new().route(
        "/metrics",
        get({
            let registry = registry.clone();
            move || {
                let registry = registry.clone();
                async move { render_metrics(&registry) }
            }
        }),
    );

    let listener = tokio::net::TcpListener::bind(listen_addr(&addr))
        .await
        .with_context(|| format!("listen on {addr}"))?;
    let metrics_listener = tokio::net::TcpListener::bind(listen_addr(&metrics_addr))
        .await
        .with_context(|| format!("listen on {metrics_addr}"))?;

    let shutdown = CancellationToken::new();
    tracing::info!(addr = %addr, "http listening");
    let mut http = tokio::spawn({
        let shutdown = shutdown.clone();
        async move {
            axum::serve(listener, app)
                .with_graceful_shutdown(async move { shutdown.cancelled().await })
                .await
        }
    });
    tracing::info!(addr = %metrics_addr, "metrics listening");
    let mut metrics_http = tokio::spawn({
        let shutdown = shutdown.clone();
        async move {
            axum::serve(metrics_listener, metrics_app)
                .with_graceful_shutdown(async move { shutdown.cancelled().await })
                .await
        }
    });

    tokio::select! {
        res = wait(&mut http) => return res,
        res = wait(&mut metrics_http) => return res,
        _ = signals() => {}
    }

    tracing::info!("shutting down");
    shutdown.cancel();
    cancel.cancel();
    let _ = tokio::time::timeout(SHUTDOWN_TIMEOUT, async {
        let _ = tokio::join!(http, metrics_http);
    })
    .await;
    Ok(())
}

async fn wait(handle: &mut tokio::task::JoinHandle<std::io::Result<()>>) -> anyhow::Result<()> {
    match handle.await {
        Ok(Ok(())) => Ok(()),
        Ok(Err(e)) => Err(e.into()),
        Err(e) => Err(e.into()),
    }
}

async fn signals() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        let mut term = match signal(SignalKind::terminate()) {
            Ok(s) => s,
            Err(_) => {
                let _ = tokio::signal::ctrl_c().await;
                return;
            }
        };
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            _ = term.recv() => {}
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}

/// Renders the registry in the Prometheus text exposition format.
fn render_metrics(registry: &Registry) -> axum::response::Response {
    use axum::response::IntoResponse;
    let encoder = TextEncoder::new();
    let mut buf = Vec::new();
    if let Err(e) = encoder.encode(&registry.gather(), &mut buf) {
        return (
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            format!("{e}\n"),
        )
            .into_response();
    }
    (
        [(axum::http::header::CONTENT_TYPE, encoder.format_type())],
        buf,
    )
        .into_response()
}

fn env_or(key: &str, fallback: &str) -> String {
    match std::env::var(key) {
        Ok(v) if !v.is_empty() => v,
        _ => fallback.to_string(),
    }
}

fn listen_addr(addr: &str) -> String {
    if let Some(port) = addr.strip_prefix(':') {
        format!("0.0.0.0:{port}")
    } else {
        addr.to_string()
    }
}
