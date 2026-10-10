//! Runs an axum router until shutdown is signalled.

use std::net::{Ipv6Addr, SocketAddr};

use anyhow::Context;
use axum::Router;
use tokio::net::TcpListener;
use tokio::sync::watch;

/// Bind a dual-stack listener on `port`. Binding up front surfaces a busy
/// port before the collector starts polling.
pub async fn bind(port: u16) -> anyhow::Result<TcpListener> {
    let addr = SocketAddr::from((Ipv6Addr::UNSPECIFIED, port));
    TcpListener::bind(addr)
        .await
        .with_context(|| format!("bind {addr}"))
}

/// Serve `router` on `listener` until `shutdown` changes.
pub async fn serve(
    name: &'static str,
    listener: TcpListener,
    router: Router,
    mut shutdown: watch::Receiver<bool>,
) -> anyhow::Result<()> {
    let addr = listener.local_addr().context("listener local addr")?;
    tracing::info!(server = name, %addr, "listening");
    axum::serve(listener, router)
        .with_graceful_shutdown(async move {
            let _ = shutdown.changed().await;
        })
        .await
        .with_context(|| format!("{name} server failed"))?;
    tracing::info!(server = name, "stopped");
    Ok(())
}

#[cfg(test)]
mod tests {
    use axum::http::StatusCode;
    use axum::routing::get;

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
        let handle = tokio::spawn(serve("test", listener, router, rx));

        let stream = tokio::net::TcpStream::connect(addr).await;
        assert!(stream.is_ok(), "server accepts connections");
        drop(stream);

        tx.send(true).expect("shutdown");
        tokio::time::timeout(std::time::Duration::from_secs(5), handle)
            .await
            .expect("serve exits")
            .expect("no panic")
            .expect("no error");
    }
}
