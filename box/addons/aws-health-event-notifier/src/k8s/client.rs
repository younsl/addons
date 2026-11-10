//! Emits Kubernetes Events for published AWS Health notifications.
//!
//! Every Event's `involvedObject` points at the notifier's own Pod (resolved
//! from the Downward API), mirroring how node-problem-detector reports against
//! the node it runs on. The client uses in-cluster config — the projected
//! `ServiceAccount` token and CA are read by `kube`, and the token rotation is
//! handled transparently.

use anyhow::Context;
use k8s_openapi::api::authorization::v1::{
    ResourceAttributes, SelfSubjectAccessReview, SelfSubjectAccessReviewSpec,
};
use k8s_openapi::api::core::v1::ObjectReference;
use kube::api::PostParams;
use kube::runtime::events::{Recorder, Reporter};
use kube::{Api, Client};

use crate::error::{AppError, AppResult};
use crate::health::HealthEvent;
use crate::k8s::event;

/// Notifier Pod identity, injected via the Downward API.
pub struct PodIdentity {
    pub name: String,
    pub namespace: String,
    /// `metadata.uid`; optional but lets `kubectl describe pod` match exactly.
    pub uid: Option<String>,
}

pub struct K8sEventClient {
    client: Client,
    namespace: String,
    recorder: Recorder,
    reference: ObjectReference,
}

impl K8sEventClient {
    /// Build the client from in-cluster config and the notifier Pod identity.
    pub async fn connect(pod: PodIdentity) -> anyhow::Result<Self> {
        let client = Client::try_default()
            .await
            .context("build in-cluster kube client (is this running inside a pod?)")?;
        Ok(Self::new(client, pod))
    }

    pub fn new(client: Client, pod: PodIdentity) -> Self {
        let reporter = Reporter {
            controller: env!("CARGO_PKG_NAME").into(),
            instance: Some(pod.name.clone()),
        };
        let recorder = Recorder::new(client.clone(), reporter);

        let reference = ObjectReference {
            api_version: Some("v1".into()),
            kind: Some("Pod".into()),
            name: Some(pod.name),
            namespace: Some(pod.namespace.clone()),
            uid: pod.uid,
            ..Default::default()
        };

        Self {
            client,
            namespace: pod.namespace,
            recorder,
            reference,
        }
    }

    /// Preflight RBAC check: may this `ServiceAccount` `create` Kubernetes
    /// Events in its namespace? Uses `SelfSubjectAccessReview`, which any authenticated
    /// identity may submit without extra permissions. `Ok(false)` means the
    /// RBAC is missing; `Err` means the check itself could not be performed.
    pub async fn can_create_events(&self) -> anyhow::Result<bool> {
        can_i(
            &self.client,
            &self.namespace,
            &Access {
                group: "events.k8s.io",
                resource: "events",
                verb: "create",
                name: None,
            },
        )
        .await
    }

    /// Publish a Kubernetes Event for the given Health event.
    pub async fn emit(
        &self,
        event: &HealthEvent,
        reminder_offset_hours: Option<u32>,
    ) -> AppResult<()> {
        let ev = event::build(event, reminder_offset_hours);
        self.recorder
            .publish(&ev, &self.reference)
            .await
            .map_err(|e| AppError::K8s(format!("publish event: {e}")))
    }

    /// Warn that a send's Slack outcome is unknown and it will not be retried.
    pub async fn emit_delivery_unknown(
        &self,
        arn: &str,
        reminder_offset_hours: Option<u32>,
    ) -> AppResult<()> {
        let ev = event::delivery_unknown(arn, reminder_offset_hours);
        self.recorder
            .publish(&ev, &self.reference)
            .await
            .map_err(|e| AppError::K8s(format!("publish event: {e}")))
    }
}

/// One permission to check. `name` matters when the Role scopes the verb
/// with `resourceNames`.
pub struct Access<'a> {
    pub group: &'a str,
    pub resource: &'a str,
    pub verb: &'a str,
    pub name: Option<&'a str>,
}

/// Preflight RBAC check through `SelfSubjectAccessReview`, which any
/// authenticated identity may submit. `Ok(false)` means the permission is
/// missing, `Err` means the check itself could not be performed.
pub async fn can_i(client: &Client, namespace: &str, access: &Access<'_>) -> anyhow::Result<bool> {
    let review = SelfSubjectAccessReview {
        spec: SelfSubjectAccessReviewSpec {
            resource_attributes: Some(ResourceAttributes {
                namespace: Some(namespace.to_string()),
                verb: Some(access.verb.to_string()),
                group: Some(access.group.to_string()),
                resource: Some(access.resource.to_string()),
                name: access.name.map(str::to_string),
                ..Default::default()
            }),
            ..Default::default()
        },
        ..Default::default()
    };
    let api: Api<SelfSubjectAccessReview> = Api::all(client.clone());
    let resp = api
        .create(&PostParams::default(), &review)
        .await
        .context("SelfSubjectAccessReview request failed")?;
    Ok(resp.status.is_some_and(|s| s.allowed))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::k8s::fake::FakeApiServer;

    fn pod() -> PodIdentity {
        PodIdentity {
            name: "ahen-0".into(),
            namespace: "ns".into(),
            uid: Some("uid".into()),
        }
    }

    #[tokio::test]
    async fn can_i_reflects_review_answer() {
        let server = FakeApiServer::start().await;
        let client = server.client();
        let access = Access {
            group: "",
            resource: "configmaps",
            verb: "update",
            name: Some("ledger"),
        };
        assert!(can_i(&client, "ns", &access).await.unwrap());
        server.deny("configmaps", "update");
        assert!(!can_i(&client, "ns", &access).await.unwrap());
        assert!(
            K8sEventClient::new(client, pod())
                .can_create_events()
                .await
                .unwrap()
        );
    }

    #[tokio::test]
    async fn can_i_fails_when_review_fails() {
        let server = FakeApiServer::start().await;
        server.fail_all();
        assert!(
            can_i(
                &server.client(),
                "ns",
                &Access {
                    group: "coordination.k8s.io",
                    resource: "leases",
                    verb: "get",
                    name: None,
                },
            )
            .await
            .is_err()
        );
    }

    #[tokio::test]
    async fn emit_delivery_unknown_posts_event() {
        let server = FakeApiServer::start().await;
        let client = K8sEventClient::new(server.client(), pod());
        client.emit_delivery_unknown("arn:1", None).await.unwrap();
        assert_eq!(server.post_count("/events/"), 1);
    }
}
