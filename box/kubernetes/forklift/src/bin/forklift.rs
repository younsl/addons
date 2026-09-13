//! Command `forklift` is a lightweight, Kubernetes-native artifact repository.
//!

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use anyhow::Context as _;
use axum::Router;
use axum::routing::get;
use chrono::Utc;
use forklift::{
    api, audit, auth, cluster, config, coverage, license, memlimit, meta, metrics, notify,
    objstore, openapi, replication, repo, server, storage, version, vuln, webui,
};
use prometheus::core::{Collector, Desc};
use prometheus::proto::{Gauge, Metric, MetricFamily, MetricType};
use prometheus::{GaugeVec, IntCounter, Opts, Registry};
use tokio_util::sync::CancellationToken;

/// Bounds the one-time, DB-mutating startup bootstrap so a stuck step can never
/// hang boot indefinitely while still being immune to a shutdown signal
/// arriving mid-startup.
const INIT_TIMEOUT: Duration = Duration::from_secs(90);

/// Bounds the metadata snapshot flush on demotion and shutdown. It has to fit
/// inside the pod's termination grace period, and a snapshot upload is a
/// `VACUUM INTO` plus a single PUT.
const FINAL_SYNC_TIMEOUT: Duration = Duration::from_secs(30);

fn main() -> std::process::ExitCode {
    let mut cfg = match config::Config::load() {
        Ok(cfg) => cfg,
        Err(e) => return fatal(&e.to_string()),
    };
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
                _ = signals() => {}
                _ = cancel.cancelled() => {}
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
        Arc::clone(&store) as Arc<dyn metrics::Reader>
    )))
    .context("register storage collector")?;

    // SQLite connection-pool saturation. Writes share one connection, so waiting
    // for it (not CPU or query volume) is what an overloaded instance looks like,
    // and what a saturation alert should watch.
    reg.register(Box::new(metrics::DbPoolCollector::new(
        Arc::clone(&store) as Arc<dyn metrics::PoolStatser>
    )))
    .context("register db pool collector")?;

    // Upstream reachability (forklift_upstream_up), probed in the background on
    // every pod; see UpstreamProber for why it is not leader-gated.
    let prober =
        metrics::UpstreamProber::new(Arc::clone(&store) as Arc<dyn metrics::RepoLister>, &reg);
    tokio::spawn(prober.run(shutdown.clone()));

    // Blob store backend: local filesystem (default) or S3. In s3 mode blobs are
    // shared directly in the bucket and the metadata database is snapshotted to
    // S3 by meta_sync, so the deployment needs no EBS/RWX volume.
    let mut meta_sync: Option<Arc<objstore::MetaSync>> = None;
    let blobs: Arc<storage::InstrumentedStore> = if cfg.storage.backend == "s3" {
        let s3cfg = to_s3_config(&cfg.storage.s3);
        let s3blobs = storage::S3BlobStore::new(&s3cfg, data_dir.join("blob-tmp"))
            .await
            .context("open s3 blob store")?;
        let client = s3blobs.client();
        let blobs = Arc::new(storage::instrument(Arc::new(s3blobs), "s3", &reg));
        let sync = objstore::MetaSync::new(objstore::MetaOptions {
            store: Arc::clone(&store),
            api: Arc::new(objstore::S3Api::new(client)),
            bucket: cfg.storage.s3.bucket.clone(),
            key: meta_key(&cfg.storage.s3.prefix),
            data_dir: data_dir.clone(),
            interval: cfg.storage.meta_sync_interval,
            registry: Some(reg.clone()),
        });
        // Restore the latest snapshot before bootstrap/seed so they see existing
        // data. The live SQLite file lives on an ephemeral volume that loses its
        // contents on restart; an empty bucket is a clean no-op.
        sync.restore_on_boot()
            .await
            .context("restore metadata from s3")?;
        tokio::spawn(Arc::clone(&sync).run(shutdown.clone()));
        meta_sync = Some(sync);
        tracing::info!(
            bucket = %cfg.storage.s3.bucket,
            prefix = %cfg.storage.s3.prefix,
            "storage backend: s3"
        );
        blobs
    } else {
        let fsblobs = storage::FsStore::new(&cfg.data_dir).context("open blob store")?;
        Arc::new(storage::instrument(Arc::new(fsblobs), "fs", &reg))
    };

    // Auth: optional Keycloak OIDC plus local users and PATs.
    let mut oidc: Option<Arc<auth::OidcProvider>> = None;
    if cfg.auth.oidc.enabled {
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
            Ok(provider) => oidc = Some(provider),
            Err(e) => tracing::error!(err = %e, "OIDC init failed; continuing without OIDC login"),
        }
    }
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
    manager.set_uploader(Some(Arc::clone(&uploader)));
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

    // Outbound approval alarms: when a package is quarantined pending approval,
    // notify the receivers the repository selected (resolved against the enabled
    // receivers managed in the admin console). Runs off the serving path.
    let notifier = Arc::new(notify::Notifier::new(cfg.notify.webhook_timeout));
    notifier.set_external_url(&cfg.external_url);
    notifier.set_batch_window(cfg.notify.batch_window);
    // Clean/Dirty breakdown for grouped alarms: returns a coordinate's stored max
    // severity ("none" = Clean), the top CVSS score at that severity, and whether
    // it was scanned. Unscanned coordinates count as Dirty with no CVE.
    notifier.set_clean_checker({
        let store = Arc::clone(&store);
        Arc::new(move |repo_id, pkg, version| {
            let store = Arc::clone(&store);
            let (pkg, version) = (pkg.to_string(), version.to_string());
            let empty = || (String::new(), String::new(), String::new(), false);
            block_on(async move {
                let query = async {
                    let repository = store.get_repository(repo_id).await.ok()?;
                    let eco = repo::osv_ecosystem(&repository.format);
                    if eco.is_empty() {
                        return None;
                    }
                    // Unscanned coordinates have no row.
                    let scan = store.get_vuln_scan(eco, &pkg, &version).await.ok()?;
                    // Highest-scoring advisory at the max severity carries the
                    // score and id.
                    let (mut score, mut id, mut best) = (String::new(), String::new(), 0.0f64);
                    for a in &scan.advisories {
                        if a.severity != scan.max_severity {
                            continue;
                        }
                        let f = a.score.parse::<f64>().unwrap_or(0.0);
                        if id.is_empty() || f >= best {
                            best = f;
                            score = a.score.clone();
                            id = a.id.clone();
                        }
                    }
                    Some((scan.max_severity.clone(), score, id, true))
                };
                match tokio::time::timeout(Duration::from_secs(3), query).await {
                    Ok(Some(found)) => found,
                    _ => empty(),
                }
            })
        })
    });
    manager.set_approval_notifier(Some(Arc::new({
        let store = Arc::clone(&store);
        let notifier = Arc::clone(&notifier);
        move |repo_name: String,
              repo_id: i64,
              repo_format: String,
              pkg: String,
              version: String,
              requested_by: String,
              receivers: Vec<String>| {
            if receivers.is_empty() {
                return;
            }
            let store = Arc::clone(&store);
            let notifier = Arc::clone(&notifier);
            tokio::spawn(async move {
                let work = async {
                    let all = match store.list_enabled_receivers().await {
                        Ok(all) => all,
                        Err(e) => {
                            tracing::warn!(err = %e, "notify: list receivers failed");
                            return;
                        }
                    };
                    let selected: std::collections::HashSet<&str> =
                        receivers.iter().map(String::as_str).collect();
                    let mut targets = Vec::new();
                    let mut names = Vec::new();
                    for rec in &all {
                        if selected.contains(rec.name.as_str()) {
                            targets.push(notify::Target {
                                name: rec.name.clone(),
                                url: rec.webhook_url.clone(),
                            });
                            names.push(rec.name.clone());
                        }
                    }
                    notifier.notify_approval_request(
                        &targets,
                        &repo_name,
                        repo_id,
                        &repo_format,
                        &pkg,
                        &version,
                        &requested_by,
                    );
                    // Record the alarm targets on the approval row for the
                    // queue's column.
                    if !names.is_empty()
                        && let Err(e) = store.mark_approval_notified(&repo_name, &pkg, &names).await
                    {
                        tracing::warn!(err = %e, "notify: mark approval notified failed");
                    }
                };
                let _ = tokio::time::timeout(Duration::from_secs(5), work).await;
            });
        }
    })));
    // Persist each approval alarm's actual delivery outcome (send time, result and
    // elapsed time) onto the covered approval rows, for the review detail page.
    // Invoked from the (async, possibly batched) delivery task.
    notifier.set_delivery_recorder({
        let store = Arc::clone(&store);
        Arc::new(move |pkgs, result, detail, duration_ms| {
            let store = Arc::clone(&store);
            let (result, detail) = (result.to_string(), detail.to_string());
            block_on(async move {
                let work = async {
                    for p in &pkgs {
                        if let Err(e) = store
                            .record_approval_delivery(
                                &p.repo,
                                &p.package,
                                &result,
                                &detail,
                                duration_ms,
                            )
                            .await
                        {
                            tracing::warn!(
                                repo = %p.repo,
                                package = %p.package,
                                err = %e,
                                "notify: record approval delivery failed"
                            );
                        }
                    }
                };
                let _ = tokio::time::timeout(Duration::from_secs(5), work).await;
            });
        })
    });

    // Pending approvals, computed on scrape (one indexed COUNT). Needs no leader
    // gating and stays accurate on standbys after a snapshot swap.
    reg.register(Box::new(FnGauge::new(
        "forklift_approval_pending",
        "Package approval requests currently pending.",
        {
            let store = Arc::clone(&store);
            Arc::new(move || {
                let store = Arc::clone(&store);
                block_on(async move {
                    let count = store.count_approvals("", meta::APPROVAL_PENDING);
                    match tokio::time::timeout(Duration::from_secs(2), count).await {
                        Ok(Ok(n)) => n as f64,
                        _ => 0.0,
                    }
                })
            })
        },
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
    reg.register(Box::new(metrics::CoverageCollector::new({
        let scanner = Arc::clone(&coverage_scanner);
        Arc::new(move || {
            let overview = scanner.overview();
            let stats = scanner.scan_stats();
            metrics::CoverageStats {
                enabled: overview.enabled,
                target: overview.summary.target,
                applied: overview.summary.applied,
                partial: overview.summary.partial,
                not_applied: overview.summary.not_applied,
                errored: overview.summary.errored,
                no_ci: overview.summary.skipped,
                muted: overview.summary.excluded,
                percent: overview.summary.percent,
                last_scanned_at: overview.last_scanned_at,
                last_scan_seconds: overview.last_scan_duration_ms as f64 / 1000.0,
                scanning: overview.scanning,
                last_scan_failed: !overview.last_scan_error.is_empty(),
                scans_succeeded: stats.succeeded,
                scans_failed: stats.failed,
                concurrency: stats.last_concurrency,
                peak_concurrency: stats.peak_concurrency,
            }
        })
    })))
    .context("register coverage collector")?;
    if coverage_scanner.enabled() {
        let host = coverage_scanner.match_host();
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
    } else if coverage_scanner.credentials_present() {
        tracing::info!(
            "coverage scanning is off; set FORKLIFT_COVERAGE_ENABLED=true to turn it on"
        );
    }

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

    // leader_state mirrors leader_gauge for the admin HA status API (gauges are
    // write-only from here). Single-instance deployments set it true on start.
    let leader_state = Arc::new(AtomicBool::new(false));

    // The manual coverage scan starts detached from whatever asked for it, and
    // reports whether it was accepted. A full crawl takes minutes, so it must
    // outlive the HTTP request that triggered it; it is bound to the process
    // token instead, which cancels it on shutdown. Leadership is required
    // because a scan writes the result, the history row and the exclusions, and
    // SQLite has exactly one writer.
    api_handler.set_coverage_scan(Arc::new({
        let leader_state = Arc::clone(&leader_state);
        let scanner = Arc::clone(&coverage_scanner);
        let shutdown = shutdown.clone();
        move |triggered_by: &str| {
            if !leader_state.load(Ordering::SeqCst) || scanner.scanning() {
                return false;
            }
            // No report follows a manual scan: somebody watching the console
            // asked for the number, not for everyone on the receiver to be told
            // about it. Sending the report is its own button on the settings
            // page, which sends what the last completed scan measured.
            let scanner = Arc::clone(&scanner);
            let shutdown = shutdown.clone();
            let triggered_by = triggered_by.to_string();
            tokio::spawn(async move {
                tokio::select! {
                    _ = shutdown.cancelled() => {}
                    res = scanner.scan(&triggered_by) => {
                        if let Err(e) = res {
                            tracing::warn!(triggered_by = %triggered_by, err = %e, "coverage: scan failed");
                        }
                    }
                }
            });
            true
        }
    }));

    let mut elector: Option<Arc<cluster::Elector>> = None;
    if cfg.ha.enabled {
        elector = Some(cluster::Elector::new(cfg.ha.clone()).context("init leader election")?);
    }

    // Admin-only HA status for the management console: backend/mode, this pod's
    // identity and role, the current Lease holder, the s3 fencing token, and the
    // process start time (for uptime display).
    let started_at = Utc::now();
    api_handler.set_ha_status(Arc::new({
        let cfg = Arc::clone(&cfg);
        let elector = elector.clone();
        let leader_state = Arc::clone(&leader_state);
        move || {
            let mut st = api::HAStatus {
                enabled: cfg.ha.enabled,
                mode: ha_mode(&cfg).to_string(),
                backend: cfg.storage.backend.clone(),
                identity: cfg.ha.identity.clone(),
                lease_name: cfg.ha.lease_name.clone(),
                is_leader: leader_state.load(Ordering::SeqCst),
                started_at: started_at.to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
                version: version::VERSION.to_string(),
                runtime: version::rust().to_string(),
                ..Default::default()
            };
            // Where artifacts live: the object-storage bucket/endpoint (s3) or
            // the block-storage data directory (fs).
            if cfg.storage.backend == "s3" {
                let mut bucket = cfg.storage.s3.bucket.clone();
                if !cfg.storage.s3.prefix.is_empty() {
                    bucket.push('/');
                    bucket.push_str(cfg.storage.s3.prefix.trim_start_matches('/'));
                }
                let endpoint = cfg.storage.s3.endpoint.trim_end_matches('/');
                st.storage_endpoint = if endpoint.is_empty() {
                    format!("s3://{bucket}")
                } else {
                    format!("{endpoint}/{bucket}")
                };
            } else {
                st.storage_endpoint = cfg.data_dir.clone();
            }
            let Some(elector) = &elector else {
                // Single instance is always the leader and serves itself.
                st.is_leader = true;
                st.leader = cfg.ha.identity.clone();
                st.role = cluster::ROLE_LEADER.to_string();
                return st;
            };
            st.role = if st.is_leader {
                cluster::ROLE_LEADER.to_string()
            } else {
                cluster::ROLE_STANDBY.to_string()
            };
            let elector = Arc::clone(elector);
            let s3 = cfg.storage.backend == "s3";
            let (leader, fence) = block_on(async move {
                let leader = elector.leader_identity().await.ok();
                let fence = if s3 {
                    elector.fencing_token().await.ok()
                } else {
                    None
                };
                (leader, fence)
            });
            if let Some(leader) = leader {
                st.leader = leader;
            }
            if let Some(fence) = fence {
                st.fencing_token = fence;
            }
            st
        }
    }));

    // Manual failover for the management console: ask this instance to release
    // leadership so a standby takes over. Only wired in HA mode; single-instance
    // has no peer to fail over to.
    if let Some(elector) = &elector {
        api_handler.set_ha_step_down(Arc::new({
            let elector = Arc::clone(elector);
            move || elector.step_down()
        }));
    }

    // Storage overview for the admin console: the backend descriptor plus, for a
    // MinIO endpoint, a live MinIO Admin API query. fs and AWS S3 (no endpoint)
    // expose no admin metrics, so the page then shows just the blob footprint.
    {
        let (endpoint, bucket, prefix) = if cfg.storage.backend == "s3" {
            let bucket = cfg.storage.s3.bucket.clone();
            let ep = cfg.storage.s3.endpoint.trim_end_matches('/');
            let endpoint = if ep.is_empty() {
                format!("s3://{bucket}")
            } else {
                ep.to_string()
            };
            (
                endpoint,
                bucket,
                cfg.storage.s3.prefix.trim_start_matches('/').to_string(),
            )
        } else {
            (cfg.data_dir.clone(), String::new(), String::new())
        };
        let minio: Option<api::MinIOInfoFn> =
            (cfg.storage.backend == "s3" && !cfg.storage.s3.endpoint.is_empty()).then(|| {
                let s3cfg = Arc::new(to_s3_config(&cfg.storage.s3));
                Arc::new(move || {
                    let s3cfg = Arc::clone(&s3cfg);
                    Box::pin(async move {
                        match tokio::time::timeout(
                            Duration::from_secs(5),
                            storage::minio_admin_info(&s3cfg),
                        )
                        .await
                        {
                            Ok(res) => res.map_err(|e| e.to_string()),
                            Err(_) => Err("minio admin info: timed out".to_string()),
                        }
                    }) as std::pin::Pin<Box<dyn Future<Output = _> + Send>>
                }) as api::MinIOInfoFn
            });
        api_handler.set_storage_backend(
            api::StorageBackend {
                backend: cfg.storage.backend.clone(),
                endpoint,
                bucket,
                prefix,
            },
            minio,
        );
    }

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

/// Writes everything this process owns declaratively: the bootstrap admin, the
/// chart-provided RBAC policy and the seeded default repositories. It is
/// idempotent, and it runs twice on purpose.
///
/// The first call happens at startup, before anything else, so a malformed
/// policy or an unreachable database fails the process immediately instead of
/// surfacing later as a half-configured instance. The second happens on
/// leadership acquisition, after the promotion path has swapped in the
/// authoritative database. Without that second call the work would be silently
/// discarded in the s3 and replication topologies: promotion replaces the whole
/// local database with the snapshot it fetches, so state written before it never
/// reaches the object store, and the next leader reads the same stale snapshot
/// again. That made every chart policy edit a no-op after the first boot on an
/// empty bucket.
async fn apply_declarative_state(
    cfg: &config::Config,
    store: &Arc<meta::Store>,
    authz: &Arc<auth::Service>,
) -> anyhow::Result<()> {
    authz
        .bootstrap_admin(
            &cfg.auth.bootstrap_admin_user,
            &cfg.auth.bootstrap_admin_password,
        )
        .await
        .context("bootstrap admin")?;
    // Declarative RBAC: reconcile chart-provided roles, grants, group mappings
    // and local accounts. No-op when no policy file is configured.
    auth::reconcile_rbac(
        Arc::clone(store),
        &cfg.auth.rbac.policy_file,
        &cfg.auth.rbac.accounts_dir,
    )
    .await
    .context("reconcile rbac")?;
    if cfg.seed_default_repos {
        repo::seed_defaults(store)
            .await
            .context("seed default repositories")?;
    }
    Ok(())
}

/// Everything the leadership callbacks touch.
struct Leadership {
    cfg: Arc<config::Config>,
    store: Arc<meta::Store>,
    authz: Arc<auth::Service>,
    engine: Arc<repo::Engine>,
    manager: Arc<repo::Manager>,
    recorder: Option<Arc<audit::Recorder>>,
    coverage: Arc<coverage::Scanner>,
    notifier: Arc<notify::Notifier>,
    meta_sync: Option<Arc<objstore::MetaSync>>,
    replicator: Option<Arc<replication::Replicator>>,
    elector: Option<Arc<cluster::Elector>>,
    ready: server::Ready,
    leader_gauge: prometheus::Gauge,
    leader_transitions: IntCounter,
    leader_state: Arc<AtomicBool>,
    label_routing: bool,
}

impl Leadership {
    /// The blob sweeper and audit retention are gated on leadership. In
    /// single-instance mode this process is always the leader; in HA mode a
    /// Kubernetes Lease elects exactly one active instance so SQLite has a
    /// single writer. With replication enabled, the replicated snapshot is
    /// applied before this instance takes traffic.
    async fn start_leading(self: Arc<Self>, cancel: CancellationToken) {
        let cfg = &self.cfg;
        self.leader_gauge.set(1.0);
        self.leader_transitions.inc();
        self.leader_state.store(true, Ordering::SeqCst);
        if let Some(replicator) = &self.replicator
            && let Err(e) = replicator.promote().await
        {
            tracing::error!(err = %e, "replication: promote failed; serving local data");
        }
        // In s3 mode, apply the latest metadata snapshot before serving so the
        // new leader takes traffic on current data. The fencing token (Lease
        // transition count) tags snapshot uploads so a superseded leader cannot
        // overwrite this term's metadata.
        if let Some(sync) = &self.meta_sync {
            let mut fence = 0i64;
            if let Some(elector) = &self.elector {
                match elector.fencing_token().await {
                    Ok(t) => fence = t,
                    Err(e) => {
                        tracing::warn!(err = %e, "objstore: read fencing token failed; using 0")
                    }
                }
            }
            if let Err(e) = sync.promote(fence).await {
                // Fail closed. Without the current snapshot this process cannot
                // know how far behind its local database is, and leading on
                // stale metadata is what produces dangling blob references: the
                // sweeper reclaims bytes a newer state still refers to. Skip the
                // leader label so no traffic is routed here and start no
                // background job that mutates state; another replica (or this
                // one, on the next term) can take over cleanly.
                tracing::error!(err = %e, "objstore: promote failed; refusing to lead on stale metadata");
                self.set_pod_role(cluster::ROLE_STANDBY).await;
                self.leader_gauge.set(0.0);
                self.leader_state.store(false, Ordering::SeqCst);
                return;
            }
        }
        // Re-apply the declarative state on the database this term actually
        // serves. Promotion above may have replaced it wholesale with the
        // snapshot from the object store, discarding what the startup pass
        // wrote; re-running here is what makes a chart policy edit take effect.
        // Refuse to lead if it fails, for the same reason a failed promotion
        // does: serving as leader on state we could not reconcile would publish
        // that state to every later term.
        if let Err(e) = apply_declarative_state(cfg, &self.store, &self.authz).await {
            tracing::error!(err = %format!("{e:#}"), "declarative state apply failed; refusing to lead");
            self.set_pod_role(cluster::ROLE_STANDBY).await;
            self.leader_gauge.set(0.0);
            self.leader_state.store(false, Ordering::SeqCst);
            return;
        }
        self.ready.set(true);
        self.set_pod_role(cluster::ROLE_LEADER).await;
        // A partitioned former leader may not have removed its own leader
        // label; strip it so the Service routes to this pod only.
        if self.label_routing
            && !cfg.replication.pod_name.is_empty()
            && let Some(elector) = &self.elector
            && let Err(e) = elector
                .demote_peers(&cfg.replication.pod_namespace, &cfg.replication.pod_name)
                .await
        {
            tracing::error!(err = %e, "demote peer leader labels");
        }

        tokio::spawn(Arc::clone(&self.engine).run_sweeper(
            cancel.clone(),
            Duration::from_secs(5 * 60),
            config::BLOB_GC_GRACE,
        ));
        tokio::spawn(
            Arc::clone(&self.manager).run_idle_reaper(cancel.clone(), Duration::from_secs(60 * 60)),
        );
        tokio::spawn(Arc::clone(&self.manager).run_oci_prune(
            cancel.clone(),
            cfg.oci.prune_interval,
            cfg.oci.upload_session_ttl,
        ));
        // Vulnerability scan worker pool + backfill (scans already-stored
        // artifacts) + periodic re-scanner (no-ops without a scanner). Multiple
        // workers drain the queue concurrently so freshly cached coordinates are
        // scanned promptly under burst.
        for _ in 0..cfg.vuln.workers.max(1) {
            tokio::spawn(Arc::clone(&self.manager).run_vuln_worker(cancel.clone()));
        }
        tokio::spawn(
            Arc::clone(&self.manager).run_vuln_backfill(cancel.clone(), cfg.vuln.rescan_interval),
        );
        tokio::spawn(Arc::clone(&self.manager).run_vuln_rescanner(
            cancel.clone(),
            cfg.vuln.rescan_interval,
            cfg.vuln.ttl,
        ));
        // License resolution worker pool + backfill + periodic re-resolver
        // (no-ops without a resolver).
        for _ in 0..cfg.license.workers.max(1) {
            tokio::spawn(Arc::clone(&self.manager).run_license_worker(cancel.clone()));
        }
        tokio::spawn(
            Arc::clone(&self.manager)
                .run_license_backfill(cancel.clone(), cfg.license.rescan_interval),
        );
        tokio::spawn(Arc::clone(&self.manager).run_license_rescanner(
            cancel.clone(),
            cfg.license.rescan_interval,
            cfg.license.ttl,
        ));
        if let Some(recorder) = &self.recorder
            && !cfg.audit.retention.is_zero()
        {
            tokio::spawn(Arc::clone(recorder).run_retention(
                cancel.clone(),
                Duration::from_secs(60 * 60),
                cfg.audit.retention,
            ));
        }
        if self.coverage.enabled() {
            tokio::spawn(run_coverage_schedule(
                cancel.clone(),
                Arc::clone(&self.store),
                Arc::clone(&self.coverage),
                Arc::clone(&self.notifier),
            ));
        }
    }

    async fn stop_leading(self: Arc<Self>) {
        self.leader_gauge.set(0.0);
        self.leader_state.store(false, Ordering::SeqCst);
        if let Some(sync) = &self.meta_sync {
            // Flush the writes made since the last cycle, then resume
            // downloading the new leader's snapshots.
            let _ = tokio::time::timeout(FINAL_SYNC_TIMEOUT, sync.demote()).await;
        }
        if let Some(replicator) = &self.replicator {
            replicator.demote();
        }
        if self.label_routing {
            // Stay Ready so rollouts proceed; moving the forklift.io/role=leader
            // label is what redirects traffic to the new leader.
            let _ = tokio::time::timeout(
                Duration::from_secs(10),
                self.set_pod_role(cluster::ROLE_STANDBY),
            )
            .await;
            return;
        }
        // Shared-volume HA without label routing: readiness gates the leader.
        self.ready.set(false);
    }

    async fn set_pod_role(&self, role: &str) {
        let cfg = &self.cfg;
        if !self.label_routing || cfg.replication.pod_name.is_empty() {
            return;
        }
        let Some(elector) = &self.elector else {
            return;
        };
        if let Err(e) = elector
            .set_pod_role(
                &cfg.replication.pod_namespace,
                &cfg.replication.pod_name,
                role,
            )
            .await
        {
            tracing::error!(role = %role, err = %e, "set pod role label");
        }
    }
}

/// Names the active high-availability/storage topology for the admin status
/// view.
fn ha_mode(cfg: &config::Config) -> &'static str {
    if cfg.replication.enabled {
        "replication"
    } else if cfg.storage.backend == "s3" {
        "object-storage"
    } else if cfg.ha.enabled {
        "shared-volume"
    } else {
        "single"
    }
}

fn to_s3_config(c: &config::S3Config) -> storage::S3Config {
    storage::S3Config {
        bucket: c.bucket.clone(),
        prefix: c.prefix.clone(),
        region: c.region.clone(),
        endpoint: c.endpoint.clone(),
        force_path_style: c.force_path_style,
        access_key_id: c.access_key_id.clone(),
        secret_access_key: c.secret_access_key.clone(),
    }
}

fn meta_key(prefix: &str) -> String {
    let prefix = prefix.trim_matches('/');
    if prefix.is_empty() {
        "meta/forklift.db".to_string()
    } else {
        format!("{prefix}/meta/forklift.db")
    }
}

async fn signals() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        let mut term = match signal(SignalKind::terminate()) {
            Ok(s) => s,
            Err(_) => {
                let _ = tokio::signal::ctrl_c().await;
                return;
            }
        };
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

/// Runs an async store query from a synchronous injection point (the metric
/// collector, the HA status provider, the notifier's callbacks).
///
/// The runtime is multi-threaded, so the poll is handed to `block_in_place`; a caller outside a
/// runtime (or on a current-thread one) falls back to a scratch thread so it can never deadlock
/// the caller's scheduler. It mirrors `metrics::block_on`, which is crate-private.
fn block_on<F>(fut: F) -> F::Output
where
    F: Future + Send,
    F::Output: Send,
{
    match tokio::runtime::Handle::try_current() {
        Ok(handle) if handle.runtime_flavor() == tokio::runtime::RuntimeFlavor::MultiThread => {
            tokio::task::block_in_place(|| handle.block_on(fut))
        }
        Ok(handle) => {
            std::thread::scope(|s| s.spawn(|| handle.block_on(fut)).join().expect("task"))
        }
        Err(_) => tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("scratch runtime")
            .block_on(fut),
    }
}

struct FnGauge {
    desc: Desc,
    value: Arc<dyn Fn() -> f64 + Send + Sync>,
}

impl FnGauge {
    fn new(name: &str, help: &str, value: Arc<dyn Fn() -> f64 + Send + Sync>) -> FnGauge {
        FnGauge {
            desc: Desc::new(name.to_string(), help.to_string(), vec![], HashMap::new())
                .unwrap_or_else(|e| panic!("metric descriptor {name}: {e}")),
            value,
        }
    }
}

impl Collector for FnGauge {
    fn desc(&self) -> Vec<&Desc> {
        vec![&self.desc]
    }

    fn collect(&self) -> Vec<MetricFamily> {
        let mut gauge = Gauge::default();
        gauge.set_value((self.value)());
        let mut metric = Metric::default();
        metric.set_gauge(gauge);
        let mut mf = MetricFamily::default();
        mf.set_name(self.desc.fq_name.clone());
        mf.set_help(self.desc.help.clone());
        mf.set_field_type(MetricType::GAUGE);
        mf.set_metric(vec![metric]);
        vec![mf]
    }
}

/// Parses the command line into `cfg`, reporting whether `-version` was asked
/// for. The `config::load()` values seed the defaults so any `FORKLIFT_*` env
/// still applies, while flags take precedence.
fn parse_flags(cfg: &mut config::Config) -> Result<bool, String> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut i = 0;
    while i < args.len() {
        let arg = &args[i];
        let Some(flag) = arg
            .strip_prefix("--")
            .or_else(|| arg.strip_prefix('-'))
            .filter(|f| !f.is_empty())
        else {
            return Err(format!("flag provided but not defined: {arg}"));
        };
        let (name, inline) = match flag.split_once('=') {
            Some((name, value)) => (name, Some(value.to_string())),
            None => (flag, None),
        };
        i += 1;
        if name == "version" {
            if !bool_value(inline.as_deref(), name)? {
                continue;
            }
            return Ok(true);
        }
        if name == "help" || name == "h" {
            return Err(String::new());
        }
        if name == "ui-upload-enabled" {
            cfg.upload.enabled = bool_value(inline.as_deref(), name)?;
            continue;
        }
        // The name is checked before its argument is consumed, so an unknown
        // flag is reported as undefined rather than as a missing argument.
        const VALUE_FLAGS: [&str; 15] = [
            "osv-url",
            "vuln-rescan-interval",
            "vuln-ttl",
            "vuln-workers",
            "deps-dev-url",
            "license-rescan-interval",
            "license-ttl",
            "license-workers",
            "ui-upload-max-duration",
            "ui-upload-max-concurrent",
            "ui-upload-max-concurrent-user",
            "ui-upload-max-assets",
            "ui-upload-max-file-bytes",
            "ui-upload-max-batch-bytes",
            "ui-upload-go-max-zip-bytes",
        ];
        if !VALUE_FLAGS.contains(&name) {
            return Err(format!("flag provided but not defined: -{name}"));
        }
        let value = match inline {
            Some(value) => value,
            None => {
                let Some(next) = args.get(i) else {
                    return Err(format!("flag needs an argument: -{name}"));
                };
                i += 1;
                next.clone()
            }
        };
        match name {
            "osv-url" => cfg.vuln.osv_url = value,
            "vuln-rescan-interval" => cfg.vuln.rescan_interval = duration_value(&value, name)?,
            "vuln-ttl" => cfg.vuln.ttl = duration_value(&value, name)?,
            "vuln-workers" => cfg.vuln.workers = int_value(&value, name)?,
            "deps-dev-url" => cfg.license.deps_dev_url = value,
            "license-rescan-interval" => {
                cfg.license.rescan_interval = duration_value(&value, name)?
            }
            "license-ttl" => cfg.license.ttl = duration_value(&value, name)?,
            "license-workers" => cfg.license.workers = int_value(&value, name)?,
            "ui-upload-max-duration" => cfg.upload.max_duration = duration_value(&value, name)?,
            "ui-upload-max-concurrent" => cfg.upload.max_concurrent = int_value(&value, name)?,
            "ui-upload-max-concurrent-user" => {
                cfg.upload.max_concurrent_user = int_value(&value, name)?
            }
            "ui-upload-max-assets" => cfg.upload.max_assets = int_value(&value, name)?,
            "ui-upload-max-file-bytes" => cfg.upload.max_file_bytes = bytes_value(&value, name)?,
            "ui-upload-max-batch-bytes" => cfg.upload.max_batch_bytes = bytes_value(&value, name)?,
            "ui-upload-go-max-zip-bytes" => {
                cfg.upload.go_max_zip_bytes = bytes_value(&value, name)?
            }
            // VALUE_FLAGS above is the same list, so this is unreachable.
            _ => return Err(format!("flag provided but not defined: -{name}")),
        }
    }
    Ok(false)
}

fn bool_value(value: Option<&str>, name: &str) -> Result<bool, String> {
    match value {
        None => Ok(true),
        Some("true" | "1" | "t" | "T" | "TRUE" | "True") => Ok(true),
        Some("false" | "0" | "f" | "F" | "FALSE" | "False") => Ok(false),
        Some(other) => Err(format!(
            "invalid boolean value {other:?} for -{name}: parse error"
        )),
    }
}

fn int_value(value: &str, name: &str) -> Result<i64, String> {
    value
        .parse::<i64>()
        .map_err(|e| format!("invalid value {value:?} for flag -{name}: {e}"))
}

fn duration_value(value: &str, name: &str) -> Result<Duration, String> {
    let nanos = config::parse_duration_nanos(value)
        .map_err(|e| format!("invalid value {value:?} for flag -{name}: {e}"))?;
    Ok(Duration::from_nanos(nanos.max(0) as u64))
}

fn bytes_value(value: &str, name: &str) -> Result<i64, String> {
    config::parse_byte_size(value)
        .map_err(|e| format!("invalid value {value:?} for flag -{name}: {e}"))
}

fn usage() -> String {
    [
        "Usage of forklift:",
        "  -deps-dev-url string",
        "        deps.dev API base URL for license scanning; empty disables it",
        "  -license-rescan-interval duration",
        "        how often stale license results are re-queried",
        "  -license-ttl duration",
        "        age at which a license result becomes stale",
        "  -license-workers int",
        "        number of concurrent license resolution workers draining the queue",
        "  -osv-url string",
        "        OSV API base URL for vulnerability scanning; empty disables it",
        "  -ui-upload-enabled",
        "        enable management-API and web UI artifact upload",
        "  -ui-upload-go-max-zip-bytes value",
        "        maximum Go module zip bytes (bytes or KiB/MiB/GiB)",
        "  -ui-upload-max-assets int",
        "        maximum files in one UI artifact upload",
        "  -ui-upload-max-batch-bytes value",
        "        maximum aggregate bytes per upload (bytes or KiB/MiB/GiB)",
        "  -ui-upload-max-concurrent int",
        "        maximum process-wide concurrent UI artifact uploads",
        "  -ui-upload-max-concurrent-user int",
        "        maximum concurrent UI artifact uploads per principal",
        "  -ui-upload-max-duration duration",
        "        maximum duration of one UI artifact upload",
        "  -ui-upload-max-file-bytes value",
        "        maximum bytes per non-Go upload file (bytes or KiB/MiB/GiB)",
        "  -version",
        "        print version and exit",
        "  -vuln-rescan-interval duration",
        "        how often stale vulnerability scan results are re-queried",
        "  -vuln-ttl duration",
        "        age at which a vulnerability scan result becomes stale",
        "  -vuln-workers int",
        "        number of concurrent vulnerability scan workers draining the queue",
    ]
    .join("\n")
}

/// Lets the rest of the process settle before the first crawl, so a cold start
/// is not competing with a GitLab walk for its first requests.
const STARTUP_SCAN_DELAY: Duration = Duration::from_secs(30);

/// Bounds one report delivery round.
const COVERAGE_DELIVER_TIMEOUT: Duration = Duration::from_secs(30);

/// Fires the coverage scan on the administrator's cron.
///
/// It ticks every minute and asks the schedule whether this minute is a firing one, rather than
/// sleeping until a precomputed next run.
///
/// Leader-gated by the caller: a scan writes the result, the history row and the
/// exclusion state, and SQLite has one writer.
async fn run_coverage_schedule(
    cancel: CancellationToken,
    store: Arc<meta::Store>,
    scanner: Arc<coverage::Scanner>,
    notifier: Arc<notify::Notifier>,
) {
    let cfg = scanner.settings();
    tracing::info!(
        cron = %cfg.scan_cron,
        timezone = %cfg.timezone,
        auto = cfg.auto_scan_enabled,
        "coverage schedule started"
    );

    // A scan on the very first boot seeds the dashboard, which would otherwise
    // sit empty until the first cron firing, possibly a day later. A forklift
    // that already has a stored result does not rescan on restart: the picture is
    // there, and a rolling deploy would otherwise crawl GitLab once per pod.
    if scanner.configured() && scanner.overview().last_scanned_at.is_none() {
        tokio::spawn({
            let cancel = cancel.clone();
            let store = Arc::clone(&store);
            let scanner = Arc::clone(&scanner);
            let notifier = Arc::clone(&notifier);
            async move {
                tokio::select! {
                    _ = cancel.cancelled() => return,
                    _ = tokio::time::sleep(STARTUP_SCAN_DELAY) => {}
                }
                if let Err(e) = scanner.scan(coverage::TRIGGER_STARTUP).await {
                    tracing::warn!(err = %e, "coverage: initial scan failed");
                    return;
                }
                deliver_coverage_report(&store, &scanner, &notifier).await;
            }
        });
    }

    let mut ticker = tokio::time::interval(Duration::from_secs(60));
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    ticker.tick().await;
    // last_fired keeps a scan from running twice in one minute: the tick does not
    // land on the minute boundary, so two ticks can share a minute after a drift
    // correction.
    let mut last_fired: Option<i64> = None;
    loop {
        tokio::select! {
            _ = cancel.cancelled() => return,
            _ = ticker.tick() => {}
        }
        let now = Utc::now();
        let cfg = scanner.settings();
        if !cfg.auto_scan_enabled || !scanner.configured() {
            continue;
        }
        let schedule = match coverage::parse_schedule(&cfg.scan_cron, &cfg.timezone) {
            Ok(schedule) => schedule,
            Err(e) => {
                tracing::warn!(
                    cron = %cfg.scan_cron,
                    timezone = %cfg.timezone,
                    err = %e,
                    "coverage: schedule is not usable, skipping this tick"
                );
                continue;
            }
        };
        if !schedule.matches(now) {
            continue;
        }
        let minute = now.timestamp().div_euclid(60);
        if last_fired == Some(minute) {
            continue;
        }
        last_fired = Some(minute);
        if let Err(e) = scanner.scan(coverage::TRIGGER_SCHEDULE).await {
            tracing::warn!(err = %e, "coverage: scheduled scan failed");
            continue;
        }
        deliver_coverage_report(&store, &scanner, &notifier).await;
    }
}

/// Posts the coverage report to the receivers the settings name.
///
/// Only the automatic scans report: the scheduled one and the first scan after a
/// cold start. A scan somebody triggered from the console does not, because
/// asking for the current number is not asking to tell the whole receiver about
/// it; that is the settings page's own send button, which sends what the last
/// completed scan measured and ignores skip_when_full_coverage because somebody
/// asked for it. A failed delivery is logged and never fails the scan that
/// produced it.
async fn deliver_coverage_report(
    store: &Arc<meta::Store>,
    scanner: &Arc<coverage::Scanner>,
    notifier: &Arc<notify::Notifier>,
) {
    let cfg = scanner.settings();
    if !cfg.report_enabled || cfg.receiver.is_empty() {
        return;
    }
    let name = cfg.receiver.clone();

    let overview = scanner.overview();
    let projects = scanner
        .not_applied()
        .into_iter()
        .map(|p| notify::CoverageProject {
            path: p.path,
            partial: p.applied == coverage::STATE_PARTIAL,
            web_url: p.web_url,
        })
        .collect();
    let report = notify::CoverageReport {
        target: overview.summary.target,
        applied: overview.summary.applied,
        partial: overview.summary.partial,
        not_applied: overview.summary.not_applied,
        errored: overview.summary.errored,
        skipped: overview.summary.skipped,
        not_applied_projects: projects,
        ..Default::default()
    };
    if cfg.skip_when_full_coverage && notify::is_full_coverage(&report) {
        tracing::info!(
            applied = report.applied,
            target = report.target,
            "coverage: report skipped, coverage is full"
        );
        return;
    }

    let deliver = async {
        let all = match store.list_enabled_receivers().await {
            Ok(all) => all,
            Err(e) => {
                tracing::warn!(err = %e, "coverage: listing receivers failed");
                return;
            }
        };
        let targets: Vec<notify::Target> = all
            .into_iter()
            .find(|rec| rec.name == name && !rec.webhook_url.is_empty())
            .map(|rec| {
                vec![notify::Target {
                    name: rec.name,
                    url: rec.webhook_url,
                }]
            })
            .unwrap_or_default();
        if targets.is_empty() {
            tracing::warn!(
                receiver = %name,
                "coverage: the configured receiver is missing or disabled"
            );
            return;
        }
        notifier.notify_coverage_report(&targets, &report);
    };
    let _ = tokio::time::timeout(COVERAGE_DELIVER_TIMEOUT, deliver).await;
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
        unsafe {
            std::env::set_var("FORKLIFT_DATA_DIR", dir.path());
            std::env::set_var("FORKLIFT_HTTP_ADDR", "127.0.0.1:0");
            std::env::set_var("FORKLIFT_METRICS_ADDR", "127.0.0.1:0");
            std::env::set_var("FORKLIFT_PPROF_ADDR", "127.0.0.1:0");
            std::env::set_var(
                "FORKLIFT_SESSION_SECRET",
                "test-secret-test-secret-test-secret-0123",
            );
            std::env::set_var("FORKLIFT_BOOTSTRAP_ADMIN_PASSWORD", "admin-pass-123456");
            // Disable the vuln scanner and license resolver (no network).
            std::env::set_var("FORKLIFT_OSV_URL", "");
            std::env::set_var("FORKLIFT_DEPSDEV_URL", "");
            std::env::set_var("FORKLIFT_SHUTDOWN_TIMEOUT", "2s");
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
            Err(_) => panic!("run did not shut down after context cancel"),
        }
    }
}
