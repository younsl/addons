//! Wires the collaborators together and runs the controller until signalled.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Instant;

use anyhow::Result;
use tokio::signal;
use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn};

use crate::awsx::Clients;
use crate::config::{Config, GROW_MODE_ABSOLUTE};
use crate::controller;
use crate::humanize::{go_duration, round_duration};
use crate::k8s::events::{Emitter, KubeEvents};
use crate::k8s::leader::{self, KubeLeases, LeaderConfig, Timing};
use crate::k8s::nodes::KubeNodes;
use crate::observability::server::{metrics_router, serve};
use crate::observability::{Health, Metrics};
use crate::policy::Resolver;
use crate::promql;
use crate::pvscan::{self, Scanner};
use crate::recstore::Store;
use crate::resizer::{self, Options as ResizerOptions, Resizer};
use crate::sinks::{build_sinks, run_preflight};
use crate::throughput::{self, Probe, Recommender};

/// Loads the collaborators and runs the controller until SIGINT or SIGTERM.
#[allow(clippy::too_many_lines)]
pub async fn run(cfg: Config) -> Result<()> {
    let cfg = Arc::new(cfg);
    info!(
        version = env!("CARGO_PKG_VERSION"),
        commit = env!("BUILD_COMMIT"),
        region = %cfg.region,
        reconcile_interval = %go_duration(cfg.reconcile_interval),
        dry_run = cfg.dry_run,
        "starting external-ebs-autoresizer"
    );
    log_resize_policy(&cfg);
    log_piggyback_policy(&cfg);
    log_unused_volume_scan_policy(&cfg);

    // Per-instance-group resize policies. Validation failures are fatal so a
    // broken policy entry never silently falls back to the global settings.
    let resolver = Arc::new(Resolver::new(&cfg).map_err(|err| {
        error!(error = %err, "policy configuration error");
        err
    })?);
    if !resolver.is_empty() {
        info!(count = resolver.len(), policies = ?resolver.summaries(), "instance-group resize policies loaded");
    }
    log_alert_policies(&cfg, &resolver);

    let shutdown = CancellationToken::new();
    spawn_signal_handler(shutdown.clone());

    let mut clients = Clients::new(&cfg.region).await;
    clients.poll_interval = cfg.ssm_poll_interval;
    let clients = Arc::new(clients);

    let metrics = Arc::new(Metrics::new());
    let health = Arc::new(Health::default());

    // One in-cluster client serves every Kubernetes collaborator. Outside a
    // cluster (running the binary locally) they are disabled one by one and
    // the resize loop runs on alone.
    let kube = match crate::k8s::in_cluster_client() {
        Ok(c) => Some(c),
        Err(err) => {
            warn!(
                error = format!("{err:#}"),
                "no in-cluster Kubernetes access; Events, leader election, the recommender, and the unused volume scan are disabled"
            );
            None
        }
    };

    let sinks = build_sinks(&cfg, kube.as_ref(), &shutdown).await;

    {
        let (router, port, token) = (health.router(), cfg.health_port, shutdown.clone());
        tokio::spawn(async move {
            if let Err(err) = serve(router, port, token).await {
                error!(error = format!("{err:#}"), "health server failed");
            }
        });
    }
    {
        let (router, port, token) = (
            metrics_router(metrics.clone()),
            cfg.metrics_port,
            shutdown.clone(),
        );
        tokio::spawn(async move {
            if let Err(err) = serve(router, port, token).await {
                error!(error = format!("{err:#}"), "metrics server failed");
            }
        });
    }

    // The hand-off store exists whenever the recommender does: it is how the
    // resize loop learns which Kubernetes Node a volume belongs to (for Node
    // events) and what throughput it should have (for applyOnResize).
    // Whether a recommendation is ever applied stays gated by
    // throughputRecommendation.applyOnResize inside the resizer.
    let store = cfg
        .throughput_recommendation
        .enabled
        .then(|| Arc::new(Store::new()));

    // One emitter serves every loop that writes Events about an object the
    // addon does not own: it can reach a Node, a cluster-scoped volume, and a
    // namespaced claim alike. The unused volume scanner always runs, so it is
    // always built.
    let object_events = kube
        .as_ref()
        .map(|c| Emitter::new(Arc::new(KubeEvents::new(c.clone()))));
    if object_events.is_none() {
        warn!(
            "Event publishing on Nodes, claims, and volumes disabled: no in-cluster Kubernetes access"
        );
    }

    let resizer = Arc::new(Resizer::new(
        cfg.clone(),
        clients.clone(),
        clients.clone(),
        metrics.clone(),
        ResizerOptions {
            resolver: Some(resolver),
            events: sinks.events.clone(),
            notifier: sinks.notifier.clone(),
            annotator: sinks.annotator.clone(),
            recs: store.clone(),
            node_events: object_events.clone(),
        },
    ));
    let recommender = build_recommender(
        &cfg,
        kube.as_ref(),
        clients.clone(),
        metrics.clone(),
        store,
        object_events.clone(),
        &shutdown,
    )
    .await
    .map(Arc::new);
    let scanner =
        build_scanner(&cfg, kube.as_ref(), metrics.clone(), object_events.clone()).map(Arc::new);
    health.set_ready(true);

    // The recommender runs on its own interval alongside the resize loop,
    // because its observation window spans days. The scanner is a third loop
    // on its own interval again. All loops live under the same leader
    // election, so only one replica annotates Nodes.
    let run_loop = |term: CancellationToken| {
        let (cfg, metrics, resizer, recommender, scanner) = (
            cfg.clone(),
            metrics.clone(),
            resizer.clone(),
            recommender.clone(),
            scanner.clone(),
        );
        async move {
            // Every loop runs only here, which is only on the leader. The
            // gauge says which replica that is.
            metrics.set_leader(true);
            let mut tasks = tokio::task::JoinSet::new();
            if let Some(rcm) = recommender {
                let (m, t) = (metrics.clone(), term.clone());
                let interval = cfg.throughput_recommendation.interval;
                tasks.spawn(async move {
                    controller::run(
                        interval,
                        t,
                        || {
                            let (m, rcm) = (m.clone(), rcm.clone());
                            async move {
                                m.observe_recommender_reconcile();
                                rcm.reconcile().await
                            }
                        },
                        "throughput_recommender",
                    )
                    .await;
                });
            }
            if let Some(scn) = scanner {
                let (m, t) = (metrics.clone(), term.clone());
                tasks.spawn(async move {
                    controller::run(
                        pvscan::INTERVAL,
                        t,
                        || {
                            let (m, scn) = (m.clone(), scn.clone());
                            async move {
                                // The counter is incremented before the pass and
                                // therefore counts attempts, not outcomes.
                                m.observe_unused_scan();
                                let start = Instant::now();
                                let result = scn.reconcile().await;
                                m.observe_unused_scan_result(start.elapsed(), result.is_err());
                                result
                            }
                        },
                        "unused_volume_scan",
                    )
                    .await;
                });
            }
            controller::run(
                cfg.reconcile_interval,
                term.clone(),
                || {
                    let (m, rsz) = (metrics.clone(), resizer.clone());
                    async move {
                        m.observe_reconcile();
                        rsz.reconcile().await
                    }
                },
                "resizer",
            )
            .await;
            while tasks.join_next().await.is_some() {}
            metrics.set_leader(false);
        }
    };

    // Leader election lets the Deployment scale to multiple replicas for HA
    // while only the leader reconciles. Requires in-cluster config; fall back
    // to running directly when disabled or outside a cluster.
    match (cfg.leader_elect && !cfg.pod_name.is_empty(), &kube) {
        (true, Some(client)) => {
            let leases = KubeLeases::new(client.clone(), &cfg.pod_namespace, &cfg.lease_name);
            let leader_cfg = LeaderConfig {
                identity: cfg.pod_name.clone(),
                namespace: cfg.pod_namespace.clone(),
                lease_name: cfg.lease_name.clone(),
                timing: Timing::default(),
            };
            leader::run(&leases, &leader_cfg, shutdown.clone(), run_loop)
                .await
                .map_err(|err| {
                    error!(error = %err, "leader election failed");
                    err
                })?;
        }
        (true, None) => {
            info!("no in-cluster config; leader election disabled, running directly");
            run_loop(shutdown.clone()).await;
        }
        (false, _) => {
            if cfg.leader_elect {
                info!("POD_NAME unset; leader election disabled, running directly");
            }
            run_loop(shutdown.clone()).await;
        }
    }

    health.set_ready(false);
    if let Some(rcm) = &recommender
        && let Some(e) = rcm.events()
    {
        e.shutdown().await;
    }
    if let Some(e) = &object_events {
        e.shutdown().await;
    }
    sinks.shutdown().await;
    info!("shutdown complete");
    Ok(())
}

/// Logs the effective volume growth policy (mode and amount) at INFO so the
/// resize behavior is unambiguous in the Pod's startup logs.
fn log_resize_policy(cfg: &Config) {
    if cfg.grow_mode == GROW_MODE_ABSOLUTE {
        info!(grow_mode = %cfg.grow_mode, grow_amount = %cfg.grow_amount, grow_amount_gib = cfg.grow_amount_gib,
            usage_threshold_percent = cfg.usage_threshold_percent, max_volume_size_gib = cfg.max_volume_size_gib, paused = cfg.paused,
            "Resize policy has been configured to grow each volume by a fixed absolute amount once root filesystem usage crosses the threshold");
    } else {
        info!(grow_mode = %cfg.grow_mode, grow_percent = cfg.grow_percent,
            usage_threshold_percent = cfg.usage_threshold_percent, max_volume_size_gib = cfg.max_volume_size_gib, paused = cfg.paused,
            "Resize policy has been configured to grow each volume by a percentage of its current size once root filesystem usage crosses the threshold");
    }
}

/// Logs, from the resize loop's perspective, whether volume modifications
/// will carry throughput recommendations. Silent when the recommender is
/// disabled: there is nothing to apply and its own startup line already says
/// so.
fn log_piggyback_policy(cfg: &Config) {
    let tr = &cfg.throughput_recommendation;
    if !tr.enabled {
        return;
    }
    if !tr.apply_on_resize {
        info!(
            "Resizer will not apply throughput recommendations; they stay advisory annotations (throughputRecommendation.applyOnResize is disabled)"
        );
        return;
    }
    info!(
        max_recommendation_age = %go_duration(resizer::recommendation_max_age(tr.interval)),
        recommender_interval = %go_duration(tr.interval),
        "Resizer will piggyback fresh throughput increase recommendations onto volume size modifications, spending the same EBS modification slot"
    );
}

/// Logs the scanner's effective settings at INFO. It exists precisely because
/// the scanner has no configuration surface: an operator reading the mounted
/// config file finds nothing about it at all.
fn log_unused_volume_scan_policy(cfg: &Config) {
    info!(
        enabled = true,
        configurable = false,
        interval = %go_duration(pvscan::INTERVAL),
        min_unused_age = %go_duration(pvscan::MIN_UNUSED_AGE),
        namespace_scope = "all",
        reads = ?["persistentvolumeclaims", "persistentvolumes", "pods", "statefulsets"],
        writes = ?["annotations", "events"],
        annotation_prefix = crate::annotations::PREFIX,
        event_reasons = ?["UnusedVolumeDetected", "UnusedVolumeCleared"],
        dry_run = cfg.dry_run,
        "Unused volume scan is enabled and will identify PersistentVolumeClaims and PersistentVolumes that no workload is using, publishing each verdict as object annotations, Kubernetes Events, and Prometheus metrics. It never deletes a claim or a volume"
    );
}

/// Logs which policy buckets (including the implicit default) will send
/// Alertmanager alerts and which are muted. Skipped when alerting is globally
/// disabled.
fn log_alert_policies(cfg: &Config, resolver: &Resolver) {
    if !cfg.alertmanager_enabled {
        return;
    }
    let (enabled, muted) = resolver.alert_policy_names();
    info!(notify_on = %cfg.alertmanager_notify_on, alert_enabled_policies = ?enabled, alert_muted_policies = ?muted,
        "alertmanager notifications configured per policy");
}

/// Constructs the recommender, or `None` when it is disabled or cannot run.
/// A `None` is not an error: the recommender is auxiliary, so a process
/// without in-cluster access still runs the resize loop.
async fn build_recommender(
    cfg: &Config,
    kube: Option<&kube::Client>,
    ec2: Arc<Clients>,
    rec: Arc<Metrics>,
    sink: Option<Arc<Store>>,
    node_events: Option<Emitter>,
    shutdown: &CancellationToken,
) -> Option<Recommender> {
    let tr = &cfg.throughput_recommendation;
    if !tr.enabled {
        info!("EBS throughput recommendation disabled");
        return None;
    }
    let Some(client) = kube else {
        error!("EBS throughput recommendation disabled: no in-cluster Kubernetes access");
        return None;
    };
    let prom = promql::Client::new(
        &tr.prometheus_url,
        &tr.prometheus_tenant_id,
        throughput::QUERY_TIMEOUT,
        query_headers(&tr.prometheus_bearer_token),
    );
    info!(
        prometheus_url = %tr.prometheus_url,
        tenant_id = %tr.prometheus_tenant_id,
        bearer_token_set = !tr.prometheus_bearer_token.is_empty(),
        interval = %go_duration(tr.interval),
        lookback_window = %tr.lookback_window,
        metric_node_name_label = %tr.metric_node_name_label,
        annotation_prefix = throughput::ANNOTATION_PREFIX,
        apply_on_resize = tr.apply_on_resize,
        dry_run = cfg.dry_run,
        "EBS throughput recommendation enabled"
    );
    // The preflight both verifies connectivity and pins the API path prefix,
    // which is what makes the same prometheusUrl work against Prometheus and
    // Mimir.
    run_preflight(
        "prometheus",
        || prom.preflight(),
        shutdown,
        std::time::Duration::from_secs(2),
    )
    .await;

    let recommender = Recommender::new(
        recommender_config(cfg),
        Arc::new(KubeNodes::new(client.clone())),
        Arc::new(prom),
        ec2,
        rec,
        node_events,
        sink,
    );
    // The probe logs the query it actually ran, which is the one an operator
    // pastes into Grafana. It cannot be logged before this point: the query
    // is scoped to this cluster's node names.
    log_probe(&recommender, &tr.metric_node_name_label).await;
    Some(recommender)
}

/// The static headers for every query. The only one is the bearer token for
/// a gateway fronting the metrics backend, which arrives from a Secret
/// through `PROMETHEUS_BEARER_TOKEN` rather than the config file.
fn query_headers(bearer_token: &str) -> BTreeMap<String, String> {
    if bearer_token.is_empty() {
        BTreeMap::new()
    } else {
        BTreeMap::from([(
            "Authorization".to_string(),
            format!("Bearer {bearer_token}"),
        )])
    }
}

/// Runs the one-time metrics check and logs what came back. It never blocks
/// startup: a backend that is briefly unavailable is no reason to keep the
/// recommender from retrying on its own interval. It runs on every replica,
/// before leader election, deliberately: a standby that cannot read the
/// metrics backend is a problem worth seeing at startup rather than at
/// failover.
async fn log_probe(r: &Recommender, metric_node_name_label: &str) {
    let result = tokio::time::timeout(throughput::QUERY_TIMEOUT, r.probe()).await;
    let (probe, err) = match result {
        Ok(Ok(p)) => (p, None),
        Ok(Err((p, err))) => (p, Some(err)),
        Err(_) => (Probe::default(), Some("timed out".to_string())),
    };
    let latency = go_duration(round_duration(
        probe.latency,
        std::time::Duration::from_millis(1),
    ));
    if let Some(err) = err {
        error!(error = %err, latency = %latency, query = %probe.query,
            "EBS throughput recommendation metrics check failed; the recommender will retry on its interval");
        return;
    }
    let busiest_peak = (probe.max_peak_mibps * 10.0).round() / 10.0;
    if probe.nodes == 0 && probe.series > 0 {
        // Series came back but none carried the node label, which is the one
        // misconfiguration every connectivity check passes.
        error!(nodes = probe.nodes, series = probe.series, latency = %latency, metric_node_name_label,
            hint = "set throughputRecommendation.metricNodeNameLabel to the label carrying the Kubernetes node name (kube-prometheus-stack relabels it to \"node\"; a plain node exporter scrape leaves only \"instance\")",
            "EBS throughput recommendation metrics check returned no usable series: the configured node label is missing from every result");
    } else if probe.nodes == 0 {
        warn!(nodes = probe.nodes, series = probe.series, latency = %latency, backend_series = probe.backend_series,
            hint = "check that node exporter is scraped into this backend and that deviceRegex matches this cluster's block devices",
            "EBS throughput recommendation metrics check returned no data");
    } else {
        info!(nodes = probe.nodes, series = probe.series, latency = %latency, busiest_node = %probe.max_node, busiest_node_peak_mibps = busiest_peak,
            "EBS throughput recommendation metrics check succeeded");
    }
}

/// Maps the config file block onto the recommender's own configuration. The
/// recommender inherits the global dry run: it never mutates AWS, but
/// annotating Nodes is still a cluster write.
fn recommender_config(cfg: &Config) -> throughput::Config {
    let tr = &cfg.throughput_recommendation;
    throughput::Config {
        metric_node_name_label: tr.metric_node_name_label.clone(),
        lookback: tr.lookback_window.clone(),
        lookback_duration: tr.lookback_duration,
        dry_run: cfg.dry_run,
    }
}

/// Constructs the scanner, or `None` when it cannot run. The scanner has no
/// enable switch: it never mutates a claim or a volume, its cost is four list
/// calls an hour, and an addon that already runs inside the cluster is the
/// natural place for the report.
fn build_scanner(
    cfg: &Config,
    kube: Option<&kube::Client>,
    rec: Arc<Metrics>,
    events: Option<Emitter>,
) -> Option<Scanner> {
    let Some(client) = kube else {
        error!(
            "Unused volume scan will not run: no in-cluster Kubernetes access. The resize loop is unaffected"
        );
        return None;
    };
    info!(
        events = events.is_some(),
        dry_run = cfg.dry_run,
        "Unused volume scan loop ready"
    );
    Some(Scanner::new(
        cfg.dry_run,
        Arc::new(pvscan::KubeClient::new(client.clone())),
        rec,
        events,
    ))
}

fn spawn_signal_handler(shutdown: CancellationToken) {
    tokio::spawn(async move {
        let ctrl_c = async {
            if let Err(err) = signal::ctrl_c().await {
                error!(error = %err, "failed to listen for SIGINT");
                std::future::pending::<()>().await;
            }
        };
        let terminate = async {
            match signal::unix::signal(signal::unix::SignalKind::terminate()) {
                Ok(mut sig) => {
                    sig.recv().await;
                }
                Err(err) => {
                    error!(error = %err, "failed to listen for SIGTERM");
                    std::future::pending::<()>().await;
                }
            }
        };
        tokio::select! {
            () = ctrl_c => {},
            () = terminate => {},
        }
        info!("received shutdown signal, stopping");
        shutdown.cancel();
    });
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;
    use crate::config::GROW_MODE_PERCENT;

    fn cfg() -> Config {
        Config {
            region: "r".into(),
            grow_mode: GROW_MODE_PERCENT.into(),
            grow_percent: 10,
            grow_amount: "10GiB".into(),
            grow_amount_gib: 10,
            alertmanager_enabled: true,
            alertmanager_notify_on: "success".into(),
            ..Config::default()
        }
    }

    #[test]
    fn startup_logging_covers_both_modes() {
        let mut c = cfg();
        log_resize_policy(&c);
        c.grow_mode = GROW_MODE_ABSOLUTE.into();
        log_resize_policy(&c);
        log_piggyback_policy(&c);
        c.throughput_recommendation.enabled = true;
        c.throughput_recommendation.apply_on_resize = false;
        log_piggyback_policy(&c);
        c.throughput_recommendation.apply_on_resize = true;
        c.throughput_recommendation.interval = Duration::from_mins(30);
        log_piggyback_policy(&c);
        log_unused_volume_scan_policy(&c);
        let resolver = Resolver::new(&c).unwrap();
        log_alert_policies(&c, &resolver);
        c.alertmanager_enabled = false;
        log_alert_policies(&c, &resolver);
    }

    #[test]
    fn recommender_config_and_headers() {
        let mut c = cfg();
        c.dry_run = true;
        c.throughput_recommendation.metric_node_name_label = "instance".into();
        c.throughput_recommendation.lookback_window = "12h".into();
        c.throughput_recommendation.lookback_duration = Duration::from_hours(12);
        let rc = recommender_config(&c);
        assert_eq!(rc.metric_node_name_label, "instance");
        assert_eq!(rc.lookback, "12h");
        assert_eq!(rc.lookback_duration, Duration::from_hours(12));
        assert!(rc.dry_run);
        assert!(query_headers("").is_empty());
        assert_eq!(query_headers("tok")["Authorization"], "Bearer tok");
    }

    #[tokio::test]
    async fn builders_without_a_cluster() {
        let c = cfg();
        let metrics = Arc::new(Metrics::new());
        assert!(build_scanner(&c, None, metrics.clone(), None).is_none());
        let clients = Arc::new(Clients::from_parts(
            Box::new(crate::awsx::fake::FakeEc2::default()),
            Box::new(crate::awsx::fake::FakeSsm::default()),
        ));
        assert!(
            build_recommender(
                &c,
                None,
                clients.clone(),
                metrics.clone(),
                None,
                None,
                &CancellationToken::new()
            )
            .await
            .is_none(),
            "disabled"
        );
        let mut c = cfg();
        c.throughput_recommendation.enabled = true;
        assert!(
            build_recommender(
                &c,
                None,
                clients,
                metrics,
                None,
                None,
                &CancellationToken::new()
            )
            .await
            .is_none(),
            "no cluster"
        );
    }

    #[tokio::test]
    async fn log_probe_outcomes() {
        use crate::throughput::tests_support::{probe_recommender, sample};
        // Every branch of the startup log is exercised against a fake backend.
        let (r, _) = probe_recommender(
            vec![("quantile_over_time", Ok(vec![sample("a", 10.0)]))],
            true,
        );
        log_probe(&r, "node").await;
        let (r, _) = probe_recommender(
            vec![(
                "quantile_over_time",
                Ok(vec![crate::promql::Sample {
                    labels: BTreeMap::new(),
                    value: 1.0,
                }]),
            )],
            true,
        );
        log_probe(&r, "node").await;
        let (r, _) = probe_recommender(vec![], true);
        log_probe(&r, "node").await;
        let (r, _) = probe_recommender(vec![("quantile_over_time", Err("timeout".into()))], true);
        log_probe(&r, "node").await;
    }
}
