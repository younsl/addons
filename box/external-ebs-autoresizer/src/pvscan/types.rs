use std::collections::{BTreeMap, HashSet};
use std::time::Duration;

use chrono::{DateTime, Utc};

/// Kinds reported by the scanner. They are the lowercase Kubernetes resource
/// names rather than the type names, so a metric label or a log field matches
/// what an operator types into kubectl.
pub const KIND_PVC: &str = "persistentvolumeclaim";
pub const KIND_PV: &str = "persistentvolume";

/// Reasons a `PersistentVolumeClaim` is unused.
///
/// `no_consumer_pod` is a bound claim that no live Pod mounts, the ordinary
/// leak. `statefulset_scaled_down` is a `volumeClaimTemplate` claim whose
/// ordinal is outside its `StatefulSet`'s current replica range, the leak that
/// survives longest unnoticed. `unbound` is a claim that never bound and has
/// no Pod to trigger binding.
pub const REASON_NO_CONSUMER_POD: &str = "no_consumer_pod";
pub const REASON_STATEFULSET_SCALED_DOWN: &str = "statefulset_scaled_down";
pub const REASON_UNBOUND: &str = "unbound";

/// Reasons a `PersistentVolume` is unused.
pub const REASON_AVAILABLE: &str = "available";
pub const REASON_RELEASED: &str = "released";
pub const REASON_FAILED: &str = "failed";
pub const REASON_MISSING_CLAIM: &str = "missing_claim";
pub const REASON_UNUSED_CLAIM: &str = "bound_to_unused_claim";

/// Reasons an object is in use, recorded so a per-object log line says why
/// the scanner left it alone. They are never reported as findings.
pub const REASON_MOUNTED_BY_POD: &str = "mounted_by_pod";
pub const REASON_STATEFULSET_SLOT: &str = "statefulset_slot";
pub const REASON_BOUND_TO_USED_CLAIM: &str = "bound_to_used_claim";
pub const REASON_PENDING: &str = "pending";

/// The fixed reason sets, in report order. The summary gauge is published for
/// every entry on every pass, including the ones that matched nothing.
pub const UNUSED_PVC_REASONS: [&str; 3] = [
    REASON_NO_CONSUMER_POD,
    REASON_STATEFULSET_SCALED_DOWN,
    REASON_UNBOUND,
];
pub const UNUSED_PV_REASONS: [&str; 5] = [
    REASON_RELEASED,
    REASON_AVAILABLE,
    REASON_FAILED,
    REASON_MISSING_CLAIM,
    REASON_UNUSED_CLAIM,
];

/// Kubernetes phase values the classifier switches on.
pub const PHASE_BOUND: &str = "Bound";
pub const PHASE_AVAILABLE: &str = "Available";
pub const PHASE_RELEASED: &str = "Released";
pub const PHASE_FAILED: &str = "Failed";

/// The subset of a `PersistentVolumeClaim` the scanner needs.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Pvc {
    pub namespace: String,
    pub name: String,
    /// Distinguishes this claim from a deleted one that had the same name, so
    /// a volume still pointing at the old UID reads as orphaned.
    pub uid: String,
    pub phase: String,
    pub volume_name: String,
    pub storage_class: String,
    pub capacity_bytes: i64,
    pub annotations: BTreeMap<String, String>,
}

/// The subset of a `PersistentVolume` the scanner needs.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Pv {
    pub name: String,
    pub uid: String,
    pub phase: String,
    pub storage_class: String,
    pub capacity_bytes: i64,
    pub reclaim_policy: String,
    /// From `spec.claimRef`. The UID is empty on a volume that was pre-bound
    /// by an operator rather than by the binder, which is why a mismatch is
    /// only checked when it is set.
    pub claim_namespace: String,
    pub claim_name: String,
    pub claim_uid: String,
    /// The EBS volume ID behind the volume, empty when it is not EBS-backed.
    pub volume_id: String,
    pub annotations: BTreeMap<String, String>,
}

/// The subset of a `StatefulSet` the scanner needs to tell a claim held open
/// for a live replica slot from one left behind by a scale-down.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StatefulSet {
    pub namespace: String,
    pub name: String,
    pub replicas: i32,
    /// `spec.ordinals.start`, 0 unless the `StatefulSet` sets it.
    pub ordinal_start: i32,
    /// The `volumeClaimTemplate` names, which prefix the claim names the
    /// controller generates.
    pub claim_templates: Vec<String>,
}

/// One pass's cluster snapshot.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Inventory {
    pub pvcs: Vec<Pvc>,
    pub pvs: Vec<Pv>,
    /// `namespace/name` for every claim a live Pod references.
    pub claims_in_use: HashSet<String>,
    pub stateful_sets: Vec<StatefulSet>,
}

/// One object's verdict for one pass.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Finding {
    pub kind: String,
    /// Empty for a `PersistentVolume`, which is cluster-scoped.
    pub namespace: String,
    pub name: String,
    pub uid: String,
    pub unused: bool,
    pub reason: String,
    /// When the object was first observed unused, carried across restarts by
    /// the annotation the scanner writes. `None` when the object is in use.
    pub unused_since: Option<DateTime<Utc>>,
    /// How long the object has been unused, and whether that has passed the
    /// minimum age. An unused object below the threshold is still annotated
    /// (that is where the clock lives) but is not yet reported.
    pub age: Duration,
    pub reportable: bool,
    pub capacity_bytes: i64,
    pub storage_class: String,
    pub volume_id: String,
    /// The `PersistentVolume` a claim is bound to, empty for a claim that
    /// never bound and on a volume finding.
    pub volume_name: String,
    /// The claim a volume is (or was) bound to, empty for a volume that was
    /// never claimed and on a claim finding.
    pub claim_namespace: String,
    pub claim_name: String,
    pub reclaim_policy: String,
    /// The object's current annotation set, used to skip a patch when nothing
    /// changed.
    pub annotations: BTreeMap<String, String>,
}

impl Finding {
    /// Identifies the finding's object for logs and errors.
    #[must_use]
    pub fn key(&self) -> String {
        if self.namespace.is_empty() {
            self.name.clone()
        } else {
            format!("{}/{}", self.namespace, self.name)
        }
    }

    /// Names the object on the other side of the binding: the volume for a
    /// claim, the claim for a volume. Empty when nothing is bound.
    #[must_use]
    pub fn bound_to(&self) -> String {
        if self.kind == KIND_PVC {
            return self.volume_name.clone();
        }
        if self.claim_name.is_empty() {
            return String::new();
        }
        format!("{}/{}", self.claim_namespace, self.claim_name)
    }

    /// Whole days the object has been unused.
    #[must_use]
    pub const fn unused_days(&self) -> u64 {
        self.age.as_secs() / (24 * 3600)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_and_bound_to() {
        let claim = Finding {
            kind: KIND_PVC.into(),
            namespace: "ns".into(),
            name: "c".into(),
            volume_name: "pv-1".into(),
            ..Finding::default()
        };
        assert_eq!(claim.key(), "ns/c");
        assert_eq!(claim.bound_to(), "pv-1");
        let pv = Finding {
            kind: KIND_PV.into(),
            name: "pv-1".into(),
            claim_namespace: "ns".into(),
            claim_name: "c".into(),
            age: Duration::from_secs(3 * 86400 + 5),
            ..Finding::default()
        };
        assert_eq!(pv.key(), "pv-1");
        assert_eq!(pv.bound_to(), "ns/c");
        assert_eq!(pv.unused_days(), 3);
        let unclaimed = Finding {
            kind: KIND_PV.into(),
            ..Finding::default()
        };
        assert_eq!(unclaimed.bound_to(), "");
    }
}
