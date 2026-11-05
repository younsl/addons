use std::io::IsTerminal;
use std::sync::Arc;

use anyhow::{Context, bail};
use tokio::sync::{mpsc, oneshot};

use crate::aws::catalog::ElastiCacheCatalog;
use crate::aws::enis::EniAddresses;
use crate::aws::metrics::CloudWatchMetrics;
use crate::collector::{Collector, CollectorConfig, Sources};
use crate::config::{self, Cli, FileConfig, Settings};
use crate::db::{DbClient, DbRequest};
use crate::k8s::pods::Scope;
use crate::k8s::source::{KubePods, ProbeConfig};
use crate::ui::{Backend, DbReplyRx, DbTarget, Env, Watch};
use crate::{aws, k8s, logging, ui};

pub async fn run(cli: Cli) -> anyhow::Result<()> {
    let file = FileConfig::load(cli.config.as_deref())?;

    if cli.list_targets {
        let Some((path, file)) = &file else {
            let looked =
                config::default_path().map_or_else(|| "-".into(), |p| p.display().to_string());
            bail!("no config file (looked for {looked})");
        };
        println!("# {}", path.display());
        for line in file.describe_targets() {
            println!("{line}");
        }
        return Ok(());
    }

    let settings = config::resolve(&cli, file.as_ref().map(|(_, f)| f))?;
    if !std::io::stdin().is_terminal() || !std::io::stdout().is_terminal() {
        bail!("an interactive terminal is required (stdin and stdout must be a TTY)");
    }
    logging::init(cli.log_file.as_deref(), &cli.log_level, cli.log_format)?;
    start(settings).await
}

/// Live backend: AWS APIs for groups, addresses, and metrics, Kubernetes for pods and the
/// probe tunnel.
struct LiveBackend {
    sdk: aws_config::SdkConfig,
    pods: Arc<KubePods>,
    settings: Settings,
}

impl Backend for LiveBackend {
    fn watch(&self) -> Watch {
        let sources = Sources {
            catalog: ElastiCacheCatalog::new(aws_sdk_elasticache::Client::new(&self.sdk)),
            addresses: EniAddresses::new(aws_sdk_ec2::Client::new(&self.sdk)),
            metrics: CloudWatchMetrics::new(aws_sdk_cloudwatch::Client::new(&self.sdk)),
            pods: Arc::clone(&self.pods),
        };
        let config = CollectorConfig {
            port: self.settings.port,
            interval: self.settings.interval,
            parallel: self.settings.parallel,
        };
        let (tx, rx) = mpsc::channel(8);
        let (refresh_tx, refresh_rx) = mpsc::channel(1);
        let task = tokio::spawn(Collector::new(sources, config).run(tx, refresh_rx));
        Watch::new(rx, refresh_tx, task)
    }

    fn db(&self, target: DbTarget, request: DbRequest) -> DbReplyRx {
        let client = DbClient::new(
            Arc::clone(&self.pods),
            target.gateways,
            target.host,
            self.settings.port,
        );
        let (tx, rx) = oneshot::channel();
        tokio::spawn(async move {
            let result = client.run(&request).await.map_err(|e| e.to_string());
            if let Err(e) = &result {
                tracing::warn!(error = %e, "database query failed");
            }
            let _ = tx.send(result);
        });
        rx
    }
}

async fn start(settings: Settings) -> anyhow::Result<()> {
    let (kube, context, default_ns) = k8s::client::connect(settings.context.as_deref())
        .await
        .context("connecting to Kubernetes")?;
    let (scope, scope_label) = if settings.all_namespaces {
        (Scope::All, "*".to_owned())
    } else if settings.namespaces.is_empty() {
        (Scope::Namespaces(vec![default_ns.clone()]), default_ns)
    } else {
        (
            Scope::Namespaces(settings.namespaces.clone()),
            settings.namespaces.join(","),
        )
    };

    let sdk = aws::client::sdk_config(
        settings.aws_profile.as_deref(),
        settings.aws_region.as_deref(),
    )
    .await;
    let env = Env {
        target: settings.target.clone(),
        context,
        scope: scope_label,
        profile: settings
            .aws_profile
            .clone()
            .unwrap_or_else(|| "default".into()),
        region: sdk.region().map_or_else(|| "-".into(), ToString::to_string),
        interval: settings.interval,
    };
    let pods = KubePods::new(
        kube,
        scope,
        settings.selector.clone(),
        settings.exec_timeout,
        ProbeConfig {
            image: settings.probe_image.clone(),
            start_timeout: settings.probe_timeout,
        },
    );
    let backend = LiveBackend {
        sdk,
        pods: Arc::new(pods),
        settings,
    };

    let terminal = ratatui::init();
    let result = ui::run(terminal, &backend, env).await;
    ratatui::restore();
    result.context("terminal UI")
}
