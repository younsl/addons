//! Watches `AgentAccessPolicy` objects in the namespace the gateway submits
//! to, serves them to the mention path from the reflector cache, and reports
//! on each through its status conditions.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use futures_util::StreamExt as _;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::{Condition, Time};
use k8s_openapi::jiff::Timestamp;
use kube::api::{Api, DynamicObject, Patch, PatchParams};
use kube::runtime::controller::{Action, Controller};
use kube::runtime::reflector::{ObjectRef, Store};
use kube::runtime::watcher;
use kube::{Client, ResourceExt as _};
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use super::Error;
use std::collections::BTreeMap;
use std::collections::hash_map::{Entry, HashMap};

use crate::access::{Backend, Lookup, Policy, PolicySource, Route, Rule, channel_key};
use crate::crd::{AGENT_GROUP, AGENT_KIND, AgentAccessPolicy, AgentAccessPolicyStatus};
use crate::observability::Metrics;

/// How often a policy is re-checked when nothing about it changed. It is
/// what flips `AgentResolved` once the Agent is created after its policy.
const RECHECK: Duration = Duration::from_mins(5);
const RETRY: Duration = Duration::from_secs(30);
const GAUGE_INTERVAL: Duration = Duration::from_secs(30);
const READY_POLL: Duration = Duration::from_secs(1);

pub const ACCEPTED: &str = "Accepted";
pub const READY: &str = "Ready";
pub const AGENT_RESOLVED: &str = "AgentResolved";

/// The reflector cache, read on every mention.
#[derive(Clone)]
pub struct PolicyStore {
    store: Store<AgentAccessPolicy>,
    namespace: String,
    synced: Arc<AtomicBool>,
}

impl PolicySource for PolicyStore {
    fn lookup(&self, agent: &str) -> Lookup {
        if !self.synced.load(Ordering::Acquire) {
            return Lookup::Unsynced;
        }
        self.store
            .get(&ObjectRef::new(agent).within(&self.namespace))
            .map_or(Lookup::Missing, |p| {
                Lookup::Found(policy_of(&p, &self.namespace))
            })
    }

    fn routes(&self) -> Option<Vec<Route>> {
        if !self.synced.load(Ordering::Acquire) {
            return None;
        }
        Some(claims(&self.store.state()).routes)
    }
}

/// The routing table and the matches each policy lost.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Claims {
    pub routes: Vec<Route>,
    pub lost: BTreeMap<String, Vec<String>>,
}

/// Settles which policy owns each channel and label value. Like Gateway API
/// route conflicts, the oldest policy wins, then the name in alphabetical
/// order, so the outcome never depends on the order the cache returns.
pub fn claims(policies: &[Arc<AgentAccessPolicy>]) -> Claims {
    let mut sorted: Vec<&AgentAccessPolicy> = policies.iter().map(AsRef::as_ref).collect();
    sorted.sort_by_key(|p| {
        (
            p.metadata.creation_timestamp.as_ref().map(|t| t.0),
            p.name_any(),
        )
    });
    let mut channels: HashMap<String, String> = HashMap::new();
    let mut labels: HashMap<(String, String), String> = HashMap::new();
    let mut out = Claims::default();
    for p in sorted {
        let m = &p.spec.route_match;
        let name = p.name_any();
        let mut route = Route {
            name: name.clone(),
            alert_channel: p.spec.alert_channel.clone(),
            ..Route::default()
        };
        let mut lost = Vec::new();
        for c in &m.slack_channels {
            match channels.entry(channel_key(c)) {
                Entry::Occupied(o) => lost.push(format!("slack channel {c} (held by {})", o.get())),
                Entry::Vacant(v) => {
                    v.insert(name.clone());
                    route.slack_channels.push(c.clone());
                }
            }
        }
        if let Some(l) = &m.alert_label {
            let mut won = Vec::new();
            for v in &l.values {
                let key = if v == crate::access::ANY_CHANNEL {
                    (crate::access::ANY_CHANNEL.to_string(), v.clone())
                } else {
                    (l.name.clone(), v.clone())
                };
                match labels.entry(key) {
                    Entry::Occupied(o) => {
                        lost.push(format!("alert label {}={v} (held by {})", l.name, o.get()));
                    }
                    Entry::Vacant(e) => {
                        e.insert(name.clone());
                        won.push(v.clone());
                    }
                }
            }
            if !won.is_empty() {
                route.alert_label = Some((l.name.clone(), won));
            }
        }
        if !lost.is_empty() {
            out.lost.insert(name, lost);
        }
        out.routes.push(route);
    }
    out
}

/// Why a policy cannot be enforced as written, as a condition reason and
/// message. An `agentRef` outside the gateway's namespace would send a turn
/// to an Agent the gateway cannot reach, and a policy with no positive weight
/// has nowhere to send one.
fn invalid(policy: &AgentAccessPolicy, namespace: &str) -> Option<(&'static str, String)> {
    let name = policy.name_any();
    if let Some(r) = policy
        .spec
        .agent_refs
        .iter()
        .find(|r| r.namespace != namespace)
    {
        return Some((
            "NamespaceMismatch",
            format!(
                "agentRefs entry {}/{} is outside {namespace}, the namespace the gateway submits to, so every mention to {name} is denied",
                r.namespace, r.name
            ),
        ));
    }
    if policy.spec.agent_refs.iter().all(|r| r.weight <= 0) {
        return Some((
            "NoWeight",
            format!(
                "no agentRefs entry has a weight above 0, so every mention to {name} is denied"
            ),
        ));
    }
    None
}

/// The rule and the backends a policy sets.
pub fn policy_of(policy: &AgentAccessPolicy, namespace: &str) -> Policy {
    let backends = policy
        .spec
        .agent_refs
        .iter()
        .map(|r| Backend {
            agent: r.name.clone(),
            weight: u32::try_from(r.weight).unwrap_or(0),
        })
        .collect();
    let rule = if invalid(policy, namespace).is_some() {
        Rule::Invalid
    } else if policy.spec.chat.allow_all {
        Rule::AllowAll
    } else {
        Rule::Users(policy.spec.chat.allowed_users.iter().cloned().collect())
    };
    Policy { rule, backends }
}

/// Whether every referenced Agent exists.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AgentPresence {
    Found,
    Missing(Vec<String>),
    Unknown(String),
}

/// The status a policy should carry. Conditions whose status holds keep
/// their `lastTransitionTime`, as the API conventions require.
pub fn desired_status(
    policy: &AgentAccessPolicy,
    namespace: &str,
    agents: &AgentPresence,
    lost: &[String],
    previous: &[Condition],
    now: &Time,
) -> AgentAccessPolicyStatus {
    let generation = policy.metadata.generation;
    let mut conditions = previous.to_vec();
    let refs = policy.spec.agent_refs.len();

    let (status, reason, message) = match invalid(policy, namespace) {
        Some((reason, message)) => ("False", reason, message),
        None if policy.spec.chat.allow_all => (
            "True",
            "Enforced",
            format!(
                "every member of the allowed channels may invoke it, split across {refs} Agents"
            ),
        ),
        None => (
            "True",
            "Enforced",
            format!(
                "{} Slack members may invoke it, split across {refs} Agents",
                policy.spec.chat.allowed_users.len()
            ),
        ),
    };
    set_condition(
        &mut conditions,
        READY,
        status,
        reason,
        message,
        generation,
        now,
    );

    let (status, reason, message) = if lost.is_empty() {
        (
            "True",
            "Accepted",
            "every match is held by this policy".to_string(),
        )
    } else {
        (
            "False",
            "Conflicted",
            format!(
                "older policies hold {}. The other matches still apply",
                lost.join(", ")
            ),
        )
    };
    set_condition(
        &mut conditions,
        ACCEPTED,
        status,
        reason,
        message,
        generation,
        now,
    );

    let (status, reason, message) = match agents {
        AgentPresence::Found => (
            "True",
            "Found",
            format!("all {refs} referenced Agents exist"),
        ),
        AgentPresence::Missing(names) => (
            "False",
            "NotFound",
            format!(
                "Agents {} do not exist, and a thread picked for one fails until it is created",
                names.join(", ")
            ),
        ),
        AgentPresence::Unknown(why) => ("Unknown", "LookupFailed", why.clone()),
    };
    set_condition(
        &mut conditions,
        AGENT_RESOLVED,
        status,
        reason,
        message,
        generation,
        now,
    );

    AgentAccessPolicyStatus {
        observed_generation: generation,
        conditions,
    }
}

fn set_condition(
    conditions: &mut Vec<Condition>,
    type_: &str,
    status: &str,
    reason: &str,
    message: String,
    generation: Option<i64>,
    now: &Time,
) {
    let kept = conditions
        .iter()
        .position(|c| c.type_ == type_)
        .map(|i| conditions.remove(i))
        .filter(|c| c.status == status);
    let last = kept.map_or_else(|| now.clone(), |c| c.last_transition_time);
    conditions.push(Condition {
        type_: type_.into(),
        status: status.into(),
        reason: reason.into(),
        message,
        observed_generation: generation,
        last_transition_time: last,
    });
    conditions.sort_by(|a, b| a.type_.cmp(&b.type_));
}

struct Ctx {
    api: Api<AgentAccessPolicy>,
    store: Store<AgentAccessPolicy>,
    agents: Result<Api<DynamicObject>, String>,
    namespace: String,
    metrics: Arc<Metrics>,
}

impl Ctx {
    async fn agent_presence(&self, policy: &AgentAccessPolicy) -> AgentPresence {
        if policy
            .spec
            .agent_refs
            .iter()
            .any(|r| r.namespace != self.namespace)
        {
            return AgentPresence::Unknown(format!(
                "not looked up: the gateway reads Agents in {} only",
                self.namespace
            ));
        }
        let api = match &self.agents {
            Err(why) => return AgentPresence::Unknown(why.clone()),
            Ok(api) => api,
        };
        let mut missing = Vec::new();
        for r in &policy.spec.agent_refs {
            match api.get_opt(&r.name).await {
                Ok(Some(_)) => {}
                Ok(None) => missing.push(format!("{}/{}", r.namespace, r.name)),
                Err(err) => {
                    return AgentPresence::Unknown(format!(
                        "Agent {} lookup failed: {err}",
                        r.name
                    ));
                }
            }
        }
        if missing.is_empty() {
            AgentPresence::Found
        } else {
            AgentPresence::Missing(missing)
        }
    }
}

async fn reconcile(policy: Arc<AgentAccessPolicy>, ctx: Arc<Ctx>) -> Result<Action, Error> {
    let name = policy.name_any();
    let agent = ctx.agent_presence(&policy).await;
    let current = policy.status.clone().unwrap_or_default();
    let lost = claims(&ctx.store.state())
        .lost
        .remove(&name)
        .unwrap_or_default();
    let desired = desired_status(
        &policy,
        &ctx.namespace,
        &agent,
        &lost,
        &current.conditions,
        &Time(Timestamp::now()),
    );
    if desired != current {
        let patch = serde_json::json!({ "status": desired });
        ctx.api
            .patch_status(&name, &PatchParams::default(), &Patch::Merge(&patch))
            .await?;
        debug!(policy = name, "updated access policy status");
    }
    ctx.metrics.access_policy_reconcile("ok");
    Ok(Action::requeue(RECHECK))
}

#[allow(clippy::needless_pass_by_value)] // signature fixed by Controller::run
fn error_policy(policy: Arc<AgentAccessPolicy>, err: &Error, ctx: Arc<Ctx>) -> Action {
    ctx.metrics.access_policy_reconcile("error");
    warn!(policy = policy.name_any(), error = %err, "access policy reconcile failed");
    Action::requeue(RETRY)
}

/// Finds the kagent `Agent` API. kagent has served it under more than one
/// version, so the version comes from discovery rather than a constant.
async fn agent_api(client: &Client, namespace: &str) -> Result<Api<DynamicObject>, String> {
    let group = kube::discovery::group(client, AGENT_GROUP)
        .await
        .map_err(|err| format!("{AGENT_GROUP} API discovery failed: {err}"))?;
    let (resource, _) = group
        .recommended_kind(AGENT_KIND)
        .ok_or_else(|| format!("{AGENT_GROUP} serves no {AGENT_KIND} kind"))?;
    Ok(Api::namespaced_with(client.clone(), namespace, &resource))
}

/// Starts the controller and returns the cache it fills. Until the initial
/// list completes, every lookup answers `Unsynced`, so the mention path fails
/// closed instead of treating a policy it has not seen yet as absent.
pub async fn start(
    client: Client,
    namespace: &str,
    metrics: Arc<Metrics>,
    shutdown: CancellationToken,
) -> PolicyStore {
    let started = Instant::now();
    let api: Api<AgentAccessPolicy> = Api::namespaced(client.clone(), namespace);
    let controller = Controller::new(api.clone(), watcher::Config::default())
        .graceful_shutdown_on(shutdown.clone().cancelled_owned());
    let store = controller.store();

    let agents = agent_api(&client, namespace).await;
    if let Err(why) = &agents {
        warn!(
            reason = why,
            "kagent Agent API unavailable; AgentResolved stays Unknown"
        );
    }
    let ctx = Arc::new(Ctx {
        api,
        store: store.clone(),
        agents,
        namespace: namespace.to_string(),
        metrics: metrics.clone(),
    });
    tokio::spawn(
        controller
            .run(reconcile, error_policy, ctx)
            .for_each(|result| async move {
                if let Err(err) = result {
                    debug!(error = %err, "access policy controller event");
                }
            }),
    );

    let synced = Arc::new(AtomicBool::new(false));
    tokio::spawn(track_sync(
        store.clone(),
        synced.clone(),
        started,
        metrics,
        namespace.to_string(),
        shutdown,
    ));

    PolicyStore {
        store,
        namespace: namespace.to_string(),
        synced,
    }
}

/// Waits for the initial list. kube-runtime's readiness signal wakes only the
/// last of several concurrent waiters, and the Controller waits on the same
/// store, so a single wait can sleep forever; polling again every
/// `READY_POLL` picks up a signal that already fired.
async fn wait_ready(store: &Store<AgentAccessPolicy>, shutdown: &CancellationToken) -> bool {
    loop {
        tokio::select! {
            ready = tokio::time::timeout(READY_POLL, store.wait_until_ready()) => match ready {
                Ok(Ok(())) => return true,
                Ok(Err(_)) => return false,
                Err(_) => {}
            },
            () = shutdown.cancelled() => return false,
        }
    }
}

/// Flips the cache to synced after the initial list, then keeps the policy
/// count gauge current. Deletions never reach the reconciler, so the count
/// is read from the cache rather than tallied there.
async fn track_sync(
    store: Store<AgentAccessPolicy>,
    synced: Arc<AtomicBool>,
    started: Instant,
    metrics: Arc<Metrics>,
    namespace: String,
    shutdown: CancellationToken,
) {
    if !wait_ready(&store, &shutdown).await {
        return;
    }
    synced.store(true, Ordering::Release);
    metrics.access_policies_synced.set(1);
    let mut names: Vec<String> = store.state().iter().map(|p| p.name_any()).collect();
    names.sort_unstable();
    info!(
        namespace,
        policy_count = names.len(),
        agents = names.join(","),
        elapsed_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
        "access policies synced; agents without a policy fall back to CHAT_ALLOWED_USERS"
    );
    let mut tick = tokio::time::interval(GAUGE_INTERVAL);
    loop {
        tokio::select! {
            _ = tick.tick() => {
                metrics
                    .access_policies
                    .set(i64::try_from(store.state().len()).unwrap_or(i64::MAX));
            }
            () = shutdown.cancelled() => return,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crd::{AgentAccessPolicySpec, AgentRef, ChatAccess, LabelMatch, RouteMatch};
    use kube::runtime::reflector::store::Writer;
    use kube::runtime::watcher::Event;

    #[tokio::test]
    async fn wait_ready_survives_a_second_waiter_stealing_the_wakeup() {
        use std::future::Future as _;
        use std::task::{Context, Waker};

        let mut writer: Writer<AgentAccessPolicy> = Writer::default();
        let store = writer.as_reader();
        let shutdown = CancellationToken::new();
        let ours = tokio::spawn({
            let store = store.clone();
            async move { wait_ready(&store, &shutdown).await }
        });
        tokio::time::sleep(Duration::from_millis(50)).await;
        let mut other = Box::pin(store.wait_until_ready());
        assert!(
            other
                .as_mut()
                .poll(&mut Context::from_waker(Waker::noop()))
                .is_pending()
        );

        writer.apply_watcher_event(&Event::InitDone);

        let ready = tokio::time::timeout(READY_POLL * 3, ours).await;
        assert!(matches!(ready, Ok(Ok(true))));
    }

    #[tokio::test]
    async fn start_syncs_the_cache_while_the_controller_waits_on_it_too() {
        use wiremock::matchers::{method, path, query_param, query_param_is_missing};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let base = "/apis/access.kagent-gateway.sh/v1alpha1/namespaces/kagent/agentaccesspolicies";
        let mut p = policy("ops-policy", "kagent", &["U1"], false);
        p.metadata.resource_version = Some("1".into());
        p.metadata.creation_timestamp = Some(time(1));
        p.spec.route_match.slack_channels = vec!["ops".into()];
        let obj = serde_json::to_value(&p).unwrap();
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path(base))
            .and(query_param_is_missing("watch"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "apiVersion": "access.kagent-gateway.sh/v1alpha1",
                "kind": "AgentAccessPolicyList",
                "metadata": {"resourceVersion": "1"},
                "items": [obj],
            })))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path(base))
            .and(query_param("watch", "true"))
            .respond_with(ResponseTemplate::new(200).set_delay(Duration::from_secs(60)))
            .mount(&server)
            .await;
        Mock::given(method("PATCH"))
            .and(path(format!("{base}/ops-policy/status")))
            .respond_with(ResponseTemplate::new(200).set_body_json(&obj))
            .mount(&server)
            .await;

        let client = Client::try_from(kube::Config::new(server.uri().parse().unwrap())).unwrap();
        let metrics = Arc::new(Metrics::new());
        let shutdown = CancellationToken::new();
        let store = start(client, "kagent", metrics.clone(), shutdown.clone()).await;

        let synced = tokio::time::timeout(READY_POLL * 5, async {
            while store.routes().is_none() {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await;
        assert!(synced.is_ok(), "policy cache never synced");
        assert_eq!(metrics.access_policies_synced.get(), 1);
        assert!(matches!(store.lookup("ops-policy"), Lookup::Found(_)));
        assert_eq!(store.routes().unwrap()[0].name, "ops-policy");

        let reconciled = tokio::time::timeout(READY_POLL * 5, async {
            loop {
                let patched = server
                    .received_requests()
                    .await
                    .unwrap_or_default()
                    .iter()
                    .any(|r| {
                        r.method == wiremock::http::Method::PATCH
                            && r.url.path().ends_with("/status")
                    });
                if patched {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await;
        assert!(reconciled.is_ok(), "controller never reconciled");
        shutdown.cancel();
    }

    #[tokio::test]
    async fn wait_ready_stops_on_shutdown() {
        let writer: Writer<AgentAccessPolicy> = Writer::default();
        let store = writer.as_reader();
        let shutdown = CancellationToken::new();
        shutdown.cancel();
        assert!(!wait_ready(&store, &shutdown).await);
    }

    fn agent_ref(name: &str, namespace: &str, weight: i32) -> AgentRef {
        AgentRef {
            group: AGENT_GROUP.into(),
            kind: AGENT_KIND.into(),
            name: name.into(),
            namespace: namespace.into(),
            weight,
        }
    }

    fn policy(name: &str, namespace: &str, users: &[&str], allow_all: bool) -> AgentAccessPolicy {
        let mut p = AgentAccessPolicy::new(
            name,
            AgentAccessPolicySpec {
                route_match: RouteMatch::default(),
                alert_channel: None,
                agent_refs: vec![agent_ref(name, namespace, 1)],
                chat: ChatAccess {
                    allowed_users: users.iter().map(|u| (*u).to_string()).collect(),
                    allow_all,
                },
            },
        );
        p.metadata.namespace = Some("kagent".into());
        p.metadata.generation = Some(3);
        p
    }

    fn routed(
        name: &str,
        created: i64,
        channels: &[&str],
        label: Option<(&str, &[&str])>,
    ) -> Arc<AgentAccessPolicy> {
        let mut p = policy(name, "kagent", &["U1"], false);
        p.metadata.creation_timestamp = Some(time(created));
        p.spec.route_match = RouteMatch {
            slack_channels: channels.iter().map(|c| (*c).to_string()).collect(),
            alert_label: label.map(|(n, vs)| LabelMatch {
                name: n.into(),
                values: vs.iter().map(|v| (*v).to_string()).collect(),
            }),
        };
        Arc::new(p)
    }

    fn time(secs: i64) -> Time {
        Time(Timestamp::from_second(secs).unwrap())
    }

    fn cond<'a>(s: &'a AgentAccessPolicyStatus, type_: &str) -> &'a Condition {
        s.conditions.iter().find(|c| c.type_ == type_).unwrap()
    }

    #[test]
    fn policy_follows_the_spec() {
        let p = policy_of(&policy("a", "kagent", &["U1"], false), "kagent");
        assert_eq!(p.rule, Rule::Users(["U1".to_string()].into()));
        assert_eq!(
            p.backends,
            vec![Backend {
                agent: "a".into(),
                weight: 1
            }]
        );
        assert_eq!(
            policy_of(&policy("a", "kagent", &[], true), "kagent").rule,
            Rule::AllowAll
        );
        assert_eq!(
            policy_of(&policy("a", "other", &["U1"], false), "kagent").rule,
            Rule::Invalid
        );

        let mut split = policy("a", "kagent", &["U1"], false);
        split.spec.agent_refs = vec![
            agent_ref("a-v1", "kagent", 90),
            agent_ref("a-v2", "kagent", 10),
        ];
        let p = policy_of(&split, "kagent");
        assert_eq!(p.backends[1].weight, 10);
        assert!(matches!(p.rule, Rule::Users(_)));

        split.spec.agent_refs = vec![agent_ref("a-v1", "kagent", 0)];
        assert_eq!(policy_of(&split, "kagent").rule, Rule::Invalid);
        let s = desired_status(&split, "kagent", &AgentPresence::Found, &[], &[], &time(1));
        assert_eq!(cond(&s, READY).reason, "NoWeight");
    }

    #[test]
    fn status_reports_enforcement_agents_and_acceptance() {
        let p = policy("a", "kagent", &["U1", "U2"], false);
        let s = desired_status(&p, "kagent", &AgentPresence::Found, &[], &[], &time(1));
        assert_eq!(s.observed_generation, Some(3));
        assert_eq!(s.conditions.len(), 3);
        assert_eq!(cond(&s, READY).status, "True");
        assert!(cond(&s, READY).message.starts_with("2 Slack members"));
        assert_eq!(cond(&s, AGENT_RESOLVED).status, "True");
        assert_eq!(cond(&s, ACCEPTED).status, "True");

        let s = desired_status(
            &p,
            "kagent",
            &AgentPresence::Missing(vec!["kagent/a".into()]),
            &[],
            &[],
            &time(1),
        );
        assert_eq!(cond(&s, AGENT_RESOLVED).reason, "NotFound");
        assert!(cond(&s, AGENT_RESOLVED).message.contains("kagent/a"));
        let s = desired_status(
            &p,
            "kagent",
            &AgentPresence::Unknown("boom".into()),
            &[],
            &[],
            &time(1),
        );
        assert_eq!(
            (
                cond(&s, AGENT_RESOLVED).status.as_str(),
                cond(&s, AGENT_RESOLVED).message.as_str()
            ),
            ("Unknown", "boom")
        );

        let open = policy("a", "kagent", &[], true);
        let s = desired_status(&open, "kagent", &AgentPresence::Found, &[], &[], &time(1));
        assert!(cond(&s, READY).message.starts_with("every member"));

        let wrong = policy("a", "other", &["U1"], false);
        let s = desired_status(&wrong, "kagent", &AgentPresence::Found, &[], &[], &time(1));
        assert_eq!(cond(&s, READY).status, "False");
        assert_eq!(cond(&s, READY).reason, "NamespaceMismatch");

        let matched = routed("a", 1, &["ops"], None);
        let s = desired_status(
            &matched,
            "kagent",
            &AgentPresence::Found,
            &[],
            &[],
            &time(1),
        );
        assert_eq!(cond(&s, ACCEPTED).status, "True");
        let lost = vec!["slack channel ops (held by b)".to_string()];
        let s = desired_status(
            &matched,
            "kagent",
            &AgentPresence::Found,
            &lost,
            &[],
            &time(1),
        );
        assert_eq!(cond(&s, ACCEPTED).reason, "Conflicted");
        assert!(cond(&s, ACCEPTED).message.contains("held by b"));
    }

    #[test]
    fn transition_time_moves_only_when_status_changes() {
        let p = policy("a", "kagent", &["U1"], false);
        let missing = AgentPresence::Missing(vec!["kagent/a".into()]);
        let first = desired_status(&p, "kagent", &missing, &[], &[], &time(1));
        let same = desired_status(&p, "kagent", &missing, &[], &first.conditions, &time(2));
        assert_eq!(first, same);
        let flipped = desired_status(
            &p,
            "kagent",
            &AgentPresence::Found,
            &[],
            &first.conditions,
            &time(3),
        );
        assert_eq!(cond(&flipped, AGENT_RESOLVED).last_transition_time, time(3));
        assert_eq!(cond(&flipped, READY).last_transition_time, time(1));
    }

    #[test]
    fn claims_give_each_match_to_the_oldest_policy() {
        let newer = routed(
            "newer",
            20,
            &["#OPS", "dba", "*"],
            Some(("slack_channel", &["dba", "infra"])),
        );
        let older = routed(
            "older",
            10,
            &["ops", "*"],
            Some(("slack_channel", &["dba"])),
        );
        let c = claims(&[newer, older]);
        assert_eq!(c.routes.len(), 2);
        assert_eq!(c.routes[0].name, "older");
        assert_eq!(c.routes[0].slack_channels, vec!["ops", "*"]);
        assert_eq!(c.routes[1].name, "newer");
        assert_eq!(c.routes[1].slack_channels, vec!["dba"]);
        assert_eq!(
            c.routes[1].alert_label,
            Some(("slack_channel".to_string(), vec!["infra".to_string()]))
        );
        let lost = &c.lost["newer"];
        assert_eq!(lost.len(), 3);
        assert!(lost[0].contains("#OPS") && lost[0].contains("older"));
        assert!(lost.iter().any(|l| l.contains("slack_channel=dba")));
        assert!(!c.lost.contains_key("older"));
    }

    #[test]
    fn claims_break_age_ties_by_name() {
        let b = routed("b", 5, &["ops"], None);
        let a = routed("a", 5, &["ops"], None);
        let c = claims(&[b, a]);
        assert_eq!(c.routes[0].name, "a");
        assert!(c.lost["b"][0].contains("held by a"));
    }

    #[test]
    fn store_lookup_is_unsynced_until_ready_then_reads_the_cache() {
        let mut writer: Writer<AgentAccessPolicy> = Writer::default();
        let ps = PolicyStore {
            store: writer.as_reader(),
            namespace: "kagent".into(),
            synced: Arc::new(AtomicBool::new(false)),
        };
        assert_eq!(ps.lookup("a"), Lookup::Unsynced);
        assert_eq!(ps.routes(), None);
        writer.apply_watcher_event(&Event::Apply(policy("a", "kagent", &["U1"], false)));
        writer.apply_watcher_event(&Event::Apply((*routed("b", 1, &["ops"], None)).clone()));
        ps.synced.store(true, Ordering::Release);
        assert_eq!(
            ps.lookup("a"),
            Lookup::Found(Policy {
                rule: Rule::Users(["U1".to_string()].into()),
                backends: vec![Backend {
                    agent: "a".into(),
                    weight: 1
                }],
            })
        );
        assert_eq!(ps.lookup("z"), Lookup::Missing);
        let names: Vec<String> = ps.routes().unwrap().into_iter().map(|r| r.name).collect();
        assert_eq!(names, vec!["a", "b"]);
    }
}
