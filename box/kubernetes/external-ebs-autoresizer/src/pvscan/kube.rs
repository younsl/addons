//! Reads the cluster objects the scanner classifies and writes its verdict
//! back as annotations. Like the Node client, it is deliberately limited to
//! list and annotate: nothing here can delete a claim or a volume.

use std::collections::{BTreeMap, HashSet};

use async_trait::async_trait;
use k8s_openapi::api::apps::v1::StatefulSet as K8sStatefulSet;
use k8s_openapi::api::core::v1::{PersistentVolume, PersistentVolumeClaim, Pod};
use k8s_openapi::apimachinery::pkg::api::resource::Quantity;
use kube::api::{ListParams, Patch, PatchParams};

use super::KubeApi;
use super::types::{Inventory, Pv, Pvc, StatefulSet};
use crate::k8s::annotation_patch;
use crate::k8s::nodes::PAGE_SIZE;

/// The CSI driver name of the AWS EBS CSI driver. A volume provisioned by it
/// carries the EBS volume ID verbatim in `spec.csi.volumeHandle`.
const EBS_CSI_DRIVER: &str = "ebs.csi.aws.com";

/// The page size of the Pod sweep. Smaller than the general page size because
/// a Pod is the widest object the scanner reads, and the memory limit in the
/// chart is sized for the addon's steady state, not for a 500-Pod page.
const POD_PAGE_SIZE: u32 = 100;

/// The kube-backed scanner client.
pub struct KubeClient {
    client: kube::Client,
}

impl KubeClient {
    #[must_use]
    pub const fn new(client: kube::Client) -> Self {
        Self { client }
    }

    /// Lists every object of one kind, handing each page to `reduce` and
    /// dropping it before the next is fetched. The full object list is never
    /// held: a Pod carries a large spec and status, and a cluster of a few
    /// hundred Pods materialized at once is enough to blow through the
    /// container's memory limit, while the scanner only keeps a claim name
    /// per Pod.
    async fn for_each_page<K, F>(
        &self,
        api: &kube::Api<K>,
        what: &str,
        page_size: u32,
        mut reduce: F,
    ) -> Result<(), String>
    where
        K: kube::Resource + Clone + serde::de::DeserializeOwned + std::fmt::Debug,
        <K as kube::Resource>::DynamicType: Default,
        F: FnMut(&K),
    {
        let mut params = ListParams::default().limit(page_size);
        loop {
            let page = api
                .list(&params)
                .await
                .map_err(|e| format!("list {what}: {e}"))?;
            for item in &page.items {
                reduce(item);
            }
            match page.metadata.continue_ {
                Some(token) if !token.is_empty() => params = params.continue_token(&token),
                _ => return Ok(()),
            }
        }
    }
}

#[async_trait]
impl KubeApi for KubeClient {
    /// Reads the whole cluster's claims, volumes, Pod claim references, and
    /// `StatefulSets` in four paginated list sweeps.
    async fn inventory(&self) -> Result<Inventory, String> {
        let mut inv = Inventory::default();
        self.for_each_page(
            &kube::Api::<PersistentVolumeClaim>::all(self.client.clone()),
            "persistentvolumeclaims",
            PAGE_SIZE,
            |p| inv.pvcs.push(from_pvc(p)),
        )
        .await?;
        self.for_each_page(
            &kube::Api::<PersistentVolume>::all(self.client.clone()),
            "persistentvolumes",
            PAGE_SIZE,
            |p| inv.pvs.push(from_pv(p)),
        )
        .await?;
        // Pods are by far the widest objects the scanner reads and the only
        // thing kept per Pod is a claim name, so they are paged small.
        self.for_each_page(
            &kube::Api::<Pod>::all(self.client.clone()),
            "pods",
            POD_PAGE_SIZE,
            |p| claims_in_use(std::slice::from_ref(p), &mut inv.claims_in_use),
        )
        .await?;
        self.for_each_page(
            &kube::Api::<K8sStatefulSet>::all(self.client.clone()),
            "statefulsets",
            PAGE_SIZE,
            |s| inv.stateful_sets.push(from_stateful_set(s)),
        )
        .await?;
        Ok(inv)
    }

    async fn annotate_pvc(
        &self,
        namespace: &str,
        name: &str,
        set: &BTreeMap<String, String>,
        remove: &[String],
    ) -> Result<(), String> {
        let Some(patch) = annotation_patch(set, remove) else {
            return Ok(());
        };
        kube::Api::<PersistentVolumeClaim>::namespaced(self.client.clone(), namespace)
            .patch(name, &PatchParams::default(), &Patch::Merge(patch))
            .await
            .map(|_| ())
            .map_err(|e| format!("patch persistentvolumeclaim {namespace}/{name} annotations: {e}"))
    }

    async fn annotate_pv(
        &self,
        name: &str,
        set: &BTreeMap<String, String>,
        remove: &[String],
    ) -> Result<(), String> {
        let Some(patch) = annotation_patch(set, remove) else {
            return Ok(());
        };
        kube::Api::<PersistentVolume>::all(self.client.clone())
            .patch(name, &PatchParams::default(), &Patch::Merge(patch))
            .await
            .map(|_| ())
            .map_err(|e| format!("patch persistentvolume {name} annotations: {e}"))
    }
}

pub(crate) fn from_pvc(p: &PersistentVolumeClaim) -> Pvc {
    let spec = p.spec.as_ref();
    Pvc {
        namespace: p.metadata.namespace.clone().unwrap_or_default(),
        name: p.metadata.name.clone().unwrap_or_default(),
        uid: p.metadata.uid.clone().unwrap_or_default(),
        phase: p
            .status
            .as_ref()
            .and_then(|s| s.phase.clone())
            .unwrap_or_default(),
        volume_name: spec.and_then(|s| s.volume_name.clone()).unwrap_or_default(),
        // An empty class name means "no class" while a missing one means "the
        // default class". Neither is a class name, so both report as empty.
        storage_class: spec
            .and_then(|s| s.storage_class_name.clone())
            .unwrap_or_default(),
        capacity_bytes: claim_capacity_bytes(p),
        annotations: p.metadata.annotations.clone().unwrap_or_default(),
    }
}

pub(crate) fn from_pv(p: &PersistentVolume) -> Pv {
    let spec = p.spec.as_ref();
    let claim = spec.and_then(|s| s.claim_ref.as_ref());
    Pv {
        name: p.metadata.name.clone().unwrap_or_default(),
        uid: p.metadata.uid.clone().unwrap_or_default(),
        phase: p
            .status
            .as_ref()
            .and_then(|s| s.phase.clone())
            .unwrap_or_default(),
        storage_class: spec
            .and_then(|s| s.storage_class_name.clone())
            .unwrap_or_default(),
        capacity_bytes: spec
            .and_then(|s| s.capacity.as_ref())
            .map_or(0, quantity_bytes),
        reclaim_policy: spec
            .and_then(|s| s.persistent_volume_reclaim_policy.clone())
            .unwrap_or_default(),
        claim_namespace: claim.and_then(|c| c.namespace.clone()).unwrap_or_default(),
        claim_name: claim.and_then(|c| c.name.clone()).unwrap_or_default(),
        claim_uid: claim.and_then(|c| c.uid.clone()).unwrap_or_default(),
        volume_id: ebs_volume_id(p),
        annotations: p.metadata.annotations.clone().unwrap_or_default(),
    }
}

/// Collects `namespace/name` for every claim referenced by a Pod that is not
/// in a terminal phase. Succeeded and Failed Pods are excluded on purpose:
/// their containers are gone and mount nothing, but the Pod object survives
/// until something reaps it.
pub(crate) fn claims_in_use(pods: &[Pod], out: &mut HashSet<String>) {
    for p in pods {
        let phase = p
            .status
            .as_ref()
            .and_then(|s| s.phase.as_deref())
            .unwrap_or_default();
        if phase == "Succeeded" || phase == "Failed" {
            continue;
        }
        let ns = p.metadata.namespace.as_deref().unwrap_or_default();
        for v in p
            .spec
            .as_ref()
            .map(|s| s.volumes.as_deref().unwrap_or_default())
            .unwrap_or_default()
        {
            if let Some(c) = &v.persistent_volume_claim {
                out.insert(format!("{ns}/{}", c.claim_name));
            }
        }
    }
}

/// Reduces a `StatefulSet` to the replica range and claim template names.
/// `spec.replicas` defaults to 1, and `spec.ordinals` is unset on everything
/// that has not opted into a non-zero start.
pub(crate) fn from_stateful_set(s: &K8sStatefulSet) -> StatefulSet {
    let spec = s.spec.as_ref();
    StatefulSet {
        namespace: s.metadata.namespace.clone().unwrap_or_default(),
        name: s.metadata.name.clone().unwrap_or_default(),
        replicas: spec.and_then(|s| s.replicas).unwrap_or(1),
        ordinal_start: spec
            .and_then(|s| s.ordinals.as_ref())
            .and_then(|o| o.start)
            .unwrap_or(0),
        claim_templates: spec
            .and_then(|s| s.volume_claim_templates.as_ref())
            .map(|ts| ts.iter().filter_map(|t| t.metadata.name.clone()).collect())
            .unwrap_or_default(),
    }
}

/// Extracts the EBS volume ID a volume is backed by, empty when it is not
/// EBS-backed. The CSI driver stores it verbatim; the removed in-tree plugin
/// stored it as `aws://<zone>/<volume-id>`, and volumes it provisioned outlive
/// the plugin itself.
pub(crate) fn ebs_volume_id(pv: &PersistentVolume) -> String {
    let Some(spec) = pv.spec.as_ref() else {
        return String::new();
    };
    if let Some(csi) = &spec.csi
        && csi.driver == EBS_CSI_DRIVER
    {
        return csi.volume_handle.clone();
    }
    if let Some(ebs) = &spec.aws_elastic_block_store {
        let id = ebs.volume_id.rsplit('/').next().unwrap_or_default();
        if id.starts_with("vol-") {
            return id.to_string();
        }
    }
    String::new()
}

/// Prefers the bound capacity over the requested one: a claim binds to a
/// volume at least as large as it asked for, and it is the volume that bills.
pub(crate) fn claim_capacity_bytes(p: &PersistentVolumeClaim) -> i64 {
    let bound = p
        .status
        .as_ref()
        .and_then(|s| s.capacity.as_ref())
        .map_or(0, quantity_bytes);
    if bound > 0 {
        return bound;
    }
    p.spec
        .as_ref()
        .and_then(|s| s.resources.as_ref())
        .and_then(|r| r.requests.as_ref())
        .map_or(0, quantity_bytes)
}

/// Reads the storage quantity out of a resource list.
pub(crate) fn quantity_bytes(list: &BTreeMap<String, Quantity>) -> i64 {
    list.get("storage").map_or(0, |q| parse_quantity(&q.0))
}

/// Parses a Kubernetes resource quantity into bytes, rounding up like
/// `Quantity.Value()`. Handles the binary (Ki, Mi, ...), decimal (k, M, ...),
/// and exponent (e3) suffixes.
#[must_use]
pub fn parse_quantity(s: &str) -> i64 {
    let s = s.trim();
    let split = s
        .find(|c: char| !(c.is_ascii_digit() || c == '.' || c == '-' || c == '+'))
        .unwrap_or(s.len());
    let (num, suffix) = s.split_at(split);
    let Ok(value) = num.parse::<f64>() else {
        return 0;
    };
    let scale: f64 = match suffix {
        "" => 1.0,
        "Ki" => 1024.0,
        "Mi" => 1024f64.powi(2),
        "Gi" => 1024f64.powi(3),
        "Ti" => 1024f64.powi(4),
        "Pi" => 1024f64.powi(5),
        "Ei" => 1024f64.powi(6),
        "m" => 1e-3,
        "k" => 1e3,
        "M" => 1e6,
        "G" => 1e9,
        "T" => 1e12,
        "P" => 1e15,
        "E" => 1e18,
        _ => match suffix
            .strip_prefix(['e', 'E'])
            .and_then(|e| e.parse::<i32>().ok())
        {
            Some(exp) => 10f64.powi(exp),
            None => return 0,
        },
    };
    #[allow(clippy::cast_possible_truncation)]
    let out = (value * scale).ceil() as i64;
    out.max(0)
}

#[cfg(test)]
mod tests {
    use k8s_openapi::api::apps::v1::{StatefulSetOrdinals, StatefulSetSpec};
    use k8s_openapi::api::core::v1::{
        AWSElasticBlockStoreVolumeSource, CSIPersistentVolumeSource, ObjectReference,
        PersistentVolumeClaimSpec, PersistentVolumeClaimStatus, PersistentVolumeClaimVolumeSource,
        PersistentVolumeSpec, PersistentVolumeStatus, PodSpec, PodStatus, Volume,
        VolumeResourceRequirements,
    };
    use kube::api::ObjectMeta;

    use super::*;

    fn q(s: &str) -> BTreeMap<String, Quantity> {
        BTreeMap::from([("storage".to_string(), Quantity(s.into()))])
    }

    #[test]
    fn quantities() {
        assert_eq!(parse_quantity("20Gi"), 20 * 1024 * 1024 * 1024);
        assert_eq!(parse_quantity("1Ki"), 1024);
        assert_eq!(parse_quantity("1.5Mi"), 1_572_864);
        assert_eq!(parse_quantity("100"), 100);
        assert_eq!(parse_quantity("1k"), 1000);
        assert_eq!(parse_quantity("2M"), 2_000_000);
        assert_eq!(parse_quantity("1G"), 1_000_000_000);
        assert_eq!(parse_quantity("1T"), 1_000_000_000_000);
        assert_eq!(parse_quantity("1Ti"), 1024i64.pow(4));
        assert_eq!(parse_quantity("1e3"), 1000);
        assert_eq!(parse_quantity("1500m"), 2, "rounds up");
        assert_eq!(parse_quantity("bogus"), 0);
        assert_eq!(parse_quantity("1Xi"), 0);
        assert_eq!(parse_quantity("-5"), 0);
        assert_eq!(quantity_bytes(&BTreeMap::new()), 0);
        assert_eq!(quantity_bytes(&q("1Mi")), 1024 * 1024);
    }

    #[test]
    fn claim_capacity_prefers_bound_size() {
        let mut p = PersistentVolumeClaim {
            spec: Some(PersistentVolumeClaimSpec {
                resources: Some(VolumeResourceRequirements {
                    requests: Some(q("10Gi")),
                    ..VolumeResourceRequirements::default()
                }),
                ..PersistentVolumeClaimSpec::default()
            }),
            ..PersistentVolumeClaim::default()
        };
        assert_eq!(
            claim_capacity_bytes(&p),
            10 * 1024 * 1024 * 1024,
            "falls back to the request"
        );
        p.status = Some(PersistentVolumeClaimStatus {
            capacity: Some(q("20Gi")),
            ..PersistentVolumeClaimStatus::default()
        });
        assert_eq!(claim_capacity_bytes(&p), 20 * 1024 * 1024 * 1024);
        assert_eq!(claim_capacity_bytes(&PersistentVolumeClaim::default()), 0);
    }

    #[test]
    fn ebs_volume_ids() {
        let csi = PersistentVolume {
            spec: Some(PersistentVolumeSpec {
                csi: Some(CSIPersistentVolumeSource {
                    driver: EBS_CSI_DRIVER.into(),
                    volume_handle: "vol-abc".into(),
                    ..CSIPersistentVolumeSource::default()
                }),
                ..PersistentVolumeSpec::default()
            }),
            ..PersistentVolume::default()
        };
        assert_eq!(ebs_volume_id(&csi), "vol-abc");
        let other_csi = PersistentVolume {
            spec: Some(PersistentVolumeSpec {
                csi: Some(CSIPersistentVolumeSource {
                    driver: "efs.csi.aws.com".into(),
                    volume_handle: "fs-1".into(),
                    ..CSIPersistentVolumeSource::default()
                }),
                ..PersistentVolumeSpec::default()
            }),
            ..PersistentVolume::default()
        };
        assert_eq!(ebs_volume_id(&other_csi), "");
        let intree = PersistentVolume {
            spec: Some(PersistentVolumeSpec {
                aws_elastic_block_store: Some(AWSElasticBlockStoreVolumeSource {
                    volume_id: "aws://ap-northeast-2a/vol-123".into(),
                    ..AWSElasticBlockStoreVolumeSource::default()
                }),
                ..PersistentVolumeSpec::default()
            }),
            ..PersistentVolume::default()
        };
        assert_eq!(ebs_volume_id(&intree), "vol-123");
        let bad = PersistentVolume {
            spec: Some(PersistentVolumeSpec {
                aws_elastic_block_store: Some(AWSElasticBlockStoreVolumeSource {
                    volume_id: "aws://zone/nope".into(),
                    ..AWSElasticBlockStoreVolumeSource::default()
                }),
                ..PersistentVolumeSpec::default()
            }),
            ..PersistentVolume::default()
        };
        assert_eq!(ebs_volume_id(&bad), "");
        assert_eq!(ebs_volume_id(&PersistentVolume::default()), "");
    }

    #[test]
    #[allow(clippy::many_single_char_names, clippy::too_many_lines)]
    fn conversions() {
        let p = PersistentVolumeClaim {
            metadata: ObjectMeta {
                namespace: Some("ns".into()),
                name: Some("c".into()),
                uid: Some("u".into()),
                annotations: Some(BTreeMap::from([("k".to_string(), "v".to_string())])),
                ..ObjectMeta::default()
            },
            spec: Some(PersistentVolumeClaimSpec {
                volume_name: Some("pv-1".into()),
                storage_class_name: Some("gp3".into()),
                ..PersistentVolumeClaimSpec::default()
            }),
            status: Some(PersistentVolumeClaimStatus {
                phase: Some("Bound".into()),
                capacity: Some(q("1Gi")),
                ..PersistentVolumeClaimStatus::default()
            }),
        };
        let c = from_pvc(&p);
        assert_eq!(c.namespace, "ns");
        assert_eq!(c.name, "c");
        assert_eq!(c.uid, "u");
        assert_eq!(c.phase, "Bound");
        assert_eq!(c.volume_name, "pv-1");
        assert_eq!(c.storage_class, "gp3");
        assert_eq!(c.capacity_bytes, 1024 * 1024 * 1024);
        assert_eq!(c.annotations["k"], "v");
        assert_eq!(
            from_pvc(&PersistentVolumeClaim::default()).storage_class,
            ""
        );

        let v = PersistentVolume {
            metadata: ObjectMeta {
                name: Some("pv-1".into()),
                uid: Some("pu".into()),
                ..ObjectMeta::default()
            },
            spec: Some(PersistentVolumeSpec {
                storage_class_name: Some("gp3".into()),
                capacity: Some(q("2Gi")),
                persistent_volume_reclaim_policy: Some("Retain".into()),
                claim_ref: Some(ObjectReference {
                    namespace: Some("ns".into()),
                    name: Some("c".into()),
                    uid: Some("u".into()),
                    ..ObjectReference::default()
                }),
                csi: Some(CSIPersistentVolumeSource {
                    driver: EBS_CSI_DRIVER.into(),
                    volume_handle: "vol-1".into(),
                    ..CSIPersistentVolumeSource::default()
                }),
                ..PersistentVolumeSpec::default()
            }),
            status: Some(PersistentVolumeStatus {
                phase: Some("Bound".into()),
                ..PersistentVolumeStatus::default()
            }),
        };
        let pv = from_pv(&v);
        assert_eq!(pv.name, "pv-1");
        assert_eq!(pv.uid, "pu");
        assert_eq!(pv.phase, "Bound");
        assert_eq!(pv.storage_class, "gp3");
        assert_eq!(pv.capacity_bytes, 2 * 1024 * 1024 * 1024);
        assert_eq!(pv.reclaim_policy, "Retain");
        assert_eq!(
            (
                pv.claim_namespace.as_str(),
                pv.claim_name.as_str(),
                pv.claim_uid.as_str()
            ),
            ("ns", "c", "u")
        );
        assert_eq!(pv.volume_id, "vol-1");

        let sts = K8sStatefulSet {
            metadata: ObjectMeta {
                namespace: Some("ns".into()),
                name: Some("db".into()),
                ..ObjectMeta::default()
            },
            spec: Some(StatefulSetSpec {
                replicas: Some(3),
                ordinals: Some(StatefulSetOrdinals { start: Some(2) }),
                volume_claim_templates: Some(vec![PersistentVolumeClaim {
                    metadata: ObjectMeta {
                        name: Some("data".into()),
                        ..ObjectMeta::default()
                    },
                    ..PersistentVolumeClaim::default()
                }]),
                ..StatefulSetSpec::default()
            }),
            status: None,
        };
        let s = from_stateful_set(&sts);
        assert_eq!(
            (
                s.namespace.as_str(),
                s.name.as_str(),
                s.replicas,
                s.ordinal_start
            ),
            ("ns", "db", 3, 2)
        );
        assert_eq!(s.claim_templates, vec!["data"]);
        let d = from_stateful_set(&K8sStatefulSet::default());
        assert_eq!((d.replicas, d.ordinal_start), (1, 0));
        assert!(d.claim_templates.is_empty());
    }

    #[test]
    fn claims_in_use_ignores_terminal_pods() {
        let pod = |ns: &str, phase: &str, claim: &str| Pod {
            metadata: ObjectMeta {
                namespace: Some(ns.into()),
                ..ObjectMeta::default()
            },
            spec: Some(PodSpec {
                volumes: Some(vec![
                    Volume {
                        persistent_volume_claim: Some(PersistentVolumeClaimVolumeSource {
                            claim_name: claim.into(),
                            read_only: None,
                        }),
                        ..Volume::default()
                    },
                    Volume::default(),
                ]),
                ..PodSpec::default()
            }),
            status: Some(PodStatus {
                phase: Some(phase.into()),
                ..PodStatus::default()
            }),
        };
        let pods = vec![
            pod("a", "Running", "live"),
            pod("a", "Pending", "soon"),
            pod("b", "Succeeded", "done"),
            pod("b", "Failed", "broken"),
            Pod::default(),
        ];
        let mut got = HashSet::new();
        claims_in_use(&pods, &mut got);
        claims_in_use(&pods[..1], &mut got);
        assert_eq!(
            got,
            HashSet::from(["a/live".to_string(), "a/soon".to_string()])
        );
    }
}
