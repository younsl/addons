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
//! first.
//!
//! Nothing a claim carries can bring its deletion forward. Anyone who can
//! patch a claim can edit its annotations, so the unused clock and the
//! deadline that count live in a [`State`] kept in a `ConfigMap` only the
//! controller writes. The claim's own `unused-since` can only push the clock
//! later, and its `orphan-pvc-delete-after` is shown to operators and has to
//! match the stored deadline before it is honored.

use std::collections::{BTreeMap, HashSet};
use std::time::Duration;

use chrono::{DateTime, SecondsFormat, Utc};

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

/// One claim's entry in the controller-owned state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Record {
    /// `namespace/name`, so the stored state reads on its own.
    pub claim: String,
    /// When the controller first saw the claim unused and eligible.
    pub first_seen: DateTime<Utc>,
    pub delete_after: Option<DateTime<Utc>>,
    /// When the one alert before deletion was sent.
    pub alerted_at: Option<DateTime<Utc>>,
}

/// The controller-owned state, keyed by claim UID.
pub type State = BTreeMap<String, Record>;

fn stamp(t: DateTime<Utc>) -> String {
    t.to_rfc3339_opts(SecondsFormat::Secs, true)
}

fn parse_time(v: &serde_json::Value) -> Option<DateTime<Utc>> {
    v.as_str()
        .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
        .map(|t| t.with_timezone(&Utc))
}

impl Record {
    /// Encodes the record as the JSON value of its `ConfigMap` key.
    #[must_use]
    pub fn encode(&self) -> String {
        let mut m = serde_json::Map::new();
        m.insert("claim".into(), self.claim.clone().into());
        m.insert("firstSeen".into(), stamp(self.first_seen).into());
        if let Some(t) = self.delete_after {
            m.insert("deleteAfter".into(), stamp(t).into());
        }
        if let Some(t) = self.alerted_at {
            m.insert("alertedAt".into(), stamp(t).into());
        }
        serde_json::Value::Object(m).to_string()
    }

    /// Decodes a stored record. One that does not parse is dropped, which
    /// restarts that claim's clock rather than guessing it.
    #[must_use]
    pub fn decode(raw: &str) -> Option<Self> {
        let v: serde_json::Value = serde_json::from_str(raw).ok()?;
        Some(Self {
            claim: v.get("claim")?.as_str()?.to_string(),
            first_seen: parse_time(v.get("firstSeen")?)?,
            delete_after: v.get("deleteAfter").and_then(parse_time),
            alerted_at: v.get("alertedAt").and_then(parse_time),
        })
    }
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
    /// Decides one finding and returns the record to keep for it, `None`
    /// when the claim leaves the state. `referenced` holds `namespace/name`
    /// for every claim a workload's Pod template names, which keeps a claim
    /// of a Deployment scaled to zero or a suspended `CronJob` alive.
    #[must_use]
    pub fn decide(
        &self,
        f: &Finding,
        referenced: &HashSet<String>,
        stored: Option<&Record>,
        now: DateTime<Utc>,
    ) -> (Verdict, Option<Record>) {
        if let Some(cause) = self.ineligible(f, referenced) {
            return (Verdict::Ineligible(cause), None);
        }
        let first_seen = stored
            .map(|r| r.first_seen)
            .filter(|t| *t <= now)
            .unwrap_or(now);
        let mut record = Record {
            claim: f.key(),
            first_seen,
            delete_after: None,
            alerted_at: None,
        };
        if let Some(r) = stored
            && let Some(deadline) = r.delete_after
            && existing_deadline(f) == Some(deadline)
        {
            record.delete_after = Some(deadline);
            record.alerted_at = r.alerted_at;
            let verdict = if deadline <= now {
                Verdict::Due {
                    delete_after: deadline,
                }
            } else {
                Verdict::Scheduled {
                    delete_after: deadline,
                    new: false,
                }
            };
            return (verdict, Some(record));
        }
        let since = f.unused_since.map_or(first_seen, |t| t.max(first_seen));
        let age = (now - since).to_std().unwrap_or_default();
        if age + self.alert_before < self.min_unused_age {
            return (Verdict::Ineligible(CAUSE_TOO_RECENT), Some(record));
        }
        let deadline = (since + delta(self.min_unused_age)).max(now + delta(self.alert_before));
        record.delete_after = Some(deadline);
        (
            Verdict::Scheduled {
                delete_after: deadline,
                new: true,
            },
            Some(record),
        )
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

    /// Whether a scheduled claim with this deadline is inside its alert
    /// window.
    #[must_use]
    pub fn alert_due(&self, delete_after: DateTime<Utc>, now: DateTime<Utc>) -> bool {
        delete_after - now <= delta(self.alert_before)
    }
}

fn delta(d: Duration) -> chrono::TimeDelta {
    chrono::TimeDelta::from_std(d).unwrap_or(chrono::TimeDelta::MAX)
}

/// Reads the deadline shown on the claim. It is a display copy: a deadline
/// counts only when it equals the stored one.
#[must_use]
pub fn existing_deadline(f: &Finding) -> Option<DateTime<Utc>> {
    f.annotations
        .get(&key(KEY_ORPHAN_PVC_DELETE_AFTER))
        .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
        .map(|t| t.with_timezone(&Utc))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pvscan::types::{KIND_PV, REASON_NO_CONSUMER_POD, REASON_UNBOUND};

    type Mutate = Box<dyn Fn(&mut Finding)>;

    fn now() -> DateTime<Utc> {
        DateTime::from_timestamp(1_700_000_000, 0).unwrap()
    }

    fn hours(h: i64) -> chrono::TimeDelta {
        chrono::TimeDelta::hours(h)
    }

    fn policy() -> Policy {
        Policy {
            min_unused_age: Duration::from_hours(72),
            reasons: vec![REASON_NO_CONSUMER_POD.into()],
            include_namespaces: vec![],
            exclude_namespaces: vec!["kube-system".into()],
            max_deletions_per_pass: 10,
            alert_before: Duration::from_hours(6),
        }
    }

    fn candidate(unused_hours: i64) -> Finding {
        Finding {
            kind: KIND_PVC.into(),
            namespace: "legacy".into(),
            name: "uploads".into(),
            uid: "uid-1".into(),
            unused: true,
            reportable: true,
            reason: REASON_NO_CONSUMER_POD.into(),
            unused_since: Some(now() - hours(unused_hours)),
            ..Finding::default()
        }
    }

    fn record(first_seen_hours_ago: i64, deadline: Option<DateTime<Utc>>) -> Record {
        Record {
            claim: "legacy/uploads".into(),
            first_seen: now() - hours(first_seen_hours_ago),
            delete_after: deadline,
            alerted_at: None,
        }
    }

    fn announce(f: &mut Finding, t: DateTime<Utc>) {
        f.annotations
            .insert(key(KEY_ORPHAN_PVC_DELETE_AFTER), stamp(t));
    }

    #[test]
    fn records_round_trip() {
        let mut r = record(3, Some(now()));
        r.alerted_at = Some(now() - hours(1));
        assert_eq!(Record::decode(&r.encode()), Some(r.clone()));
        r.delete_after = None;
        r.alerted_at = None;
        assert_eq!(Record::decode(&r.encode()), Some(r));
        assert_eq!(Record::decode("not json"), None);
        assert_eq!(Record::decode(r#"{"claim":"a/b"}"#), None);
    }

    #[test]
    fn the_first_pass_starts_the_controller_clock() {
        let none = HashSet::new();
        let (v, r) = policy().decide(&candidate(100), &none, None, now());
        assert_eq!(
            v,
            Verdict::Ineligible(CAUSE_TOO_RECENT),
            "a claim the controller never saw is not scheduled from its annotation"
        );
        assert_eq!(r.unwrap().first_seen, now());
    }

    #[test]
    fn scheduled_alert_before_ahead_and_due_at_min_unused_age() {
        let p = policy();
        let none = HashSet::new();
        let (v, r) = p.decide(&candidate(66), &none, Some(&record(66, None)), now());
        assert_eq!(
            v,
            Verdict::Scheduled {
                delete_after: now() + hours(6),
                new: true
            }
        );
        assert_eq!(r.unwrap().delete_after, Some(now() + hours(6)));

        let (v, _) = p.decide(&candidate(65), &none, Some(&record(65, None)), now());
        assert_eq!(v, Verdict::Ineligible(CAUSE_TOO_RECENT));

        let (v, _) = p.decide(&candidate(70), &none, Some(&record(70, None)), now());
        assert_eq!(
            v,
            Verdict::Scheduled {
                delete_after: now() + hours(6),
                new: true
            },
            "a late first schedule still leaves alertBefore"
        );

        let deadline = now() + hours(2);
        let mut f = candidate(70);
        announce(&mut f, deadline);
        let (v, r) = p.decide(&f, &none, Some(&record(70, Some(deadline))), now());
        assert_eq!(
            v,
            Verdict::Scheduled {
                delete_after: deadline,
                new: false
            }
        );
        assert_eq!(r.unwrap().delete_after, Some(deadline));
        let (v, _) = p.decide(&f, &none, Some(&record(70, Some(deadline))), deadline);
        assert_eq!(
            v,
            Verdict::Due {
                delete_after: deadline
            }
        );
    }

    #[test]
    fn tampered_annotations_cannot_bring_deletion_forward() {
        let p = policy();
        let none = HashSet::new();
        let mut f = candidate(10_000);
        announce(&mut f, now() - hours(1));
        let (v, _) = p.decide(&f, &none, Some(&record(1, None)), now());
        assert_eq!(
            v,
            Verdict::Ineligible(CAUSE_TOO_RECENT),
            "an old unused-since and a past deadline on the claim count for nothing"
        );

        let stored = now() + hours(5);
        let mut f = candidate(70);
        announce(&mut f, now() - hours(1));
        let (v, _) = p.decide(&f, &none, Some(&record(70, Some(stored))), now());
        assert_eq!(
            v,
            Verdict::Scheduled {
                delete_after: now() + hours(6),
                new: true
            },
            "a deadline that does not match the stored one reschedules"
        );

        let mut f = candidate(70);
        f.annotations.clear();
        let (v, _) = p.decide(&f, &none, Some(&record(70, Some(now() - hours(1)))), now());
        assert!(
            matches!(v, Verdict::Scheduled { new: true, .. }),
            "a removed deadline reschedules instead of deleting"
        );

        let (v, _) = p.decide(&candidate(1), &none, Some(&record(100, None)), now());
        assert_eq!(
            v,
            Verdict::Ineligible(CAUSE_TOO_RECENT),
            "a later unused-since pushes the clock back"
        );

        let mut future = record(0, None);
        future.first_seen = now() + hours(5);
        let (_, r) = p.decide(&candidate(1), &none, Some(&future), now());
        assert_eq!(
            r.unwrap().first_seen,
            now(),
            "a stored time in the future restarts at now"
        );
    }

    #[test]
    fn ineligible_causes() {
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
            let mut f = candidate(100);
            mutate(&mut f);
            let (v, r) = p.decide(&f, &none, Some(&record(100, None)), now());
            assert_eq!(v, Verdict::Ineligible(want), "{want}");
            assert_eq!(v.delete_after(), None);
            assert!(r.is_none(), "{want}: the claim leaves the state");
        }
        let refs = HashSet::from(["legacy/uploads".to_string()]);
        let (v, r) = p.decide(&candidate(100), &refs, Some(&record(100, None)), now());
        assert_eq!(v, Verdict::Ineligible(CAUSE_REFERENCED));
        assert!(r.is_none());

        let mut f = candidate(100);
        f.annotations.insert(
            key(KEY_ORPHAN_PVC_CLEANUP),
            ORPHAN_PVC_CLEANUP_ENABLED.into(),
        );
        let (v, _) = p.decide(&f, &none, Some(&record(100, None)), now());
        assert!(matches!(v, Verdict::Scheduled { .. }));
    }

    #[test]
    fn alert_window_and_scope() {
        let mut p = policy();
        assert!(!p.alert_due(now() + hours(7), now()));
        assert!(p.alert_due(now() + hours(6), now()));
        p.include_namespaces = vec!["legacy".into()];
        assert!(p.namespace_in_scope("legacy"));
        assert!(!p.namespace_in_scope("app"));
        p.exclude_namespaces = vec!["legacy".into()];
        assert!(!p.namespace_in_scope("legacy"), "exclude wins");
    }
}
