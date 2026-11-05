//! Consistency checks over the fully loaded `Config`. They run once at the end
//! of `load`, after every raw value has been parsed, so each rule can assume
//! typed fields (durations, GiB sizes) are already populated.

use super::parse::is_label_name;
use super::{
    ANNOTATE_ON_ALL, ANNOTATE_ON_FAILURE, ANNOTATE_ON_SUCCESS, Config, ConfigError, NOTIFY_ON_ALL,
    NOTIFY_ON_FAILURE, NOTIFY_ON_SUCCESS, ThroughputRecommendation,
};
use crate::humanize::go_duration;

const fn invalid(msg: String) -> ConfigError {
    ConfigError::Invalid(msg)
}

impl Config {
    pub(super) fn validate(&self) -> Result<(), ConfigError> {
        if self.region.is_empty() {
            return Err(invalid("region is required".into()));
        }
        if self.reconcile_interval.is_zero() {
            return Err(invalid(format!(
                "reconcileInterval must be greater than 0, got {}",
                go_duration(self.reconcile_interval)
            )));
        }
        if self.claim_reconcile_interval < std::time::Duration::from_secs(10) {
            return Err(invalid(format!(
                "claimReconcileInterval must be at least 10s, got {}",
                go_duration(self.claim_reconcile_interval)
            )));
        }
        if self.reconcile_concurrency == 0 {
            return Err(invalid(
                "reconcileConcurrency must be greater than 0, got 0".into(),
            ));
        }
        if self.ssm_poll_interval.is_zero() {
            return Err(invalid(format!(
                "ssmPollInterval must be greater than 0, got {}",
                go_duration(self.ssm_poll_interval)
            )));
        }
        if ![NOTIFY_ON_ALL, NOTIFY_ON_SUCCESS, NOTIFY_ON_FAILURE]
            .contains(&self.alertmanager_notify_on.as_str())
        {
            return Err(invalid(format!(
                "alertmanager.notifyOn must be one of {NOTIFY_ON_ALL}, {NOTIFY_ON_SUCCESS}, {NOTIFY_ON_FAILURE}, got {:?}",
                self.alertmanager_notify_on
            )));
        }
        if self.alertmanager_enabled && self.alertmanager_url.is_empty() {
            return Err(invalid(
                "alertmanager.url is required when alertmanager.enabled is true".into(),
            ));
        }
        if ![ANNOTATE_ON_ALL, ANNOTATE_ON_SUCCESS, ANNOTATE_ON_FAILURE]
            .contains(&self.grafana_annotate_on.as_str())
        {
            return Err(invalid(format!(
                "grafanaAnnotation.annotateOn must be one of {ANNOTATE_ON_ALL}, {ANNOTATE_ON_SUCCESS}, {ANNOTATE_ON_FAILURE}, got {:?}",
                self.grafana_annotate_on
            )));
        }
        if self.grafana_annotation_enabled {
            if self.grafana_url.is_empty() {
                return Err(invalid(
                    "grafanaAnnotation.url is required when grafanaAnnotation.enabled is true"
                        .into(),
                ));
            }
            if self.grafana_api_token.is_empty() {
                return Err(invalid(
                    "GRAFANA_API_TOKEN is required when grafanaAnnotation.enabled is true".into(),
                ));
            }
        }
        if self.throughput_recommendation.enabled && self.metrics.url.is_empty() {
            return Err(invalid(
                "metrics.url is required when throughputRecommendation.enabled is true".into(),
            ));
        }
        if let Some(k) = self
            .metrics
            .label_selector
            .keys()
            .find(|k| !is_label_name(k))
        {
            return Err(invalid(format!(
                "invalid metrics.labelSelector key {k:?}: must be a Prometheus label name"
            )));
        }
        self.throughput_recommendation.validate()
    }
}

impl ThroughputRecommendation {
    /// Checks the recommender block. Every rule runs even when the recommender
    /// is disabled, except the one that only makes sense for a live backend, so
    /// a config error surfaces at startup rather than the first time someone
    /// flips enabled to true.
    pub(super) fn validate(&self) -> Result<(), ConfigError> {
        if self.interval.is_zero() {
            return Err(invalid(format!(
                "throughputRecommendation.interval must be greater than 0, got {}",
                go_duration(self.interval)
            )));
        }
        if !is_label_name(&self.metric_node_name_label) {
            return Err(invalid(format!(
                "invalid throughputRecommendation.metricNodeNameLabel {:?}: must be a Prometheus label name",
                self.metric_node_name_label
            )));
        }
        Ok(())
    }
}
