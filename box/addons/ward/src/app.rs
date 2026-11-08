//! Command line: one binary for the operator, the manager, the agent, and
//! the feed tooling.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Args, Parser, Subcommand};

use crate::agent::AgentArgs;
use crate::content::FeedDb;
use crate::feed::build::{BuildInput, build, check};
use crate::manager::ManagerArgs;
use crate::observability::{shutdown_channel, subscriber};
use crate::operator::OperatorArgs;
use crate::operator::resources::Defaults;

#[derive(Parser, Debug)]
#[command(
    name = "ward",
    about = "Kubernetes-native antivirus and EDR",
    version = concat!(env!("CARGO_PKG_VERSION"), " (commit ", env!("BUILD_COMMIT"), ", built ", env!("BUILD_DATE"), ")")
)]
struct Cli {
    /// Log level: trace, debug, info, warn, error. `RUST_LOG` overrides it.
    #[arg(long, env = "LOG_LEVEL", default_value = "info", global = true)]
    log_level: String,
    /// Log format: json or text.
    #[arg(long, env = "LOG_FORMAT", default_value = "json", global = true)]
    log_format: String,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Reconcile Ward objects
    Operator(OperatorCmd),
    /// Run the feed relay and detection store
    Manager(ManagerCmd),
    /// Run the node agent
    Agent(AgentCmd),
    /// Build or check a signature database
    #[command(subcommand)]
    Feed(FeedCmd),
}

#[derive(Args, Debug)]
struct OperatorCmd {
    #[arg(long, env = "HEALTH_ADDR", default_value = "0.0.0.0:8081")]
    health_addr: SocketAddr,
    /// Image of the manager and agent, unless a Ward sets its own
    #[arg(long, env = "WARD_IMAGE")]
    image: String,
    #[arg(long, env = "WARD_IMAGE_PULL_POLICY", default_value = "IfNotPresent")]
    image_pull_policy: String,
    #[arg(
        long,
        env = "WARD_WORKLOAD_CLUSTER_ROLE",
        default_value = "ward-workload"
    )]
    workload_cluster_role: String,
    #[arg(
        long,
        env = "WARD_LEASE_CLUSTER_ROLE",
        default_value = "ward-agent-lease"
    )]
    lease_cluster_role: String,
    /// The only namespace a Ward may live in. Defaults to the operator's own.
    #[arg(long, env = "WARD_NAMESPACE")]
    ward_namespace: Option<String>,
    /// Prefixes a Ward's `spec.image` must start with. Empty allows only
    /// the operator's own image repository.
    #[arg(long, env = "WARD_ALLOWED_IMAGE_PREFIXES", value_delimiter = ',')]
    allowed_image_prefixes: Vec<String>,
    /// Honor `spec.agent.privileged`. Off, such a Ward is refused.
    #[arg(long, env = "WARD_ALLOW_PRIVILEGED_AGENT", default_value_t = false)]
    allow_privileged_agent: bool,
    #[arg(long, env = "LEADER_ELECT", default_value_t = false)]
    leader_elect: bool,
    #[arg(long, env = "POD_NAME", default_value = "ward-operator")]
    pod_name: String,
    #[arg(long, env = "POD_NAMESPACE", default_value = "default")]
    pod_namespace: String,
}

#[derive(Args, Debug)]
struct ManagerCmd {
    #[arg(long, env = "AGENT_ADDR", default_value = "0.0.0.0:8443")]
    agent_addr: SocketAddr,
    #[arg(long, env = "ADMIN_ADDR", default_value = "0.0.0.0:8080")]
    admin_addr: SocketAddr,
    #[arg(long, env = "HEALTH_ADDR", default_value = "0.0.0.0:8081")]
    health_addr: SocketAddr,
    #[arg(long, env = "WARD_DATA_DIR", default_value = "/var/lib/ward")]
    data_dir: PathBuf,
    #[arg(long, env = "WARD_POLICY_DIR", default_value = "/etc/ward/policy")]
    policy_dir: PathBuf,
    #[arg(long, env = "WARD_ROLLOUT_DIR", default_value = "/etc/ward/rollout")]
    rollout_dir: PathBuf,
    #[arg(long, env = "WARD_TLS_DIR", default_value = "/etc/ward/tls")]
    tls_dir: PathBuf,
    #[arg(
        long,
        env = "WARD_API_TOKEN_FILE",
        default_value = "/etc/ward/api-token/token"
    )]
    token_file: PathBuf,
    #[arg(long, env = "WARD_REGISTRY_AUTH")]
    registry_auth: Option<PathBuf>,
    #[arg(long, env = "WARD_RETENTION", default_value_t = 10_000)]
    retention: usize,
    /// Namespace of the agent `ServiceAccount` whose tokens are accepted
    #[arg(long, env = "POD_NAMESPACE", default_value = "default")]
    namespace: String,
}

#[derive(Args, Debug)]
struct AgentCmd {
    #[arg(long, env = "NODE_NAME")]
    node: String,
    #[arg(long, env = "POD_NAMESPACE", default_value = "default")]
    namespace: String,
    #[arg(long, env = "WARD_INSTANCE", default_value = "default")]
    instance: String,
    #[arg(long, env = "WARD_POLICY_DIR", default_value = "/etc/ward/policy")]
    policy_dir: PathBuf,
    #[arg(long, env = "WARD_ROLLOUT_DIR", default_value = "/etc/ward/rollout")]
    rollout_dir: PathBuf,
    #[arg(long, env = "WARD_STATE_DIR", default_value = "/var/lib/ward")]
    state_dir: PathBuf,
    #[arg(long, env = "PROC_ROOT", default_value = "/proc")]
    proc_root: PathBuf,
    /// Manager agent API. Unset, the agent pulls the feed itself.
    #[arg(long, env = "WARD_MANAGER_URL")]
    manager_url: Option<String>,
    #[arg(long, env = "WARD_TLS_DIR", default_value = "/etc/ward/tls")]
    tls_dir: PathBuf,
    /// Projected `ServiceAccount` token with the manager audience
    #[arg(
        long,
        env = "WARD_MANAGER_TOKEN_FILE",
        default_value = "/var/run/secrets/ward/manager/token"
    )]
    manager_token_file: PathBuf,
    #[arg(long, env = "WARD_REGISTRY_AUTH")]
    registry_auth: Option<PathBuf>,
    #[arg(long, env = "HEALTH_ADDR", default_value = "0.0.0.0:8081")]
    health_addr: SocketAddr,
    #[arg(long, env = "WARD_TRACEFS", default_value = "/sys/kernel/tracing")]
    tracefs: PathBuf,
    #[arg(long, env = "WARD_BTF", default_value = "/sys/kernel/btf/vmlinux")]
    btf: PathBuf,
}

#[derive(Subcommand, Debug)]
enum FeedCmd {
    /// Assemble a database from local source files
    Build {
        #[arg(long)]
        out: PathBuf,
        #[arg(long)]
        revision: Option<String>,
        /// `MalwareBazaar` SHA-256 dump, one digest per line
        #[arg(long)]
        malwarebazaar: Vec<PathBuf>,
        /// Directory of .yar or .yara files
        #[arg(long = "yara-dir")]
        yara_dirs: Vec<PathBuf>,
        /// YAML list of signatures
        #[arg(long)]
        signatures: Vec<PathBuf>,
    },
    /// Self-test a database and scan a false-positive corpus
    Check {
        #[arg(long)]
        db: PathBuf,
        #[arg(long)]
        corpus: Vec<PathBuf>,
    },
}

fn feed(cmd: FeedCmd) -> anyhow::Result<()> {
    match cmd {
        FeedCmd::Build {
            out,
            revision,
            malwarebazaar,
            yara_dirs,
            signatures,
        } => {
            let (db, report) = build(&BuildInput {
                revision,
                malwarebazaar,
                yara_dirs,
                signatures,
            })?;
            std::fs::write(&out, db.encode())?;
            for d in &report.dropped_yara {
                eprintln!("dropped {d}");
            }
            println!(
                "{} revision {}: {} hashes, {} signatures, {} YARA files ({} dropped)",
                out.display(),
                db.manifest.revision,
                report.hashes,
                report.signatures,
                report.yara_files,
                report.dropped_yara.len()
            );
        }
        FeedCmd::Check { db, corpus } => {
            let db = FeedDb::decode(&std::fs::read(&db)?)?;
            let report = check(&db, &corpus)?;
            println!(
                "revision {}: self-test passed, {} corpus files clean",
                db.manifest.revision, report.scanned
            );
        }
    }
    Ok(())
}

fn runtime() -> anyhow::Result<tokio::runtime::Runtime> {
    Ok(tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()?)
}

fn dispatch(cli: Cli) -> anyhow::Result<()> {
    match cli.command {
        Command::Feed(cmd) => feed(cmd),
        Command::Operator(c) => runtime()?.block_on(async {
            let args = OperatorArgs {
                defaults: Defaults {
                    image: c.image,
                    image_pull_policy: c.image_pull_policy,
                    workload_cluster_role: c.workload_cluster_role,
                    lease_cluster_role: c.lease_cluster_role,
                    ward_namespace: c.ward_namespace.unwrap_or_else(|| c.pod_namespace.clone()),
                    allowed_image_prefixes: c
                        .allowed_image_prefixes
                        .into_iter()
                        .filter(|p| !p.is_empty())
                        .collect(),
                    allow_privileged_agent: c.allow_privileged_agent,
                },
                leader_elect: c.leader_elect,
                pod_name: c.pod_name,
                pod_namespace: c.pod_namespace,
                health_addr: c.health_addr,
            };
            crate::operator::run(args, shutdown_channel()).await
        }),
        Command::Manager(c) => runtime()?.block_on(async {
            let args = ManagerArgs {
                agent_addr: c.agent_addr,
                admin_addr: c.admin_addr,
                health_addr: c.health_addr,
                data_dir: c.data_dir,
                policy_dir: c.policy_dir,
                rollout_dir: c.rollout_dir,
                tls_dir: c.tls_dir,
                token_file: c.token_file,
                registry_auth: c.registry_auth,
                retention: c.retention,
                namespace: c.namespace,
            };
            crate::manager::run(args, shutdown_channel()).await
        }),
        Command::Agent(c) => runtime()?.block_on(async {
            let args = AgentArgs {
                node: c.node,
                namespace: c.namespace,
                instance: c.instance,
                policy_dir: c.policy_dir,
                rollout_dir: c.rollout_dir,
                state_dir: c.state_dir,
                proc_root: c.proc_root,
                manager_url: c.manager_url,
                tls_dir: c.tls_dir,
                manager_token_file: c.manager_token_file,
                registry_auth: c.registry_auth,
                health_addr: c.health_addr,
                tracefs: c.tracefs,
                btf: c.btf,
            };
            crate::agent::run(args, shutdown_channel()).await
        }),
    }
}

pub fn start() -> ExitCode {
    crate::tls::install_provider();
    let cli = Cli::parse();
    if tracing::subscriber::set_global_default(subscriber(&cli.log_level, &cli.log_format)).is_err()
    {
        eprintln!("a global tracing subscriber was already installed");
        return ExitCode::FAILURE;
    }
    match dispatch(cli) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            tracing::error!(error = format!("{e:#}"), "exiting");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use clap::CommandFactory as _;

    use super::*;

    #[test]
    fn cli_parses_every_role() {
        Cli::command().debug_assert();
        let c = Cli::try_parse_from(["ward", "operator", "--image", "x:1"]).unwrap();
        assert!(
            matches!(c.command, Command::Operator(OperatorCmd { ref image, .. }) if image == "x:1")
        );
        let c = Cli::try_parse_from(["ward", "agent", "--node", "n1"]).unwrap();
        assert!(matches!(c.command, Command::Agent(AgentCmd { ref node, .. }) if node == "n1"));
        let c = Cli::try_parse_from(["ward", "--log-format", "text", "manager"]).unwrap();
        assert_eq!(c.log_format, "text");
        assert!(Cli::try_parse_from(["ward", "crd"]).is_err());
        assert!(Cli::command().render_version().contains("commit "));
    }

    #[test]
    fn feed_build_then_check() {
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("db.json.gz");
        let mb = dir.path().join("mb.txt");
        std::fs::write(&mb, "a".repeat(64)).unwrap();
        feed(FeedCmd::Build {
            out: out.clone(),
            revision: Some("t".into()),
            malwarebazaar: vec![mb],
            yara_dirs: vec![],
            signatures: vec![],
        })
        .unwrap();
        let corpus = dir.path().join("corpus");
        std::fs::create_dir_all(&corpus).unwrap();
        feed(FeedCmd::Check {
            db: out,
            corpus: vec![corpus],
        })
        .unwrap();
        assert!(
            feed(FeedCmd::Check {
                db: dir.path().join("missing"),
                corpus: vec![]
            })
            .is_err()
        );
    }
}
