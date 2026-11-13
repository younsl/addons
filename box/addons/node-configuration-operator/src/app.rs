//! Wires the clients, the HTTP server, and the controller or agent loop
//! together.

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context as _, Result};
use clap::Parser as _;
use futures::StreamExt as _;
use k8s_openapi::api::core::v1::Node;
use kube::runtime::events::Recorder;
use kube::runtime::reflector::ObjectRef;
use kube::runtime::watcher::Config;
use kube::runtime::{Controller, controller};
use kube::{Api, Client};
use prometheus_client::registry::Registry;
use tokio::signal;
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};
use tracing_subscriber::{EnvFilter, fmt, prelude::*};

use crate::agent::hostfs::HostRoot;
use crate::agent::script::Scripts;
use crate::cli::{AgentArgs, Cli, Command, ControllerArgs};
use crate::controller::leader::{Elector, Settings};
use crate::crd::NodeConfiguration;
use crate::crd::binding::{NODE_LABEL, NodeConfigurationBinding, node_key};
use crate::telemetry::metrics::Metrics;
use crate::telemetry::server::{self, Health};
use crate::{BUILD_DATE, COMMIT, RUSTC_VERSION, VERSION, agent, controller as operator};

/// Runs the controller or the agent until a shutdown signal arrives.
pub async fn start() {
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();

    let cli = Cli::parse();
    if let Command::HostExec(args) = &cli.command {
        let error = agent::script::host_exec(&args.root, &args.script);
        eprintln!("cannot run the script in {}, {error}", args.root);
        std::process::exit(126);
    }
    init_tracing(&cli.log_level, &cli.log_format);
    info!(
        version = VERSION,
        commit = COMMIT,
        build_date = BUILD_DATE,
        rustc_version = RUSTC_VERSION,
        "Starting node-configuration-operator"
    );

    let shutdown = CancellationToken::new();
    let signal_token = shutdown.clone();
    tokio::spawn(async move {
        wait_for_signal().await;
        info!("Shutdown signal received, stopping");
        signal_token.cancel();
    });

    if let Err(e) = run(cli, shutdown).await {
        error!("node-configuration-operator failed, {e:#}");
        std::process::exit(1);
    }
    info!("node-configuration-operator stopped");
}

fn init_tracing(level: &str, format: &str) {
    let filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(log_level(level)));
    let registry = tracing_subscriber::registry().with(filter);
    if format.eq_ignore_ascii_case("text") {
        registry.with(fmt::layer().with_target(true)).init();
    } else {
        registry.with(fmt::layer().json().with_target(true)).init();
    }
}

fn log_level(level: &str) -> &'static str {
    match level.to_ascii_lowercase().as_str() {
        "trace" => "trace",
        "debug" => "debug",
        "warn" => "warn",
        "error" => "error",
        _ => "info",
    }
}

async fn wait_for_signal() {
    let ctrl_c = async {
        let _ = signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let terminate = async {
        if let Ok(mut sig) = signal::unix::signal(signal::unix::SignalKind::terminate()) {
            sig.recv().await;
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        () = ctrl_c => {},
        () = terminate => {},
    }
}

async fn run(cli: Cli, shutdown: CancellationToken) -> Result<()> {
    let client = Client::try_default()
        .await
        .context("build the Kubernetes client")?;

    let mut registry = Registry::default();
    let metrics = Arc::new(Metrics::new(&mut registry));
    let health = Health::default();
    let http = tokio::spawn(server::serve(
        cli.http_port,
        health.clone(),
        Arc::new(registry),
        shutdown.clone(),
    ));

    health.set_ready(true);
    match cli.command {
        Command::Controller(args) => run_controller(client, args, metrics, &shutdown).await?,
        Command::Agent(args) => run_agent(client, args, metrics, &shutdown).await?,
        Command::HostExec(_) => {}
    }
    health.set_ready(false);

    shutdown.cancel();
    http.await
        .context("join the HTTP server")?
        .context("serve HTTP")
}

/// The leader election settings, or `None` when a single replica runs
/// without a `Lease`.
fn leader_settings(args: &ControllerArgs) -> Option<Settings> {
    args.leader_election.then(|| Settings {
        namespace: args.namespace.clone(),
        name: args.leader_election_lease_name.clone(),
        identity: args
            .pod_name
            .clone()
            .unwrap_or_else(|| format!("{}-{}", env!("CARGO_PKG_NAME"), std::process::id())),
        lease_duration: Duration::from_secs(args.leader_election_lease_duration_seconds),
        renew_deadline: Duration::from_secs(args.leader_election_renew_deadline_seconds),
        retry_period: Duration::from_secs(args.leader_election_retry_period_seconds),
    })
}

async fn run_controller(
    client: Client,
    args: ControllerArgs,
    metrics: Arc<Metrics>,
    shutdown: &CancellationToken,
) -> Result<()> {
    let mut elector = match leader_settings(&args) {
        Some(settings) => {
            settings
                .check()
                .context("check the leader election flags")?;
            Some(Elector::new(client.clone(), settings))
        }
        None => None,
    };
    // A standby stays ready and idle until the lease is free.
    if let Some(elector) = elector.as_mut()
        && !elector.acquire(shutdown).await
    {
        return Ok(());
    }
    metrics.leader.set(1);

    let leader = metrics.leader.clone();
    let work = lead(client, args, metrics, shutdown);
    let renewal = async {
        if let Some(elector) = elector.as_mut() {
            elector.hold(shutdown).await;
            if !shutdown.is_cancelled() {
                warn!("Lost the leader lease, stopping so that a standby takes over");
                shutdown.cancel();
            }
        }
    };
    tokio::join!(work, renewal);
    leader.set(0);
    // Released only after the work stopped, so two replicas never act at once.
    if let Some(mut elector) = elector {
        elector.release().await;
    }
    Ok(())
}

async fn lead(
    client: Client,
    args: ControllerArgs,
    metrics: Arc<Metrics>,
    shutdown: &CancellationToken,
) {
    info!(
        namespace = args.namespace,
        agent_template = args.agent_template,
        "Starting the NodeConfiguration controller"
    );
    let agents = tokio::spawn(operator::agentset::run(
        client.clone(),
        operator::agentset::Source {
            namespace: args.namespace,
            config_map: args.agent_template,
        },
        shutdown.clone(),
    ));
    let ctx = Arc::new(operator::Context {
        client: client.clone(),
        recorder: Recorder::new(client.clone(), env!("CARGO_PKG_NAME").to_string().into()),
        metrics,
    });
    let cleanup = tokio::spawn(operator::cleanup::run(ctx.clone(), shutdown.clone()));
    let controller = Controller::new(
        Api::<NodeConfiguration>::all(client.clone()),
        Config::default(),
    );
    let configurations = controller.store();
    controller
        // Node heartbeats arrive in bursts, and one pass covers them all.
        .with_config(controller::Config::default().debounce(Duration::from_secs(2)))
        // A binding is owned by its node, so it names its resource in the spec.
        .watches(
            Api::<NodeConfigurationBinding>::all(client.clone()),
            Config::default(),
            |binding| {
                Some(ObjectRef::<NodeConfiguration>::new(
                    &binding.spec.configuration,
                ))
            },
        )
        .watches(
            Api::<Node>::all(client),
            Config::default().labels(operator::NODE_GROUP_LABEL),
            move |_node| {
                configurations
                    .state()
                    .iter()
                    .map(|configuration| ObjectRef::from_obj(configuration.as_ref()))
                    .collect::<Vec<_>>()
            },
        )
        .graceful_shutdown_on(shutdown.clone().cancelled_owned())
        .run(operator::reconcile, operator::error_policy, ctx)
        .for_each(|result| async move {
            match result {
                Ok((object, _)) => debug!(name = object.name, "Reconciled"),
                Err(e) => warn!("Reconcile did not complete, {e}"),
            }
        })
        .await;
    for (task, result) in [
        ("Agent DaemonSet", agents.await),
        ("Cleanup", cleanup.await),
    ] {
        if let Err(e) = result {
            warn!("{task} loop did not stop cleanly, {e}");
        }
    }
}

async fn run_agent(
    client: Client,
    args: AgentArgs,
    metrics: Arc<Metrics>,
    shutdown: &CancellationToken,
) -> Result<()> {
    info!(
        node = args.node_name,
        host_root = args.host_root,
        allow_scripts = args.allow_scripts,
        "Starting the NodeConfiguration agent"
    );
    let scripts = if args.allow_scripts {
        Scripts::host(&args.host_root).context("locate the agent binary")?
    } else {
        Scripts::Disabled
    };
    let mine = Config::default().labels(&format!("{NODE_LABEL}={}", node_key(&args.node_name)));
    let ctx = Arc::new(agent::Context {
        client: client.clone(),
        host: HostRoot::new(&args.host_root),
        scripts,
        node_name: args.node_name,
        metrics,
        schedule: agent::Schedule::default(),
    });
    Controller::new(Api::<NodeConfigurationBinding>::all(client), mine)
        // One run at a time, so two configurations never write a file at once.
        .with_config(controller::Config::default().concurrency(1))
        .graceful_shutdown_on(shutdown.clone().cancelled_owned())
        .run(agent::reconcile, agent::error_policy, ctx)
        .for_each(|result| async move {
            match result {
                Ok((object, _)) => debug!(binding = object.name, "Applied"),
                Err(e) => warn!("Run did not complete, {e}"),
            }
        })
        .await;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn leader_election_is_off_unless_asked_for() {
        let parse = |extra: &[&str]| {
            let mut flags = vec![
                "node-configuration-operator",
                "controller",
                "--namespace",
                "kube-system",
                "--agent-template",
                "agent-template",
            ];
            flags.extend_from_slice(extra);
            let Command::Controller(args) = Cli::try_parse_from(flags).unwrap().command else {
                panic!("expected the controller command");
            };
            args
        };
        assert!(leader_settings(&parse(&[])).is_none());

        let settings = leader_settings(&parse(&[
            "--leader-election",
            "true",
            "--leader-election-lease-name",
            "nco",
            "--pod-name",
            "nco-7d9f-abcde",
        ]))
        .unwrap();
        assert_eq!(settings.namespace, "kube-system");
        assert_eq!(settings.name, "nco");
        assert_eq!(settings.identity, "nco-7d9f-abcde");
        assert_eq!(settings.lease_duration, Duration::from_secs(15));
        assert_eq!(settings.check(), Ok(()));

        let fallback = leader_settings(&parse(&["--leader-election", "true"])).unwrap();
        assert!(
            fallback
                .identity
                .starts_with("node-configuration-operator-")
        );
    }

    #[test]
    fn unknown_log_levels_fall_back_to_info() {
        assert_eq!(log_level("DEBUG"), "debug");
        assert_eq!(log_level("trace"), "trace");
        assert_eq!(log_level("warn"), "warn");
        assert_eq!(log_level("error"), "error");
        assert_eq!(log_level("verbose"), "info");
    }
}
