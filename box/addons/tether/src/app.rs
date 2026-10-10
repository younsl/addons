//! Wires the reconcile loop and the HTTP server together.

use std::sync::Arc;

use anyhow::Context;
use prometheus_client::registry::Registry;
use tokio::sync::watch;
use tokio::task::JoinSet;

use crate::api::{self, AppState};
use crate::config::{BuildInfo, Config};
use crate::observability::metrics::{self, Metrics};
use crate::observability::server;
use crate::reconciler::{Reconciler, Settings, Shared};

/// Run until `shutdown` changes. Returns the first task error.
pub async fn run(cfg: Config, shutdown: watch::Receiver<bool>) -> anyhow::Result<()> {
    let build = BuildInfo::CURRENT;
    tracing::info!(
        version = build.version,
        commit = build.commit,
        built = build.date,
        rustc = build.rustc,
        config_file = %cfg.file.display(),
        home = %cfg.home.display(),
        reconcile_interval = %humantime::format_duration(cfg.reconcile_interval),
        dry_run = cfg.dry_run,
        port = cfg.port,
        "starting tether"
    );

    let listener = server::bind(cfg.port).await.context("http port")?;

    let mut registry = Registry::default();
    metrics::register_build_info(&mut registry, build);
    let metrics = Metrics::register(&mut registry);
    let shared = Arc::new(Shared::default());
    let reconciler = Reconciler::new(Settings::from(&cfg), Arc::clone(&shared), metrics);
    let state = AppState {
        shared,
        registry: Arc::new(registry),
    };

    let mut tasks = JoinSet::new();
    tasks.spawn(server::serve(
        "http",
        listener,
        api::router(state),
        shutdown.clone(),
    ));
    let interval = cfg.reconcile_interval;
    tasks.spawn(async move {
        reconciler.run(interval, shutdown).await;
        Ok(())
    });

    while let Some(res) = tasks.join_next().await {
        res.context("task panicked")??;
    }
    tracing::info!("shutdown complete");
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::time::Duration;

    use super::*;
    use crate::config::LogFormat;

    fn test_config() -> Config {
        Config {
            file: PathBuf::from("/nonexistent/config.toml"),
            home: PathBuf::from("/nonexistent"),
            reconcile_interval: Duration::from_secs(3600),
            dry_run: true,
            port: 0,
            log_level: "info".into(),
            log_format: LogFormat::Text,
        }
    }

    #[tokio::test]
    async fn run_stops_on_shutdown() {
        let (tx, rx) = watch::channel(false);
        tx.send(true).expect("signal shutdown");
        tokio::time::timeout(Duration::from_secs(10), run(test_config(), rx))
            .await
            .expect("run returns")
            .expect("run succeeds");
    }

    #[tokio::test]
    async fn run_fails_on_busy_port() {
        let busy = server::bind(0).await.expect("bind");
        let mut cfg = test_config();
        cfg.port = busy.local_addr().expect("addr").port();
        let (_tx, rx) = watch::channel(false);
        let err = run(cfg, rx).await.expect_err("busy port must fail");
        assert!(format!("{err:#}").contains("http port"), "{err:#}");
    }
}
