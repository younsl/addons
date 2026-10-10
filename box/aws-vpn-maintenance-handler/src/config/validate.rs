//! Reports every configuration problem that would make automated tunnel
//! replacement unsafe or impossible. All of them fail startup: a controller
//! with no working approval channel would replace tunnels with nobody watching.

use super::{Config, ConfigError, TrafficGate, Window};
use crate::promx::gate::OnError;
use crate::window;

impl Config {
    /// Checks the whole configuration, collecting every problem.
    #[allow(clippy::too_many_lines)]
    pub fn validate(&self) -> Result<(), ConfigError> {
        let mut errs = Vec::new();

        if self.region.is_empty() {
            errs.push("region is required".to_string());
        }
        if self.targets.tag_filters.is_empty() {
            errs.push(
                "targets.tagFilters must list at least one tag; managed VPN connections are opted in explicitly, never by default"
                    .to_string(),
            );
        }
        for (i, f) in self.targets.tag_filters.iter().enumerate() {
            if f.key.is_empty() {
                errs.push(format!("targets.tagFilters[{i}].key is required"));
            }
        }

        if self.approval.slack_user_ids.is_empty() {
            errs.push(
                "approval.slackUserIDs must list at least one Slack user ID; replacements require a human approver"
                    .to_string(),
            );
        }
        for (i, id) in self.approval.slack_user_ids.iter().enumerate() {
            if !id.starts_with('U') && !id.starts_with('W') {
                errs.push(format!(
                    "approval.slackUserIDs[{i}] = {id:?} is not a Slack user ID (expected Uxxxxxxxx or Wxxxxxxxx, not a display name)"
                ));
            }
        }
        if self.slack_bot_token.is_empty() {
            errs.push("SLACK_BOT_TOKEN is required (xoxb- bot token)".to_string());
        }
        if self.slack_app_token.is_empty() {
            errs.push(
                "SLACK_APP_TOKEN is required (xapp- app-level token for Socket Mode)".to_string(),
            );
        } else if !self.slack_app_token.starts_with("xapp-") {
            errs.push(
                "SLACK_APP_TOKEN must be an app-level token starting with xapp-; Socket Mode does not accept a bot token"
                    .to_string(),
            );
        }

        errs.extend(self.maintenance_window.validate());
        errs.extend(self.traffic_gate.validate());

        if self.reconcile_interval.is_zero() {
            errs.push("reconcileInterval must be positive".to_string());
        }
        let vt = self.safety.verify_timeout;
        let poll = self.safety.verify_poll_interval;
        if vt.is_zero() {
            errs.push("safety.verifyTimeout must be positive".to_string());
        }
        if poll.is_zero() {
            errs.push("safety.verifyPollInterval must be positive".to_string());
        }
        if poll >= vt {
            errs.push(format!(
                "safety.verifyPollInterval ({poll}) must be shorter than safety.verifyTimeout ({vt})"
            ));
        }
        if self.safety.peer_min_accepted_routes < 0 {
            errs.push("safety.peerMinAcceptedRoutes must not be negative".to_string());
        }
        if self.approval.timeout.is_zero() {
            errs.push("approval.timeout must be positive".to_string());
        }
        let span = self.maintenance_window.duration;
        let mr = self.maintenance_window.min_remaining;
        // A minRemaining longer than the window itself can never be satisfied.
        if !span.is_zero() && mr > span {
            errs.push(format!(
                "maintenanceWindow.minRemaining ({mr}) exceeds maintenanceWindow.duration ({span}); no replacement could ever start"
            ));
        }
        // A window shorter than the verification it has to contain would leave
        // every replacement spilling past the window it was authorized in.
        if !span.is_zero() && vt > span {
            errs.push(format!(
                "safety.verifyTimeout ({vt}) exceeds maintenanceWindow.duration ({span}); verification could not finish inside the window"
            ));
        }
        // This is the check that actually enforces what minRemaining is for.
        if !mr.is_zero() && !vt.is_zero() && mr < vt {
            errs.push(format!(
                "maintenanceWindow.minRemaining ({mr}) is shorter than safety.verifyTimeout ({vt}); a replacement started at the window boundary would verify past the close"
            ));
        }

        if self.leader_elect && self.pod_name.is_empty() {
            errs.push("leaderElect requires POD_NAME (downward API)".to_string());
        }
        if self.pod_namespace.is_empty() {
            errs.push(
                "POD_NAMESPACE is required (downward API); it locates the Lease, the state ConfigMap, and emitted Events"
                    .to_string(),
            );
        }
        if self.state_config_map_name.is_empty() {
            errs.push("stateConfigMapName is required".to_string());
        }
        if self.health_port == self.metrics_port {
            errs.push(format!(
                "healthPort and metricsPort must differ (both {})",
                self.health_port
            ));
        }

        if errs.is_empty() {
            Ok(())
        } else {
            Err(ConfigError::Invalid(errs))
        }
    }
}

impl TrafficGate {
    /// Checks the traffic gate. The whole block is only meaningful when
    /// enabled, so a disabled gate with half-filled fields is not an error.
    fn validate(&self) -> Vec<String> {
        let mut errs = Vec::new();
        if let Err(err) = OnError::parse(&self.on_error) {
            errs.push(format!("trafficGate.{err}"));
        }
        if !self.enabled {
            return errs;
        }
        if self.endpoint.trim().is_empty() {
            errs.push("trafficGate.endpoint is required when the gate is enabled".to_string());
        }
        if self.timeout.is_zero() {
            errs.push("trafficGate.timeout must be positive".to_string());
        }
        // 100 would allow every moment, including the busiest one ever
        // recorded, which reads as a configured gate while being none.
        if self.quiet_percentile <= 0.0 || self.quiet_percentile >= 100.0 {
            errs.push(format!(
                "trafficGate.quietPercentile must be above 0 and below 100, got {}; it is the share of this window's own traffic that counts as quiet",
                self.quiet_percentile
            ));
        }
        errs
    }
}

impl Window {
    fn validate(&self) -> Vec<String> {
        let mut errs = Vec::new();
        if let Err(err) = window::timezone(&self.timezone) {
            errs.push(format!("maintenanceWindow.{err}"));
        }
        if let Err(err) = window::parse(&self.cron_schedule) {
            errs.push(format!("maintenanceWindow.cronSchedule: {err}"));
        }
        if self.duration.is_zero() {
            errs.push(
                "maintenanceWindow.duration must be positive; a cron schedule names instants, so the duration is what makes it a window"
                    .to_string(),
            );
        }
        errs
    }
}

#[cfg(test)]
mod tests {
    use crate::config::{Duration, Env, MINIMAL_YAML, parse, test_env};

    fn errors(yaml: &str, env: &Env) -> String {
        parse(yaml, env).unwrap_err().to_string()
    }

    #[test]
    fn minimal_config_is_valid() {
        parse(MINIMAL_YAML, &test_env()).unwrap();
    }

    #[test]
    fn reports_every_missing_required_value_at_once() {
        let err = errors("", &Env::default());
        for want in [
            "region is required",
            "targets.tagFilters must list at least one tag",
            "approval.slackUserIDs must list at least one",
            "SLACK_BOT_TOKEN is required",
            "SLACK_APP_TOKEN is required",
            "leaderElect requires POD_NAME",
            "POD_NAMESPACE is required",
        ] {
            assert!(err.contains(want), "missing {want:?} in:\n{err}");
        }
    }

    #[test]
    fn rejects_non_user_ids_and_bot_token_as_app_token() {
        let yaml = MINIMAL_YAML.replace("U0123456789", "younsl");
        let env = Env {
            slack_app_token: "xoxb-not-app".into(),
            ..test_env()
        };
        let err = errors(&yaml, &env);
        assert!(err.contains("is not a Slack user ID"), "{err}");
        assert!(err.contains("must be an app-level token"), "{err}");
    }

    #[test]
    fn enforces_window_and_verify_relationships() {
        let yaml = format!(
            "{MINIMAL_YAML}\nmaintenanceWindow:\n  duration: \"1h\"\n  minRemaining: \"2h\"\nsafety:\n  verifyTimeout: \"3h\"\n  verifyPollInterval: \"4h\"\n"
        );
        let err = errors(&yaml, &test_env());
        assert!(
            err.contains("minRemaining (2h0m0s) exceeds maintenanceWindow.duration"),
            "{err}"
        );
        assert!(
            err.contains("verifyTimeout (3h0m0s) exceeds maintenanceWindow.duration"),
            "{err}"
        );
        assert!(
            err.contains("minRemaining (2h0m0s) is shorter than safety.verifyTimeout"),
            "{err}"
        );
        assert!(
            err.contains("must be shorter than safety.verifyTimeout"),
            "{err}"
        );
    }

    #[test]
    fn rejects_zero_and_negative_thresholds() {
        let yaml = "region: r\ntargets:\n  tagFilters: [{key: k}]\nreconcileInterval: \"0s\"\nsafety:\n  peerMinAcceptedRoutes: -1\n  verifyTimeout: \"0s\"\n  verifyPollInterval: \"0s\"\napproval:\n  slackUserIDs: [U1]\n  timeout: \"0s\"\nmaintenanceWindow:\n  duration: \"0s\"\n  timezone: Nowhere/Land\n  cronSchedule: \"bad\"\nhealthPort: 9090\n";
        let err = errors(yaml, &test_env());
        for want in [
            "reconcileInterval must be positive",
            "safety.verifyTimeout must be positive",
            "safety.verifyPollInterval must be positive",
            "peerMinAcceptedRoutes must not be negative",
            "approval.timeout must be positive",
            "maintenanceWindow.duration must be positive",
            "maintenanceWindow.timezone \"Nowhere/Land\"",
            "maintenanceWindow.cronSchedule: invalid cron schedule",
            "healthPort and metricsPort must differ (both 9090)",
        ] {
            assert!(err.contains(want), "missing {want:?} in:\n{err}");
        }
    }

    #[test]
    fn traffic_gate_only_checked_when_enabled() {
        let disabled =
            format!("{MINIMAL_YAML}\ntrafficGate:\n  enabled: false\n  quietPercentile: 500\n");
        parse(&disabled, &test_env()).unwrap();

        let enabled = format!(
            "{MINIMAL_YAML}\ntrafficGate:\n  enabled: true\n  quietPercentile: 100\n  timeout: \"0s\"\n  onError: maybe\n"
        );
        let err = errors(&enabled, &test_env());
        assert!(err.contains("trafficGate.endpoint is required"), "{err}");
        assert!(
            err.contains("trafficGate.timeout must be positive"),
            "{err}"
        );
        assert!(
            err.contains("quietPercentile must be above 0 and below 100"),
            "{err}"
        );
        assert!(err.contains("trafficGate.onError must be"), "{err}");
    }

    #[test]
    fn leader_election_off_needs_no_pod_name() {
        let yaml = format!("{MINIMAL_YAML}\nleaderElect: false\n");
        let env = Env {
            pod_name: String::new(),
            ..test_env()
        };
        let cfg = parse(&yaml, &env).unwrap();
        assert!(!cfg.leader_elect);
        assert_eq!(cfg.safety.verify_poll_interval, Duration::secs(10));
    }
}
