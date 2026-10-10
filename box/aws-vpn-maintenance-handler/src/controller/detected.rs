//! Tells the approvers about queued maintenance that is not being proposed
//! right now, once per connection per maintenance cycle.
//!
//! Without it the metrics are the only place pending maintenance appears until
//! a window opens, so the first thing an approver sees is an approval card,
//! possibly days after AWS queued the work and hours before its deadline. The
//! notice is deliberately not an approval card: it carries no buttons, because
//! the preflight evidence a decision needs does not exist while the connection
//! is still blocked.

use std::collections::{BTreeMap, HashSet};

use chrono::Utc;
use tracing::{error, info, warn};

use super::Controller;
use crate::k8s::Snapshot;
use crate::k8s::events::reason;
use crate::planner::{Plan, Reason};
use crate::slack::{Detected, DetectedTunnel, detected_blocks};

/// One VPN connection's queued maintenance, and whether the approvers should
/// hear about it.
#[derive(Debug, Clone)]
pub struct Detection {
    /// The identity the notice is remembered under: every covered tunnel's
    /// request ID, joined. Scoping it to the whole group is what makes the
    /// notice once-per-connection and still re-send when the set of queued
    /// tunnels changes.
    pub notice_id: String,
    pub connection_id: String,
    /// The machine-readable planner reason of the tunnel AWS takes over first,
    /// for logs and the metric label.
    pub reason: String,
    /// False when the controller is already working on this connection, which
    /// needs no notice because the approval card is doing the telling.
    pub notify: bool,
    pub detected: Detected,
}

impl Detection {
    fn tunnel_ips(&self) -> String {
        self.detected
            .tunnels
            .iter()
            .map(|t| t.ip.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    }
}

/// Pairs one tunnel's display form with the planner facts the grouping needs.
struct TunnelEntry {
    request_id: String,
    reason: Reason,
    detected: DetectedTunnel,
}

impl Controller {
    /// Delivery is not retried. A notice is a courtesy ahead of the card that
    /// actually gates the replacement, and a failed one must not hold up the
    /// pass; the state write below only happens for a notice Slack accepted,
    /// so a total delivery failure is retried on the next pass anyway.
    pub(super) async fn notify_detected(&self, plan: &Plan, snap: &Snapshot, proposing: &str) {
        let groups = self.detections(plan, proposing);
        let mut live = HashSet::with_capacity(groups.len());
        for g in &groups {
            // Every notice ID this pass saw stays live, including the
            // suppressed ones: forgetting a suppressed notice would send it
            // again as soon as the connection went back to being merely
            // blocked.
            live.insert(g.notice_id.clone());
            if !g.notify || snap.notices.contains_key(&g.notice_id) {
                continue;
            }

            let (fallback, blocks) = detected_blocks(&g.detected);
            let refs = self
                .slack
                .broadcast(&self.dm_channels, &fallback, &blocks)
                .await;
            if refs.is_empty() {
                warn!(vpn_connection_id = %g.connection_id, tunnels = %g.tunnel_ips(), "could not deliver the maintenance notice to any approver");
                self.metrics.observe_reconcile_error("notice_delivery");
                continue;
            }
            if let Err(err) = self.store.add_notice(&g.notice_id, Utc::now()).await {
                // Recorded but not persisted means it will be sent again next
                // pass. Noisy, not wrong.
                error!(error = %err, notice_id = %g.notice_id, "failed to persist the maintenance notice");
            }
            info!(
                approvers = refs.len(),
                vpn_connection_id = %g.connection_id,
                tunnels = %g.tunnel_ips(),
                reason = %g.reason,
                "maintenance notice sent"
            );
            self.metrics.observe_detection_notice(&g.reason);
            self.events.normal(
                reason::MAINTENANCE_DETECTED,
                format!(
                    "AWS has queued endpoint maintenance for {} of {} ({})",
                    g.tunnel_ips(),
                    g.detected.target(),
                    g.reason
                ),
            );
        }

        // Only written when there is something stale, since a mutation writes
        // the ConfigMap unconditionally.
        if snap.notices.keys().any(|id| !live.contains(id))
            && let Err(err) = self.store.prune_notices(&live).await
        {
            error!(error = %err, "failed to prune finished maintenance notices");
        }
    }

    /// Groups every tunnel with maintenance queued by VPN connection: the ones a
    /// preflight rule held back, the ones lifecycle control rules out entirely,
    /// and the candidates that were eligible this pass.
    ///
    /// Candidates are here because being eligible is not the same as being
    /// acted on. They are only notified about when nothing was proposed at
    /// all, which is the traffic gate holding the whole window. `proposing` is
    /// the request ID whose approval card is going out in this pass, or empty.
    pub(super) fn detections(&self, plan: &Plan, proposing: &str) -> Vec<Detection> {
        let now = Utc::now();
        let escalate_before = self.cfg.safety.escalate_before.get();

        let mut by_connection: BTreeMap<String, Vec<TunnelEntry>> = BTreeMap::new();
        let mut names: BTreeMap<String, String> = BTreeMap::new();
        // A connection the controller is already working on is suppressed as a
        // whole: the card that exists covers it.
        let mut suppressed: HashSet<String> = HashSet::new();

        for b in plan.held() {
            names.insert(b.connection_id.clone(), b.connection_name.clone());
            if !notifiable(b.reason) {
                suppressed.insert(b.connection_id.clone());
            }
            by_connection
                .entry(b.connection_id.clone())
                .or_default()
                .push(TunnelEntry {
                    request_id: b.request_id.clone(),
                    reason: b.reason,
                    detected: DetectedTunnel {
                        ip: b.tunnel_ip.clone(),
                        deadline: b.deadline,
                        deadline_in: b.deadline_in,
                        reason: b.detail.clone(),
                        escalate: !b.deadline_in.is_zero() && b.deadline_in <= escalate_before,
                        unmanageable: b.reason == Reason::LifecycleControlDisabled,
                    },
                });
        }

        for cand in &plan.candidates {
            names.insert(cand.connection.id.clone(), cand.connection.name.clone());
            if cand.request_id == proposing {
                suppressed.insert(cand.connection.id.clone());
            }
            by_connection.entry(cand.connection.id.clone()).or_default().push(TunnelEntry {
                request_id: cand.request_id.clone(),
                reason: Reason::TrafficHigh,
                detected: DetectedTunnel {
                    ip: cand.tunnel.outside_ip.clone(),
                    deadline: cand.maintenance.auto_applied_after,
                    deadline_in: cand.deadline_in,
                    reason: "every preflight check passes, but the replacement has not started yet: replacements run one at a time and the traffic gate waits for the tunnel to be quiet".to_string(),
                    escalate: cand.escalate,
                    unmanageable: false,
                },
            });
        }

        let next_window = self.next_window(now);
        let mut out: Vec<Detection> = by_connection
            .into_iter()
            .map(|(id, mut tunnels)| {
                // Sorted so the notice, its ID, and the logs read the same on
                // every pass.
                tunnels.sort_by(|a, b| a.detected.ip.cmp(&b.detected.ip));
                let notice_id = tunnels
                    .iter()
                    .map(|t| t.request_id.as_str())
                    .collect::<Vec<_>>()
                    .join(" ");
                let reason = urgent_reason(&tunnels).as_str().to_string();
                let shown: Vec<DetectedTunnel> = tunnels.into_iter().map(|t| t.detected).collect();
                Detection {
                    notice_id: notice_id.clone(),
                    connection_id: id.clone(),
                    reason,
                    notify: !suppressed.contains(&id),
                    detected: Detected {
                        notice_id,
                        connection_id: id.clone(),
                        connection_name: names.get(&id).cloned().unwrap_or_default(),
                        region: self.cfg.region.clone(),
                        tunnels: shown,
                        next_window,
                        window: self.window.to_string(),
                    },
                }
            })
            .collect();
        out.sort_by(|a, b| a.notice_id.cmp(&b.notice_id));
        out
    }

    /// The next opening, or `None` while the window is open, where naming a
    /// future opening would read as the wait being the schedule's fault.
    fn next_window(&self, now: chrono::DateTime<Utc>) -> Option<chrono::DateTime<Utc>> {
        if self.window.open(now).0 {
            None
        } else {
            self.window.next_open(now)
        }
    }
}

/// The reason of the tunnel AWS takes over first, which is the one worth
/// counting for a notice covering several. An unpublished deadline never wins.
fn urgent_reason(tunnels: &[TunnelEntry]) -> Reason {
    let mut best = &tunnels[0];
    for t in &tunnels[1..] {
        match (best.detected.deadline, t.detected.deadline) {
            (None, Some(_)) => best = t,
            (Some(b), Some(d)) if d < b => best = t,
            _ => {}
        }
    }
    best.reason
}

/// Whether a blocked reason is worth a notice. A tunnel awaiting approval
/// already has a card in front of the same people, and one held back because a
/// replacement is running is a queueing detail of a window that is actively
/// working.
const fn notifiable(r: Reason) -> bool {
    !matches!(r, Reason::AwaitingApproval | Reason::ReplacementInFlight)
}
