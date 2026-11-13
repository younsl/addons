//! The agent `DaemonSet`: one for the whole cluster, created from the template
//! the chart ships, scheduled on exactly the nodes the `NodeConfiguration`
//! resources target, and removed when none is left. A `PodMonitor` for the
//! agent pods lives and dies with it when the chart ships a template for one.

use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::Duration;

use futures::StreamExt as _;
use k8s_openapi::api::apps::v1::DaemonSet;
use k8s_openapi::api::core::v1::{
    Affinity, ConfigMap, NodeAffinity, NodeSelector, NodeSelectorRequirement, NodeSelectorTerm,
};
use k8s_openapi::apimachinery::pkg::apis::meta::v1::OwnerReference;
use kube::api::{Api, ApiResource, DeleteParams, DynamicObject, Patch, PatchParams, TypeMeta};
use kube::runtime::{WatchStreamExt as _, reflector, watcher};
use kube::{Client, ResourceExt as _};
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use super::{KARPENTER_LABEL, NODE_GROUP_LABEL};
use node_configuration_common::crd::{NodeConfiguration, Target};

pub const TEMPLATE_KEY: &str = "daemonset.yaml";
/// The chart adds this key when `agent.podMonitor.enabled` is set.
pub const MONITOR_KEY: &str = "podmonitor.yaml";
const MONITOR_API_VERSION: &str = "monitoring.coreos.com/v1";
const MONITOR_KIND: &str = "PodMonitor";
const FIELD_MANAGER: &str = "node-configuration-operator";
const RESYNC: Duration = Duration::from_secs(60);

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("Kubernetes API request failed, {0}")]
    Kube(#[from] kube::Error),
    #[error("ConfigMap {0} has no {TEMPLATE_KEY} key")]
    MissingTemplate(String),
    #[error("agent template is not a DaemonSet, {0}")]
    Parse(#[from] serde_yaml::Error),
    #[error("agent template has no {0}")]
    Incomplete(&'static str),
    #[error("ConfigMap key {MONITOR_KEY} is not a PodMonitor, {0}")]
    Monitor(serde_yaml::Error),
    #[error("the PodMonitor API of the Prometheus Operator is not installed")]
    MonitorApiMissing,
}

/// Where the chart keeps the agent template.
#[derive(Clone, Debug)]
pub struct Source {
    pub namespace: String,
    pub config_map: String,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Outcome {
    Applied,
    Removed,
}

fn requirement(key: &str, operator: &str, values: Option<Vec<String>>) -> NodeSelectorRequirement {
    NodeSelectorRequirement {
        key: key.to_string(),
        operator: operator.to_string(),
        values,
    }
}

/// The node affinity term that selects the same nodes as
/// [`super::node_selector`] does for one target.
fn term(target: &Target) -> NodeSelectorTerm {
    let mut expressions = vec![requirement(KARPENTER_LABEL, "DoesNotExist", None)];
    if target.node_groups.is_empty() {
        expressions.push(requirement(NODE_GROUP_LABEL, "Exists", None));
    } else {
        let groups: BTreeSet<_> = target.node_groups.iter().cloned().collect();
        expressions.push(requirement(
            NODE_GROUP_LABEL,
            "In",
            Some(groups.into_iter().collect()),
        ));
    }
    expressions.extend(
        target
            .node_selector
            .iter()
            .map(|(key, value)| requirement(key, "In", Some(vec![value.clone()]))),
    );
    NodeSelectorTerm {
        match_expressions: Some(expressions),
        match_fields: None,
    }
}

/// One term per distinct target, in a stable order so an unchanged set of
/// targets never changes the pod template.
pub fn terms<'a>(targets: impl IntoIterator<Item = &'a Target>) -> Vec<NodeSelectorTerm> {
    let mut unique: Vec<(String, NodeSelectorTerm)> = targets
        .into_iter()
        .map(term)
        .map(|term| (serde_json::to_string(&term).unwrap_or_default(), term))
        .collect();
    unique.sort_by(|a, b| a.0.cmp(&b.0));
    unique.dedup_by(|a, b| a.0 == b.0);
    unique.into_iter().map(|(_, term)| term).collect()
}

// Removing the chart removes the template, and what it owns goes with it.
fn owner(source: &Source, config_map: &ConfigMap) -> Option<Vec<OwnerReference>> {
    config_map.metadata.uid.clone().map(|uid| {
        vec![OwnerReference {
            api_version: "v1".to_string(),
            kind: "ConfigMap".to_string(),
            name: source.config_map.clone(),
            uid,
            controller: None,
            block_owner_deletion: None,
        }]
    })
}

/// The `PodMonitor` of the template under the name of the `DaemonSet`, or
/// nothing when the chart ships none.
fn monitor(
    source: &Source,
    config_map: &ConfigMap,
    name: &str,
) -> Result<Option<DynamicObject>, Error> {
    let Some(yaml) = config_map
        .data
        .as_ref()
        .and_then(|data| data.get(MONITOR_KEY))
    else {
        return Ok(None);
    };
    let mut monitor: DynamicObject = serde_yaml::from_str(yaml).map_err(Error::Monitor)?;
    monitor.types = Some(TypeMeta {
        api_version: MONITOR_API_VERSION.to_string(),
        kind: MONITOR_KIND.to_string(),
    });
    monitor.metadata.name = Some(name.to_string());
    monitor.metadata.namespace = Some(source.namespace.clone());
    monitor.metadata.owner_references = owner(source, config_map);
    Ok(Some(monitor))
}

/// Applies the `PodMonitor` while the agents run and the chart ships one, and
/// removes it otherwise.
async fn sync_monitor(
    client: &Client,
    source: &Source,
    config_map: &ConfigMap,
    name: &str,
    agents: &Outcome,
) -> Result<Outcome, Error> {
    let resource = ApiResource {
        group: "monitoring.coreos.com".to_string(),
        version: "v1".to_string(),
        api_version: MONITOR_API_VERSION.to_string(),
        kind: MONITOR_KIND.to_string(),
        plural: "podmonitors".to_string(),
    };
    let api = Api::<DynamicObject>::namespaced_with(client.clone(), &source.namespace, &resource);
    let wanted = monitor(source, config_map, name)?.filter(|_| *agents == Outcome::Applied);
    let Some(monitor) = wanted else {
        return match api.delete(name, &DeleteParams::default()).await {
            // Without the API or the permission there is nothing of ours to remove.
            Ok(_) => Ok(Outcome::Removed),
            Err(kube::Error::Api(status)) if matches!(status.code, 403 | 404) => {
                Ok(Outcome::Removed)
            }
            Err(e) => Err(e.into()),
        };
    };
    match api
        .patch(
            name,
            &PatchParams::apply(FIELD_MANAGER).force(),
            &Patch::Apply(&monitor),
        )
        .await
    {
        Ok(_) => Ok(Outcome::Applied),
        Err(kube::Error::Api(status)) if status.code == 404 => Err(Error::MonitorApiMissing),
        Err(e) => Err(e.into()),
    }
}

fn template(source: &Source, config_map: &ConfigMap) -> Result<DaemonSet, Error> {
    let yaml = config_map
        .data
        .as_ref()
        .and_then(|data| data.get(TEMPLATE_KEY))
        .ok_or_else(|| Error::MissingTemplate(source.config_map.clone()))?;
    let mut daemon_set: DaemonSet = serde_yaml::from_str(yaml)?;
    if daemon_set.metadata.name.is_none() {
        return Err(Error::Incomplete("metadata.name"));
    }
    daemon_set.metadata.namespace = Some(source.namespace.clone());
    daemon_set.metadata.owner_references = owner(source, config_map);
    Ok(daemon_set)
}

/// The template restricted to the nodes of `terms`. Any other affinity the
/// template carries is kept.
fn render(mut daemon_set: DaemonSet, terms: Vec<NodeSelectorTerm>) -> Result<DaemonSet, Error> {
    let pod = daemon_set
        .spec
        .as_mut()
        .and_then(|spec| spec.template.spec.as_mut())
        .ok_or(Error::Incomplete("spec.template.spec"))?;
    pod.affinity
        .get_or_insert_with(Affinity::default)
        .node_affinity
        .get_or_insert_with(NodeAffinity::default)
        .required_during_scheduling_ignored_during_execution = Some(NodeSelector {
        node_selector_terms: terms,
    });
    Ok(daemon_set)
}

/// Applies the `DaemonSet` for `terms`, or removes it when no target is left,
/// and keeps the `PodMonitor` of the agents in step. A `PodMonitor` that
/// cannot be synced is logged and never holds the agents back.
pub async fn sync(
    client: &Client,
    source: &Source,
    terms: Vec<NodeSelectorTerm>,
) -> Result<Outcome, Error> {
    let config_map = Api::<ConfigMap>::namespaced(client.clone(), &source.namespace)
        .get(&source.config_map)
        .await?;
    let daemon_set = template(source, &config_map)?;
    let name = daemon_set.name_any();
    let agents = sync_daemon_set(client, source, daemon_set, terms).await?;
    match sync_monitor(client, source, &config_map, &name, &agents).await {
        Ok(outcome) => debug!(?outcome, "Agent PodMonitor synced"),
        Err(e) => warn!("Agent PodMonitor sync failed, {e}"),
    }
    Ok(agents)
}

async fn sync_daemon_set(
    client: &Client,
    source: &Source,
    daemon_set: DaemonSet,
    terms: Vec<NodeSelectorTerm>,
) -> Result<Outcome, Error> {
    let name = daemon_set.name_any();
    let api = Api::<DaemonSet>::namespaced(client.clone(), &source.namespace);

    if terms.is_empty() {
        return match api.delete(&name, &DeleteParams::default()).await {
            Ok(_) => Ok(Outcome::Removed),
            Err(kube::Error::Api(status)) if status.code == 404 => Ok(Outcome::Removed),
            Err(e) => Err(e.into()),
        };
    }
    let daemon_set = render(daemon_set, terms)?;
    api.patch(
        &name,
        &PatchParams::apply(FIELD_MANAGER).force(),
        &Patch::Apply(&daemon_set),
    )
    .await?;
    Ok(Outcome::Applied)
}

fn targets(configurations: &[Arc<NodeConfiguration>]) -> Vec<NodeSelectorTerm> {
    terms(configurations.iter().map(|c| &c.spec.target))
}

/// Keeps the agent `DaemonSet` in line with the targets until `shutdown`. A
/// change of the targets is applied at once, and every [`RESYNC`] picks up a
/// new template and repairs a manual edit.
pub async fn run(client: Client, source: Source, shutdown: CancellationToken) {
    let (reader, writer) = reflector::store::<NodeConfiguration>();
    let api = Api::<NodeConfiguration>::all(client.clone());
    let mut events = reflector(writer, watcher(api, watcher::Config::default()))
        .default_backoff()
        .boxed();
    let mut resync = tokio::time::interval(RESYNC);
    let mut listed = false;
    let mut applied: Option<Vec<NodeSelectorTerm>> = None;

    loop {
        let force = tokio::select! {
            () = shutdown.cancelled() => break,
            _ = resync.tick() => true,
            event = events.next() => match event {
                Some(Ok(watcher::Event::Init | watcher::Event::InitApply(_))) => continue,
                Some(Ok(_)) => {
                    listed = true;
                    false
                }
                Some(Err(e)) => {
                    warn!("NodeConfiguration watch failed, {e}");
                    continue;
                }
                None => break,
            },
        };
        // An empty store before the first list would remove a live DaemonSet.
        if !listed {
            continue;
        }
        let desired = targets(&reader.state());
        if !force && applied.as_ref() == Some(&desired) {
            continue;
        }
        match sync(&client, &source, desired.clone()).await {
            Ok(outcome) => {
                if applied.as_ref() != Some(&desired) {
                    info!(targets = desired.len(), ?outcome, "Agent DaemonSet synced");
                }
                applied = Some(desired);
            }
            Err(e) => {
                warn!("Agent DaemonSet sync failed, {e}");
                applied = None;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use wiremock::matchers::{body_partial_json, method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    const TEMPLATE: &str = r"
apiVersion: apps/v1
kind: DaemonSet
metadata:
  name: nco-agent
spec:
  selector:
    matchLabels:
      app: agent
  template:
    metadata:
      labels:
        app: agent
    spec:
      affinity:
        podAntiAffinity: {}
      containers:
        - name: agent
          image: example.com/agent:1
          resizePolicy:
            - resourceName: cpu
              restartPolicy: NotRequired
            - resourceName: memory
              restartPolicy: RestartContainer
";

    fn source() -> Source {
        Source {
            namespace: "kube-system".into(),
            config_map: "nco-agent-template".into(),
        }
    }

    fn client(server: &MockServer) -> Client {
        let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
        Client::try_from(kube::Config::new(server.uri().parse().unwrap())).unwrap()
    }

    fn target(groups: &[&str], selector: &[(&str, &str)]) -> Target {
        Target {
            node_groups: groups.iter().map(ToString::to_string).collect(),
            node_selector: selector
                .iter()
                .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
                .collect(),
            exclude_nodes: Vec::new(),
        }
    }

    async fn config_map(server: &MockServer, data: serde_json::Value) {
        Mock::given(method("GET"))
            .and(path(
                "/api/v1/namespaces/kube-system/configmaps/nco-agent-template",
            ))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "apiVersion": "v1",
                "kind": "ConfigMap",
                "metadata": {"name": "nco-agent-template", "uid": "uid-1"},
                "data": data,
            })))
            .mount(server)
            .await;
    }

    #[test]
    fn a_term_mirrors_the_node_selector_of_its_target() {
        let all = term(&Target::default());
        assert_eq!(
            serde_json::to_value(&all).unwrap(),
            json!({"matchExpressions": [
                {"key": "karpenter.sh/nodepool", "operator": "DoesNotExist"},
                {"key": "eks.amazonaws.com/nodegroup", "operator": "Exists"},
            ]})
        );
        let narrow = term(&target(&["b", "a", "b"], &[("tier", "db")]));
        assert_eq!(
            serde_json::to_value(&narrow).unwrap(),
            json!({"matchExpressions": [
                {"key": "karpenter.sh/nodepool", "operator": "DoesNotExist"},
                {"key": "eks.amazonaws.com/nodegroup", "operator": "In", "values": ["a", "b"]},
                {"key": "tier", "operator": "In", "values": ["db"]},
            ]})
        );
    }

    #[test]
    fn terms_are_unique_and_ordered() {
        let a = target(&["a"], &[]);
        let b = target(&["b"], &[]);
        let forward = terms([&a, &b, &a]);
        assert_eq!(forward.len(), 2);
        assert_eq!(forward, terms([&b, &a]));
        assert_eq!(terms(std::iter::empty()), Vec::new());
    }

    #[tokio::test]
    async fn the_daemon_set_is_applied_with_the_target_affinity() {
        let server = MockServer::start().await;
        config_map(&server, json!({TEMPLATE_KEY: TEMPLATE})).await;
        Mock::given(method("PATCH"))
            .and(path(
                "/apis/apps/v1/namespaces/kube-system/daemonsets/nco-agent",
            ))
            .and(query_param("fieldManager", FIELD_MANAGER))
            .and(query_param("force", "true"))
            .and(body_partial_json(json!({
                "metadata": {
                    "namespace": "kube-system",
                    "ownerReferences": [{"kind": "ConfigMap", "name": "nco-agent-template", "uid": "uid-1"}],
                },
                "spec": {"template": {"spec": {
                    "containers": [{"resizePolicy": [
                        {"resourceName": "cpu", "restartPolicy": "NotRequired"},
                        {"resourceName": "memory", "restartPolicy": "RestartContainer"},
                    ]}],
                    "affinity": {
                    "podAntiAffinity": {},
                    "nodeAffinity": {"requiredDuringSchedulingIgnoredDuringExecution": {
                        "nodeSelectorTerms": [{"matchExpressions": [
                            {"key": "karpenter.sh/nodepool", "operator": "DoesNotExist"},
                            {"key": "eks.amazonaws.com/nodegroup", "operator": "In", "values": ["a"]},
                        ]}],
                    }},
                }}}},
            })))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "apiVersion": "apps/v1",
                "kind": "DaemonSet",
                "metadata": {"name": "nco-agent"},
            })))
            .expect(1)
            .mount(&server)
            .await;

        let outcome = sync(&client(&server), &source(), terms([&target(&["a"], &[])]))
            .await
            .unwrap();
        assert_eq!(outcome, Outcome::Applied);
    }

    #[tokio::test]
    async fn the_daemon_set_is_removed_when_no_target_is_left() {
        let server = MockServer::start().await;
        config_map(&server, json!({TEMPLATE_KEY: TEMPLATE})).await;
        Mock::given(method("DELETE"))
            .and(path(
                "/apis/apps/v1/namespaces/kube-system/daemonsets/nco-agent",
            ))
            .respond_with(ResponseTemplate::new(404).set_body_json(json!({
                "kind": "Status",
                "apiVersion": "v1",
                "status": "Failure",
                "reason": "NotFound",
                "code": 404,
            })))
            .expect(1)
            .mount(&server)
            .await;

        let outcome = sync(&client(&server), &source(), Vec::new()).await.unwrap();
        assert_eq!(outcome, Outcome::Removed);
    }

    const MONITOR: &str = r"
apiVersion: monitoring.coreos.com/v1
kind: PodMonitor
metadata:
  name: from-the-chart
spec:
  selector:
    matchLabels:
      app: agent
  podMetricsEndpoints:
    - port: http
      path: /metrics
";
    const MONITOR_PATH: &str =
        "/apis/monitoring.coreos.com/v1/namespaces/kube-system/podmonitors/nco-agent";

    async fn daemon_set_api(server: &MockServer) {
        let body = json!({"apiVersion": "apps/v1", "kind": "DaemonSet", "metadata": {"name": "nco-agent"}});
        for verb in ["PATCH", "DELETE"] {
            Mock::given(method(verb))
                .and(path(
                    "/apis/apps/v1/namespaces/kube-system/daemonsets/nco-agent",
                ))
                .respond_with(ResponseTemplate::new(200).set_body_json(&body))
                .mount(server)
                .await;
        }
    }

    fn status(code: u16) -> ResponseTemplate {
        ResponseTemplate::new(code).set_body_json(json!({
            "kind": "Status",
            "apiVersion": "v1",
            "status": "Failure",
            "code": code,
        }))
    }

    #[tokio::test]
    async fn the_pod_monitor_follows_the_agents() {
        let server = MockServer::start().await;
        config_map(
            &server,
            json!({TEMPLATE_KEY: TEMPLATE, MONITOR_KEY: MONITOR}),
        )
        .await;
        daemon_set_api(&server).await;
        Mock::given(method("PATCH"))
            .and(path(MONITOR_PATH))
            .and(query_param("fieldManager", FIELD_MANAGER))
            .and(body_partial_json(json!({
                "apiVersion": "monitoring.coreos.com/v1",
                "kind": "PodMonitor",
                "metadata": {
                    "name": "nco-agent",
                    "namespace": "kube-system",
                    "ownerReferences": [{"kind": "ConfigMap", "name": "nco-agent-template", "uid": "uid-1"}],
                },
                "spec": {"podMetricsEndpoints": [{"port": "http", "path": "/metrics"}]},
            })))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "apiVersion": "monitoring.coreos.com/v1",
                "kind": "PodMonitor",
                "metadata": {"name": "nco-agent"},
            })))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("DELETE"))
            .and(path(MONITOR_PATH))
            .respond_with(status(200))
            .expect(1)
            .mount(&server)
            .await;

        let client = client(&server);
        let applied = sync(&client, &source(), terms([&Target::default()])).await;
        assert_eq!(applied.unwrap(), Outcome::Applied);
        // Without a target the agents go, and the monitor with them.
        let removed = sync(&client, &source(), Vec::new()).await;
        assert_eq!(removed.unwrap(), Outcome::Removed);
    }

    #[tokio::test]
    async fn the_pod_monitor_is_removed_when_the_chart_ships_none() {
        let server = MockServer::start().await;
        config_map(&server, json!({TEMPLATE_KEY: TEMPLATE})).await;
        daemon_set_api(&server).await;
        Mock::given(method("DELETE"))
            .and(path(MONITOR_PATH))
            .respond_with(status(403))
            .expect(1)
            .mount(&server)
            .await;

        let outcome = sync(&client(&server), &source(), terms([&Target::default()])).await;
        assert_eq!(outcome.unwrap(), Outcome::Applied);
    }

    #[tokio::test]
    async fn a_pod_monitor_that_cannot_be_applied_leaves_the_agents_alone() {
        let server = MockServer::start().await;
        config_map(
            &server,
            json!({TEMPLATE_KEY: TEMPLATE, MONITOR_KEY: MONITOR}),
        )
        .await;
        daemon_set_api(&server).await;
        let client = client(&server);
        let config_map = Api::<ConfigMap>::namespaced(client.clone(), "kube-system")
            .get("nco-agent-template")
            .await
            .unwrap();

        // No mock answers the PodMonitor path, like a cluster without the CRD.
        let missing = sync_monitor(
            &client,
            &source(),
            &config_map,
            "nco-agent",
            &Outcome::Applied,
        )
        .await
        .unwrap_err();
        assert!(matches!(missing, Error::MonitorApiMissing));
        let outcome = sync(&client, &source(), terms([&Target::default()])).await;
        assert_eq!(outcome.unwrap(), Outcome::Applied);

        let mut broken = config_map.clone();
        broken
            .data
            .as_mut()
            .unwrap()
            .insert(MONITOR_KEY.to_string(), "- not\n- an object\n".to_string());
        let err = monitor(&source(), &broken, "nco-agent").unwrap_err();
        assert!(matches!(err, Error::Monitor(_)));
    }

    #[tokio::test]
    async fn a_broken_template_is_reported() {
        let server = MockServer::start().await;
        config_map(&server, json!({"other": "x"})).await;
        let err = sync(&client(&server), &source(), Vec::new())
            .await
            .unwrap_err();
        assert!(matches!(err, Error::MissingTemplate(_)));

        let nameless = MockServer::start().await;
        config_map(
            &nameless,
            json!({TEMPLATE_KEY: "apiVersion: apps/v1\nkind: DaemonSet\nmetadata: {}\n"}),
        )
        .await;
        let err = sync(&client(&nameless), &source(), Vec::new())
            .await
            .unwrap_err();
        assert!(matches!(err, Error::Incomplete("metadata.name")));

        let specless = MockServer::start().await;
        config_map(
            &specless,
            json!({TEMPLATE_KEY: "apiVersion: apps/v1\nkind: DaemonSet\nmetadata:\n  name: a\n"}),
        )
        .await;
        let err = sync(&client(&specless), &source(), terms([&Target::default()]))
            .await
            .unwrap_err();
        assert!(matches!(err, Error::Incomplete("spec.template.spec")));
    }
}
