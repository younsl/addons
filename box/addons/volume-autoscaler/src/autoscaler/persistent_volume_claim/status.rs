//! Turns one reconcile's per-claim outcomes into
//! `PersistentVolumeClaimAutoscaler` status, with the same three conditions
//! an HPA reports.

use std::collections::HashMap;

use chrono::{DateTime, Utc};

use crate::autoscaler::persistent_volume_claim::crd::{
    ClaimStatus, PersistentVolumeClaimAutoscalerStatus,
};
use crate::autoscaler::persistent_volume_claim::policy::{
    Claim, Outcome, REASON_EXPANSION_NOT_ALLOWED,
};
use crate::autoscaler::{
    CONDITION_ABLE_TO_SCALE, CONDITION_SCALING_ACTIVE, CONDITION_SCALING_LIMITED, GIB,
    REASON_COOLDOWN, REASON_FAILED_GET_SCALE, REASON_FAILED_METRIC, REASON_FAILED_RESCALE,
    REASON_MAX_CAPACITY, REASON_READY_FOR_NEW_SCALE, REASON_SUCCEEDED_RESCALE, REASON_VALID_METRIC,
    REASON_WITHIN_RANGE, STATUS_FALSE, STATUS_TRUE, brief, gib_quantity, latest, set_condition,
};
use crate::k8s::k8s_time;

/// Why an autoscaler as a whole is not scaling, overriding what its claims
/// report.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Inactive {
    pub reason: &'static str,
    pub message: String,
}

#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn bytes_quantity(b: f64) -> k8s_openapi::apimachinery::pkg::api::resource::Quantity {
    k8s_openapi::apimachinery::pkg::api::resource::Quantity(format!("{}", b.max(0.0) as u64))
}

/// The status entry for `claim`.
#[must_use]
pub fn entry(claim: &Claim, o: &Outcome, scaled_at: Option<DateTime<Utc>>) -> ClaimStatus {
    ClaimStatus {
        name: claim.name.clone(),
        current_capacity: claim.capacity_bytes.map(|b| {
            #[allow(clippy::cast_possible_truncation)]
            let gib = (b / GIB).ceil().min(f64::from(i32::MAX)) as i32;
            gib_quantity(gib)
        }),
        desired_capacity: o.desired_gib.map(gib_quantity),
        current_utilization: o.storage,
        current_inodes_utilization: o.inodes,
        last_scale_time: scaled_at.map(k8s_time),
        cooldown_until: o.cooldown_until.map(k8s_time),
        capacity_at_last_scale: o.capacity_at_last_scale.map(bytes_quantity),
        tracked_by: claim.tracked_by.clone(),
        reason: o.reason.to_string(),
        message: brief(&o.message),
    }
}

/// Marks the autoscaler unable to scale because a `StorageClass` it depends
/// on does not allow expansion. CEL cannot see other objects, so this is the
/// check's only surface.
pub fn block_expansion(
    status: &mut PersistentVolumeClaimAutoscalerStatus,
    blockers: &[String],
    generation: Option<i64>,
    now: DateTime<Utc>,
) {
    set_condition(
        &mut status.conditions,
        CONDITION_ABLE_TO_SCALE,
        STATUS_FALSE,
        REASON_EXPANSION_NOT_ALLOWED,
        brief(&blockers.join("; ")),
        generation,
        now,
    );
}

const fn plural(n: usize) -> &'static str {
    if n == 1 { "claim" } else { "claims" }
}

/// Builds the next status from the previous one and this reconcile's entries.
#[must_use]
#[allow(clippy::too_many_lines)]
pub fn build(
    prev: Option<&PersistentVolumeClaimAutoscalerStatus>,
    generation: Option<i64>,
    mut entries: Vec<ClaimStatus>,
    inactive: Option<Inactive>,
    now: DateTime<Utc>,
) -> PersistentVolumeClaimAutoscalerStatus {
    let prev_times: HashMap<&str, _> = prev
        .map(|p| {
            p.claims
                .iter()
                .filter_map(|c| Some((c.name.as_str(), c.last_scale_time.as_ref()?)))
                .collect()
        })
        .unwrap_or_default();
    entries.sort_by(|a, b| a.name.cmp(&b.name));
    let mut last = prev.and_then(|p| p.last_scale_time.clone());
    for e in &mut entries {
        if e.last_scale_time.is_none() {
            e.last_scale_time = prev_times.get(e.name.as_str()).map(|t| (*t).clone());
        }
        last = latest(last, e.last_scale_time.as_ref());
    }

    let mut conditions = prev.map(|p| p.conditions.clone()).unwrap_or_default();
    let has = |reason: &str| entries.iter().filter(|e| e.reason == reason).count();
    let target_missing = inactive
        .as_ref()
        .is_some_and(|i| i.reason == REASON_FAILED_GET_SCALE);
    let (active_status, active_reason, active_msg) = match &inactive {
        Some(i) => (STATUS_FALSE, i.reason, brief(&i.message)),
        None if entries.is_empty() => (
            STATUS_FALSE,
            REASON_FAILED_GET_SCALE,
            "scaleTargetRef selects no PersistentVolumeClaim".to_string(),
        ),
        None if has(REASON_FAILED_METRIC) == entries.len() => (
            STATUS_FALSE,
            REASON_FAILED_METRIC,
            "kubelet_volume_stats has no series for any target claim".to_string(),
        ),
        None => (
            STATUS_TRUE,
            REASON_VALID_METRIC,
            format!(
                "measuring utilization of {} {}",
                entries.len(),
                plural(entries.len())
            ),
        ),
    };
    set_condition(
        &mut conditions,
        CONDITION_SCALING_ACTIVE,
        active_status,
        active_reason,
        active_msg,
        generation,
        now,
    );

    let (able_status, able_reason, able_msg) = if target_missing {
        (
            STATUS_FALSE,
            REASON_FAILED_GET_SCALE,
            inactive.map(|i| brief(&i.message)).unwrap_or_default(),
        )
    } else if has(REASON_FAILED_RESCALE) > 0 {
        (
            STATUS_FALSE,
            REASON_FAILED_RESCALE,
            format!(
                "{} {} failed to resize",
                has(REASON_FAILED_RESCALE),
                plural(has(REASON_FAILED_RESCALE))
            ),
        )
    } else if has(REASON_COOLDOWN) > 0 {
        (
            STATUS_FALSE,
            REASON_COOLDOWN,
            format!(
                "{} {} wait for the cooldown",
                has(REASON_COOLDOWN),
                plural(has(REASON_COOLDOWN))
            ),
        )
    } else if has(REASON_SUCCEEDED_RESCALE) > 0 {
        (
            STATUS_TRUE,
            REASON_SUCCEEDED_RESCALE,
            format!(
                "{} {} resized",
                has(REASON_SUCCEEDED_RESCALE),
                plural(has(REASON_SUCCEEDED_RESCALE))
            ),
        )
    } else {
        (
            STATUS_TRUE,
            REASON_READY_FOR_NEW_SCALE,
            "the controller can resize any target claim".to_string(),
        )
    };
    set_condition(
        &mut conditions,
        CONDITION_ABLE_TO_SCALE,
        able_status,
        able_reason,
        able_msg,
        generation,
        now,
    );

    let limited = has(REASON_MAX_CAPACITY);
    let (lim_status, lim_reason, lim_msg) = if limited > 0 {
        (
            STATUS_TRUE,
            REASON_MAX_CAPACITY,
            format!("{limited} {} at maxCapacity", plural(limited)),
        )
    } else {
        (
            STATUS_FALSE,
            REASON_WITHIN_RANGE,
            "every desired capacity is within maxCapacity".to_string(),
        )
    };
    set_condition(
        &mut conditions,
        CONDITION_SCALING_LIMITED,
        lim_status,
        lim_reason,
        lim_msg,
        generation,
        now,
    );

    PersistentVolumeClaimAutoscalerStatus {
        observed_generation: generation,
        last_scale_time: last,
        current_volumes: i32::try_from(entries.len()).unwrap_or(i32::MAX),
        claims: entries,
        conditions,
        ..PersistentVolumeClaimAutoscalerStatus::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::autoscaler::persistent_volume_claim::policy::tests::claim;
    use crate::autoscaler::{
        REASON_AMBIGUOUS, REASON_BELOW_TARGET, REASON_SCALING_DISABLED, condition,
    };

    fn outcome(reason: &'static str) -> Outcome {
        Outcome {
            reason,
            message: String::new(),
            storage: Some(50),
            inodes: None,
            current_gib: Some(10),
            desired_gib: None,
            cooldown_until: None,
            capacity_at_last_scale: None,
        }
    }

    fn e(name: &str, reason: &'static str) -> ClaimStatus {
        entry(&claim(name, 10.0), &outcome(reason), None)
    }

    fn cond(s: &PersistentVolumeClaimAutoscalerStatus, t: &str) -> (String, String) {
        let c = condition(&s.conditions, t).unwrap();
        (c.status.clone(), c.reason.clone())
    }

    fn pair(a: &str, b: &str) -> (String, String) {
        (a.into(), b.into())
    }

    #[test]
    fn healthy_and_resized() {
        let now = Utc::now();
        let mut o = outcome(REASON_SUCCEEDED_RESCALE);
        o.desired_gib = Some(11);
        let resized = entry(&claim("b", 10.0), &o, Some(now));
        assert_eq!(resized.current_capacity, Some(gib_quantity(10)));
        assert_eq!(resized.desired_capacity, Some(gib_quantity(11)));
        let s = build(
            None,
            Some(2),
            vec![resized, e("a", REASON_BELOW_TARGET)],
            None,
            now,
        );
        assert_eq!(s.claims[0].name, "a", "sorted");
        assert_eq!(s.current_volumes, 2);
        assert_eq!(s.observed_generation, Some(2));
        assert_eq!(s.last_scale_time, Some(k8s_time(now)));
        assert_eq!(
            cond(&s, CONDITION_SCALING_ACTIVE),
            pair(STATUS_TRUE, REASON_VALID_METRIC)
        );
        assert_eq!(
            cond(&s, CONDITION_ABLE_TO_SCALE),
            pair(STATUS_TRUE, REASON_SUCCEEDED_RESCALE)
        );
        assert_eq!(
            cond(&s, CONDITION_SCALING_LIMITED),
            pair(STATUS_FALSE, REASON_WITHIN_RANGE)
        );

        let next = build(
            Some(&s),
            Some(2),
            vec![e("b", REASON_BELOW_TARGET)],
            None,
            now,
        );
        assert_eq!(
            next.claims[0].last_scale_time,
            Some(k8s_time(now)),
            "kept from prev"
        );
        assert_eq!(
            cond(&next, CONDITION_ABLE_TO_SCALE),
            pair(STATUS_TRUE, REASON_READY_FOR_NEW_SCALE)
        );
    }

    #[test]
    fn degraded_states() {
        let now = Utc::now();
        let s = build(
            None,
            None,
            vec![e("a", REASON_FAILED_RESCALE), e("b", REASON_MAX_CAPACITY)],
            None,
            now,
        );
        assert_eq!(
            cond(&s, CONDITION_ABLE_TO_SCALE),
            pair(STATUS_FALSE, REASON_FAILED_RESCALE)
        );
        assert_eq!(
            cond(&s, CONDITION_SCALING_LIMITED),
            pair(STATUS_TRUE, REASON_MAX_CAPACITY)
        );

        let s = build(None, None, vec![e("a", REASON_COOLDOWN)], None, now);
        assert_eq!(
            cond(&s, CONDITION_ABLE_TO_SCALE),
            pair(STATUS_FALSE, REASON_COOLDOWN)
        );

        let s = build(None, None, vec![e("a", REASON_FAILED_METRIC)], None, now);
        assert_eq!(
            cond(&s, CONDITION_SCALING_ACTIVE),
            pair(STATUS_FALSE, REASON_FAILED_METRIC)
        );

        let s = build(None, None, vec![], None, now);
        assert_eq!(
            cond(&s, CONDITION_SCALING_ACTIVE),
            pair(STATUS_FALSE, REASON_FAILED_GET_SCALE)
        );

        let missing = Inactive {
            reason: REASON_FAILED_GET_SCALE,
            message: "StatefulSet web not found".into(),
        };
        let s = build(None, None, vec![], Some(missing), now);
        assert_eq!(
            cond(&s, CONDITION_ABLE_TO_SCALE),
            pair(STATUS_FALSE, REASON_FAILED_GET_SCALE)
        );
        assert_eq!(
            condition(&s.conditions, CONDITION_ABLE_TO_SCALE)
                .unwrap()
                .message,
            "StatefulSet web not found"
        );

        for reason in [REASON_AMBIGUOUS, REASON_SCALING_DISABLED] {
            let s = build(
                None,
                None,
                vec![e("a", REASON_BELOW_TARGET)],
                Some(Inactive {
                    reason,
                    message: "x".into(),
                }),
                now,
            );
            assert_eq!(
                cond(&s, CONDITION_SCALING_ACTIVE),
                pair(STATUS_FALSE, reason)
            );
            assert_eq!(
                cond(&s, CONDITION_ABLE_TO_SCALE),
                pair(STATUS_TRUE, REASON_READY_FOR_NEW_SCALE)
            );
        }
    }
}
