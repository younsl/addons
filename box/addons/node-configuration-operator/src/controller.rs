//! The controller: rolls each `NodeConfiguration` out to the nodes it targets
//! through one `NodeConfigurationBinding` per node, and reports the rollout.

pub mod agentset;
pub mod results;
pub mod rollout;
pub mod status;

use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::Duration;

use k8s_openapi::api::core::v1::Node;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta;
use kube::api::{Api, DeleteParams, ListParams, Patch, PatchParams, PostParams};
use kube::runtime::controller::Action;
use kube::{Client, Resource as _, ResourceExt as _};
use serde_json::json;
use tracing::{info, warn};

use crate::crd::binding::{
    self, CONFIGURATION_LABEL, NODE_LABEL, NodeConfigurationBinding, NodeConfigurationBindingSpec,
};
use crate::crd::{NodeConfiguration, Target, validate};
use crate::payload::AgentPayload;
use crate::telemetry::metrics::Metrics;
use status::Observation;

pub const NODE_GROUP_LABEL: &str = "eks.amazonaws.com/nodegroup";
pub const KARPENTER_LABEL: &str = "karpenter.sh/nodepool";
const RESYNC: Duration = Duration::from_secs(300);
const RETRY: Duration = Duration::from_secs(30);

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("Kubernetes API request failed, {0}")]
    Kube(#[from] kube::Error),
}

pub struct Context {
    pub client: Client,
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

fn new_binding(
    obj: &NodeConfiguration,
    node: &str,
    spec: NodeConfigurationBindingSpec,
) -> NodeConfigurationBinding {
    let configuration = obj.name_any();
    NodeConfigurationBinding {
        metadata: ObjectMeta {
            name: Some(binding::name(&configuration, node)),
            labels: Some(
                [
                    (CONFIGURATION_LABEL.to_string(), configuration),
                    (NODE_LABEL.to_string(), binding::node_key(node)),
                ]
                .into(),
            ),
            owner_references: obj.controller_owner_ref(&()).map(|owner| vec![owner]),
            ..ObjectMeta::default()
        },
        spec,
        status: None,
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

async fn roll_out(obj: &NodeConfiguration, ctx: &Context) -> Result<Observation, Error> {
    let name = obj.name_any();
    let revision = AgentPayload::of(&obj.spec).revision();

    let nodes: BTreeSet<String> = Api::<Node>::all(ctx.client.clone())
        .list_metadata(&ListParams::default().labels(&node_selector(&obj.spec.target)))
        .await?
        .into_iter()
        .filter(|node| node.metadata.deletion_timestamp.is_none())
        .map(|node| node.name_any())
        .filter(|node| !obj.spec.target.exclude_nodes.contains(node))
        .collect();
    let api: Api<NodeConfigurationBinding> = Api::all(ctx.client.clone());
    let bindings = api
        .list(&ListParams::default().labels(&format!("{CONFIGURATION_LABEL}={name}")))
        .await?
        .items;

    let plan = rollout::plan(&obj.spec, &revision, &nodes, &bindings, chrono::Utc::now());
    for index in &plan.delete {
        let stale = bindings[*index].name_any();
        api.delete(&stale, &DeleteParams::default()).await?;
        info!(
            name,
            binding = stale,
            "Deleted the binding of a node that is no longer targeted"
        );
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
        api.create(
            &PostParams::default(),
            &new_binding(obj, &create.node, spec),
        )
        .await?;
        info!(
            name,
            node = create.node,
            revision = assigned,
            "Assigned a revision to a new node"
        );
    }
    for index in &plan.update {
        let mut updated = bindings[*index].clone();
        let node = updated.spec.node_name.clone();
        updated.spec = current_spec(obj, &node, &revision);
        api.replace(&updated.name_any(), &PostParams::default(), &updated)
            .await?;
        info!(name, node, revision, "Moved a node to the current revision");
    }

    let results = results::collect(obj, &revision, &nodes, &bindings);
    Ok(Observation::Rollout {
        revision,
        counts: plan.counts,
        results,
    })
}

async fn apply(obj: &NodeConfiguration, ctx: &Context) -> Result<Action, Error> {
    let name = obj.name_any();
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
        .await?;
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
    use crate::crd::binding::NodeConfigurationBindingStatus;
    use crate::crd::testing::resource;
    use crate::crd::{FileRule, LineEditRule, NamedRule, Rules};
    use prometheus_client::registry::Registry;
    use wiremock::matchers::{method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    const GROUP_PATH: &str = "/apis/node-configuration-operator.sh/v1alpha1";

    fn context(server: &MockServer) -> Arc<Context> {
        let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
        let config = kube::Config::new(server.uri().parse().unwrap());
        Arc::new(Context {
            client: Client::try_from(config).unwrap(),
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
        let mut binding = new_binding(obj, node, current_spec(obj, node, revision));
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
        let items: Vec<_> = nodes
            .iter()
            .map(|n| json!({"metadata": {"name": n}}))
            .collect();
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
            created["metadata"]["ownerReferences"][0]["kind"],
            "NodeConfiguration"
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
