//! The controller: rolls each `NodeConfiguration` out to the nodes it targets
//! through one `NodeConfigurationBinding` per node, and reports the rollout.

pub mod agentset;
pub mod cleanup;
pub mod cordon;
pub mod leader;
pub mod results;
pub mod rollout;
pub mod status;

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::time::Duration;

use k8s_openapi::api::core::v1::Node;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::{ObjectMeta, OwnerReference};
use kube::api::{Api, DeleteParams, ListParams, Patch, PatchParams, PostParams};
use kube::runtime::controller::Action;
use kube::runtime::events::Recorder;
use kube::{Client, ResourceExt as _};
use serde_json::json;
use tracing::{info, warn};

use crate::metrics::Metrics;
use node_configuration_common::crd::binding::{
    self, CONFIGURATION_LABEL, NODE_LABEL, NodeConfigurationBinding, NodeConfigurationBindingSpec,
};
use node_configuration_common::crd::{NodeConfiguration, Target, validate};
use node_configuration_common::payload::AgentPayload;
use status::Observation;

pub const NODE_GROUP_LABEL: &str = "eks.amazonaws.com/nodegroup";
pub const KARPENTER_LABEL: &str = "karpenter.sh/nodepool";
const BINDING_KIND: &str = "NodeConfigurationBinding";
const RESYNC: Duration = Duration::from_secs(300);
const RETRY: Duration = Duration::from_secs(30);

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("Kubernetes API request failed, {0}")]
    Kube(#[from] kube::Error),
    #[error("could not {action} binding {binding}, {source}")]
    Binding {
        action: &'static str,
        binding: String,
        source: Box<kube::Error>,
    },
    #[error("could not update the status of NodeConfiguration {name}, {source}")]
    Status {
        name: String,
        source: Box<kube::Error>,
    },
}

fn failed(action: &'static str, binding: &str) -> impl FnOnce(kube::Error) -> Error {
    let binding = binding.to_string();
    move |source| Error::Binding {
        action,
        binding,
        source: Box::new(source),
    }
}

pub struct Context {
    pub client: Client,
    pub recorder: Recorder,
    pub metrics: Arc<Metrics>,
}

/// The label selector of the nodes a target covers: managed node group nodes
/// that Karpenter does not own, narrowed by the target.
pub fn node_selector(target: &Target) -> String {
    let mut terms = vec![format!("!{KARPENTER_LABEL}")];
    if target.node_groups.is_empty() {
        terms.push(NODE_GROUP_LABEL.to_string());
    } else {
        terms.push(format!(
            "{NODE_GROUP_LABEL} in ({})",
            target.node_groups.join(",")
        ));
    }
    terms.extend(target.node_selector.iter().map(|(k, v)| format!("{k}={v}")));
    terms.join(",")
}

fn node_owner(node: &str, uid: &str) -> OwnerReference {
    OwnerReference {
        api_version: "v1".to_string(),
        kind: "Node".to_string(),
        name: node.to_string(),
        uid: uid.to_string(),
        controller: None,
        block_owner_deletion: None,
    }
}

/// The UID of the node that owns a binding. A binding written before the
/// node became the owner has none.
fn owner_uid(binding: &NodeConfigurationBinding) -> Option<&str> {
    binding
        .owner_references()
        .iter()
        .find(|owner| owner.kind == "Node")
        .map(|owner| owner.uid.as_str())
}

// The node is the only owner, so the garbage collector removes the binding
// with the node, also while no controller runs.
fn new_binding(
    configuration: &str,
    node: &str,
    uid: &str,
    spec: NodeConfigurationBindingSpec,
) -> NodeConfigurationBinding {
    NodeConfigurationBinding {
        metadata: ObjectMeta {
            name: Some(binding::name(configuration, node)),
            labels: Some(
                [
                    (CONFIGURATION_LABEL.to_string(), configuration.to_string()),
                    (NODE_LABEL.to_string(), binding::node_key(node)),
                ]
                .into(),
            ),
            owner_references: Some(vec![node_owner(node, uid)]),
            ..ObjectMeta::default()
        },
        spec,
        status: None,
    }
}

/// Deletes a binding. One the garbage collector removed first is fine.
pub async fn remove(api: &Api<NodeConfigurationBinding>, binding: &str) -> Result<(), kube::Error> {
    match api.delete(binding, &DeleteParams::default()).await {
        Ok(_) => Ok(()),
        Err(kube::Error::Api(status)) if status.code == 404 => Ok(()),
        Err(e) => Err(e),
    }
}

fn current_spec(
    obj: &NodeConfiguration,
    node: &str,
    revision: &str,
) -> NodeConfigurationBindingSpec {
    NodeConfigurationBindingSpec {
        configuration: obj.name_any(),
        node_name: node.to_string(),
        revision: revision.to_string(),
        group: obj.spec.group.clone(),
        priority: obj.spec.priority,
        dry_run: obj.spec.dry_run,
        interval_seconds: obj.spec.interval_seconds,
        rules: obj.spec.rules.clone(),
    }
}

pub async fn reconcile(obj: Arc<NodeConfiguration>, ctx: Arc<Context>) -> Result<Action, Error> {
    let result = apply(&obj, &ctx).await;
    ctx.metrics.reconciled(result.is_ok());
    result
}

/// The targeted nodes with their UID, and the listed nodes `name` holds
/// cordoned.
async fn nodes_of(
    obj: &NodeConfiguration,
    ctx: &Context,
) -> Result<(BTreeMap<String, String>, BTreeSet<String>), Error> {
    let name = obj.name_any();
    let mut nodes = BTreeMap::new();
    let mut cordoned = BTreeSet::new();
    for node in Api::<Node>::all(ctx.client.clone())
        .list_metadata(&ListParams::default().labels(&node_selector(&obj.spec.target)))
        .await?
    {
        let node_name = node.name_any();
        if cordon::holders(node.metadata.annotations.as_ref()).contains(&name) {
            cordoned.insert(node_name.clone());
        }
        if node.metadata.deletion_timestamp.is_none()
            && !obj.spec.target.exclude_nodes.contains(&node_name)
        {
            nodes.insert(node_name, node.uid().unwrap_or_default());
        }
    }
    Ok((nodes, cordoned))
}

/// The bindings of `name`, without those of a node that was replaced under
/// the same name. Such a node has not run anything yet, so it starts over.
async fn bindings_of(
    name: &str,
    nodes: &BTreeMap<String, String>,
    api: &Api<NodeConfigurationBinding>,
) -> Result<Vec<NodeConfigurationBinding>, Error> {
    let mut bindings = Vec::new();
    for binding in api
        .list(&ListParams::default().labels(&format!("{CONFIGURATION_LABEL}={name}")))
        .await?
    {
        let node = &binding.spec.node_name;
        let replaced = owner_uid(&binding)
            .zip(nodes.get(node))
            .is_some_and(|(owner, current)| owner != current);
        if replaced {
            let stale = binding.name_any();
            remove(api, &stale)
                .await
                .map_err(failed("delete", &stale))?;
            info!(
                kind = BINDING_KIND,
                name,
                node,
                binding = stale,
                "Deleted the binding of a node that was replaced"
            );
        } else {
            bindings.push(binding);
        }
    }
    Ok(bindings)
}

async fn untarget(
    ctx: &Context,
    api: &Api<NodeConfigurationBinding>,
    name: &str,
    stale: &NodeConfigurationBinding,
) -> Result<(), Error> {
    let binding = stale.name_any();
    let node = &stale.spec.node_name;
    cordon::free(ctx, node, name, cordon::Why::Untargeted).await?;
    remove(api, &binding)
        .await
        .map_err(failed("delete", &binding))?;
    info!(
        kind = BINDING_KIND,
        name, node, binding, "Deleted the binding of a node that is no longer targeted"
    );
    Ok(())
}

async fn roll_out(obj: &NodeConfiguration, ctx: &Context) -> Result<Observation, Error> {
    let name = obj.name_any();
    let revision = AgentPayload::of(&obj.spec).revision();
    let cordons = obj.spec.cordons();

    let (nodes, cordoned) = nodes_of(obj, ctx).await?;
    if !cordons {
        for node in &cordoned {
            cordon::free(ctx, node, &name, cordon::Why::Disabled).await?;
        }
    }
    let api: Api<NodeConfigurationBinding> = Api::all(ctx.client.clone());
    let bindings = bindings_of(&name, &nodes, &api).await?;

    let targeted: BTreeSet<String> = nodes.keys().cloned().collect();
    let now = chrono::Utc::now();
    let plan = rollout::plan(&obj.spec, &revision, &targeted, &bindings, now);
    for index in &plan.delete {
        untarget(ctx, &api, &name, &bindings[*index]).await?;
    }
    for create in &plan.create {
        let spec = create.held.map_or_else(
            || current_spec(obj, &create.node, &revision),
            |held| NodeConfigurationBindingSpec {
                node_name: create.node.clone(),
                ..bindings[held].spec.clone()
            },
        );
        let assigned = spec.revision.clone();
        let uid = nodes.get(&create.node).map_or("", String::as_str);
        let binding = new_binding(&name, &create.node, uid, spec);
        api.create(&PostParams::default(), &binding)
            .await
            .map_err(failed("create", &binding.name_any()))?;
        info!(
            kind = BINDING_KIND,
            name,
            node = create.node,
            binding = binding.name_any(),
            revision = assigned,
            "Assigned a revision to a new node"
        );
    }
    for (index, binding) in bindings.iter().enumerate() {
        let node = &binding.spec.node_name;
        let Some(uid) = nodes.get(node) else {
            continue;
        };
        let moves = plan.update.contains(&index);
        let adopted = owner_uid(binding) != Some(uid.as_str());
        if moves || adopted {
            let mut updated = binding.clone();
            updated.metadata.owner_references = Some(vec![node_owner(node, uid)]);
            if moves {
                if cordons {
                    cordon::acquire(ctx, node, &name, &revision).await?;
                }
                updated.spec = current_spec(obj, node, &revision);
            }
            let binding = updated.name_any();
            api.replace(&binding, &PostParams::default(), &updated)
                .await
                .map_err(failed("update", &binding))?;
            if moves {
                info!(
                    kind = BINDING_KIND,
                    name,
                    node,
                    binding,
                    from = bindings[index].spec.revision,
                    revision,
                    "Moved a node to the current revision"
                );
            } else {
                info!(
                    kind = BINDING_KIND,
                    name, node, binding, "Handed the binding over to its node"
                );
            }
        }
        if cordons
            && !moves
            && cordoned.contains(node)
            && rollout::available(binding, obj.spec.min_ready_seconds, now)
        {
            cordon::free(ctx, node, &name, cordon::Why::Configured).await?;
        }
    }

    let results = results::collect(obj, &revision, &targeted, &bindings);
    Ok(Observation::Rollout {
        revision,
        counts: plan.counts,
        results,
    })
}

async fn apply(obj: &NodeConfiguration, ctx: &Context) -> Result<Action, Error> {
    let name = obj.name_any();
    if obj.metadata.deletion_timestamp.is_some() {
        return Ok(Action::await_change());
    }
    let problems = validate::problems(&name, &obj.spec);
    let observation = if problems.is_empty() {
        roll_out(obj, ctx).await?
    } else {
        warn!(name, problems = ?problems, "NodeConfiguration is invalid, no node is updated");
        Observation::Invalid(problems)
    };

    let status = status::compute(obj, &observation, chrono::Utc::now());
    if obj.status.as_ref() != Some(&status) {
        let api: Api<NodeConfiguration> = Api::all(ctx.client.clone());
        api.patch_status(
            &name,
            &PatchParams::default(),
            &Patch::Merge(json!({ "status": status })),
        )
        .await
        .map_err(|source| Error::Status {
            name: name.clone(),
            source: Box::new(source),
        })?;
        info!(
            name,
            revision = status.revision,
            desired_nodes = status.desired_nodes,
            updated_nodes = status.updated_nodes,
            configured_nodes = status.configured_nodes,
            "Updated NodeConfiguration status"
        );
    }
    // A rollout is checked again soon, for the progress deadline and for nodes
    // that are waiting out minReadySeconds.
    Ok(Action::requeue(if status::rolling_out(&status) {
        Duration::from_secs(u64::from(obj.spec.min_ready_seconds.clamp(5, 30)))
    } else {
        RESYNC
    }))
}

#[allow(clippy::needless_pass_by_value)]
pub fn error_policy(obj: Arc<NodeConfiguration>, error: &Error, _ctx: Arc<Context>) -> Action {
    warn!(name = obj.name_any(), "Reconcile failed, {error}");
    Action::requeue(RETRY)
}

#[cfg(test)]
mod tests {
    use super::*;
    use kube::Resource as _;
    use node_configuration_common::crd::binding::NodeConfigurationBindingStatus;
    use node_configuration_common::crd::testing::resource;
    use node_configuration_common::crd::{FileRule, LineEditRule, NamedRule, RollingUpdate, Rules};
    use prometheus_client::registry::Registry;
    use wiremock::matchers::{method, path, path_regex, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    const GROUP_PATH: &str = "/apis/node-configuration-operator.sh/v1alpha1";
    const EVENTS_PATH: &str = "/apis/events.k8s.io/v1/namespaces/default/events";

    fn context(server: &MockServer) -> Arc<Context> {
        let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
        let config = kube::Config::new(server.uri().parse().unwrap());
        let client = Client::try_from(config).unwrap();
        Arc::new(Context {
            recorder: Recorder::new(
                client.clone(),
                "node-configuration-operator".to_string().into(),
            ),
            client,
            metrics: Arc::new(Metrics::new(&mut Registry::default())),
        })
    }

    fn motd() -> Rules {
        vec![NamedRule::file(
            "motd",
            FileRule {
                path: "/etc/motd".into(),
                content: "hi\n".into(),
                mode: "0644".into(),
            },
        )]
    }

    fn applied(obj: &NodeConfiguration, node: &str, revision: &str) -> NodeConfigurationBinding {
        let mut binding = new_binding(
            &obj.name_any(),
            node,
            &format!("uid-{node}"),
            current_spec(obj, node, revision),
        );
        binding.metadata.resource_version = Some("7".to_string());
        binding.status = Some(NodeConfigurationBindingStatus {
            observed_revision: Some(revision.to_string()),
            ready: true,
            ..NodeConfigurationBindingStatus::default()
        });
        binding
    }

    async fn mock(server: &MockServer, verb: &str, at: String, body: serde_json::Value) {
        Mock::given(method(verb))
            .and(path(at))
            .respond_with(ResponseTemplate::new(200).set_body_json(body))
            .mount(server)
            .await;
    }

    async fn cluster(
        server: &MockServer,
        obj: &NodeConfiguration,
        nodes: &[&str],
        bindings: &[NodeConfigurationBinding],
    ) {
        let items = nodes.iter().map(|n| node(n, None)).collect();
        cluster_of(server, obj, items, bindings).await;
    }

    fn node(name: &str, cordoned_by: Option<&str>) -> serde_json::Value {
        let mut node = json!({
            "apiVersion": "v1",
            "kind": "Node",
            "metadata": {"name": name, "uid": format!("uid-{name}"), "resourceVersion": "5"}
        });
        if let Some(holder) = cordoned_by {
            node["metadata"]["annotations"] = json!({cordon::HOLDERS_ANNOTATION: holder});
            node["spec"] = json!({"unschedulable": true});
        }
        node
    }

    async fn node_api(server: &MockServer, node: &serde_json::Value) {
        let at = format!(
            "/api/v1/nodes/{}",
            node["metadata"]["name"].as_str().unwrap()
        );
        mock(server, "GET", at.clone(), node.clone()).await;
        mock(server, "PATCH", at, node.clone()).await;
        mock(
            server,
            "POST",
            EVENTS_PATH.to_string(),
            json!({
                "apiVersion": "events.k8s.io/v1",
                "kind": "Event",
                "metadata": {"name": "event", "namespace": "default"}
            }),
        )
        .await;
    }

    fn cordoning() -> NodeConfiguration {
        let mut obj = resource(motd());
        obj.spec.strategy.rolling_update = Some(RollingUpdate {
            max_unavailable: None,
            cordon: true,
        });
        obj
    }

    fn gone() -> ResponseTemplate {
        ResponseTemplate::new(404).set_body_json(json!({
            "apiVersion": "v1", "kind": "Status", "status": "Failure", "reason": "NotFound", "code": 404
        }))
    }

    async fn cluster_of(
        server: &MockServer,
        obj: &NodeConfiguration,
        items: Vec<serde_json::Value>,
        bindings: &[NodeConfigurationBinding],
    ) {
        Mock::given(method("GET"))
            .and(path_regex("^/api/v1/nodes/[^/]+$"))
            .respond_with(gone())
            .with_priority(9)
            .mount(server)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/v1/nodes"))
            .and(query_param(
                "labelSelector",
                "!karpenter.sh/nodepool,eks.amazonaws.com/nodegroup",
            ))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "apiVersion": "v1", "kind": "PartialObjectMetadataList", "metadata": {}, "items": items
            })))
            .mount(server)
            .await;
        mock(
            server,
            "GET",
            format!("{GROUP_PATH}/nodeconfigurationbindings"),
            json!({
                "apiVersion": "node-configuration-operator.sh/v1alpha1",
                "kind": "NodeConfigurationBindingList",
                "metadata": {},
                "items": bindings
            }),
        )
        .await;
        mock(
            server,
            "PATCH",
            format!("{GROUP_PATH}/nodeconfigurations/mng/status"),
            json!(obj),
        )
        .await;
    }

    async fn requests(server: &MockServer) -> Vec<(String, String, serde_json::Value)> {
        server
            .received_requests()
            .await
            .unwrap()
            .iter()
            .filter(|r| r.method.as_str() != "GET")
            .map(|r| {
                (
                    r.method.to_string(),
                    r.url.path().trim_start_matches(GROUP_PATH).to_string(),
                    serde_json::from_slice(&r.body).unwrap_or_default(),
                )
            })
            .collect()
    }

    #[test]
    fn node_selector_always_excludes_karpenter() {
        assert_eq!(
            node_selector(&Target::default()),
            "!karpenter.sh/nodepool,eks.amazonaws.com/nodegroup"
        );
        let target = Target {
            node_groups: vec!["system".into(), "batch".into()],
            node_selector: [("kubernetes.io/arch".to_string(), "arm64".to_string())].into(),
            exclude_nodes: Vec::new(),
        };
        assert_eq!(
            node_selector(&target),
            "!karpenter.sh/nodepool,eks.amazonaws.com/nodegroup in (system,batch),kubernetes.io/arch=arm64"
        );
    }

    #[tokio::test]
    async fn a_rollout_creates_updates_and_deletes_bindings() {
        let server = MockServer::start().await;
        let obj = resource(motd());
        let revision = AgentPayload::of(&obj.spec).revision();
        let bindings = [
            applied(&obj, "a", "old"),
            applied(&obj, "b", "old"),
            applied(&obj, "gone", "old"),
        ];
        cluster(&server, &obj, &["a", "b", "c"], &bindings).await;
        mock(
            &server,
            "POST",
            format!("{GROUP_PATH}/nodeconfigurationbindings"),
            json!(bindings[0]),
        )
        .await;
        mock(
            &server,
            "PUT",
            format!("{GROUP_PATH}/nodeconfigurationbindings/mng.a"),
            json!(bindings[0]),
        )
        .await;
        mock(
            &server,
            "DELETE",
            format!("{GROUP_PATH}/nodeconfigurationbindings/mng.gone"),
            json!({"apiVersion": "v1", "kind": "Status", "status": "Success"}),
        )
        .await;

        let action = reconcile(Arc::new(obj), context(&server)).await.unwrap();
        assert_eq!(action, Action::requeue(Duration::from_secs(5)));

        let sent = requests(&server).await;
        let calls: Vec<(&str, &str)> = sent
            .iter()
            .map(|(verb, at, _)| (verb.as_str(), at.as_str()))
            .collect();
        assert_eq!(
            calls,
            [
                ("DELETE", "/nodeconfigurationbindings/mng.gone"),
                ("POST", "/nodeconfigurationbindings"),
                ("PATCH", "/nodeconfigurations/mng/status"),
            ],
            "the new node uses up the budget of one, so no old node moves yet"
        );
        let created = &sent[1].2;
        assert_eq!(created["metadata"]["name"], "mng.c");
        assert_eq!(created["metadata"]["labels"][NODE_LABEL], "c");
        assert_eq!(
            created["metadata"]["ownerReferences"],
            json!([{"apiVersion": "v1", "kind": "Node", "name": "c", "uid": "uid-c"}]),
            "the node is the only owner"
        );
        assert_eq!(created["spec"]["revision"], revision);
        assert_eq!(created["spec"]["rules"][0]["name"], "motd");
        assert_eq!(created["spec"]["rules"][0]["file"]["path"], "/etc/motd");
        let status = &sent[2].2["status"];
        assert_eq!(status["desiredNodes"], 3);
        assert_eq!(status["updatedNodes"], 1);
        assert_eq!(status["configuredNodes"], 2);
        assert_eq!(status["conditions"][1]["reason"], "RollingOut");
    }

    #[tokio::test]
    async fn an_excluded_node_loses_its_binding() {
        let server = MockServer::start().await;
        let mut obj = resource(motd());
        obj.spec.target.exclude_nodes = vec!["b".to_string()];
        let revision = AgentPayload::of(&obj.spec).revision();
        let bindings = [applied(&obj, "a", &revision), applied(&obj, "b", &revision)];
        cluster(&server, &obj, &["a", "b"], &bindings).await;
        Mock::given(method("DELETE"))
            .and(path(format!(
                "{GROUP_PATH}/nodeconfigurationbindings/mng.b"
            )))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(
                    json!({"apiVersion": "v1", "kind": "Status", "status": "Success"}),
                ),
            )
            .expect(1)
            .mount(&server)
            .await;

        reconcile(Arc::new(obj), context(&server)).await.unwrap();
    }

    #[tokio::test]
    async fn an_old_node_moves_once_the_budget_is_free() {
        let server = MockServer::start().await;
        let obj = resource(motd());
        let revision = AgentPayload::of(&obj.spec).revision();
        let bindings = [applied(&obj, "a", "old"), applied(&obj, "b", &revision)];
        cluster(&server, &obj, &["a", "b"], &bindings).await;
        mock(
            &server,
            "PUT",
            format!("{GROUP_PATH}/nodeconfigurationbindings/mng.a"),
            json!(bindings[0]),
        )
        .await;

        reconcile(Arc::new(obj), context(&server)).await.unwrap();
        let sent = requests(&server).await;
        assert_eq!(sent[0].0, "PUT");
        assert_eq!(sent[0].2["spec"]["revision"], revision);
        assert_eq!(sent[0].2["metadata"]["resourceVersion"], "7");
        assert_eq!(sent[1].2["status"]["updatedNodes"], 2);
    }

    #[tokio::test]
    async fn a_paused_resource_gives_a_new_node_the_held_revision() {
        let server = MockServer::start().await;
        let mut obj = resource(motd());
        obj.spec.paused = true;
        let mut held = applied(&obj, "a", "old");
        held.spec.rules = Rules::default();
        cluster(&server, &obj, &["a", "b"], std::slice::from_ref(&held)).await;
        mock(
            &server,
            "POST",
            format!("{GROUP_PATH}/nodeconfigurationbindings"),
            json!(held),
        )
        .await;

        let action = reconcile(Arc::new(obj), context(&server)).await.unwrap();
        assert_eq!(action, Action::requeue(RESYNC));
        let sent = requests(&server).await;
        assert_eq!(sent[0].2["spec"]["revision"], "old");
        assert_eq!(sent[0].2["spec"]["nodeName"], "b");
        assert_eq!(sent[0].2["spec"]["rules"], json!([]));
        assert_eq!(
            sent[1].2["status"]["conditions"][1]["reason"],
            "RolloutPaused"
        );
    }

    #[tokio::test]
    async fn an_invalid_resource_only_reports_its_problems() {
        let server = MockServer::start().await;
        let obj = resource(vec![NamedRule::line_edit(
            "hosts",
            LineEditRule {
                path: "/etc/hosts".into(),
                pattern: "(".into(),
                replacement: String::new(),
            },
        )]);
        cluster(&server, &obj, &[], &[]).await;

        reconcile(Arc::new(obj), context(&server)).await.unwrap();
        let all = server.received_requests().await.unwrap();
        assert_eq!(all.len(), 1);
        let sent = requests(&server).await;
        assert_eq!(
            sent[0].2["status"]["conditions"][0]["reason"],
            "InvalidSpec"
        );
        assert_eq!(
            sent[0].2["status"]["conditions"][1]["reason"],
            "InvalidSpec"
        );
    }

    #[tokio::test]
    async fn a_cordoning_rollout_cordons_the_node_before_it_moves() {
        let server = MockServer::start().await;
        let obj = cordoning();
        let revision = AgentPayload::of(&obj.spec).revision();
        let bindings = [applied(&obj, "a", "old"), applied(&obj, "b", &revision)];
        cluster(&server, &obj, &["a", "b"], &bindings).await;
        node_api(&server, &node("a", None)).await;
        mock(
            &server,
            "PUT",
            format!("{GROUP_PATH}/nodeconfigurationbindings/mng.a"),
            json!(bindings[0]),
        )
        .await;

        reconcile(Arc::new(obj), context(&server)).await.unwrap();
        let sent = requests(&server).await;
        let calls: Vec<(&str, &str)> = sent
            .iter()
            .map(|(verb, at, _)| (verb.as_str(), at.as_str()))
            .collect();
        assert_eq!(
            calls,
            [
                ("PATCH", "/api/v1/nodes/a"),
                ("POST", EVENTS_PATH),
                ("PUT", "/nodeconfigurationbindings/mng.a"),
                ("PATCH", "/nodeconfigurations/mng/status"),
            ]
        );
        assert_eq!(sent[0].2["spec"]["unschedulable"], true);
        assert_eq!(sent[0].2["metadata"]["resourceVersion"], "5");
        assert_eq!(
            sent[0].2["metadata"]["annotations"][cordon::HOLDERS_ANNOTATION],
            "mng"
        );
        assert_eq!(sent[1].2["reason"], "Cordon");
        assert_eq!(sent[1].2["type"], "Normal");
        assert_eq!(sent[1].2["regarding"]["kind"], "Node");
        assert_eq!(sent[1].2["regarding"]["name"], "a");
        assert_eq!(
            sent[1].2["note"],
            format!("Cordoned the node while NodeConfiguration mng applies revision {revision}")
        );
    }

    #[tokio::test]
    async fn a_configured_node_is_uncordoned() {
        let server = MockServer::start().await;
        let obj = cordoning();
        let revision = AgentPayload::of(&obj.spec).revision();
        let mut moving = applied(&obj, "b", &revision);
        moving.status.as_mut().unwrap().observed_revision = Some("old".to_string());
        let bindings = [applied(&obj, "a", &revision), moving];
        let nodes = vec![node("a", Some("mng")), node("b", Some("mng"))];
        cluster_of(&server, &obj, nodes, &bindings).await;
        node_api(&server, &node("a", Some("mng"))).await;

        reconcile(Arc::new(obj), context(&server)).await.unwrap();
        let sent = requests(&server).await;
        assert_eq!(sent[0].1, "/api/v1/nodes/a");
        assert_eq!(sent[0].2["spec"], json!({"unschedulable": null}));
        assert_eq!(
            sent[0].2["metadata"]["annotations"],
            json!({cordon::HOLDERS_ANNOTATION: null})
        );
        assert_eq!(sent[1].2["reason"], "Uncordon");
        assert_eq!(
            sent[1].2["note"],
            "Uncordoned the node, NodeConfiguration mng is configured on it"
        );
        assert!(
            sent.iter().all(|(_, at, _)| at != "/api/v1/nodes/b"),
            "the node that still applies its revision stays cordoned"
        );
    }

    #[tokio::test]
    async fn a_node_that_leaves_the_target_is_uncordoned() {
        let server = MockServer::start().await;
        let mut obj = cordoning();
        obj.spec.target.exclude_nodes = vec!["b".to_string()];
        let revision = AgentPayload::of(&obj.spec).revision();
        let bindings = [applied(&obj, "a", &revision), applied(&obj, "b", "old")];
        let nodes = vec![node("a", None), node("b", Some("mng"))];
        cluster_of(&server, &obj, nodes, &bindings).await;
        node_api(&server, &node("b", Some("mng"))).await;
        mock(
            &server,
            "DELETE",
            format!("{GROUP_PATH}/nodeconfigurationbindings/mng.b"),
            json!({"apiVersion": "v1", "kind": "Status", "status": "Success"}),
        )
        .await;

        reconcile(Arc::new(obj), context(&server)).await.unwrap();
        let sent = requests(&server).await;
        assert_eq!(sent[0].1, "/api/v1/nodes/b");
        assert_eq!(
            sent[1].2["note"],
            "Uncordoned the node, NodeConfiguration mng no longer targets it"
        );
        assert_eq!(sent[2].0, "DELETE");
    }

    #[tokio::test]
    async fn turning_cordon_off_releases_the_nodes() {
        let server = MockServer::start().await;
        let mut obj = cordoning();
        obj.spec.dry_run = true;
        let revision = AgentPayload::of(&obj.spec).revision();
        let bindings = [applied(&obj, "a", &revision)];
        cluster_of(&server, &obj, vec![node("a", Some("mng"))], &bindings).await;
        node_api(&server, &node("a", Some("mng"))).await;

        reconcile(Arc::new(obj), context(&server)).await.unwrap();
        let sent = requests(&server).await;
        assert_eq!(sent[0].1, "/api/v1/nodes/a");
        assert_eq!(
            sent[1].2["note"],
            "Uncordoned the node, NodeConfiguration mng no longer cordons nodes"
        );
    }

    #[tokio::test]
    async fn a_node_someone_else_cordoned_is_left_alone() {
        let server = MockServer::start().await;
        let obj = cordoning();
        let bindings = [applied(&obj, "a", "old")];
        cluster(&server, &obj, &["a"], &bindings).await;
        let mut foreign = node("a", None);
        foreign["spec"] = json!({"unschedulable": true});
        mock(&server, "GET", "/api/v1/nodes/a".to_string(), foreign).await;
        mock(
            &server,
            "PUT",
            format!("{GROUP_PATH}/nodeconfigurationbindings/mng.a"),
            json!(bindings[0]),
        )
        .await;

        reconcile(Arc::new(obj), context(&server)).await.unwrap();
        let sent = requests(&server).await;
        let calls: Vec<(&str, &str)> = sent
            .iter()
            .map(|(verb, at, _)| (verb.as_str(), at.as_str()))
            .collect();
        assert_eq!(
            calls,
            [
                ("PUT", "/nodeconfigurationbindings/mng.a"),
                ("PATCH", "/nodeconfigurations/mng/status"),
            ]
        );
    }

    #[tokio::test]
    async fn a_node_replaced_under_the_same_name_starts_over() {
        let server = MockServer::start().await;
        let obj = resource(motd());
        let revision = AgentPayload::of(&obj.spec).revision();
        let mut previous = applied(&obj, "a", &revision);
        previous.metadata.owner_references = Some(vec![node_owner("a", "uid-before")]);
        cluster(&server, &obj, &["a"], std::slice::from_ref(&previous)).await;
        Mock::given(method("DELETE"))
            .and(path(format!(
                "{GROUP_PATH}/nodeconfigurationbindings/mng.a"
            )))
            .respond_with(gone())
            .mount(&server)
            .await;
        mock(
            &server,
            "POST",
            format!("{GROUP_PATH}/nodeconfigurationbindings"),
            json!(previous),
        )
        .await;

        reconcile(Arc::new(obj), context(&server)).await.unwrap();
        let sent = requests(&server).await;
        assert_eq!(
            sent[0].0, "DELETE",
            "one the collector removed first is fine"
        );
        assert_eq!(sent[1].0, "POST");
        assert_eq!(sent[1].2["metadata"]["ownerReferences"][0]["uid"], "uid-a");
        assert_eq!(sent[2].2["status"]["configuredNodes"], 0);
    }

    #[tokio::test]
    async fn a_binding_owned_by_its_configuration_is_handed_to_the_node() {
        let server = MockServer::start().await;
        let obj = resource(motd());
        let revision = AgentPayload::of(&obj.spec).revision();
        let mut legacy = applied(&obj, "a", &revision);
        legacy.metadata.owner_references = obj.controller_owner_ref(&()).map(|owner| vec![owner]);
        cluster(&server, &obj, &["a"], std::slice::from_ref(&legacy)).await;
        mock(
            &server,
            "PUT",
            format!("{GROUP_PATH}/nodeconfigurationbindings/mng.a"),
            json!(legacy),
        )
        .await;

        reconcile(Arc::new(obj), context(&server)).await.unwrap();
        let sent = requests(&server).await;
        assert_eq!(sent[0].0, "PUT");
        assert_eq!(
            sent[0].2["metadata"]["ownerReferences"],
            json!([{"apiVersion": "v1", "kind": "Node", "name": "a", "uid": "uid-a"}])
        );
        assert_eq!(sent[0].2["spec"]["revision"], revision);
    }

    #[tokio::test]
    async fn a_resource_that_is_being_deleted_is_left_to_the_cleanup() {
        let server = MockServer::start().await;
        let mut obj = json!(resource(motd()));
        obj["metadata"]["deletionTimestamp"] = json!("2026-11-13T00:00:00Z");
        let obj: NodeConfiguration = serde_json::from_value(obj).unwrap();

        let action = reconcile(Arc::new(obj), context(&server)).await.unwrap();
        assert_eq!(action, Action::await_change());
        assert!(server.received_requests().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn an_api_failure_is_retried() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;
        let obj = Arc::new(resource(Rules::default()));
        let ctx = context(&server);
        let error = reconcile(obj.clone(), ctx.clone()).await.unwrap_err();
        assert_eq!(error_policy(obj, &error, ctx), Action::requeue(RETRY));
    }
}
