//! The Prometheus registry and the metric set.
//!
//! Application identity is deliberately absent from the labels: it belongs in
//! the logs, where a denial costs one line, not in a time series, where it
//! would cost one series per Application forever.

use std::sync::Mutex;
use std::time::Duration;

use prometheus_client::encoding::{EncodeLabelSet, text::encode};
use prometheus_client::metrics::counter::Counter;
use prometheus_client::metrics::family::Family;
use prometheus_client::metrics::gauge::Gauge;
use prometheus_client::metrics::histogram::Histogram;
use prometheus_client::registry::Registry;

use crate::config::Mode;
use crate::gate::Decision;

const NAMESPACE: &str = "argocd_canary_gate";

/// Buckets that resolve the range that matters for an admission webhook.
///
/// The deadline the gate lives under is the webhook's own `timeoutSeconds`,
/// single-digit seconds, so the buckets are dense below one second and stop at
/// five. A request slower than the top bucket has already been abandoned by
/// the API server, and where exactly it landed after that is not worth a
/// series.
const LATENCY_BUCKETS: [f64; 13] = [
    0.001, 0.0025, 0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.0, 3.0, 5.0,
];

#[derive(Clone, Debug, Hash, PartialEq, Eq, EncodeLabelSet)]
struct DecisionLabels {
    code: String,
    allowed: String,
    mode: String,
}

#[derive(Clone, Debug, Hash, PartialEq, Eq, EncodeLabelSet)]
struct OutcomeLabels {
    outcome: String,
}

#[derive(Clone, Debug, Hash, PartialEq, Eq, EncodeLabelSet)]
struct KindLabels {
    kind: String,
}

#[derive(Clone, Debug, Hash, PartialEq, Eq, EncodeLabelSet)]
struct EventLabels {
    reason: String,
    r#type: String,
}

#[derive(Clone, Debug, Hash, PartialEq, Eq, EncodeLabelSet)]
struct ResultLabels {
    result: String,
}

fn latency_histogram() -> Histogram {
    Histogram::new(LATENCY_BUCKETS)
}

/// The gate metric set, registered on its own registry so the exposed series
/// stay limited to what this binary owns.
pub struct Metrics {
    registry: Mutex<Registry>,
    decisions: Family<DecisionLabels, Counter>,
    admission_requests: Family<OutcomeLabels, Counter>,
    lookup_failures: Family<KindLabels, Counter>,
    events: Family<EventLabels, Counter>,
    admission_duration: Family<OutcomeLabels, Histogram>,
    rollout_lookup: Family<ResultLabels, Histogram>,
    certificate_expiry: Gauge,
}

impl Default for Metrics {
    fn default() -> Self {
        Self::new()
    }
}

impl Metrics {
    /// Builds and registers the metric set.
    #[must_use]
    pub fn new() -> Self {
        let mut registry = Registry::with_prefix(NAMESPACE);

        let decisions = Family::<DecisionLabels, Counter>::default();
        registry.register(
            "decisions",
            "Gate verdicts by reason code, outcome, and the mode in force",
            decisions.clone(),
        );

        let admission_requests = Family::<OutcomeLabels, Counter>::default();
        registry.register(
            "admission_requests",
            "Admission requests handled, labeled by outcome",
            admission_requests.clone(),
        );

        let lookup_failures = Family::<KindLabels, Counter>::default();
        registry.register(
            "lookup_failures",
            "Fact lookups that failed, labeled by the kind of lookup",
            lookup_failures.clone(),
        );

        let events = Family::<EventLabels, Counter>::default();
        registry.register(
            "events",
            "Kubernetes Events submitted for a verdict, by event reason and type. Verdict codes live on decisions_total",
            events.clone(),
        );

        let admission_duration =
            Family::<OutcomeLabels, Histogram>::new_with_constructor(latency_histogram);
        registry.register(
            "admission_duration_seconds",
            "Wall time to answer one admission request, labeled by outcome",
            admission_duration.clone(),
        );

        let rollout_lookup =
            Family::<ResultLabels, Histogram>::new_with_constructor(latency_histogram);
        registry.register(
            "rollout_lookup_duration_seconds",
            "Wall time of the Kubernetes list of the Application's Rollouts, labeled by result",
            rollout_lookup.clone(),
        );

        // It covers only the certificate running out. A re-issued CA leaving
        // the served leaf off the published caBundle is invisible from here,
        // because the handshake fails before any request reaches this process.
        let certificate_expiry = Gauge::default();
        registry.register(
            "webhook_certificate_expiry_seconds",
            "Expiry of the serving certificate the webhook currently has loaded, as a unix timestamp",
            certificate_expiry.clone(),
        );

        Self {
            registry: Mutex::new(registry),
            decisions,
            admission_requests,
            lookup_failures,
            events,
            admission_duration,
            rollout_lookup,
            certificate_expiry,
        }
    }

    /// Counts one verdict.
    pub fn record_decision(&self, verdict: &Decision, mode: Mode) {
        self.decisions
            .get_or_create(&DecisionLabels {
                code: verdict.code.to_string(),
                allowed: verdict.allowed.to_string(),
                mode: mode.as_str().to_string(),
            })
            .inc();
    }

    /// Counts one admission request outcome.
    pub fn record_admission(&self, outcome: &str) {
        self.admission_requests
            .get_or_create(&OutcomeLabels {
                outcome: outcome.to_string(),
            })
            .inc();
    }

    /// Counts one failed fact lookup.
    pub fn record_lookup_failure(&self, kind: &str) {
        self.lookup_failures
            .get_or_create(&KindLabels {
                kind: kind.to_string(),
            })
            .inc();
    }

    /// Counts one Kubernetes Event handed to the writer.
    ///
    /// It counts submissions, not writes. Delivery is asynchronous and best
    /// effort, so this is the gate's intent rather than what landed in etcd.
    pub fn record_event(&self, reason: &str, event_type: &str) {
        self.events
            .get_or_create(&EventLabels {
                reason: reason.to_string(),
                r#type: event_type.to_string(),
            })
            .inc();
    }

    /// Records how long one admission request took.
    ///
    /// This is the number the webhook's `timeoutSeconds` has to cover. A denial
    /// that arrives after the API server gave up is indistinguishable from an
    /// outage, and with `failurePolicy: Fail` both block the sync for reasons
    /// nobody can read.
    pub fn observe_admission(&self, outcome: &str, elapsed: Duration) {
        self.admission_duration
            .get_or_create(&OutcomeLabels {
                outcome: outcome.to_string(),
            })
            .observe(elapsed.as_secs_f64());
    }

    /// Records the Kubernetes list of the Application's Rollouts.
    pub fn observe_rollout_lookup(&self, result: &str, elapsed: Duration) {
        self.rollout_lookup
            .get_or_create(&ResultLabels {
                result: result.to_string(),
            })
            .observe(elapsed.as_secs_f64());
    }

    /// Publishes the webhook certificate's expiry as a unix timestamp. Zero
    /// means no leaf is loaded or it could not be parsed.
    pub fn set_certificate_expiry(&self, not_after_unix: i64) {
        self.certificate_expiry.set(not_after_unix);
    }

    /// Renders the registry in the Prometheus text exposition format.
    pub fn encode(&self) -> Result<String, std::fmt::Error> {
        let mut out = String::new();
        let registry = self.registry.lock().map_err(|_| std::fmt::Error)?;
        encode(&mut out, &registry)?;
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gate::Code;

    #[test]
    fn every_metric_is_exposed_under_the_namespace() {
        let m = Metrics::new();
        let verdict = Decision {
            app: "prd-api".into(),
            namespace: "payments".into(),
            allowed: false,
            code: Code::CanaryInProgress,
            message: String::new(),
            warnings: Vec::new(),
            rollouts: Vec::new(),
        };
        m.record_decision(&verdict, Mode::Enforce);
        m.record_admission("allowed");
        m.record_lookup_failure("rollouts");
        m.record_event("CanaryBlocked", "Warning");
        m.observe_admission("denied", Duration::from_millis(12));
        m.observe_rollout_lookup("ok", Duration::from_millis(3));
        m.set_certificate_expiry(1_700_000_000);

        let text = m.encode().unwrap();
        for needle in [
            r#"argocd_canary_gate_decisions_total{code="CanaryInProgress",allowed="false",mode="enforce"} 1"#,
            r#"argocd_canary_gate_admission_requests_total{outcome="allowed"} 1"#,
            r#"argocd_canary_gate_lookup_failures_total{kind="rollouts"} 1"#,
            r#"argocd_canary_gate_events_total{reason="CanaryBlocked",type="Warning"} 1"#,
            r#"argocd_canary_gate_admission_duration_seconds_count{outcome="denied"} 1"#,
            r#"argocd_canary_gate_rollout_lookup_duration_seconds_count{result="ok"} 1"#,
            "argocd_canary_gate_webhook_certificate_expiry_seconds 1700000000",
        ] {
            assert!(text.contains(needle), "missing {needle} in:\n{text}");
        }
        assert!(Metrics::default().encode().is_ok());
    }
}
