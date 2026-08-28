//! Wires the collector and HTTP servers together.

use std::sync::Arc;

use anyhow::Context;
use prometheus_client::registry::Registry;
use tokio::sync::watch;
use tokio::task::JoinSet;

use crate::aws::ec2::Ec2Source;
use crate::collector::Collector;
use crate::config::{BuildInfo, Config};
use crate::observability::health::Health;
use crate::observability::{metrics, server};
use crate::types::INFO_LABELS;

/// Run the exporter until `shutdown` changes. Returns the first task error.
pub async fn run(cfg: Config, shutdown: watch::Receiver<bool>) -> anyhow::Result<()> {
    let build = BuildInfo::CURRENT;
    tracing::info!(
        version = build.version,
        commit = build.commit,
        built = build.date,
        rustc = build.rustc,
        region = cfg.region.as_deref().unwrap_or("sdk-default"),
        scrape_interval = %humantime::format_duration(cfg.scrape_interval),
        metrics_port = cfg.metrics_port,
        health_port = cfg.health_port,
        "starting ec2-metadata-exporter"
    );
    tracing::info!(
        labels = INFO_LABELS.join(","),
        label_count = INFO_LABELS.len(),
        "collecting EC2 instance metadata"
    );

    // Bind before touching AWS so a port clash fails fast.
    let health_listener = server::bind(cfg.health_port).await.context("health port")?;
    let metrics_listener = server::bind(cfg.metrics_port)
        .await
        .context("metrics port")?;

    let source = Ec2Source::from_env(cfg.region.clone()).await;
    let health = Arc::new(Health::default());
    let mut registry = Registry::default();
    metrics::register_build_info(&mut registry, build);
    let collector = Collector::new(source, Arc::clone(&health), &mut registry);
    let registry = Arc::new(registry);

    let mut tasks = JoinSet::new();
    tasks.spawn(server::serve(
        "health",
        health_listener,
        Arc::clone(&health).router(),
        shutdown.clone(),
    ));
    tasks.spawn(server::serve(
        "metrics",
        metrics_listener,
        metrics::router(registry),
        shutdown.clone(),
    ));
    let interval = cfg.scrape_interval;
    tasks.spawn(async move {
        collector.run(interval, shutdown).await;
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
    use std::time::Duration;

    use super::*;
    use crate::config::LogFormat;

    fn test_config() -> Config {
        Config {
            region: Some("us-east-1".into()),
            scrape_interval: Duration::from_secs(3600),
            metrics_port: 0,
            health_port: 0,
            log_level: "info".into(),
            log_format: LogFormat::Text,
        }
    }

    /// Full wiring with shutdown already signalled: listeners bind on
    /// ephemeral ports, the AWS client builds from the default chain without
    /// network access, and every task exits cleanly.
    #[tokio::test]
    async fn run_stops_on_shutdown() {
        let (tx, rx) = watch::channel(false);
        tx.send(true).expect("signal shutdown");
        tokio::time::timeout(Duration::from_secs(30), run(test_config(), rx))
            .await
            .expect("run returns")
            .expect("run succeeds");
    }

    #[tokio::test]
    async fn run_fails_on_busy_port() {
        let busy = server::bind(0).await.expect("bind");
        let mut cfg = test_config();
        cfg.health_port = busy.local_addr().expect("addr").port();
        let (_tx, rx) = watch::channel(false);
        let err = run(cfg, rx).await.expect_err("busy port must fail");
        assert!(format!("{err:#}").contains("health port"), "{err:#}");
    }
}
