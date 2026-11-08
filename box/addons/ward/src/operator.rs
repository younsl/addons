//! The operator: reconciles each `Ward` into its agent `DaemonSet`, the
//! optional manager, RBAC, PKI, policy, and the signature database rollout.

mod leader;
pub mod resources;
mod signatures;

use std::collections::{BTreeMap, HashMap};
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use chrono::Utc;
use futures::StreamExt as _;
use k8s_openapi::api::apps::v1::{DaemonSet, StatefulSet};
use k8s_openapi::api::coordination::v1::Lease;
use k8s_openapi::api::core::v1::{ConfigMap, Node, Secret, Service, ServiceAccount};
use k8s_openapi::api::rbac::v1::{ClusterRoleBinding, RoleBinding};
use k8s_openapi::apimachinery::pkg::apis::meta::v1::Condition;
use kube::ResourceExt as _;
use kube::api::{Api, DeleteParams, ListParams, Patch, PatchParams};
use kube::runtime::controller::{Action, Controller};
use kube::runtime::finalizer::{Event as Finalizer, finalizer};
use kube::runtime::reflector::ObjectRef;
use kube::runtime::watcher;
use prometheus_client::encoding::EncodeLabelSet;
use prometheus_client::metrics::counter::Counter;
use prometheus_client::metrics::family::Family;
use prometheus_client::metrics::gauge::Gauge;
use prometheus_client::registry::Registry;
use serde_json::json;
use tokio::sync::watch;

use crate::content::{Engine, EngineParts};
use crate::crd::{ContentStatus, Ward, WardSignature, WardStatus, parse_duration};
use crate::feed::oci::{Reference, Registry as OciRegistry, credentials};
use crate::k8s::{AgentStatus, COMPONENT_AGENT, K8sError, selector, time};
use crate::observability::{Health, router, serve, stopped};
use crate::policy::{Policy, random_hex, revision_of};
use crate::rollout::{
    AgentReport, ContentRef, Decision, RING_CANARY, RING_LABEL, Rollout, choose_canary,
};
use crate::tls::{CA_FILE, CERT_FILE, Ca, KEY_FILE, needs_renewal};

use self::resources::{Defaults, Names};

pub const FINALIZER: &str = "security.ward.sh/cleanup";
pub const FIELD_MANAGER: &str = "ward-operator";
const REQUEUE: Duration = Duration::from_mins(1);
const CONFLICT_REQUEUE: Duration = Duration::from_mins(5);
const ANN_NOT_AFTER: &str = "ward.sh/not-after";
const ANN_CA: &str = "ward.sh/ca";
/// A `ConfigMap` holds at most 1 MiB. The policy leaves room for metadata.
const MAX_POLICY_BYTES: usize = 900 * 1024;

#[derive(Debug, thiserror::Error)]
pub enum OperatorError {
    #[error(transparent)]
    Kube(#[from] kube::Error),
    #[error(transparent)]
    K8s(#[from] K8sError),
    #[error("finalizer: {0}")]
    Finalizer(String),
    #[error("pki: {0}")]
    Pki(String),
}

#[derive(Clone, Debug, Hash, PartialEq, Eq, EncodeLabelSet)]
struct ResultLabels {
    result: String,
}

#[derive(Clone, Debug, Hash, PartialEq, Eq, EncodeLabelSet)]
struct PhaseLabels {
    namespace: String,
    ward: String,
    phase: String,
}

#[derive(Clone, Debug, Hash, PartialEq, Eq, EncodeLabelSet)]
struct SourceLabels {
    namespace: String,
    ward: String,
    source: String,
}

#[derive(Clone)]
struct OperatorMetrics {
    reconcile: Family<ResultLabels, Counter>,
    leader: Gauge,
    feed: Family<ResultLabels, Counter>,
    phase: Family<PhaseLabels, Gauge>,
    agents: Family<SourceLabels, Gauge>,
}

impl OperatorMetrics {
    fn register(reg: &mut Registry) -> Self {
        let m = Self {
            reconcile: Family::default(),
            leader: Gauge::default(),
            feed: Family::default(),
            phase: Family::default(),
            agents: Family::default(),
        };
        reg.register(
            "ward_operator_reconcile",
            "Reconciles by outcome",
            m.reconcile.clone(),
        );
        reg.register(
            "ward_operator_leader",
            "1 on the replica holding the leader Lease",
            m.leader.clone(),
        );
        reg.register("ward_feed_pull", "Feed checks by outcome", m.feed.clone());
        reg.register(
            "ward_rollout_phase",
            "1 for the current rollout phase",
            m.phase.clone(),
        );
        reg.register(
            "ward_agents",
            "Agents per event source mode",
            m.agents.clone(),
        );
        m
    }

    fn feed(&self, result: &str) {
        self.feed
            .get_or_create(&ResultLabels {
                result: result.into(),
            })
            .inc();
    }
}

#[derive(Clone, Debug)]
pub struct OperatorArgs {
    pub defaults: Defaults,
    pub leader_elect: bool,
    pub pod_name: String,
    pub pod_namespace: String,
    pub health_addr: SocketAddr,
}

struct Ctx {
    client: kube::Client,
    defaults: Defaults,
    metrics: OperatorMetrics,
    feed_checked: Mutex<HashMap<String, Instant>>,
}

async fn apply<K>(api: &Api<K>, obj: &K) -> Result<K, kube::Error>
where
    K: kube::Resource
        + Clone
        + serde::de::DeserializeOwned
        + serde::Serialize
        + std::fmt::Debug
        + Send
        + Sync,
{
    let name = obj.meta().name.clone().unwrap_or_default();
    api.patch(
        &name,
        &PatchParams::apply(FIELD_MANAGER).force(),
        &Patch::Apply(obj),
    )
    .await
}

async fn delete_if_present<K>(api: &Api<K>, name: &str) -> Result<(), kube::Error>
where
    K: kube::Resource + Clone + serde::de::DeserializeOwned + std::fmt::Debug,
{
    match api.delete(name, &DeleteParams::default()).await {
        Ok(_) => Ok(()),
        Err(kube::Error::Api(e)) if e.code == 404 => Ok(()),
        Err(e) => Err(e),
    }
}

/// The repository of an image reference, without tag or digest.
pub fn repository(image: &str) -> &str {
    let image = image.split('@').next().unwrap_or(image);
    match image.rfind(':') {
        Some(i) if !image[i..].contains('/') => &image[..i],
        _ => image,
    }
}

/// Whether a `Ward` may run `image` on every node.
pub fn image_allowed(image: &str, d: &Defaults) -> bool {
    if d.allowed_image_prefixes.is_empty() {
        let repo = repository(&d.image);
        image == d.image
            || image.starts_with(&format!("{repo}:"))
            || image.starts_with(&format!("{repo}@"))
    } else {
        d.allowed_image_prefixes
            .iter()
            .any(|p| !p.is_empty() && image.starts_with(p.as_str()))
    }
}

/// Why a `Ward` is refused before it deploys anything. A `Ward` runs a
/// privileged agent on every node, so it is held to the operator's
/// namespace, image allowlist, and privileged flag.
pub fn refusal(w: &Ward, d: &Defaults) -> Option<(&'static str, String)> {
    let ns = w.namespace().unwrap_or_default();
    if ns != d.ward_namespace {
        return Some((
            "NamespaceNotAllowed",
            format!(
                "a Ward deploys privileged node agents, so it is only accepted in namespace {}",
                d.ward_namespace
            ),
        ));
    }
    if let Some(image) = &w.spec.image
        && !image_allowed(image, d)
    {
        return Some((
            "ImageNotAllowed",
            format!("spec.image {image} is outside the operator's allowed image prefixes"),
        ));
    }
    if w.spec.agent.privileged && !d.allow_privileged_agent {
        return Some((
            "PrivilegedNotAllowed",
            "spec.agent.privileged needs the operator flag --allow-privileged-agent".into(),
        ));
    }
    None
}

/// The oldest `Ward` in the cluster is active, the rest report a conflict:
/// two agent `DaemonSets` on one node would fight over the same kernel hooks.
pub fn active_ward<'a>(wards: impl IntoIterator<Item = &'a Ward>) -> Option<(String, String)> {
    wards
        .into_iter()
        .filter(|w| w.metadata.deletion_timestamp.is_none())
        .min_by_key(|w| {
            (
                w.metadata
                    .creation_timestamp
                    .as_ref()
                    .map(|t| t.0.as_millisecond()),
                w.namespace().unwrap_or_default(),
                w.name_any(),
            )
        })
        .map(|w| (w.namespace().unwrap_or_default(), w.name_any()))
}

fn condition(
    type_: &str,
    ok: bool,
    reason: &str,
    message: &str,
    prev: &[Condition],
    generation: Option<i64>,
) -> Condition {
    let status = if ok { "True" } else { "False" };
    let since = prev
        .iter()
        .find(|c| c.type_ == type_ && c.status == status)
        .map_or_else(|| time(Utc::now()), |c| c.last_transition_time.clone());
    Condition {
        type_: type_.into(),
        status: status.into(),
        reason: reason.into(),
        message: message.chars().take(1024).collect(),
        last_transition_time: since,
        observed_generation: generation,
    }
}

/// The overlap of `allow` and `block` indicators. `allow` wins.
pub fn indicator_overlap(w: &Ward) -> Vec<String> {
    let allow: std::collections::HashSet<&str> = w
        .spec
        .indicators
        .allow
        .iter()
        .map(|i| i.sha256.as_str())
        .collect();
    w.spec
        .indicators
        .block
        .iter()
        .filter(|b| allow.contains(b.sha256.as_str()))
        .map(|b| b.sha256.clone())
        .collect()
}

async fn registry_for(client: &kube::Client, ns: &str, w: &Ward) -> Result<OciRegistry, String> {
    let feed = w.spec.feed.as_ref().ok_or("no feed")?;
    let creds = match &feed.pull_secret_ref {
        Some(r) => {
            let secret = Api::<Secret>::namespaced(client.clone(), ns)
                .get(&r.name)
                .await
                .map_err(|e| format!("pull secret {}: {e}", r.name))?;
            let cfg = secret
                .data
                .and_then(|d| d.get(".dockerconfigjson").map(|b| b.0.clone()))
                .unwrap_or_default();
            let reference = Reference::parse(&feed.image).map_err(|e| e.to_string())?;
            credentials(&cfg, &reference.registry)
        }
        None => None,
    };
    OciRegistry::new(creds).map_err(|e| e.to_string())
}

/// Checks the feed, and verifies and compiles a new revision before any
/// agent sees it.
async fn check_feed(
    ctx: &Ctx,
    ns: &str,
    w: &Ward,
    rollout: &Rollout,
) -> Result<Option<ContentRef>, String> {
    let feed = w.spec.feed.as_ref().ok_or("no feed")?;
    let registry = registry_for(&ctx.client, ns, w).await?;
    let reference = Reference::parse(&feed.image).map_err(|e| e.to_string())?;
    let resolved = registry
        .resolve(&reference)
        .await
        .map_err(|e| e.to_string())?;
    if rollout.knows(&resolved.db.digest) {
        ctx.metrics.feed("unchanged");
        return Ok(None);
    }
    let pulled = match crate::feed::pull(&registry, feed).await {
        Ok(p) => p,
        Err(e) => {
            ctx.metrics.feed("verify_failed");
            return Err(e.to_string());
        }
    };
    let db = pulled.db;
    tokio::task::spawn_blocking(move || {
        let engine = Engine::build(&EngineParts {
            revision: db.manifest.revision.clone(),
            feed: Some(&db),
            yara: true,
            ..EngineParts::default()
        })?;
        engine.self_test()
    })
    .await
    .map_err(|e| e.to_string())?
    .map_err(|e| {
        ctx.metrics.feed("verify_failed");
        e.to_string()
    })?;
    ctx.metrics.feed("updated");
    Ok(Some(pulled.content))
}

async fn ensure_pki(api: &Api<Secret>, w: &Ward, n: &Names) -> Result<(), OperatorError> {
    let now = Utc::now();
    let pki = |e: crate::tls::TlsError| OperatorError::Pki(e.to_string());
    let read = |s: &Secret, k: &str| {
        s.data
            .as_ref()
            .and_then(|d| d.get(k))
            .map(|b| String::from_utf8_lossy(&b.0).into_owned())
            .unwrap_or_default()
    };
    let ca = match api.get_opt(&n.ca()).await? {
        Some(s) if !read(&s, CERT_FILE).is_empty() => Ca {
            cert_pem: read(&s, CERT_FILE),
            key_pem: read(&s, KEY_FILE),
        },
        _ => {
            let ca = Ca::generate(now).map_err(pki)?;
            let data = BTreeMap::from([
                (CERT_FILE.to_string(), ca.cert_pem.clone()),
                (KEY_FILE.to_string(), ca.key_pem.clone()),
            ]);
            apply(api, &resources::secret(w, &n.ca(), &data, &BTreeMap::new())).await?;
            ca
        }
    };
    let ca_hash = revision_of(&[ca.cert_pem.as_bytes()]);
    for (name, cn, server) in [
        (n.manager_tls(), "ward-manager", true),
        (n.agent_tls(), "ward-agent", false),
    ] {
        let existing = api.get_opt(&name).await?;
        let ann = existing
            .as_ref()
            .and_then(|s| s.metadata.annotations.clone())
            .unwrap_or_default();
        let not_after = ann
            .get(ANN_NOT_AFTER)
            .and_then(|t| chrono::DateTime::parse_from_rfc3339(t).ok())
            .map(|t| t.with_timezone(&Utc));
        if !needs_renewal(not_after, now) && ann.get(ANN_CA) == Some(&ca_hash) {
            continue;
        }
        let leaf = ca.issue(cn, &n.manager_dns(), server, now).map_err(pki)?;
        let data = BTreeMap::from([
            (CERT_FILE.to_string(), leaf.cert_pem),
            (KEY_FILE.to_string(), leaf.key_pem),
            (CA_FILE.to_string(), ca.cert_pem.clone()),
        ]);
        let annotations = BTreeMap::from([
            (ANN_NOT_AFTER.to_string(), leaf.not_after.to_rfc3339()),
            (ANN_CA.to_string(), ca_hash.clone()),
        ]);
        apply(api, &resources::secret(w, &name, &data, &annotations)).await?;
    }
    if api.get_opt(&n.api_token()).await?.is_none() {
        let data = BTreeMap::from([("token".to_string(), random_hex(32))]);
        apply(
            api,
            &resources::secret(w, &n.api_token(), &data, &BTreeMap::new()),
        )
        .await?;
    }
    Ok(())
}

#[allow(clippy::too_many_lines)]
async fn reconcile_ward(w: Arc<Ward>, ctx: Arc<Ctx>) -> Result<Action, OperatorError> {
    let n = Names::of(&w);
    let ns = n.namespace.clone();
    let client = ctx.client.clone();
    let wards: Api<Ward> = Api::namespaced(client.clone(), &ns);
    let generation = w.metadata.generation;
    let prev = w.status.clone().unwrap_or_default();
    let mut conditions = Vec::new();

    let all: Vec<Ward> = Api::<Ward>::namespaced(client.clone(), &ctx.defaults.ward_namespace)
        .list(&ListParams::default())
        .await?
        .items;
    let refused = refusal(&w, &ctx.defaults).or_else(|| {
        (active_ward(&all) != Some((ns.clone(), n.name.clone()))).then(|| {
            (
                "Conflict",
                "another Ward in the cluster is active, only one Ward deploys agents".to_string(),
            )
        })
    });
    if let Some((reason, message)) = refused {
        let status = WardStatus {
            observed_generation: generation,
            phase: Some("Degraded".into()),
            conditions: vec![condition(
                "Ready",
                false,
                reason,
                &message,
                &prev.conditions,
                generation,
            )],
            ..WardStatus::default()
        };
        wards
            .patch_status(
                &n.name,
                &PatchParams::default(),
                &Patch::Merge(json!({"status": status})),
            )
            .await?;
        return Ok(Action::requeue(CONFLICT_REQUEUE));
    }

    let mut policy = Policy::from_spec(&w.spec);
    let sigs = signatures::collect(&client, &ns).await?;
    policy.custom_signatures = sigs.signatures;
    policy.custom_yara = sigs.yara;
    let mut policy_json = policy.to_json();
    if policy_json.len() > MAX_POLICY_BYTES {
        conditions.push(condition(
            "PolicyFits",
            false,
            "TooLarge",
            "WardSignatures exceed the policy ConfigMap size, they are left out",
            &prev.conditions,
            generation,
        ));
        policy.custom_signatures.clear();
        policy.custom_yara.clear();
        policy_json = policy.to_json();
    }
    let overlap = indicator_overlap(&w);
    if !overlap.is_empty() {
        conditions.push(condition(
            "IndicatorsConsistent",
            false,
            "AllowAndBlock",
            &format!(
                "{} hashes are in both allow and block, allow wins",
                overlap.len()
            ),
            &prev.conditions,
            generation,
        ));
    }
    let maps: Api<ConfigMap> = Api::namespaced(client.clone(), &ns);
    apply(&maps, &resources::policy_map(&w, &policy_json)).await?;

    let leases: Api<Lease> = Api::namespaced(client.clone(), &ns);
    let now = Utc::now();
    let agents: Vec<AgentStatus> = leases
        .list(&ListParams::default().labels(&selector(&n.name, COMPONENT_AGENT)))
        .await?
        .items
        .iter()
        .filter_map(|l| AgentStatus::from_lease(l, now))
        .collect();
    let reports: Vec<AgentReport> = agents.iter().map(AgentStatus::report).collect();

    let mut rollout = maps
        .get_opt(&n.rollout())
        .await?
        .and_then(|c| {
            c.data
                .and_then(|d| d.get(crate::rollout::ROLLOUT_FILE).cloned())
        })
        .map(|s| Rollout::parse(&s))
        .unwrap_or_default();
    if let Some(feed) = &w.spec.feed {
        let key = format!("{ns}/{}", n.name);
        let interval = parse_duration(&feed.interval).unwrap_or(Duration::from_hours(6));
        let due = ctx.feed_checked.lock().map_or(true, |m| {
            m.get(&key).is_none_or(|t| t.elapsed() >= interval)
        });
        if due {
            match check_feed(&ctx, &ns, &w, &rollout).await {
                Ok(Some(candidate)) => {
                    let labeled: HashMap<String, bool> = Api::<Node>::all(client.clone())
                        .list(&ListParams::default())
                        .await?
                        .items
                        .iter()
                        .map(|node| {
                            let canary = node
                                .labels()
                                .get(RING_LABEL)
                                .is_some_and(|v| v == RING_CANARY);
                            (node.name_any(), canary)
                        })
                        .collect();
                    let canary = choose_canary(
                        agents.iter().map(|a| {
                            (
                                a.node.as_str(),
                                labeled.get(&a.node).copied().unwrap_or(false),
                            )
                        }),
                        feed.rollout.canary_percent,
                    );
                    if let Decision::Promoted(r) = rollout.begin(candidate, canary, &reports, now) {
                        tracing::info!(ward = %key, revision = %r, "signature database promoted");
                    }
                    conditions.push(condition(
                        "FeedReady",
                        true,
                        "Verified",
                        "latest revision verified",
                        &prev.conditions,
                        generation,
                    ));
                }
                Ok(None) => conditions.push(condition(
                    "FeedReady",
                    true,
                    "UpToDate",
                    "no new revision",
                    &prev.conditions,
                    generation,
                )),
                Err(e) => {
                    ctx.metrics.feed("error");
                    conditions.push(condition(
                        "FeedReady",
                        false,
                        "PullFailed",
                        &e,
                        &prev.conditions,
                        generation,
                    ));
                }
            }
            if let Ok(mut m) = ctx.feed_checked.lock() {
                m.insert(key.clone(), Instant::now());
            }
        } else if let Some(c) = prev.conditions.iter().find(|c| c.type_ == "FeedReady") {
            conditions.push(c.clone());
        }
        match rollout.evaluate(&reports, &feed.rollout, now) {
            Decision::Promoted(r) => {
                tracing::info!(ward = %key, revision = %r, "signature database promoted");
            }
            Decision::Halted(m) => {
                tracing::warn!(ward = %key, reason = %m, "signature database rollout halted");
            }
            Decision::None => {}
        }
    } else {
        rollout = Rollout::default();
    }
    apply(&maps, &resources::rollout_map(&w, &rollout.to_json())).await?;

    apply(
        &Api::<ServiceAccount>::namespaced(client.clone(), &ns),
        &resources::service_account(&w),
    )
    .await?;
    apply(
        &Api::<ClusterRoleBinding>::all(client.clone()),
        &resources::cluster_role_binding(&w, &ctx.defaults),
    )
    .await?;
    apply(
        &Api::<RoleBinding>::namespaced(client.clone(), &ns),
        &resources::lease_role_binding(&w, &ctx.defaults),
    )
    .await?;

    let secrets: Api<Secret> = Api::namespaced(client.clone(), &ns);
    let services: Api<Service> = Api::namespaced(client.clone(), &ns);
    let sets: Api<StatefulSet> = Api::namespaced(client.clone(), &ns);
    let manager_ready = if w.spec.manager.enabled {
        ensure_pki(&secrets, &w, &n).await?;
        apply(&services, &resources::manager_service(&w)).await?;
        let sts = apply(&sets, &resources::manager_statefulset(&w, &ctx.defaults)).await?;
        sts.status.and_then(|s| s.ready_replicas).unwrap_or(0) >= 1
    } else {
        delete_if_present(&sets, &n.manager()).await?;
        delete_if_present(&services, &n.manager()).await?;
        for s in [n.ca(), n.manager_tls(), n.agent_tls(), n.api_token()] {
            delete_if_present(&secrets, &s).await?;
        }
        false
    };
    let ds = apply(
        &Api::<DaemonSet>::namespaced(client.clone(), &ns),
        &resources::agent_daemonset(&w, &ctx.defaults),
    )
    .await?;
    let (desired, ready) = ds
        .status
        .map_or((0, 0), |s| (s.desired_number_scheduled, s.number_ready));

    let mut sources: BTreeMap<String, i32> = BTreeMap::new();
    let mut revisions: BTreeMap<String, i32> = BTreeMap::new();
    let names: HashMap<String, String> = [rollout.stable.as_ref(), rollout.target.as_ref()]
        .into_iter()
        .flatten()
        .map(|c| (c.digest.clone(), c.revision.clone()))
        .collect();
    for a in &agents {
        *sources.entry(a.event_source.clone()).or_default() += 1;
        let r = a.content_digest.as_ref().map_or_else(
            || "builtin".to_string(),
            |d| names.get(d).cloned().unwrap_or_else(|| d.clone()),
        );
        *revisions.entry(r).or_default() += 1;
    }
    ctx.metrics.agents.clear();
    for (source, count) in &sources {
        ctx.metrics
            .agents
            .get_or_create(&SourceLabels {
                namespace: ns.clone(),
                ward: n.name.clone(),
                source: source.clone(),
            })
            .set(i64::from(*count));
    }
    for phase in ["Idle", "Canary", "Halted"] {
        ctx.metrics
            .phase
            .get_or_create(&PhaseLabels {
                namespace: ns.clone(),
                ward: n.name.clone(),
                phase: phase.into(),
            })
            .set(i64::from(rollout.phase.as_str() == phase));
    }

    let all_ready = desired > 0 && ready == desired && (!w.spec.manager.enabled || manager_ready);
    let degraded = conditions.iter().any(|c| c.status == "False");
    conditions.insert(
        0,
        condition(
            "Ready",
            all_ready,
            if all_ready {
                "AgentsReady"
            } else {
                "Progressing"
            },
            &format!(
                "{ready}/{desired} agents ready, manager {}",
                if !w.spec.manager.enabled {
                    "disabled"
                } else if manager_ready {
                    "ready"
                } else {
                    "not ready"
                }
            ),
            &prev.conditions,
            generation,
        ),
    );
    let status = WardStatus {
        observed_generation: generation,
        phase: Some(
            if degraded {
                "Degraded"
            } else if all_ready {
                "Ready"
            } else {
                "Progressing"
            }
            .into(),
        ),
        agents: Some(format!("{ready}/{desired}")),
        manager_ready,
        policy_revision: Some(policy.revision()),
        content: w.spec.feed.as_ref().map(|_| ContentStatus {
            stable: rollout.stable.as_ref().map(|c| c.revision.clone()),
            target: rollout.target.as_ref().map(|c| c.revision.clone()),
            phase: Some(rollout.phase.as_str().into()),
            message: rollout.message.clone(),
            revisions,
        }),
        event_sources: sources,
        conditions,
    };
    if w.status.as_ref() != Some(&status) {
        wards
            .patch_status(
                &n.name,
                &PatchParams::default(),
                &Patch::Merge(json!({"status": status})),
            )
            .await?;
    }
    Ok(Action::requeue(REQUEUE))
}

async fn cleanup(w: Arc<Ward>, ctx: Arc<Ctx>) -> Result<Action, OperatorError> {
    let n = Names::of(&w);
    delete_if_present(
        &Api::<ClusterRoleBinding>::all(ctx.client.clone()),
        &n.cluster_role_binding(),
    )
    .await?;
    Ok(Action::await_change())
}

async fn reconcile(w: Arc<Ward>, ctx: Arc<Ctx>) -> Result<Action, OperatorError> {
    let ns = w.namespace().unwrap_or_default();
    let api: Api<Ward> = Api::namespaced(ctx.client.clone(), &ns);
    if ns != ctx.defaults.ward_namespace {
        // No finalizer outside the home namespace: the operator only reports
        // the refusal there, and a finalizer would outlive an uninstall.
        return reconcile_ward(w, ctx).await;
    }
    let result = finalizer(&api, FINALIZER, w, |event| async {
        match event {
            Finalizer::Apply(w) => reconcile_ward(w, ctx.clone()).await,
            Finalizer::Cleanup(w) => cleanup(w, ctx.clone()).await,
        }
    })
    .await
    .map_err(|e| OperatorError::Finalizer(e.to_string()));
    ctx.metrics
        .reconcile
        .get_or_create(&ResultLabels {
            result: if result.is_ok() { "ok" } else { "error" }.into(),
        })
        .inc();
    result
}

#[allow(clippy::needless_pass_by_value)]
fn error_policy(w: Arc<Ward>, e: &OperatorError, _ctx: Arc<Ctx>) -> Action {
    tracing::warn!(ward = %w.name_any(), error = %e, "reconcile failed");
    Action::requeue(Duration::from_secs(30))
}

pub async fn run(args: OperatorArgs, shutdown: watch::Receiver<bool>) -> anyhow::Result<()> {
    let mut reg = Registry::default();
    let metrics = OperatorMetrics::register(&mut reg);
    let health = Health::new(reg);
    tokio::spawn(serve(
        args.health_addr,
        router(health.clone()),
        shutdown.clone(),
    ));
    let client = crate::k8s::client().await?;

    if args.leader_elect {
        leader::acquire(
            &client,
            &args.pod_namespace,
            &args.pod_name,
            shutdown.clone(),
        )
        .await?;
    }
    metrics.leader.set(1);
    health.set_ready(true);

    let home = args.defaults.ward_namespace.clone();
    let wards = Api::<Ward>::all(client.clone());
    let sigs = Api::<WardSignature>::namespaced(client.clone(), &home);
    let sets = Api::<StatefulSet>::namespaced(client.clone(), &home);
    let daemons = Api::<DaemonSet>::namespaced(client.clone(), &home);
    let ctx = Arc::new(Ctx {
        client: client.clone(),
        defaults: args.defaults.clone(),
        metrics,
        feed_checked: Mutex::new(HashMap::new()),
    });
    let owned = watcher::Config::default().labels(&format!(
        "{}={}",
        crate::k8s::LABEL_NAME,
        crate::k8s::APP_NAME
    ));
    let controller = Controller::new(wards, watcher::Config::default());
    let store = controller.store();
    tracing::info!("operator running");
    controller
        .owns(daemons, owned.clone())
        .owns(sets, owned)
        .watches(sigs, watcher::Config::default(), move |s: WardSignature| {
            let ns = s.namespace();
            store
                .state()
                .iter()
                .filter(|w| w.namespace() == ns)
                .map(|w| ObjectRef::from_obj(w.as_ref()))
                .collect::<Vec<_>>()
        })
        .graceful_shutdown_on(stopped(shutdown))
        .run(reconcile, error_policy, ctx)
        .for_each(|_| futures::future::ready(()))
        .await;
    Ok(())
}

#[cfg(test)]
mod tests {
    use k8s_openapi::apimachinery::pkg::apis::meta::v1::{ObjectMeta, Time};

    use super::*;
    use crate::crd::{Indicator, WardSpec};

    fn ward(ns: &str, name: &str, ms: i64) -> Ward {
        Ward {
            metadata: ObjectMeta {
                name: Some(name.into()),
                namespace: Some(ns.into()),
                creation_timestamp: Some(Time(
                    k8s_openapi::jiff::Timestamp::from_millisecond(ms).unwrap(),
                )),
                ..ObjectMeta::default()
            },
            spec: WardSpec::default(),
            status: None,
        }
    }

    #[test]
    fn oldest_ward_wins() {
        let a = ward("b", "x", 2000);
        let b = ward("a", "y", 1000);
        let mut c = ward("a", "z", 500);
        c.metadata.deletion_timestamp = Some(time(Utc::now()));
        assert_eq!(active_ward([&a, &b, &c]), Some(("a".into(), "y".into())));
        assert_eq!(active_ward(std::iter::empty()), None);
    }

    #[test]
    fn overlap_and_conditions() {
        let mut w = ward("a", "x", 1);
        let h = "a".repeat(64);
        w.spec.indicators.allow = vec![Indicator {
            sha256: h.clone(),
            comment: String::new(),
        }];
        w.spec.indicators.block = vec![
            Indicator {
                sha256: h.clone(),
                comment: String::new(),
            },
            Indicator {
                sha256: "b".repeat(64),
                comment: String::new(),
            },
        ];
        assert_eq!(indicator_overlap(&w), vec![h]);
        let first = condition("Ready", true, "R", "m", &[], Some(1));
        let again = condition(
            "Ready",
            true,
            "R",
            "m2",
            std::slice::from_ref(&first),
            Some(2),
        );
        assert_eq!(again.last_transition_time, first.last_transition_time);
        assert_eq!(
            condition("Ready", false, "R", "m", &[first], None).status,
            "False"
        );
    }

    #[test]
    fn refuses_foreign_namespaces_images_and_privilege() {
        let d = Defaults {
            image: "ghcr.io/younsl/ward:0.1.0".into(),
            ward_namespace: "ward-system".into(),
            ..Defaults::default()
        };
        let ok = ward("ward-system", "w", 1);
        assert_eq!(refusal(&ok, &d), None);
        assert_eq!(
            refusal(&ward("tenant", "w", 1), &d).unwrap().0,
            "NamespaceNotAllowed"
        );
        let mut img = ok.clone();
        img.spec.image = Some("evil.example.com/ward:0.1.0".into());
        assert_eq!(refusal(&img, &d).unwrap().0, "ImageNotAllowed");
        img.spec.image = Some("ghcr.io/younsl/ward:0.2.0".into());
        assert_eq!(refusal(&img, &d), None);
        img.spec.image = Some("ghcr.io/younsl/ward-evil:1".into());
        assert_eq!(refusal(&img, &d).unwrap().0, "ImageNotAllowed");
        let mirror = Defaults {
            allowed_image_prefixes: vec!["harbor.example.com/mirror/ward".into()],
            ..d.clone()
        };
        img.spec.image = Some("harbor.example.com/mirror/ward@sha256:00".into());
        assert_eq!(refusal(&img, &mirror), None);
        let mut privileged = ok;
        privileged.spec.agent.privileged = true;
        assert_eq!(refusal(&privileged, &d).unwrap().0, "PrivilegedNotAllowed");
        let allow = Defaults {
            allow_privileged_agent: true,
            ..d
        };
        assert_eq!(refusal(&privileged, &allow), None);
        assert_eq!(
            repository("ghcr.io/younsl/ward:0.1.0"),
            "ghcr.io/younsl/ward"
        );
        assert_eq!(
            repository("localhost:5000/ward@sha256:ab"),
            "localhost:5000/ward"
        );
        assert_eq!(repository("localhost:5000/ward"), "localhost:5000/ward");
    }

    #[test]
    fn metrics_register() {
        let mut reg = Registry::default();
        let m = OperatorMetrics::register(&mut reg);
        m.feed("updated");
        let mut out = String::new();
        prometheus_client::encoding::text::encode(&mut out, &reg).unwrap();
        assert!(
            out.contains("ward_feed_pull_total{result=\"updated\"} 1"),
            "{out}"
        );
    }
}
