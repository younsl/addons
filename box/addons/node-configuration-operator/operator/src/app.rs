//! Wires the clients and the controller loops together.

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
use node_configuration_common::boot;
use node_configuration_common::crd::NodeConfiguration;
use node_configuration_common::crd::binding::NodeConfigurationBinding;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use crate::cli::{ControllerArgs, ControllerCli};
use crate::controller::leader::{Elector, Settings};
use crate::metrics::Metrics;
use crate::{BUILD, controller as operator};

/// Runs the controller until a shutdown signal arrives.
pub async fn start() {
    let cli = ControllerCli::parse();
    let args = cli.controller;
    boot::start(&BUILD, &cli.common, |client, registry, shutdown| {
        let metrics = Arc::new(Metrics::new(registry));
        async move { run_controller(client, args, metrics, &shutdown).await }
    })
    .await;
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn leader_election_is_off_unless_asked_for() {
        let parse = |extra: &[&str]| {
            let mut flags = vec![
                "node-configuration-operator",
                "--namespace",
                "kube-system",
                "--agent-template",
                "agent-template",
            ];
            flags.extend_from_slice(extra);
            ControllerCli::try_parse_from(flags).unwrap().controller
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
}
