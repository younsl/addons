//! Turns one pass's per-instance outcomes into `InstanceVolumeAutoscaler`
//! status, with the same three conditions an HPA reports.

use std::collections::HashMap;

use chrono::{DateTime, Utc};

use crate::autoscaler::instance_volume::crd::{InstanceStatus, InstanceVolumeAutoscalerStatus};
use crate::autoscaler::{
    CONDITION_ABLE_TO_SCALE, CONDITION_SCALING_ACTIVE, CONDITION_SCALING_LIMITED, STATUS_FALSE,
    STATUS_TRUE, gib_quantity, latest, set_condition,
};
use crate::k8s::k8s_time;

pub use crate::autoscaler::{
    REASON_AMBIGUOUS, REASON_BELOW_TARGET, REASON_COOLDOWN, REASON_DRY_RUN,
    REASON_FAILED_GET_SCALE, REASON_FAILED_METRIC, REASON_FAILED_RESCALE, REASON_MAX_CAPACITY,
    REASON_READY_FOR_NEW_SCALE as REASON_READY, REASON_SCALING_DISABLED, REASON_SUCCEEDED_RESCALE,
    REASON_VALID_METRIC, REASON_WITHIN_RANGE, brief,
};
pub const REASON_NO_ROOT_VOLUME: &str = "NoRootVolume";
pub const REASON_NO_INSTANCES: &str = "NoMatchingInstances";

/// What happened to one instance during a pass.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Observation {
    pub capacity_gib: i32,
    pub usage: Option<i32>,
    pub desired_gib: Option<i32>,
    pub reason: &'static str,
    pub message: String,
    pub scaled_at: Option<DateTime<Utc>>,
    pub cooldown_until: Option<DateTime<Utc>>,
}

impl Observation {
    #[must_use]
    pub fn new(capacity_gib: i32) -> Self {
        Self {
            capacity_gib,
            ..Self::default()
        }
    }

    pub fn set(&mut self, reason: &'static str, message: impl Into<String>) {
        self.reason = reason;
        self.message = message.into();
    }

    /// The status entry for `inst`.
    #[must_use]
    pub fn entry(&self, inst: &crate::awsx::Instance) -> InstanceStatus {
        InstanceStatus {
            instance_id: inst.id.clone(),
            name: inst.name.clone(),
            volume_id: inst.root_volume_id.clone(),
            current_capacity: (self.capacity_gib > 0).then(|| gib_quantity(self.capacity_gib)),
            desired_capacity: self.desired_gib.map(gib_quantity),
            current_utilization: self.usage,
            last_scale_time: self.scaled_at.map(k8s_time),
            cooldown_until: self.cooldown_until.map(k8s_time),
            reason: self.reason.to_string(),
            message: brief(&self.message),
        }
    }
}

/// Why an autoscaler as a whole is not scaling, overriding what its
/// instances report.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Inactive {
    pub reason: &'static str,
    pub message: String,
}

/// Builds the next status from the previous one and this pass's entries.
#[must_use]
#[allow(clippy::too_many_lines)]
pub fn build(
    prev: Option<&InstanceVolumeAutoscalerStatus>,
    generation: Option<i64>,
    mut entries: Vec<InstanceStatus>,
    inactive: Option<Inactive>,
    now: DateTime<Utc>,
) -> InstanceVolumeAutoscalerStatus {
    let prev_times: HashMap<&str, _> = prev.map_or_default(|p| {
        p.instances
            .iter()
            .filter_map(|i| Some((i.instance_id.as_str(), i.last_scale_time.as_ref()?)))
            .collect()
    });
    entries.sort_by(|a, b| a.instance_id.cmp(&b.instance_id));
    let mut last = prev.and_then(|p| p.last_scale_time.clone());
    for e in &mut entries {
        if e.last_scale_time.is_none() {
            e.last_scale_time = prev_times.get(e.instance_id.as_str()).map(|t| (*t).clone());
        }
        last = latest(last, e.last_scale_time.as_ref());
    }

    let mut conditions = prev.map_or_default(|p| p.conditions.clone());
    let has = |reason: &str| entries.iter().filter(|e| e.reason == reason).count();
    let (active_status, active_reason, active_msg) = match inactive {
        Some(i) => (STATUS_FALSE, i.reason, brief(&i.message)),
        None if entries.is_empty() => (
            STATUS_FALSE,
            REASON_NO_INSTANCES,
            "no discovered instance matches scaleTargetRef".to_string(),
        ),
        None if has(REASON_FAILED_METRIC) == entries.len() => (
            STATUS_FALSE,
            REASON_FAILED_METRIC,
            "root filesystem usage could not be measured on any target instance".to_string(),
        ),
        None => (
            STATUS_TRUE,
            REASON_VALID_METRIC,
            format!(
                "measuring root filesystem usage of {} {}",
                entries.len(),
                if entries.len() == 1 {
                    "instance"
                } else {
                    "instances"
                }
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

    let (able_status, able_reason, able_msg) = if has(REASON_FAILED_RESCALE) > 0 {
        (
            STATUS_FALSE,
            REASON_FAILED_RESCALE,
            format!(
                "{} target volume(s) failed to grow",
                has(REASON_FAILED_RESCALE)
            ),
        )
    } else if has(REASON_COOLDOWN) > 0 {
        (
            STATUS_FALSE,
            REASON_COOLDOWN,
            format!(
                "{} target volume(s) wait for the modification cooldown",
                has(REASON_COOLDOWN)
            ),
        )
    } else if has(REASON_SUCCEEDED_RESCALE) > 0 {
        (
            STATUS_TRUE,
            REASON_SUCCEEDED_RESCALE,
            format!(
                "{} target volume(s) grown this pass",
                has(REASON_SUCCEEDED_RESCALE)
            ),
        )
    } else {
        (
            STATUS_TRUE,
            REASON_READY,
            "the controller can grow any target volume".to_string(),
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
            format!("{limited} target volume(s) would exceed maxCapacity"),
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

    InstanceVolumeAutoscalerStatus {
        observed_generation: generation,
        last_scale_time: last,
        current_volumes: i32::try_from(entries.iter().filter(|e| !e.volume_id.is_empty()).count())
            .unwrap_or(i32::MAX),
        instances: entries,
        conditions,
        ..InstanceVolumeAutoscalerStatus::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::autoscaler::MAX_MESSAGE_CHARS;
    use crate::autoscaler::condition;
    use crate::autoscaler::instance_volume::resizer::tests::instance;
    use crate::k8s::from_k8s_time;

    fn entry(id: &str, reason: &'static str) -> InstanceStatus {
        let mut o = Observation::new(100);
        o.set(reason, "");
        o.entry(&instance(id, 100))
    }

    fn cond(s: &InstanceVolumeAutoscalerStatus, t: &str) -> (String, String) {
        let c = condition(&s.conditions, t).unwrap();
        (c.status.clone(), c.reason.clone())
    }

    #[test]
    fn conditions_follow_outcomes() {
        let now = Utc::now();
        let s = build(None, Some(3), vec![], None, now);
        assert_eq!(s.observed_generation, Some(3));
        assert_eq!(s.current_volumes, 0);
        assert_eq!(
            cond(&s, CONDITION_SCALING_ACTIVE),
            (STATUS_FALSE.into(), REASON_NO_INSTANCES.into())
        );
        assert_eq!(
            cond(&s, CONDITION_ABLE_TO_SCALE),
            (STATUS_TRUE.into(), REASON_READY.into())
        );
        assert_eq!(
            cond(&s, CONDITION_SCALING_LIMITED),
            (STATUS_FALSE.into(), REASON_WITHIN_RANGE.into())
        );

        let s = build(
            None,
            None,
            vec![
                entry("i-2", REASON_FAILED_METRIC),
                entry("i-1", REASON_FAILED_METRIC),
            ],
            None,
            now,
        );
        assert_eq!(s.instances[0].instance_id, "i-1", "sorted");
        assert_eq!(cond(&s, CONDITION_SCALING_ACTIVE).1, REASON_FAILED_METRIC);

        let s = build(
            None,
            None,
            vec![
                entry("i-1", REASON_COOLDOWN),
                entry("i-2", REASON_MAX_CAPACITY),
                entry("i-3", REASON_SUCCEEDED_RESCALE),
            ],
            None,
            now,
        );
        assert_eq!(
            cond(&s, CONDITION_SCALING_ACTIVE),
            (STATUS_TRUE.into(), REASON_VALID_METRIC.into())
        );
        assert_eq!(
            cond(&s, CONDITION_ABLE_TO_SCALE),
            (STATUS_FALSE.into(), REASON_COOLDOWN.into())
        );
        assert_eq!(
            cond(&s, CONDITION_SCALING_LIMITED),
            (STATUS_TRUE.into(), REASON_MAX_CAPACITY.into())
        );

        let s = build(
            None,
            None,
            vec![
                entry("i-1", REASON_FAILED_RESCALE),
                entry("i-2", REASON_SUCCEEDED_RESCALE),
            ],
            None,
            now,
        );
        assert_eq!(cond(&s, CONDITION_ABLE_TO_SCALE).1, REASON_FAILED_RESCALE);
        let s = build(
            None,
            None,
            vec![entry("i-2", REASON_SUCCEEDED_RESCALE)],
            None,
            now,
        );
        assert_eq!(
            cond(&s, CONDITION_ABLE_TO_SCALE),
            (STATUS_TRUE.into(), REASON_SUCCEEDED_RESCALE.into())
        );

        let s = build(
            None,
            None,
            vec![entry("i-1", REASON_AMBIGUOUS)],
            Some(Inactive {
                reason: REASON_AMBIGUOUS,
                message: "overlaps b".into(),
            }),
            now,
        );
        assert_eq!(
            cond(&s, CONDITION_SCALING_ACTIVE),
            (STATUS_FALSE.into(), REASON_AMBIGUOUS.into())
        );
    }

    #[test]
    fn volumes_count_only_resolved_root_volumes() {
        let mut novol = instance("i-2", 0);
        novol.root_volume_id = String::new();
        let mut o = Observation::new(0);
        o.set(REASON_NO_ROOT_VOLUME, "");
        let s = build(
            None,
            None,
            vec![entry("i-1", REASON_BELOW_TARGET), o.entry(&novol)],
            None,
            Utc::now(),
        );
        assert_eq!(s.instances.len(), 2, "both are listed");
        assert_eq!(s.current_volumes, 1, "only the resolved root volume counts");
    }

    #[test]
    fn long_messages_are_cut() {
        assert_eq!(brief("short"), "short");
        let long = "x".repeat(300);
        let cut = brief(&long);
        assert_eq!(cut.chars().count(), MAX_MESSAGE_CHARS + 3);
        assert!(cut.ends_with("..."));
        let mut o = Observation::new(1);
        o.set(REASON_FAILED_RESCALE, long.clone());
        assert_eq!(o.entry(&instance("i-1", 1)).message, cut);
        let s = build(
            None,
            None,
            vec![],
            Some(Inactive {
                reason: REASON_FAILED_GET_SCALE,
                message: long,
            }),
            Utc::now(),
        );
        assert_eq!(
            condition(&s.conditions, CONDITION_SCALING_ACTIVE)
                .unwrap()
                .message,
            cut
        );
    }

    #[test]
    fn scale_times_carry_over() {
        let t0 = DateTime::from_timestamp(1_700_000_000, 0).unwrap();
        let t1 = DateTime::from_timestamp(1_700_100_000, 0).unwrap();
        let mut scaled = Observation::new(100);
        scaled.set(REASON_SUCCEEDED_RESCALE, "");
        scaled.scaled_at = Some(t0);
        scaled.desired_gib = Some(110);
        scaled.usage = Some(60);
        let first = build(
            None,
            None,
            vec![
                scaled.entry(&instance("i-1", 100)),
                entry("i-2", REASON_BELOW_TARGET),
            ],
            None,
            t0,
        );
        assert_eq!(
            first.last_scale_time.as_ref().and_then(from_k8s_time),
            Some(t0)
        );
        let e = &first.instances[0];
        assert_eq!(e.desired_capacity.as_ref().unwrap().0, "110Gi");
        assert_eq!(e.current_capacity.as_ref().unwrap().0, "100Gi");
        assert_eq!(e.current_utilization, Some(60));
        assert_eq!(e.volume_id, "vol-i-1");

        let second = build(
            Some(&first),
            None,
            vec![entry("i-1", REASON_BELOW_TARGET)],
            None,
            t1,
        );
        assert_eq!(
            second.instances[0]
                .last_scale_time
                .as_ref()
                .and_then(from_k8s_time),
            Some(t0),
            "kept from the previous pass"
        );
        assert_eq!(
            second.last_scale_time.as_ref().and_then(from_k8s_time),
            Some(t0)
        );
        let active = condition(&second.conditions, CONDITION_SCALING_ACTIVE).unwrap();
        assert_eq!(from_k8s_time(&active.last_transition_time), Some(t0));

        assert!(
            Observation::new(0)
                .entry(&instance("i-3", 0))
                .current_capacity
                .is_none()
        );
    }
}
