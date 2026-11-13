//! Command-line flags of the agent, each backed by an environment variable so the Helm
//! chart configures the binary through `env`.

use clap::{Args, Parser, Subcommand};
use node_configuration_common::boot::CommonArgs;

/// The `node-configuration-agent` binary: applies every
/// `NodeConfigurationBinding` of the node its pod runs on.
#[derive(Parser, Debug)]
#[command(
    name = "node-configuration-agent",
    version,
    about = "Keeps the OS configuration of the node it runs on in line with its NodeConfigurationBindings",
    subcommand_negates_reqs = true
)]
pub struct AgentCli {
    #[command(flatten)]
    pub common: CommonArgs,

    #[command(flatten)]
    pub agent: AgentArgs,

    #[command(subcommand)]
    pub helper: Option<Helper>,
}

#[derive(Subcommand, Debug)]
pub enum Helper {
    /// Internal: run a script inside the host root. The agent starts it.
    #[command(name = "host-exec", hide = true)]
    HostExec(HostExecArgs),
}

#[derive(Args, Debug, Clone)]
pub struct HostExecArgs {
    #[arg(long)]
    pub root: String,

    #[arg(long, allow_hyphen_values = true)]
    pub script: String,
}

#[derive(Args, Debug, Clone)]
pub struct AgentArgs {
    /// Name of the node this pod runs on.
    #[arg(long, env = "NODE_NAME", required = true)]
    pub node_name: Option<String>,

    /// Where the host root filesystem is mounted.
    #[arg(long, env = "HOST_ROOT", default_value = "/host")]
    pub host_root: String,

    /// Whether `script` rules may run. The pod needs to be privileged.
    #[arg(
        long,
        env = "ALLOW_SCRIPTS",
        default_value_t = false,
        action = clap::ArgAction::Set
    )]
    pub allow_scripts: bool,
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory as _;

    #[test]
    fn definition_is_valid() {
        AgentCli::command().debug_assert();
    }

    #[test]
    fn the_script_helper_takes_any_script() {
        let cli = AgentCli::try_parse_from([
            "node-configuration-agent",
            "host-exec",
            "--root",
            "/host",
            "--script",
            "-x; systemctl reload sshd",
        ])
        .unwrap();
        let Some(Helper::HostExec(args)) = cli.helper else {
            panic!("expected the helper command");
        };
        assert_eq!(args.root, "/host");
        assert_eq!(args.script, "-x; systemctl reload sshd");
        assert_eq!(cli.agent.node_name, None);

        let agent = AgentCli::try_parse_from([
            "node-configuration-agent",
            "--node-name",
            "node-1",
            "--allow-scripts",
            "true",
        ])
        .unwrap();
        assert!(agent.helper.is_none() && agent.agent.allow_scripts);
    }

    #[test]
    fn agent_flags_parse() {
        let cli = AgentCli::try_parse_from([
            "node-configuration-agent",
            "--node-name",
            "node-1",
            "--http-port",
            "9000",
        ])
        .unwrap();
        assert_eq!(cli.common.http_port, 9000);
        let args = cli.agent;
        assert_eq!(args.node_name.as_deref(), Some("node-1"));
        assert_eq!(args.host_root, "/host");
        assert!(!args.allow_scripts);
        assert!(AgentCli::try_parse_from(["node-configuration-agent"]).is_err());
    }
}
