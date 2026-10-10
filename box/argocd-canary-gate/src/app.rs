//! Wires the pieces together and runs the two listeners: an HTTPS
//! `ValidatingAdmissionWebhook` that enforces the gate, and a plain HTTP
//! listener serving probes and metrics.

use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use axum::Router;
use axum::extract::State;
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum_server::Handle;
use axum_server::tls_rustls::RustlsConfig;
use kube::config::{KubeConfigOptions, Kubeconfig};
use tokio::sync::watch;

use crate::admission::{self, AdmissionState};
use crate::cli::Cli;
use crate::config::Config;
use crate::engine::Engine;
use crate::events::{KubeSink, Recorder};
use crate::extension::{self, ExtensionState};
use crate::k8s::{AppReader, KubeAppReader, KubeRolloutReader, RolloutReader};
use crate::observability::Metrics;
use crate::servingcert::Reloader;
use crate::uiextension;

/// Bounds in-flight requests during a graceful stop. It stays under the usual
/// pod `terminationGracePeriodSeconds` so the process exits on its own terms
/// rather than being killed.
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(10);

/// How often the serving certificate files are checked for a re-issue.
const CERT_POLL_INTERVAL: Duration = Duration::from_secs(10);

/// Everything the admin listener needs.
pub struct AdminState {
    pub metrics: Arc<Metrics>,
    pub extension_name: String,
}

/// Runs the gate until `shutdown` fires or a listener fails.
pub async fn run(cli: Cli, mut shutdown: watch::Receiver<bool>) -> anyhow::Result<()> {
    // Printed before anything can fail, so a bug report always carries the
    // build and the runtime it came from.
    tracing::info!(
        version = env!("CARGO_PKG_VERSION"),
        commit = env!("BUILD_COMMIT"),
        rustc = env!("BUILD_RUSTC_VERSION"),
        platform = format!("{}/{}", std::env::consts::OS, std::env::consts::ARCH),
        cpus = std::thread::available_parallelism().map_or(0, std::num::NonZero::get),
        config = %cli.config.display(),
        webhook_addr = %cli.webhook_addr,
        admin_addr = %cli.admin_addr,
        "starting argocd-canary-gate"
    );

    let cfg = Config::load(&cli.config)?;
    log_gate_config(&cfg);

    let client = kube_client(&cli.kubeconfig).await?;
    let metrics = Arc::new(Metrics::new());
    let reader: Arc<dyn RolloutReader> = Arc::new(KubeRolloutReader::new(client.clone()));

    let recorder = Arc::new(Recorder::new(
        Arc::new(KubeSink::new(client.clone(), &cfg.argocd.namespace)),
        Arc::clone(&metrics),
    ));
    tracing::info!(
        enabled = true,
        namespace = %cfg.argocd.namespace,
        involved_object_kind = "Application",
        required_rbac = "create on events",
        note = "a write failure is logged and never changes a verdict",
        "kubernetes event recording started"
    );

    let app_reader: Arc<dyn AppReader> = Arc::new(KubeAppReader::new(
        client,
        &cfg.argocd.namespace,
        &cfg.exempt.annotation,
    ));
    let engine = Arc::new(Engine::new(cfg, reader, Arc::clone(&metrics)));

    // Loaded eagerly so a broken pair fails startup here rather than at the
    // first handshake.
    let reloader = Arc::new(Reloader::new(&cli.tls_cert_file, &cli.tls_key_file));
    let loaded = reloader.load()?;
    log_certificate(&reloader, loaded.leaf.as_ref());
    metrics.set_certificate_expiry(loaded.leaf.as_ref().map_or(0, |l| l.not_after_unix));
    let tls = RustlsConfig::from_config(loaded.config);

    let admin_router = admin_router(
        Arc::new(AdminState {
            metrics: Arc::clone(&metrics),
            extension_name: cli.extension_name.clone(),
        }),
        Arc::new(ExtensionState {
            engine: Arc::clone(&engine),
            reader: app_reader,
        }),
    );
    let webhook_router = admission::router(Arc::new(AdmissionState {
        engine,
        metrics: Arc::clone(&metrics),
        events: Some(Arc::clone(&recorder) as Arc<_>),
    }));

    let admin_listener = tokio::net::TcpListener::bind(cli.admin_addr)
        .await
        .with_context(|| format!("bind admin listener {}", cli.admin_addr))?;
    tracing::info!(addr = %cli.admin_addr, "admin listener listening");
    let mut admin_shutdown = shutdown.clone();
    let admin = tokio::spawn(async move {
        axum::serve(admin_listener, admin_router)
            .with_graceful_shutdown(async move {
                let _ = admin_shutdown.wait_for(|stop| *stop).await;
            })
            .await
            .context("admin server")
    });

    let webhook_handle = Handle::new();
    tracing::info!(addr = %cli.webhook_addr, "admission webhook listening");
    let webhook = {
        let handle = webhook_handle.clone();
        let tls = tls.clone();
        tokio::spawn(async move {
            axum_server::bind_rustls(cli.webhook_addr, tls)
                .handle(handle)
                .serve(webhook_router.into_make_service())
                .await
                .context("webhook server")
        })
    };

    let cert_watch = tokio::spawn(watch_certificate(reloader, tls, Arc::clone(&metrics)));

    let result = tokio::select! {
        res = admin => res.map_err(anyhow::Error::from).and_then(|r| r),
        res = webhook => res.map_err(anyhow::Error::from).and_then(|r| r),
        _ = shutdown.wait_for(|stop| *stop) => {
            tracing::info!("shutdown signal received");
            Ok(())
        }
    };

    cert_watch.abort();
    webhook_handle.graceful_shutdown(Some(SHUTDOWN_TIMEOUT));
    recorder.shutdown().await;
    result
}

/// Reloads the serving pair when the files change.
///
/// Polling rather than per handshake: a stat every few seconds costs nothing,
/// and a re-issued certificate is picked up well before the old one expires.
async fn watch_certificate(reloader: Arc<Reloader>, tls: RustlsConfig, metrics: Arc<Metrics>) {
    let mut ticker = tokio::time::interval(CERT_POLL_INTERVAL);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        ticker.tick().await;
        if !reloader.changed() {
            continue;
        }
        match reloader.load() {
            Ok(loaded) => {
                tls.reload_from_config(loaded.config);
                metrics
                    .set_certificate_expiry(loaded.leaf.as_ref().map_or(0, |l| l.not_after_unix));
                log_certificate(&reloader, loaded.leaf.as_ref());
            }
            // Serving the pair already in memory beats failing the handshake:
            // the old leaf may still verify against caBundle, a missing one
            // never can.
            Err(err) => tracing::warn!(
                cert_file = %reloader.cert_file().display(),
                error = %err,
                "could not reload the webhook certificate, serving the one already loaded"
            ),
        }
    }
}

/// The issuer is logged next to the expiry because the failure the reloader
/// exists for is a re-issued CA, not an expired leaf.
fn log_certificate(reloader: &Reloader, leaf: Option<&crate::servingcert::LeafInfo>) {
    if let Some(leaf) = leaf {
        tracing::info!(
            subject = %leaf.subject,
            issuer = %leaf.issuer,
            dns_names = ?leaf.dns_names,
            not_after_unix = leaf.not_after_unix,
            "webhook certificate loaded"
        );
    } else {
        tracing::info!(cert_file = %reloader.cert_file().display(), "webhook certificate loaded");
    }
}

/// Builds the admin router: probes, metrics, the UI extension API, and the
/// extension script itself.
pub fn admin_router(state: Arc<AdminState>, extension_state: Arc<ExtensionState>) -> Router {
    Router::new()
        .route("/healthz", get(|| async { StatusCode::OK }))
        .route("/readyz", get(|| async { StatusCode::OK }))
        .route("/metrics", get(metrics_handler))
        // Serving the script here is not how the browser gets it, since Argo
        // CD loads extensions off disk. It is here so the running version can
        // be diffed against what argocd-server is actually serving, and so
        // argocd-extension-installer can fetch it with its standard
        // EXTENSION_URL and unpack it into argocd-server's extensions volume.
        .route("/api/v1/extension.tar", get(extension_tar))
        .route("/api/v1/extension.js", get(extension_js))
        .with_state(state)
        .merge(extension::router(extension_state))
}

async fn extension_tar(State(state): State<Arc<AdminState>>) -> Response {
    match uiextension::tar(&state.extension_name) {
        Ok(archive) => ([(header::CONTENT_TYPE, "application/x-tar")], archive).into_response(),
        Err(err) => {
            tracing::error!(error = %err, "could not pack the embedded extension script");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "extension archive unavailable",
            )
                .into_response()
        }
    }
}

async fn extension_js(State(state): State<Arc<AdminState>>) -> Response {
    match uiextension::script(&state.extension_name) {
        Ok(script) => (
            [(
                header::CONTENT_TYPE,
                "application/javascript; charset=utf-8",
            )],
            script,
        )
            .into_response(),
        Err(err) => {
            tracing::error!(error = %err, "could not render the embedded extension script");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "extension script unavailable",
            )
                .into_response()
        }
    }
}

async fn metrics_handler(State(state): State<Arc<AdminState>>) -> Response {
    match state.metrics.encode() {
        Ok(body) => (
            [(
                header::CONTENT_TYPE,
                "application/openmetrics-text; version=1.0.0; charset=utf-8",
            )],
            body,
        )
            .into_response(),
        Err(err) => {
            tracing::error!(error = %err, "could not encode metrics");
            (StatusCode::INTERNAL_SERVER_ERROR, "metrics unavailable").into_response()
        }
    }
}

async fn kube_client(kubeconfig: &str) -> anyhow::Result<kube::Client> {
    let config = if kubeconfig.trim().is_empty() {
        kube::Config::incluster().context("build in-cluster config")?
    } else {
        let file = Kubeconfig::read_from(kubeconfig)
            .with_context(|| format!("build config from kubeconfig {kubeconfig}"))?;
        kube::Config::from_custom_kubeconfig(file, &KubeConfigOptions::default())
            .await
            .with_context(|| format!("build config from kubeconfig {kubeconfig}"))?
    };
    kube::Client::try_from(config).context("build kubernetes client")
}

/// Prints the whole effective policy at startup.
///
/// Every value here is either a decision somebody made or a default they
/// inherited, and a gate that refuses a deploy is the wrong place to be
/// guessing which.
fn log_gate_config(cfg: &Config) {
    tracing::info!(
        mode = cfg.mode.as_str(),
        on_error = cfg.on_error.as_str(),
        tracking_label = %cfg.rollouts.tracking_label,
        strategies = ?cfg.rollouts.strategies,
        "gate policy"
    );

    tracing::info!(
        usernames = ?cfg.exempt.usernames,
        automated = cfg.exempt.automated,
        skip_annotation = %cfg.exempt.annotation,
        skip_annotation_value = "true",
        "exemptions"
    );

    tracing::info!(
        enabled = true,
        blocked_verdicts = "Warning",
        warned_verdicts = "Normal",
        passed_verdicts = "no event",
        namespace = %cfg.argocd.namespace,
        "kubernetes events"
    );
}

#[cfg(test)]
mod tests {
    use axum::body::Body;
    use axum::http::Request;
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    use super::*;

    struct NoApps;

    #[async_trait::async_trait]
    impl AppReader for NoApps {
        async fn get(
            &self,
            _name: &str,
        ) -> Result<Option<crate::gate::AppSnapshot>, crate::k8s::application::AppReadError>
        {
            Ok(None)
        }
    }

    fn state() -> (Arc<AdminState>, Arc<ExtensionState>) {
        let metrics = Arc::new(Metrics::new());
        let engine = Arc::new(Engine::new(
            Config::default(),
            Arc::new(crate::engine::testing::FakeReader::default()) as Arc<dyn RolloutReader>,
            Arc::clone(&metrics),
        ));
        (
            Arc::new(AdminState {
                metrics,
                extension_name: "my-gate".to_string(),
            }),
            Arc::new(ExtensionState {
                engine,
                reader: Arc::new(NoApps),
            }),
        )
    }

    async fn get(uri: &str) -> (StatusCode, String, Vec<u8>) {
        let (admin, extension) = state();
        let response = admin_router(admin, extension)
            .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
            .await
            .unwrap();
        let status = response.status();
        let content_type = response
            .headers()
            .get(header::CONTENT_TYPE)
            .map(|v| v.to_str().unwrap().to_string())
            .unwrap_or_default();
        let bytes = response
            .into_body()
            .collect()
            .await
            .unwrap()
            .to_bytes()
            .to_vec();
        (status, content_type, bytes)
    }

    #[tokio::test]
    async fn probes_and_metrics() {
        assert_eq!(get("/healthz").await.0, StatusCode::OK);
        assert_eq!(get("/readyz").await.0, StatusCode::OK);
        let (status, content_type, body) = get("/metrics").await;
        assert_eq!(status, StatusCode::OK);
        assert!(content_type.contains("openmetrics"));
        assert!(
            String::from_utf8(body)
                .unwrap()
                .contains("argocd_canary_gate_")
        );
        assert_eq!(get("/nope").await.0, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn extension_script_and_archive() {
        let (status, content_type, body) = get("/api/v1/extension.js").await;
        assert_eq!(status, StatusCode::OK);
        assert!(content_type.starts_with("application/javascript"));
        assert!(
            String::from_utf8(body)
                .unwrap()
                .contains("/extensions/my-gate/")
        );

        let (status, content_type, body) = get("/api/v1/extension.tar").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(content_type, "application/x-tar");
        assert_eq!(body, uiextension::tar("my-gate").unwrap());

        let (status, _, body) = get("/api/v1/config").await;
        assert_eq!(status, StatusCode::OK);
        assert!(String::from_utf8(body).unwrap().contains("trackingLabel"));

        let (status, _, _) = get("/api/v1/gate?app=missing").await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn startup_reports_do_not_fail() {
        let cfg = Config::parse("mode: warn\nonError: allow\n").unwrap();
        log_gate_config(&cfg);
        log_gate_config(&Config::default());
        assert!(kube_client("/nonexistent/kubeconfig").await.is_err());
    }
}
