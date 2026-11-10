//! Consistency checks over the fully loaded `Config`. They run once at the end
//! of `load`, after every raw value has been parsed, so each rule can assume
//! typed fields (durations, GiB sizes) are already populated.
//!
//! Only a missing `region` fails. Every other rule replaces an invalid value
//! with its default, clamps it into range, or turns off the feature that
//! cannot work, and records why in `Config::warnings`.

use std::time::Duration;

use super::parse::is_label_name;
use super::{
    ANNOTATE_ON_ALL, ANNOTATE_ON_FAILURE, ANNOTATE_ON_SUCCESS, Config, ConfigError, Fixes,
    NOTIFY_ON_ALL, NOTIFY_ON_FAILURE, NOTIFY_ON_SUCCESS, OrphanPvcCleanup,
    ThroughputRecommendation,
};
use crate::humanize::go_duration;

const MIN_CLAIM_RECONCILE_INTERVAL: Duration = Duration::from_secs(10);
const MAX_DELETIONS_PER_PASS: usize = 100;

/// Raises `value` to `min` and records it.
fn at_least(fx: &mut Fixes, name: &str, value: &mut Duration, min: Duration, why: &str) {
    if *value < min {
        fx.push(format!(
            "{name} must be at least {}{why}, got {}, using {}",
            go_duration(min),
            go_duration(*value),
            go_duration(min)
        ));
        *value = min;
    }
}

/// Replaces a zero duration with `default` and records it.
fn non_zero(fx: &mut Fixes, name: &str, value: &mut Duration, default: Duration) {
    if value.is_zero() {
        fx.push(format!(
            "{name} must be greater than 0, using {}",
            go_duration(default)
        ));
        *value = default;
    }
}

impl Config {
    pub(super) fn sanitize(&mut self, fx: &mut Fixes) -> Result<(), ConfigError> {
        if self.region.is_empty() {
            return Err(ConfigError::Invalid("region is required".into()));
        }
        non_zero(
            fx,
            "reconcileInterval",
            &mut self.reconcile_interval,
            Duration::from_mins(5),
        );
        at_least(
            fx,
            "claimReconcileInterval",
            &mut self.claim_reconcile_interval,
            MIN_CLAIM_RECONCILE_INTERVAL,
            "",
        );
        if self.reconcile_concurrency == 0 {
            fx.push("reconcileConcurrency must be greater than 0, using 10".into());
            self.reconcile_concurrency = 10;
        }
        non_zero(
            fx,
            "ssmPollInterval",
            &mut self.ssm_poll_interval,
            Duration::from_secs(1),
        );
        if ![NOTIFY_ON_ALL, NOTIFY_ON_SUCCESS, NOTIFY_ON_FAILURE]
            .contains(&self.alertmanager_notify_on.as_str())
        {
            fx.push(format!(
                "alertmanager.notifyOn must be one of {NOTIFY_ON_ALL}, {NOTIFY_ON_SUCCESS}, {NOTIFY_ON_FAILURE}, got {:?}, using {NOTIFY_ON_SUCCESS}",
                self.alertmanager_notify_on
            ));
            self.alertmanager_notify_on = NOTIFY_ON_SUCCESS.into();
        }
        if self.alertmanager_enabled && self.alertmanager_url.is_empty() {
            fx.push(
                "alertmanager.url is required when alertmanager.enabled is true, alerting is disabled"
                    .into(),
            );
            self.alertmanager_enabled = false;
        }
        if ![ANNOTATE_ON_ALL, ANNOTATE_ON_SUCCESS, ANNOTATE_ON_FAILURE]
            .contains(&self.grafana_annotate_on.as_str())
        {
            fx.push(format!(
                "grafanaAnnotation.annotateOn must be one of {ANNOTATE_ON_ALL}, {ANNOTATE_ON_SUCCESS}, {ANNOTATE_ON_FAILURE}, got {:?}, using {ANNOTATE_ON_ALL}",
                self.grafana_annotate_on
            ));
            self.grafana_annotate_on = ANNOTATE_ON_ALL.into();
        }
        if self.grafana_annotation_enabled {
            if self.grafana_url.is_empty() {
                fx.push("grafanaAnnotation.url is required when grafanaAnnotation.enabled is true, annotations are disabled".into());
                self.grafana_annotation_enabled = false;
            } else if self.grafana_api_token.is_empty() {
                fx.push("GRAFANA_API_TOKEN is required when grafanaAnnotation.enabled is true, annotations are disabled".into());
                self.grafana_annotation_enabled = false;
            }
        }
        if self.throughput_recommendation.enabled && self.metrics.url.is_empty() {
            fx.push("metrics.url is required when throughputRecommendation.enabled is true, recommendations are disabled".into());
            self.throughput_recommendation.enabled = false;
        }
        let invalid: Vec<String> = self
            .metrics
            .label_selector
            .keys()
            .filter(|k| !is_label_name(k))
            .cloned()
            .collect();
        for k in invalid {
            fx.push(format!(
                "invalid metrics.labelSelector key {k:?}: must be a Prometheus label name, ignored"
            ));
            self.metrics.label_selector.remove(&k);
        }
        self.throughput_recommendation.sanitize(fx);
        self.orphan_pvc_cleanup.sanitize(fx);
        Ok(())
    }
}

impl ThroughputRecommendation {
    fn sanitize(&mut self, fx: &mut Fixes) {
        non_zero(
            fx,
            "throughputRecommendation.interval",
            &mut self.interval,
            Duration::from_mins(30),
        );
        if !is_label_name(&self.metric_node_name_label) {
            fx.push(format!(
                "invalid throughputRecommendation.metricNodeNameLabel {:?}: must be a Prometheus label name, using node",
                self.metric_node_name_label
            ));
            self.metric_node_name_label = "node".into();
        }
    }
}

impl OrphanPvcCleanup {
    /// Durations are raised to their floor, the per-pass cap is clamped, and
    /// unknown reasons are dropped.
    fn sanitize(&mut self, fx: &mut Fixes) {
        at_least(
            fx,
            "orphanPvcCleanup.minUnusedAge",
            &mut self.min_unused_age,
            crate::pvscan::MIN_UNUSED_AGE,
            ", the scan's minimum unused age",
        );
        at_least(
            fx,
            "orphanPvcCleanup.alertBefore",
            &mut self.alert_before,
            crate::pvscan::INTERVAL,
            ", the scan interval",
        );
        let (known, unknown): (Vec<String>, Vec<String>) = self
            .reasons
            .drain(..)
            .partition(|r| crate::pvscan::UNUSED_PVC_REASONS.contains(&r.as_str()));
        for r in &unknown {
            fx.push(format!(
                "invalid orphanPvcCleanup.reasons entry {r:?}: must be one of {}, ignored",
                crate::pvscan::UNUSED_PVC_REASONS.join(", ")
            ));
        }
        self.reasons = known;
        if self.reasons.is_empty() {
            let all = crate::pvscan::UNUSED_PVC_REASONS.join(", ");
            fx.push(format!(
                "orphanPvcCleanup.reasons has no valid entry, using {all}"
            ));
            self.reasons = crate::pvscan::UNUSED_PVC_REASONS
                .iter()
                .map(|r| (*r).to_string())
                .collect();
        }
        let clamped = self.max_deletions_per_pass.clamp(1, MAX_DELETIONS_PER_PASS);
        if clamped != self.max_deletions_per_pass {
            fx.push(format!(
                "orphanPvcCleanup.maxDeletionsPerPass must be between 1 and {MAX_DELETIONS_PER_PASS}, got {}, using {clamped}",
                self.max_deletions_per_pass
            ));
            self.max_deletions_per_pass = clamped;
        }
        for ns in self
            .include_namespaces
            .iter()
            .filter(|n| self.exclude_namespaces.contains(n))
        {
            fx.push(format!(
                "orphanPvcCleanup namespace {ns:?} is both included and excluded, excluded wins"
            ));
        }
    }
}
