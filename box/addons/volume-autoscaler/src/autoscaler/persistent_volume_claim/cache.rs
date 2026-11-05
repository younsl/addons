//! Watch caches of the objects every reconcile reads. One watch per kind
//! replaces a list per autoscaler per pass, and each object is cut down to
//! the fields the controller reads before it is stored.

use std::fmt::Debug;
use std::hash::Hash;

use futures::StreamExt as _;
use k8s_openapi::api::apps::v1::{StatefulSet, StatefulSetSpec};
use k8s_openapi::api::core::v1::{PersistentVolumeClaim, PersistentVolumeClaimSpec};
use k8s_openapi::api::storage::v1::StorageClass;
use kube::api::ObjectMeta;
use kube::runtime::reflector::{self, Store};
use kube::runtime::{WatchStreamExt as _, watcher};
use kube::{Api, Client, Resource};
use serde::de::DeserializeOwned;
use tokio_util::sync::CancellationToken;
use tracing::warn;

use crate::autoscaler::persistent_volume_claim::api::{ARGOCD_TRACKING_ANNOTATION, tracking_app};
use crate::autoscaler::persistent_volume_claim::crd::PersistentVolumeClaimAutoscaler;

/// The cached objects, cluster-wide.
#[derive(Clone)]
pub struct Caches {
    pub claims: Store<PersistentVolumeClaim>,
    pub stateful_sets: Store<StatefulSet>,
    pub classes: Store<StorageClass>,
    pub autoscalers: Store<PersistentVolumeClaimAutoscaler>,
}

impl Caches {
    /// Starts one watch per kind. Each runs until `shutdown` fires.
    #[must_use]
    pub fn start(client: &Client, shutdown: &CancellationToken) -> Self {
        Self {
            claims: watch(Api::all(client.clone()), strip_claim, shutdown),
            stateful_sets: watch(Api::all(client.clone()), strip_stateful_set, shutdown),
            classes: watch(Api::all(client.clone()), strip_class, shutdown),
            autoscalers: watch(Api::all(client.clone()), strip_autoscaler, shutdown),
        }
    }
}

impl Caches {
    /// Resolves once every cache holds its first full list. Called by one
    /// task before the controller starts: a store wakes only one of several
    /// concurrent `wait_until_ready` callers.
    pub async fn wait_ready(&self) -> Result<(), String> {
        let err = |e: reflector::store::WriterDropped| format!("watch cache: {e}");
        self.claims.wait_until_ready().await.map_err(err)?;
        self.stateful_sets.wait_until_ready().await.map_err(err)?;
        self.classes.wait_until_ready().await.map_err(err)?;
        self.autoscalers.wait_until_ready().await.map_err(err)
    }
}

fn watch<K>(api: Api<K>, strip: fn(&mut K), shutdown: &CancellationToken) -> Store<K>
where
    K: Resource + Clone + DeserializeOwned + Debug + Send + Sync + 'static,
    K::DynamicType: Default + Eq + Hash + Clone,
{
    let (reader, writer) = reflector::store();
    let stream = watcher(api, watcher::Config::default())
        .default_backoff()
        .modify(strip)
        .reflect(writer)
        .applied_objects();
    let shutdown = shutdown.clone();
    tokio::spawn(async move {
        let mut stream = std::pin::pin!(stream);
        loop {
            tokio::select! {
                () = shutdown.cancelled() => return,
                next = stream.next() => match next {
                    Some(Ok(_)) => {}
                    Some(Err(err)) => warn!(kind = %K::kind(&K::DynamicType::default()), error = %err, "watch failed, retrying"),
                    None => return,
                },
            }
        }
    });
    reader
}

/// Keeps the identity a store needs and drops the rest of the metadata.
fn identity(meta: &ObjectMeta) -> ObjectMeta {
    ObjectMeta {
        name: meta.name.clone(),
        namespace: meta.namespace.clone(),
        uid: meta.uid.clone(),
        resource_version: meta.resource_version.clone(),
        creation_timestamp: meta.creation_timestamp.clone(),
        ..ObjectMeta::default()
    }
}

/// Keeps the class, the requested size, the resize state, and the Argo CD
/// tracking annotation.
pub fn strip_claim(pvc: &mut PersistentVolumeClaim) {
    let tracking = pvc
        .metadata
        .annotations
        .as_ref()
        .and_then(|a| a.get(ARGOCD_TRACKING_ANNOTATION))
        .and_then(|id| tracking_app(id))
        .map(str::to_string);
    pvc.metadata = identity(&pvc.metadata);
    pvc.metadata.annotations = tracking
        .map(|t| std::collections::BTreeMap::from([(ARGOCD_TRACKING_ANNOTATION.to_string(), t)]));
    pvc.spec = pvc.spec.take().map(|s| PersistentVolumeClaimSpec {
        storage_class_name: s.storage_class_name,
        resources: s
            .resources
            .map(|r| k8s_openapi::api::core::v1::VolumeResourceRequirements {
                requests: r.requests,
                ..Default::default()
            }),
        ..PersistentVolumeClaimSpec::default()
    });
    if let Some(st) = pvc.status.as_mut() {
        st.access_modes = None;
        st.allocated_resources = None;
        st.current_volume_attributes_class_name = None;
        st.modify_volume_status = None;
    }
}

/// Keeps the name and class of each claim template.
pub fn strip_stateful_set(sts: &mut StatefulSet) {
    sts.metadata = identity(&sts.metadata);
    let templates = sts
        .spec
        .take()
        .and_then(|s| s.volume_claim_templates)
        .map(|ts| {
            ts.into_iter()
                .map(|t| PersistentVolumeClaim {
                    metadata: ObjectMeta {
                        name: t.metadata.name,
                        ..ObjectMeta::default()
                    },
                    spec: t.spec.map(|s| PersistentVolumeClaimSpec {
                        storage_class_name: s.storage_class_name,
                        ..PersistentVolumeClaimSpec::default()
                    }),
                    ..PersistentVolumeClaim::default()
                })
                .collect()
        });
    sts.spec = Some(StatefulSetSpec {
        volume_claim_templates: templates,
        ..StatefulSetSpec::default()
    });
    sts.status = None;
}

/// Keeps `allowVolumeExpansion` and the default-class marker.
pub fn strip_class(sc: &mut StorageClass) {
    let mut metadata = identity(&sc.metadata);
    metadata.annotations = sc.metadata.annotations.as_ref().and_then(|a| {
        let default =
            a.get(crate::autoscaler::persistent_volume_claim::api::DEFAULT_CLASS_ANNOTATION)?;
        Some(std::collections::BTreeMap::from([(
            crate::autoscaler::persistent_volume_claim::api::DEFAULT_CLASS_ANNOTATION.to_string(),
            default.clone(),
        )]))
    });
    *sc = StorageClass {
        metadata,
        allow_volume_expansion: sc.allow_volume_expansion,
        provisioner: String::new(),
        ..StorageClass::default()
    };
}

/// Keeps the spec, which overlap detection reads.
pub fn strip_autoscaler(pvca: &mut PersistentVolumeClaimAutoscaler) {
    pvca.metadata = identity(&pvca.metadata);
    pvca.status = None;
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use k8s_openapi::api::core::v1::PersistentVolumeClaimStatus;
    use k8s_openapi::apimachinery::pkg::api::resource::Quantity;

    use super::*;
    use crate::autoscaler::persistent_volume_claim::api::tests::{pvc, sts};
    use crate::autoscaler::persistent_volume_claim::policy::tests::{base, pvca};

    fn bulky() -> ObjectMeta {
        ObjectMeta {
            name: Some("x".into()),
            namespace: Some("ns".into()),
            uid: Some("u".into()),
            annotations: Some(BTreeMap::from([(
                "kubectl/last-applied".into(),
                "{...}".into(),
            )])),
            labels: Some(BTreeMap::from([("a".into(), "b".into())])),
            managed_fields: Some(vec![
                k8s_openapi::apimachinery::pkg::apis::meta::v1::ManagedFieldsEntry::default(),
            ]),
            ..ObjectMeta::default()
        }
    }

    #[test]
    fn keeps_only_the_argocd_tracking_annotation() {
        let mut tracked = pvc("t", "1Gi", "1Gi");
        tracked.metadata.annotations = Some(BTreeMap::from([
            (
                ARGOCD_TRACKING_ANNOTATION.into(),
                "app:/PersistentVolumeClaim:ns/t".into(),
            ),
            ("kubectl/last-applied".into(), "{...}".into()),
        ]));
        strip_claim(&mut tracked);
        assert_eq!(
            tracked
                .metadata
                .annotations
                .unwrap()
                .keys()
                .collect::<Vec<_>>(),
            [ARGOCD_TRACKING_ANNOTATION],
            "only the tracking annotation survives"
        );

        let mut huge = pvc("h", "1Gi", "1Gi");
        huge.metadata.annotations = Some(BTreeMap::from([(
            ARGOCD_TRACKING_ANNOTATION.into(),
            format!("app:{}", "x".repeat(200_000)),
        )]));
        strip_claim(&mut huge);
        assert_eq!(
            huge.metadata.annotations.unwrap()[ARGOCD_TRACKING_ANNOTATION],
            "app",
            "only the application name is cached"
        );
        let mut crafted = pvc("c", "1Gi", "1Gi");
        crafted.metadata.annotations = Some(BTreeMap::from([(
            ARGOCD_TRACKING_ANNOTATION.into(),
            format!("{}:/x", "a".repeat(300)),
        )]));
        strip_claim(&mut crafted);
        assert!(
            crafted.metadata.annotations.is_none(),
            "invalid names are dropped"
        );
    }

    #[test]
    fn strips_to_what_the_controller_reads() {
        let mut c = pvc("data-web-0", "10Gi", "10Gi");
        c.metadata = bulky();
        c.spec.as_mut().unwrap().volume_name = Some("pv-1".into());
        c.status.as_mut().unwrap().allocated_resources = Some(BTreeMap::from([(
            "storage".into(),
            Quantity("10Gi".into()),
        )]));
        let before = crate::autoscaler::persistent_volume_claim::api::claim(
            &c,
            &crate::autoscaler::persistent_volume_claim::api::StorageClasses::default(),
        );
        strip_claim(&mut c);
        assert_eq!(c.metadata.name.as_deref(), Some("x"));
        assert!(c.metadata.annotations.is_none() && c.metadata.managed_fields.is_none());

        assert!(c.spec.as_ref().unwrap().volume_name.is_none());
        assert!(c.status.as_ref().unwrap().allocated_resources.is_none());
        let after = crate::autoscaler::persistent_volume_claim::api::claim(
            &c,
            &crate::autoscaler::persistent_volume_claim::api::StorageClasses::default(),
        );
        assert_eq!(
            (
                before.capacity_bytes,
                before.bound,
                before.storage_class,
                before.resizing
            ),
            (
                after.capacity_bytes,
                after.bound,
                after.storage_class,
                after.resizing
            ),
            "the policy input is unchanged"
        );

        let mut empty = PersistentVolumeClaim {
            status: Some(PersistentVolumeClaimStatus::default()),
            ..PersistentVolumeClaim::default()
        };
        strip_claim(&mut empty);
        assert!(empty.spec.is_none());

        let mut s = sts("web", &["data", "logs"]);
        s.metadata = bulky();
        strip_stateful_set(&mut s);
        let names: Vec<_> = s
            .spec
            .unwrap()
            .volume_claim_templates
            .unwrap()
            .into_iter()
            .map(|t| t.metadata.name.unwrap())
            .collect();
        assert_eq!(names, ["data", "logs"]);

        let mut sc = StorageClass {
            metadata: bulky(),
            provisioner: "ebs.csi.aws.com".into(),
            allow_volume_expansion: Some(true),
            parameters: Some(BTreeMap::from([("type".into(), "gp3".into())])),
            ..StorageClass::default()
        };
        strip_class(&mut sc);
        assert_eq!(sc.allow_volume_expansion, Some(true));
        assert!(sc.parameters.is_none() && sc.metadata.labels.is_none());
        assert!(
            sc.metadata.annotations.is_none(),
            "only the default marker is kept"
        );
        let mut default = StorageClass {
            metadata: ObjectMeta {
                annotations: Some(BTreeMap::from([
                    (
                        crate::autoscaler::persistent_volume_claim::api::DEFAULT_CLASS_ANNOTATION
                            .into(),
                        "true".into(),
                    ),
                    ("other".into(), "x".into()),
                ])),
                ..bulky()
            },
            ..StorageClass::default()
        };
        strip_class(&mut default);
        assert_eq!(default.metadata.annotations.unwrap().len(), 1);

        let mut p = pvca(base());
        p.metadata = bulky();
        p.status = Some(
            crate::autoscaler::persistent_volume_claim::crd::PersistentVolumeClaimAutoscalerStatus::default(),
        );
        strip_autoscaler(&mut p);
        assert!(p.status.is_none() && p.metadata.annotations.is_none());
        assert_eq!(p.spec.max_capacity.0, "100Gi");
    }
}
