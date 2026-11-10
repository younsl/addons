//! Orphan PVC cleanup: opt-in deletion of the `PersistentVolumeClaims` the
//! scan reports as unused.
//!
//! Only claims are ever deleted, never a `PersistentVolume` or an EBS volume:
//! what happens to the volume behind a deleted claim is decided by its own
//! reclaim policy, and a `Retain` volume stays reported as `released`.
//!
//! A claim is deleted once it has been unused for `min_unused_age`. It is
//! scheduled `alert_before` ahead of that, and the deadline is never less than
//! `alert_before` after the pass that schedules it, so enabling cleanup on a
//! cluster full of claims unused for months still announces every deletion
//! first. The deadline is persisted in the claim's own
//! `orphan-pvc-delete-after` annotation, so a restart neither resets nor
//! skips it.

use std::collections::HashSet;
use std::time::Duration;

use chrono::{DateTime, Utc};

use super::annotations::{KEY_ORPHAN_PVC_CLEANUP, KEY_ORPHAN_PVC_DELETE_AFTER, key};
use super::types::{Finding, KIND_PVC};

/// The only `orphan-pvc-cleanup` annotation value that leaves a claim eligible. Any
/// other value protects it, so a typo never removes protection.
pub const ORPHAN_PVC_CLEANUP_ENABLED: &str = "enabled";

/// Why a claim is not an orphan PVC cleanup candidate. Logged per claim and quoted in the
/// Event that cancels a scheduled deletion.
pub const CAUSE_NOT_A_CLAIM: &str = "not_a_claim";
pub const CAUSE_IN_USE: &str = "in_use";
pub const CAUSE_REASON_NOT_SELECTED: &str = "reason_not_selected";
pub const CAUSE_TOO_RECENT: &str = "unused_for_less_than_min_unused_age";
pub const CAUSE_NAMESPACE_EXCLUDED: &str = "namespace_excluded";
pub const CAUSE_PROTECTED: &str = "protected_by_annotation";
pub const CAUSE_OWNED: &str = "has_owner_reference";
pub const CAUSE_REFERENCED: &str = "referenced_by_workload";
pub const CAUSE_DISABLED: &str = "orphan_pvc_cleanup_disabled";

/// Orphan PVC cleanup outcomes, the `action` label of its counter.
pub const ACTION_SCHEDULED: &str = "scheduled";
pub const ACTION_CANCELED: &str = "canceled";
pub const ACTION_DELETED: &str = "deleted";
pub const ACTION_FAILED: &str = "failed";
pub const ACTION_DEFERRED: &str = "deferred";
pub const ACTION_SKIPPED_IN_USE: &str = "skipped_in_use";
pub const ACTION_DRY_RUN: &str = "dry_run";

/// Which unused claims may be deleted, and how carefully.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Policy {
    /// How long a claim must have been continuously unused before it is
    /// deleted. Never below the scan's own reporting threshold.
    pub min_unused_age: Duration,
    /// The unused reasons that make a claim eligible.
    pub reasons: Vec<String>,
    /// Empty means every namespace.
    pub include_namespaces: Vec<String>,
    pub exclude_namespaces: Vec<String>,
    /// Caps the deletions of one pass, so a misjudgement cannot empty a
    /// cluster in a single hour.
    pub max_deletions_per_pass: usize,
    /// How far ahead of its deadline a claim is scheduled and raises its one
    /// alert, and the least notice any deletion gets.
    pub alert_before: Duration,
}

/// One claim's cleanup decision for one pass.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    Ineligible(&'static str),
    /// Eligible and waiting out the notice. `new` is true on the pass that
    /// starts the notice.
    Scheduled {
        delete_after: DateTime<Utc>,
        new: bool,
    },
    /// Eligible and past the notice.
    Due {
        delete_after: DateTime<Utc>,
    },
}

impl Verdict {
    #[must_use]
    pub const fn delete_after(&self) -> Option<DateTime<Utc>> {
        match self {
            Self::Ineligible(_) => None,
            Self::Scheduled { delete_after, .. } | Self::Due { delete_after } => {
                Some(*delete_after)
            }
        }
    }
}

impl Policy {
    /// Decides one finding. `referenced` holds `namespace/name` for every
    /// claim a workload's Pod template names, which keeps a claim of a
    /// Deployment scaled to zero or a suspended `CronJob` alive.
    #[must_use]
    pub fn decide(&self, f: &Finding, referenced: &HashSet<String>, now: DateTime<Utc>) -> Verdict {
        if let Some(cause) = self.ineligible(f, referenced) {
            return Verdict::Ineligible(cause);
        }
        match existing_deadline(f) {
            Some(delete_after) if delete_after <= now => Verdict::Due { delete_after },
            Some(delete_after) => Verdict::Scheduled {
                delete_after,
                new: false,
            },
            None => {
                let since = f.unused_since.unwrap_or(now);
                Verdict::Scheduled {
                    delete_after: (since + delta(self.min_unused_age))
                        .max(now + delta(self.alert_before)),
                    new: true,
                }
            }
        }
    }

    fn ineligible(&self, f: &Finding, referenced: &HashSet<String>) -> Option<&'static str> {
        if f.kind != KIND_PVC || f.uid.is_empty() {
            return Some(CAUSE_NOT_A_CLAIM);
        }
        if !f.unused {
            return Some(CAUSE_IN_USE);
        }
        if !self.reasons.contains(&f.reason) {
            return Some(CAUSE_REASON_NOT_SELECTED);
        }
        if !f.reportable || f.age + self.alert_before < self.min_unused_age {
            return Some(CAUSE_TOO_RECENT);
        }
        if !self.namespace_in_scope(&f.namespace) {
            return Some(CAUSE_NAMESPACE_EXCLUDED);
        }
        if f.annotations
            .get(&key(KEY_ORPHAN_PVC_CLEANUP))
            .is_some_and(|v| v != ORPHAN_PVC_CLEANUP_ENABLED)
        {
            return Some(CAUSE_PROTECTED);
        }
        if f.owned {
            return Some(CAUSE_OWNED);
        }
        if referenced.contains(&f.key()) {
            return Some(CAUSE_REFERENCED);
        }
        None
    }

    fn namespace_in_scope(&self, namespace: &str) -> bool {
        if self.exclude_namespaces.iter().any(|n| n == namespace) {
            return false;
        }
        self.include_namespaces.is_empty() || self.include_namespaces.iter().any(|n| n == namespace)
    }
}

fn delta(d: Duration) -> chrono::TimeDelta {
    chrono::TimeDelta::from_std(d).unwrap_or(chrono::TimeDelta::MAX)
}

/// Reads when the alert before deletion was sent.
#[must_use]
pub fn existing_alerted_at(f: &Finding) -> Option<DateTime<Utc>> {
    f.annotations
        .get(&key(super::annotations::KEY_ORPHAN_PVC_ALERTED_AT))
        .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
        .map(|t| t.with_timezone(&Utc))
}

impl Policy {
    /// Whether a scheduled claim with this deadline is inside its alert
    /// window.
    #[must_use]
    pub fn alert_due(&self, delete_after: DateTime<Utc>, now: DateTime<Utc>) -> bool {
        delete_after - now
            <= chrono::TimeDelta::from_std(self.alert_before).unwrap_or(chrono::TimeDelta::MAX)
    }
}

/// Reads the persisted deadline. An unparseable one restarts the notice
/// rather than reading as due.
#[must_use]
pub fn existing_deadline(f: &Finding) -> Option<DateTime<Utc>> {
    f.annotations
        .get(&key(KEY_ORPHAN_PVC_DELETE_AFTER))
        .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
        .map(|t| t.with_timezone(&Utc))
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;
    use crate::pvscan::types::{KIND_PV, REASON_NO_CONSUMER_POD, REASON_UNBOUND};

    type Mutate = Box<dyn Fn(&mut Finding)>;

    fn policy() -> Policy {
        Policy {
            min_unused_age: Duration::from_hours(168),
            reasons: vec![REASON_NO_CONSUMER_POD.into()],
            include_namespaces: vec![],
            exclude_namespaces: vec!["kube-system".into()],
            max_deletions_per_pass: 10,
            alert_before: Duration::from_hours(6),
        }
    }

    #[test]
    fn alert_window() {
        let now = DateTime::from_timestamp(1_700_000_000, 0).unwrap();
        let p = policy();
        assert!(!p.alert_due(now + chrono::TimeDelta::hours(7), now));
        assert!(p.alert_due(now + chrono::TimeDelta::hours(6), now));
        assert!(p.alert_due(now + chrono::TimeDelta::hours(1), now));
        let mut f = candidate();
        assert_eq!(existing_alerted_at(&f), None);
        f.annotations.insert(
            key(super::super::annotations::KEY_ORPHAN_PVC_ALERTED_AT),
            "2023-11-14T22:13:20Z".into(),
        );
        assert_eq!(existing_alerted_at(&f), Some(now));
    }

    fn candidate() -> Finding {
        Finding {
            kind: KIND_PVC.into(),
            namespace: "legacy".into(),
            name: "uploads".into(),
            uid: "uid-1".into(),
            unused: true,
            reportable: true,
            reason: REASON_NO_CONSUMER_POD.into(),
            age: Duration::from_hours(200),
            ..Finding::default()
        }
    }

    #[test]
    fn eligible_claim_is_scheduled_then_due() {
        let now = DateTime::from_timestamp(1_700_000_000, 0).unwrap();
        let p = policy();
        let none = HashSet::new();
        let mut fresh = candidate();
        fresh.age = Duration::from_hours(162);
        fresh.unused_since = Some(now - chrono::TimeDelta::hours(162));
        assert_eq!(
            p.decide(&fresh, &none, now),
            Verdict::Scheduled {
                delete_after: now + chrono::TimeDelta::hours(6),
                new: true
            },
            "deleted when unused for minUnusedAge, scheduled alertBefore ahead"
        );
        fresh.age = Duration::from_hours(165);
        fresh.unused_since = Some(now - chrono::TimeDelta::hours(165));
        assert_eq!(
            p.decide(&fresh, &none, now),
            Verdict::Scheduled {
                delete_after: now + chrono::TimeDelta::hours(6),
                new: true
            },
            "a late first pass still leaves alertBefore of notice"
        );
        fresh.age = Duration::from_hours(161);
        fresh.unused_since = Some(now - chrono::TimeDelta::hours(161));
        assert_eq!(
            p.decide(&fresh, &none, now),
            Verdict::Ineligible(CAUSE_TOO_RECENT),
            "not yet inside the alertBefore window"
        );

        let mut old = candidate();
        old.unused_since = Some(now - chrono::TimeDelta::hours(200));
        assert_eq!(
            p.decide(&old, &none, now),
            Verdict::Scheduled {
                delete_after: now + chrono::TimeDelta::hours(6),
                new: true
            },
            "a claim long past minUnusedAge still gets alertBefore of notice"
        );

        let mut f = candidate();
        let later = now + chrono::TimeDelta::hours(3);
        f.annotations.insert(
            key(KEY_ORPHAN_PVC_DELETE_AFTER),
            later.to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
        );
        assert_eq!(
            p.decide(&f, &none, now),
            Verdict::Scheduled {
                delete_after: later,
                new: false
            }
        );
        assert_eq!(
            p.decide(&f, &none, later),
            Verdict::Due {
                delete_after: later
            }
        );
        assert_eq!(p.decide(&f, &none, later).delete_after(), Some(later));

        f.annotations
            .insert(key(KEY_ORPHAN_PVC_DELETE_AFTER), "garbage".into());
        assert!(
            matches!(
                p.decide(&f, &none, now),
                Verdict::Scheduled { new: true, .. }
            ),
            "an unparseable deadline restarts the notice"
        );
    }

    #[test]
    fn ineligible_causes() {
        let now = Utc::now();
        let p = policy();
        let none = HashSet::new();
        let cases: Vec<(Mutate, &str)> = vec![
            (Box::new(|f| f.kind = KIND_PV.into()), CAUSE_NOT_A_CLAIM),
            (Box::new(|f| f.uid.clear()), CAUSE_NOT_A_CLAIM),
            (Box::new(|f| f.unused = false), CAUSE_IN_USE),
            (
                Box::new(|f| f.reason = REASON_UNBOUND.into()),
                CAUSE_REASON_NOT_SELECTED,
            ),
            (
                Box::new(|f| f.age = Duration::from_hours(161)),
                CAUSE_TOO_RECENT,
            ),
            (Box::new(|f| f.reportable = false), CAUSE_TOO_RECENT),
            (
                Box::new(|f| f.namespace = "kube-system".into()),
                CAUSE_NAMESPACE_EXCLUDED,
            ),
            (
                Box::new(|f| {
                    f.annotations
                        .insert(key(KEY_ORPHAN_PVC_CLEANUP), "disabled".into());
                }),
                CAUSE_PROTECTED,
            ),
            (
                Box::new(|f| {
                    f.annotations
                        .insert(key(KEY_ORPHAN_PVC_CLEANUP), "disable".into());
                }),
                CAUSE_PROTECTED,
            ),
            (Box::new(|f| f.owned = true), CAUSE_OWNED),
        ];
        for (mutate, want) in cases {
            let mut f = candidate();
            mutate(&mut f);
            assert_eq!(
                p.decide(&f, &none, now),
                Verdict::Ineligible(want),
                "{want}"
            );
            assert_eq!(p.decide(&f, &none, now).delete_after(), None);
        }

        let refs = HashSet::from(["legacy/uploads".to_string()]);
        assert_eq!(
            p.decide(&candidate(), &refs, now),
            Verdict::Ineligible(CAUSE_REFERENCED)
        );

        let mut f = candidate();
        f.annotations = BTreeMap::from([(
            key(KEY_ORPHAN_PVC_CLEANUP),
            ORPHAN_PVC_CLEANUP_ENABLED.to_string(),
        )]);
        assert!(matches!(
            p.decide(&f, &none, now),
            Verdict::Scheduled { .. }
        ));
    }

    #[test]
    fn include_namespaces_narrow_the_scope() {
        let mut p = policy();
        p.include_namespaces = vec!["legacy".into()];
        assert!(p.namespace_in_scope("legacy"));
        assert!(!p.namespace_in_scope("app"));
        p.exclude_namespaces = vec!["legacy".into()];
        assert!(!p.namespace_in_scope("legacy"), "exclude wins");
    }
}
