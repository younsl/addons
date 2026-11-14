//! Command-line flags of the controller, each backed by an environment variable so the Helm
//! chart configures the binary through `env`.

use clap::{Args, Parser};
use node_configuration_common::boot::CommonArgs;

/// The `node-configuration-operator` binary: rolls each `NodeConfiguration`
/// out to its nodes and reports the rollout.
#[derive(Parser, Debug)]
#[command(
    name = "node-configuration-operator",
    version,
    about = "Rolls NodeConfiguration resources out to EKS managed node group nodes"
)]
pub struct ControllerCli {
    #[command(flatten)]
    pub common: CommonArgs,

    #[command(flatten)]
    pub controller: ControllerArgs,
}

#[derive(Args, Debug, Clone)]
pub struct ControllerArgs {
    /// Namespace the agent `DaemonSet` and its template live in.
    #[arg(long, env = "POD_NAMESPACE")]
    pub namespace: String,

    /// Name of the `ConfigMap` that holds the agent `DaemonSet` template.
    #[arg(long, env = "AGENT_TEMPLATE")]
    pub agent_template: String,

    /// Whether to act only while holding a `Lease`, which more than one
    /// replica needs.
    #[arg(
        long,
        env = "LEADER_ELECTION",
        default_value_t = false,
        action = clap::ArgAction::Set
    )]
    pub leader_election: bool,

    /// Name of the `Lease` in the namespace of the controller.
    #[arg(
        long,
        env = "LEADER_ELECTION_LEASE_NAME",
        default_value = "node-configuration-operator"
    )]
    pub leader_election_lease_name: String,

    /// Seconds a standby waits on a `Lease` that is not renewed before it
    /// takes over.
    #[arg(
        long,
        env = "LEADER_ELECTION_LEASE_DURATION_SECONDS",
        default_value_t = 15
    )]
    pub leader_election_lease_duration_seconds: u64,

    /// Seconds the leader keeps trying to renew before it steps down.
    #[arg(
        long,
        env = "LEADER_ELECTION_RENEW_DEADLINE_SECONDS",
        default_value_t = 10
    )]
    pub leader_election_renew_deadline_seconds: u64,

    /// Seconds between two attempts to take or renew the `Lease`.
    #[arg(
        long,
        env = "LEADER_ELECTION_RETRY_PERIOD_SECONDS",
        default_value_t = 2
    )]
    pub leader_election_retry_period_seconds: u64,

    /// Name of this pod, written into the `Lease` as the holder.
    #[arg(long, env = "POD_NAME")]
    pub pod_name: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory as _;

    #[test]
    fn definition_is_valid() {
        ControllerCli::command().debug_assert();
    }

    #[test]
    fn controller_flags_parse() {
        let cli = ControllerCli::try_parse_from([
            "node-configuration-operator",
            "--namespace",
            "kube-system",
            "--agent-template",
            "agent-template",
            "--log-format",
            "text",
        ])
        .unwrap();
        assert_eq!(cli.common.log_level, "info");
        assert_eq!(cli.common.log_format, "text");
        assert_eq!(cli.common.http_port, 8080);
        let args = cli.controller;
        assert_eq!(args.namespace, "kube-system");
        assert_eq!(args.agent_template, "agent-template");
        assert!(!args.leader_election);
        assert_eq!(
            args.leader_election_lease_name,
            "node-configuration-operator"
        );
        assert_eq!(args.leader_election_lease_duration_seconds, 15);
        assert_eq!(args.leader_election_renew_deadline_seconds, 10);
        assert_eq!(args.leader_election_retry_period_seconds, 2);
        assert!(ControllerCli::try_parse_from(["node-configuration-operator"]).is_err());
        // The agent is a binary of its own, no longer a subcommand.
        assert!(ControllerCli::try_parse_from(["node-configuration-operator", "agent"]).is_err());
    }
}
