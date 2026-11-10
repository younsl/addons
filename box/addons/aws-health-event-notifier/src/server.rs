use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use axum::{Router, extract::State, http::StatusCode, response::IntoResponse, routing::get};
use tokio::signal;
use tokio::sync::watch;
use tokio::task::JoinSet;

use crate::aws::account::AccountIdentity;
use crate::aws::health::HealthClient;
use crate::config::{RunArgs, SlackArgs};
use crate::filter::{Catalogs, EventFilter, FilterLists, ServiceEventCode, validate_filters};
use crate::k8s::client::{Access, K8sEventClient, PodIdentity, can_i};
use crate::k8s::lease::LeaseLock;
use crate::k8s::ledger_store::LedgerStore;
use crate::leader::{Elector, ElectorCfg};
use crate::ledger::journal::Journal;
use crate::notify::{Notifier, SlackOpts};
use crate::observability::metrics::Metrics;
use crate::poller::{Poller, PollerCfg};
use crate::slack::client::SlackClient;

/// RBAC the leader-elected mode needs. `get` and `update` are checked against
/// the named objects, matching a Role scoped with `resourceNames`.
const fn ha_permissions<'a>(lease: &'a str, ledger: &'a str) -> [Access<'a>; 7] {
    [
        access("coordination.k8s.io", "leases", "create", None),
        access("coordination.k8s.io", "leases", "get", Some(lease)),
        access("coordination.k8s.io", "leases", "update", Some(lease)),
        access("", "configmaps", "create", None),
        access("", "configmaps", "get", Some(ledger)),
        access("", "configmaps", "update", Some(ledger)),
        access("events.k8s.io", "events", "create", None),
    ]
}

const fn access<'a>(
    group: &'a str,
    resource: &'a str,
    verb: &'a str,
    name: Option<&'a str>,
) -> Access<'a> {
    Access {
        group,
        resource,
        verb,
        name,
    }
}

/// Kubernetes wiring for leader election. Built before anything else so a
/// misconfigured HA deployment fails at startup instead of running unfenced.
struct HaSetup {
    client: kube::Client,
    namespace: String,
    identity: String,
    pod_uid: Option<String>,
    lease_name: String,
    ledger_name: String,
    election: ElectorCfg,
    lease_duration: Duration,
}

impl HaSetup {
    async fn from_config(run_cfg: &RunArgs) -> anyhow::Result<Option<Self>> {
        let election = &run_cfg.leader_election;
        if !election.enabled {
            return Ok(None);
        }
        election.validate()?;
        let ha = Self::connect(run_cfg).await?;
        tracing::info!(
            identity = %ha.identity,
            lease = %election.lease_name,
            ledger = %election.ledger_configmap,
            "leader election ENABLED"
        );
        Ok(Some(ha))
    }

    async fn connect(run_cfg: &RunArgs) -> anyhow::Result<Self> {
        let (Some(identity), Some(namespace)) = (
            run_cfg.k8s.pod_name.clone(),
            run_cfg.k8s.pod_namespace.clone(),
        ) else {
            anyhow::bail!("leader election requires POD_NAME and POD_NAMESPACE (Downward API)");
        };
        let client = kube::Client::try_default()
            .await
            .context("leader election requires an in-cluster Kubernetes client")?;
        let election = &run_cfg.leader_election;
        let mut missing = Vec::new();
        for access in ha_permissions(&election.lease_name, &election.ledger_configmap) {
            if !can_i(&client, &namespace, &access)
                .await
                .context("leader election RBAC preflight failed")?
            {
                missing.push(format!("{} {}", access.verb, access.resource));
            }
        }
        if !missing.is_empty() {
            anyhow::bail!(
                "leader election RBAC missing in namespace {namespace}: {}. Apply the chart RBAC (rbac.create=true).",
                missing.join(", ")
            );
        }
        Ok(Self {
            client,
            namespace,
            identity,
            pod_uid: run_cfg.k8s.pod_uid.clone(),
            lease_name: run_cfg.leader_election.lease_name.clone(),
            ledger_name: run_cfg.leader_election.ledger_configmap.clone(),
            election: ElectorCfg {
                retry_period: run_cfg.leader_election.retry_period(),
                renew_deadline: run_cfg.leader_election.renew_deadline(),
            },
            lease_duration: run_cfg.leader_election.lease_duration(),
        })
    }

    fn events_client(&self) -> K8sEventClient {
        K8sEventClient::new(
            self.client.clone(),
            PodIdentity {
                name: self.identity.clone(),
                namespace: self.namespace.clone(),
                uid: self.pod_uid.clone(),
            },
        )
    }

    fn journal(&self) -> Journal {
        Journal::configmap(LedgerStore::new(
            self.client.clone(),
            &self.namespace,
            self.ledger_name.clone(),
        ))
    }

    fn elector(self, leader: watch::Sender<bool>, metrics: Metrics) -> Elector {
        let lock = LeaseLock::new(
            self.client,
            &self.namespace,
            self.lease_name,
            self.identity,
            self.lease_duration,
        );
        Elector::new(lock, self.election, leader, metrics)
    }
}

/// Spawns the poller, gated by the elector under leader election. Without
/// election this replica is always the leader.
fn spawn_poller(
    tasks: &mut JoinSet<anyhow::Result<()>>,
    poller: Arc<Poller>,
    ha: Option<HaSetup>,
    metrics: Metrics,
    shutdown: &watch::Receiver<bool>,
) {
    let (leader_tx, leader_rx) = watch::channel(ha.is_none());
    let (poller_done_tx, poller_done_rx) = watch::channel(false);
    let poller_shutdown = shutdown.clone();
    tasks.spawn(async move {
        let result = poller.run(leader_rx, poller_shutdown).await;
        let _ = poller_done_tx.send(true);
        result
    });

    let elector_shutdown = shutdown.clone();
    if let Some(ha) = ha {
        let elector = ha.elector(leader_tx, metrics);
        tasks.spawn(async move { elector.run(elector_shutdown, poller_done_rx).await });
    } else {
        metrics.set_leader(true);
        tasks.spawn(async move {
            let mut rx = elector_shutdown;
            let _ = rx.changed().await;
            drop(leader_tx);
            Ok(())
        });
    }
}

const SEND_RETRY_BACKOFF: Duration = Duration::from_secs(1);

pub async fn run(slack_cfg: SlackArgs, run_cfg: RunArgs) -> anyhow::Result<()> {
    let metrics = Metrics::new();

    let ha = HaSetup::from_config(&run_cfg).await?;

    let slack = SlackClient::new(
        &slack_cfg.slack_webhook_url,
        Duration::from_secs(slack_cfg.slack_timeout_secs),
    )
    .context("failed to construct Slack client")?;

    let filter = EventFilter::new(&filter_lists(&run_cfg));
    tracing::info!(
        allow_categories = ?run_cfg.allow_categories,
        deny_categories = ?run_cfg.deny_categories,
        allow_services = ?run_cfg.allow_services,
        deny_services = ?run_cfg.deny_services,
        allow_regions = ?run_cfg.allow_regions,
        deny_regions = ?run_cfg.deny_regions,
        // Includes the implicit `global`, so the enforced list is never a guess.
        allow_regions_effective = ?filter.effective_allow_regions(),
        allow_event_codes = ?run_cfg.allow_event_codes,
        deny_event_codes = ?run_cfg.deny_event_codes,
        "filter configured"
    );

    let aws = HealthClient::from_env(run_cfg.event_locale.clone())
        .await
        .context("failed to construct AWS Health client")?;

    let admin = Router::new()
        .route("/healthz", get(|| async { StatusCode::OK }))
        .route("/readyz", get(|| async { StatusCode::OK }))
        .route("/metrics", get(metrics_handler))
        .with_state(metrics.clone());

    let admin_listener = tokio::net::TcpListener::bind(&run_cfg.admin_addr)
        .await
        .with_context(|| format!("bind admin addr {}", run_cfg.admin_addr))?;
    tracing::info!(addr = %run_cfg.admin_addr, "admin server listening");

    let (shutdown_tx, shutdown_rx) = watch::channel(false);

    // Bind admin listener before validation so liveness probes succeed
    // while the (potentially slow) AWS Health catalog fetch is running.
    let mut tasks = JoinSet::new();
    let admin_rx = shutdown_rx.clone();
    tasks.spawn(async move {
        axum::serve(admin_listener, admin)
            .with_graceful_shutdown(async move {
                let mut rx = admin_rx;
                let _ = rx.changed().await;
            })
            .await
            .map_err(|e| anyhow::anyhow!("admin server: {e}"))
    });

    validate_against_catalog(&aws, &run_cfg).await?;

    let account = AccountIdentity::resolve().await;
    tracing::info!(
        account_id = account.account_id.as_deref().unwrap_or("?"),
        alias = account.alias.as_deref().unwrap_or("?"),
        "resolved AWS account identity"
    );

    let k8s = match &ha {
        Some(ha) => Some(ha.events_client()),
        None => build_k8s_client(&run_cfg).await,
    };
    let notifier = Notifier::new(
        slack,
        SlackOpts {
            channel: slack_cfg.slack_channel.clone(),
            username: slack_cfg.slack_username.clone(),
            icon_emoji: slack_cfg.slack_icon_emoji.clone(),
            account_label: account.display(),
        },
        k8s,
        metrics.clone(),
    );

    let (journal, identity) = match &ha {
        Some(ha) => (ha.journal(), ha.identity.clone()),
        None => (
            Journal::memory(),
            run_cfg
                .k8s
                .pod_name
                .clone()
                .unwrap_or_else(|| "local".to_string()),
        ),
    };

    let poller = Arc::new(Poller::new(
        aws,
        notifier,
        filter,
        metrics.clone(),
        journal,
        identity,
        poller_cfg(&run_cfg),
    ));

    spawn_poller(&mut tasks, poller, ha, metrics, &shutdown_rx);

    tasks.spawn(async move {
        wait_for_signal().await;
        let _ = shutdown_tx.send(true);
        Ok(())
    });

    while let Some(res) = tasks.join_next().await {
        res.context("task panicked")??;
    }
    Ok(())
}

fn poller_cfg(run_cfg: &RunArgs) -> PollerCfg {
    PollerCfg {
        interval: Duration::from_secs(run_cfg.poll_interval_secs),
        initial_lookback: Duration::from_secs(run_cfg.initial_lookback_secs),
        cold_start_suppress: run_cfg.cold_start_suppress,
        services: run_cfg.allow_services.clone(),
        categories: run_cfg.allow_categories.clone(),
        reminder_offsets_hours: run_cfg.reminder_offsets_hours.clone(),
        retry_backoff: SEND_RETRY_BACKOFF,
    }
}

/// Borrows the configured filter lists off `RunArgs` for the filter and its
/// startup validation, which read exactly the same values.
fn filter_lists(run_cfg: &RunArgs) -> FilterLists<'_> {
    FilterLists {
        allow_categories: &run_cfg.allow_categories,
        deny_categories: &run_cfg.deny_categories,
        allow_services: &run_cfg.allow_services,
        deny_services: &run_cfg.deny_services,
        allow_regions: &run_cfg.allow_regions,
        deny_regions: &run_cfg.deny_regions,
        allow_event_codes: &run_cfg.allow_event_codes,
        deny_event_codes: &run_cfg.deny_event_codes,
    }
}

async fn validate_against_catalog(aws: &HealthClient, run_cfg: &RunArgs) -> anyhow::Result<()> {
    // Query only the service codes and event type codes the user actually
    // configured, instead of paginating the full AWS Health catalog
    // (thousands of event types).
    let mut services_to_query: Vec<String> = run_cfg
        .allow_services
        .iter()
        .chain(run_cfg.deny_services.iter())
        .filter(|s| !s.trim().is_empty())
        .map(|s| s.trim().to_string())
        .collect();
    services_to_query.sort();
    services_to_query.dedup();

    // Event-code entries are `SERVICE/EVENT_TYPE_CODE` pairs; the catalog
    // query takes the code part only. Malformed entries are skipped here and
    // rejected by `validate_filters` below.
    let mut codes_to_query: Vec<String> = run_cfg
        .allow_event_codes
        .iter()
        .chain(run_cfg.deny_event_codes.iter())
        .filter_map(|c| ServiceEventCode::parse(c))
        .map(|p| p.code)
        .collect();
    codes_to_query.sort();
    codes_to_query.dedup();

    let started = std::time::Instant::now();
    let catalogs = Catalogs {
        services: aws
            .lookup_service_codes(&services_to_query)
            .await
            .context("failed to look up AWS Health service codes for filter validation")?,
        event_codes: aws
            .lookup_event_type_codes(&codes_to_query)
            .await
            .context("failed to look up AWS Health event type codes for filter validation")?,
    };
    let elapsed_ms = started.elapsed().as_millis();
    // Regions add no catalog lookup: `DescribeEventTypes` publishes no region
    // list, so they are shape-checked instead.
    let report = validate_filters(&filter_lists(run_cfg), &catalogs);

    tracing::info!(
        elapsed_ms,
        queried_service_count = services_to_query.len(),
        queried_event_code_count = codes_to_query.len(),
        service_catalog_size = catalogs.services.len(),
        event_code_catalog_size = catalogs.event_codes.len(),
        allow_services_valid_count = report.allow_services.valid.len(),
        allow_services_valid = ?report.allow_services.valid,
        allow_services_invalid_count = report.allow_services.invalid.len(),
        allow_services_invalid = ?report.allow_services.invalid,
        deny_services_valid_count = report.deny_services.valid.len(),
        deny_services_valid = ?report.deny_services.valid,
        deny_services_invalid_count = report.deny_services.invalid.len(),
        deny_services_invalid = ?report.deny_services.invalid,
        allow_regions_valid_count = report.allow_regions.valid.len(),
        allow_regions_valid = ?report.allow_regions.valid,
        allow_regions_invalid_count = report.allow_regions.invalid.len(),
        allow_regions_invalid = ?report.allow_regions.invalid,
        deny_regions_valid_count = report.deny_regions.valid.len(),
        deny_regions_valid = ?report.deny_regions.valid,
        deny_regions_invalid_count = report.deny_regions.invalid.len(),
        deny_regions_invalid = ?report.deny_regions.invalid,
        allow_event_codes_valid_count = report.allow_event_codes.valid.len(),
        allow_event_codes_valid = ?report.allow_event_codes.valid,
        allow_event_codes_invalid_count = report.allow_event_codes.invalid.len(),
        allow_event_codes_invalid = ?report.allow_event_codes.invalid,
        deny_event_codes_valid_count = report.deny_event_codes.valid.len(),
        deny_event_codes_valid = ?report.deny_event_codes.valid,
        deny_event_codes_invalid_count = report.deny_event_codes.invalid.len(),
        deny_event_codes_invalid = ?report.deny_event_codes.invalid,
        "filter validation result"
    );

    if !report.is_ok() {
        let invalid = report.all_invalid();
        tracing::error!(
            invalid_count = invalid.len(),
            invalid = ?invalid,
            "filter validation failed: unknown service codes or event type codes, or malformed region codes, detected; please check them again; aborting startup"
        );
        anyhow::bail!(
            "filter validation failed: {} invalid value(s): {}; please check service codes, event type codes, and region codes again",
            invalid.len(),
            invalid.join(", ")
        );
    }
    tracing::info!("filter validation passed");
    Ok(())
}

/// Construct the K8s Event client. Activates automatically when the pod
/// identity is present (Downward API). Non-fatal: any failure logs a clear
/// reason and returns `None` so the poller simply skips emission — Slack
/// (the primary sink) must never be taken down by a K8s problem.
async fn build_k8s_client(run_cfg: &RunArgs) -> Option<K8sEventClient> {
    let (Some(name), Some(namespace)) = (
        run_cfg.k8s.pod_name.clone(),
        run_cfg.k8s.pod_namespace.clone(),
    ) else {
        tracing::info!(
            "Kubernetes Event emission DISABLED: POD_NAME/POD_NAMESPACE not set (not running in-cluster)."
        );
        return None;
    };
    let client = match K8sEventClient::connect(PodIdentity {
        name: name.clone(),
        namespace: namespace.clone(),
        uid: run_cfg.k8s.pod_uid.clone(),
    })
    .await
    {
        Ok(client) => client,
        Err(e) => {
            tracing::warn!(
                error = format!("{e:#}"),
                "Kubernetes Event emission DISABLED: could not initialize the in-cluster Kubernetes client."
            );
            return None;
        }
    };

    // Preflight the RBAC so a missing Role/RoleBinding is reported now, not
    // silently on every poll cycle.
    match client.can_create_events().await {
        Ok(true) => {
            tracing::info!(
                pod = %name,
                namespace = %namespace,
                "Kubernetes Event emission ENABLED: Kubernetes Events will be recorded on this pod."
            );
            Some(client)
        }
        Ok(false) => {
            tracing::warn!(
                namespace = %namespace,
                "Kubernetes Event emission DISABLED: ServiceAccount lacks RBAC to create Kubernetes Events (need verb=create on events.k8s.io/events in this namespace). Apply the chart RBAC (rbac.create=true)."
            );
            None
        }
        Err(e) => {
            tracing::warn!(
                error = format!("{e:#}"),
                "Kubernetes Event emission DISABLED: could not verify RBAC permissions (SelfSubjectAccessReview failed)."
            );
            None
        }
    }
}

async fn metrics_handler(State(metrics): State<Metrics>) -> impl IntoResponse {
    (
        StatusCode::OK,
        [("content-type", "text/plain; version=0.0.4")],
        metrics.render(),
    )
}

async fn wait_for_signal() {
    let ctrl_c = async {
        let _ = signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let terminate = async {
        if let Ok(mut sig) = signal::unix::signal(signal::unix::SignalKind::terminate()) {
            sig.recv().await;
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        () = ctrl_c => {},
        () = terminate => {},
    }
    tracing::info!("shutdown signal received");
}
