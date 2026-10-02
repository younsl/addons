use std::collections::HashMap;
use std::sync::Arc;

use axum::extract::{Path, State};
use axum::response::Response;
use chrono::{DateTime, Utc};
use http::StatusCode;
use serde::Serialize;

use crate::meta;
use crate::storage;

use super::{Handler, map_error, write_error, write_json};
use crate::migrate::record::{MigrationStoredAt, MigrationSummary};

/// The object-storage overview shown on the Storage admin page.
#[derive(Debug, Clone, Default, Serialize)]
struct StorageStats {
    /// `fs` (PersistentVolume) or `s3` (object storage).
    backend: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    provider: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    provider_name: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    endpoint: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    bucket: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    prefix: String,
    blob_count: i64,
    blob_bytes: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    cluster: Option<ClusterStats>,
    #[serde(skip_serializing_if = "String::is_empty")]
    cluster_error: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    conditional_writes: Option<bool>,
    #[serde(skip_serializing_if = "String::is_empty")]
    conditional_writes_detail: String,
    /// The newest migration run recorded in this store.
    #[serde(skip_serializing_if = "Option::is_none")]
    last_migration: Option<MigrationSummary>,
    #[serde(skip_serializing_if = "String::is_empty")]
    migration_history_error: String,
    /// Absent for S3: a bucket is not a disk that fills up.
    #[serde(skip_serializing_if = "Option::is_none")]
    fs: Option<storage::Disk>,
    #[serde(skip_serializing_if = "String::is_empty")]
    fs_error: String,
    /// Artifacts whose bytes are absent from the blob store, so serving them
    /// fails. Metadata and bytes are separate stores, and this is where they
    /// disagree; the Storage page is where an operator looks when they need the
    /// whole picture rather than one repository's artifact list.
    dangling: Vec<DanglingRefDTO>,
}

/// One metadata reference whose blob bytes are missing.
#[derive(Debug, Clone, Serialize)]
pub(super) struct DanglingRefDTO {
    pub(super) repository: String,
    pub(super) repo_id: i64,
    pub(super) path: String,
    pub(super) sha256: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub(super) role: String,
    pub(super) first_seen: DateTime<Utc>,
    pub(super) last_seen: DateTime<Utc>,
    pub(super) hits: i64,
    /// The response codes these failures produced, most frequent first. A fetch
    /// answers 500 and a publish or delete answers 503, so this is what lets an
    /// operator match the row against the error someone reported.
    pub(super) statuses: Vec<StatusCountDTO>,
    /// The code the most recent failure returned.
    #[serde(skip_serializing_if = "is_zero")]
    pub(super) last_status: i64,
}

/// One observed response code and how often it was returned.
#[derive(Debug, Clone, Serialize)]
pub(super) struct StatusCountDTO {
    pub(super) code: i64,
    pub(super) count: i64,
}

fn is_zero(v: &i64) -> bool {
    *v == 0
}

/// Flattens the status histogram into a stable order: most frequent first, then
/// by code so equal counts do not shuffle between requests.
pub(super) fn status_counts(in_: &HashMap<i64, i64>) -> Vec<StatusCountDTO> {
    let mut out: Vec<StatusCountDTO> = in_
        .iter()
        .map(|(code, count)| StatusCountDTO {
            code: *code,
            count: *count,
        })
        .collect();
    out.sort_by(|a, b| b.count.cmp(&a.count).then(a.code.cmp(&b.code)));
    out
}

/// Caps the payload. The count is a fault signal, not a dataset: if it is ever
/// long, the point has already been made.
const MAX_DANGLING_REPORTED: usize = 100;

/// Mirrors [`storage::ClusterInfo`] for JSON delivery.
#[derive(Debug, Clone, Default, Serialize)]
struct ClusterStats {
    total_capacity_bytes: i64,
    used_bytes: i64,
    available_bytes: i64,
    usage_ratio: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    logical_used_bytes: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    object_count: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    bucket_count: Option<i64>,
    online_drives: i64,
    offline_drives: i64,
    servers: i64,
    #[serde(skip_serializing_if = "String::is_empty")]
    version: String,
}

impl From<storage::ClusterInfo> for ClusterStats {
    fn from(info: storage::ClusterInfo) -> Self {
        ClusterStats {
            total_capacity_bytes: info.total_capacity_bytes,
            used_bytes: info.used_bytes,
            available_bytes: info.available_bytes,
            usage_ratio: info.usage_ratio,
            logical_used_bytes: info.logical_used_bytes,
            object_count: info.object_count,
            bucket_count: info.bucket_count,
            online_drives: info.online_drives,
            offline_drives: info.offline_drives,
            servers: info.servers,
            version: info.version,
        }
    }
}

/// Reports the object-storage overview. Admin-only.
pub(super) async fn get_stats(State(h): State<Arc<Handler>>) -> Response {
    let (descriptor, cluster, migrations) = {
        let injected = h.injected.read();
        (
            injected.storage.clone(),
            injected.cluster.clone(),
            injected.migrations.clone(),
        )
    };
    let backend = if descriptor.backend.is_empty() {
        "fs".to_string()
    } else {
        descriptor.backend.clone()
    };
    let provider_name = storage::admin::provider(&descriptor.provider)
        .map(|p| p.display_name.to_string())
        .unwrap_or_default();
    let mut out = StorageStats {
        backend: backend.clone(),
        provider: descriptor.provider.clone(),
        provider_name,
        endpoint: descriptor.endpoint.clone(),
        bucket: descriptor.bucket.clone(),
        prefix: descriptor.prefix.clone(),
        cluster_error: descriptor.cluster_unavailable.clone(),
        conditional_writes: descriptor.conditional_writes,
        conditional_writes_detail: descriptor.conditional_writes_detail.clone(),
        ..Default::default()
    };
    match h.store.blob_stats().await {
        Ok((count, bytes)) => {
            out.blob_count = count;
            out.blob_bytes = bytes;
        }
        Err(err) => return map_error(err),
    }
    if backend == "fs" && !descriptor.endpoint.is_empty() {
        match storage::disk_usage(&descriptor.endpoint) {
            Ok(disk) => out.fs = Some(disk),
            Err(err) => out.fs_error = err.to_string(),
        }
    }
    if let Some(cluster) = cluster {
        match cluster().await {
            Ok(info) => out.cluster = Some(info.into()),
            Err(err) => out.cluster_error = err,
        }
    }
    if let Some(history) = migrations {
        match tokio::time::timeout(std::time::Duration::from_secs(5), history.list()).await {
            Ok(Ok(list)) => out.last_migration = list.into_iter().next(),
            Ok(Err(e)) => out.migration_history_error = e,
            Err(_) => out.migration_history_error = "migration history: timed out".into(),
        }
    }
    out.dangling = dangling_refs(&h, 0).await;
    write_json(StatusCode::OK, out)
}

#[derive(Debug, Serialize)]
struct MigrationListDTO {
    items: Vec<MigrationSummary>,
}

/// Lists recorded migration runs, newest first. Admin-only. Empty for the fs
/// backend, which has no object store to keep them in.
pub(super) async fn list_migrations(State(h): State<Arc<Handler>>) -> Response {
    let history = h.injected.read().migrations.clone();
    let Some(history) = history else {
        return write_json(StatusCode::OK, MigrationListDTO { items: Vec::new() });
    };
    match history.list().await {
        Ok(items) => write_json(StatusCode::OK, MigrationListDTO { items }),
        Err(e) => write_error(StatusCode::BAD_GATEWAY, &e),
    }
}

/// Returns one migration run with every check. Admin-only.
pub(super) async fn get_migration(
    State(h): State<Arc<Handler>>,
    Path(id): Path<String>,
) -> Response {
    let (history, store) = {
        let injected = h.injected.read();
        (injected.migrations.clone(), injected.storage.clone())
    };
    let Some(history) = history else {
        return write_error(StatusCode::NOT_FOUND, "not found");
    };
    match history.get(&id).await {
        Ok(Some(mut rec)) => {
            rec.stored_at = Some(MigrationStoredAt::new(
                &store.provider,
                &store.endpoint,
                &store.region,
                &store.bucket,
                &store.prefix,
                &rec.id,
            ));
            write_json(StatusCode::OK, rec)
        }
        Ok(None) => write_error(StatusCode::NOT_FOUND, "not found"),
        Err(e) => write_error(StatusCode::BAD_GATEWAY, &e),
    }
}

/// Collects the tracked missing-blob references, for one repository when
/// `repo_id` is non-zero and across all of them when it is 0.
///
/// Each one is re-checked against its artifact row before it is reported: an
/// entry whose artifact has since been deleted, or now points at different
/// bytes, is stale and is dropped rather than shown. That keeps a view from
/// accusing an artifact somebody already fixed.
pub(super) async fn dangling_refs(h: &Arc<Handler>, repo_id: i64) -> Vec<DanglingRefDTO> {
    let mut out: Vec<DanglingRefDTO> = Vec::new();
    let Some(manager) = h.repo_manager() else {
        return out;
    };
    let mut names: HashMap<i64, String> = HashMap::new();
    for reference in manager.dangling_refs() {
        if repo_id != 0 && reference.repo_id != repo_id {
            continue;
        }
        match h
            .store
            .get_artifact(reference.repo_id, &reference.path)
            .await
        {
            Err(meta::Error::NotFound) => {
                manager.forget_dangling_ref(reference.repo_id, &reference.path);
                continue;
            }
            Ok(artifact) if artifact.blob_sha256 != reference.sha256 => {
                manager.forget_dangling_ref(reference.repo_id, &reference.path);
                continue;
            }
            Err(_) => continue,
            Ok(_) => {}
        }
        // Bytes restored in place keep the digest, so existence is the real
        // check.
        if !manager
            .still_missing(reference.repo_id, &reference.path, &reference.sha256)
            .await
        {
            continue;
        }
        let name = match names.get(&reference.repo_id) {
            Some(name) => name.clone(),
            None => {
                let name = h
                    .store
                    .get_repository(reference.repo_id)
                    .await
                    .map(|repo| repo.name)
                    .unwrap_or_default();
                names.insert(reference.repo_id, name.clone());
                name
            }
        };
        out.push(DanglingRefDTO {
            repository: name,
            repo_id: reference.repo_id,
            path: reference.path.clone(),
            sha256: reference.sha256.clone(),
            role: reference.role.clone(),
            first_seen: reference.first_seen,
            last_seen: reference.last_seen,
            hits: reference.hits,
            statuses: status_counts(&reference.statuses),
            last_status: reference.last_status,
        });
    }
    // Oldest failure first: the longest-broken artifact is the one to fix.
    out.sort_by(|a, b| {
        a.first_seen
            .cmp(&b.first_seen)
            .then_with(|| a.path.cmp(&b.path))
    });
    out.truncate(MAX_DANGLING_REPORTED);
    out
}

#[cfg(test)]
pub(crate) mod tests {
    use http::{Method, StatusCode};
    use serde_json::Value;

    use crate::api::StorageBackend;
    use crate::testing::api::{TestServer, new_test_server};

    /// Serves the admin API with a storage descriptor wired, which is what
    /// production does once the backend is known.
    async fn storage_harness(backend: &str, endpoint: &str) -> TestServer {
        let srv = new_test_server().await;
        srv.handler.set_storage_backend(
            StorageBackend {
                backend: backend.to_string(),
                endpoint: endpoint.to_string(),
                bucket: "forklift".into(),
                ..Default::default()
            },
            None,
        );
        srv
    }

    /// The cluster panel reads whichever admin client is wired, and a
    /// failing one is reported rather than dropped.
    #[tokio::test]
    async fn storage_stats_cluster_from_injected_admin() {
        let srv = new_test_server().await;
        srv.handler.set_storage_backend(
            StorageBackend {
                backend: "s3".into(),
                provider: "seaweedfs".into(),
                conditional_writes: Some(true),
                ..Default::default()
            },
            Some(std::sync::Arc::new(|| {
                Box::pin(async {
                    let mut info = crate::storage::ClusterInfo {
                        servers: 2,
                        ..Default::default()
                    };
                    info.set_capacity(100, 25, 75);
                    Ok(info)
                })
            })),
        );
        let stats = get_storage(&srv).await;
        assert_eq!(stats["provider"], "seaweedfs");
        assert_eq!(stats["provider_name"], "SeaweedFS");
        assert_eq!(stats["conditional_writes"], true);
        assert_eq!(stats["cluster"]["usage_ratio"], 0.25);
        assert_eq!(stats["cluster"]["servers"], 2);
        assert_eq!(
            stats["cluster"]["object_count"],
            Value::Null,
            "unknown count"
        );

        let srv = new_test_server().await;
        srv.handler.set_storage_backend(
            StorageBackend {
                backend: "s3".into(),
                provider: "garage".into(),
                conditional_writes: Some(false),
                conditional_writes_detail: "ignored".into(),
                ..Default::default()
            },
            Some(std::sync::Arc::new(|| {
                Box::pin(async { Err("garage admin: 401".to_string()) })
            })),
        );
        let stats = get_storage(&srv).await;
        assert_eq!(stats["cluster"], Value::Null);
        assert_eq!(stats["cluster_error"], "garage admin: 401");
        assert_eq!(stats["conditional_writes"], false);
        assert_eq!(stats["conditional_writes_detail"], "ignored");
    }

    /// Missing admin settings are explained on the page instead of hiding the
    /// panel silently.
    #[tokio::test]
    async fn storage_stats_reports_unavailable_admin() {
        let srv = new_test_server().await;
        srv.handler.set_storage_backend(
            StorageBackend {
                backend: "s3".into(),
                provider: "garage".into(),
                cluster_unavailable: "admin api unavailable: token".into(),
                ..Default::default()
            },
            None,
        );
        let stats = get_storage(&srv).await;
        assert_eq!(stats["cluster_error"], "admin api unavailable: token");
    }

    struct FakeHistory(Vec<crate::migrate::record::MigrationRecord>);

    #[async_trait::async_trait]
    impl crate::migrate::record::History for FakeHistory {
        async fn list(&self) -> Result<Vec<crate::migrate::record::MigrationSummary>, String> {
            Ok(self.0.iter().map(Into::into).collect())
        }
        async fn get(
            &self,
            id: &str,
        ) -> Result<Option<crate::migrate::record::MigrationRecord>, String> {
            Ok(self.0.iter().find(|r| r.id == id).cloned())
        }
    }

    #[tokio::test]
    async fn migration_history_is_listed_summarised_and_drilled_into() {
        use crate::migrate::record::tests::sample;
        let srv = storage_harness("s3", "http://seaweedfs:8333").await;
        let newer = sample("20261003T010203Z-0123abcd", "failed");
        let older = sample("20261001T000000Z-aaaaaaaa", "succeeded");
        srv.handler
            .set_migration_history(std::sync::Arc::new(FakeHistory(vec![newer.clone(), older])));

        let stats = get_storage(&srv).await;
        assert_eq!(stats["last_migration"]["id"], newer.id);
        assert_eq!(stats["last_migration"]["outcome"], "failed");
        assert_eq!(stats["last_migration"]["failed_stage"], "postflight");
        let dot = &stats["last_migration"]["preflight"][0];
        assert_eq!(dot["id"], "PF01");
        assert_eq!(dot.get("detail"), None, "summary carries statuses only");

        let resp = srv.admin_do(Method::GET, "/storage/migrations", "").await;
        assert_eq!(resp.status, StatusCode::OK, "{}", resp.text());
        let list: Value = resp.json();
        assert_eq!(list["items"].as_array().unwrap().len(), 2);

        let resp = srv
            .admin_do(
                Method::GET,
                &format!("/storage/migrations/{}", newer.id),
                "",
            )
            .await;
        assert_eq!(resp.status, StatusCode::OK, "{}", resp.text());
        let detail: Value = resp.json();
        assert_eq!(detail["preflight"][0]["id"], "PF01");
        assert_eq!(detail["settings"]["verify"], "sample 5%");
        assert_eq!(
            detail["stored_at"]["uri"],
            format!("s3://forklift/meta/migrations/{}.json", newer.id)
        );
        assert_eq!(detail["stored_at"]["endpoint"], "http://seaweedfs:8333");

        let resp = srv
            .admin_do(
                Method::GET,
                "/storage/migrations/20990101T000000Z-ffffffff",
                "",
            )
            .await;
        assert_eq!(resp.status, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn no_history_without_object_storage() {
        let srv = storage_harness("fs", "/data").await;
        let resp = srv.admin_do(Method::GET, "/storage/migrations", "").await;
        assert_eq!(resp.status, StatusCode::OK);
        let v: Value = resp.json();
        assert_eq!(v["items"], serde_json::json!([]));
        let resp = srv
            .admin_do(
                Method::GET,
                "/storage/migrations/20261003T010203Z-0123abcd",
                "",
            )
            .await;
        assert_eq!(resp.status, StatusCode::NOT_FOUND);
        assert_eq!(get_storage(&srv).await["last_migration"], Value::Null);
    }

    async fn get_storage(srv: &TestServer) -> Value {
        let resp = srv.admin_do(Method::GET, "/storage", "").await;
        assert_eq!(resp.status, StatusCode::OK, "{}", resp.text());
        resp.json()
    }

    /// The fs backend reports the volume the data directory lives on, which is what
    /// the HA topology draws its utilization bar from.
    #[tokio::test]
    async fn storage_stats_filesystem_capacity() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().to_string_lossy().to_string();
        let stats = get_storage(&storage_harness("fs", &path).await).await;

        assert_eq!(stats["fs_error"], Value::Null, "fs_error, want none");
        let fs = stats["fs"].as_object().expect("fs capacity missing");
        assert!(
            fs["total_bytes"].as_i64().unwrap_or_default() > 0,
            "implausible fs capacity: {stats}"
        );
        let ratio = fs["usage_ratio"].as_f64().unwrap_or_default();
        assert!(
            ratio > 0.0 && ratio <= 1.0,
            "implausible fs capacity: {stats}"
        );
        assert_eq!(fs["path"], path, "fs path");
    }

    /// An S3 backend reports no filesystem capacity: a bucket is not a disk that
    /// fills up, and a utilization figure there would invent a limit that does not
    /// exist.
    #[tokio::test]
    async fn storage_stats_s3_no_filesystem_capacity() {
        let stats = get_storage(&storage_harness("s3", "s3://artifacts").await).await;

        assert_eq!(stats["fs"], Value::Null, "fs capacity reported for s3");
        assert_eq!(stats["fs_error"], Value::Null, "fs_error, want none for s3");
    }

    /// A data directory that cannot be measured is reported as an error rather than
    /// a zeroed volume, which would render as an empty disk.
    #[tokio::test]
    async fn storage_stats_unmeasurable_filesystem() {
        let dir = tempfile::tempdir().expect("temp dir");
        let missing = dir.path().join("gone").to_string_lossy().to_string();
        let stats = get_storage(&storage_harness("fs", &missing).await).await;

        assert_eq!(
            stats["fs"],
            Value::Null,
            "fs capacity reported for a missing directory"
        );
        assert!(
            stats["fs_error"].as_str().is_some_and(|e| !e.is_empty()),
            "fs_error missing for a missing directory: {stats}"
        );
    }
}
