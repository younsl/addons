//! Command `forklift` is a lightweight, Kubernetes-native artifact repository.
//!

mod bridge;
mod coverage_schedule;
mod flags;
mod ha;
mod hooks;
mod leadership;
mod storage_setup;

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::Duration;

use anyhow::Context as _;
use axum::Router;
use axum::routing::get;
use chrono::Utc;
use forklift::{
    api, audit, auth, cluster, config, coverage, insights, license, memlimit, meta, metrics,
    notify, openapi, outbound, replication, repo, server, storage, version, vuln, webui,
};
use prometheus::{GaugeVec, IntCounter, Opts, Registry};
use tokio_util::sync::CancellationToken;

use crate::bridge::FnGauge;
use crate::flags::{parse_flags, usage};
use crate::leadership::{Leadership, apply_declarative_state};
use crate::storage_setup::{open_blob_store, wire_storage_backend};

/// Event target for the submodules, so their lines keep the `main` log component.
const LOG_TARGET: &str = "forklift";

/// Bounds the one-time, DB-mutating startup bootstrap so a stuck step can never
/// hang boot indefinitely while still being immune to a shutdown signal
/// arriving mid-startup.
const INIT_TIMEOUT: Duration = Duration::from_secs(90);

/// Bounds the metadata snapshot flush on demotion and shutdown. It has to fit
/// inside the pod's termination grace period, and a snapshot upload is a
/// `VACUUM INTO` plus a single PUT.
const FINAL_SYNC_TIMEOUT: Duration = Duration::from_secs(30);

fn main() -> std::process::ExitCode {
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
    let mut cfg = match config::Config::load() {
        Ok(cfg) => cfg,
        Err(e) => return fatal(&e.to_string()),
    };
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.first().map(String::as_str) == Some("migrate-storage") {
        return migrate_storage(&cfg, &args[1..]);
    }
    match parse_flags(&mut cfg) {
        Ok(true) => {
            println!("forklift {}", version::string());
            return std::process::ExitCode::SUCCESS;
        }
        Ok(false) => {}
        Err(e) => {
            eprintln!("{e}");
            eprintln!("{}", usage());
            return std::process::ExitCode::from(2);
        }
    }
    if let Err(e) = cfg.validate() {
        return fatal(&e.to_string());
    }

    let runtime = match tokio::runtime::Runtime::new() {
        Ok(runtime) => runtime,
        Err(e) => return fatal(&e.to_string()),
    };
    match runtime.block_on(run(Arc::new(cfg), CancellationToken::new())) {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(e) => fatal(&format!("{e:#}")),
    }
}

/// Installs the extra CA bundle before any outbound client is built. The proxy
/// is logged as set or not, never by URL, which may carry credentials.
fn init_outbound(cfg: &config::Config) -> anyhow::Result<()> {
    let extra_ca_certs = match outbound::ExtraRoots::load(&cfg.extra_ca_file)? {
        Some(roots) => {
            let n = roots.len();
            outbound::install(roots)?;
            n
        }
        None => 0,
    };
    tracing::info!(
        extra_ca_certs,
        proxy = outbound::proxy_configured(),
        "outbound http configured"
    );
    Ok(())
}

fn migrate_storage(cfg: &config::Config, args: &[String]) -> std::process::ExitCode {
    use forklift::migrate::cli;
    let args = match cli::parse_args(args) {
        Ok(args) => args,
        Err(e) => {
            if !e.is_empty() {
                eprintln!("{e}");
            }
            eprintln!("{}", cli::USAGE);
            return std::process::ExitCode::from(2);
        }
    };
    if cfg.storage.backend != "s3" {
        return fatal(
            "migrate-storage copies between object stores; the source needs FORKLIFT_STORAGE_BACKEND=s3",
        );
    }
    server::logging::init_logging(&cfg.log_level, &cfg.log_format);
    tracing::info!(
        version = %version::string(),
        rust = %version::rust(),
        staging = %args.staging.display(),
        "starting forklift migrate-storage"
    );
    if let Err(e) = init_outbound(cfg) {
        return fatal(&format!("{e:#}"));
    }
    let runtime = match tokio::runtime::Runtime::new() {
        Ok(runtime) => runtime,
        Err(e) => return fatal(&e.to_string()),
    };
    let result = runtime.block_on(async {
        if args.interactive {
            let mut console = cli::Terminal {
                input: std::io::stdin().lock(),
                output: std::io::stdout(),
            };
            cli::run_interactive(&mut console, &cfg.storage.s3, &args).await
        } else {
            cli::run(&cfg.storage.s3, &args).await.map(Some)
        }
    });
    match result {
        Ok(Some(report)) => {
            if !args.interactive {
                eprintln!(
                    "{}",
                    forklift::migrate::preflight::render(&report.preflight)
                );
                if !report.postflight.is_empty() {
                    eprintln!(
                        "{}",
                        forklift::migrate::preflight::render(&report.postflight)
                    );
                }
            }
            println!("{}", serde_json::to_string(&report).unwrap_or_default());
            std::process::ExitCode::SUCCESS
        }
        Ok(None) => {
            eprintln!("migration cancelled");
            std::process::ExitCode::from(1)
        }
        Err(e) => fatal(&e),
    }
}

fn fatal(msg: &str) -> std::process::ExitCode {
    eprintln!("fatal: {msg}");
    std::process::ExitCode::FAILURE
}

/// Wires and serves the whole application. `cancel` governs lifetime:
/// cancelling it (or a SIGINT/SIGTERM) triggers graceful shutdown, so tests can
/// drive a full start/stop without a signal.
async fn run(cfg: Arc<config::Config>, cancel: CancellationToken) -> anyhow::Result<()> {
    server::logging::init_logging(&cfg.log_level, &cfg.log_format);
    tracing::info!(
        version = %version::string(),
        rust = %version::rust(),
        data_dir = %cfg.data_dir,
        "starting forklift"
    );
    init_outbound(&cfg)?;

    // Keep the GC ahead of the container memory limit so request bursts degrade
    // into extra allocator work instead of an OOMKill.
    memlimit::apply();

    tokio::fs::create_dir_all(&cfg.data_dir)
        .await
        .context("create data dir")?;

    let shutdown = CancellationToken::new();
    tokio::spawn({
        let shutdown = shutdown.clone();
        let cancel = cancel.clone();
        async move {
            tokio::select! {
                () = server::signal::shutdown_signal() => {}
                () = cancel.cancelled() => {}
            }
            shutdown.cancel();
        }
    });

    // One-time, DB-mutating bootstrap (schema migrations, admin seed, RBAC reconcile, default
    // repos) is a startup-critical unit: a shutdown signal arriving mid-startup must not abort
    // it and leave the store half-seeded. Nothing below selects on `shutdown` for those steps,
    // so they are already detached from cancellation; the timeout keeps a stuck step from
    // hanging boot forever.
    let data_dir = PathBuf::from(&cfg.data_dir);
    let store = Arc::new(
        tokio::time::timeout(
            INIT_TIMEOUT,
            meta::Store::open(data_dir.join("forklift.db")),
        )
        .await
        .context("open metadata store: timed out")?
        .context("open metadata store")?,
    );

    let reg = Registry::new();
    register_core_metrics(&reg, &store)?;

    // Upstream reachability (forklift_upstream_up), probed in the background on
    // every pod; see UpstreamProber for why it is not leader-gated.
    let prober =
        metrics::UpstreamProber::new(Arc::clone(&store) as Arc<dyn metrics::RepoLister>, &reg);
    tokio::spawn(prober.run(shutdown.clone()));

    let backend = open_blob_store(&cfg, &store, &data_dir, &reg, &shutdown).await?;
    let blobs = Arc::clone(&backend.blobs);
    let meta_sync = backend.meta_sync.clone();

    // Auth: optional Keycloak OIDC plus local users and PATs.
    let oidc = init_oidc(&cfg).await;
    let authz = auth::Service::new(
        Arc::clone(&store),
        auth::Options {
            session_secret: cfg.auth.session_secret.clone().into_bytes(),
            session_ttl: cfg.auth.session_ttl,
            anonymous_read: cfg.auth.anonymous_read,
            oidc: oidc.clone(),
            default_role: cfg.auth.rbac.default_role.clone(),
            bootstrap_admin_user: cfg.auth.bootstrap_admin_user.clone(),
        },
    );
    // The first declarative pass happens here, before anything else, so a
    // malformed policy or an unreachable database fails the process
    // immediately instead of surfacing later as a half-configured instance.
    tokio::time::timeout(INIT_TIMEOUT, apply_declarative_state(&cfg, &store, &authz))
        .await
        .context("apply declarative state: timed out")??;

    // Audit recorder: None (no-op) when disabled. Closed on shutdown so buffered
    // events flush before the store closes.
    let recorder = cfg
        .audit
        .enabled
        .then(|| audit::Recorder::new(Arc::clone(&store), &reg));

    let engine = repo::Engine::new(
        Arc::clone(&store),
        Arc::clone(&blobs) as Arc<dyn storage::BlobStore>,
        &reg,
    );
    let uploader = repo::Uploader::new(Arc::clone(&engine), cfg.upload.clone());
    uploader.set_async_durability(cfg.storage.backend == "s3" || cfg.replication.enabled);
    let manager = repo::Manager::new(
        Arc::clone(&engine),
        Arc::clone(&store),
        Some(Arc::clone(&authz)),
        recorder.clone(),
        Some(&reg),
    );
    configure_manager(&cfg, &manager, &uploader, &data_dir);

    // Outbound approval alarms: when a package is quarantined pending approval,
    // notify the receivers the repository selected (resolved against the enabled
    // receivers managed in the admin console). Runs off the serving path.
    let notifier = Arc::new(notify::Notifier::new(cfg.notify.webhook_timeout));
    notifier.set_external_url(&cfg.external_url);
    notifier.set_batch_window(cfg.notify.batch_window);
    notifier.set_clean_checker(hooks::clean_checker(&store));
    manager.set_approval_notifier(Some(Arc::new(hooks::approval_notifier(&store, &notifier))));
    notifier.set_delivery_recorder(hooks::delivery_recorder(&store));

    // Pending approvals, computed on scrape (one indexed COUNT). Needs no leader
    // gating and stays accurate on standbys after a snapshot swap.
    reg.register(Box::new(FnGauge::new(
        "forklift_approval_pending",
        "Package approval requests currently pending.",
        hooks::approval_pending(&store),
    )))
    .context("register forklift_approval_pending")?;

    let mut srv = server::Server::new(Arc::clone(&cfg), Arc::clone(&store), &reg);
    let ready = srv.ready_flag();
    let api_handler = api::Handler::new(
        Arc::clone(&store),
        Some(Arc::clone(&authz)),
        recorder.clone(),
    );
    api_handler.set_upload_enabled(cfg.upload.enabled);
    api_handler.set_uploader(Arc::clone(&uploader), &cfg.external_url);
    api_handler.set_repo_manager(Arc::clone(&manager));
    // Back the receiver test and repository sample/preview endpoints.
    api_handler.set_notifier(Arc::clone(&notifier));

    // Forklift coverage: walk the configured GitLab instance and report how many
    // projects actually build through this forklift. The scanner is constructed
    // whether or not GitLab is configured, so the console can explain what is
    // missing rather than 404ing; without a URL and token it refuses to scan.
    let coverage_scanner = coverage::Scanner::new(coverage::ScannerOptions {
        store: Arc::clone(&store) as Arc<dyn coverage::Store>,
        enabled: cfg.coverage.enabled,
        gitlab_url: cfg.coverage.gitlab_url.clone(),
        gitlab_token: cfg.coverage.gitlab_token.clone(),
        forklift_host: cfg.external_url.clone(),
    });
    if let Err(e) = coverage_scanner.load().await {
        tracing::warn!(err = %e, "coverage: loading stored state failed");
    }
    api_handler.set_coverage(Arc::clone(&coverage_scanner));

    // Coverage is exported on scrape from the scanner's in-memory picture, so it
    // costs nothing per scrape and needs no leader gating: a standby reports the
    // result it restored at boot. The metric worth alerting on is the scan
    // timestamp, since a scan that stops running is otherwise invisible.
    reg.register(Box::new(metrics::CoverageCollector::new(
        hooks::coverage_stats(&coverage_scanner),
    )))
    .context("register coverage collector")?;
    log_coverage_status(&cfg, &coverage_scanner);

    // Public OIDC login endpoints (no auth middleware required).
    if oidc.is_some() {
        srv.merge(
            Router::new()
                .route("/auth/login", get(auth::handle_login))
                .route("/auth/callback", get(auth::handle_callback))
                .with_state(Arc::clone(&authz)),
        );
    }

    // OpenAPI spec and Scalar docs UI (public).
    srv.merge(openapi::routes());

    // Application routes carry the auth middleware so handlers see the principal.
    let app = Router::new().nest("/api/v1", api::routes(Arc::clone(&api_handler)));
    let app = manager.register(app);
    srv.merge(app.layer(axum::middleware::from_fn_with_state(
        Arc::clone(&authz),
        auth::middleware,
    )));

    let (leader_gauge, leader_transitions) = register_leader_metrics(&reg)?;

    // leader_state mirrors leader_gauge for the admin HA status API (gauges are
    // write-only from here). Single-instance deployments set it true on start.
    let leader_state = Arc::new(AtomicBool::new(false));

    // The manual coverage scan starts detached from whatever asked for it, and
    // reports whether it was accepted. A full crawl takes minutes, so it must
    // outlive the HTTP request that triggered it; it is bound to the process
    // token instead, which cancels it on shutdown. Leadership is required
    // because a scan writes the result, the history row and the exclusions, and
    // SQLite has exactly one writer.
    //
    // No report follows a manual scan: somebody watching the console asked for
    // the number, not for everyone on the receiver to be told about it. Sending
    // the report is its own button on the settings page, which sends what the
    // last completed scan measured.
    api_handler.set_coverage_scan(hooks::leader_trigger(
        &leader_state,
        &shutdown,
        &coverage_scanner,
        coverage::Scanner::scanning,
        |scanner: Arc<coverage::Scanner>, triggered_by: String| async move {
            scanner.scan(&triggered_by).await
        },
        "coverage: scan failed",
    ));

    // Security insights run on the leader only (a run writes the report) and
    // are served from the stored report by every instance. A manual refresh is
    // detached from the request like a coverage scan.
    let insight_analyzer = cfg.insights.enabled.then(|| {
        insights::Analyzer::new(
            Arc::clone(&store),
            Some(Arc::clone(&coverage_scanner)),
            cfg.insights.interval,
            cfg.auth.rbac.default_role.clone(),
        )
    });
    if let Some(analyzer) = &insight_analyzer {
        api_handler.set_insights(
            Arc::clone(analyzer),
            hooks::leader_trigger(
                &leader_state,
                &shutdown,
                analyzer,
                insights::Analyzer::running,
                |analyzer: Arc<insights::Analyzer>, triggered_by: String| async move {
                    analyzer.run_once(&triggered_by).await
                },
                "security insights failed",
            ),
        );
    }

    let mut elector: Option<Arc<cluster::Elector>> = None;
    if cfg.ha.enabled {
        elector = Some(cluster::Elector::new(cfg.ha.clone()).context("init leader election")?);
    }

    api_handler.set_ha_status(ha::ha_status_fn(
        Arc::clone(&cfg),
        elector.clone(),
        Arc::clone(&leader_state),
        Utc::now(),
    ));

    // Manual failover for the management console: ask this instance to release
    // leadership so a standby takes over. Only wired in HA mode; single-instance
    // has no peer to fail over to.
    if let Some(elector) = &elector {
        api_handler.set_ha_step_down(Arc::new({
            let elector = Arc::clone(elector);
            move || elector.step_down()
        }));
    }

    wire_storage_backend(&cfg, &api_handler, &backend, &reg, &shutdown);

    // PV-based replication: the leader serves token-gated snapshot/blob
    // endpoints; the standby pulls them onto its own volume and promotes that
    // copy when it wins the election. The mount sits outside the auth middleware
    // group because it carries its own bearer-token check.
    let mut replicator: Option<Arc<replication::Replicator>> = None;
    if cfg.replication.enabled {
        let source = replication::Source::new(
            Arc::clone(&store),
            Arc::clone(&blobs) as Arc<dyn storage::BlobStore>,
            &cfg.replication.token,
            data_dir.clone(),
        );
        srv.merge(Router::new().nest("/internal/replication", source.routes()));

        let mut resolver = replication::static_leader_url(&cfg.replication.leader_url);
        if cfg.replication.leader_url.is_empty() {
            let elector = elector
                .clone()
                .context("replication without a leader URL needs leader election")?;
            resolver = replication::lease_leader_url(
                elector,
                &cfg.ha.identity,
                &cfg.replication.peer_service,
                cfg.replication.peer_port,
            );
        }
        let r = replication::Replicator::new(replication::Options {
            store: Arc::clone(&store),
            blobs: Arc::clone(&blobs) as Arc<dyn storage::WalkableStore>,
            data_dir: data_dir.clone(),
            token: cfg.replication.token.clone(),
            interval: cfg.replication.interval,
            leader_url: resolver,
            registry: Some(reg.clone()),
        });
        tokio::spawn(Arc::clone(&r).run(shutdown.clone()));
        replicator = Some(r);

        // With per-pod volumes a StatefulSet rollout waits on pod readiness, so
        // readiness cannot encode leadership (the standby would block rollouts
        // forever). Every pod is Ready; the main Service instead selects the
        // forklift.io/role=leader pod label patched on (de)promotion.
        srv.set_ready(true);
    }

    // label_routing keeps every replica Ready and routes the Service to the
    // leader via the forklift.io/role label, instead of gating readiness on
    // leadership. Used by replication (per-pod volumes can't gate readiness on
    // leadership) and by the s3 backend in HA, where both pods must stay Ready
    // while a single writer is enforced by leader routing plus S3 fencing.
    let label_routing = cfg.replication.enabled || (meta_sync.is_some() && cfg.ha.enabled);
    if meta_sync.is_some() && cfg.ha.enabled {
        // s3 HA: become Ready immediately; the leader label routes traffic.
        srv.set_ready(true);
    }

    // The embedded React SPA serves the UI and handles client-side routing for
    // any path not matched above.
    *srv.router_mut() = std::mem::take(srv.router_mut()).fallback(webui::handler);

    let leadership = Arc::new(Leadership {
        cfg: Arc::clone(&cfg),
        store: Arc::clone(&store),
        authz: Arc::clone(&authz),
        engine: Arc::clone(&engine),
        manager: Arc::clone(&manager),
        recorder: recorder.clone(),
        coverage: Arc::clone(&coverage_scanner),
        insights: insight_analyzer.clone(),
        notifier: Arc::clone(&notifier),
        meta_sync: meta_sync.clone(),
        replicator: replicator.clone(),
        elector: elector.clone(),
        ready: ready.clone(),
        leader_gauge,
        leader_transitions,
        leader_state: Arc::clone(&leader_state),
        label_routing,
    });

    if let Some(elector) = elector.clone() {
        let start = {
            let leadership = Arc::clone(&leadership);
            move |lead_cancel: CancellationToken| {
                tokio::spawn(Arc::clone(&leadership).start_leading(lead_cancel));
            }
        };
        let stop = {
            let leadership = Arc::clone(&leadership);
            move || {
                tokio::spawn(Arc::clone(&leadership).stop_leading());
            }
        };
        tokio::spawn(elector.run(shutdown.clone(), start, stop));
    } else {
        Arc::clone(&leadership)
            .start_leading(shutdown.clone())
            .await;
    }

    let result = srv.run(shutdown.clone(), reg.clone()).await;

    // Flush a final metadata snapshot on the way out. The periodic loop stops
    // with the cancelled token, so without this the writes made since the last
    // cycle would only exist in this pod's local database -- which in s3 mode is
    // an emptyDir that does not survive the restart.
    if let Some(sync) = &meta_sync {
        let _ = tokio::time::timeout(FINAL_SYNC_TIMEOUT, sync.final_sync()).await;
    }
    if let Some(recorder) = &recorder {
        recorder.close().await;
    }
    store.close();
    result
}

/// Registers the process, build, inventory and connection-pool metrics, none
/// of which depend on anything wired later.
fn register_core_metrics(reg: &Registry, store: &Arc<meta::Store>) -> anyhow::Result<()> {
    #[cfg(target_os = "linux")]
    reg.register(Box::new(
        prometheus::process_collector::ProcessCollector::for_self(),
    ))
    .context("register process collector")?;

    // Build metadata, exposed as a constant gauge=1 (standard exporter pattern).
    let build_info = GaugeVec::new(
        Opts::new("build_info", "Build metadata; the value is always 1.").namespace("forklift"),
        &["version", "commit", "rust_version"],
    )
    .context("build forklift_build_info")?;
    build_info
        .with_label_values(&[version::VERSION, version::COMMIT, version::rust()])
        .set(1.0);
    reg.register(Box::new(build_info))
        .context("register forklift_build_info")?;

    // Repository inventory and physical storage usage, computed per scrape.
    reg.register(Box::new(metrics::StorageCollector::new(
        Arc::clone(store) as Arc<dyn metrics::Reader>
    )))
    .context("register storage collector")?;

    // SQLite connection-pool saturation. Writes share one connection, so waiting
    // for it (not CPU or query volume) is what an overloaded instance looks like,
    // and what a saturation alert should watch.
    reg.register(Box::new(metrics::DbPoolCollector::new(
        Arc::clone(store) as Arc<dyn metrics::PoolStatser>
    )))
    .context("register db pool collector")?;
    Ok(())
}

/// Builds the OIDC provider when enabled. A provider that fails to initialize
/// is logged and skipped so local users and PATs still work.
async fn init_oidc(cfg: &config::Config) -> Option<Arc<auth::OidcProvider>> {
    if !cfg.auth.oidc.enabled {
        return None;
    }
    match auth::OidcProvider::new(auth::OidcParams {
        issuer_url: cfg.auth.oidc.issuer_url.clone(),
        client_id: cfg.auth.oidc.client_id.clone(),
        client_secret: cfg.auth.oidc.client_secret.clone(),
        redirect_url: cfg.auth.oidc.redirect_url.clone(),
        username_claim: cfg.auth.oidc.username_claim.clone(),
        groups_claim: cfg.auth.oidc.groups_claim.clone(),
    })
    .await
    {
        Ok(provider) => Some(provider),
        Err(e) => {
            tracing::error!(err = %e, "OIDC init failed; continuing without OIDC login");
            None
        }
    }
}

/// Applies the repository manager's configuration: the uploader, the external
/// URL, the OCI limits and the optional vulnerability and license backends.
fn configure_manager(
    cfg: &config::Config,
    manager: &Arc<repo::Manager>,
    uploader: &Arc<repo::Uploader>,
    data_dir: &std::path::Path,
) {
    manager.set_uploader(Some(Arc::clone(uploader)));
    manager.set_external_url(&cfg.external_url);
    // OCI push sessions accumulate under the data directory so any replica can
    // continue a session another one started (RWX volume or S3 backend).
    manager.set_oci_upload_dir(&data_dir.join("oci-uploads").to_string_lossy());
    manager.set_oci_max_manifest_bytes(cfg.oci.max_manifest_bytes);
    manager.set_oci_max_blob_bytes(cfg.oci.max_blob_bytes);
    if !cfg.vuln.osv_url.is_empty() {
        manager.set_vuln_scanner(Some(Arc::new(vuln::Osv::new(&cfg.vuln.osv_url, None))));
        tracing::info!(osv_url = %cfg.vuln.osv_url, "vulnerability scanning enabled");
    }
    if !cfg.license.deps_dev_url.is_empty() {
        manager.set_license_resolver(Some(Arc::new(license::DepsDev::new(
            &cfg.license.deps_dev_url,
            None,
        ))));
        tracing::info!(deps_dev_url = %cfg.license.deps_dev_url, "license resolution enabled");
    }
}

/// Says at boot whether coverage scanning will run, and why not when it was
/// asked for or looks half-configured.
fn log_coverage_status(cfg: &config::Config, scanner: &coverage::Scanner) {
    if scanner.enabled() {
        let host = scanner.match_host();
        tracing::info!(
            gitlab_url = %cfg.coverage.gitlab_url,
            forklift_host = %host,
            "coverage scanning enabled"
        );
        // A build reaches forklift at an external domain, so a host that is not
        // one cannot be what any repository references. Coverage would report
        // zero with nothing to explain it, which is worth saying out loud rather
        // than leaving as a dashboard nobody can account for.
        let bare = host.split(':').next().unwrap_or_default();
        if !host.is_empty() && !coverage::is_external_domain(bare) {
            tracing::warn!(
                forklift_host = %host,
                "coverage: the external URL is not an external domain, so no project is likely to reference it"
            );
        }
    } else if cfg.coverage.enabled {
        // Asked for but unusable, which is a misconfiguration rather than a
        // choice, so it is worth a warning instead of silence.
        tracing::warn!(
            "coverage scanning is enabled but has no GitLab connection; set the URL and token"
        );
    } else if scanner.credentials_present() {
        tracing::info!(
            "coverage scanning is off; set FORKLIFT_COVERAGE_ENABLED=true to turn it on"
        );
    }
}

/// Registers the leadership gauge and transition counter.
fn register_leader_metrics(reg: &Registry) -> anyhow::Result<(prometheus::Gauge, IntCounter)> {
    // leader_gauge reports whether this instance currently holds leadership.
    // Single-instance deployments are always leader; in HA exactly one pod is 1.
    let leader_gauge = prometheus::Gauge::with_opts(
        Opts::new(
            "leader",
            "1 if this instance currently holds leadership, else 0.",
        )
        .namespace("forklift"),
    )
    .context("build forklift_leader")?;
    reg.register(Box::new(leader_gauge.clone()))
        .context("register forklift_leader")?;

    // leader_transitions counts leadership acquisitions by this instance, so a
    // flapping Lease shows up as a rising sum across pods where the leader gauge
    // alone only shows the current holder. Single-instance deployments record
    // exactly one transition at startup.
    let leader_transitions = IntCounter::with_opts(
        Opts::new(
            "leader_transitions_total",
            "Times this instance acquired leadership.",
        )
        .namespace("forklift"),
    )
    .context("build forklift_leader_transitions_total")?;
    reg.register(Box::new(leader_transitions.clone()))
        .context("register forklift_leader_transitions_total")?;
    Ok((leader_gauge, leader_transitions))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Boots the whole application on ephemeral ports with the filesystem
    /// backend and no external scanners, then cancels the token to confirm a
    /// clean graceful shutdown. It exercises the full `run` wiring: storage,
    /// auth bootstrap, RBAC, repo seeding, audit, engine/manager/uploader,
    /// notifier, HTTP server, and the leader-scoped worker tasks.
    #[tokio::test(flavor = "multi_thread")]
    async fn run_starts_and_shuts_down() {
        let dir = tempfile::tempdir().expect("temp dir");
        {
            use config::test_env::set;
            set("FORKLIFT_DATA_DIR", dir.path().to_string_lossy());
            set("FORKLIFT_HTTP_ADDR", "127.0.0.1:0");
            set("FORKLIFT_METRICS_ADDR", "127.0.0.1:0");
            set("FORKLIFT_PPROF_ADDR", "127.0.0.1:0");
            set(
                "FORKLIFT_SESSION_SECRET",
                "test-secret-test-secret-test-secret-0123",
            );
            set("FORKLIFT_BOOTSTRAP_ADMIN_PASSWORD", "admin-pass-123456");
            // Disable the vuln scanner and license resolver (no network).
            set("FORKLIFT_OSV_URL", "");
            set("FORKLIFT_DEPSDEV_URL", "");
            set("FORKLIFT_SHUTDOWN_TIMEOUT", "2s");
        }

        let cfg = config::Config::load().expect("config load");
        cfg.validate().expect("config validate");

        let cancel = CancellationToken::new();
        let done = tokio::spawn(run(Arc::new(cfg), cancel.clone()));

        // Give startup time to bind and spin up leader workers, then shut down.
        tokio::time::sleep(Duration::from_millis(300)).await;
        cancel.cancel();

        match tokio::time::timeout(Duration::from_secs(10), done).await {
            Ok(joined) => joined
                .expect("run task")
                .unwrap_or_else(|e| panic!("run returned error: {e:#}")),
            Err(e) => panic!("run did not shut down after context cancel: {e}"),
        }
    }
}
