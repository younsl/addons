//! Wires the clients, the HTTP server, and the controller or agent loop
//! together.

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context as _, Result};
use clap::Parser as _;
use futures::StreamExt as _;
use k8s_openapi::api::core::v1::Node;
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
        Command::Controller(args) => run_controller(client, args, metrics, &shutdown).await,
        Command::Agent(args) => run_agent(client, args, metrics, &shutdown).await?,
        Command::HostExec(_) => {}
    }
    health.set_ready(false);

    shutdown.cancel();
    http.await
        .context("join the HTTP server")?
        .context("serve HTTP")
}

async fn run_controller(
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
        metrics,
    });
    let controller = Controller::new(
        Api::<NodeConfiguration>::all(client.clone()),
        Config::default(),
    );
    let configurations = controller.store();
    controller
        // Node heartbeats arrive in bursts, and one pass covers them all.
        .with_config(controller::Config::default().debounce(Duration::from_secs(2)))
        .owns(
            Api::<NodeConfigurationBinding>::all(client.clone()),
            Config::default(),
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
    if let Err(e) = agents.await {
        warn!("Agent DaemonSet loop did not stop cleanly, {e}");
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
    fn unknown_log_levels_fall_back_to_info() {
        assert_eq!(log_level("DEBUG"), "debug");
        assert_eq!(log_level("trace"), "trace");
        assert_eq!(log_level("warn"), "warn");
        assert_eq!(log_level("error"), "error");
        assert_eq!(log_level("verbose"), "info");
    }
}
