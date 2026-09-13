//! The accept loop behind every listener.
//!

use std::net::SocketAddr;
use std::time::Duration;

use axum::Router;
use axum::extract::ConnectInfo;
use hyper::Request;
use hyper::body::Incoming;
use hyper_util::rt::{TokioExecutor, TokioIo, TokioTimer};
use hyper_util::server::conn::auto;
use hyper_util::server::graceful::GracefulShutdown;
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;
use tower::ServiceExt;

use super::READ_HEADER_TIMEOUT;

/// Accepts connections until `cancel` fires, then stops accepting and waits
/// out `shutdown_timeout` for in-flight requests before dropping the rest.
///
pub(crate) async fn serve(
    listener: TcpListener,
    router: Router,
    cancel: CancellationToken,
    shutdown_timeout: Duration,
    connect_info: bool,
) {
    let mut builder = auto::Builder::new(TokioExecutor::new());
    builder
        .http1()
        .timer(TokioTimer::new())
        .header_read_timeout(READ_HEADER_TIMEOUT);
    let graceful = GracefulShutdown::new();

    loop {
        let (stream, peer) = tokio::select! {
            _ = cancel.cancelled() => break,
            accepted = listener.accept() => match accepted {
                Ok(v) => v,
                Err(e) => {
                    tracing::warn!(err = %e, "accept failed");
                    continue;
                }
            },
        };

        let router = router.clone();
        let service = hyper::service::service_fn(move |mut req: Request<Incoming>| {
            let router = router.clone();
            if connect_info {
                req.extensions_mut().insert(ConnectInfo(peer as SocketAddr));
            }
            async move { router.oneshot(req).await }
        });
        let conn = builder
            .serve_connection_with_upgrades(TokioIo::new(stream), service)
            .into_owned();
        let watched = graceful.watch(conn);
        tokio::spawn(async move {
            if let Err(e) = watched.await {
                tracing::debug!(err = %e, "connection closed with error");
            }
        });
    }

    if tokio::time::timeout(shutdown_timeout, graceful.shutdown())
        .await
        .is_err()
    {
        tracing::warn!("shutdown timed out with connections still open");
    }
}
