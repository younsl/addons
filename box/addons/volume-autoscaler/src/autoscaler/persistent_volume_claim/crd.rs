//! The `PersistentVolumeClaimAutoscaler` custom resource. Like the
//! `InstanceVolumeAutoscaler` it follows the `HorizontalPodAutoscaler`:
//! `scaleTargetRef` names the claims (a `StatefulSet`'s claims, as the gardener
//! pvc-autoscaler targets them, or one claim), `metrics` sets the space and
//! inode utilization targets that topolvm pvc-autoresizer reads from
//! `kubelet_volume_stats_*`, `maxCapacity` caps growth, and `behavior.scaleUp`
//! holds the step policies and cooldown.

use k8s_openapi::apimachinery::pkg::api::resource::Quantity;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::{Condition, Time};
use kube::{CustomResource, KubeSchema};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::autoscaler::{
    DEFAULT_UTILIZATION_PERCENT, MetricSourceType, MetricTarget, QUANTITY_PATTERN, ScalingPolicy,
    SelectPolicy, format_cooldown, parse_cooldown, summary_reason,
};

pub const KIND: &str = "PersistentVolumeClaimAutoscaler";

/// The cooldown when `behavior.scaleUp.cooldownDuration` is unset.
pub const DEFAULT_COOLDOWN: std::time::Duration = std::time::Duration::from_mins(30);

/// Grows the `PersistentVolumeClaims` selected by `scaleTargetRef` when their
/// space or inode utilization crosses the target.
#[derive(CustomResource, KubeSchema, Clone, Debug, Deserialize, Serialize, PartialEq)]
#[kube(
    group = "autoscaling.volume-autoscaler.sh",
    version = "v1alpha1",
    kind = "PersistentVolumeClaimAutoscaler",
    plural = "persistentvolumeclaimautoscalers",
    singular = "persistentvolumeclaimautoscaler",
    shortname = "pvca",
    namespaced,
    status = "PersistentVolumeClaimAutoscalerStatus",
    derive = "PartialEq",
    doc = "Grows the PersistentVolumeClaims selected by scaleTargetRef when space or inode utilization crosses the target.",
    printcolumn = r#"{"name":"Target","type":"string","jsonPath":".status.target"}"#,
    printcolumn = r#"{"name":"Status","type":"string","jsonPath":".status.reason"}"#,
    printcolumn = r#"{"name":"Volumes","type":"integer","jsonPath":".status.currentVolumes"}"#,
    printcolumn = r#"{"name":"Utilization","type":"string","jsonPath":".status.utilization"}"#,
    printcolumn = r#"{"name":"MaxSize","type":"string","jsonPath":".spec.maxCapacity"}"#,
    printcolumn = r#"{"name":"LastScale","type":"date","jsonPath":".status.lastScaleTime"}"#,
    printcolumn = r#"{"name":"Age","type":"date","jsonPath":".metadata.creationTimestamp"}"#
)]
#[x_kube(
    validation = Rule::new("!has(self.metrics) || self.metrics.all(a, self.metrics.exists_one(b, b.resource.name == a.resource.name))").message("each metrics resource name may appear once")
)]
#[serde(rename_all = "camelCase")]
pub struct PersistentVolumeClaimAutoscalerSpec {
    /// The claims this autoscaler grows: every claim of a `StatefulSet`, or
    /// one `PersistentVolumeClaim`, in the autoscaler's namespace.
    pub scale_target_ref: ClaimTargetRef,
    /// The upper bound of a claim's capacity. A step that would pass it is
    /// clamped to it.
    #[schemars(extend("pattern" = QUANTITY_PATTERN))]
    #[x_kube(validation = Rule::new("isQuantity(string(self)) && quantity(string(self)).compareTo(quantity('1Gi')) >= 0").message("maxCapacity must be at least 1Gi"))]
    pub max_capacity: Quantity,
    /// The utilization targets that trigger a scale up: `storage` (used
    /// space) and `inodes`. Any one crossing its target grows the claim.
    /// Empty means `storage` at 80 percent.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    #[schemars(length(max = 2))]
    pub metrics: Vec<ClaimMetricSpec>,
    /// How the claim grows once a target is crossed. The API server fills in
    /// the scale-up defaults when it is omitted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(extend("default" = {}))]
    pub behavior: Option<ClaimBehavior>,
}

/// Scaling behavior. Claims never shrink, so there is no `scaleDown`.
#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ClaimBehavior {
    /// Scale-up rules. Omitted means `Max` of 10 percent and 1 GiB.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(extend("default" = {}))]
    pub scale_up: Option<ClaimScalingRules>,
}

/// How one step is sized.
#[derive(KubeSchema, Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ClaimScalingRules {
    /// `Max` (default) takes the largest policy step, `Min` the smallest,
    /// `Disabled` stops scaling for this autoscaler.
    #[serde(default)]
    pub select_policy: SelectPolicy,
    /// Step policies. Empty means 10 percent and 1 GiB with `Max`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub policies: Vec<ScalingPolicy>,
    /// Minimum time between two resizes of the same claim, in hours,
    /// minutes, and seconds (`30m`, `6h`). At least `1m`. EBS volumes accept
    /// four modifications in a rolling 24 hours, and a request past that
    /// stays in progress until the window allows it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(regex(pattern = r"^([0-9]+(h|m|s))+$"), extend("default" = "30m"))]
    #[x_kube(validation = Rule::new("duration(self) >= duration('1m')").message("cooldownDuration must be at least 1m"))]
    pub cooldown_duration: Option<String>,
}

/// The kind of workload a `scaleTargetRef` names.
#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq, JsonSchema)]
pub enum ClaimTargetKind {
    /// Every claim a `StatefulSet`'s `volumeClaimTemplates` produced.
    StatefulSet,
    /// One claim by name.
    PersistentVolumeClaim,
}

/// Names the claims to scale, in the autoscaler's namespace.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ClaimTargetRef {
    /// `StatefulSet` or `PersistentVolumeClaim`.
    pub kind: ClaimTargetKind,
    /// The object's name.
    #[schemars(
        regex(pattern = r"^[a-z0-9]([-a-z0-9.]*[a-z0-9])?$"),
        length(max = 253)
    )]
    pub name: String,
}

/// One metric the autoscaler scales on.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ClaimMetricSpec {
    /// `Resource`.
    #[serde(rename = "type")]
    pub type_: MetricSourceType,
    /// The resource metric to scale on.
    pub resource: ClaimResourceMetricSource,
}

/// The claim resource a metric reads.
#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum ClaimResourceName {
    /// Used space against capacity.
    Storage,
    /// Used inodes against the inode total.
    Inodes,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ClaimResourceMetricSource {
    /// `storage` or `inodes`.
    pub name: ClaimResourceName,
    /// The utilization that triggers a scale up.
    pub target: MetricTarget,
}

/// What the controller last observed and did.
#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct PersistentVolumeClaimAutoscalerStatus {
    /// The scale target in one line, as `StatefulSet/web`.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub target: String,
    /// The fullest claim against its space target, as `92%/80%`, or
    /// `<unknown>/80%` before a measurement.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub utilization: String,
    /// The most actionable condition reason, or `Ready` when every condition
    /// is healthy.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub reason: String,
    /// The cooldown in effect between two resizes of one claim, the default
    /// included.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub cooldown_duration: String,
    /// The `metadata.generation` the status was computed from.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub observed_generation: Option<i64>,
    /// When any target claim was last grown.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_scale_time: Option<Time>,
    /// How many claims the autoscaler currently targets.
    #[serde(default)]
    pub current_volumes: i32,
    /// Per-claim observations from the latest pass.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub claims: Vec<ClaimStatus>,
    /// `AbleToScale`, `ScalingActive`, and `ScalingLimited`, as on an HPA.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub conditions: Vec<Condition>,
}

/// One target claim as observed in the latest pass.
#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ClaimStatus {
    /// The claim's name.
    pub name: String,
    /// The claim's `status.capacity.storage`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current_capacity: Option<Quantity>,
    /// The capacity a scale up requests, set once a target was crossed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub desired_capacity: Option<Quantity>,
    /// Used space percent from `kubelet_volume_stats_*`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current_utilization: Option<i32>,
    /// Used inode percent from `kubelet_volume_stats_*`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current_inodes_utilization: Option<i32>,
    /// When this claim was last grown.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_scale_time: Option<Time>,
    /// When the claim may be grown again.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cooldown_until: Option<Time>,
    /// The filesystem size kubelet reported when the last resize was
    /// requested. Until kubelet reports a larger one, utilization is measured
    /// against the old size and the claim is not grown again.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub capacity_at_last_scale: Option<Quantity>,
    /// The Argo CD Application tracking the claim. A resize leaves it
    /// `OutOfSync` unless it ignores `/spec/resources/requests/storage`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tracked_by: Option<String>,
    /// Why the claim is in its current state, in `CamelCase`.
    pub reason: String,
    /// A human readable detail of `reason`.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub message: String,
}

/// The scale target in one line.
#[must_use]
pub fn target_summary(r: &ClaimTargetRef) -> String {
    let kind = match r.kind {
        ClaimTargetKind::StatefulSet => "StatefulSet",
        ClaimTargetKind::PersistentVolumeClaim => "PersistentVolumeClaim",
    };
    format!("{kind}/{}", r.name)
}

/// The utilization target of one resource, `None` when the spec sets none.
/// An empty `metrics` list means `storage` at the default target.
#[must_use]
pub fn target_utilization(
    spec: &PersistentVolumeClaimAutoscalerSpec,
    name: ClaimResourceName,
) -> Option<i32> {
    if spec.metrics.is_empty() {
        return (name == ClaimResourceName::Storage).then_some(DEFAULT_UTILIZATION_PERCENT);
    }
    spec.metrics
        .iter()
        .find(|m| m.resource.name == name)
        .map(|m| m.resource.target.average_utilization)
}

impl PersistentVolumeClaimAutoscalerStatus {
    /// Recomputes the one-line fields `kubectl get` prints.
    pub fn refresh_summary(&mut self, spec: &PersistentVolumeClaimAutoscalerSpec) {
        self.target = target_summary(&spec.scale_target_ref);
        let used = self
            .claims
            .iter()
            .filter_map(|c| c.current_utilization)
            .max();
        let goal = target_utilization(spec, ClaimResourceName::Storage);
        let shown = used.map_or_else(|| "<unknown>".to_string(), |u| format!("{u}%"));
        self.utilization = goal.map_or_else(|| shown.clone(), |g| format!("{shown}/{g}%"));
        // A StorageClass that blocks expansion is a setup error no pass can
        // fix, so it outranks the generic order.
        self.reason = crate::autoscaler::condition(
            &self.conditions,
            crate::autoscaler::CONDITION_ABLE_TO_SCALE,
        )
        .filter(|c| c.reason == super::policy::REASON_EXPANSION_NOT_ALLOWED)
        .map_or_else(|| summary_reason(&self.conditions), |c| c.reason.clone());
        let raw = spec
            .behavior
            .as_ref()
            .and_then(|b| b.scale_up.as_ref())
            .and_then(|r| r.cooldown_duration.as_deref());
        self.cooldown_duration = raw.map_or_else(
            || format_cooldown(DEFAULT_COOLDOWN),
            |r| parse_cooldown(r).map_or_else(|_| r.to_string(), format_cooldown),
        );
    }
}

#[cfg(test)]
mod tests {
    use kube::CustomResourceExt;

    use super::*;
    use crate::autoscaler::MetricTargetType;

    #[test]
    fn crd_shape() {
        let crd = serde_json::to_value(PersistentVolumeClaimAutoscaler::crd()).unwrap();
        assert_eq!(
            crd["metadata"]["name"],
            "persistentvolumeclaimautoscalers.autoscaling.volume-autoscaler.sh"
        );
        assert_eq!(crd["spec"]["scope"], "Namespaced");
        assert_eq!(crd["spec"]["names"]["shortNames"][0], "pvca");
        let columns: Vec<&str> = crd["spec"]["versions"][0]["additionalPrinterColumns"]
            .as_array()
            .unwrap()
            .iter()
            .map(|c| c["name"].as_str().unwrap())
            .collect();
        assert_eq!(
            columns,
            [
                "Target",
                "Status",
                "Volumes",
                "Utilization",
                "MaxSize",
                "LastScale",
                "Age"
            ]
        );
        let s = crd.to_string();
        assert!(
            s.contains("!has(self.metrics) || self.metrics.all"),
            "an omitted metrics list must not fail the rule: {s}"
        );
        assert!(s.contains("maxCapacity must be at least 1Gi"), "{s}");
        let spec = &crd["spec"]["versions"][0]["schema"]["openAPIV3Schema"]["properties"]["spec"]["properties"];
        assert_eq!(
            spec["metrics"]["items"]["properties"]["resource"]["properties"]["name"]["enum"],
            serde_json::json!(["storage", "inodes"])
        );
        assert_eq!(
            spec["scaleTargetRef"]["properties"]["kind"]["enum"],
            serde_json::json!(["StatefulSet", "PersistentVolumeClaim"])
        );
    }

    fn metric(name: ClaimResourceName, pct: i32) -> ClaimMetricSpec {
        ClaimMetricSpec {
            type_: MetricSourceType::Resource,
            resource: ClaimResourceMetricSource {
                name,
                target: MetricTarget {
                    type_: MetricTargetType::Utilization,
                    average_utilization: pct,
                },
            },
        }
    }

    #[test]
    fn targets_and_summary() {
        let mut spec: PersistentVolumeClaimAutoscalerSpec =
            serde_json::from_value(serde_json::json!({
                "scaleTargetRef": {"kind": "StatefulSet", "name": "web"},
                "maxCapacity": "100Gi"
            }))
            .unwrap();
        assert_eq!(
            target_utilization(&spec, ClaimResourceName::Storage),
            Some(80)
        );
        assert_eq!(target_utilization(&spec, ClaimResourceName::Inodes), None);
        spec.metrics = vec![metric(ClaimResourceName::Inodes, 90)];
        assert_eq!(target_utilization(&spec, ClaimResourceName::Storage), None);
        assert_eq!(
            target_utilization(&spec, ClaimResourceName::Inodes),
            Some(90)
        );

        let mut s = PersistentVolumeClaimAutoscalerStatus::default();
        s.refresh_summary(&spec);
        assert_eq!(s.target, "StatefulSet/web");
        assert_eq!(s.utilization, "<unknown>");
        assert_eq!(s.cooldown_duration, "30m");
        spec.metrics.push(metric(ClaimResourceName::Storage, 70));
        s.claims = vec![
            ClaimStatus {
                current_utilization: Some(85),
                ..ClaimStatus::default()
            },
            ClaimStatus {
                current_utilization: Some(40),
                ..ClaimStatus::default()
            },
        ];
        s.refresh_summary(&spec);
        assert_eq!(s.utilization, "85%/70%");
        assert_eq!(
            target_summary(&ClaimTargetRef {
                kind: ClaimTargetKind::PersistentVolumeClaim,
                name: "data-0".into()
            }),
            "PersistentVolumeClaim/data-0"
        );
    }
}
