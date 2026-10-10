//! Wires the collaborators together and runs the controller until signalled.

use std::sync::Arc;

use anyhow::{Context as _, Result, bail};
use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn};

use crate::approval::Broker;
use crate::aws::Client as AwsClient;
use crate::aws::preflight::Access;
use crate::commands::{discover_input, window_from};
use crate::config::Config;
use crate::controller::{Controller, Options};
use crate::executor::{Executor, Options as ExecOptions};
use crate::k8s::events::{KubeEvents, Recorder};
use crate::k8s::leader::{self, KubeLeases, LeaderConfig, Timing};
use crate::k8s::state::KubeConfigMaps;
use crate::k8s::{Emitter, Store};
use crate::observability::server::{metrics_router, serve};
use crate::observability::{Health, Metrics};
use crate::promx::gate::{GateConfig, OnError};
use crate::promx::{Client as PromClient, ClientConfig, Gate, Vars};
use crate::slack::client::DEFAULT_API_URL;
use crate::slack::socket::{Handler, SocketClient};
use crate::slack::{Approver, Client as SlackClient, Interaction};

/// Compiles the traffic gate. A disabled gate needs no client, so nothing is
/// dialed and no endpoint is required.
pub fn build_traffic_gate(cfg: &Config) -> Result<Gate> {
    let t = &cfg.traffic_gate;
    let on_error = OnError::parse(&t.on_error)?;
    let client = if t.enabled {
        let client = PromClient::new(ClientConfig {
            endpoint: t.endpoint.clone(),
            headers: t.headers.clone(),
            timeout: t.timeout.get(),
        })?;
        info!(endpoint = %t.endpoint, quiet_percentile = t.quiet_percentile, on_error = %on_error, "traffic gate enabled");
        Some(client)
    } else {
        info!(
            "traffic gate disabled; replacements are gated only by the window and the peer checks"
        );
        None
    };
    Ok(Gate::new(
        client,
        GateConfig {
            enabled: t.enabled,
            percentile: t.quiet_percentile,
            on_error,
        },
    )?)
}

/// Refuses to start when the controller could not do its job.
///
/// The failure this prevents is the quiet one. A missing IAM permission or an
/// unreachable metric endpoint does not crash a running controller: it turns
/// every pass into a logged error or a blocked candidate, and the tunnel is
/// still handed to AWS at its own auto-apply time while the Pod reports Ready.
/// Failing at startup makes that a `CrashLoopBackOff`, which someone notices.
pub async fn verify_dependencies(cfg: &Config, vpn: &AwsClient, gate: &Gate) -> Result<Access> {
    let access = match vpn.verify_access(&discover_input(cfg)).await {
        Ok(a) => a,
        Err(err) => {
            error!(
                error = %err,
                hint = "check the IRSA or EKS Pod Identity association on this ServiceAccount and the role's EC2 permissions",
                "AWS access check failed; refusing to start"
            );
            return Err(err.into());
        }
    };
    info!(
        identity = %access.identity,
        account = %access.account,
        region = %cfg.region,
        managed_connections = access.connections.len(),
        "AWS access verified"
    );
    if access.connections.is_empty() {
        // Not fatal: an account with nothing enrolled yet is a legitimate
        // state. It is a warning because the far more likely cause is a tag
        // filter that matches nothing.
        warn!(
            tag_filters = cfg.targets.tag_filters.len(),
            "no VPN connection matches the configured tag filters, so there is nothing to manage yet"
        );
    }

    if !gate.enabled() {
        return Ok(access);
    }
    if let Err(err) = gate.verify(&gate_probe(&access)).await {
        if gate.fail_closed() {
            error!(
                error = %err,
                hint = "fix the endpoint, its headers, or the exporter, or set trafficGate.onError to allow",
                "traffic gate check failed; refusing to start"
            );
            return Err(err.into());
        }
        // onError is allow, which is an explicit decision that an unavailable
        // metric source must not stop maintenance.
        warn!(
            error = %err,
            "traffic gate could not be verified, and onError is allow, so replacements will proceed without a traffic verdict until it recovers"
        );
    }
    Ok(access)
}

/// Picks the connection the gate is verified against. Any managed connection
/// proves the exporter publishes the metric; the first one keeps startup to one
/// query.
#[must_use]
pub fn gate_probe(access: &Access) -> Vars {
    let mut v = Vars::default();
    if let Some(conn) = access.connections.first() {
        v.vpn_connection_id.clone_from(&conn.id);
    }
    v
}

/// Renders the approver list as `name (ID)` entries. Both halves are present
/// on purpose: the name is what a reviewer recognizes, the ID is what actually
/// authorizes a click.
#[must_use]
pub fn format_approvers(approvers: &[Approver]) -> String {
    approvers
        .iter()
        .map(|a| {
            if a.name == a.id {
                a.id.clone()
            } else {
                format!("{} ({})", a.name, a.id)
            }
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// Routes Socket Mode events to the broker and the readiness probe.
struct SocketHandler {
    broker: Arc<Broker>,
    health: Arc<Health>,
}

impl Handler for SocketHandler {
    fn handle(&self, interaction: Interaction) {
        self.broker.handle(&interaction);
    }
    fn connected(&self, connected: bool) {
        self.health.set_slack_connected(connected);
    }
}

/// Loads the collaborators and runs the controller until SIGINT or SIGTERM.
#[allow(clippy::too_many_lines)]
pub async fn run(cfg: Config) -> Result<()> {
    let cfg = Arc::new(cfg);
    let window = Arc::new(window_from(&cfg)?);

    info!(
        version = env!("CARGO_PKG_VERSION"),
        commit = env!("BUILD_COMMIT"),
        rust_version = env!("BUILD_RUSTC_VERSION"),
        region = %cfg.region,
        reconcile_interval = %cfg.reconcile_interval,
        dry_run = cfg.dry_run,
        maintenance_window = %window,
        approvers = cfg.approval.slack_user_ids.len(),
        "starting aws-vpn-maintenance-handler"
    );
    if cfg.dry_run {
        warn!(
            "dry run is enabled: approvals will validate ReplaceVpnTunnel through the AWS DryRun flag and no tunnel will actually be replaced"
        );
    }

    let shutdown = CancellationToken::new();
    spawn_signal_handler(shutdown.clone());

    let vpn = Arc::new(AwsClient::new(&cfg.region).await);
    let kube = kube::Client::try_default()
        .await
        .context("kubernetes client")?;
    let gate = Arc::new(build_traffic_gate(&cfg).context("invalid traffic gate configuration")?);
    verify_dependencies(&cfg, &vpn, &gate).await?;

    let metrics = Arc::new(Metrics::new());
    let health = Arc::new(Health::default());
    {
        let (router, port, token) = (health.router(), cfg.health_port, shutdown.clone());
        tokio::spawn(async move {
            if let Err(err) = serve(router, port, token).await {
                error!(error = %err, "health server failed");
            }
        });
        let (router, port, token) = (
            metrics_router(metrics.clone()),
            cfg.metrics_port,
            shutdown.clone(),
        );
        tokio::spawn(async move {
            if let Err(err) = serve(router, port, token).await {
                error!(error = %err, "metrics server failed");
            }
        });
    }

    let slack = Arc::new(SlackClient::new(DEFAULT_API_URL, &cfg.slack_bot_token));
    // Checked at startup so a revoked token fails visibly rather than when
    // maintenance is first queued.
    let bot_user = slack
        .auth_test()
        .await
        .context("Slack bot token rejected")?;
    let dm_channels = slack
        .open_dms(&cfg.approval.slack_user_ids)
        .await
        .context("could not open a DM channel with any approver")?;
    // Names, not just IDs: a change to the approver list is reviewed as a list
    // of opaque IDs, and this log line is where that gets checked against
    // people.
    let approvers = format_approvers(&slack.resolve_approvers(&cfg.approval.slack_user_ids).await);
    info!(bot_user = %bot_user, dm_channels = dm_channels.len(), approvers = %approvers, "Slack ready");

    let broker = Broker::new(&cfg.approval.slack_user_ids);
    {
        let socket = SocketClient::new(DEFAULT_API_URL, &cfg.slack_app_token);
        let handler: Arc<dyn Handler> = Arc::new(SocketHandler {
            broker: broker.clone(),
            health: health.clone(),
        });
        let token = shutdown.clone();
        tokio::spawn(async move { socket.run(handler, token).await });
    }

    let events: Arc<dyn Recorder> = Arc::new(
        Emitter::new(
            Arc::new(KubeEvents::new(kube.clone(), &cfg.pod_namespace)),
            &cfg.pod_name,
            &cfg.pod_namespace,
            &cfg.pod_uid,
        )
        .context("failed to initialize the Kubernetes event emitter")?,
    );

    let store = Arc::new(Store::new(
        Box::new(KubeConfigMaps::new(
            kube.clone(),
            &cfg.pod_namespace,
            &cfg.state_config_map_name,
        )),
        &cfg.pod_namespace,
        &cfg.state_config_map_name,
    ));
    let exec = Arc::new(Executor::new(
        vpn.clone(),
        ExecOptions {
            verify_timeout: cfg.safety.verify_timeout.get(),
            poll_interval: cfg.safety.verify_poll_interval.get(),
            min_accepted_routes: cfg.safety.peer_min_accepted_routes,
            heartbeat: cfg.approval.progress_heartbeat.get(),
        },
    ));

    let ctrl = Controller::new(Options {
        config: cfg.clone(),
        aws: vpn,
        exec,
        store,
        slack,
        broker,
        window,
        traffic: gate,
        metrics,
        events,
        dm_channels,
        revalidate_interval: None,
    });

    // Printed before leader election so every replica states what it would act
    // on.
    ctrl.log_scope().await;
    health.set_ready(true);

    // A safety requirement, not an availability feature: two active replicas
    // could each replace a different tunnel of the same connection.
    if cfg.leader_elect {
        let leases = KubeLeases::new(kube, &cfg.pod_namespace, &cfg.lease_name);
        let leader_cfg = LeaderConfig {
            identity: cfg.pod_name.clone(),
            lease_name: cfg.lease_name.clone(),
            timing: Timing::default(),
        };
        if cfg.pod_namespace.is_empty() {
            bail!("leader election requires identity, namespace, and lease name");
        }
        let ctrl = ctrl.clone();
        leader::run(&leases, &leader_cfg, shutdown.clone(), move |term| {
            let ctrl = ctrl.clone();
            async move { ctrl.run(term).await }
        })
        .await
        .context("leader election failed")?;
    } else {
        warn!(
            "leader election is disabled; run exactly one replica or two replicas could replace both tunnels of one connection at once"
        );
        ctrl.run(shutdown.clone()).await;
    }

    health.set_ready(false);
    info!("shutdown complete");
    Ok(())
}

/// Cancels `shutdown` on SIGINT or SIGTERM.
fn spawn_signal_handler(shutdown: CancellationToken) {
    tokio::spawn(async move {
        let ctrl_c = tokio::signal::ctrl_c();
        #[cfg(unix)]
        {
            let mut term =
                match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
                    Ok(s) => s,
                    Err(err) => {
                        error!(error = %err, "failed to install the SIGTERM handler");
                        let _ = ctrl_c.await;
                        shutdown.cancel();
                        return;
                    }
                };
            tokio::select! {
                _ = ctrl_c => info!("received SIGINT"),
                _ = term.recv() => info!("received SIGTERM"),
            }
        }
        #[cfg(not(unix))]
        {
            let _ = ctrl_c.await;
        }
        shutdown.cancel();
    });
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::aws::client::fake::*;
    use crate::config::{MINIMAL_YAML, parse, test_env};

    fn cfg(extra: &str) -> Config {
        parse(&format!("{MINIMAL_YAML}\n{extra}"), &test_env()).unwrap()
    }

    #[test]
    fn traffic_gate_builds_from_config() {
        let gate = build_traffic_gate(&cfg("")).unwrap();
        assert!(!gate.enabled());
        let gate = build_traffic_gate(&cfg(
            "trafficGate:\n  enabled: true\n  endpoint: http://mimir.example.com\n  onError: allow\n",
        ))
        .unwrap();
        assert!(gate.enabled());
        assert!(!gate.fail_closed());
        let mut broken = cfg("");
        broken.traffic_gate.on_error = "maybe".into();
        assert!(build_traffic_gate(&broken).is_err());
        let mut broken = cfg("");
        broken.traffic_gate.enabled = true;
        broken.traffic_gate.endpoint = "::not a url".into();
        assert!(build_traffic_gate(&broken).is_err());
    }

    #[test]
    fn gate_probe_and_approver_formatting() {
        let mut access = Access {
            identity: "arn".into(),
            account: "1".into(),
            connections: Vec::new(),
        };
        let v = gate_probe(&access);
        assert!(v.vpn_connection_id.is_empty());
        access
            .connections
            .push(crate::planner::fixtures::connection("vpn-1"));
        let v = gate_probe(&access);
        assert_eq!(v.vpn_connection_id, "vpn-1");

        let approvers = vec![
            Approver {
                id: "U1".into(),
                name: "younsl".into(),
            },
            Approver {
                id: "U2".into(),
                name: "U2".into(),
            },
        ];
        assert_eq!(format_approvers(&approvers), "younsl (U1), U2");
    }

    #[tokio::test]
    async fn verify_dependencies_checks_aws_and_gate() {
        let cfg = cfg("");
        let ec2 = Arc::new(FakeEc2::default());
        *ec2.connections.lock().unwrap() = vec![vpn_connection("vpn-1", "prod", [true, true])];
        let sts = FakeSts(Ok((
            "arn:aws:sts::123456789012:assumed-role/x".into(),
            "123456789012".into(),
        )));
        let vpn = AwsClient::from_parts(Box::new(ec2.clone()), Some(Box::new(sts)), None);
        let access = verify_dependencies(&cfg, &vpn, &Gate::disabled())
            .await
            .unwrap();
        assert_eq!(access.connections.len(), 1);

        // Fail-closed gate with a dead endpoint refuses to start.
        let gate = build_traffic_gate(&self::cfg(
            "trafficGate:\n  enabled: true\n  endpoint: http://127.0.0.1:9\n",
        ))
        .unwrap();
        assert!(verify_dependencies(&cfg, &vpn, &gate).await.is_err());

        // onError allow starts anyway.
        let gate = build_traffic_gate(&self::cfg(
            "trafficGate:\n  enabled: true\n  endpoint: http://127.0.0.1:9\n  onError: allow\n",
        ))
        .unwrap();
        assert!(verify_dependencies(&cfg, &vpn, &gate).await.is_ok());

        // Empty account is a warning, not a failure.
        *ec2.connections.lock().unwrap() = Vec::new();
        assert!(
            verify_dependencies(&cfg, &vpn, &Gate::disabled())
                .await
                .unwrap()
                .connections
                .is_empty()
        );

        // No credentials fails.
        let vpn = AwsClient::from_parts(
            Box::new(ec2),
            Some(Box::new(FakeSts(Err("none".into())))),
            None,
        );
        assert!(
            verify_dependencies(&cfg, &vpn, &Gate::disabled())
                .await
                .is_err()
        );
    }

    #[test]
    fn socket_handler_routes_to_broker_and_health() {
        let broker = Broker::new(&["U1".to_string()]);
        let health = Arc::new(Health::default());
        let handler = SocketHandler {
            broker: broker.clone(),
            health: health.clone(),
        };
        handler.connected(true);
        health.set_ready(true);
        assert_eq!(health.readiness().0, axum::http::StatusCode::OK);
        let mut watch = broker.watch("r");
        handler.handle(Interaction {
            request_id: "r".into(),
            approved: true,
            user_id: "U1".into(),
            user_name: "n".into(),
        });
        assert!(watch.try_recv().unwrap().approved);
    }
}
