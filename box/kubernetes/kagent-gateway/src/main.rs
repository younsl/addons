//! kagent-gateway receives Alertmanager webhooks, posts each alert to Slack,
//! asks a kagent agent to investigate it over A2A, and replies with the
//! analysis in the alert's thread. Mentioning the bot inside a thread runs the
//! agent again and answers there.

mod a2a;
mod alert;
mod config;
mod gateway;
mod observability;
mod slack;

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context as _, Result};
use clap::Parser;
use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn};
use tracing_subscriber::{EnvFilter, fmt, prelude::*};

use crate::config::{Config, ParentMode, format_duration};
use crate::observability::Metrics;

const BUILD_COMMIT: &str = env!("BUILD_COMMIT");
const BUILD_DATE: &str = env!("BUILD_DATE");
const RUSTC_VERSION: &str = env!("BUILD_RUSTC_VERSION");

/// Added to the analysis deadline to form the shutdown drain timeout, so a run
/// that already cost model tokens still gets its Slack reply posted. The
/// margin covers the reply's own Slack calls.
const DRAIN_MARGIN: Duration = Duration::from_secs(30);

/// Every setting is read from the environment; the flags only cover what a
/// shell needs to ask directly.
#[derive(Parser)]
#[command(name = "kagent-gateway", version, about)]
struct Cli {
    /// Enable debug logging regardless of `LOG_LEVEL`.
    #[arg(short, long)]
    verbose: bool,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let cfg = match Config::load() {
        Ok(cfg) => cfg,
        Err(err) => {
            // Config failed to load, so log the error with the default level
            // and format to keep the output structured.
            init_tracing("info", "json");
            error!(error = %err, "configuration error");
            std::process::exit(1);
        }
    };
    init_tracing(
        if cli.verbose { "debug" } else { &cfg.log_level },
        &cfg.log_format,
    );

    let shutdown = CancellationToken::new();
    tokio::spawn({
        let shutdown = shutdown.clone();
        async move {
            wait_for_signal().await;
            info!("shutdown signal received");
            shutdown.cancel();
        }
    });

    run(cfg, shutdown).await
}

/// Wires the gateway and HTTP servers and blocks until shutdown.
async fn run(cfg: Config, shutdown: CancellationToken) -> Result<()> {
    info!(
        version = env!("CARGO_PKG_VERSION"),
        commit = BUILD_COMMIT,
        built = BUILD_DATE,
        rustc = RUSTC_VERSION,
        kagent_url = cfg.kagent_url,
        kagent_namespace = cfg.kagent_namespace,
        agents = cfg.agents().join(","),
        default_agent = cfg.kagent_agent,
        agent_routing_label = cfg.kagent_agent_routing_label,
        agent_timeout = format_duration(cfg.kagent_timeout),
        default_channel = cfg.slack_channel,
        parent_mode = cfg.parent_mode.as_str(),
        webhook_path = cfg.webhook_path,
        listen_port = cfg.listen_port,
        metrics_port = cfg.metrics_port,
        "starting kagent-gateway"
    );
    if cfg.parent_mode == ParentMode::Lookup {
        info!(
            lookup_window = format_duration(cfg.lookup_window),
            lookup_attempts = cfg.lookup_attempts,
            required_scopes = "chat:write, channels:history, channels:read (groups:* for private channels, reactions:write for reactions)",
            "alertmanager owns the alert notification; the gateway only threads under it"
        );
    }
    info!(
        severities = severity_list(&cfg),
        analyze_label = cfg.analyze_label,
        analyze_resolved = cfg.analyze_resolved,
        dedupe_ttl = format_duration(cfg.dedupe_ttl),
        max_concurrent = cfg.max_concurrent,
        "analysis policy"
    );
    if cfg.webhook_token.is_empty() {
        warn!(
            hint = "set WEBHOOK_BEARER_TOKEN to require a bearer token",
            "webhook authentication disabled"
        );
    }

    let metrics = Arc::new(Metrics::new());
    metrics.register_build_info(env!("CARGO_PKG_VERSION"), BUILD_COMMIT, RUSTC_VERSION);

    let slack_client = Arc::new(slack::Client::new(
        &cfg.slack_api_url,
        &cfg.slack_token,
        Duration::from_secs(30),
        metrics.clone(),
    ));
    let agent = Arc::new(a2a::Client::new(
        &cfg.kagent_url,
        &cfg.kagent_namespace,
        &cfg.kagent_user_id,
        cfg.kagent_request_timeout,
        cfg.kagent_poll_interval,
        metrics.clone(),
    ));
    let gateway = gateway::Gateway::new(cfg.clone(), slack_client.clone(), agent, metrics.clone());

    if cfg.chat_enabled() {
        start_chat(&cfg, &slack_client, &gateway, &metrics, shutdown.clone()).await;
    }

    let metrics_listener = observability::server::bind(cfg.metrics_port).await?;
    let webhook_listener = observability::server::bind(cfg.listen_port).await?;

    tokio::spawn({
        let (router, shutdown) = (
            observability::server::metrics_router(metrics.clone()),
            shutdown.clone(),
        );
        async move {
            if let Err(err) = observability::server::serve(metrics_listener, router, shutdown).await
            {
                error!(error = %err, "metrics server failed");
            }
        }
    });

    observability::server::serve(webhook_listener, gateway.router(), shutdown.clone())
        .await
        .context("webhook server")?;

    // The listener is closed, so no new analysis can start. Anything already
    // running still owes a Slack reply, so wait for it before exiting. A
    // mention turn has its own deadline, so the drain covers whichever is
    // longer. The pod's terminationGracePeriodSeconds must exceed this window.
    let drain_timeout = if cfg.chat_enabled() {
        cfg.kagent_timeout.max(cfg.chat_timeout) + DRAIN_MARGIN
    } else {
        cfg.kagent_timeout + DRAIN_MARGIN
    };
    info!(
        timeout = format_duration(drain_timeout),
        "draining in-flight runs"
    );
    if !gateway.wait(drain_timeout).await {
        warn!(
            timeout = format_duration(drain_timeout),
            "shutdown timed out with runs still in flight"
        );
        return Ok(());
    }
    info!("shutdown complete");
    Ok(())
}

/// Resolves the bot identity and opens the Socket Mode connection that carries
/// mentions. Neither failure is fatal: the alert path must keep working when
/// Slack cannot be reached, so the connection retries in the background and an
/// unresolved bot id only leaves the loop guard resting on `bot_id` alone.
async fn start_chat(
    cfg: &Config,
    slack_client: &Arc<slack::Client>,
    gateway: &Arc<gateway::Gateway>,
    metrics: &Arc<Metrics>,
    shutdown: CancellationToken,
) {
    info!(
        chat_agent = cfg.chat_agent,
        chat_timeout = format_duration(cfg.chat_timeout),
        session_ttl = format_duration(cfg.chat_session_ttl),
        status_interval = format_duration(cfg.chat_status_interval),
        max_concurrent_chats = cfg.max_concurrent_chats,
        channels = if cfg.chat_channels.is_empty() {
            "all".to_string()
        } else {
            cfg.chat_channels.join(",")
        },
        required_scopes = "app_mentions:read, chat:write, reactions:write",
        "mention invocation enabled"
    );
    // The two allow list states differ in who can spend agent tokens, so each
    // gets its own line rather than a value a reader has to interpret.
    if cfg.chat_allowed_users.is_empty() {
        warn!(
            allowed_user_count = 0,
            hint = "set CHAT_ALLOWED_USERS to a comma-separated list of Slack member IDs",
            "mention user allow list disabled; every member of the allowed channels can invoke the agent"
        );
    } else {
        // Unlike a channel drop, a denied user gets no ephemeral hint, so a
        // mention that never comes back looks the same as an outage. Saying so
        // here is what turns such a report into a config answer.
        let mut users: Vec<&str> = cfg.chat_allowed_users.iter().map(String::as_str).collect();
        users.sort_unstable();
        info!(
            allowed_user_count = users.len(),
            allowed_users = users.join(","),
            "mention user allow list enforced; a Slack user outside the list gets no reply and no hint"
        );
    }

    match tokio::time::timeout(Duration::from_secs(30), slack_client.auth_test()).await {
        Ok(Ok(id)) => {
            gateway.set_bot_user_id(&id);
            info!(bot_user_id = id, "resolved bot identity");
        }
        Ok(Err(err)) => {
            warn!(error = %err, "failed to resolve the bot user id; the mention loop guard falls back to bot_id alone");
        }
        Err(_) => warn!("auth.test timed out; the mention loop guard falls back to bot_id alone"),
    }

    let socket =
        slack::socket::Client::new(&cfg.slack_api_url, &cfg.slack_app_token, metrics.clone());
    let handler: Arc<dyn slack::socket::Handler> = Arc::new(gateway.clone());
    tokio::spawn(async move { socket.run(handler, shutdown).await });
}

fn severity_list(cfg: &Config) -> String {
    if cfg.analyze_severities.is_empty() {
        return "all".to_string();
    }
    let mut items: Vec<&str> = cfg.analyze_severities.iter().map(String::as_str).collect();
    items.sort_unstable();
    items.join(",")
}

fn init_tracing(level: &str, format: &str) {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(level));
    let registry = tracing_subscriber::registry().with(filter);
    if format.eq_ignore_ascii_case("text") {
        registry.with(fmt::layer().with_target(false)).init();
    } else {
        registry
            .with(fmt::layer().json().flatten_event(true))
            .init();
    }
}

/// Resolves on SIGINT or SIGTERM.
async fn wait_for_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        let mut term = signal(SignalKind::terminate()).expect("install SIGTERM handler");
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            _ = term.recv() => {}
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}
