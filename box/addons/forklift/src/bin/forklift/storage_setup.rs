use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use crate::LOG_TARGET;
use anyhow::Context as _;
use forklift::{api, config, meta, metrics, objstore, storage};
use prometheus::Registry;
use tokio_util::sync::CancellationToken;

/// Bounds waiting for the object store at boot, e.g. a bundled store that
/// starts alongside forklift. It stays under the chart's startupProbe budget
/// (5s x 60) so a store that never comes up still ends in a restart.
const STORE_BOOT_BUDGET: Duration = Duration::from_secs(240);

/// The opened blob store plus what the s3 backend adds next to it.
pub(crate) struct BlobBackend {
    pub(crate) blobs: Arc<storage::InstrumentedStore>,
    pub(crate) meta_sync: Option<Arc<objstore::MetaSync>>,
    pub(crate) conditional_writes: Option<objstore::ConditionalWrites>,
    pub(crate) s3_client: Option<aws_sdk_s3::Client>,
}

/// Opens the blob store backend: local filesystem (default) or S3. In s3 mode blobs are
/// shared directly in the bucket and the metadata database is snapshotted to
/// S3 by meta_sync, so the deployment needs no EBS/RWX volume.
pub(crate) async fn open_blob_store(
    cfg: &config::Config,
    store: &Arc<meta::Store>,
    data_dir: &Path,
    reg: &Registry,
    shutdown: &CancellationToken,
) -> anyhow::Result<BlobBackend> {
    if cfg.storage.backend != "s3" {
        let fsblobs = storage::FsStore::new(&cfg.data_dir).context("open blob store")?;
        return Ok(BlobBackend {
            blobs: Arc::new(storage::instrument(Arc::new(fsblobs), "fs", reg)),
            meta_sync: None,
            conditional_writes: None,
            s3_client: None,
        });
    }
    let s3cfg = to_s3_config(&cfg.storage.s3);
    let s3blobs = storage::S3BlobStore::new(&s3cfg, data_dir.join("blob-tmp"))
        .await
        .context("open s3 blob store")?;
    let client = s3blobs.client();
    let s3_client = Some(client.clone());
    let blobs = Arc::new(storage::instrument(Arc::new(s3blobs), "s3", reg));
    if cfg.storage.s3.create_bucket {
        let api = objstore::S3Api::new(client.clone());
        let created = objstore::retry_transient(
            "create bucket",
            STORE_BOOT_BUDGET,
            Duration::from_secs(1),
            || api.ensure_bucket(&cfg.storage.s3.bucket),
        )
        .await
        .context("create s3 bucket")?;
        if created {
            tracing::info!(target: LOG_TARGET, bucket = %cfg.storage.s3.bucket, "created s3 bucket");
        }
    }
    let object_api: Arc<dyn objstore::ObjectApi> = Arc::new(objstore::S3Api::new(client));
    let conditional_writes = Some(
        check_conditional_writes(object_api.as_ref(), cfg)
            .await
            .context("probe s3 conditional writes")?,
    );
    let sync = objstore::MetaSync::new(objstore::MetaOptions {
        store: Arc::clone(store),
        api: object_api,
        bucket: cfg.storage.s3.bucket.clone(),
        key: objstore::meta_key(&cfg.storage.s3.prefix),
        data_dir: data_dir.to_path_buf(),
        interval: cfg.storage.meta_sync_interval,
        compression: objstore::Compression::parse(&cfg.storage.meta_compression)
            .unwrap_or_default(),
        mode: objstore::SyncMode::parse(&cfg.storage.meta_sync_mode).unwrap_or_default(),
        registry: Some(reg.clone()),
    });
    // Restore the latest snapshot before bootstrap/seed so they see existing
    // data. The live SQLite file lives on an ephemeral volume that loses its
    // contents on restart; an empty bucket is a clean no-op.
    objstore::retry_transient(
        "restore metadata",
        STORE_BOOT_BUDGET,
        Duration::from_secs(1),
        || sync.restore_on_boot(),
    )
    .await
    .context("restore metadata from s3")?;
    tokio::spawn(Arc::clone(&sync).run(shutdown.clone()));
    tracing::info!(target: LOG_TARGET,
        bucket = %cfg.storage.s3.bucket,
        prefix = %cfg.storage.s3.prefix,
        meta_compression = %cfg.storage.meta_compression,
        meta_sync_mode = %cfg.storage.meta_sync_mode,
        "storage backend: s3"
    );
    Ok(BlobBackend {
        blobs,
        meta_sync: Some(sync),
        conditional_writes,
        s3_client,
    })
}

/// Hands the admin console its storage view: the backend descriptor, the
/// cluster admin info when the provider has one, and in s3 mode the bucket
/// health monitor and the migration history.
pub(crate) fn wire_storage_backend(
    cfg: &config::Config,
    api_handler: &api::Handler,
    backend: &BlobBackend,
    reg: &Registry,
    shutdown: &CancellationToken,
) {
    let s3 = &cfg.storage.s3;
    let mut descriptor = if cfg.storage.backend == "s3" {
        let ep = s3.endpoint.trim_end_matches('/');
        api::StorageBackend {
            backend: "s3".into(),
            provider: s3.effective_provider().to_string(),
            endpoint: if ep.is_empty() {
                format!("s3://{}", s3.bucket)
            } else {
                ep.to_string()
            },
            region: s3.region.clone(),
            bucket: s3.bucket.clone(),
            prefix: s3.prefix.trim_start_matches('/').to_string(),
            ..Default::default()
        }
    } else {
        api::StorageBackend {
            backend: cfg.storage.backend.clone(),
            endpoint: cfg.data_dir.clone(),
            ..Default::default()
        }
    };
    if let Some(cw) = &backend.conditional_writes {
        descriptor.conditional_writes = Some(cw.is_enforced());
        if let objstore::ConditionalWrites::Ignored { reason } = cw {
            descriptor.conditional_writes_detail.clone_from(reason);
        }
    }
    let mut cluster: Option<api::ClusterInfoFn> = None;
    if let Some(spec) = storage::admin::provider(&descriptor.provider) {
        match storage::cluster_admin(spec, &to_admin_config(s3)) {
            Ok(Some(admin)) => cluster = Some(cluster_info_fn(admin)),
            Ok(None) => {}
            Err(e) => descriptor.cluster_unavailable = e.to_string(),
        }
    }
    api_handler.set_storage_backend(descriptor, cluster);
    if let Some(client) = &backend.s3_client {
        let monitor = storage::health::HealthMonitor::new(Arc::new(
            storage::health::HeadBucketProbe::new(client.clone(), &cfg.storage.s3.bucket),
        ));
        tokio::spawn(Arc::clone(&monitor).run(shutdown.clone()));
        reg.register(Box::new(metrics::StorageHealthCollector::new(Arc::clone(
            &monitor,
        ))))
        .expect("register forklift_storage_up");
        api_handler.set_storage_health(monitor);
        api_handler.set_migration_history(forklift::migrate::record::S3History::new(
            client.clone(),
            &cfg.storage.s3.bucket,
            &cfg.storage.s3.prefix,
        ));
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
        read_timeout: c.read_timeout,
    }
}

fn to_admin_config(c: &config::S3Config) -> storage::AdminConfig {
    storage::AdminConfig {
        s3_endpoint: c.endpoint.clone(),
        admin_endpoint: c.admin_endpoint.clone(),
        region: c.region.clone(),
        access_key_id: c.access_key_id.clone(),
        secret_access_key: c.secret_access_key.clone(),
        admin_token: c.admin_token.clone(),
    }
}

fn cluster_info_fn(admin: Arc<dyn storage::ClusterAdmin>) -> api::ClusterInfoFn {
    Arc::new(move || {
        let admin = Arc::clone(&admin);
        Box::pin(async move {
            match tokio::time::timeout(Duration::from_secs(5), admin.cluster_info()).await {
                Ok(res) => res.map_err(|e| e.to_string()),
                Err(_) => Err("cluster admin info: timed out".to_string()),
            }
        }) as std::pin::Pin<Box<dyn Future<Output = _> + Send>>
    })
}

/// HA fencing is unsound on a store that accepts writes whose precondition
/// failed, so HA refuses to start there; a single instance only warns.
async fn check_conditional_writes(
    api: &dyn objstore::ObjectApi,
    cfg: &config::Config,
) -> anyhow::Result<objstore::ConditionalWrites> {
    let key = format!(
        "{}.probe/{}",
        objstore::meta_key(&cfg.storage.s3.prefix).trim_end_matches("forklift.db"),
        uuid::Uuid::new_v4()
    );
    let result = objstore::retry_transient(
        "probe conditional writes",
        STORE_BOOT_BUDGET,
        Duration::from_secs(1),
        || objstore::probe_conditional_writes(api, &cfg.storage.s3.bucket, &key),
    )
    .await?;
    if let objstore::ConditionalWrites::Ignored { reason } = &result {
        let provider = cfg.storage.s3.effective_provider();
        if cfg.ha.enabled {
            anyhow::bail!(
                "s3 provider {provider} does not enforce conditional writes ({reason}); HA metadata fencing needs them, so disable HA or use a store that enforces If-Match/If-None-Match"
            );
        }
        tracing::warn!(target: LOG_TARGET, provider, reason = %reason, "s3 store ignores conditional writes; HA must stay disabled");
    }
    Ok(result)
}
