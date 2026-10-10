//! The controller's collectors on a private registry.

use std::sync::Mutex;
use std::sync::atomic::{AtomicI64, AtomicU64};
use std::time::Duration;

use chrono::{DateTime, Utc};
use prometheus_client::encoding::{EncodeLabelSet, text::encode};
use prometheus_client::metrics::counter::Counter;
use prometheus_client::metrics::family::Family;
use prometheus_client::metrics::gauge::Gauge;
use prometheus_client::metrics::histogram::Histogram;
use prometheus_client::registry::Registry;

/// Shared by every per-tunnel gauge, so a dashboard can join health against
/// pending maintenance without relabeling.
#[derive(Clone, Debug, Hash, PartialEq, Eq, EncodeLabelSet)]
pub struct TunnelLabels {
    pub vpn_connection_id: String,
    pub vpn_connection_name: String,
    pub tunnel_ip: String,
}

#[derive(Clone, Debug, Hash, PartialEq, Eq, EncodeLabelSet)]
struct StageLabel {
    stage: String,
}

#[derive(Clone, Debug, Hash, PartialEq, Eq, EncodeLabelSet)]
struct ReasonLabel {
    reason: String,
}

#[derive(Clone, Debug, Hash, PartialEq, Eq, EncodeLabelSet)]
struct VerdictLabel {
    verdict: String,
}

#[derive(Clone, Debug, Hash, PartialEq, Eq, EncodeLabelSet)]
struct DecisionLabel {
    decision: String,
}

#[derive(Clone, Debug, Hash, PartialEq, Eq, EncodeLabelSet)]
struct OutcomeLabel {
    outcome: String,
}

type FGauge = Gauge<f64, AtomicU64>;

/// One tunnel's state for a single pass.
#[derive(Debug, Clone, Default)]
pub struct TunnelSample {
    pub connection_id: String,
    pub connection_name: String,
    pub tunnel_ip: String,
    pub up: bool,
    pub routes: i32,
    pub pending: bool,
    pub deadline: Option<DateTime<Utc>>,
    /// Whether this controller can manage the tunnel at all.
    pub lifecycle_control: bool,
}

/// Holds the collectors on a private registry.
pub struct Metrics {
    registry: Mutex<Registry>,

    reconcile_total: Counter,
    reconcile_errors: Family<StageLabel, Counter>,
    connections: Gauge<i64, AtomicI64>,

    tunnel_up: Family<TunnelLabels, Gauge<i64, AtomicI64>>,
    tunnel_routes: Family<TunnelLabels, Gauge<i64, AtomicI64>>,
    tunnel_pending: Family<TunnelLabels, Gauge<i64, AtomicI64>>,
    tunnel_deadline: Family<TunnelLabels, Gauge<i64, AtomicI64>>,
    tunnel_lifecycle: Family<TunnelLabels, Gauge<i64, AtomicI64>>,
    blocked_tunnels: Family<ReasonLabel, Gauge<i64, AtomicI64>>,
    blocked_total: Family<ReasonLabel, Counter>,
    window_open: Gauge<i64, AtomicI64>,
    window_remaining: FGauge,

    notice_total: Family<ReasonLabel, Counter>,
    traffic_gate_total: Family<VerdictLabel, Counter>,
    traffic_ratio: FGauge,
    traffic_percentile: FGauge,
    approval_total: Family<DecisionLabel, Counter>,
    replacement_total: Family<OutcomeLabel, Counter>,
    replacement_duration: Histogram,
    replacement_in_flight: Gauge<i64, AtomicI64>,
    peer_dropped_total: Counter,
}

impl Default for Metrics {
    fn default() -> Self {
        Self::new()
    }
}

impl Metrics {
    /// Builds and registers the collectors.
    #[must_use]
    #[allow(clippy::too_many_lines)]
    pub fn new() -> Self {
        let mut registry = Registry::with_prefix("aws_vpn_maintenance_handler");

        let reconcile_total = Counter::default();
        registry.register(
            "reconcile",
            "Total reconcile passes started.",
            reconcile_total.clone(),
        );
        let reconcile_errors = Family::<StageLabel, Counter>::default();
        registry.register(
            "reconcile_errors",
            "Total reconcile errors by stage.",
            reconcile_errors.clone(),
        );
        let connections = Gauge::default();
        registry.register(
            "managed_connections",
            "Number of tag-matched Site-to-Site VPN connections discovered in the latest pass.",
            connections.clone(),
        );

        let tunnel_up = Family::default();
        registry.register(
            "tunnel_up",
            "Tunnel telemetry status: 1 when UP, 0 when DOWN.",
            tunnel_up.clone(),
        );
        let tunnel_routes = Family::default();
        registry.register(
            "tunnel_accepted_routes",
            "BGP routes accepted on the tunnel. Always 0 on static-routes-only connections, where it carries no health information.",
            tunnel_routes.clone(),
        );
        let tunnel_pending = Family::default();
        registry.register(
            "tunnel_pending_maintenance",
            "1 when AWS reports pending endpoint maintenance for the tunnel, 0 otherwise.",
            tunnel_pending.clone(),
        );
        let tunnel_deadline = Family::default();
        registry.register(
            "tunnel_maintenance_deadline_seconds",
            "Unix timestamp after which AWS applies the pending maintenance itself. 0 when no deadline is published. Alert on this approaching to catch unanswered approvals.",
            tunnel_deadline.clone(),
        );
        let tunnel_lifecycle = Family::default();
        registry.register(
            "tunnel_lifecycle_control",
            "1 when tunnel endpoint lifecycle control is enabled on the tunnel, 0 otherwise. A 0 means AWS applies that tunnel's maintenance on its own schedule and this controller cannot take it over. Alert on this.",
            tunnel_lifecycle.clone(),
        );
        let blocked_tunnels = Family::default();
        registry.register(
            "blocked_tunnels",
            "Tunnels with pending maintenance currently held back by a preflight rule, by reason.",
            blocked_tunnels.clone(),
        );
        let blocked_total = Family::default();
        registry.register(
            "blocked",
            "Total preflight rejections of a tunnel with pending maintenance, by reason.",
            blocked_total.clone(),
        );
        let window_open = Gauge::default();
        registry.register(
            "window_open",
            "1 when the maintenance window is open and long enough to start a replacement, 0 otherwise.",
            window_open.clone(),
        );
        let window_remaining = FGauge::default();
        registry.register(
            "window_remaining_seconds",
            "Seconds left in the current maintenance window, 0 when closed.",
            window_remaining.clone(),
        );

        let notice_total = Family::default();
        registry.register(
            "detection_notices",
            "Total detection notices sent to the approvers, by the reason the tunnel was not being replaced at the time. One per tunnel per maintenance cycle.",
            notice_total.clone(),
        );
        let traffic_gate_total = Family::default();
        registry.register(
            "traffic_gate",
            "Total traffic gate evaluations by verdict (allowed, blocked).",
            traffic_gate_total.clone(),
        );
        let traffic_ratio = FGauge::default();
        registry.register(
            "traffic_ratio",
            "Most recent traffic over the quiet threshold from the traffic gate. Below 1 means the gate would open. Only set when the window's traffic history was readable.",
            traffic_ratio.clone(),
        );
        let traffic_percentile = FGauge::default();
        registry.register(
            "traffic_percentile",
            "Where the most recently measured traffic falls in what the connection carries during its maintenance window, in percent. Compare against the configured quietPercentile.",
            traffic_percentile.clone(),
        );
        let approval_total = Family::default();
        registry.register(
            "approval",
            "Total approval requests resolved, by decision (approved, denied, timeout, expired, aborted). timeout means nobody answered; expired means the preconditions lapsed while the request was outstanding.",
            approval_total.clone(),
        );
        let replacement_total = Family::default();
        registry.register(
            "replacement",
            "Total tunnel replacements attempted, by outcome.",
            replacement_total.clone(),
        );
        // Replacements usually finish in single-digit minutes; the upper
        // buckets show a run heading for a timeout.
        let replacement_duration =
            Histogram::new([30.0, 60.0, 120.0, 300.0, 600.0, 900.0, 1800.0, 3600.0]);
        registry.register(
            "replacement_duration_seconds",
            "Time from the ReplaceVpnTunnel call until the tunnel was verified healthy or gave up.",
            replacement_duration.clone(),
        );
        let replacement_in_flight = Gauge::default();
        registry.register(
            "replacement_in_flight",
            "1 while a tunnel replacement is being performed or verified, 0 otherwise.",
            replacement_in_flight.clone(),
        );
        let peer_dropped_total = Counter::default();
        registry.register(
            "peer_dropped",
            "Total replacements during which the surviving tunnel also went DOWN, leaving the connection with no healthy path. Should stay at 0.",
            peer_dropped_total.clone(),
        );

        Self {
            registry: Mutex::new(registry),
            reconcile_total,
            reconcile_errors,
            connections,
            tunnel_up,
            tunnel_routes,
            tunnel_pending,
            tunnel_deadline,
            tunnel_lifecycle,
            blocked_tunnels,
            blocked_total,
            window_open,
            window_remaining,
            notice_total,
            traffic_gate_total,
            traffic_ratio,
            traffic_percentile,
            approval_total,
            replacement_total,
            replacement_duration,
            replacement_in_flight,
            peer_dropped_total,
        }
    }

    /// Counts a reconcile pass.
    pub fn observe_reconcile(&self) {
        self.reconcile_total.inc();
    }

    /// Counts a failure in a named reconcile stage.
    pub fn observe_reconcile_error(&self, stage: &str) {
        self.reconcile_errors
            .get_or_create(&StageLabel {
                stage: stage.into(),
            })
            .inc();
    }

    /// Records how many managed connections were discovered.
    pub fn set_connections(&self, n: usize) {
        self.connections.set(i64::try_from(n).unwrap_or(i64::MAX));
    }

    /// Clears the per-tunnel series before a pass repopulates them, so an
    /// untagged or deleted connection stops reporting instead of freezing.
    pub fn reset_tunnels(&self) {
        self.tunnel_up.clear();
        self.tunnel_routes.clear();
        self.tunnel_pending.clear();
        self.tunnel_deadline.clear();
        self.tunnel_lifecycle.clear();
        self.blocked_tunnels.clear();
    }

    /// Records one tunnel's telemetry and maintenance state.
    pub fn set_tunnel(&self, s: &TunnelSample) {
        let labels = TunnelLabels {
            vpn_connection_id: s.connection_id.clone(),
            vpn_connection_name: s.connection_name.clone(),
            tunnel_ip: s.tunnel_ip.clone(),
        };
        self.tunnel_up.get_or_create(&labels).set(i64::from(s.up));
        self.tunnel_routes
            .get_or_create(&labels)
            .set(i64::from(s.routes));
        self.tunnel_pending
            .get_or_create(&labels)
            .set(i64::from(s.pending));
        self.tunnel_lifecycle
            .get_or_create(&labels)
            .set(i64::from(s.lifecycle_control));
        self.tunnel_deadline
            .get_or_create(&labels)
            .set(s.deadline.map_or(0, |d| d.timestamp()));
    }

    /// Records a tunnel with pending maintenance held back by a rule.
    pub fn observe_blocked(&self, reason: &str) {
        let label = ReasonLabel {
            reason: reason.into(),
        };
        self.blocked_tunnels.get_or_create(&label).inc();
        self.blocked_total.get_or_create(&label).inc();
    }

    /// Records a detection notice delivered to the approvers.
    pub fn observe_detection_notice(&self, reason: &str) {
        self.notice_total
            .get_or_create(&ReasonLabel {
                reason: reason.into(),
            })
            .inc();
    }

    /// Records one traffic gate verdict. The two gauges are only set when the
    /// verdict came from a distribution: an `onError` verdict has no measured
    /// position, and leaving the previous value in place is more honest than
    /// writing a zero that would read as a perfectly quiet tunnel.
    pub fn observe_traffic_gate(
        &self,
        allowed: bool,
        ratio: f64,
        percentile: f64,
        has_history: bool,
    ) {
        let verdict = if allowed { "allowed" } else { "blocked" };
        self.traffic_gate_total
            .get_or_create(&VerdictLabel {
                verdict: verdict.into(),
            })
            .inc();
        if has_history {
            self.traffic_ratio.set(ratio);
            self.traffic_percentile.set(percentile);
        }
    }

    /// Records the maintenance window state.
    pub fn set_window(&self, open: bool, remaining: Duration) {
        self.window_open.set(i64::from(open));
        self.window_remaining.set(remaining.as_secs_f64());
    }

    /// Counts a resolved approval request.
    pub fn observe_approval(&self, decision: &str) {
        self.approval_total
            .get_or_create(&DecisionLabel {
                decision: decision.into(),
            })
            .inc();
    }

    /// Records a finished replacement.
    pub fn observe_replacement(&self, outcome: &str, d: Duration, peer_dropped: bool) {
        self.replacement_total
            .get_or_create(&OutcomeLabel {
                outcome: outcome.into(),
            })
            .inc();
        self.replacement_duration.observe(d.as_secs_f64());
        if peer_dropped {
            self.peer_dropped_total.inc();
        }
    }

    /// Records whether a replacement is running.
    pub fn set_in_flight(&self, in_flight: bool) {
        self.replacement_in_flight.set(i64::from(in_flight));
    }

    /// Renders the registry in the text exposition format.
    #[must_use]
    pub fn render(&self) -> String {
        let mut out = String::new();
        let registry = self
            .registry
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if encode(&mut out, &registry).is_err() {
            out.clear();
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn records_and_renders_everything() {
        let m = Metrics::new();
        m.observe_reconcile();
        m.observe_reconcile_error("discover");
        m.set_connections(2);
        m.set_tunnel(&TunnelSample {
            connection_id: "vpn-1".into(),
            connection_name: "prod".into(),
            tunnel_ip: "1.1.1.1".into(),
            up: true,
            routes: 4,
            pending: true,
            deadline: Some(DateTime::from_timestamp(1_785_000_000, 0).unwrap()),
            lifecycle_control: false,
        });
        m.observe_blocked("peer_down");
        m.observe_detection_notice("window_closed");
        m.observe_traffic_gate(false, 2.5, 80.0, true);
        m.observe_traffic_gate(true, 0.0, 0.0, false);
        m.set_window(true, Duration::from_secs(90));
        m.observe_approval("approved");
        m.observe_replacement("succeeded", Duration::from_secs(200), true);
        m.set_in_flight(true);

        let text = m.render();
        for want in [
            "aws_vpn_maintenance_handler_reconcile_total 1",
            "aws_vpn_maintenance_handler_reconcile_errors_total{stage=\"discover\"} 1",
            "aws_vpn_maintenance_handler_managed_connections 2",
            "aws_vpn_maintenance_handler_tunnel_up{vpn_connection_id=\"vpn-1\",vpn_connection_name=\"prod\",tunnel_ip=\"1.1.1.1\"} 1",
            "aws_vpn_maintenance_handler_tunnel_accepted_routes{vpn_connection_id=\"vpn-1\",vpn_connection_name=\"prod\",tunnel_ip=\"1.1.1.1\"} 4",
            "aws_vpn_maintenance_handler_tunnel_pending_maintenance{vpn_connection_id=\"vpn-1\",vpn_connection_name=\"prod\",tunnel_ip=\"1.1.1.1\"} 1",
            "aws_vpn_maintenance_handler_tunnel_maintenance_deadline_seconds{vpn_connection_id=\"vpn-1\",vpn_connection_name=\"prod\",tunnel_ip=\"1.1.1.1\"} 1785000000",
            "aws_vpn_maintenance_handler_tunnel_lifecycle_control{vpn_connection_id=\"vpn-1\",vpn_connection_name=\"prod\",tunnel_ip=\"1.1.1.1\"} 0",
            "aws_vpn_maintenance_handler_blocked_tunnels{reason=\"peer_down\"} 1",
            "aws_vpn_maintenance_handler_blocked_total{reason=\"peer_down\"} 1",
            "aws_vpn_maintenance_handler_detection_notices_total{reason=\"window_closed\"} 1",
            "aws_vpn_maintenance_handler_traffic_gate_total{verdict=\"blocked\"} 1",
            "aws_vpn_maintenance_handler_traffic_gate_total{verdict=\"allowed\"} 1",
            "aws_vpn_maintenance_handler_traffic_ratio 2.5",
            "aws_vpn_maintenance_handler_traffic_percentile 80.0",
            "aws_vpn_maintenance_handler_window_open 1",
            "aws_vpn_maintenance_handler_window_remaining_seconds 90.0",
            "aws_vpn_maintenance_handler_approval_total{decision=\"approved\"} 1",
            "aws_vpn_maintenance_handler_replacement_total{outcome=\"succeeded\"} 1",
            "aws_vpn_maintenance_handler_replacement_duration_seconds_bucket{le=\"300.0\"} 1",
            "aws_vpn_maintenance_handler_replacement_in_flight 1",
            "aws_vpn_maintenance_handler_peer_dropped_total 1",
        ] {
            assert!(text.contains(want), "missing {want:?} in:\n{text}");
        }

        m.reset_tunnels();
        let text = m.render();
        assert!(!text.contains("tunnel_ip=\"1.1.1.1\""), "{text}");
        assert!(!text.contains("blocked_tunnels{"), "{text}");
        assert!(
            text.contains("blocked_total{reason=\"peer_down\"} 1"),
            "counters survive the reset"
        );
        let _ = Metrics::default();
    }
}
