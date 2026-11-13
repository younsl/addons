//! Command-line flags, each backed by an environment variable so the Helm
//! chart configures the binary through `env`.

use clap::{Args, Parser, Subcommand};

#[derive(Parser, Debug)]
#[command(
    name = "node-configuration-operator",
    version,
    about = "Continuously reconciles the OS configuration of EKS managed node group nodes"
)]
pub struct Cli {
    /// Log level (`trace`, `debug`, `info`, `warn`, `error`). `RUST_LOG` overrides it.
    #[arg(long, env = "LOG_LEVEL", default_value = "info", global = true)]
    pub log_level: String,

    /// Log format (`json` or `text`).
    #[arg(long, env = "LOG_FORMAT", default_value = "json", global = true)]
    pub log_format: String,

    /// Port of the health and metrics server.
    #[arg(long, env = "HTTP_PORT", default_value_t = 8080, global = true)]
    pub http_port: u16,

    #[command(subcommand)]
    pub command: Command,
}

#[derive(Subcommand, Debug)]
pub enum Command {
    /// Roll each `NodeConfiguration` out to its nodes and report the rollout.
    Controller(ControllerArgs),
    /// Apply every `NodeConfigurationBinding` of the node this pod runs on.
    Agent(AgentArgs),
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
pub struct ControllerArgs {
    /// Namespace the agent `DaemonSet` and its template live in.
    #[arg(long, env = "POD_NAMESPACE")]
    pub namespace: String,

    /// Name of the `ConfigMap` that holds the agent `DaemonSet` template.
    #[arg(long, env = "AGENT_TEMPLATE")]
    pub agent_template: String,
}

#[derive(Args, Debug, Clone)]
pub struct AgentArgs {
    /// Name of the node this pod runs on.
    #[arg(long, env = "NODE_NAME")]
    pub node_name: String,

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
        Cli::command().debug_assert();
    }

    #[test]
    fn controller_flags_parse() {
        let cli = Cli::try_parse_from([
            "node-configuration-operator",
            "controller",
            "--namespace",
            "kube-system",
            "--agent-template",
            "agent-template",
            "--log-format",
            "text",
        ])
        .unwrap();
        assert_eq!(cli.log_level, "info");
        assert_eq!(cli.log_format, "text");
        assert_eq!(cli.http_port, 8080);
        let Command::Controller(args) = cli.command else {
            panic!("expected the controller command");
        };
        assert_eq!(args.namespace, "kube-system");
        assert_eq!(args.agent_template, "agent-template");
        assert!(Cli::try_parse_from(["node-configuration-operator", "controller"]).is_err());
    }

    #[test]
    fn the_script_helper_takes_any_script() {
        let cli = Cli::try_parse_from([
            "node-configuration-operator",
            "host-exec",
            "--root",
            "/host",
            "--script",
            "-x; systemctl reload sshd",
        ])
        .unwrap();
        let Command::HostExec(args) = cli.command else {
            panic!("expected the helper command");
        };
        assert_eq!(args.root, "/host");
        assert_eq!(args.script, "-x; systemctl reload sshd");

        let agent = Cli::try_parse_from([
            "node-configuration-operator",
            "agent",
            "--node-name",
            "node-1",
            "--allow-scripts",
            "true",
        ])
        .unwrap();
        assert!(matches!(agent.command, Command::Agent(args) if args.allow_scripts));
    }

    #[test]
    fn agent_flags_parse() {
        let cli = Cli::try_parse_from([
            "node-configuration-operator",
            "agent",
            "--node-name",
            "node-1",
            "--http-port",
            "9000",
        ])
        .unwrap();
        assert_eq!(cli.http_port, 9000);
        let Command::Agent(args) = cli.command else {
            panic!("expected the agent command");
        };
        assert_eq!(args.node_name, "node-1");
        assert_eq!(args.host_root, "/host");
        assert!(!args.allow_scripts);
        assert!(Cli::try_parse_from(["node-configuration-operator", "agent"]).is_err());
    }
}
