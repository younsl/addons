//! The two autoscalers of the `autoscaling.volume-autoscaler.sh` group, and
//! the API types and helpers they share: quantities, cooldowns, the HPA-style
//! conditions, and the `behavior.scaleUp` rules.

pub mod instance_volume;
pub mod persistent_volume_claim;

use std::fmt::Write as _;

use chrono::{DateTime, Utc};
use k8s_openapi::apiextensions_apiserver::pkg::apis::apiextensions::v1::CustomResourceDefinition;
use k8s_openapi::apimachinery::pkg::api::resource::Quantity;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::{Condition, Time};
use kube::{CustomResourceExt as _, KubeSchema};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::k8s::{from_k8s_time, k8s_time};

pub const API_VERSION: &str = "autoscaling.volume-autoscaler.sh/v1alpha1";
/// The Kubernetes quantity grammar, as `controller-gen` emits it for a
/// `resource.Quantity` field. Quantity's own schema carries no pattern.
pub const QUANTITY_PATTERN: &str = r"^(\+|-)?(([0-9]+(\.[0-9]*)?)|(\.[0-9]+))(([KMGTPE]i)|[numkMGTPE]|([eE](\+|-)?(([0-9]+(\.[0-9]*)?)|(\.[0-9]+))))?$";

/// A custom resource of the group, as `crdgen` names it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
pub enum CrdKind {
    InstanceVolumeAutoscaler,
    PersistentVolumeClaimAutoscaler,
}

impl CrdKind {
    pub const ALL: [Self; 2] = [
        Self::InstanceVolumeAutoscaler,
        Self::PersistentVolumeClaimAutoscaler,
    ];

    /// The `CustomResourceDefinition` kube derives from the Rust types.
    #[must_use]
    pub fn crd(self) -> CustomResourceDefinition {
        match self {
            Self::InstanceVolumeAutoscaler => instance_volume::crd::InstanceVolumeAutoscaler::crd(),
            Self::PersistentVolumeClaimAutoscaler => {
                persistent_volume_claim::crd::PersistentVolumeClaimAutoscaler::crd()
            }
        }
    }
}

/// Field manager for every status write.
pub const FIELD_MANAGER: &str = "volume-autoscaler";

/// The default utilization target when `metrics` is empty, as in an HPA
/// without metrics.
pub const DEFAULT_UTILIZATION_PERCENT: i32 = 80;
/// The default step policies when `behavior.scaleUp.policies` is empty: grow
/// by 10 percent, at least 1 GiB.
pub const DEFAULT_STEP_PERCENT: i32 = 10;
pub const DEFAULT_STEP_GIB: i32 = 1;

pub const CONDITION_ABLE_TO_SCALE: &str = "AbleToScale";
pub const CONDITION_SCALING_ACTIVE: &str = "ScalingActive";
pub const CONDITION_SCALING_LIMITED: &str = "ScalingLimited";

pub const STATUS_TRUE: &str = "True";
pub const STATUS_FALSE: &str = "False";
pub const STATUS_UNKNOWN: &str = "Unknown";

/// The metric source type. Only `Resource` is supported.
#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq, JsonSchema)]
pub enum MetricSourceType {
    Resource,
}

/// The metric target type. Only `Utilization` is supported.
#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq, JsonSchema)]
pub enum MetricTargetType {
    Utilization,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct MetricTarget {
    /// `Utilization`.
    #[serde(rename = "type")]
    pub type_: MetricTargetType,
    /// Usage percent at or above which the volume grows.
    #[schemars(range(min = 1, max = 100))]
    pub average_utilization: i32,
}

/// Scaling behavior. Volumes never shrink, so there is no `scaleDown`.
#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct Behavior {
    /// Scale-up rules. Omitted means `Max` of 10 percent and 1 GiB.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(extend("default" = {}))]
    pub scale_up: Option<ScalingRules>,
}

/// How one step is sized. EBS allows one modification per volume every six
/// hours, so there is no `periodSeconds`.
#[derive(KubeSchema, Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ScalingRules {
    /// `Max` (default) takes the largest policy step, `Min` the smallest,
    /// `Disabled` stops scaling for this autoscaler.
    #[serde(default)]
    pub select_policy: SelectPolicy,
    /// Step policies. Empty means 10 percent and 1 GiB with `Max`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub policies: Vec<ScalingPolicy>,
    /// Minimum time between two modifications of the same volume, in hours,
    /// minutes, and seconds (`6h`, `6h30m`, `90m`). At least `6h`, the EBS
    /// limit of one modification per volume every six hours, which is also
    /// the default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(regex(pattern = r"^([0-9]+(h|m|s))+$"), extend("default" = "6h"))]
    #[x_kube(validation = Rule::new("duration(self) >= duration('6h')").message("cooldownDuration must be at least 6h, the EBS limit of one modification per volume every six hours"))]
    pub cooldown_duration: Option<String>,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq, JsonSchema)]
pub enum SelectPolicy {
    #[default]
    Max,
    Min,
    Disabled,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq, JsonSchema)]
pub enum ScalingPolicyType {
    /// `value` percent of the current capacity, rounded up to whole GiB.
    Percent,
    /// `value` GiB.
    GiB,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ScalingPolicy {
    /// `Percent` of the current capacity, or a fixed `GiB` amount.
    #[serde(rename = "type")]
    pub type_: ScalingPolicyType,
    /// The step size in the unit `type` names.
    #[schemars(range(min = 1))]
    pub value: i32,
}

/// One step policy, normalized.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Step {
    Percent(i32),
    GiB(i32),
}

/// Normalizes `behavior.scaleUp.policies`. Empty means 10 percent and 1 GiB.
pub fn compile_steps(policies: &[ScalingPolicy]) -> Result<Vec<Step>, String> {
    if policies.is_empty() {
        return Ok(vec![
            Step::Percent(DEFAULT_STEP_PERCENT),
            Step::GiB(DEFAULT_STEP_GIB),
        ]);
    }
    policies
        .iter()
        .map(|p| {
            if p.value <= 0 {
                return Err(format!(
                    "scaleUp policy value must be positive, got {}",
                    p.value
                ));
            }
            Ok(match p.type_ {
                ScalingPolicyType::Percent => Step::Percent(p.value),
                ScalingPolicyType::GiB => Step::GiB(p.value),
            })
        })
        .collect()
}

/// The capacity in GiB after one scale-up step from `current`. Each policy
/// proposes a step (`Percent` rounds up to whole GiB), and the largest wins
/// unless `select_min`. The result is always at least one GiB larger.
#[must_use]
pub fn step_up(current: i32, steps: &[Step], select_min: bool) -> i32 {
    let current = i64::from(current);
    let sizes = steps.iter().map(|s| match *s {
        Step::Percent(p) => (current * i64::from(p) + 99) / 100,
        Step::GiB(g) => i64::from(g),
    });
    let step = if select_min { sizes.min() } else { sizes.max() }.unwrap_or(0);
    i32::try_from((current + step).max(current + 1)).unwrap_or(i32::MAX)
}

/// EBS allows one modification per volume every six hours, so no cooldown can
/// be shorter.
pub const MIN_COOLDOWN: std::time::Duration = std::time::Duration::from_hours(6);

/// Parses a cooldown in hours, minutes, and seconds, such as `6h30m`.
pub fn parse_cooldown(raw: &str) -> Result<std::time::Duration, String> {
    let mut total = 0u64;
    let mut digits = String::new();
    for c in raw.chars() {
        if c.is_ascii_digit() {
            digits.push(c);
            continue;
        }
        let unit = match c {
            'h' => 3600,
            'm' => 60,
            's' => 1,
            _ => {
                return Err(format!(
                    "invalid duration {raw:?}: use h, m, and s such as 6h30m"
                ));
            }
        };
        let n: u64 = digits
            .parse()
            .map_err(|_| format!("invalid duration {raw:?}: use h, m, and s such as 6h30m"))?;
        total = total.saturating_add(n.saturating_mul(unit));
        digits.clear();
    }
    if raw.is_empty() || !digits.is_empty() {
        return Err(format!(
            "invalid duration {raw:?}: use h, m, and s such as 6h30m"
        ));
    }
    Ok(std::time::Duration::from_secs(total))
}

// Per-target and condition reasons both autoscalers report.
pub const REASON_BELOW_TARGET: &str = "BelowTarget";
pub const REASON_SUCCEEDED_RESCALE: &str = "SucceededRescale";
pub const REASON_FAILED_RESCALE: &str = "FailedRescale";
pub const REASON_COOLDOWN: &str = "Cooldown";
pub const REASON_MAX_CAPACITY: &str = "MaxCapacityReached";
pub const REASON_SCALING_DISABLED: &str = "ScalingDisabled";
pub const REASON_DRY_RUN: &str = "DryRun";
pub const REASON_FAILED_METRIC: &str = "FailedGetResourceMetric";
pub const REASON_FAILED_GET_SCALE: &str = "FailedGetScale";
pub const REASON_AMBIGUOUS: &str = "AmbiguousSelector";
pub const REASON_VALID_METRIC: &str = "ValidMetricFound";
pub const REASON_READY_FOR_NEW_SCALE: &str = "ReadyForNewScale";
pub const REASON_WITHIN_RANGE: &str = "DesiredWithinRange";

/// The longest message written into status. SDK errors carry the whole HTTP
/// response, which belongs in the logs rather than in `kubectl describe`.
pub const MAX_MESSAGE_CHARS: usize = 256;

/// Cuts `s` to `MAX_MESSAGE_CHARS`, marking the cut.
#[must_use]
pub fn brief(s: &str) -> String {
    if s.chars().count() <= MAX_MESSAGE_CHARS {
        return s.to_string();
    }
    let mut out: String = s.chars().take(MAX_MESSAGE_CHARS).collect();
    out.push_str("...");
    out
}

/// The `reason` an autoscaler shows when every condition is healthy.
pub const REASON_READY: &str = "Ready";

/// Picks the reason an operator should look at first: an inactive autoscaler,
/// then one unable to scale, then one at its ceiling.
#[must_use]
pub fn summary_reason(conditions: &[Condition]) -> String {
    if conditions.is_empty() {
        return String::new();
    }
    let unless = |t: &str, healthy: &str| {
        condition(conditions, t)
            .filter(|c| c.status != healthy)
            .map(|c| c.reason.clone())
    };
    unless(CONDITION_SCALING_ACTIVE, STATUS_TRUE)
        .or_else(|| unless(CONDITION_ABLE_TO_SCALE, STATUS_TRUE))
        .or_else(|| unless(CONDITION_SCALING_LIMITED, STATUS_FALSE))
        .unwrap_or_else(|| REASON_READY.to_string())
}

/// Renders a cooldown with only its non-zero units, as `6h`, `6h30m`, or
/// `1h0m30s`.
#[must_use]
pub fn format_cooldown(d: std::time::Duration) -> String {
    let s = d.as_secs();
    let parts = [(s / 3600, 'h'), ((s % 3600) / 60, 'm'), (s % 60, 's')];
    let mut out = String::new();
    for (n, u) in parts.iter().filter(|(n, _)| *n > 0) {
        let _ = write!(out, "{n}{u}");
    }
    if out.is_empty() { "0s".into() } else { out }
}

/// Renders whole GiB as a Kubernetes quantity.
#[must_use]
pub fn gib_quantity(gib: i32) -> Quantity {
    Quantity(format!("{gib}Gi"))
}

pub const GIB: f64 = 1_073_741_824.0;

/// Parses a quantity into bytes. Accepts the binary (`Ki` to `Ei`) and
/// decimal (`n` to `E`) suffixes, a decimal exponent (`1e12`), and plain
/// bytes.
pub fn quantity_bytes(q: &Quantity) -> Result<f64, String> {
    let raw = q.0.trim();
    let split = raw
        .find(|c: char| !(c.is_ascii_digit() || c == '.'))
        .unwrap_or(raw.len());
    let (num, suffix) = raw.split_at(split);
    let value: f64 = num
        .parse()
        .map_err(|_| format!("invalid quantity {raw:?}"))?;
    let multiplier: f64 = match suffix {
        "" => 1.0,
        "Ki" => 1024.0,
        "Mi" => 1024f64.powi(2),
        "Gi" => GIB,
        "Ti" => 1024f64.powi(4),
        "Pi" => 1024f64.powi(5),
        "Ei" => 1024f64.powi(6),
        "n" => 1e-9,
        "u" => 1e-6,
        "m" => 1e-3,
        "k" => 1e3,
        "M" => 1e6,
        "G" => 1e9,
        "T" => 1e12,
        "P" => 1e15,
        "E" => 1e18,
        exp if exp.len() > 1 && exp.starts_with(['e', 'E']) => exp[1..]
            .parse::<i32>()
            .map(|e| 10f64.powi(e))
            .map_err(|_| format!("invalid quantity exponent in {raw:?}"))?,
        other => return Err(format!("unsupported quantity suffix {other:?} in {raw:?}")),
    };
    Ok(value * multiplier)
}

/// Parses a quantity and rounds it up to whole GiB.
pub fn quantity_gib(q: &Quantity) -> Result<i32, String> {
    let gib = (quantity_bytes(q)? / GIB).ceil();
    if !(gib >= 0.0 && gib <= f64::from(i32::MAX)) {
        return Err(format!("quantity {:?} out of range", q.0.trim()));
    }
    #[allow(clippy::cast_possible_truncation)]
    Ok(gib as i32)
}

/// Inserts or updates the condition of `type_`. `lastTransitionTime` moves only
/// when the status changes, as the API conventions require.
pub fn set_condition(
    conditions: &mut Vec<Condition>,
    type_: &str,
    status: &str,
    reason: &str,
    message: String,
    generation: Option<i64>,
    now: DateTime<Utc>,
) {
    let kept = conditions
        .iter()
        .position(|c| c.type_ == type_)
        .map(|i| conditions.remove(i))
        .filter(|c| c.status == status);
    let last = kept.map_or_else(|| k8s_time(now), |c| c.last_transition_time);
    conditions.push(Condition {
        type_: type_.into(),
        status: status.into(),
        reason: reason.into(),
        message,
        observed_generation: generation,
        last_transition_time: last,
    });
    conditions.sort_by(|a, b| a.type_.cmp(&b.type_));
}

/// The condition of `type_`, if present.
#[must_use]
pub fn condition<'a>(conditions: &'a [Condition], type_: &str) -> Option<&'a Condition> {
    conditions.iter().find(|c| c.type_ == type_)
}

/// The later of two optional times.
#[must_use]
pub fn latest(a: Option<Time>, b: Option<&Time>) -> Option<Time> {
    match (a, b) {
        (Some(a), Some(b)) => {
            if from_k8s_time(b) > from_k8s_time(&a) {
                Some(b.clone())
            } else {
                Some(a)
            }
        }
        (a, b) => a.or_else(|| b.cloned()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quantities() {
        let q = |s: &str| quantity_gib(&Quantity(s.into()));
        assert_eq!(q("100Gi").unwrap(), 100);
        assert_eq!(q("2Ti").unwrap(), 2048);
        assert_eq!(q("1.5Gi").unwrap(), 2, "rounds up");
        assert_eq!(q("5120Mi").unwrap(), 5);
        assert_eq!(q("5121Mi").unwrap(), 6);
        assert_eq!(q("100G").unwrap(), 94);
        assert_eq!(q("1073741824").unwrap(), 1);
        assert_eq!(q(" 1Ki ").unwrap(), 1);
        assert_eq!(q("0").unwrap(), 0);
        assert_eq!(q("1e12").unwrap(), 932, "decimal exponent");
        assert_eq!(q("2E+9").unwrap(), 2);
        assert_eq!(
            q("100m").unwrap(),
            1,
            "milli parses, the 1Gi floor rejects it later"
        );
        for bad in ["", "Gi", "10GB", "1ex", "-1Gi", "99999999Pi"] {
            assert!(q(bad).is_err(), "{bad}");
        }
        assert_eq!(gib_quantity(110).0, "110Gi");
    }

    #[test]
    fn conditions_keep_transition_time_while_status_holds() {
        let t0 = DateTime::from_timestamp(1_700_000_000, 0).unwrap();
        let t1 = DateTime::from_timestamp(1_700_000_600, 0).unwrap();
        let mut c = Vec::new();
        set_condition(&mut c, "A", STATUS_TRUE, "X", "m".into(), Some(1), t0);
        set_condition(&mut c, "A", STATUS_TRUE, "Y", "n".into(), Some(2), t1);
        assert_eq!(c.len(), 1);
        assert_eq!(c[0].reason, "Y");
        assert_eq!(c[0].observed_generation, Some(2));
        assert_eq!(from_k8s_time(&c[0].last_transition_time), Some(t0));
        set_condition(&mut c, "A", STATUS_FALSE, "Z", String::new(), None, t1);
        assert_eq!(from_k8s_time(&c[0].last_transition_time), Some(t1));
        assert!(condition(&c, "A").is_some());
        assert!(condition(&c, "B").is_none());
    }

    #[test]
    fn latest_picks_the_later_time() {
        let a = k8s_time(DateTime::from_timestamp(10, 0).unwrap());
        let b = k8s_time(DateTime::from_timestamp(20, 0).unwrap());
        assert_eq!(latest(Some(a.clone()), Some(&b)), Some(b.clone()));
        assert_eq!(latest(Some(b.clone()), Some(&a)), Some(b));
        assert_eq!(latest(None, Some(&a)), Some(a.clone()));
        assert_eq!(latest(Some(a.clone()), None), Some(a));
        assert_eq!(latest(None, None), None);
    }

    #[test]
    fn summary_reason_picks_the_most_actionable() {
        let now = Utc::now();
        let mut c = Vec::new();
        let set = |c: &mut Vec<Condition>, t: &str, st: &str, r: &str| {
            set_condition(c, t, st, r, String::new(), None, now);
        };
        set(
            &mut c,
            CONDITION_SCALING_ACTIVE,
            STATUS_TRUE,
            "ValidMetricFound",
        );
        set(
            &mut c,
            CONDITION_ABLE_TO_SCALE,
            STATUS_TRUE,
            "ReadyForNewScale",
        );
        set(
            &mut c,
            CONDITION_SCALING_LIMITED,
            STATUS_FALSE,
            "DesiredWithinRange",
        );
        assert_eq!(summary_reason(&c), REASON_READY);
        set(
            &mut c,
            CONDITION_SCALING_LIMITED,
            STATUS_TRUE,
            "MaxCapacityReached",
        );
        assert_eq!(summary_reason(&c), "MaxCapacityReached");
        set(&mut c, CONDITION_ABLE_TO_SCALE, STATUS_FALSE, "Cooldown");
        assert_eq!(summary_reason(&c), "Cooldown");
        set(
            &mut c,
            CONDITION_SCALING_ACTIVE,
            STATUS_UNKNOWN,
            "AwaitingReconcile",
        );
        assert_eq!(summary_reason(&c), "AwaitingReconcile");
        set(
            &mut c,
            CONDITION_SCALING_ACTIVE,
            STATUS_FALSE,
            "FailedGetScale",
        );
        assert_eq!(summary_reason(&c), "FailedGetScale");
        assert_eq!(summary_reason(&[]), "");
    }
}
