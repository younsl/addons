//! Wires the pieces together and runs the two listeners: an HTTPS
//! `ValidatingAdmissionWebhook` that enforces the gate, and a plain HTTP
//! listener serving probes, metrics, and the read-only API behind the Argo CD
//! UI extension.

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
use crate::argocd::{AppReader, DesiredImageClient, ImageResolver, KubeReader};
use crate::cli::Cli;
use crate::config::Config;
use crate::engine::Engine;
use crate::events::{KubeSink, Recorder};
use crate::extension;
use crate::gate::AppSnapshot;
use crate::observability::Metrics;
use crate::servingcert::Reloader;
use crate::uiextension;

/// Bounds in-flight requests during a graceful stop. It stays under the usual
/// pod `terminationGracePeriodSeconds` so the process exits on its own terms
/// rather than being killed.
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(10);

/// Bounds the one-off startup listing. It is a report, not a dependency, so it
/// gives up quickly.
const EXEMPT_SCAN_TIMEOUT: Duration = Duration::from_secs(10);

/// Bounds the one-off startup token check. Same reasoning: a report, so it must
/// not hold the listeners back.
const TOKEN_PROBE_TIMEOUT: Duration = Duration::from_secs(5);

/// How often the serving certificate files are checked for a re-issue.
const CERT_POLL_INTERVAL: Duration = Duration::from_secs(10);

/// Caps the names printed for the startup scan.
const MAX_EXEMPT_NAMES_LOGGED: usize = 20;

/// Everything the admin listener needs.
pub struct AdminState {
    pub engine: Arc<Engine>,
    pub metrics: Arc<Metrics>,
    pub extension_name: String,
}

/// Runs the gate until `shutdown` fires or a listener fails.
#[allow(clippy::too_many_lines)]
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
        "starting argocd-promotion-gate"
    );

    let cfg = Config::load(&cli.config)?;
    log_gate_config(&cfg);

    let client = kube_client(&cli.kubeconfig).await?;
    let metrics = Arc::new(Metrics::new());
    let reader: Arc<dyn AppReader> = Arc::new(KubeReader::new(
        client.clone(),
        &cfg.argocd.namespace,
        &cfg.exempt.annotation,
    ));

    let images: Option<Arc<dyn ImageResolver>> = if cfg.image_tag.enabled {
        let client = DesiredImageClient::new(&cfg.argocd, &cfg.image_tag.kinds)
            .context("build argocd api client")?;
        if client.has_token() {
            probe_token(&cfg, &client).await;
        } else {
            tracing::warn!(
                token_path = %cfg.argocd.token_path,
                on_error = cfg.image_tag.on_error.as_str(),
                "no argocd api token found. Desired image lookups will fail until the secret is mounted"
            );
        }
        Some(Arc::new(client))
    } else {
        tracing::info!("image tag comparison disabled. Only upstream sync and health are checked");
        None
    };

    log_exempt_applications(&cfg, reader.as_ref()).await;

    let recorder = Arc::new(Recorder::new(
        Arc::new(KubeSink::new(client, &cfg.argocd.namespace)),
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

    let engine = Arc::new(Engine::new(cfg, reader, images, Arc::clone(&metrics)));

    // Loaded eagerly so a broken pair fails startup here rather than at the
    // first handshake.
    let reloader = Arc::new(Reloader::new(&cli.tls_cert_file, &cli.tls_key_file));
    let loaded = reloader.load()?;
    log_certificate(&reloader, loaded.leaf.as_ref());
    metrics.set_certificate_expiry(loaded.leaf.as_ref().map_or(0, |l| l.not_after_unix));
    let tls = RustlsConfig::from_config(loaded.config);

    let admin_router = admin_router(Arc::new(AdminState {
        engine: Arc::clone(&engine),
        metrics: Arc::clone(&metrics),
        extension_name: cli.extension_name.clone(),
    }));
    let webhook_router = admission::router(Arc::new(AdmissionState {
        engine,
        metrics: Arc::clone(&metrics),
        events: Some(Arc::clone(&recorder) as Arc<_>),
    }));

    let admin_listener = tokio::net::TcpListener::bind(cli.admin_addr)
        .await
        .with_context(|| format!("bind admin listener {}", cli.admin_addr))?;
    tracing::info!(addr = %cli.admin_addr, "admin and extension API listening");
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
pub fn admin_router(state: Arc<AdminState>) -> Router {
    let engine = Arc::clone(&state.engine);
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
        .merge(extension::router(engine))
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
/// guessing which. The resolved chain is included because an off-by-one in the
/// order is the mistake that would be hardest to spot from the raw list alone.
fn log_gate_config(cfg: &Config) {
    let gated = cfg.gated_envs();
    tracing::info!(
        chain = ?cfg.chain,
        gated_envs = ?gated,
        gated_envs_explicit = !cfg.gated_envs.is_empty(),
        require_sync = cfg.require.sync,
        require_health = cfg.require.health,
        rollback_allowed = cfg.rollback.allow_previously_deployed_revision,
        "gate policy"
    );

    for env in &gated {
        tracing::info!(env = %env, upstream = cfg.upstream_env(env).unwrap_or_default(), "chain resolved");
    }

    if cfg.image_tag.enabled {
        tracing::info!(
            mode = cfg.image_tag.mode.as_str(),
            on_error = cfg.image_tag.on_error.as_str(),
            kinds = ?cfg.image_tag.kinds,
            ignore_repos = ?cfg.image_tag.ignore_repos,
            "image tag check"
        );
    } else {
        tracing::info!("image tag check disabled. Only upstream sync and health are checked");
    }

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
        "kubernetes events"
    );

    tracing::info!(
        namespace = %cfg.argocd.namespace,
        server_address = %cfg.argocd.server_address,
        ca_file = or_none(&cfg.argocd.ca_file),
        insecure_skip_verify = cfg.argocd.insecure_skip_verify,
        token_path = %cfg.argocd.token_path,
        timeout_seconds = cfg.argocd.timeout_seconds,
        cache_ttl_seconds = cfg.argocd.cache_ttl_seconds,
        "argocd access"
    );
}

/// Asks argocd-server whether the mounted token is usable, so a revoked
/// credential is a startup warning rather than a denied production sync hours
/// later.
///
/// Never fatal and never wired into readiness: a webhook that reported itself
/// unready whenever argocd-server was unreachable would take syncs down for a
/// reason unrelated to promotion.
async fn probe_token(cfg: &Config, client: &DesiredImageClient) {
    let probe = tokio::time::timeout(TOKEN_PROBE_TIMEOUT, client.probe()).await;
    match probe {
        Ok(Ok(username)) => tracing::info!(
            account = %username,
            server_address = %cfg.argocd.server_address,
            "argocd api token accepted"
        ),
        Ok(Err(err)) => tracing::warn!(
            server_address = %cfg.argocd.server_address,
            token_path = %cfg.argocd.token_path,
            on_error = cfg.image_tag.on_error.as_str(),
            error = %err,
            "argocd api token did not pass a startup check. Desired image lookups will fail while this stands"
        ),
        Err(_) => tracing::warn!(
            server_address = %cfg.argocd.server_address,
            token_path = %cfg.argocd.token_path,
            on_error = cfg.image_tag.on_error.as_str(),
            "argocd api token check timed out. Desired image lookups will fail while this stands"
        ),
    }
}

/// Counts the annotation's current reach at startup.
///
/// Read-only and non-fatal: a listing failure is worth a warning but must not
/// stop a gate from starting.
async fn log_exempt_applications(cfg: &Config, reader: &dyn AppReader) {
    let apps = match tokio::time::timeout(EXEMPT_SCAN_TIMEOUT, reader.list()).await {
        Ok(Ok(apps)) => apps,
        Ok(Err(err)) => {
            tracing::warn!(
                namespace = %cfg.argocd.namespace,
                annotation = %cfg.exempt.annotation,
                error = %err,
                "could not list applications to report current exemptions"
            );
            return;
        }
        Err(_) => {
            tracing::warn!(
                namespace = %cfg.argocd.namespace,
                annotation = %cfg.exempt.annotation,
                "listing applications to report current exemptions timed out"
            );
            return;
        }
    };

    let (all, gated) = exempt_names(cfg, &apps);
    tracing::info!(
        annotation = %cfg.exempt.annotation,
        namespace = %cfg.argocd.namespace,
        applications_scanned = apps.len(),
        exempt = all.len(),
        exempt_in_gated_envs = gated.len(),
        apps = ?truncate_names(&all),
        "skip annotation scan"
    );
}

/// Returns the applications carrying the skip annotation, split by whether the
/// gate would otherwise have enforced anything on them.
///
/// Both numbers matter. The total says how widely the hatch is open, and the
/// gated subset says how much of it is actually bypassing a check rather than
/// sitting on an environment the gate ignores anyway.
pub fn exempt_names(cfg: &Config, apps: &[AppSnapshot]) -> (Vec<String>, Vec<String>) {
    let mut all = Vec::new();
    let mut gated = Vec::new();
    for app in apps.iter().filter(|app| app.skip_requested) {
        all.push(app.name.clone());
        if cfg.is_gated(&app.project) {
            gated.push(app.name.clone());
        }
    }
    all.sort();
    gated.sort();
    (all, gated)
}

/// Keeps a long list from turning one startup line into a wall.
pub fn truncate_names(names: &[String]) -> Vec<String> {
    if names.len() <= MAX_EXEMPT_NAMES_LOGGED {
        return names.to_vec();
    }
    let mut out = names[..MAX_EXEMPT_NAMES_LOGGED].to_vec();
    out.push(format!(
        "and {} more",
        names.len() - MAX_EXEMPT_NAMES_LOGGED
    ));
    out
}

fn or_none(value: &str) -> &str {
    if value.trim().is_empty() {
        "<none>"
    } else {
        value
    }
}

#[cfg(test)]
mod tests {
    use axum::body::Body;
    use axum::http::Request;
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    use super::*;
    use crate::engine::testing::{FakeReader, snapshot};

    fn state() -> Arc<AdminState> {
        let cfg = Config::parse("chain: [stg, prd]\nimageTag:\n  enabled: false\n").unwrap();
        let metrics = Arc::new(Metrics::new());
        let engine = Arc::new(Engine::new(
            cfg,
            Arc::new(FakeReader::default()),
            None,
            Arc::clone(&metrics),
        ));
        Arc::new(AdminState {
            engine,
            metrics,
            extension_name: "my-gate".to_string(),
        })
    }

    async fn get(uri: &str) -> (StatusCode, String, Vec<u8>) {
        let response = admin_router(state())
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
                .contains("argocd_promotion_gate_")
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
        assert!(String::from_utf8(body).unwrap().contains("\"chain\""));
    }

    #[test]
    fn exempt_names_split_by_gating() {
        let cfg = Config::parse("chain: [stg, prd]\n").unwrap();
        let mut a = snapshot("prd-b", "prd", "", "", &[]);
        a.skip_requested = true;
        let mut b = snapshot("stg-a", "stg", "", "", &[]);
        b.skip_requested = true;
        let c = snapshot("prd-c", "prd", "", "", &[]);
        let (all, gated) = exempt_names(&cfg, &[a, b, c]);
        assert_eq!(all, vec!["prd-b", "stg-a"]);
        assert_eq!(gated, vec!["prd-b"]);
    }

    #[test]
    fn truncate_names_caps_the_list() {
        let short: Vec<String> = (0..3).map(|i| i.to_string()).collect();
        assert_eq!(truncate_names(&short), short);
        let long: Vec<String> = (0..25).map(|i| i.to_string()).collect();
        let out = truncate_names(&long);
        assert_eq!(out.len(), 21);
        assert_eq!(out[20], "and 5 more");
        assert_eq!(or_none(" "), "<none>");
        assert_eq!(or_none("x"), "x");
    }

    #[tokio::test]
    async fn startup_reports_do_not_fail() {
        let cfg = Config::parse("chain: [stg, prd]\n").unwrap();
        log_gate_config(&cfg);
        let mut app = snapshot("prd-a", "prd", "", "", &[]);
        app.skip_requested = true;
        log_exempt_applications(&cfg, &FakeReader::with(vec![app])).await;
        log_exempt_applications(
            &cfg,
            &FakeReader {
                fail: true,
                ..FakeReader::default()
            },
        )
        .await;
        let disabled = Config::parse("chain: [stg, prd]\nimageTag:\n  enabled: false\n").unwrap();
        log_gate_config(&disabled);

        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("token"), "t").unwrap();
        let argocd = crate::config::ArgoCd {
            server_address: "http://127.0.0.1:1".into(),
            ca_file: String::new(),
            token_path: dir.path().join("token").display().to_string(),
            ..crate::config::ArgoCd::default()
        };
        let client = DesiredImageClient::new(&argocd, &[]).unwrap();
        probe_token(&cfg, &client).await;

        assert!(kube_client("/nonexistent/kubeconfig").await.is_err());
    }
}
