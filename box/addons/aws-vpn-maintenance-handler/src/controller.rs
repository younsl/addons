//! The reconcile loop that owns Site-to-Site VPN tunnel endpoint maintenance:
//! read telemetry and pending maintenance, apply the safety rules, ask a human
//! over Slack, then replace and verify.
//!
//! Discovery keeps polling on its interval while an approved replacement runs
//! in its own task, which may outlive several passes. A busy flag allows one
//! worker at a time; with leader election and the persisted in-flight record,
//! that makes "one replacement at a time" hold across passes, replicas, and
//! restarts.

pub mod detected;
pub mod maintenance;
pub mod progress;
pub mod proposal;
pub mod reporter;
#[cfg(test)]
mod tests;

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use chrono::Utc;
use serde_json::Value;
use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn};

use crate::approval::Broker;
use crate::aws::client::ApiError;
use crate::aws::{Connection, DiscoverInput, TagFilter, TunnelStatus};
use crate::config::Config;
use crate::executor::{Outcome, Replacer};
use crate::k8s::Store;
use crate::k8s::events::Recorder;
use crate::observability::{Metrics, TunnelSample};
use crate::planner::{self, Candidate, ConnectionState, Reason, Thresholds};
use crate::promx::{Assessment, Gate, Vars};
use crate::slack::{MessageRef, Notice};
use crate::window::Window;

/// The AWS surface the controller reads.
#[async_trait]
pub trait VpnApi: Send + Sync {
    async fn discover(&self, input: &DiscoverInput) -> Result<Vec<Connection>, ApiError>;
    async fn describe(&self, connection_id: &str) -> Result<Connection, ApiError>;
    async fn statuses(&self, conn: &Connection) -> Result<Vec<TunnelStatus>, ApiError>;
}

#[async_trait]
impl VpnApi for crate::aws::Client {
    async fn discover(&self, input: &DiscoverInput) -> Result<Vec<Connection>, ApiError> {
        Self::discover(self, input).await
    }
    async fn describe(&self, connection_id: &str) -> Result<Connection, ApiError> {
        Self::describe(self, connection_id).await
    }
    async fn statuses(&self, conn: &Connection) -> Result<Vec<TunnelStatus>, ApiError> {
        Self::statuses(self, conn).await
    }
}

/// The Slack surface the controller needs.
#[async_trait]
pub trait Notifier: Send + Sync {
    async fn broadcast(
        &self,
        channel_ids: &[String],
        fallback: &str,
        blocks: &[Value],
    ) -> Vec<MessageRef>;
    async fn reply(&self, refs: &[MessageRef], notice: &Notice);
    async fn update(&self, refs: &[MessageRef], fallback: &str, blocks: &[Value]);
}

#[async_trait]
impl Notifier for crate::slack::Client {
    async fn broadcast(
        &self,
        channel_ids: &[String],
        fallback: &str,
        blocks: &[Value],
    ) -> Vec<MessageRef> {
        Self::broadcast(self, channel_ids, fallback, blocks).await
    }
    async fn reply(&self, refs: &[MessageRef], notice: &Notice) {
        Self::reply(self, refs, notice).await;
    }
    async fn update(&self, refs: &[MessageRef], fallback: &str, blocks: &[Value]) {
        Self::update(self, refs, fallback, blocks).await;
    }
}

/// The controller's collaborators.
pub struct Options {
    pub config: Arc<Config>,
    pub aws: Arc<dyn VpnApi>,
    pub exec: Arc<dyn Replacer>,
    pub store: Arc<Store>,
    pub slack: Arc<dyn Notifier>,
    pub broker: Arc<Broker>,
    pub window: Arc<Window>,
    pub traffic: Arc<Gate>,
    pub metrics: Arc<Metrics>,
    pub events: Arc<dyn Recorder>,
    /// The approvers' DM channels, resolved once at startup.
    pub dm_channels: Vec<String>,
    /// Overrides how often an outstanding approval is re-checked; tests shrink
    /// it.
    pub revalidate_interval: Option<Duration>,
}

/// Reconciles VPN tunnel maintenance.
pub struct Controller {
    cfg: Arc<Config>,
    aws: Arc<dyn VpnApi>,
    exec: Arc<dyn Replacer>,
    store: Arc<Store>,
    slack: Arc<dyn Notifier>,
    broker: Arc<Broker>,
    window: Arc<Window>,
    traffic: Arc<Gate>,
    metrics: Arc<Metrics>,
    events: Arc<dyn Recorder>,
    dm_channels: Vec<String>,
    /// Held for the whole approve-and-replace cycle.
    busy: AtomicBool,
    revalidate_interval: Duration,
}

impl Controller {
    #[must_use]
    pub fn new(o: Options) -> Arc<Self> {
        Arc::new(Self {
            cfg: o.config,
            aws: o.aws,
            exec: o.exec,
            store: o.store,
            slack: o.slack,
            broker: o.broker,
            window: o.window,
            traffic: o.traffic,
            metrics: o.metrics,
            events: o.events,
            dm_channels: o.dm_channels,
            busy: AtomicBool::new(false),
            revalidate_interval: o
                .revalidate_interval
                .unwrap_or(maintenance::REVALIDATE_INTERVAL),
        })
    }

    /// Reconciles until `shutdown` fires. Recovering an interrupted replacement
    /// comes first: that tunnel is down with nobody watching it.
    pub async fn run(self: &Arc<Self>, shutdown: CancellationToken) {
        self.resume_in_flight(shutdown.clone()).await;

        let mut ticker = tokio::time::interval(self.cfg.reconcile_interval.get());
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                () = shutdown.cancelled() => {
                    info!("reconcile loop stopped");
                    return;
                }
                _ = ticker.tick() => self.reconcile_once(shutdown.clone()).await,
            }
        }
    }

    /// One pass, with the failure logged rather than propagated.
    pub async fn reconcile_once(self: &Arc<Self>, shutdown: CancellationToken) {
        self.metrics.observe_reconcile();
        if let Err(err) = self.reconcile(shutdown.clone()).await
            && !shutdown.is_cancelled()
        {
            error!(error = %err, "reconcile pass failed");
        }
    }

    /// Discover, read status, publish metrics, evaluate, and hand off at most
    /// one candidate to the maintenance worker.
    async fn reconcile(self: &Arc<Self>, shutdown: CancellationToken) -> Result<(), String> {
        let now = Utc::now();
        let (open, window_detail) = self.window.open(now);
        self.metrics.set_window(open, self.window.remaining(now));

        let conns = match self.aws.discover(&self.discover_input()).await {
            Ok(c) => c,
            Err(err) => {
                self.metrics.observe_reconcile_error("discover");
                return Err(err.to_string());
            }
        };
        self.metrics.set_connections(conns.len());
        self.metrics.reset_tunnels();

        let mut statuses: HashMap<String, Vec<TunnelStatus>> = HashMap::with_capacity(conns.len());
        for conn in &conns {
            let st = match self.aws.statuses(conn).await {
                Ok(st) => st,
                Err(err) => {
                    // Skip this connection rather than blinding the pass to the
                    // others.
                    self.metrics.observe_reconcile_error("maintenance_status");
                    error!(vpn_connection_id = %conn.id, error = %err, "failed to read tunnel maintenance status");
                    continue;
                }
            };
            for s in &st {
                self.metrics.set_tunnel(&TunnelSample {
                    connection_id: conn.id.clone(),
                    connection_name: conn.name.clone(),
                    tunnel_ip: s.tunnel.outside_ip.clone(),
                    up: s.tunnel.up,
                    routes: s.tunnel.accepted_routes,
                    pending: s.maintenance.pending,
                    deadline: s.maintenance.auto_applied_after,
                    lifecycle_control: s.tunnel.lifecycle_control,
                });
            }
            statuses.insert(conn.id.clone(), st);
        }

        let snap = match self.store.load().await {
            Ok(s) => s,
            Err(err) => {
                self.metrics.observe_reconcile_error("state_load");
                return Err(err.to_string());
            }
        };
        self.metrics.set_in_flight(snap.in_flight.is_some());

        let plan = planner::evaluate(&planner::Input {
            now,
            connections: conns,
            statuses,
            window_open: open,
            window_detail,
            replacement_in_flight: snap.in_flight.is_some() || self.busy.load(Ordering::SeqCst),
            awaiting_approval: self.broker.pending(),
            history: history_from(&snap),
            thresholds: self.thresholds(),
        });

        for b in plan.held() {
            self.metrics.observe_blocked(b.reason.as_str());
            if b.reason == Reason::LifecycleControlDisabled {
                // Not transient: this tunnel can never be taken over until
                // someone changes its options.
                warn!(vpn_connection_id = %b.connection_id, tunnel_ip = %b.tunnel_ip, "tunnel cannot be managed: endpoint lifecycle control is disabled");
                continue;
            }
            info!(
                vpn_connection_id = %b.connection_id,
                tunnel_ip = %b.tunnel_ip,
                reason = %b.reason,
                detail = %b.detail,
                "pending maintenance held back by a preflight rule"
            );
        }

        // Serialized, so only one candidate is acted on; the rest are
        // re-evaluated next pass against fresh telemetry.
        let picked = self.quietest_candidate(&plan.candidates).await;
        let proposing = picked
            .as_ref()
            .map(|(c, _)| c.request_id.clone())
            .unwrap_or_default();
        if let Some((cand, assessment)) = picked {
            if plan.candidates.len() > 1 {
                info!(eligible = plan.candidates.len(), selected = %cand.label(), "multiple tunnels are eligible; taking the most urgent quiet one");
            }
            self.start_maintenance(cand, assessment, shutdown);
        }

        // Last, and after the handoff: a notice is a courtesy, and its Slack
        // and ConfigMap calls must not sit in front of the replacement that
        // this pass exists to start.
        self.notify_detected(&plan, &snap, &proposing).await;
        Ok(())
    }

    /// Walks the candidates in urgency order and returns the first one the
    /// traffic gate clears. Walking in order rather than picking the globally
    /// quietest tunnel keeps the AWS deadline as the primary priority: being
    /// quiet is a permission, not a ranking.
    async fn quietest_candidate(
        &self,
        candidates: &[Candidate],
    ) -> Option<(Candidate, Assessment)> {
        for cand in candidates {
            let assessment = self.traffic.evaluate(&self.traffic_vars(cand)).await;
            if assessment.evaluated {
                self.metrics.observe_traffic_gate(
                    assessment.allowed,
                    assessment.ratio,
                    assessment.rank,
                    assessment.has_history,
                );
            }
            if assessment.allowed {
                return Some((cand.clone(), assessment));
            }
            self.metrics.observe_blocked(Reason::TrafficHigh.as_str());
            info!(
                vpn_connection_id = %cand.connection.id,
                tunnel_ip = %cand.tunnel.outside_ip,
                detail = %assessment.detail,
                "candidate held back by the traffic gate"
            );
        }
        None
    }

    /// Describes a candidate to the traffic gate. The window travels with the
    /// request because the gate compares the present moment against past
    /// window moments only.
    fn traffic_vars(&self, cand: &Candidate) -> Vars {
        let window = self.window.clone();
        Vars {
            vpn_connection_id: cand.connection.id.clone(),
            in_window: Some(Arc::new(move |t| window.contains(t))),
            tz: Some(self.window.timezone()),
            urgent: cand.escalate,
        }
    }

    /// Launches the approve-and-replace worker unless one is running.
    /// `compare_exchange` stops two overlapping passes both starting one.
    fn start_maintenance(
        self: &Arc<Self>,
        cand: Candidate,
        assessment: Assessment,
        shutdown: CancellationToken,
    ) {
        if self
            .busy
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_err()
        {
            return;
        }
        let this = self.clone();
        tokio::spawn(async move {
            this.run_maintenance(cand, assessment, shutdown).await;
            this.busy.store(false, Ordering::SeqCst);
        });
    }

    fn discover_input(&self) -> DiscoverInput {
        DiscoverInput {
            tag_filters: self
                .cfg
                .targets
                .tag_filters
                .iter()
                .map(|f| TagFilter {
                    key: f.key.clone(),
                    value: f.value.clone(),
                })
                .collect(),
            exclude_ids: self.cfg.targets.exclude_connection_ids.clone(),
        }
    }

    fn thresholds(&self) -> Thresholds {
        Thresholds {
            peer_min_stable_for: self.cfg.safety.peer_min_stable_for.get(),
            peer_min_accepted_routes: self.cfg.safety.peer_min_accepted_routes,
            per_connection_cooldown: self.cfg.safety.per_connection_cooldown.get(),
            chain_sibling_tunnel: self.cfg.safety.chain_sibling_tunnel,
            escalate_before: self.cfg.safety.escalate_before.get(),
        }
    }

    /// Whether a worker holds the replacement slot.
    #[cfg(test)]
    pub fn is_busy(&self) -> bool {
        self.busy.load(Ordering::SeqCst)
    }

    /// Reports which VPN connections and tunnels this controller manages.
    ///
    /// It runs at startup on every replica, before leader election, because
    /// the most expensive mistake here is a silent one: a wrong tag filter or
    /// a tunnel without lifecycle control both look exactly like "nothing
    /// needed doing". A failure is not fatal; the reconcile loop retries.
    pub async fn log_scope(&self) {
        let conns = match self.aws.discover(&self.discover_input()).await {
            Ok(c) => c,
            Err(err) => {
                warn!(error = %err, "could not list managed VPN connections at startup; the reconcile loop will retry");
                return;
            }
        };
        if conns.is_empty() {
            warn!(
                tag_filters = %self.tag_filter_summary(),
                excluded = self.cfg.targets.exclude_connection_ids.len(),
                "no VPN connections match the configured tag filters, so nothing is managed"
            );
            return;
        }

        let mut unmanaged = 0;
        for conn in &conns {
            let statuses = match self.aws.statuses(conn).await {
                Ok(s) => s,
                Err(err) => {
                    warn!(vpn_connection_id = %conn.id, error = %err, "could not read tunnel maintenance status at startup");
                    continue;
                }
            };
            let tunnels: Vec<String> = statuses
                .iter()
                .map(|s| {
                    let mut state = if s.tunnel.lifecycle_control {
                        "lifecycle_control=on".to_string()
                    } else {
                        unmanaged += 1;
                        "lifecycle_control=OFF".to_string()
                    };
                    if s.maintenance.pending {
                        state.push_str(" maintenance_pending");
                    }
                    format!("{} ({} {state})", s.tunnel.outside_ip, up_down(s.tunnel.up))
                })
                .collect();
            info!(
                vpn_connection_id = %conn.id,
                name = %conn.name,
                routing = routing_mode(conn),
                gateway = %proposal::gateway_of(conn),
                tunnels = %tunnels.join(", "),
                "managing VPN connection"
            );
        }
        info!(connections = conns.len(), tag_filters = %self.tag_filter_summary(), "scope resolved");
        if unmanaged > 0 {
            warn!(
                tunnels = unmanaged,
                "some tunnels have endpoint lifecycle control disabled and can never be replaced early; enable it with ModifyVpnTunnelOptions (EnableTunnelLifecycleControl)"
            );
        }
    }

    fn tag_filter_summary(&self) -> String {
        tag_filter_summary(&self.cfg)
    }
}

/// Renders the tag filters for a log line.
#[must_use]
pub fn tag_filter_summary(cfg: &Config) -> String {
    cfg.targets
        .tag_filters
        .iter()
        .map(|f| {
            if f.value.is_empty() {
                format!("{}=<any>", f.key)
            } else {
                format!("{}={}", f.key, f.value)
            }
        })
        .collect::<Vec<_>>()
        .join(", ")
}

fn history_from(snap: &crate::k8s::Snapshot) -> HashMap<String, ConnectionState> {
    snap.connections
        .iter()
        .map(|(id, rec)| {
            (
                id.clone(),
                ConnectionState {
                    last_replacement_at: rec.last_replacement_at,
                    last_tunnel_ip: rec.last_tunnel_ip.clone(),
                    // Derived from the executor's own verdict rather than by
                    // string comparison, so a new outcome cannot silently
                    // become chainable.
                    last_succeeded: Outcome::parse(&rec.last_result).is_some_and(Outcome::healthy),
                },
            )
        })
        .collect()
}

pub const fn up_down(up: bool) -> &'static str {
    if up { "UP" } else { "DOWN" }
}

pub const fn routing_mode(conn: &Connection) -> &'static str {
    if conn.static_routes_only {
        "static"
    } else {
        "bgp"
    }
}

/// A 30 second grace for the calls that close a run out after cancellation.
pub const FINISH_TIMEOUT: Duration = Duration::from_secs(30);
