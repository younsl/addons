//! Compiles a `PersistentVolumeClaimAutoscaler` into effective settings and
//! decides, per claim, whether and how far to grow it. Everything here is
//! pure so every skip rule is testable without a cluster.
//!
//! The skip rules follow topolvm pvc-autoresizer: a claim that is not bound,
//! whose `StorageClass` does not allow expansion, whose previous resize has
//! not finished, or that has no `kubelet_volume_stats_*` series is left alone.

use std::time::Duration;

use chrono::{DateTime, Utc};

use crate::autoscaler::persistent_volume_claim::crd::{
    ClaimResourceName, DEFAULT_COOLDOWN, PersistentVolumeClaimAutoscaler, target_utilization,
};
use crate::autoscaler::{
    GIB, REASON_BELOW_TARGET, REASON_COOLDOWN, REASON_DRY_RUN, REASON_FAILED_METRIC,
    REASON_MAX_CAPACITY, REASON_SCALING_DISABLED, REASON_SUCCEEDED_RESCALE, SelectPolicy, Step,
    compile_steps, parse_cooldown, quantity_bytes, step_up,
};

pub const REASON_CLAIM_NOT_BOUND: &str = "ClaimNotBound";
pub const REASON_EXPANSION_NOT_ALLOWED: &str = "VolumeExpansionNotAllowed";
pub const REASON_RESIZE_IN_PROGRESS: &str = "ResizeInProgress";
pub const REASON_AWAITING_FILESYSTEM_RESIZE: &str = "AwaitingFilesystemResize";

/// Below this a reported size counts as unchanged. A resize adds at least a
/// GiB, and the recorded size is rounded to whole bytes.
const GROWTH_TOLERANCE_BYTES: f64 = 1_048_576.0;

/// The shortest cooldown the schema accepts.
const MIN_COOLDOWN: Duration = Duration::from_mins(1);

/// The fully resolved settings of one autoscaler.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Effective {
    /// `behavior.scaleUp.selectPolicy: Disabled`.
    pub paused: bool,
    pub storage_target: Option<i32>,
    pub inodes_target: Option<i32>,
    pub select_min: bool,
    pub steps: Vec<Step>,
    /// `maxCapacity` rounded down to whole GiB, so a step never passes it.
    pub max_capacity_gib: i32,
    pub cooldown: Duration,
}

/// Re-checks what the CRD schema and CEL rules enforce, for specs stored
/// before a CRD upgrade.
pub fn compile(pvca: &PersistentVolumeClaimAutoscaler) -> Result<Effective, String> {
    let spec = &pvca.spec;
    if spec.metrics.len() > 2 {
        return Err("metrics supports at most storage and inodes".into());
    }
    for (i, m) in spec.metrics.iter().enumerate() {
        if spec.metrics[..i]
            .iter()
            .any(|o| o.resource.name == m.resource.name)
        {
            return Err("each metrics resource name may appear once".into());
        }
        let pct = m.resource.target.average_utilization;
        if !(1..=100).contains(&pct) {
            return Err(format!(
                "averageUtilization must be between 1 and 100, got {pct}"
            ));
        }
    }
    let max_bytes = quantity_bytes(&spec.max_capacity).map_err(|e| format!("maxCapacity: {e}"))?;
    let max_gib = (max_bytes / GIB).floor();
    if max_gib < 1.0 {
        return Err(format!(
            "maxCapacity must be at least 1Gi, got {}",
            spec.max_capacity.0
        ));
    }
    let rules = spec
        .behavior
        .as_ref()
        .and_then(|b| b.scale_up.clone())
        .unwrap_or_default();
    let cooldown = match rules.cooldown_duration.as_deref() {
        None => DEFAULT_COOLDOWN,
        Some(raw) => {
            let d = parse_cooldown(raw)
                .map_err(|e| format!("behavior.scaleUp.cooldownDuration: {e}"))?;
            if d < MIN_COOLDOWN {
                return Err(format!(
                    "behavior.scaleUp.cooldownDuration {raw:?} is shorter than 1m"
                ));
            }
            d
        }
    };
    #[allow(clippy::cast_possible_truncation)]
    let max_capacity_gib = max_gib.min(f64::from(i32::MAX)) as i32;
    Ok(Effective {
        paused: rules.select_policy == SelectPolicy::Disabled,
        storage_target: target_utilization(spec, ClaimResourceName::Storage),
        inodes_target: target_utilization(spec, ClaimResourceName::Inodes),
        select_min: rules.select_policy == SelectPolicy::Min,
        steps: compile_steps(&rules.policies)?,
        max_capacity_gib,
        cooldown,
    })
}

/// What the controller knows about one claim before deciding.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Claim {
    pub name: String,
    pub uid: String,
    pub bound: bool,
    pub storage_class: String,
    /// The `StorageClass` sets `allowVolumeExpansion: true`. `None` when the
    /// claim names no class or the class does not exist.
    pub expandable: Option<bool>,
    /// `status.capacity.storage`.
    pub capacity_bytes: Option<f64>,
    /// Why a previous resize is still pending, if it is.
    pub resizing: Option<String>,
    /// The Argo CD Application that tracks the claim, which sees every resize
    /// as drift from the manifest.
    pub tracked_by: Option<String>,
}

/// Used percent per resource from `kubelet_volume_stats_*`.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Usage {
    pub storage: Option<f64>,
    pub inodes: Option<f64>,
    /// The filesystem size kubelet reports, which trails the claim's
    /// capacity until the filesystem has grown and been scraped again.
    pub capacity_bytes: Option<f64>,
}

/// What the controller recorded at the claim's last resize.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct LastScale {
    pub at: Option<DateTime<Utc>>,
    /// The filesystem size kubelet reported when the resize was requested.
    /// Cleared once kubelet reports a larger one.
    pub capacity_bytes: Option<f64>,
}

/// The decision for one claim.
#[derive(Debug, Clone, PartialEq)]
pub struct Outcome {
    pub reason: &'static str,
    pub message: String,
    pub storage: Option<i32>,
    pub inodes: Option<i32>,
    pub current_gib: Option<i32>,
    /// Set when a target is crossed and the claim may grow, or is being
    /// clamped at `maxCapacity`.
    pub desired_gib: Option<i32>,
    pub cooldown_until: Option<DateTime<Utc>>,
    /// The filesystem size to record for the next decision.
    pub capacity_at_last_scale: Option<f64>,
}

impl Outcome {
    /// The claim should be patched to `desired_gib`.
    #[must_use]
    pub fn resize(&self) -> bool {
        self.reason == REASON_SUCCEEDED_RESCALE
    }
}

#[allow(clippy::cast_possible_truncation)]
const fn percent(v: f64) -> i32 {
    v.clamp(0.0, 100.0).round() as i32
}

/// Decides what to do with `claim`, given what was recorded at its last
/// resize.
#[must_use]
#[allow(clippy::too_many_lines)]
pub fn decide(
    eff: &Effective,
    claim: &Claim,
    usage: Usage,
    last_scale: LastScale,
    dry_run: bool,
    now: DateTime<Utc>,
) -> Outcome {
    let current_gib = claim.capacity_bytes.map(|b| {
        #[allow(clippy::cast_possible_truncation)]
        let g = (b / GIB).ceil().min(f64::from(i32::MAX)) as i32;
        g
    });
    let mut out = Outcome {
        reason: REASON_BELOW_TARGET,
        message: String::new(),
        storage: usage.storage.map(percent),
        inodes: usage.inodes.map(percent),
        current_gib,
        desired_gib: None,
        cooldown_until: None,
        capacity_at_last_scale: last_scale.capacity_bytes,
    };
    let finish = |mut o: Outcome, reason: &'static str, message: String| {
        o.reason = reason;
        o.message = message;
        o
    };
    if !claim.bound {
        return finish(out, REASON_CLAIM_NOT_BOUND, "the claim is not bound".into());
    }
    let Some(current) = current_gib else {
        return finish(
            out,
            REASON_CLAIM_NOT_BOUND,
            "the claim reports no status.capacity.storage".into(),
        );
    };
    if claim.expandable != Some(true) {
        return finish(
            out,
            REASON_EXPANSION_NOT_ALLOWED,
            if claim.storage_class.is_empty() {
                "the claim names no StorageClass".into()
            } else if claim.expandable.is_none() {
                format!("StorageClass {} not found", claim.storage_class)
            } else {
                format!(
                    "StorageClass {} does not set allowVolumeExpansion",
                    claim.storage_class
                )
            },
        );
    }
    if let Some(why) = &claim.resizing {
        return finish(out, REASON_RESIZE_IN_PROGRESS, why.clone());
    }
    if let (Some(before), Some(now_bytes)) = (last_scale.capacity_bytes, usage.capacity_bytes) {
        if now_bytes <= before + GROWTH_TOLERANCE_BYTES {
            return finish(
                out,
                REASON_AWAITING_FILESYSTEM_RESIZE,
                format!(
                    "kubelet still reports a {} filesystem, as before the last resize, so its utilization is stale",
                    bytes_gib(now_bytes)
                ),
            );
        }
        out.capacity_at_last_scale = None;
    }
    let measured = [
        (eff.storage_target, usage.storage),
        (eff.inodes_target, usage.inodes),
    ]
    .iter()
    .any(|(target, value)| target.is_some() && value.is_some());
    if !measured {
        return finish(
            out,
            REASON_FAILED_METRIC,
            "no kubelet_volume_stats series for the claim; is a Pod mounting it?".into(),
        );
    }
    let crossed: Vec<String> = [
        ("storage", eff.storage_target, out.storage),
        ("inodes", eff.inodes_target, out.inodes),
    ]
    .iter()
    .filter_map(|(name, target, value)| match (target, value) {
        (Some(t), Some(v)) if v >= t => Some(format!("{name} {v}% >= {t}%")),
        _ => None,
    })
    .collect();
    if crossed.is_empty() {
        return finish(out, REASON_BELOW_TARGET, String::new());
    }
    let crossed = crossed.join(", ");
    if eff.paused {
        return finish(
            out,
            REASON_SCALING_DISABLED,
            format!("{crossed}, but selectPolicy is Disabled"),
        );
    }
    if let Some(last) = last_scale.at {
        let until = last + chrono::TimeDelta::from_std(eff.cooldown).unwrap_or_default();
        if now < until {
            out.cooldown_until = Some(until);
            return finish(
                out,
                REASON_COOLDOWN,
                format!("{crossed}, cooling down until {}", until.to_rfc3339()),
            );
        }
    }
    if current >= eff.max_capacity_gib {
        out.desired_gib = Some(current);
        return finish(
            out,
            REASON_MAX_CAPACITY,
            format!(
                "{crossed}, but {current}Gi is already at maxCapacity {}Gi",
                eff.max_capacity_gib
            ),
        );
    }
    let desired = step_up(current, &eff.steps, eff.select_min).min(eff.max_capacity_gib);
    out.desired_gib = Some(desired);
    if dry_run {
        return finish(
            out,
            REASON_DRY_RUN,
            format!("{crossed}, would grow {current}Gi to {desired}Gi"),
        );
    }
    out.capacity_at_last_scale = usage.capacity_bytes;
    finish(
        out,
        REASON_SUCCEEDED_RESCALE,
        format!("{crossed}, growing {current}Gi to {desired}Gi"),
    )
}

fn bytes_gib(b: f64) -> String {
    format!("{:.1}Gi", b / GIB)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::autoscaler::persistent_volume_claim::crd::PersistentVolumeClaimAutoscalerSpec;

    pub(crate) fn pvca(spec: serde_json::Value) -> PersistentVolumeClaimAutoscaler {
        let spec: PersistentVolumeClaimAutoscalerSpec = serde_json::from_value(spec).unwrap();
        let mut p = PersistentVolumeClaimAutoscaler::new("web", spec);
        p.metadata.namespace = Some("ns".into());
        p.metadata.uid = Some("uid-web".into());
        p.metadata.generation = Some(1);
        p
    }

    pub(crate) fn base() -> serde_json::Value {
        serde_json::json!({
            "scaleTargetRef": {"kind": "StatefulSet", "name": "web"},
            "maxCapacity": "100Gi"
        })
    }

    pub(crate) fn claim(name: &str, gib: f64) -> Claim {
        Claim {
            name: name.into(),
            uid: format!("uid-{name}"),
            bound: true,
            storage_class: "gp3".into(),
            expandable: Some(true),
            capacity_bytes: Some(gib * GIB),
            resizing: None,
            tracked_by: None,
        }
    }

    fn with(mut v: serde_json::Value, key: &str, value: serde_json::Value) -> serde_json::Value {
        v[key] = value;
        v
    }

    #[test]
    fn compile_defaults_and_overrides() {
        let eff = compile(&pvca(base())).unwrap();
        assert_eq!(eff.storage_target, Some(80));
        assert_eq!(eff.inodes_target, None);
        assert_eq!(eff.max_capacity_gib, 100);
        assert_eq!(eff.cooldown, DEFAULT_COOLDOWN);
        assert_eq!(eff.steps, vec![Step::Percent(10), Step::GiB(1)]);
        assert!(!eff.paused && !eff.select_min);

        let spec = with(
            with(
                base(),
                "metrics",
                serde_json::json!([
                    {"type": "Resource", "resource": {"name": "storage", "target": {"type": "Utilization", "averageUtilization": 70}}},
                    {"type": "Resource", "resource": {"name": "inodes", "target": {"type": "Utilization", "averageUtilization": 90}}}
                ]),
            ),
            "behavior",
            serde_json::json!({"scaleUp": {"selectPolicy": "Min", "cooldownDuration": "2h", "policies": [{"type": "GiB", "value": 5}]}}),
        );
        let eff = compile(&pvca(with(spec, "maxCapacity", "1500Mi".into()))).unwrap();
        assert_eq!(
            (eff.storage_target, eff.inodes_target),
            (Some(70), Some(90))
        );
        assert!(eff.select_min);
        assert_eq!(eff.steps, vec![Step::GiB(5)]);
        assert_eq!(eff.cooldown, Duration::from_hours(2));
        assert_eq!(eff.max_capacity_gib, 1, "rounded down");

        let paused = with(
            base(),
            "behavior",
            serde_json::json!({"scaleUp": {"selectPolicy": "Disabled"}}),
        );
        assert!(compile(&pvca(paused)).unwrap().paused);
    }

    #[test]
    fn example_manifests_compile() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("examples");
        let load = |f: &str| -> PersistentVolumeClaimAutoscaler {
            serde_yaml::from_str(&std::fs::read_to_string(dir.join(f)).unwrap()).unwrap()
        };
        let web = compile(&load("statefulset.yaml")).unwrap();
        assert_eq!(
            (web.storage_target, web.inodes_target),
            (Some(80), Some(90))
        );
        assert_eq!(web.steps, vec![Step::Percent(20), Step::GiB(5)]);
        assert_eq!(web.cooldown, Duration::from_mins(30));
        let single = compile(&load("persistent-volume-claim.yaml")).unwrap();
        assert_eq!(single.max_capacity_gib, 1024);
    }

    #[test]
    fn compile_errors() {
        let metric = |name: &str, pct: i32| serde_json::json!({"type": "Resource", "resource": {"name": name, "target": {"type": "Utilization", "averageUtilization": pct}}});
        for (spec, want) in [
            (with(base(), "maxCapacity", "512Mi".into()), "at least 1Gi"),
            (with(base(), "maxCapacity", "lots".into()), "maxCapacity"),
            (
                with(
                    base(),
                    "metrics",
                    serde_json::json!([metric("storage", 80), metric("storage", 90)]),
                ),
                "appear once",
            ),
            (
                with(base(), "metrics", serde_json::json!([metric("storage", 0)])),
                "between 1 and 100",
            ),
            (
                with(
                    base(),
                    "metrics",
                    serde_json::json!([
                        metric("storage", 80),
                        metric("inodes", 80),
                        metric("inodes", 70)
                    ]),
                ),
                "at most",
            ),
            (
                with(
                    base(),
                    "behavior",
                    serde_json::json!({"scaleUp": {"cooldownDuration": "30s"}}),
                ),
                "shorter than 1m",
            ),
            (
                with(
                    base(),
                    "behavior",
                    serde_json::json!({"scaleUp": {"cooldownDuration": "soon"}}),
                ),
                "cooldownDuration",
            ),
            (
                with(
                    base(),
                    "behavior",
                    serde_json::json!({"scaleUp": {"policies": [{"type": "GiB", "value": 0}]}}),
                ),
                "must be positive",
            ),
        ] {
            let err = compile(&pvca(spec)).unwrap_err();
            assert!(err.contains(want), "{err} lacks {want}");
        }
    }

    fn at(s: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(s).unwrap().into()
    }

    #[test]
    fn decide_skips_and_grows() {
        let eff = compile(&pvca(base())).unwrap();
        let now = at("2026-11-03T12:00:00Z");
        let full = Usage {
            storage: Some(85.4),
            inodes: None,
            capacity_bytes: None,
        };
        let go = |c: &Claim, u: Usage, last: Option<DateTime<Utc>>| {
            decide(
                &eff,
                c,
                u,
                LastScale {
                    at: last,
                    capacity_bytes: None,
                },
                false,
                now,
            )
        };

        let mut c = claim("data-web-0", 50.0);
        let o = go(&c, full, None);
        assert_eq!(o.reason, REASON_SUCCEEDED_RESCALE);
        assert!(o.resize());
        assert_eq!(
            (o.current_gib, o.desired_gib, o.storage),
            (Some(50), Some(55), Some(85))
        );
        assert!(o.message.contains("storage 85% >= 80%"), "{}", o.message);

        let o = decide(&eff, &c, full, LastScale::default(), true, now);
        assert_eq!((o.reason, o.desired_gib), (REASON_DRY_RUN, Some(55)));
        assert!(!o.resize());

        assert_eq!(
            go(
                &c,
                Usage {
                    storage: Some(10.0),
                    inodes: None,
                    capacity_bytes: None
                },
                None
            )
            .reason,
            REASON_BELOW_TARGET
        );
        assert_eq!(go(&c, Usage::default(), None).reason, REASON_FAILED_METRIC);
        assert_eq!(
            go(
                &c,
                Usage {
                    storage: None,
                    inodes: Some(99.0),
                    capacity_bytes: None
                },
                None
            )
            .reason,
            REASON_FAILED_METRIC,
            "inodes are not a target"
        );

        let o = go(&c, full, Some(now - chrono::TimeDelta::minutes(10)));
        assert_eq!(o.reason, REASON_COOLDOWN);
        assert_eq!(o.cooldown_until, Some(at("2026-11-03T12:20:00Z")));
        assert_eq!(
            go(&c, full, Some(now - chrono::TimeDelta::hours(1))).reason,
            REASON_SUCCEEDED_RESCALE
        );

        c.capacity_bytes = Some(98.0 * GIB);
        assert_eq!(
            go(&c, full, None).desired_gib,
            Some(100),
            "clamped to maxCapacity"
        );
        c.capacity_bytes = Some(100.0 * GIB);
        let o = go(&c, full, None);
        assert_eq!((o.reason, o.desired_gib), (REASON_MAX_CAPACITY, Some(100)));

        let mut c = claim("data-web-0", 50.0);
        c.resizing = Some("requested 55Gi, capacity 50Gi".into());
        assert_eq!(go(&c, full, None).reason, REASON_RESIZE_IN_PROGRESS);
        c.resizing = None;
        c.expandable = Some(false);
        let o = go(&c, full, None);
        assert_eq!(o.reason, REASON_EXPANSION_NOT_ALLOWED);
        assert!(o.message.contains("allowVolumeExpansion"));
        c.expandable = None;
        assert!(go(&c, full, None).message.contains("not found"));
        c.storage_class.clear();
        assert!(go(&c, full, None).message.contains("names no StorageClass"));
        c.bound = false;
        assert_eq!(go(&c, full, None).reason, REASON_CLAIM_NOT_BOUND);
        let mut c = claim("data-web-0", 50.0);
        c.capacity_bytes = None;
        assert_eq!(go(&c, full, None).reason, REASON_CLAIM_NOT_BOUND);
    }

    #[test]
    fn decide_waits_for_the_filesystem_to_grow() {
        let eff = compile(&pvca(base())).unwrap();
        let now = Utc::now();
        let c = claim("data-web-0", 60.0);
        let at = |fs_gib: f64| Usage {
            storage: Some(90.0),
            inodes: None,
            capacity_bytes: Some(fs_gib * GIB),
        };
        let after_resize = LastScale {
            at: Some(now - chrono::TimeDelta::hours(7)),
            capacity_bytes: Some(49.0 * GIB),
        };

        let o = decide(&eff, &c, at(49.0), after_resize, false, now);
        assert_eq!(o.reason, REASON_AWAITING_FILESYSTEM_RESIZE);
        assert!(o.message.contains("49.0Gi"), "{}", o.message);
        assert_eq!(
            o.capacity_at_last_scale,
            Some(49.0 * GIB),
            "kept for the next pass"
        );
        assert!(!o.resize());

        let o = decide(&eff, &c, at(59.0), after_resize, false, now);
        assert_eq!(
            o.reason, REASON_SUCCEEDED_RESCALE,
            "grown and still over the target"
        );
        assert_eq!(
            o.capacity_at_last_scale,
            Some(59.0 * GIB),
            "the new resize is recorded"
        );

        let below = Usage {
            storage: Some(10.0),
            ..at(59.0)
        };
        let o = decide(&eff, &c, below, after_resize, false, now);
        assert_eq!(o.reason, REASON_BELOW_TARGET);
        assert_eq!(
            o.capacity_at_last_scale, None,
            "cleared once the filesystem grew"
        );

        let no_metric = Usage {
            capacity_bytes: None,
            ..at(49.0)
        };
        let o = decide(&eff, &c, no_metric, after_resize, false, now);
        assert_eq!(
            o.reason, REASON_SUCCEEDED_RESCALE,
            "no capacity series, no guard"
        );
        assert_eq!(o.capacity_at_last_scale, None);

        let o = decide(&eff, &c, at(49.0), LastScale::default(), false, now);
        assert_eq!(
            o.reason, REASON_SUCCEEDED_RESCALE,
            "nothing recorded, nothing to wait for"
        );
        let o = decide(&eff, &c, at(49.0), after_resize, true, now);
        assert_eq!(
            o.reason, REASON_AWAITING_FILESYSTEM_RESIZE,
            "dry run waits too"
        );
    }

    #[test]
    fn decide_inodes_and_pause() {
        let spec = with(
            base(),
            "metrics",
            serde_json::json!([
                {"type": "Resource", "resource": {"name": "storage", "target": {"type": "Utilization", "averageUtilization": 80}}},
                {"type": "Resource", "resource": {"name": "inodes", "target": {"type": "Utilization", "averageUtilization": 90}}}
            ]),
        );
        let eff = compile(&pvca(spec.clone())).unwrap();
        let now = Utc::now();
        let c = claim("data-web-0", 10.0);
        let o = decide(
            &eff,
            &c,
            Usage {
                storage: Some(20.0),
                inodes: Some(95.0),
                capacity_bytes: None,
            },
            LastScale::default(),
            false,
            now,
        );
        assert_eq!(o.reason, REASON_SUCCEEDED_RESCALE);
        assert!(o.message.contains("inodes 95% >= 90%"), "{}", o.message);
        assert_eq!(o.desired_gib, Some(11));
        let o = decide(
            &eff,
            &c,
            Usage {
                storage: None,
                inodes: Some(10.0),
                capacity_bytes: None,
            },
            LastScale::default(),
            false,
            now,
        );
        assert_eq!(
            o.reason, REASON_BELOW_TARGET,
            "one measured target is enough"
        );

        let paused = with(
            spec,
            "behavior",
            serde_json::json!({"scaleUp": {"selectPolicy": "Disabled"}}),
        );
        let eff = compile(&pvca(paused)).unwrap();
        let o = decide(
            &eff,
            &c,
            Usage {
                storage: Some(99.0),
                inodes: None,
                capacity_bytes: None,
            },
            LastScale::default(),
            false,
            now,
        );
        assert_eq!(o.reason, REASON_SCALING_DISABLED);
    }
}
