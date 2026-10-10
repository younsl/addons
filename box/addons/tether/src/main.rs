//! tether binary: settings and logging, then the controller and the web
//! server until SIGTERM or Ctrl-C.

use std::sync::Arc;

use anyhow::Context as _;
use tokio::signal;
use tokio::sync::watch;
use tokio::task::JoinSet;

use tether::config::{BuildInfo, Config};
use tether::telemetry::{self, LogBuffer, LogFilterHandle};
use tether::web::{self, AppState, Info};
use tether::{Metrics, State, banner, controller};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cfg = Config::load();
    eprint!("{}", banner::render(BuildInfo::CURRENT));
    let (log_filter, logs) = telemetry::init(&cfg.log_level, cfg.log_format);

    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    tokio::spawn(async move {
        wait_for_signal().await;
        let _ = shutdown_tx.send(true);
    });

    run(cfg, log_filter, logs, shutdown_rx).await
}

async fn run(
    cfg: Config,
    log_filter: LogFilterHandle,
    logs: LogBuffer,
    shutdown: watch::Receiver<bool>,
) -> anyhow::Result<()> {
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

    let listener = web::bind(cfg.port).await.context("http port")?;

    let state = State::new(Metrics::new(build));
    let ctx = state.to_context(&cfg);
    let app = AppState {
        state: state.clone(),
        info: Arc::new(Info::new(&cfg, build)),
        log_filter,
        logs,
    };

    let mut tasks = JoinSet::new();
    tasks.spawn(web::serve(listener, web::router(app), shutdown.clone()));
    let interval = cfg.reconcile_interval;
    tasks.spawn(async move {
        controller::run(state, ctx, interval, shutdown).await;
        Ok(())
    });

    while let Some(res) = tasks.join_next().await {
        res.context("task panicked")??;
    }
    tracing::info!("shutdown complete");
    Ok(())
}

async fn wait_for_signal() {
    let ctrl_c = async {
        let _ = signal::ctrl_c().await;
    };
    let terminate = async {
        if let Ok(mut sig) = signal::unix::signal(signal::unix::SignalKind::terminate()) {
            sig.recv().await;
        }
    };

    tokio::select! {
        () = ctrl_c => {},
        () = terminate => {},
    }
    tracing::info!("shutdown signal received");
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::time::Duration;

    use tether::config::LogFormat;
    use tracing_subscriber::{EnvFilter, Registry, reload};

    use super::*;

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

    fn filter() -> (reload::Layer<EnvFilter, Registry>, LogFilterHandle) {
        reload::Layer::new(EnvFilter::new("info"))
    }

    #[tokio::test]
    async fn run_stops_on_shutdown() {
        let (_layer, handle) = filter();
        let (tx, rx) = watch::channel(false);
        tx.send(true).expect("signal shutdown");
        tokio::time::timeout(
            Duration::from_secs(10),
            run(test_config(), handle, LogBuffer::default(), rx),
        )
        .await
        .expect("run returns")
        .expect("run succeeds");
    }

    #[tokio::test]
    async fn run_fails_on_busy_port() {
        let (_layer, handle) = filter();
        let busy = web::bind(0).await.expect("bind");
        let mut cfg = test_config();
        cfg.port = busy.local_addr().expect("addr").port();
        let (_tx, rx) = watch::channel(false);
        let err = run(cfg, handle, LogBuffer::default(), rx)
            .await
            .expect_err("busy port must fail");
        assert!(format!("{err:#}").contains("http port"), "{err:#}");
    }
}
