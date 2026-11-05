//! Reads claims, `StatefulSets`, and `StorageClasses`, patches claim sizes,
//! and writes autoscaler status. The conversions from Kubernetes objects into
//! the policy inputs live here too, so the reconciler only sees `Claim`s.

use std::collections::{BTreeMap, BTreeSet};
use std::hash::Hash;
use std::sync::Arc;

use async_trait::async_trait;
use k8s_openapi::api::apps::v1::StatefulSet;
use k8s_openapi::api::core::v1::PersistentVolumeClaim;
use k8s_openapi::apimachinery::pkg::api::resource::Quantity;
use kube::api::{Patch, PatchParams};
use kube::runtime::reflector::Store;
use kube::{Api, Client, Resource};

use crate::autoscaler::persistent_volume_claim::cache::Caches;
use crate::autoscaler::persistent_volume_claim::crd::{
    ClaimTargetKind, ClaimTargetRef, PersistentVolumeClaimAutoscaler,
    PersistentVolumeClaimAutoscalerStatus,
};
use crate::autoscaler::persistent_volume_claim::policy::Claim;
use crate::autoscaler::{FIELD_MANAGER, quantity_bytes};

/// What the reconciler needs from the Kubernetes API. Reads return shared
/// handles to cached objects rather than copies.
#[async_trait]
pub trait ClaimApi: Send + Sync {
    /// Resolves once every read is served from a complete cache.
    async fn ready(&self) -> Result<(), String> {
        Ok(())
    }
    async fn list_autoscalers(
        &self,
        namespace: &str,
    ) -> Result<Vec<Arc<PersistentVolumeClaimAutoscaler>>, String>;
    async fn list_claims(&self, namespace: &str)
    -> Result<Vec<Arc<PersistentVolumeClaim>>, String>;
    async fn list_stateful_sets(&self, namespace: &str) -> Result<Vec<Arc<StatefulSet>>, String>;
    async fn storage_classes(&self) -> Result<StorageClasses, String>;
    async fn resize_claim(
        &self,
        namespace: &str,
        name: &str,
        storage: &Quantity,
    ) -> Result<(), String>;
    async fn patch_status(
        &self,
        namespace: &str,
        name: &str,
        status: &PersistentVolumeClaimAutoscalerStatus,
    ) -> Result<(), String>;
}

/// Argo CD's annotation-based resource tracking, `<app>:<group>/<kind>:<ns>/<name>`.
pub const ARGOCD_TRACKING_ANNOTATION: &str = "argocd.argoproj.io/tracking-id";

/// The Application name in a tracking annotation value, when it is a valid
/// DNS-1123 subdomain. Anything else is ignored, so an oversized or crafted
/// annotation never reaches the cache, status, or Events.
#[must_use]
pub fn tracking_app(id: &str) -> Option<&str> {
    let app = id.split(':').next()?;
    let valid = !app.is_empty()
        && app.len() <= 253
        && app
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'.')
        && app
            .bytes()
            .next()
            .is_some_and(|b| b.is_ascii_alphanumeric())
        && app
            .bytes()
            .last()
            .is_some_and(|b| b.is_ascii_alphanumeric());
    valid.then_some(app)
}

/// The Argo CD Application tracking `pvc`, from its tracking annotation.
fn tracked_by(pvc: &PersistentVolumeClaim) -> Option<String> {
    let id = pvc
        .metadata
        .annotations
        .as_ref()?
        .get(ARGOCD_TRACKING_ANNOTATION)?;
    tracking_app(id).map(str::to_string)
}

/// Marks the class a claim without `storageClassName` gets.
pub const DEFAULT_CLASS_ANNOTATION: &str = "storageclass.kubernetes.io/is-default-class";

/// Every `StorageClass` by name with its `allowVolumeExpansion`, and the
/// default class.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StorageClasses {
    pub expandable: BTreeMap<String, bool>,
    pub default: Option<String>,
}

/// Why `target` cannot be grown, one line per `StorageClass`: a claim or a
/// `volumeClaimTemplate` whose class is missing or does not set
/// `allowVolumeExpansion`. Empty when every class allows expansion. Checking
/// the templates catches a class before the first claim exists.
#[must_use]
pub fn expansion_blockers(
    target: &ClaimTargetRef,
    stateful_sets: &[Arc<StatefulSet>],
    claims: &[Claim],
    classes: &StorageClasses,
) -> Vec<String> {
    let mut by_class: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for c in claims.iter().filter(|c| c.expandable != Some(true)) {
        by_class
            .entry(c.storage_class.clone())
            .or_default()
            .push(c.name.clone());
    }
    if target.kind == ClaimTargetKind::StatefulSet
        && let Some(sts) = stateful_sets
            .iter()
            .find(|s| s.metadata.name.as_deref() == Some(target.name.as_str()))
    {
        for t in sts
            .spec
            .iter()
            .flat_map(|s| s.volume_claim_templates.iter().flatten())
        {
            let class = t
                .spec
                .as_ref()
                .and_then(|s| s.storage_class_name.clone())
                .or_else(|| classes.default.clone())
                .unwrap_or_default();
            if classes.expandable.get(&class) != Some(&true) {
                by_class.entry(class).or_default().push(format!(
                    "volumeClaimTemplate {}",
                    t.metadata.name.as_deref().unwrap_or_default()
                ));
            }
        }
    }
    by_class
        .into_iter()
        .map(|(class, subjects)| {
            let why = if class.is_empty() {
                "no StorageClass is named and the cluster has no default".to_string()
            } else if classes.expandable.contains_key(&class) {
                format!("StorageClass {class} does not set allowVolumeExpansion")
            } else {
                format!("StorageClass {class} not found")
            };
            format!("{why} ({})", subjects.join(", "))
        })
        .collect()
}

/// The kube-backed implementation: reads come from the watch caches, writes
/// go to the API server.
pub struct KubeClaims {
    client: Client,
    caches: Caches,
}

impl KubeClaims {
    #[must_use]
    pub const fn new(client: Client, caches: Caches) -> Self {
        Self { client, caches }
    }
}

/// The objects of `store` in `namespace`.
#[allow(clippy::unused_async)]
async fn in_namespace<K>(store: &Store<K>, namespace: &str) -> Result<Vec<Arc<K>>, String>
where
    K: Resource + Clone + Send + Sync + 'static,
    K::DynamicType: Eq + Hash + Clone + Send + Sync,
{
    Ok(store
        .state()
        .into_iter()
        .filter(|o| o.meta().namespace.as_deref() == Some(namespace))
        .collect())
}

#[async_trait]
impl ClaimApi for KubeClaims {
    async fn ready(&self) -> Result<(), String> {
        self.caches.wait_ready().await
    }

    async fn list_autoscalers(
        &self,
        namespace: &str,
    ) -> Result<Vec<Arc<PersistentVolumeClaimAutoscaler>>, String> {
        in_namespace(&self.caches.autoscalers, namespace).await
    }

    async fn list_claims(
        &self,
        namespace: &str,
    ) -> Result<Vec<Arc<PersistentVolumeClaim>>, String> {
        in_namespace(&self.caches.claims, namespace).await
    }

    async fn list_stateful_sets(&self, namespace: &str) -> Result<Vec<Arc<StatefulSet>>, String> {
        in_namespace(&self.caches.stateful_sets, namespace).await
    }

    async fn storage_classes(&self) -> Result<StorageClasses, String> {
        let mut out = StorageClasses::default();
        for sc in self.caches.classes.state() {
            let Some(name) = sc.metadata.name.clone() else {
                continue;
            };
            if sc
                .metadata
                .annotations
                .as_ref()
                .and_then(|a| a.get(DEFAULT_CLASS_ANNOTATION))
                .is_some_and(|v| v == "true")
            {
                out.default = Some(name.clone());
            }
            out.expandable
                .insert(name, sc.allow_volume_expansion.unwrap_or(false));
        }
        Ok(out)
    }

    async fn resize_claim(
        &self,
        namespace: &str,
        name: &str,
        storage: &Quantity,
    ) -> Result<(), String> {
        let api: Api<PersistentVolumeClaim> = Api::namespaced(self.client.clone(), namespace);
        let patch = serde_json::json!({
            "spec": { "resources": { "requests": { "storage": storage } } }
        });
        api.patch(name, &PatchParams::default(), &Patch::Merge(&patch))
            .await
            .map(|_| ())
            .map_err(|e| e.to_string())
    }

    async fn patch_status(
        &self,
        namespace: &str,
        name: &str,
        status: &PersistentVolumeClaimAutoscalerStatus,
    ) -> Result<(), String> {
        let api: Api<PersistentVolumeClaimAutoscaler> =
            Api::namespaced(self.client.clone(), namespace);
        let patch = serde_json::json!({ "status": status });
        api.patch_status(
            name,
            &PatchParams::apply(FIELD_MANAGER),
            &Patch::Merge(&patch),
        )
        .await
        .map(|_| ())
        .map_err(|e| e.to_string())
    }
}

/// The claim names a `StatefulSet` owns: `<template>-<statefulset>-<ordinal>`
/// for each of its `volumeClaimTemplates`, among `claims`.
fn stateful_set_claims(sts: &StatefulSet, claims: &[String]) -> BTreeSet<String> {
    let name = sts.metadata.name.as_deref().unwrap_or_default();
    let prefixes: Vec<String> = sts
        .spec
        .as_ref()
        .and_then(|s| s.volume_claim_templates.as_ref())
        .into_iter()
        .flatten()
        .filter_map(|t| t.metadata.name.as_deref())
        .map(|t| format!("{t}-{name}-"))
        .collect();
    claims
        .iter()
        .filter(|c| {
            prefixes.iter().any(|p| {
                c.strip_prefix(p.as_str())
                    .is_some_and(|ord| !ord.is_empty() && ord.bytes().all(|b| b.is_ascii_digit()))
            })
        })
        .cloned()
        .collect()
}

/// The claims `target` names, or why the target does not exist. A
/// `StatefulSet` that has not created a claim yet names none.
pub fn resolve(
    target: &ClaimTargetRef,
    claims: &[String],
    stateful_sets: &[Arc<StatefulSet>],
) -> Result<BTreeSet<String>, String> {
    match target.kind {
        ClaimTargetKind::PersistentVolumeClaim => {
            if claims.contains(&target.name) {
                Ok(BTreeSet::from([target.name.clone()]))
            } else {
                Err(format!("PersistentVolumeClaim {} not found", target.name))
            }
        }
        ClaimTargetKind::StatefulSet => {
            let sts = stateful_sets
                .iter()
                .find(|s| s.metadata.name.as_deref() == Some(target.name.as_str()))
                .ok_or_else(|| format!("StatefulSet {} not found", target.name))?;
            Ok(stateful_set_claims(sts, claims))
        }
    }
}

/// Resize states the external resizer and kubelet report on a claim.
const RESIZE_CONDITIONS: [&str; 4] = [
    "Resizing",
    "FileSystemResizePending",
    "ControllerResizeError",
    "NodeResizeError",
];

fn storage_of(map: Option<&BTreeMap<String, Quantity>>) -> Option<f64> {
    map.and_then(|m| m.get("storage"))
        .and_then(|q| quantity_bytes(q).ok())
}

/// Why a previous resize of `pvc` has not finished, if it has not.
fn resizing(pvc: &PersistentVolumeClaim) -> Option<String> {
    let status = pvc.status.as_ref()?;
    if let Some(c) = status
        .conditions
        .iter()
        .flatten()
        .find(|c| c.status == "True" && RESIZE_CONDITIONS.contains(&c.type_.as_str()))
    {
        return Some(match c.message.as_deref() {
            Some(m) if !m.is_empty() => format!("{}: {m}", c.type_),
            _ => c.type_.clone(),
        });
    }
    if let Some(s) = status
        .allocated_resource_statuses
        .as_ref()
        .and_then(|m| m.get("storage"))
    {
        return Some(format!("allocatedResourceStatuses.storage is {s}"));
    }
    let requested = storage_of(pvc.spec.as_ref()?.resources.as_ref()?.requests.as_ref())?;
    let capacity = storage_of(status.capacity.as_ref())?;
    (requested > capacity)
        .then(|| format!("requested {requested:.0} bytes, capacity {capacity:.0} bytes"))
}

/// The policy input for `pvc`.
#[must_use]
pub fn claim(pvc: &PersistentVolumeClaim, classes: &StorageClasses) -> Claim {
    let storage_class = pvc
        .spec
        .as_ref()
        .and_then(|s| s.storage_class_name.clone())
        .unwrap_or_default();
    Claim {
        name: pvc.metadata.name.clone().unwrap_or_default(),
        uid: pvc.metadata.uid.clone().unwrap_or_default(),
        bound: pvc.status.as_ref().and_then(|s| s.phase.as_deref()) == Some("Bound"),
        expandable: classes.expandable.get(&storage_class).copied(),
        storage_class,
        capacity_bytes: storage_of(pvc.status.as_ref().and_then(|s| s.capacity.as_ref())),
        resizing: resizing(pvc),
        tracked_by: tracked_by(pvc),
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use std::sync::Mutex;

    use k8s_openapi::api::apps::v1::StatefulSetSpec;
    use k8s_openapi::api::core::v1::{
        PersistentVolumeClaimCondition, PersistentVolumeClaimSpec, PersistentVolumeClaimStatus,
        VolumeResourceRequirements,
    };
    use kube::api::ObjectMeta;

    use super::*;

    pub(crate) fn sts(name: &str, templates: &[&str]) -> StatefulSet {
        StatefulSet {
            metadata: ObjectMeta {
                name: Some(name.into()),
                ..ObjectMeta::default()
            },
            spec: Some(StatefulSetSpec {
                volume_claim_templates: Some(
                    templates
                        .iter()
                        .map(|t| PersistentVolumeClaim {
                            metadata: ObjectMeta {
                                name: Some((*t).into()),
                                ..ObjectMeta::default()
                            },
                            spec: Some(PersistentVolumeClaimSpec {
                                storage_class_name: Some("gp3".into()),
                                ..PersistentVolumeClaimSpec::default()
                            }),
                            ..PersistentVolumeClaim::default()
                        })
                        .collect(),
                ),
                ..StatefulSetSpec::default()
            }),
            status: None,
        }
    }

    pub(crate) fn pvc(name: &str, requested: &str, capacity: &str) -> PersistentVolumeClaim {
        PersistentVolumeClaim {
            metadata: ObjectMeta {
                name: Some(name.into()),
                uid: Some(format!("uid-{name}")),
                ..ObjectMeta::default()
            },
            spec: Some(PersistentVolumeClaimSpec {
                storage_class_name: Some("gp3".into()),
                resources: Some(VolumeResourceRequirements {
                    requests: Some(BTreeMap::from([(
                        "storage".into(),
                        Quantity(requested.into()),
                    )])),
                    ..VolumeResourceRequirements::default()
                }),
                ..PersistentVolumeClaimSpec::default()
            }),
            status: Some(PersistentVolumeClaimStatus {
                phase: Some("Bound".into()),
                capacity: Some(BTreeMap::from([(
                    "storage".into(),
                    Quantity(capacity.into()),
                )])),
                ..PersistentVolumeClaimStatus::default()
            }),
        }
    }

    /// An in-memory cluster for reconciler tests.
    #[derive(Default)]
    pub(crate) struct Fake {
        pub(crate) autoscalers: Vec<PersistentVolumeClaimAutoscaler>,
        pub(crate) claims: Vec<PersistentVolumeClaim>,
        pub(crate) stateful_sets: Vec<StatefulSet>,
        pub(crate) classes: BTreeMap<String, bool>,
        pub(crate) default_class: Option<String>,
        pub(crate) fail_list: bool,
        pub(crate) fail_resize: bool,
        pub(crate) resized: Mutex<Vec<(String, String)>>,
        pub(crate) statuses: Mutex<Vec<PersistentVolumeClaimAutoscalerStatus>>,
    }

    #[async_trait]
    impl ClaimApi for Fake {
        async fn list_autoscalers(
            &self,
            _: &str,
        ) -> Result<Vec<Arc<PersistentVolumeClaimAutoscaler>>, String> {
            Ok(self.autoscalers.iter().cloned().map(Arc::new).collect())
        }
        async fn list_claims(&self, _: &str) -> Result<Vec<Arc<PersistentVolumeClaim>>, String> {
            if self.fail_list {
                return Err("connection refused".into());
            }
            Ok(self.claims.iter().cloned().map(Arc::new).collect())
        }
        async fn list_stateful_sets(&self, _: &str) -> Result<Vec<Arc<StatefulSet>>, String> {
            Ok(self.stateful_sets.iter().cloned().map(Arc::new).collect())
        }
        async fn storage_classes(&self) -> Result<StorageClasses, String> {
            Ok(StorageClasses {
                expandable: self.classes.clone(),
                default: self.default_class.clone(),
            })
        }
        async fn resize_claim(&self, _: &str, name: &str, q: &Quantity) -> Result<(), String> {
            if self.fail_resize {
                return Err("forbidden".into());
            }
            self.resized
                .lock()
                .unwrap()
                .push((name.into(), q.0.clone()));
            Ok(())
        }
        async fn patch_status(
            &self,
            _: &str,
            _: &str,
            status: &PersistentVolumeClaimAutoscalerStatus,
        ) -> Result<(), String> {
            self.statuses.lock().unwrap().push(status.clone());
            Ok(())
        }
    }

    fn names(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| (*s).to_string()).collect()
    }

    #[test]
    fn resolves_stateful_set_and_single_claims() {
        let claims = names(&[
            "data-web-0",
            "data-web-1",
            "logs-web-0",
            "data-web-x",
            "data-web-0-1",
            "data-api-0",
        ]);
        let sets = [
            Arc::new(sts("web", &["data", "logs"])),
            Arc::new(sts("empty", &[])),
        ];
        let r = |kind, name: &str| {
            resolve(
                &ClaimTargetRef {
                    kind,
                    name: name.into(),
                },
                &claims,
                &sets,
            )
        };
        assert_eq!(
            r(ClaimTargetKind::StatefulSet, "web").unwrap(),
            BTreeSet::from([
                "data-web-0".into(),
                "data-web-1".into(),
                "logs-web-0".into()
            ])
        );
        assert!(
            r(ClaimTargetKind::StatefulSet, "gone")
                .unwrap_err()
                .contains("not found")
        );
        assert!(
            r(ClaimTargetKind::StatefulSet, "empty").unwrap().is_empty(),
            "a StatefulSet without claims yet is not an error"
        );
        assert_eq!(
            r(ClaimTargetKind::PersistentVolumeClaim, "data-api-0").unwrap(),
            BTreeSet::from(["data-api-0".into()])
        );
        assert!(r(ClaimTargetKind::PersistentVolumeClaim, "nope").is_err());
    }

    fn with_template_class(mut s: StatefulSet, class: Option<&str>) -> StatefulSet {
        for t in s
            .spec
            .as_mut()
            .unwrap()
            .volume_claim_templates
            .as_mut()
            .unwrap()
        {
            t.spec = Some(PersistentVolumeClaimSpec {
                storage_class_name: class.map(str::to_string),
                ..PersistentVolumeClaimSpec::default()
            });
        }
        s
    }

    #[test]
    fn expansion_blockers_cover_claims_and_templates() {
        let classes = StorageClasses {
            expandable: BTreeMap::from([("gp3".to_string(), true), ("gp2".to_string(), false)]),
            default: Some("gp3".into()),
        };
        let target = ClaimTargetRef {
            kind: ClaimTargetKind::StatefulSet,
            name: "web".into(),
        };
        let ok = claim(&pvc("data-web-0", "1Gi", "1Gi"), &classes);
        let mut old = claim(&pvc("data-web-1", "1Gi", "1Gi"), &classes);
        old.storage_class = "gp2".into();
        old.expandable = Some(false);
        let mut gone = ok.clone();
        gone.name = "data-web-2".into();
        gone.storage_class = "io2".into();
        gone.expandable = None;

        let sets = [Arc::new(with_template_class(sts("web", &["data"]), None))];
        assert!(
            expansion_blockers(&target, &sets, std::slice::from_ref(&ok), &classes).is_empty(),
            "template falls back to the expandable default"
        );

        let got = expansion_blockers(&target, &sets, &[ok.clone(), old, gone], &classes);
        assert_eq!(
            got,
            [
                "StorageClass gp2 does not set allowVolumeExpansion (data-web-1)",
                "StorageClass io2 not found (data-web-2)",
            ]
        );

        let sets = [Arc::new(with_template_class(
            sts("web", &["data"]),
            Some("gp2"),
        ))];
        assert_eq!(
            expansion_blockers(&target, &sets, &[], &classes),
            ["StorageClass gp2 does not set allowVolumeExpansion (volumeClaimTemplate data)"],
            "caught before any claim exists"
        );
        let no_default = StorageClasses {
            default: None,
            ..classes.clone()
        };
        let sets = [Arc::new(with_template_class(sts("web", &["data"]), None))];
        assert_eq!(
            expansion_blockers(&target, &sets, &[], &no_default),
            ["no StorageClass is named and the cluster has no default (volumeClaimTemplate data)"]
        );
        let single = ClaimTargetRef {
            kind: ClaimTargetKind::PersistentVolumeClaim,
            name: "data-web-0".into(),
        };
        let sets = [Arc::new(with_template_class(
            sts("data-web-0", &["x"]),
            Some("gp2"),
        ))];
        assert!(
            expansion_blockers(&single, &sets, &[ok], &classes).is_empty(),
            "templates only matter for a StatefulSet target"
        );
    }

    #[test]
    fn converts_claims() {
        let classes = StorageClasses {
            expandable: BTreeMap::from([("gp3".to_string(), true), ("gp2".to_string(), false)]),
            default: None,
        };
        let c = claim(&pvc("a", "10Gi", "10Gi"), &classes);
        assert!(c.bound);
        assert_eq!(c.expandable, Some(true));
        assert_eq!(c.capacity_bytes, Some(10.0 * crate::autoscaler::GIB));
        assert_eq!(c.resizing, None);

        let c = claim(&pvc("a", "20Gi", "10Gi"), &classes);
        assert!(c.resizing.unwrap().starts_with("requested"));

        let mut p = pvc("a", "10Gi", "10Gi");
        p.spec.as_mut().unwrap().storage_class_name = Some("gp2".into());
        p.status.as_mut().unwrap().conditions = Some(vec![PersistentVolumeClaimCondition {
            type_: "FileSystemResizePending".into(),
            status: "True".into(),
            message: Some("waiting for a Pod".into()),
            ..PersistentVolumeClaimCondition::default()
        }]);
        let c = claim(&p, &classes);
        assert_eq!(c.expandable, Some(false));
        assert_eq!(
            c.resizing.as_deref(),
            Some("FileSystemResizePending: waiting for a Pod")
        );

        let mut p = pvc("a", "10Gi", "10Gi");
        p.status.as_mut().unwrap().allocated_resource_statuses = Some(BTreeMap::from([(
            "storage".into(),
            "ControllerResizeInProgress".into(),
        )]));
        p.spec.as_mut().unwrap().storage_class_name = None;
        let c = claim(&p, &classes);
        assert!(c.resizing.unwrap().contains("ControllerResizeInProgress"));
        assert_eq!((c.storage_class.as_str(), c.expandable), ("", None));

        let mut p = pvc("a", "10Gi", "10Gi");
        assert_eq!(claim(&p, &classes).tracked_by, None);
        p.metadata.annotations = Some(BTreeMap::from([(
            ARGOCD_TRACKING_ANNOTATION.to_string(),
            "shared-forklift:/PersistentVolumeClaim:forklift/a".to_string(),
        )]));
        assert_eq!(
            claim(&p, &classes).tracked_by.as_deref(),
            Some("shared-forklift")
        );
        p.metadata.annotations = Some(BTreeMap::from([(
            ARGOCD_TRACKING_ANNOTATION.to_string(),
            ":/PersistentVolumeClaim:forklift/a".to_string(),
        )]));
        assert_eq!(claim(&p, &classes).tracked_by, None, "no app name");
        for bad in [
            "Shared-Forklift:/x".to_string(),
            "-app:/x".to_string(),
            "app_name:/x".to_string(),
            format!("{}:/x", "a".repeat(254)),
        ] {
            assert_eq!(tracking_app(&bad), None, "{bad:.40}");
        }
        assert_eq!(
            tracking_app(&format!("{}:/x", "a".repeat(253))).map(str::len),
            Some(253)
        );
        assert_eq!(tracking_app("shared.forklift-1"), Some("shared.forklift-1"));

        let mut p = pvc("a", "10Gi", "10Gi");
        p.status.as_mut().unwrap().phase = Some("Pending".into());
        p.status.as_mut().unwrap().capacity = None;
        let c = claim(&p, &classes);
        assert!(!c.bound);
        assert_eq!(c.capacity_bytes, None);
    }
}
