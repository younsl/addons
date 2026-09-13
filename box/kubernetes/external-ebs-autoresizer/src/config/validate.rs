//! Consistency checks over the fully loaded `Config`. They run once at the end
//! of `load`, after every raw value has been parsed, so each rule can assume
//! typed fields (durations, GiB sizes) are already populated.

use super::parse::is_label_name;
use super::{
    ANNOTATE_ON_ALL, ANNOTATE_ON_FAILURE, ANNOTATE_ON_SUCCESS, Config, ConfigError,
    GROW_MODE_ABSOLUTE, GROW_MODE_PERCENT, NOTIFY_ON_ALL, NOTIFY_ON_FAILURE, NOTIFY_ON_SUCCESS,
    ThroughputRecommendation,
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
        if !(0..=100).contains(&self.usage_threshold_percent) {
            return Err(invalid(format!(
                "usageThresholdPercent must be between 0 and 100, got {}",
                self.usage_threshold_percent
            )));
        }
        match self.grow_mode.as_str() {
            GROW_MODE_PERCENT => {
                if self.grow_percent <= 0 {
                    return Err(invalid(format!(
                        "growPercent must be greater than 0, got {}",
                        self.grow_percent
                    )));
                }
            }
            GROW_MODE_ABSOLUTE => {
                if self.grow_amount_gib <= 0 {
                    return Err(invalid(format!(
                        "growAmount must resolve to at least 1 GiB, got {:?}",
                        self.grow_amount
                    )));
                }
            }
            other => {
                return Err(invalid(format!(
                    "growMode must be one of {GROW_MODE_PERCENT}, {GROW_MODE_ABSOLUTE}, got {other:?}"
                )));
            }
        }
        if self.max_volume_size_gib <= 0 {
            return Err(invalid(format!(
                "maxVolumeSizeGiB must be greater than 0, got {}",
                self.max_volume_size_gib
            )));
        }
        if self.reconcile_interval.is_zero() {
            return Err(invalid(format!(
                "reconcileInterval must be greater than 0, got {}",
                go_duration(self.reconcile_interval)
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
        self.throughput_recommendation.validate()
    }
}

impl ThroughputRecommendation {
    /// Checks the recommender block. Every rule runs even when the recommender
    /// is disabled, except the one that only makes sense for a live backend, so
    /// a config error surfaces at startup rather than the first time someone
    /// flips enabled to true.
    pub(super) fn validate(&self) -> Result<(), ConfigError> {
        if self.enabled && self.prometheus_url.is_empty() {
            return Err(invalid(
                "throughputRecommendation.prometheusUrl is required when throughputRecommendation.enabled is true".into(),
            ));
        }
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
