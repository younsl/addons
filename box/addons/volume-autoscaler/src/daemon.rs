//! Wires the collaborators together and runs the controller until signalled.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Instant;

use anyhow::Result;
use tokio::signal;
use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn};

use crate::autoscaler::instance_volume::api::{AutoscalerApi, KubeAutoscalers, run_validator};
use crate::autoscaler::instance_volume::policy::Resolver;
use crate::autoscaler::instance_volume::resizer::{self, Options as ResizerOptions, Resizer};
use crate::autoscaler::persistent_volume_claim::api::KubeClaims;
use crate::autoscaler::persistent_volume_claim::cache::Caches;
use crate::autoscaler::persistent_volume_claim::reconciler::Context as ClaimContext;
use crate::awsx::Clients;
use crate::config::Config;
use crate::controller;
use crate::humanize::{go_duration, round_duration};
use crate::k8s::cordon::{CONDITION_TYPE, CordonApi, CordonNode, KubeCordon, annotation_key};
use crate::k8s::events::{Emitter, KubeEvents};
use crate::k8s::leader::{self, KubeLeases, LeaderConfig, Timing};
use crate::k8s::nodes::KubeNodes;
use crate::observability::server::{metrics_router, serve};
use crate::observability::{Health, Metrics};
use crate::promql;
use crate::pvscan::{self, Scanner};
use crate::recstore::Store;
use crate::sinks::{Sinks, build_sinks, run_preflight};
use crate::throughput::{self, Probe, Recommender};

/// The timeout of one claim utilization query.
const CLAIM_QUERY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// A controller the process can run, as named on `--controllers`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, clap::ValueEnum)]
pub enum ControllerName {
    #[value(
        help = "Grow EC2 root volumes InstanceVolumeAutoscalers target, plus the throughput recommender when enabled"
    )]
    InstanceVolumeAutoscaler,
    #[value(help = "Grow the PersistentVolumeClaims PersistentVolumeClaimAutoscalers target")]
    PersistentVolumeClaimAutoscaler,
    #[value(help = "Report PersistentVolumeClaims and PersistentVolumes no workload uses")]
    UnusedVolumeScan,
}

/// The controllers enabled on this process.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[allow(clippy::struct_excessive_bools)]
pub struct Controllers {
    pub instance_volume_autoscaler: bool,
    pub persistent_volume_claim_autoscaler: bool,
    pub unused_volume_scan: bool,
}

impl Controllers {
    #[must_use]
    pub fn of(names: &[ControllerName]) -> Self {
        Self {
            instance_volume_autoscaler: names.contains(&ControllerName::InstanceVolumeAutoscaler),
            persistent_volume_claim_autoscaler: names
                .contains(&ControllerName::PersistentVolumeClaimAutoscaler),
            unused_volume_scan: names.contains(&ControllerName::UnusedVolumeScan),
        }
    }

    const fn any(self) -> bool {
        self.instance_volume_autoscaler
            || self.persistent_volume_claim_autoscaler
            || self.unused_volume_scan
    }
}

/// Loads the collaborators and runs the enabled controllers until SIGINT or
/// SIGTERM.
#[allow(clippy::too_many_lines)]
pub async fn run(cfg: Config, enabled: Controllers) -> Result<()> {
    let cfg = Arc::new(cfg);
    info!(
        version = env!("CARGO_PKG_VERSION"),
        commit = env!("BUILD_COMMIT"),
        rust_version = env!("BUILD_RUSTC_VERSION"),
        region = %cfg.region,
        reconcile_interval = %go_duration(cfg.reconcile_interval),
        claim_reconcile_interval = %go_duration(cfg.claim_reconcile_interval),
        dry_run = cfg.dry_run,
        instance_volume_autoscaler = enabled.instance_volume_autoscaler,
        persistent_volume_claim_autoscaler = enabled.persistent_volume_claim_autoscaler,
        unused_volume_scan = enabled.unused_volume_scan,
        "starting volume-autoscaler"
    );
    if !enabled.any() {
        anyhow::bail!("--controllers enables nothing");
    }
    if enabled.persistent_volume_claim_autoscaler && cfg.metrics.url.is_empty() {
        anyhow::bail!(
            "metrics.url is required by the persistent-volume-claim-autoscaler controller"
        );
    }
    if enabled.instance_volume_autoscaler {
        log_piggyback_policy(&cfg);
    }
    if enabled.unused_volume_scan {
        log_unused_volume_scan_policy(&cfg);
    }

    let shutdown = CancellationToken::new();
    spawn_signal_handler(shutdown.clone());

    let metrics = Arc::new(Metrics::new());
    let health = Arc::new(Health::default());

    // One client serves every Kubernetes collaborator. Every controller reads
    // its settings from custom resources, so without the API there is
    // nothing to reconcile.
    let client = crate::k8s::client().await.map_err(|err| {
        error!(
            error = format!("{err:#}"),
            "Kubernetes API access is required to read autoscaler resources"
        );
        err
    })?;
    let kube = Some(client.clone());

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

    // One emitter serves every loop that writes Events about an object the
    // addon does not own: it can reach a Node, a cluster-scoped volume, and a
    // namespaced claim alike.
    let object_events = Some(Emitter::new(Arc::new(KubeEvents::new(client.clone()))));

    let instance = if enabled.instance_volume_autoscaler {
        Some(
            build_instance_volume_autoscaler(
                &cfg,
                &client,
                metrics.clone(),
                object_events.clone(),
                &shutdown,
            )
            .await,
        )
    } else {
        info!("InstanceVolumeAutoscaler controller disabled by --controllers");
        None
    };
    let claim = if enabled.persistent_volume_claim_autoscaler {
        Some(
            build_persistent_volume_claim_autoscaler(
                &cfg,
                &client,
                metrics.clone(),
                object_events.clone(),
                &shutdown,
            )
            .await,
        )
    } else {
        info!("PersistentVolumeClaimAutoscaler controller disabled by --controllers");
        None
    };
    let scanner = if enabled.unused_volume_scan {
        build_scanner(&cfg, kube.as_ref(), metrics.clone(), object_events.clone()).map(Arc::new)
    } else {
        info!("Unused volume scan disabled by --controllers");
        None
    };
    let sinks = instance.as_ref().map(|i| i.sinks.clone());
    let recommender = instance.as_ref().and_then(|i| i.recommender.clone());
    health.set_ready(true);

    // Every loop runs on its own interval under the same leader election, so
    // only one replica writes to AWS or the cluster.
    let run_loop = |term: CancellationToken| {
        let (cfg, metrics, instance, claim, scanner, client) = (
            cfg.clone(),
            metrics.clone(),
            instance.clone(),
            claim.clone(),
            scanner.clone(),
            client.clone(),
        );
        async move {
            // Every loop runs only here, which is only on the leader. The
            // gauge says which replica that is.
            metrics.set_leader(true);
            let mut tasks = tokio::task::JoinSet::new();
            if let Some(inst) = instance {
                spawn_instance_loops(&mut tasks, &cfg, &metrics, inst, client.clone(), &term);
            }
            if let Some(ctx) = claim {
                tasks.spawn(crate::autoscaler::persistent_volume_claim::reconciler::run(
                    client,
                    ctx,
                    term.clone(),
                ));
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
            while tasks.join_next().await.is_some() {}
            metrics.set_leader(false);
        }
    };

    // Leader election lets the Deployment scale to multiple replicas for HA
    // while only the leader reconciles.
    if cfg.leader_elect && !cfg.pod_name.is_empty() {
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
    } else {
        if cfg.leader_elect {
            info!("POD_NAME unset; leader election disabled, running directly");
        }
        run_loop(shutdown.clone()).await;
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
    if let Some(sinks) = sinks {
        sinks.shutdown().await;
    }
    info!("shutdown complete");
    Ok(())
}

/// The collaborators of the `InstanceVolumeAutoscaler` controller.
#[derive(Clone)]
struct InstanceLoops {
    resizer: Arc<Resizer>,
    recommender: Option<Arc<Recommender>>,
    sinks: Arc<Sinks>,
}

async fn build_instance_volume_autoscaler(
    cfg: &Arc<Config>,
    client: &kube::Client,
    metrics: Arc<Metrics>,
    object_events: Option<Emitter>,
    shutdown: &CancellationToken,
) -> InstanceLoops {
    let mut clients = Clients::new(&cfg.region).await;
    clients.poll_interval = cfg.ssm_poll_interval;
    let clients = Arc::new(clients);
    let autoscalers: Arc<dyn AutoscalerApi> = Arc::new(KubeAutoscalers::new(client.clone()));
    log_autoscalers(autoscalers.as_ref(), cfg).await;
    let kube = Some(client.clone());
    let sinks = Arc::new(build_sinks(cfg, kube.as_ref(), shutdown).await);

    // The hand-off store exists whenever the recommender does: it is how the
    // resize loop learns which Kubernetes Node a volume belongs to (for Node
    // events) and what throughput it should have (for applyOnResize).
    // Whether a recommendation is ever applied stays gated by
    // throughputRecommendation.applyOnResize inside the resizer.
    let store = cfg
        .throughput_recommendation
        .enabled
        .then(|| Arc::new(Store::new()));

    let cordon = build_cordon(cfg, kube.as_ref());
    if let Some(api) = &cordon {
        log_cordon_detection(api.as_ref()).await;
    }
    let resizer = Arc::new(Resizer::new(
        cfg.clone(),
        clients.clone(),
        clients.clone(),
        metrics.clone(),
        ResizerOptions {
            autoscalers: Some(autoscalers),
            events: sinks.events.clone(),
            notifier: sinks.notifier.clone(),
            annotator: sinks.annotator.clone(),
            recs: store.clone(),
            node_events: object_events.clone(),
            autoscaler_events: object_events.clone(),
            cordon,
        },
    ));
    let recommender = build_recommender(
        cfg,
        kube.as_ref(),
        clients,
        metrics,
        store,
        object_events,
        shutdown,
    )
    .await
    .map(Arc::new);
    InstanceLoops {
        resizer,
        recommender,
        sinks,
    }
}

/// Spawns the spec validator, the resize loop, and the recommender.
fn spawn_instance_loops(
    tasks: &mut tokio::task::JoinSet<()>,
    cfg: &Arc<Config>,
    metrics: &Arc<Metrics>,
    inst: InstanceLoops,
    client: kube::Client,
    term: &CancellationToken,
) {
    tasks.spawn(run_validator(client, term.clone()));
    if let Some(rcm) = inst.recommender {
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
    let (m, t, rsz, interval) = (
        metrics.clone(),
        term.clone(),
        inst.resizer,
        cfg.reconcile_interval,
    );
    tasks.spawn(async move {
        controller::run(
            interval,
            t,
            || {
                let (m, rsz) = (m.clone(), rsz.clone());
                async move {
                    m.observe_reconcile();
                    rsz.reconcile().await
                }
            },
            "resizer",
        )
        .await;
    });
}

/// Builds the `PersistentVolumeClaimAutoscaler` controller context and checks
/// the metrics backend once. A failed check is not fatal: every reconcile
/// queries again and reports `FailedGetResourceMetric` meanwhile.
async fn build_persistent_volume_claim_autoscaler(
    cfg: &Config,
    client: &kube::Client,
    metrics: Arc<Metrics>,
    events: Option<Emitter>,
    shutdown: &CancellationToken,
) -> Arc<ClaimContext> {
    let prom = metrics_client(cfg, CLAIM_QUERY_TIMEOUT);
    info!(
        metrics_url = %cfg.metrics.url,
        tenant_id = %cfg.metrics.tenant_id,
        label_selector = ?cfg.metrics.label_selector,
        bearer_token_set = !cfg.metrics.bearer_token.is_empty(),
        interval = %go_duration(cfg.claim_reconcile_interval),
        dry_run = cfg.dry_run,
        "PersistentVolumeClaimAutoscaler controller enabled"
    );
    run_preflight(
        "prometheus",
        || prom.preflight(),
        shutdown,
        std::time::Duration::from_secs(2),
    )
    .await;
    Arc::new(ClaimContext {
        api: Arc::new(KubeClaims::new(
            client.clone(),
            Caches::start(client, shutdown),
        )),
        usage: Arc::new(prom),
        label_selector: cfg.metrics.label_selector.clone(),
        dry_run: cfg.dry_run,
        interval: cfg.claim_reconcile_interval,
        metrics,
        events,
    })
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

/// Lists the autoscalers once at startup and logs what the resize loop will
/// act on. Like the cordon check it runs on every replica before leader
/// election, so a missing CRD or RBAC grant shows at startup. A failure is not
/// fatal: every pass lists again.
async fn log_autoscalers(api: &dyn AutoscalerApi, cfg: &Config) {
    match api.list().await {
        Ok(ivas) => {
            let r = Resolver::new(&ivas);
            let alert_muted: Vec<String> = r
                .autoscalers
                .iter()
                .filter(|c| !c.effective.alert_enabled)
                .map(|c| c.effective.policy.clone())
                .collect();
            let targets: Vec<String> = r
                .autoscalers
                .iter()
                .map(|c| format!("{}={}", c.effective.policy, c.target))
                .collect();
            info!(count = ivas.len(), valid = r.autoscalers.len(), invalid = r.invalid.len(), targets = ?targets,
                alertmanager_enabled = cfg.alertmanager_enabled, alert_muted = ?alert_muted,
                "InstanceVolumeAutoscalers loaded; instances no autoscaler targets are left alone");
        }
        Err(err) => {
            error!(error = %err, "failed to list InstanceVolumeAutoscalers; check that the CRD is installed and the ClusterRole grants list");
        }
    }
}

/// Constructs the protective cordon client. It is always on for every
/// targeted instance that is a Node, so `None` only means there is no
/// cluster to cordon in.
fn build_cordon(cfg: &Config, kube: Option<&kube::Client>) -> Option<Arc<dyn CordonApi>> {
    let Some(client) = kube else {
        error!(
            enabled = false,
            reason = "no in-cluster Kubernetes access",
            "Protective cordon disabled: EKS nodes are resized but cannot be cordoned"
        );
        return None;
    };
    info!(enabled = true, annotation = %annotation_key(), condition = CONDITION_TYPE, dry_run = cfg.dry_run,
        "Protective cordon enabled: a Node whose root filesystem usage is at or above its policy threshold is cordoned, and uncordoned once usage falls back under it. Only cordons the addon applied are ever lifted");
    Some(Arc::new(KubeCordon::new(client.clone())))
}

/// What the startup Node list found.
#[derive(Debug, Default, PartialEq, Eq)]
struct Detection {
    /// EC2-backed Nodes, the ones an instance can map to.
    nodes: usize,
    /// Nodes holding the addon's cordon mark, sorted.
    held: Vec<String>,
    /// Nodes cordoned by someone else, which the addon leaves alone.
    cordoned_by_others: usize,
    /// Nodes whose `ProtectiveCordon` condition is missing or disagrees with
    /// the mark, written on the first pass that measures them.
    condition_out_of_sync: usize,
}

impl Detection {
    fn of<'a>(nodes: impl IntoIterator<Item = &'a CordonNode>) -> Self {
        let mut d = Self::default();
        for n in nodes {
            d.nodes += 1;
            if n.protective {
                d.held.push(n.name.clone());
            } else if n.unschedulable {
                d.cordoned_by_others += 1;
            }
            if n.condition != Some(n.protective) {
                d.condition_out_of_sync += 1;
            }
        }
        d.held.sort();
        d
    }
}

/// Lists the cluster's Nodes once at startup and logs what the protective
/// cordon starts watching. Like the metrics check it runs on every replica
/// before leader election, so a missing `nodes` grant shows at startup
/// rather than at failover. A failure is not fatal: every pass lists again.
async fn log_cordon_detection(api: &dyn CordonApi) {
    match api.list().await {
        Ok(nodes) => {
            let d = Detection::of(nodes.values());
            info!(nodes_detected = d.nodes, held_by_addon = d.held.len(), held_nodes = ?d.held,
                cordoned_by_others = d.cordoned_by_others, condition_out_of_sync = d.condition_out_of_sync,
                "Protective cordon monitoring started");
        }
        Err(err) => error!(error = %err,
            "Protective cordon monitoring started, but the initial Node detection failed; it is retried every pass"),
    }
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
    let prom = metrics_client(cfg, throughput::QUERY_TIMEOUT);
    info!(
        metrics_url = %cfg.metrics.url,
        tenant_id = %cfg.metrics.tenant_id,
        label_selector = ?cfg.metrics.label_selector,
        bearer_token_set = !cfg.metrics.bearer_token.is_empty(),
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

/// The client for the shared metrics backend, cluster-local Prometheus or
/// central Mimir alike.
fn metrics_client(cfg: &Config, timeout: std::time::Duration) -> promql::Client {
    promql::Client::new(
        &cfg.metrics.url,
        &cfg.metrics.tenant_id,
        timeout,
        query_headers(&cfg.metrics.bearer_token),
    )
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
        label_selector: cfg.metrics.label_selector.clone(),
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
    use crate::autoscaler::instance_volume::policy::tests::{by_id, iva};

    fn cfg() -> Config {
        Config {
            region: "r".into(),
            alertmanager_enabled: true,
            alertmanager_notify_on: "success".into(),
            ..Config::default()
        }
    }

    struct Listed(
        Result<Vec<crate::autoscaler::instance_volume::crd::InstanceVolumeAutoscaler>, String>,
    );

    #[async_trait::async_trait]
    impl AutoscalerApi for Listed {
        async fn list(
            &self,
        ) -> Result<Vec<crate::autoscaler::instance_volume::crd::InstanceVolumeAutoscaler>, String>
        {
            self.0.clone()
        }
        async fn patch_status(
            &self,
            _: &str,
            _: &crate::autoscaler::instance_volume::crd::InstanceVolumeAutoscalerStatus,
        ) -> Result<(), String> {
            Ok(())
        }
    }

    #[tokio::test]
    async fn startup_logging() {
        let mut c = cfg();
        let mut muted = iva("muted", by_id("i-12345678"));
        muted.spec.alert_enabled = false;
        log_autoscalers(&Listed(Ok(vec![muted, iva("bad", by_id("x"))])), &c).await;
        log_autoscalers(&Listed(Err("forbidden".into())), &c).await;
        log_piggyback_policy(&c);
        c.throughput_recommendation.enabled = true;
        c.throughput_recommendation.apply_on_resize = false;
        log_piggyback_policy(&c);
        c.throughput_recommendation.apply_on_resize = true;
        c.throughput_recommendation.interval = Duration::from_mins(30);
        log_piggyback_policy(&c);
        log_unused_volume_scan_policy(&c);
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
        assert!(build_cordon(&c, None).is_none(), "no cluster");
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

    #[test]
    fn cordon_detection_summary() {
        let node = |name: &str, unschedulable: bool, protective: bool, condition: Option<bool>| {
            CordonNode {
                name: name.into(),
                uid: String::new(),
                unschedulable,
                protective,
                condition,
            }
        };
        let nodes = [
            node("b", true, true, Some(true)),
            node("a", true, true, None),
            node("c", true, false, None),
            node("d", false, false, Some(true)),
            node("e", false, false, None),
        ];
        assert_eq!(
            Detection::of(&nodes),
            Detection {
                nodes: 5,
                held: vec!["a".into(), "b".into()],
                cordoned_by_others: 1,
                condition_out_of_sync: 4,
            }
        );
        assert_eq!(Detection::of(&[]), Detection::default());
    }

    #[tokio::test]
    async fn log_cordon_detection_outcomes() {
        use crate::autoscaler::instance_volume::resizer::tests::FakeCordon;
        log_cordon_detection(&FakeCordon::default()).await;
        log_cordon_detection(&FakeCordon {
            list_error: Some("forbidden".into()),
            ..FakeCordon::default()
        })
        .await;
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
