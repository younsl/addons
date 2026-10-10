//! Wires the optional observation sinks (Kubernetes Events, Alertmanager
//! alerts, Grafana annotations) from config. Each builder yields `None` when
//! its sink is disabled, so the resizer's checks work.

use std::sync::Arc;
use std::time::Duration;

use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn};

use crate::alertmanager::Preflight;
use crate::config::Config;
use crate::humanize::{go_duration, round_duration};
use crate::k8s::events::{Emitter, KubeEvents, pod_target};
use crate::resizer::{AlertNotifier, Annotator, PodEvents};
use crate::{alertmanager, grafana};

/// The optional per-outcome observation sinks handed to the resizer. Any
/// field may be `None` when the corresponding sink is disabled.
#[derive(Default)]
pub struct Sinks {
    pub events: Option<PodEvents>,
    pub notifier: Option<Arc<dyn AlertNotifier>>,
    pub annotator: Option<Arc<dyn Annotator>>,
}

impl Sinks {
    /// Flushes whatever needs flushing.
    pub async fn shutdown(&self) {
        if let Some(e) = &self.events {
            e.emitter.shutdown().await;
        }
    }
}

/// Constructs every configured sink. `kube` is the in-cluster client when
/// one could be built.
pub async fn build_sinks(
    cfg: &Config,
    kube: Option<&kube::Client>,
    shutdown: &CancellationToken,
) -> Sinks {
    let mut s = Sinks::default();

    // Kubernetes Events about resize attempts attach to this controller's own
    // Pod. Disabled gracefully when not running in-cluster (no downward API).
    if cfg.pod_name.is_empty() {
        info!("POD_NAME unset; Kubernetes Event publishing disabled");
    } else {
        match (
            pod_target(&cfg.pod_name, &cfg.pod_namespace, &cfg.pod_uid),
            kube,
        ) {
            (Ok(target), Some(client)) => {
                s.events = Some(PodEvents {
                    emitter: Emitter::new(Arc::new(KubeEvents::new(client.clone()))),
                    target,
                });
            }
            (Err(err), _) => warn!(error = %err, "Kubernetes Event publishing disabled"),
            (Ok(_), None) => warn!(
                error = "no in-cluster Kubernetes access",
                "Kubernetes Event publishing disabled"
            ),
        }
    }

    // Alertmanager alerting about resize attempts. Disabled unless explicitly
    // enabled.
    if cfg.alertmanager_enabled {
        let client = alertmanager::Client::new(
            &cfg.alertmanager_url,
            cfg.alertmanager_timeout,
            cfg.alertmanager_labels.clone(),
            &cfg.alertmanager_dashboard_url,
        );
        info!(url = %cfg.alertmanager_url, notify_on = %cfg.alertmanager_notify_on, "Alertmanager alerting enabled");
        run_preflight(
            "alertmanager",
            || client.preflight(),
            shutdown,
            PREFLIGHT_BACKOFF,
        )
        .await;
        s.notifier = Some(Arc::new(client));
    } else {
        info!("Alertmanager alerting disabled");
    }

    // Grafana annotations about resize attempts. Disabled unless explicitly
    // enabled. The API token is never logged.
    if cfg.grafana_annotation_enabled {
        let client = grafana::Client::new(
            &cfg.grafana_url,
            &cfg.grafana_api_token,
            cfg.grafana_timeout,
            cfg.grafana_annotation_tags.clone(),
        );
        info!(url = %cfg.grafana_url, annotate_on = %cfg.grafana_annotate_on, tags = ?cfg.grafana_annotation_tags, "Grafana annotations enabled");
        run_preflight(
            "grafana",
            || client.preflight(),
            shutdown,
            PREFLIGHT_BACKOFF,
        )
        .await;
        s.annotator = Some(Arc::new(client));
    } else {
        info!("Grafana annotations disabled");
    }

    s
}

/// How many times a startup connectivity check is tried before it is
/// reported as failed.
const PREFLIGHT_ATTEMPTS: u32 = 3;

/// The delay between preflight retries.
const PREFLIGHT_BACKOFF: Duration = Duration::from_secs(2);

/// Bounds each preflight request.
const PREFLIGHT_TIMEOUT: Duration = Duration::from_secs(10);

/// Runs a best-effort startup connectivity check, retrying up to
/// `PREFLIGHT_ATTEMPTS` times, and logs the outcome. It never blocks startup:
/// a persistent failure is logged at error level so misconfiguration is
/// visible immediately, but the controller still starts because alerting and
/// annotations are auxiliary to the core resize loop.
pub async fn run_preflight<F, Fut>(
    name: &str,
    check: F,
    shutdown: &CancellationToken,
    backoff: Duration,
) where
    F: Fn() -> Fut,
    Fut: Future<Output = Result<Preflight, (Preflight, String)>>,
{
    for attempt in 1..=PREFLIGHT_ATTEMPTS {
        let result = tokio::time::timeout(PREFLIGHT_TIMEOUT, check())
            .await
            .unwrap_or_else(|_| {
                Err((
                    Preflight {
                        endpoint: String::new(),
                        status: 0,
                        latency: PREFLIGHT_TIMEOUT,
                    },
                    "timed out".into(),
                ))
            });
        match result {
            Ok(p) => {
                info!(endpoint = %p.endpoint, status = p.status, latency = %latency(p.latency), attempt, "{name} preflight check succeeded");
                return;
            }
            Err((p, err)) if attempt < PREFLIGHT_ATTEMPTS => {
                warn!(endpoint = %p.endpoint, status = p.status, latency = %latency(p.latency), attempt, max_attempts = PREFLIGHT_ATTEMPTS, error = %err,
                    "{name} preflight check failed, retrying");
                tokio::select! {
                    () = shutdown.cancelled() => return,
                    () = tokio::time::sleep(backoff) => {}
                }
            }
            Err((p, err)) => {
                error!(endpoint = %p.endpoint, status = p.status, latency = %latency(p.latency), attempts = PREFLIGHT_ATTEMPTS, error = %err,
                    "{name} preflight check failed");
            }
        }
    }
}

fn latency(d: Duration) -> String {
    go_duration(round_duration(d, Duration::from_millis(1)))
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;

    fn ok() -> Preflight {
        Preflight {
            endpoint: "http://x/-/healthy".into(),
            status: 200,
            latency: Duration::from_millis(3),
        }
    }

    #[tokio::test]
    async fn preflight_succeeds_first_try() {
        let calls = AtomicUsize::new(0);
        run_preflight(
            "x",
            || {
                calls.fetch_add(1, Ordering::SeqCst);
                async { Ok(ok()) }
            },
            &CancellationToken::new(),
            Duration::from_millis(1),
        )
        .await;
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn preflight_retries_then_fails() {
        let calls = AtomicUsize::new(0);
        run_preflight(
            "x",
            || {
                calls.fetch_add(1, Ordering::SeqCst);
                async { Err((ok(), "down".to_string())) }
            },
            &CancellationToken::new(),
            Duration::from_millis(1),
        )
        .await;
        assert_eq!(calls.load(Ordering::SeqCst), 3);

        let outcomes = Mutex::new(vec![Err((ok(), "down".to_string())), Ok(ok())]);
        let calls = AtomicUsize::new(0);
        run_preflight(
            "x",
            || {
                calls.fetch_add(1, Ordering::SeqCst);
                let next = outcomes.lock().unwrap().remove(0);
                async move { next }
            },
            &CancellationToken::new(),
            Duration::from_millis(1),
        )
        .await;
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn preflight_stops_on_shutdown() {
        let shutdown = CancellationToken::new();
        shutdown.cancel();
        let calls = AtomicUsize::new(0);
        run_preflight(
            "x",
            || {
                calls.fetch_add(1, Ordering::SeqCst);
                async { Err((ok(), "down".to_string())) }
            },
            &shutdown,
            Duration::from_mins(1),
        )
        .await;
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn build_sinks_all_disabled() {
        let cfg = Config::default();
        let s = build_sinks(&cfg, None, &CancellationToken::new()).await;
        assert!(s.events.is_none());
        assert!(s.notifier.is_none());
        assert!(s.annotator.is_none());
        s.shutdown().await;
    }

    #[tokio::test]
    async fn build_sinks_enabled_without_cluster() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .respond_with(wiremock::ResponseTemplate::new(200))
            .mount(&server)
            .await;
        let cfg = Config {
            pod_name: "pod-0".into(),
            pod_namespace: "ns".into(),
            alertmanager_enabled: true,
            alertmanager_url: server.uri(),
            alertmanager_timeout: Duration::from_secs(1),
            alertmanager_notify_on: "success".into(),
            grafana_annotation_enabled: true,
            grafana_url: server.uri(),
            grafana_api_token: "t".into(),
            grafana_timeout: Duration::from_secs(1),
            ..Config::default()
        };
        let s = build_sinks(&cfg, None, &CancellationToken::new()).await;
        assert!(s.events.is_none(), "no in-cluster access");
        assert!(s.notifier.is_some());
        assert!(s.annotator.is_some());
        let cfg = Config {
            pod_name: "pod-0".into(),
            ..Config::default()
        };
        let s = build_sinks(&cfg, None, &CancellationToken::new()).await;
        assert!(s.events.is_none(), "missing namespace");
    }
}
