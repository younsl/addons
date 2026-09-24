//! Controller tests over in-memory collaborators: a scripted AWS, a recording
//! Slack, an in-memory `ConfigMap`, and a scripted replacer.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use chrono::Utc;
use serde_json::Value;
use tokio::time::sleep;
use tokio_util::sync::CancellationToken;

use super::*;
use crate::approval::Broker;
use crate::aws::client::ApiError;
use crate::aws::{Connection, DiscoverInput, Maintenance, TunnelStatus};
use crate::config::{self, Config, Env};
use crate::executor::{ExecResult, Outcome, Replacer, Reporter, Request};
use crate::k8s::events::Recorder;
use crate::k8s::state::ConnectionRecord;
use crate::k8s::state::fake::{MemoryConfigMap, store};
use crate::k8s::{Approval, InFlight, Phase, Snapshot, Store};
use crate::observability::Metrics;
use crate::planner::fixtures::{
    connection, now as fixed_now, pending, statuses as make_statuses, tunnel,
};
use crate::promx::Gate;
use crate::slack::{Interaction, MessageRef, Notice};
use crate::window::{Window, WindowConfig};

#[derive(Default)]
struct FakeVpn {
    connections: Mutex<Vec<Connection>>,
    statuses: Mutex<HashMap<String, Vec<TunnelStatus>>>,
    discover_error: Mutex<Option<String>>,
    describe_error: Mutex<Option<String>>,
    status_error: Mutex<Option<String>>,
}

impl FakeVpn {
    fn set(&self, conn: Connection, m: &[Maintenance]) {
        let st = make_statuses(&conn, m);
        self.statuses.lock().unwrap().insert(conn.id.clone(), st);
        let mut conns = self.connections.lock().unwrap();
        conns.retain(|c| c.id != conn.id);
        conns.push(conn);
    }
}

#[async_trait]
impl VpnApi for FakeVpn {
    async fn discover(&self, _input: &DiscoverInput) -> Result<Vec<Connection>, ApiError> {
        let err = self.discover_error.lock().unwrap().clone();
        if let Some(err) = err {
            return Err(ApiError::Rejected(err));
        }
        Ok(self.connections.lock().unwrap().clone())
    }
    async fn describe(&self, connection_id: &str) -> Result<Connection, ApiError> {
        let err = self.describe_error.lock().unwrap().clone();
        if let Some(err) = err {
            return Err(ApiError::Uncertain(err));
        }
        self.connections
            .lock()
            .unwrap()
            .iter()
            .find(|c| c.id == connection_id)
            .cloned()
            .ok_or_else(|| ApiError::Rejected("not found".into()))
    }
    async fn statuses(&self, conn: &Connection) -> Result<Vec<TunnelStatus>, ApiError> {
        let err = self.status_error.lock().unwrap().clone();
        if let Some(err) = err {
            return Err(ApiError::Rejected(err));
        }
        Ok(self
            .statuses
            .lock()
            .unwrap()
            .get(&conn.id)
            .cloned()
            .unwrap_or_default())
    }
}

#[derive(Default)]
struct FakeSlack {
    broadcasts: Mutex<Vec<(String, Vec<Value>)>>,
    replies: Mutex<Vec<Notice>>,
    updates: Mutex<Vec<String>>,
    unreachable: Mutex<bool>,
}

impl FakeSlack {
    fn reply_text(&self) -> String {
        self.replies
            .lock()
            .unwrap()
            .iter()
            .map(Notice::render)
            .collect::<Vec<_>>()
            .join("\n")
    }
    fn broadcast_text(&self) -> String {
        self.broadcasts
            .lock()
            .unwrap()
            .iter()
            .map(|(f, b)| format!("{f}\n{}", crate::slack::blocks::text_of(b)))
            .collect::<Vec<_>>()
            .join("\n---\n")
    }
}

#[async_trait]
impl Notifier for FakeSlack {
    async fn broadcast(
        &self,
        channel_ids: &[String],
        fallback: &str,
        blocks: &[Value],
    ) -> Vec<MessageRef> {
        self.broadcasts
            .lock()
            .unwrap()
            .push((fallback.to_string(), blocks.to_vec()));
        if *self.unreachable.lock().unwrap() {
            return Vec::new();
        }
        channel_ids
            .iter()
            .map(|c| MessageRef {
                channel_id: c.clone(),
                ts: "1.0".into(),
            })
            .collect()
    }
    async fn reply(&self, _refs: &[MessageRef], notice: &Notice) {
        self.replies.lock().unwrap().push(notice.clone());
    }
    async fn update(&self, _refs: &[MessageRef], fallback: &str, _blocks: &[Value]) {
        self.updates.lock().unwrap().push(fallback.to_string());
    }
}

#[derive(Default)]
struct FakeEvents(Mutex<Vec<(String, String, String)>>);

impl FakeEvents {
    fn reasons(&self) -> Vec<String> {
        self.0
            .lock()
            .unwrap()
            .iter()
            .map(|(_, r, _)| r.clone())
            .collect()
    }
}

impl Recorder for FakeEvents {
    fn normal(&self, reason: &'static str, message: String) {
        self.0
            .lock()
            .unwrap()
            .push(("Normal".into(), reason.into(), message));
    }
    fn warning(&self, reason: &'static str, message: String) {
        self.0
            .lock()
            .unwrap()
            .push(("Warning".into(), reason.into(), message));
    }
}

struct RunRecord {
    tunnel_ip: String,
    peer_ip: String,
    resuming: bool,
    acceptance_unknown: bool,
    dry_run: bool,
}

#[derive(Default)]
struct FakeReplacer {
    results: Mutex<VecDeque<ExecResult>>,
    runs: Mutex<Vec<RunRecord>>,
    /// Marks the connection's tunnel as replaced in the shared VPN fake so the
    /// following recheck sees the sibling chainable.
    vpn: Option<Arc<FakeVpn>>,
}

fn result(outcome: Outcome) -> ExecResult {
    ExecResult {
        outcome,
        duration: Duration::from_secs(90),
        detail: "tunnel UP with 4 accepted route(s)".into(),
        peer_dropped: false,
    }
}

#[async_trait]
impl Replacer for FakeReplacer {
    async fn run(
        &self,
        req: Request,
        reporter: &dyn Reporter,
        _shutdown: CancellationToken,
    ) -> ExecResult {
        self.runs.lock().unwrap().push(RunRecord {
            tunnel_ip: req.tunnel_ip.clone(),
            peer_ip: req.peer_ip.clone(),
            resuming: req.resuming,
            acceptance_unknown: req.acceptance_unknown,
            dry_run: req.dry_run,
        });
        if let Some(on_accepted) = &req.on_accepted {
            on_accepted().await;
        }
        reporter.info(format!("replacing {}", req.tunnel_ip)).await;
        let res = self
            .results
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or_else(|| result(Outcome::Succeeded));
        // Maintenance for the replaced tunnel is no longer pending afterwards.
        if let Some(vpn) = &self.vpn
            && res.outcome.healthy()
        {
            let mut all = vpn.statuses.lock().unwrap();
            if let Some(st) = all.get_mut(&req.connection.id) {
                for s in st.iter_mut() {
                    if s.tunnel.outside_ip == req.tunnel_ip {
                        s.maintenance = Maintenance::default();
                    }
                }
            }
        }
        res
    }
}

struct Harness {
    ctrl: Arc<Controller>,
    vpn: Arc<FakeVpn>,
    slack: Arc<FakeSlack>,
    events: Arc<FakeEvents>,
    exec: Arc<FakeReplacer>,
    cm: Arc<MemoryConfigMap>,
    broker: Arc<Broker>,
    metrics: Arc<Metrics>,
    shutdown: CancellationToken,
}

fn test_config(extra: &str) -> Config {
    let yaml = format!(
        "{}\napproval:\n  slackUserIDs: [U1]\n  timeout: \"300ms\"\nsafety:\n  verifyTimeout: \"1s\"\n  verifyPollInterval: \"20ms\"\nmaintenanceWindow:\n  cronSchedule: \"* * * * *\"\n  duration: \"1h\"\nreconcileInterval: \"50ms\"\n{extra}",
        config::MINIMAL_YAML.replace("approval:\n  slackUserIDs: [U0123456789]\n", "")
    );
    let env = Env {
        slack_bot_token: "xoxb".into(),
        slack_app_token: "xapp-".into(),
        pod_name: "pod".into(),
        pod_namespace: "ns".into(),
        pod_uid: "uid".into(),
        ..Env::default()
    };
    config::parse(&yaml, &env).unwrap()
}

fn harness_with(cfg: Config, window: Window, gate: Gate) -> Harness {
    let vpn = Arc::new(FakeVpn::default());
    let slack = Arc::new(FakeSlack::default());
    let events = Arc::new(FakeEvents::default());
    let exec = Arc::new(FakeReplacer {
        vpn: Some(vpn.clone()),
        ..FakeReplacer::default()
    });
    let cm = MemoryConfigMap::shared();
    let broker = Broker::new(&["U1".to_string()]);
    let metrics = Arc::new(Metrics::new());
    let ctrl = Controller::new(Options {
        config: Arc::new(cfg),
        aws: vpn.clone(),
        exec: exec.clone(),
        store: Arc::new(store(cm.clone())),
        slack: slack.clone(),
        broker: broker.clone(),
        window: Arc::new(window),
        traffic: Arc::new(gate),
        metrics: metrics.clone(),
        events: events.clone(),
        dm_channels: vec!["D1".into()],
        revalidate_interval: Some(Duration::from_millis(40)),
    });
    Harness {
        ctrl,
        vpn,
        slack,
        events,
        exec,
        cm,
        broker,
        metrics,
        shutdown: CancellationToken::new(),
    }
}

fn open_window() -> Window {
    Window::new(&WindowConfig {
        timezone: "UTC".into(),
        cron_schedule: "* * * * *".into(),
        duration: Duration::from_secs(3600),
        min_remaining: Duration::from_secs(1),
    })
    .unwrap()
}

fn closed_window() -> Window {
    // Opens for an hour twelve hours from now, so it is shut at test time.
    let hour = (chrono::Timelike::hour(&Utc::now()) + 12) % 24;
    Window::new(&WindowConfig {
        timezone: "UTC".into(),
        cron_schedule: format!("0 {hour} * * *"),
        duration: Duration::from_secs(3600),
        min_remaining: Duration::from_secs(1),
    })
    .unwrap()
}

fn harness() -> Harness {
    harness_with(test_config(""), open_window(), Gate::disabled())
}

/// A connection with maintenance pending on the given tunnels, stable peers.
fn pending_connection(id: &str, first: bool, second: bool) -> (Connection, Vec<Maintenance>) {
    let mut conn = connection(id);
    // Fixtures date the telemetry to a fixed instant; move it to real time so
    // the peer stability check reads against the wall clock.
    for t in &mut conn.tunnels {
        t.last_status_change = Some(Utc::now() - chrono::TimeDelta::hours(1));
    }
    let m = |on: bool| {
        if on {
            Maintenance {
                pending: true,
                auto_applied_after: Some(Utc::now() + chrono::TimeDelta::hours(400)),
                last_applied: None,
            }
        } else {
            Maintenance::default()
        }
    };
    (conn, vec![m(first), m(second)])
}

async fn wait_until(what: &str, mut cond: impl FnMut() -> bool) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while !cond() {
        assert!(
            tokio::time::Instant::now() < deadline,
            "timed out waiting for {what}"
        );
        sleep(Duration::from_millis(10)).await;
    }
}

fn click(h: &Harness, request_id: &str, approved: bool) {
    h.broker.handle(&Interaction {
        request_id: request_id.into(),
        approved,
        user_id: "U1".into(),
        user_name: "younsl".into(),
    });
}

fn request_id(h: &Harness) -> String {
    let snap = h.cm.snapshot();
    snap.approvals
        .keys()
        .next()
        .cloned()
        .expect("an approval is recorded")
}

#[tokio::test]
async fn idle_pass_records_metrics_and_sends_nothing() {
    let h = harness();
    let (conn, m) = pending_connection("vpn-1", false, false);
    h.vpn.set(conn, &m);
    h.ctrl.reconcile_once(h.shutdown.clone()).await;
    let text = h.metrics.render();
    assert!(text.contains("managed_connections 1"), "{text}");
    assert!(text.contains("tunnel_pending_maintenance{vpn_connection_id=\"vpn-1\",vpn_connection_name=\"prod\",tunnel_ip=\"1.1.1.1\"} 0"), "{text}");
    assert!(text.contains("window_open 1"), "{text}");
    assert!(h.slack.broadcasts.lock().unwrap().is_empty());
    assert!(!h.ctrl.is_busy());
    h.ctrl.log_scope().await;
}

#[tokio::test]
async fn discover_and_state_failures_are_counted() {
    let h = harness();
    *h.vpn.discover_error.lock().unwrap() = Some("throttled".into());
    h.ctrl.reconcile_once(h.shutdown.clone()).await;
    assert!(
        h.metrics
            .render()
            .contains("reconcile_errors_total{stage=\"discover\"} 1")
    );
    *h.vpn.discover_error.lock().unwrap() = None;

    let (conn, m) = pending_connection("vpn-1", true, false);
    h.vpn.set(conn, &m);
    *h.vpn.status_error.lock().unwrap() = Some("denied".into());
    h.ctrl.reconcile_once(h.shutdown.clone()).await;
    assert!(
        h.metrics
            .render()
            .contains("reconcile_errors_total{stage=\"maintenance_status\"} 1")
    );
    *h.vpn.status_error.lock().unwrap() = None;

    *h.cm.fail_get.lock().unwrap() = Some("api down".into());
    h.ctrl.reconcile_once(h.shutdown.clone()).await;
    assert!(
        h.metrics
            .render()
            .contains("reconcile_errors_total{stage=\"state_load\"} 1")
    );
    h.ctrl.log_scope().await;
    *h.vpn.status_error.lock().unwrap() = Some("denied".into());
    h.ctrl.log_scope().await;
    *h.vpn.discover_error.lock().unwrap() = Some("throttled".into());
    h.ctrl.log_scope().await;
}

#[tokio::test]
async fn approved_chain_replaces_both_tunnels_under_one_card() {
    let h = harness();
    let (conn, m) = pending_connection("vpn-1", true, true);
    h.vpn.set(conn, &m);

    h.ctrl.reconcile_once(h.shutdown.clone()).await;
    wait_until("approval card", || !h.cm.snapshot().approvals.is_empty()).await;
    let card = h.slack.broadcast_text();
    assert!(
        card.contains(
            "[ACTION] VPN connection prod (vpn-1). VPN tunnel replacement approval (dry run)"
        ),
        "{card}"
    );
    assert!(
        card.contains("2. `2.2.2.2` starts only once `1.1.1.1` is back UP"),
        "{card}"
    );
    assert!(h.ctrl.is_busy());
    let rid = request_id(&h);
    assert!(rid.starts_with("vpn-1|1.1.1.1|"), "{rid}");
    assert!(h.metrics.render().contains("replacement_in_flight 0"));

    // A second pass while the card is up must not post another.
    h.ctrl.reconcile_once(h.shutdown.clone()).await;
    assert_eq!(h.slack.broadcasts.lock().unwrap().len(), 1);
    assert!(
        h.metrics
            .render()
            .contains("blocked_total{reason=\"awaiting_approval\"} 1"),
        "{}",
        h.metrics.render()
    );

    click(&h, &rid, true);
    wait_until("worker to finish", || !h.ctrl.is_busy()).await;

    let runs = h.exec.runs.lock().unwrap();
    assert_eq!(runs.len(), 2);
    assert_eq!(runs[0].tunnel_ip, "1.1.1.1");
    assert_eq!(runs[0].peer_ip, "2.2.2.2");
    assert!(runs[0].dry_run);
    assert_eq!(runs[1].tunnel_ip, "2.2.2.2");
    assert_eq!(runs[1].peer_ip, "1.1.1.1");
    drop(runs);

    let replies = h.slack.reply_text();
    assert!(
        replies.contains("Approved by <@U1>. Re-checking safety conditions"),
        "{replies}"
    );
    assert!(
        replies.contains("This approval covers 2 tunnel(s) of prod (vpn-1)"),
        "{replies}"
    );
    assert!(
        replies.contains("*Step 1 of 2.* Tunnel `1.1.1.1` is next."),
        "{replies}"
    );
    assert!(
        replies.contains("*Step 2 of 2.* Tunnel `2.2.2.2` is next."),
        "{replies}"
    );
    assert!(replies.contains("[SUCCESS] VPN connection prod (vpn-1). *Replaced.* tunnel UP with 4 accepted route(s) in 1m 30s.\nProgress: 4/8 (50%)"), "{replies}");
    assert!(
        replies.contains("*Run complete.* All 2 tunnel(s) of this connection are done."),
        "{replies}"
    );
    assert_eq!(
        h.slack.updates.lock().unwrap().len(),
        2,
        "the card is closed after each step"
    );

    let snap = h.cm.snapshot();
    assert!(snap.in_flight.is_none());
    assert!(snap.approvals.is_empty());
    let rec = &snap.connections["vpn-1"];
    assert_eq!(rec.last_tunnel_ip, "2.2.2.2");
    assert_eq!(rec.last_result, "succeeded");

    let reasons = h.events.reasons();
    for want in [
        "ApprovalRequested",
        "ReplacementApproved",
        "ReplacingTunnel",
        "TunnelReplaced",
    ] {
        assert!(
            reasons.iter().any(|r| r == want),
            "missing {want} in {reasons:?}"
        );
    }
    let text = h.metrics.render();
    assert!(
        text.contains("approval_total{decision=\"approved\"} 1"),
        "{text}"
    );
    assert!(
        text.contains("replacement_total{outcome=\"succeeded\"} 2"),
        "{text}"
    );
    assert!(text.contains("replacement_in_flight 0"), "{text}");
}

#[tokio::test]
async fn unhealthy_first_step_stops_the_chain() {
    let h = harness();
    let (conn, m) = pending_connection("vpn-1", true, true);
    h.vpn.set(conn, &m);
    h.exec.results.lock().unwrap().push_back(ExecResult {
        outcome: Outcome::VerifyTimeout,
        duration: Duration::from_secs(600),
        detail: "never came back".into(),
        peer_dropped: true,
    });
    h.ctrl.reconcile_once(h.shutdown.clone()).await;
    wait_until("approval card", || !h.cm.snapshot().approvals.is_empty()).await;
    click(&h, &request_id(&h), true);
    wait_until("worker to finish", || !h.ctrl.is_busy()).await;

    assert_eq!(h.exec.runs.lock().unwrap().len(), 1);
    let replies = h.slack.reply_text();
    assert!(
        replies.contains(
            "[ERROR] VPN connection prod (vpn-1). *Replaced but not healthy after 10m 00s.*"
        ),
        "{replies}"
    );
    assert!(
        replies.contains(
            "Stopping here: 1 tunnel(s) of this connection still have maintenance pending"
        ),
        "{replies}"
    );
    assert!(
        replies.contains("*Run ended early.* 1 of 2 tunnel(s) replaced"),
        "{replies}"
    );
    let snap = h.cm.snapshot();
    assert!(snap.in_flight.is_none());
    assert_eq!(snap.connections["vpn-1"].last_result, "verify_timeout");
    let reasons = h.events.reasons();
    assert!(
        reasons.contains(&"PeerTunnelLost".to_string()),
        "{reasons:?}"
    );
    assert!(
        reasons.contains(&"TunnelReplaceFailed".to_string()),
        "{reasons:?}"
    );
    assert!(h.metrics.render().contains("peer_dropped_total 1"));
}

#[tokio::test]
async fn denial_and_timeout_close_the_card_without_replacing() {
    let h = harness();
    let (conn, m) = pending_connection("vpn-1", true, false);
    h.vpn.set(conn.clone(), &m);
    h.ctrl.reconcile_once(h.shutdown.clone()).await;
    wait_until("approval card", || !h.cm.snapshot().approvals.is_empty()).await;
    click(&h, &request_id(&h), false);
    wait_until("worker to finish", || !h.ctrl.is_busy()).await;
    assert!(h.exec.runs.lock().unwrap().is_empty());
    let replies = h.slack.reply_text();
    assert!(
        replies.contains(
            "[WARN] VPN connection prod (vpn-1). *Denied* by <@U1>. The tunnel was left alone."
        ),
        "{replies}"
    );
    assert!(h.cm.snapshot().approvals.is_empty());
    assert!(
        h.events
            .reasons()
            .contains(&"ReplacementDenied".to_string())
    );
    assert!(
        h.metrics
            .render()
            .contains("approval_total{decision=\"denied\"} 1")
    );

    // Nobody answers: the 300ms timeout expires.
    h.ctrl.reconcile_once(h.shutdown.clone()).await;
    wait_until("second card", || {
        h.slack.broadcasts.lock().unwrap().len() == 2
    })
    .await;
    wait_until("timeout", || !h.ctrl.is_busy()).await;
    let replies = h.slack.reply_text();
    assert!(
        replies.contains("*Expired.* Nobody responded within 300ms."),
        "{replies}"
    );
    assert!(h.events.reasons().contains(&"ApprovalTimedOut".to_string()));
    assert!(
        h.metrics
            .render()
            .contains("approval_total{decision=\"timeout\"} 1")
    );
}

#[tokio::test]
async fn card_is_withdrawn_when_conditions_can_no_longer_clear() {
    let mut cfg = test_config("");
    cfg.approval.timeout = config::Duration::secs(60);
    let h = harness_with(cfg, open_window(), Gate::disabled());
    let (mut conn, m) = pending_connection("vpn-1", true, false);
    h.vpn.set(conn.clone(), &m);
    h.ctrl.reconcile_once(h.shutdown.clone()).await;
    wait_until("approval card", || !h.cm.snapshot().approvals.is_empty()).await;

    // The connection leaves "available": not waitable, so the card goes.
    conn.state = "deleting".into();
    h.vpn.set(conn, &m);
    wait_until("withdrawal", || !h.ctrl.is_busy()).await;
    let replies = h.slack.reply_text();
    assert!(replies.contains("*Expired.* vpn connection state is \"deleting\", not \"available\". The tunnel was left alone"), "{replies}");
    assert!(h.events.reasons().contains(&"ApprovalExpired".to_string()));
    assert!(
        h.metrics
            .render()
            .contains("approval_total{decision=\"expired\"} 1")
    );
}

#[tokio::test]
async fn waitable_block_keeps_the_card_while_time_remains() {
    let mut cfg = test_config("");
    cfg.approval.timeout = config::Duration::secs(3600);
    let h = harness_with(cfg, open_window(), Gate::disabled());
    let (mut conn, m) = pending_connection("vpn-1", true, false);
    h.vpn.set(conn.clone(), &m);
    h.ctrl.reconcile_once(h.shutdown.clone()).await;
    wait_until("approval card", || !h.cm.snapshot().approvals.is_empty()).await;
    let rid = request_id(&h);

    // Peer drops: waitable, and the hour-long budget covers recovery.
    conn.tunnels[1].up = false;
    h.vpn.set(conn.clone(), &m);
    sleep(Duration::from_millis(150)).await;
    assert!(h.ctrl.is_busy(), "card kept while the peer could come back");
    assert!(h.cm.snapshot().approvals.contains_key(&rid));

    // A read failure says nothing about the tunnel either.
    *h.vpn.describe_error.lock().unwrap() = Some("throttled".into());
    sleep(Duration::from_millis(100)).await;
    assert!(h.ctrl.is_busy());
    *h.vpn.describe_error.lock().unwrap() = None;

    // The peer comes back and the approver clicks: the recheck passes.
    conn.tunnels[1].up = true;
    h.vpn.set(conn, &m);
    click(&h, &rid, true);
    wait_until("replacement", || !h.ctrl.is_busy()).await;
    assert_eq!(h.exec.runs.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn recheck_failure_after_approval_aborts() {
    let h = harness();
    let (mut conn, m) = pending_connection("vpn-1", true, false);
    h.vpn.set(conn.clone(), &m);
    h.ctrl.reconcile_once(h.shutdown.clone()).await;
    wait_until("approval card", || !h.cm.snapshot().approvals.is_empty()).await;
    let rid = request_id(&h);
    // The peer loses its routes between the card and the click.
    conn.tunnels[1].accepted_routes = 0;
    h.vpn.set(conn, &m);
    click(&h, &rid, true);
    wait_until("abort", || !h.ctrl.is_busy()).await;
    assert!(h.exec.runs.lock().unwrap().is_empty());
    let replies = h.slack.reply_text();
    assert!(
        replies.contains("*Not replacing.* Conditions changed between approval and execution."),
        "{replies}"
    );
    assert!(
        replies.contains("accepts 0 BGP route(s), 1 required"),
        "{replies}"
    );
    assert!(
        h.metrics
            .render()
            .contains("approval_total{decision=\"aborted\"} 1")
    );
    assert!(
        h.events
            .reasons()
            .contains(&"MaintenanceHeldBack".to_string())
    );
}

#[tokio::test]
async fn persist_failure_refuses_to_replace() {
    let h = harness();
    let (conn, m) = pending_connection("vpn-1", true, false);
    h.vpn.set(conn, &m);
    h.ctrl.reconcile_once(h.shutdown.clone()).await;
    wait_until("approval card", || !h.cm.snapshot().approvals.is_empty()).await;
    let rid = request_id(&h);
    *h.cm.fail_write.lock().unwrap() = Some("forbidden".into());
    click(&h, &rid, true);
    wait_until("refusal", || !h.ctrl.is_busy()).await;
    assert!(h.exec.runs.lock().unwrap().is_empty());
    let replies = h.slack.reply_text();
    assert!(
        replies.contains("*Not replacing tunnel `1.1.1.1`.* Could not record the in-flight state"),
        "{replies}"
    );
    assert!(
        replies.contains("*Closed without replacing anything.*"),
        "{replies}"
    );
    assert!(
        h.metrics
            .render()
            .contains("reconcile_errors_total{stage=\"persist_in_flight\"} 1")
    );
}

#[tokio::test]
async fn unreachable_approvers_skip_the_candidate() {
    let h = harness();
    *h.slack.unreachable.lock().unwrap() = true;
    let (conn, m) = pending_connection("vpn-1", true, false);
    h.vpn.set(conn, &m);
    h.ctrl.reconcile_once(h.shutdown.clone()).await;
    wait_until("skip", || !h.ctrl.is_busy()).await;
    assert!(h.cm.snapshot().approvals.is_empty());
    assert!(
        h.metrics
            .render()
            .contains("reconcile_errors_total{stage=\"approval_delivery\"} 1")
    );
}

#[tokio::test]
async fn detection_notice_is_sent_once_and_pruned() {
    let h = harness_with(test_config(""), closed_window(), Gate::disabled());
    let (mut conn, m) = pending_connection("vpn-1", true, true);
    conn.tunnels[1].lifecycle_control = false;
    h.vpn.set(conn.clone(), &m);

    h.ctrl.reconcile_once(h.shutdown.clone()).await;
    assert_eq!(h.slack.broadcasts.lock().unwrap().len(), 1);
    let text = h.slack.broadcast_text();
    assert!(text.starts_with("[WARN] VPN connection prod (vpn-1). Pending VPN tunnel maintenance detected 2 tunnels: 1.1.1.1, 2.2.2.2."), "{text}");
    assert!(text.contains("The window next opens at *"), "{text}");
    assert!(text.contains("outside window"), "{text}");
    assert!(text.contains("*What to do*"), "{text}");
    assert_eq!(h.cm.snapshot().notices.len(), 1);
    assert!(
        h.events
            .reasons()
            .contains(&"MaintenanceDetected".to_string())
    );
    assert!(
        h.metrics
            .render()
            .contains("detection_notices_total{reason=\"window_closed\"} 1"),
        "{}",
        h.metrics.render()
    );
    assert!(
        h.metrics
            .render()
            .contains("blocked_total{reason=\"lifecycle_control_disabled\"} 1")
    );

    // Same cycle: no second notice.
    h.ctrl.reconcile_once(h.shutdown.clone()).await;
    assert_eq!(h.slack.broadcasts.lock().unwrap().len(), 1);

    // Maintenance applied by AWS and lifecycle control fixed: the notice is
    // pruned.
    conn.tunnels[1].lifecycle_control = true;
    h.vpn
        .set(conn, &[Maintenance::default(), Maintenance::default()]);
    h.ctrl.reconcile_once(h.shutdown.clone()).await;
    assert!(h.cm.snapshot().notices.is_empty());
}

#[tokio::test]
async fn notice_delivery_failure_is_retried_next_pass() {
    let h = harness_with(test_config(""), closed_window(), Gate::disabled());
    *h.slack.unreachable.lock().unwrap() = true;
    let (conn, m) = pending_connection("vpn-1", true, false);
    h.vpn.set(conn, &m);
    h.ctrl.reconcile_once(h.shutdown.clone()).await;
    assert!(h.cm.snapshot().notices.is_empty());
    assert!(
        h.metrics
            .render()
            .contains("reconcile_errors_total{stage=\"notice_delivery\"} 1")
    );
    *h.slack.unreachable.lock().unwrap() = false;
    h.ctrl.reconcile_once(h.shutdown.clone()).await;
    assert_eq!(h.cm.snapshot().notices.len(), 1);
}

#[tokio::test]
async fn candidate_without_proposal_is_noticed_as_traffic_held() {
    // An in-flight record blocks proposing, yet the pending tunnel is worth a
    // notice: the block reason is not notifiable, so nothing goes out here.
    let h = harness();
    let (conn, m) = pending_connection("vpn-1", true, false);
    h.vpn.set(conn, &m);
    h.cm.seed(&Snapshot {
        in_flight: Some(InFlight {
            request_id: "other".into(),
            connection_id: "vpn-9".into(),
            phase: Phase::Verifying,
            ..InFlight::default()
        }),
        ..Snapshot::default()
    });
    h.ctrl.reconcile_once(h.shutdown.clone()).await;
    assert!(h.slack.broadcasts.lock().unwrap().is_empty());
    assert!(h.metrics.render().contains("replacement_in_flight 1"));
    assert!(
        h.metrics
            .render()
            .contains("blocked_total{reason=\"replacement_in_flight\"} 1")
    );
}

#[tokio::test]
async fn resume_verifying_record_finishes_the_run() {
    let h = harness();
    let (conn, m) = pending_connection("vpn-1", true, true);
    h.vpn.set(conn, &m);
    h.cm.seed(&Snapshot {
        in_flight: Some(InFlight {
            request_id: "vpn-1|1.1.1.1|1".into(),
            connection_id: "vpn-1".into(),
            tunnel_ip: "1.1.1.1".into(),
            peer_ip: "2.2.2.2".into(),
            phase: Phase::Requested,
            started_at: Some(Utc::now() - chrono::TimeDelta::minutes(3)),
            run_started_at: None,
            approved_by: "U1".into(),
            thread: vec![MessageRef {
                channel_id: "D1".into(),
                ts: "1.0".into(),
            }],
            queue: vec!["2.2.2.2".into()],
            done: 0,
        }),
        ..Snapshot::default()
    });
    h.ctrl.resume_in_flight(h.shutdown.clone()).await;
    wait_until("resumed run", || !h.ctrl.is_busy()).await;
    let runs = h.exec.runs.lock().unwrap();
    assert_eq!(runs.len(), 2);
    assert!(runs[0].resuming);
    assert!(
        runs[0].acceptance_unknown,
        "requested phase never saw acceptance"
    );
    assert!(!runs[1].resuming);
    drop(runs);
    let replies = h.slack.reply_text();
    assert!(
        replies.contains("Continuing the approved run. 1 of 2 tunnel(s) are done"),
        "{replies}"
    );
    assert!(
        replies.contains("*Run complete.* All 2 tunnel(s)"),
        "{replies}"
    );
    let snap = h.cm.snapshot();
    assert!(snap.in_flight.is_none());
    assert_eq!(snap.connections["vpn-1"].last_tunnel_ip, "2.2.2.2");
}

#[tokio::test]
async fn resume_waiting_record_continues_the_chain() {
    let h = harness();
    let (conn, m) = pending_connection("vpn-1", false, true);
    h.vpn.set(conn, &m);
    let replaced_at = Utc::now() - chrono::TimeDelta::minutes(10);
    h.cm.seed(&Snapshot {
        in_flight: Some(InFlight {
            request_id: "vpn-1|1.1.1.1|1".into(),
            connection_id: "vpn-1".into(),
            tunnel_ip: "2.2.2.2".into(),
            peer_ip: "1.1.1.1".into(),
            phase: Phase::Waiting,
            started_at: Some(replaced_at),
            run_started_at: Some(replaced_at - chrono::TimeDelta::minutes(5)),
            approved_by: "U1".into(),
            thread: vec![MessageRef {
                channel_id: "D1".into(),
                ts: "1.0".into(),
            }],
            queue: Vec::new(),
            done: 1,
        }),
        connections: std::iter::once((
            "vpn-1".to_string(),
            ConnectionRecord {
                last_replacement_at: Some(replaced_at),
                last_tunnel_ip: "1.1.1.1".into(),
                last_result: "succeeded".into(),
            },
        ))
        .collect(),
        ..Snapshot::default()
    });
    h.ctrl.resume_in_flight(h.shutdown.clone()).await;
    wait_until("resumed chain", || !h.ctrl.is_busy()).await;
    let runs = h.exec.runs.lock().unwrap();
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0].tunnel_ip, "2.2.2.2");
    assert!(!runs[0].resuming);
    drop(runs);
    let replies = h.slack.reply_text();
    assert!(
        replies.contains(
            "Picking the approved run back up after a restart. 1 of 2 tunnel(s) are done"
        ),
        "{replies}"
    );
    assert!(
        replies.contains("*Step 2 of 2.* Tunnel `2.2.2.2` is next."),
        "{replies}"
    );
    assert!(replies.contains("*Run complete.*"), "{replies}");
}

#[tokio::test]
async fn resume_gives_up_when_the_next_tunnel_cannot_become_ready() {
    let h = harness_with(test_config(""), closed_window(), Gate::disabled());
    let (conn, m) = pending_connection("vpn-1", false, true);
    h.vpn.set(conn, &m);
    h.cm.seed(&Snapshot {
        in_flight: Some(InFlight {
            request_id: "vpn-1|1.1.1.1|1".into(),
            connection_id: "vpn-1".into(),
            tunnel_ip: "2.2.2.2".into(),
            peer_ip: "1.1.1.1".into(),
            phase: Phase::Waiting,
            started_at: Some(Utc::now()),
            approved_by: "U1".into(),
            done: 1,
            ..InFlight::default()
        }),
        ..Snapshot::default()
    });
    h.ctrl.resume_in_flight(h.shutdown.clone()).await;
    wait_until("chain stop", || !h.ctrl.is_busy()).await;
    assert!(h.exec.runs.lock().unwrap().is_empty());
    let replies = h.slack.reply_text();
    assert!(
        replies.contains("Stopping before tunnel `2.2.2.2`:\n> outside window"),
        "{replies}"
    );
    assert!(
        h.cm.snapshot().in_flight.is_none(),
        "the waiting record is dropped"
    );
}

#[tokio::test]
async fn resume_with_unreadable_connection_leaves_the_record() {
    let h = harness();
    h.cm.seed(&Snapshot {
        in_flight: Some(InFlight {
            connection_id: "vpn-missing".into(),
            phase: Phase::Verifying,
            ..InFlight::default()
        }),
        ..Snapshot::default()
    });
    h.ctrl.resume_in_flight(h.shutdown.clone()).await;
    assert!(!h.ctrl.is_busy());
    assert!(
        h.metrics
            .render()
            .contains("reconcile_errors_total{stage=\"resume_describe\"} 1")
    );
    assert!(h.cm.snapshot().in_flight.is_some());
    *h.cm.fail_get.lock().unwrap() = Some("down".into());
    h.ctrl.resume_in_flight(h.shutdown.clone()).await;
}

#[tokio::test]
async fn adopts_a_recorded_approval_and_drops_stale_ones() {
    let mut cfg = test_config("");
    cfg.approval.timeout = config::Duration::secs(3600);
    let h = harness_with(cfg, open_window(), Gate::disabled());
    let (conn, m) = pending_connection("vpn-1", true, false);
    h.vpn.set(conn, &m);
    let rid = format!(
        "vpn-1|1.1.1.1|{}",
        m[0].auto_applied_after.unwrap().timestamp()
    );
    h.cm.seed(&Snapshot {
        approvals: [
            (
                rid.clone(),
                Approval {
                    request_id: rid.clone(),
                    posted_at: Some(Utc::now() - chrono::TimeDelta::minutes(5)),
                    thread: vec![MessageRef {
                        channel_id: "D1".into(),
                        ts: "1.0".into(),
                    }],
                },
            ),
            (
                "expired".into(),
                Approval {
                    request_id: "expired".into(),
                    posted_at: Some(Utc::now() - chrono::TimeDelta::days(1)),
                    thread: Vec::new(),
                },
            ),
        ]
        .into_iter()
        .collect(),
        ..Snapshot::default()
    });
    h.ctrl.resume_in_flight(h.shutdown.clone()).await;
    wait_until("adoption", || h.ctrl.is_busy()).await;
    wait_until("stale removal", || {
        !h.cm.snapshot().approvals.contains_key("expired")
    })
    .await;
    assert!(
        h.slack.broadcasts.lock().unwrap().is_empty(),
        "the existing card is reused"
    );
    click(&h, &rid, true);
    wait_until("replacement", || !h.ctrl.is_busy()).await;
    assert_eq!(h.exec.runs.lock().unwrap().len(), 1);
    assert!(h.cm.snapshot().approvals.is_empty());
}

#[tokio::test]
async fn shutdown_while_waiting_keeps_the_approval_for_the_next_leader() {
    let mut cfg = test_config("");
    cfg.approval.timeout = config::Duration::secs(3600);
    let h = harness_with(cfg, open_window(), Gate::disabled());
    let (conn, m) = pending_connection("vpn-1", true, false);
    h.vpn.set(conn, &m);
    h.ctrl.reconcile_once(h.shutdown.clone()).await;
    wait_until("approval card", || !h.cm.snapshot().approvals.is_empty()).await;
    h.shutdown.cancel();
    wait_until("worker stop", || !h.ctrl.is_busy()).await;
    assert_eq!(h.cm.snapshot().approvals.len(), 1);
    assert!(
        h.metrics
            .render()
            .contains("approval_total{decision=\"aborted\"} 1")
    );
}

#[tokio::test]
async fn run_loop_reconciles_until_cancelled() {
    let h = harness();
    let (conn, m) = pending_connection("vpn-1", false, false);
    h.vpn.set(conn, &m);
    let ctrl = h.ctrl.clone();
    let token = h.shutdown.clone();
    let task = tokio::spawn(async move { ctrl.run(token).await });
    sleep(Duration::from_millis(180)).await;
    h.shutdown.cancel();
    task.await.unwrap();
    assert!(h.metrics.render().contains("reconcile_total"));
}

#[tokio::test]
async fn cooldown_after_a_replacement_blocks_a_repeat_but_chains_the_sibling() {
    let h = harness();
    let (conn, m) = pending_connection("vpn-1", true, true);
    h.vpn.set(conn, &m);
    h.cm.seed(&Snapshot {
        connections: std::iter::once((
            "vpn-1".to_string(),
            ConnectionRecord {
                last_replacement_at: Some(Utc::now() - chrono::TimeDelta::minutes(30)),
                last_tunnel_ip: "1.1.1.1".into(),
                last_result: "succeeded".into(),
            },
        ))
        .collect(),
        ..Snapshot::default()
    });
    h.ctrl.reconcile_once(h.shutdown.clone()).await;
    wait_until("approval card", || !h.cm.snapshot().approvals.is_empty()).await;
    let rid = request_id(&h);
    assert!(
        rid.starts_with("vpn-1|2.2.2.2|"),
        "the sibling is proposed: {rid}"
    );
    assert!(
        h.metrics
            .render()
            .contains("blocked_total{reason=\"cooldown\"} 1")
    );
    h.shutdown.cancel();
    wait_until("stop", || !h.ctrl.is_busy()).await;
}

#[test]
fn history_and_summaries() {
    let snap = Snapshot {
        connections: [
            (
                "a".to_string(),
                ConnectionRecord {
                    last_replacement_at: Some(fixed_now()),
                    last_tunnel_ip: "1.1.1.1".into(),
                    last_result: "succeeded".into(),
                },
            ),
            (
                "b".to_string(),
                ConnectionRecord {
                    last_result: "mystery".into(),
                    ..ConnectionRecord::default()
                },
            ),
        ]
        .into_iter()
        .collect(),
        ..Snapshot::default()
    };
    let history = history_from(&snap);
    assert!(history["a"].last_succeeded);
    assert!(!history["b"].last_succeeded);
    let cfg = test_config("");
    assert_eq!(tag_filter_summary(&cfg), "managed=true");
    let mut any = cfg;
    any.targets.tag_filters[0].value.clear();
    assert_eq!(tag_filter_summary(&any), "managed=<any>");
    assert_eq!(up_down(true), "UP");
    let mut c = connection("x");
    c.static_routes_only = true;
    assert_eq!(routing_mode(&c), "static");
    assert_eq!(proposal::gateway_of(&c), "tgw-1");
    c.transit_gateway_id.clear();
    c.vpn_gateway_id = "vgw-1".into();
    c.vpn_gateway_name = "office".into();
    assert_eq!(proposal::gateway_of(&c), "vgw-1");
    assert_eq!(proposal::gateway_name_of(&c), "office");
    let _ = tunnel("1.1.1.1", true, 1, 1);
    let _ = pending(1);
    let _: Arc<Store> = Arc::new(store(MemoryConfigMap::shared()));
}
