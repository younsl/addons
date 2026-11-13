//! Wires the clients and the agent loop together.

use std::sync::Arc;

use anyhow::{Context as _, Result};
use clap::Parser as _;
use futures::StreamExt as _;
use kube::runtime::watcher::Config;
use kube::runtime::{Controller, controller};
use kube::{Api, Client};
use node_configuration_common::boot;
use node_configuration_common::crd::binding::{NODE_LABEL, NodeConfigurationBinding, node_key};
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use crate::cli::{AgentArgs, AgentCli, Helper};
use crate::hostfs::HostRoot;
use crate::metrics::Metrics;
use crate::script::{self, Scripts};
use crate::{BUILD, reconcile};

/// Runs the agent until a shutdown signal arrives.
pub async fn start() {
    let cli = AgentCli::parse();
    if let Some(Helper::HostExec(args)) = &cli.helper {
        let error = script::host_exec(&args.root, &args.script);
        eprintln!("cannot run the script in {}, {error}", args.root);
        std::process::exit(126);
    }
    let args = cli.agent;
    boot::start(&BUILD, &cli.common, |client, registry, shutdown| {
        let metrics = Arc::new(Metrics::new(registry));
        async move { run_agent(client, args, metrics, &shutdown).await }
    })
    .await;
}

async fn run_agent(
    client: Client,
    args: AgentArgs,
    metrics: Arc<Metrics>,
    shutdown: &CancellationToken,
) -> Result<()> {
    let node_name = args.node_name.context("the node name is required")?;
    info!(
        node = node_name,
        host_root = args.host_root,
        allow_scripts = args.allow_scripts,
        "Starting the NodeConfiguration agent"
    );
    let scripts = if args.allow_scripts {
        Scripts::host(&args.host_root).context("locate the agent binary")?
    } else {
        Scripts::Disabled
    };
    let mine = Config::default().labels(&format!("{NODE_LABEL}={}", node_key(&node_name)));
    let ctx = Arc::new(reconcile::Context {
        client: client.clone(),
        host: HostRoot::new(&args.host_root),
        scripts,
        node_name,
        metrics,
        schedule: reconcile::Schedule::default(),
    });
    Controller::new(Api::<NodeConfigurationBinding>::all(client), mine)
        // One run at a time, so two configurations never write a file at once.
        .with_config(controller::Config::default().concurrency(1))
        .graceful_shutdown_on(shutdown.clone().cancelled_owned())
        .run(reconcile::reconcile, reconcile::error_policy, ctx)
        .for_each(|result| async move {
            match result {
                Ok((object, _)) => debug!(binding = object.name, "Applied"),
                Err(e) => warn!("Run did not complete, {e}"),
            }
        })
        .await;
    Ok(())
}
