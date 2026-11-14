//! Removes what a deleted `NodeConfiguration` leaves behind. A binding is
//! owned by its node, so nothing collects it when only the resource goes.

use std::collections::BTreeSet;
use std::pin::pin;
use std::sync::Arc;
use std::time::Duration;

use futures::StreamExt as _;
use k8s_openapi::api::core::v1::Node;
use kube::api::{Api, ListParams};
use kube::core::PartialObjectMeta;
use kube::runtime::WatchStreamExt as _;
use kube::runtime::watcher::{self, Event, watcher};
use kube::{Client, ResourceExt as _};
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

use super::{Context, cordon, remove};
use node_configuration_common::crd::NodeConfiguration;
use node_configuration_common::crd::binding::NodeConfigurationBinding;

const RETRY: Duration = Duration::from_secs(30);

/// Deletes the bindings and gives back the cordons of every resource that no
/// longer exists.
pub async fn sweep(ctx: &Context) -> Result<(), kube::Error> {
    let client: &Client = &ctx.client;
    let bindings = Api::<NodeConfigurationBinding>::all(client.clone())
        .list(&ListParams::default())
        .await?;
    let nodes = Api::<Node>::all(client.clone())
        .list_metadata(&ListParams::default())
        .await?;
    // Listed last, so a resource created meanwhile is never taken for deleted.
    let alive: BTreeSet<String> = Api::<NodeConfiguration>::all(client.clone())
        .list_metadata(&ListParams::default())
        .await?
        .into_iter()
        .map(|configuration| configuration.name_any())
        .collect();

    for node in nodes {
        for holder in cordon::holders(node.metadata.annotations.as_ref()) {
            if !alive.contains(&holder) {
                cordon::free(ctx, &node.name_any(), &holder, cordon::Why::Deleted).await?;
            }
        }
    }
    let api: Api<NodeConfigurationBinding> = Api::all(client.clone());
    for binding in bindings {
        let configuration = &binding.spec.configuration;
        if !alive.contains(configuration) {
            remove(&api, &binding.name_any()).await?;
            info!(
                kind = "NodeConfigurationBinding",
                name = configuration,
                node = binding.spec.node_name,
                binding = binding.name_any(),
                "Deleted the binding of a deleted NodeConfiguration"
            );
        }
    }
    Ok(())
}

/// Sweeps at the start, after every deletion, and whenever the watch starts
/// over, which covers a deletion made while no controller ran.
pub async fn run(ctx: Arc<Context>, shutdown: CancellationToken) {
    let api: Api<PartialObjectMeta<NodeConfiguration>> = Api::all(ctx.client.clone());
    let mut events = pin!(watcher(api, watcher::Config::default()).default_backoff());
    let mut retry = false;
    loop {
        let due = tokio::select! {
            () = shutdown.cancelled() => return,
            () = tokio::time::sleep(RETRY), if retry => true,
            event = events.next() => match event {
                Some(Ok(Event::InitDone | Event::Delete(_))) => true,
                Some(Ok(_)) => false,
                Some(Err(e)) => {
                    warn!("Watch of NodeConfigurations failed, {e}");
                    false
                }
                None => return,
            },
        };
        if due {
            retry = sweep(&ctx)
                .await
                .inspect_err(|e| warn!("Cleanup after a deleted NodeConfiguration failed, {e}"))
                .is_err();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::metrics::Metrics;
    use kube::runtime::events::Recorder;
    use node_configuration_common::crd::binding::NodeConfigurationBindingSpec;
    use node_configuration_common::crd::testing::resource;
    use node_configuration_common::crd::{GROUP, Rules};
    use prometheus_client::registry::Registry;
    use serde_json::{Value, json};
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn context(server: &MockServer) -> Context {
        let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
        let config = kube::Config::new(server.uri().parse().unwrap());
        let client = Client::try_from(config).unwrap();
        Context {
            recorder: Recorder::new(client.clone(), "nco".to_string().into()),
            client,
            metrics: Arc::new(Metrics::new(&mut Registry::default())),
        }
    }

    async fn mock(server: &MockServer, verb: &str, at: String, body: Value) {
        Mock::given(method(verb))
            .and(path(at))
            .respond_with(ResponseTemplate::new(200).set_body_json(body))
            .mount(server)
            .await;
    }

    fn binding(configuration: &str) -> NodeConfigurationBinding {
        NodeConfigurationBinding::new(
            &format!("{configuration}.a"),
            NodeConfigurationBindingSpec {
                configuration: configuration.to_string(),
                node_name: "a".to_string(),
                revision: "r1".to_string(),
                group: None,
                priority: 0,
                dry_run: false,
                interval_seconds: 300,
                rules: Rules::default(),
            },
        )
    }

    #[tokio::test]
    async fn bindings_and_cordons_of_a_deleted_resource_are_removed() {
        let server = MockServer::start().await;
        let group = format!("/apis/{GROUP}/v1alpha1");
        let node = json!({
            "apiVersion": "v1",
            "kind": "Node",
            "metadata": {
                "name": "a",
                "uid": "uid-a",
                "resourceVersion": "5",
                "annotations": {cordon::HOLDERS_ANNOTATION: "gone,mng"}
            },
            "spec": {"unschedulable": true}
        });
        mock(
            &server,
            "GET",
            format!("{group}/nodeconfigurationbindings"),
            json!({
                "apiVersion": format!("{GROUP}/v1alpha1"),
                "kind": "NodeConfigurationBindingList",
                "metadata": {},
                "items": [binding("mng"), binding("gone")]
            }),
        )
        .await;
        mock(
            &server,
            "GET",
            "/api/v1/nodes".to_string(),
            json!({"apiVersion": "v1", "kind": "PartialObjectMetadataList", "metadata": {}, "items": [node]}),
        )
        .await;
        mock(
            &server,
            "GET",
            format!("{group}/nodeconfigurations"),
            json!({
                "apiVersion": "v1",
                "kind": "PartialObjectMetadataList",
                "metadata": {},
                "items": [{"metadata": resource(Rules::default()).metadata}]
            }),
        )
        .await;
        mock(&server, "GET", "/api/v1/nodes/a".to_string(), node.clone()).await;
        mock(&server, "PATCH", "/api/v1/nodes/a".to_string(), node).await;
        mock(
            &server,
            "DELETE",
            format!("{group}/nodeconfigurationbindings/gone.a"),
            json!({"apiVersion": "v1", "kind": "Status", "status": "Success"}),
        )
        .await;

        sweep(&context(&server)).await.unwrap();
        let sent: Vec<(String, String, Value)> = server
            .received_requests()
            .await
            .unwrap()
            .iter()
            .filter(|r| r.method.as_str() != "GET")
            .map(|r| {
                (
                    r.method.to_string(),
                    r.url.path().to_string(),
                    serde_json::from_slice(&r.body).unwrap_or_default(),
                )
            })
            .collect();
        assert_eq!(sent.len(), 2, "the binding of the live resource stays");
        assert_eq!(sent[0].1, "/api/v1/nodes/a");
        assert_eq!(
            sent[0].2["metadata"]["annotations"][cordon::HOLDERS_ANNOTATION],
            "mng",
            "the live resource keeps its cordon"
        );
        assert_eq!(sent[0].2.get("spec"), None);
        assert_eq!(sent[1].0, "DELETE");
        assert!(sent[1].1.ends_with("/nodeconfigurationbindings/gone.a"));
    }

    #[tokio::test]
    async fn a_failed_listing_is_reported() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;
        assert!(sweep(&context(&server)).await.is_err());
    }
}
