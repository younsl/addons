//! Turns one pass's inventory into a `Finding` per claim and volume.

use std::collections::{HashMap, HashSet};
use std::time::Duration;

use chrono::{DateTime, Utc};

use super::annotations::{KEY_UNUSED_SINCE, key};
use super::types::{
    Finding, Inventory, KIND_PV, KIND_PVC, PHASE_AVAILABLE, PHASE_BOUND, PHASE_FAILED,
    PHASE_RELEASED, Pv, Pvc, REASON_AVAILABLE, REASON_BOUND_TO_USED_CLAIM, REASON_FAILED,
    REASON_MISSING_CLAIM, REASON_MOUNTED_BY_POD, REASON_NO_CONSUMER_POD, REASON_PENDING,
    REASON_RELEASED, REASON_STATEFULSET_SCALED_DOWN, REASON_STATEFULSET_SLOT, REASON_UNBOUND,
    REASON_UNUSED_CLAIM, StatefulSet,
};

/// Classifies the inventory. Claims are classified first because a volume's
/// verdict can depend on its claim's: a bound volume is only unused if the
/// claim holding it is, and deciding that twice from different data would let
/// the two disagree. Every namespace is in scope.
#[must_use]
pub fn classify(inv: &Inventory, min_age: Duration, now: DateTime<Utc>) -> Vec<Finding> {
    // The claim-to-volume lookup is built once: resolving it per claim would
    // make the pass quadratic on a cluster with thousands of volumes.
    let volume_id: HashMap<&str, &str> = inv
        .pvs
        .iter()
        .map(|v| (v.name.as_str(), v.volume_id.as_str()))
        .collect();
    let mut sets_by_namespace: HashMap<&str, Vec<&StatefulSet>> = HashMap::new();
    for s in &inv.stateful_sets {
        sets_by_namespace
            .entry(s.namespace.as_str())
            .or_default()
            .push(s);
    }

    let mut claim_unused: HashMap<String, bool> = HashMap::with_capacity(inv.pvcs.len());
    let mut claim_uid: HashMap<String, &str> = HashMap::with_capacity(inv.pvcs.len());

    let mut out = Vec::with_capacity(inv.pvcs.len() + inv.pvs.len());
    for c in &inv.pvcs {
        let k = format!("{}/{}", c.namespace, c.name);
        let sets = sets_by_namespace
            .get(c.namespace.as_str())
            .map(Vec::as_slice)
            .unwrap_or_default();
        let (unused, reason) = classify_pvc(c, &inv.claims_in_use, sets, &k);
        claim_unused.insert(k.clone(), unused);
        claim_uid.insert(k, c.uid.as_str());
        out.push(finalize(
            Finding {
                kind: KIND_PVC.into(),
                namespace: c.namespace.clone(),
                name: c.name.clone(),
                uid: c.uid.clone(),
                unused,
                reason: reason.into(),
                capacity_bytes: c.capacity_bytes,
                storage_class: c.storage_class.clone(),
                volume_id: volume_id
                    .get(c.volume_name.as_str())
                    .map(|s| (*s).to_string())
                    .unwrap_or_default(),
                volume_name: c.volume_name.clone(),
                annotations: c.annotations.clone(),
                ..Finding::default()
            },
            min_age,
            now,
        ));
    }

    for v in &inv.pvs {
        let k = format!("{}/{}", v.claim_namespace, v.claim_name);
        let (unused, reason) = classify_pv(v, &k, &claim_unused, &claim_uid);
        out.push(finalize(
            Finding {
                kind: KIND_PV.into(),
                name: v.name.clone(),
                uid: v.uid.clone(),
                unused,
                reason: reason.into(),
                capacity_bytes: v.capacity_bytes,
                storage_class: v.storage_class.clone(),
                volume_id: v.volume_id.clone(),
                claim_namespace: v.claim_namespace.clone(),
                claim_name: v.claim_name.clone(),
                reclaim_policy: v.reclaim_policy.clone(),
                annotations: v.annotations.clone(),
                ..Finding::default()
            },
            min_age,
            now,
        ));
    }
    out
}

/// Decides whether one claim is unused and why. A Pod in a terminal phase is
/// not a consumer; that filter is applied where the Pod list is built.
fn classify_pvc(
    c: &Pvc,
    in_use: &HashSet<String>,
    sets: &[&StatefulSet],
    key: &str,
) -> (bool, &'static str) {
    if in_use.contains(key) {
        return (false, REASON_MOUNTED_BY_POD);
    }
    // A volumeClaimTemplate claim within its StatefulSet's replica range has
    // no Pod between a delete and the next schedule, and during a rolling
    // update every replica passes through that gap.
    if let Some(live) = stateful_set_slot(c, sets) {
        return if live {
            (false, REASON_STATEFULSET_SLOT)
        } else {
            (true, REASON_STATEFULSET_SCALED_DOWN)
        };
    }
    if c.phase != PHASE_BOUND {
        return (true, REASON_UNBOUND);
    }
    (true, REASON_NO_CONSUMER_POD)
}

/// Decides whether one volume is unused and why.
fn classify_pv(
    v: &Pv,
    key: &str,
    claim_unused: &HashMap<String, bool>,
    claim_uid: &HashMap<String, &str>,
) -> (bool, &'static str) {
    match v.phase.as_str() {
        PHASE_AVAILABLE => (true, REASON_AVAILABLE),
        PHASE_RELEASED => (true, REASON_RELEASED),
        PHASE_FAILED => (true, REASON_FAILED),
        PHASE_BOUND => {
            if v.claim_name.is_empty() {
                return (true, REASON_MISSING_CLAIM);
            }
            let Some(&unused) = claim_unused.get(key) else {
                return (true, REASON_MISSING_CLAIM);
            };
            // An empty claimRef UID means the volume was pre-bound by hand
            // rather than by the binder, so a name match is all the evidence
            // there is.
            if !v.claim_uid.is_empty() && claim_uid.get(key).copied() != Some(v.claim_uid.as_str())
            {
                return (true, REASON_MISSING_CLAIM);
            }
            if unused {
                (true, REASON_UNUSED_CLAIM)
            } else {
                (false, REASON_BOUND_TO_USED_CLAIM)
            }
        }
        // Pending, or a phase this build does not know. Provisioning is in
        // flight; calling it unused would report every volume being created.
        _ => (false, REASON_PENDING),
    }
}

/// Reports whether a claim was generated from a `volumeClaimTemplate` and, if
/// so, whether its ordinal is inside the `StatefulSet`'s current replica
/// range. The match is by name because the `StatefulSet` controller sets no
/// owner reference on the claims it generates.
fn stateful_set_slot(c: &Pvc, sets: &[&StatefulSet]) -> Option<bool> {
    for s in sets {
        if s.namespace != c.namespace {
            continue;
        }
        for tmpl in &s.claim_templates {
            let Some(rest) = c.name.strip_prefix(&format!("{tmpl}-{}-", s.name)) else {
                continue;
            };
            let Ok(ordinal) = rest.parse::<i32>() else {
                continue;
            };
            return Some(ordinal >= s.ordinal_start && ordinal < s.ordinal_start + s.replicas);
        }
    }
    None
}

/// Resolves how long the object has been unused and whether that is long
/// enough to report. The clock is read from the annotation the previous pass
/// wrote, so it survives a restart of the controller; an object that has
/// never been marked starts its clock now.
fn finalize(mut f: Finding, min_age: Duration, now: DateTime<Utc>) -> Finding {
    if !f.unused {
        return f;
    }
    let since = unused_since(&f.annotations, now);
    f.unused_since = Some(since);
    f.age = (now - since).to_std().unwrap_or_default();
    f.reportable = f.age >= min_age;
    f
}

/// Reads the persisted first-observed timestamp, falling back to now for an
/// object that carries none or carries one that does not parse. A timestamp
/// in the future is also discarded: a clock that ran backwards would
/// otherwise hold the object below the threshold indefinitely.
fn unused_since(
    existing: &std::collections::BTreeMap<String, String>,
    now: DateTime<Utc>,
) -> DateTime<Utc> {
    existing
        .get(&key(KEY_UNUSED_SINCE))
        .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
        .map(|t| t.with_timezone(&Utc))
        .filter(|t| *t <= now)
        .unwrap_or(now)
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;

    fn pvc(ns: &str, name: &str, phase: &str) -> Pvc {
        Pvc {
            namespace: ns.into(),
            name: name.into(),
            uid: format!("uid-{name}"),
            phase: phase.into(),
            volume_name: format!("pv-{name}"),
            ..Pvc::default()
        }
    }

    fn sts(ns: &str, name: &str, replicas: i32, start: i32, templates: &[&str]) -> StatefulSet {
        StatefulSet {
            namespace: ns.into(),
            name: name.into(),
            replicas,
            ordinal_start: start,
            claim_templates: templates.iter().map(|s| (*s).to_string()).collect(),
        }
    }

    fn pv(name: &str, phase: &str, claim: (&str, &str, &str)) -> Pv {
        Pv {
            name: name.into(),
            phase: phase.into(),
            claim_namespace: claim.0.into(),
            claim_name: claim.1.into(),
            claim_uid: claim.2.into(),
            volume_id: format!("vol-{name}"),
            ..Pv::default()
        }
    }

    #[test]
    fn claims() {
        let in_use = HashSet::from(["ns/mounted".to_string()]);
        let sets = [
            sts("ns", "db", 2, 0, &["data"]),
            sts("ns", "cache", 1, 5, &["cache"]),
            sts("other", "db", 10, 0, &["data"]),
        ];
        let refs: Vec<&StatefulSet> = sets.iter().collect();
        let cases = [
            (
                pvc("ns", "mounted", "Bound"),
                (false, REASON_MOUNTED_BY_POD),
            ),
            (pvc("ns", "orphan", "Bound"), (true, REASON_NO_CONSUMER_POD)),
            (pvc("ns", "never", "Pending"), (true, REASON_UNBOUND)),
            (
                pvc("ns", "data-db-0", "Bound"),
                (false, REASON_STATEFULSET_SLOT),
            ),
            (
                pvc("ns", "data-db-1", "Bound"),
                (false, REASON_STATEFULSET_SLOT),
            ),
            (
                pvc("ns", "data-db-2", "Bound"),
                (true, REASON_STATEFULSET_SCALED_DOWN),
            ),
            (
                pvc("ns", "data-db-x", "Bound"),
                (true, REASON_NO_CONSUMER_POD),
            ),
            (
                pvc("ns", "cache-cache-5", "Bound"),
                (false, REASON_STATEFULSET_SLOT),
            ),
            (
                pvc("ns", "cache-cache-4", "Bound"),
                (true, REASON_STATEFULSET_SCALED_DOWN),
            ),
            (
                pvc("ns", "data-db-7", "Pending"),
                (true, REASON_STATEFULSET_SCALED_DOWN),
            ),
        ];
        for (c, want) in cases {
            let k = format!("{}/{}", c.namespace, c.name);
            assert_eq!(classify_pvc(&c, &in_use, &refs, &k), want, "{}", c.name);
        }
        // A same-named StatefulSet in another namespace does not claim the slot.
        let c = pvc("other", "data-db-7", "Bound");
        let other: Vec<&StatefulSet> = sets.iter().filter(|s| s.namespace == "other").collect();
        assert_eq!(
            classify_pvc(&c, &in_use, &other, "other/data-db-7"),
            (false, REASON_STATEFULSET_SLOT)
        );
        assert_eq!(
            classify_pvc(&c, &in_use, &refs[..2], "other/data-db-7"),
            (true, REASON_NO_CONSUMER_POD)
        );
    }

    #[test]
    fn volumes() {
        let claim_unused = HashMap::from([
            ("ns/used".to_string(), false),
            ("ns/idle".to_string(), true),
        ]);
        let claim_uid = HashMap::from([
            ("ns/used".to_string(), "uid-used"),
            ("ns/idle".to_string(), "uid-idle"),
        ]);
        let cases = [
            (pv("a", "Available", ("", "", "")), (true, REASON_AVAILABLE)),
            (
                pv("r", "Released", ("ns", "gone", "u")),
                (true, REASON_RELEASED),
            ),
            (pv("f", "Failed", ("", "", "")), (true, REASON_FAILED)),
            (
                pv("b1", "Bound", ("", "", "")),
                (true, REASON_MISSING_CLAIM),
            ),
            (
                pv("b2", "Bound", ("ns", "missing", "u")),
                (true, REASON_MISSING_CLAIM),
            ),
            (
                pv("b3", "Bound", ("ns", "used", "uid-stale")),
                (true, REASON_MISSING_CLAIM),
            ),
            (
                pv("b4", "Bound", ("ns", "used", "uid-used")),
                (false, REASON_BOUND_TO_USED_CLAIM),
            ),
            (
                pv("b5", "Bound", ("ns", "used", "")),
                (false, REASON_BOUND_TO_USED_CLAIM),
            ),
            (
                pv("b6", "Bound", ("ns", "idle", "uid-idle")),
                (true, REASON_UNUSED_CLAIM),
            ),
            (pv("p", "Pending", ("", "", "")), (false, REASON_PENDING)),
            (pv("w", "Weird", ("", "", "")), (false, REASON_PENDING)),
        ];
        for (v, want) in cases {
            let k = format!("{}/{}", v.claim_namespace, v.claim_name);
            assert_eq!(
                classify_pv(&v, &k, &claim_unused, &claim_uid),
                want,
                "{}",
                v.name
            );
        }
    }

    #[test]
    fn classify_joins_claims_and_volumes() {
        let now = Utc::now();
        let inv = Inventory {
            pvcs: vec![pvc("ns", "idle", "Bound"), pvc("ns", "used", "Bound")],
            pvs: vec![
                pv("pv-idle", "Bound", ("ns", "idle", "uid-idle")),
                pv("pv-used", "Bound", ("ns", "used", "uid-used")),
            ],
            claims_in_use: HashSet::from(["ns/used".to_string()]),
            stateful_sets: vec![],
        };
        let f = classify(&inv, Duration::from_hours(24), now);
        assert_eq!(f.len(), 4);
        assert_eq!(f[0].kind, KIND_PVC);
        assert!(f[0].unused);
        assert_eq!(
            f[0].volume_id, "vol-pv-idle",
            "claim carries the volume's EBS ID"
        );
        assert_eq!(f[0].volume_name, "pv-idle");
        assert_eq!(f[0].unused_since, Some(now));
        assert!(!f[0].reportable);
        assert!(!f[1].unused);
        assert!(f[1].unused_since.is_none());
        assert_eq!(f[2].kind, KIND_PV);
        assert_eq!(f[2].reason, REASON_UNUSED_CLAIM);
        assert_eq!(f[2].claim_name, "idle");
        assert!(!f[3].unused);
    }

    #[test]
    fn finalize_uses_persisted_clock_and_ignores_unusable_ones() {
        let now = Utc::now();
        let since = now - chrono::TimeDelta::days(2);
        let mut f = Finding {
            unused: true,
            ..Finding::default()
        };
        f.annotations
            .insert(key(KEY_UNUSED_SINCE), since.to_rfc3339());
        let out = finalize(f.clone(), Duration::from_hours(24), now);
        assert_eq!(out.unused_since.unwrap().timestamp(), since.timestamp());
        assert!(out.reportable);
        assert_eq!(out.unused_days(), 2);

        f.annotations
            .insert(key(KEY_UNUSED_SINCE), "not a time".into());
        let out = finalize(f.clone(), Duration::from_hours(24), now);
        assert_eq!(out.unused_since, Some(now));
        assert!(!out.reportable);

        f.annotations.insert(
            key(KEY_UNUSED_SINCE),
            (now + chrono::TimeDelta::days(1)).to_rfc3339(),
        );
        let out = finalize(f, Duration::from_hours(24), now);
        assert_eq!(out.unused_since, Some(now), "future clock is discarded");

        let used = finalize(Finding::default(), Duration::from_secs(1), now);
        assert!(used.unused_since.is_none());
        assert_eq!(used.age, Duration::ZERO);
        let _ = BTreeMap::<String, String>::new();
    }
}
