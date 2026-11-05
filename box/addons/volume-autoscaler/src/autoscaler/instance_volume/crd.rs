//! The `InstanceVolumeAutoscaler` custom resource. It is modeled on the
//! `HorizontalPodAutoscaler` (`autoscaling/v2`): a `scaleTargetRef` names what
//! to scale, `metrics` sets the utilization target, `maxCapacity` caps growth,
//! and `behavior.scaleUp` holds the step policies. The scaled quantity is the
//! root EBS volume capacity of standalone EC2 instances, which only grows.

use std::collections::BTreeMap;

use k8s_openapi::apimachinery::pkg::api::resource::Quantity;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::{Condition, Time};
use kube::{CustomResource, KubeSchema};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::autoscaler::{
    Behavior, DEFAULT_UTILIZATION_PERCENT, MIN_COOLDOWN, MetricSourceType, MetricTarget,
    QUANTITY_PATTERN, format_cooldown, parse_cooldown, summary_reason,
};

pub const KIND: &str = "InstanceVolumeAutoscaler";

/// Automatically grows the root EBS volume of the EC2 instances that
/// `scaleTargetRef` selects when root filesystem utilization crosses the
/// metric target.
#[derive(CustomResource, KubeSchema, Clone, Debug, Deserialize, Serialize, PartialEq)]
#[kube(
    group = "autoscaling.volume-autoscaler.sh",
    version = "v1alpha1",
    kind = "InstanceVolumeAutoscaler",
    plural = "instancevolumeautoscalers",
    singular = "instancevolumeautoscaler",
    shortname = "iva",
    status = "InstanceVolumeAutoscalerStatus",
    derive = "PartialEq",
    doc = "Grows the root EBS volume of the EC2 instances selected by scaleTargetRef when root filesystem utilization crosses the target.",
    printcolumn = r#"{"name":"Target","type":"string","jsonPath":".status.target"}"#,
    printcolumn = r#"{"name":"Utilization","type":"string","jsonPath":".status.utilization"}"#,
    printcolumn = r#"{"name":"Volumes","type":"integer","jsonPath":".status.currentVolumes"}"#,
    printcolumn = r#"{"name":"Status","type":"string","jsonPath":".status.reason"}"#,
    printcolumn = r#"{"name":"LastScale","type":"date","jsonPath":".status.lastScaleTime"}"#,
    printcolumn = r#"{"name":"Age","type":"date","jsonPath":".metadata.creationTimestamp"}"#
)]
#[serde(rename_all = "camelCase")]
pub struct InstanceVolumeAutoscalerSpec {
    /// The EC2 instances whose root volume this autoscaler grows.
    pub scale_target_ref: ScaleTargetRef,
    /// The upper bound of the root volume capacity. A step that would exceed
    /// it is not taken.
    #[schemars(extend("pattern" = QUANTITY_PATTERN))]
    #[x_kube(validation = Rule::new("isQuantity(string(self)) && quantity(string(self)).compareTo(quantity('1Gi')) >= 0").message("maxCapacity must be at least 1Gi"))]
    pub max_capacity: Quantity,
    /// The utilization target that triggers a scale up. Only the `storage`
    /// resource is supported. Empty means `storage` at 80 percent.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    #[schemars(length(max = 1))]
    pub metrics: Vec<MetricSpec>,
    /// How the volume grows once the target is crossed. The API server fills
    /// in the scale-up defaults when it is omitted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(extend("default" = {}))]
    pub behavior: Option<Behavior>,
    /// Sends Alertmanager alerts for this autoscaler's resizes. The
    /// controller-wide `alertmanager.enabled` switch remains the master gate.
    #[serde(default = "default_true")]
    pub alert_enabled: bool,
}

const fn default_true() -> bool {
    true
}

/// The kind of target a `scaleTargetRef` names.
#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq, JsonSchema)]
pub enum TargetKind {
    /// One EC2 instance named by its instance ID.
    Instance,
    /// Every discovered EC2 instance carrying all of `tags`.
    TagSelector,
}

/// Names the EC2 instances to scale: one instance by ID (`kind: Instance`
/// with `name`), or a group by tags (`kind: TagSelector` with `tags`).
#[derive(KubeSchema, Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[x_kube(
    validation = Rule::new("self.kind == 'Instance' ? (has(self.name) && !has(self.tags)) : (!has(self.name) && has(self.tags) && size(self.tags) > 0)").message("kind Instance requires name and no tags, kind TagSelector requires at least one tag and no name")
)]
#[serde(rename_all = "camelCase")]
pub struct ScaleTargetRef {
    /// `Instance` or `TagSelector`.
    pub kind: TargetKind,
    /// The EC2 instance ID, for `kind: Instance`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(regex(pattern = r"^i-[0-9a-f]{8,17}$"))]
    pub name: Option<String>,
    /// Tag key and value pairs an instance must all carry, for
    /// `kind: TagSelector`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(extend(
        "maxProperties" = 50,
        "additionalProperties" = {"type": "string", "minLength": 1, "maxLength": 256}
    ))]
    #[x_kube(validation = Rule::new("self.all(k, size(k) > 0 && size(k) <= 128)").message("tag keys must be 1 to 128 characters"))]
    pub tags: Option<BTreeMap<String, String>>,
}

/// One metric the autoscaler scales on.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct MetricSpec {
    /// `Resource`.
    #[serde(rename = "type")]
    pub type_: MetricSourceType,
    /// The resource metric to scale on.
    pub resource: ResourceMetricSource,
}

/// The resource a metric reads. Only `storage` (root filesystem usage) is
/// supported.
#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum ResourceName {
    Storage,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ResourceMetricSource {
    /// `storage`, the root filesystem usage.
    pub name: ResourceName,
    /// The utilization that triggers a scale up.
    pub target: MetricTarget,
}

/// What the controller last observed and did.
#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct InstanceVolumeAutoscalerStatus {
    /// The scale target in one line: an instance ID, or the required tags as
    /// `Key=Value` pairs.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub target: String,
    /// The highest root filesystem usage among the targets against the
    /// utilization target, as `92%/80%`, or `<unknown>/80%` before a
    /// measurement.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub utilization: String,
    /// The most actionable condition reason, or `Ready` when every condition
    /// is healthy.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub reason: String,
    /// The cooldown in effect between two modifications of one volume, the
    /// default included.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub cooldown_duration: String,
    /// The `metadata.generation` the status was computed from.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub observed_generation: Option<i64>,
    /// When any target volume was last grown.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_scale_time: Option<Time>,
    /// How many root EBS volumes the autoscaler currently targets, one per
    /// targeted instance with a resolvable root volume.
    #[serde(default)]
    pub current_volumes: i32,
    /// Per-instance observations from the latest pass.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub instances: Vec<InstanceStatus>,
    /// `AbleToScale`, `ScalingActive`, and `ScalingLimited`, as on an HPA.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub conditions: Vec<Condition>,
}

/// One target instance as observed in the latest pass.
#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct InstanceStatus {
    /// The EC2 instance ID.
    pub instance_id: String,
    /// The instance's Name tag.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub name: String,
    /// The root EBS volume ID.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub volume_id: String,
    /// The root volume capacity after this pass.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current_capacity: Option<Quantity>,
    /// The capacity a scale up targets, set once utilization crossed the
    /// target.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub desired_capacity: Option<Quantity>,
    /// The measured root filesystem usage percent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current_utilization: Option<i32>,
    /// When this instance's volume was last grown.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_scale_time: Option<Time>,
    /// When the volume may be modified again. Set while a cooldown runs,
    /// including right after a resize.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cooldown_until: Option<Time>,
    /// Why the instance is in its current state, in `CamelCase`.
    pub reason: String,
    /// A human readable detail of `reason`.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub message: String,
}

/// How many tags the one-line target shows before counting the rest.
const SUMMARY_TAGS: usize = 3;

/// The scale target in one line, readable even for a spec the controller
/// rejects.
#[must_use]
pub fn target_summary(r: &ScaleTargetRef) -> String {
    match r.kind {
        TargetKind::Instance => r.name.clone().unwrap_or_default(),
        TargetKind::TagSelector => {
            let tags: Vec<String> = r
                .tags
                .iter()
                .flatten()
                .map(|(k, v)| format!("{k}={v}"))
                .collect();
            if tags.len() > SUMMARY_TAGS {
                format!(
                    "{},+{} more",
                    tags[..SUMMARY_TAGS].join(","),
                    tags.len() - SUMMARY_TAGS
                )
            } else {
                tags.join(",")
            }
        }
    }
}

/// The utilization target of a spec, the default when `metrics` is empty.
#[must_use]
pub fn target_utilization(spec: &InstanceVolumeAutoscalerSpec) -> i32 {
    spec.metrics
        .first()
        .map_or(DEFAULT_UTILIZATION_PERCENT, |m| {
            m.resource.target.average_utilization
        })
}

impl InstanceVolumeAutoscalerStatus {
    /// Recomputes the one-line fields `kubectl get` prints from the spec, the
    /// instances, and the conditions.
    pub fn refresh_summary(&mut self, spec: &InstanceVolumeAutoscalerSpec) {
        self.target = target_summary(&spec.scale_target_ref);
        let goal = target_utilization(spec);
        self.utilization = self
            .instances
            .iter()
            .filter_map(|i| i.current_utilization)
            .max()
            .map_or_else(|| format!("<unknown>/{goal}%"), |u| format!("{u}%/{goal}%"));
        self.reason = summary_reason(&self.conditions);
        let raw = spec
            .behavior
            .as_ref()
            .and_then(|b| b.scale_up.as_ref())
            .and_then(|r| r.cooldown_duration.as_deref());
        self.cooldown_duration = raw.map_or_else(
            || format_cooldown(MIN_COOLDOWN),
            |r| parse_cooldown(r).map_or_else(|_| r.to_string(), format_cooldown),
        );
    }
}

#[cfg(test)]
mod tests {
    use kube::CustomResourceExt;

    use super::*;
    use crate::autoscaler::{MetricTargetType, ScalingRules, SelectPolicy};

    #[test]
    fn crd_shape() {
        let crd = serde_json::to_value(InstanceVolumeAutoscaler::crd()).unwrap();
        assert_eq!(
            crd["metadata"]["name"],
            "instancevolumeautoscalers.autoscaling.volume-autoscaler.sh"
        );
        assert_eq!(crd["spec"]["scope"], "Cluster");
        assert_eq!(crd["spec"]["names"]["shortNames"][0], "iva");
        let s = crd.to_string();
        assert!(s.contains("x-kubernetes-validations"), "{s}");
        assert!(s.contains(r"^i-[0-9a-f]{8,17}$"), "{s}");
        assert_eq!(
            crd["spec"]["versions"][0]["subresources"]["status"],
            serde_json::json!({})
        );
        let spec = &crd["spec"]["versions"][0]["schema"]["openAPIV3Schema"]["properties"]["spec"]["properties"];
        assert!(
            spec["maxCapacity"]["pattern"]
                .as_str()
                .is_some_and(|p| p.contains("[KMGTPE]i")),
            "{spec}"
        );
        assert_eq!(spec["behavior"]["default"], serde_json::json!({}));
        assert_eq!(
            spec["behavior"]["properties"]["scaleUp"]["default"],
            serde_json::json!({})
        );
        assert_eq!(
            spec["behavior"]["properties"]["scaleUp"]["properties"]["cooldownDuration"]["default"],
            "6h"
        );
        let select = &spec["behavior"]["properties"]["scaleUp"]["properties"]["selectPolicy"];
        assert_eq!(select["default"], "Max");
        assert_eq!(
            select["enum"],
            serde_json::json!(["Max", "Min", "Disabled"])
        );
    }

    #[test]
    fn spec_round_trips_hpa_shape() {
        let raw = serde_json::json!({
            "scaleTargetRef": {"kind": "TagSelector", "tags": {"Role": "db"}},
            "maxCapacity": "2Ti",
            "metrics": [{"type": "Resource", "resource": {"name": "storage", "target": {"type": "Utilization", "averageUtilization": 70}}}],
            "behavior": {"scaleUp": {"selectPolicy": "Min", "policies": [{"type": "Percent", "value": 20}, {"type": "GiB", "value": 50}]}}
        });
        let spec: InstanceVolumeAutoscalerSpec = serde_json::from_value(raw.clone()).unwrap();
        assert!(spec.alert_enabled, "defaults to true");
        assert_eq!(spec.scale_target_ref.kind, TargetKind::TagSelector);
        assert_eq!(
            spec.behavior
                .as_ref()
                .unwrap()
                .scale_up
                .as_ref()
                .unwrap()
                .select_policy,
            SelectPolicy::Min
        );
        let mut back = serde_json::to_value(&spec).unwrap();
        back.as_object_mut().unwrap().remove("alertEnabled");
        assert_eq!(back, raw);
    }

    #[test]
    fn summary_fields() {
        let tags = ScaleTargetRef {
            kind: TargetKind::TagSelector,
            name: None,
            tags: Some(BTreeMap::from([
                ("Role".to_string(), "db".to_string()),
                ("Env".to_string(), "prod".to_string()),
            ])),
        };
        assert_eq!(target_summary(&tags), "Env=prod,Role=db");
        let many = ScaleTargetRef {
            kind: TargetKind::TagSelector,
            name: None,
            tags: Some(
                (1..=5)
                    .map(|i| (format!("k{i}"), "v".to_string()))
                    .collect(),
            ),
        };
        assert_eq!(target_summary(&many), "k1=v,k2=v,k3=v,+2 more");
        let id = ScaleTargetRef {
            kind: TargetKind::Instance,
            name: Some("i-12345678".into()),
            tags: None,
        };
        assert_eq!(target_summary(&id), "i-12345678");

        let mut spec: InstanceVolumeAutoscalerSpec = serde_json::from_value(serde_json::json!({
            "scaleTargetRef": {"kind": "Instance", "name": "i-12345678"},
            "maxCapacity": "10Gi"
        }))
        .unwrap();
        assert_eq!(target_utilization(&spec), DEFAULT_UTILIZATION_PERCENT);
        let mut s = InstanceVolumeAutoscalerStatus::default();
        s.refresh_summary(&spec);
        assert_eq!(s.utilization, "<unknown>/80%");
        assert_eq!(s.reason, "", "no conditions yet");

        spec.metrics = vec![MetricSpec {
            type_: MetricSourceType::Resource,
            resource: ResourceMetricSource {
                name: ResourceName::Storage,
                target: MetricTarget {
                    type_: MetricTargetType::Utilization,
                    average_utilization: 70,
                },
            },
        }];
        s.instances = vec![
            InstanceStatus {
                current_utilization: Some(64),
                ..InstanceStatus::default()
            },
            InstanceStatus {
                current_utilization: Some(92),
                ..InstanceStatus::default()
            },
            InstanceStatus::default(),
        ];
        s.refresh_summary(&spec);
        assert_eq!(s.utilization, "92%/70%", "the fullest disk is what matters");
    }

    #[test]
    fn cooldowns_parse_and_format() {
        let p = |s: &str| parse_cooldown(s).map(|d| d.as_secs());
        assert_eq!(p("6h"), Ok(21_600));
        assert_eq!(p("6h30m"), Ok(23_400));
        assert_eq!(p("90m"), Ok(5_400));
        assert_eq!(p("1h0m30s"), Ok(3_630));
        for bad in ["", "6", "1d", "6h30", "1.5h", "-6h", "6H"] {
            assert!(parse_cooldown(bad).is_err(), "{bad}");
        }
        let f = |s: u64| format_cooldown(std::time::Duration::from_secs(s));
        assert_eq!(f(21_600), "6h");
        assert_eq!(f(23_400), "6h30m");
        assert_eq!(f(5_400), "1h30m");
        assert_eq!(f(3_630), "1h30s");
        assert_eq!(f(45), "45s");
        assert_eq!(f(0), "0s");

        let mut spec: InstanceVolumeAutoscalerSpec = serde_json::from_value(serde_json::json!({
            "scaleTargetRef": {"kind": "Instance", "name": "i-12345678"},
            "maxCapacity": "10Gi"
        }))
        .unwrap();
        let mut s = InstanceVolumeAutoscalerStatus::default();
        s.refresh_summary(&spec);
        assert_eq!(s.cooldown_duration, "6h", "the EBS floor by default");
        spec.behavior = Some(Behavior {
            scale_up: Some(ScalingRules {
                cooldown_duration: Some("390m".into()),
                ..ScalingRules::default()
            }),
        });
        s.refresh_summary(&spec);
        assert_eq!(s.cooldown_duration, "6h30m", "normalized");
        spec.behavior
            .as_mut()
            .unwrap()
            .scale_up
            .as_mut()
            .unwrap()
            .cooldown_duration = Some("soon".into());
        s.refresh_summary(&spec);
        assert_eq!(
            s.cooldown_duration, "soon",
            "a rejected value is shown as written"
        );
    }
}
