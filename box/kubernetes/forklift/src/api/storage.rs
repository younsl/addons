use std::collections::HashMap;
use std::sync::Arc;

use axum::extract::State;
use axum::response::Response;
use chrono::{DateTime, Utc};
use http::StatusCode;
use serde::Serialize;

use crate::meta;
use crate::storage;

use super::{Handler, map_error, write_json};

/// The object-storage overview shown on the Storage admin page: the active
/// backend and where artifacts live, forklift's own content-addressed footprint
/// (always available), and — for a MinIO backend — live cluster capacity, usage,
/// object counts and drive health from the MinIO Admin API.
#[derive(Debug, Clone, Default, Serialize)]
struct StorageStats {
    /// `fs` (PersistentVolume) or `s3` (object storage).
    backend: String,
    /// The human-facing storage mode for the overview: `filesystem`, `minio`
    /// (S3-compatible endpoint with a reachable MinIO admin API), or `s3` (AWS
    /// S3 or an S3-compatible endpoint without MinIO admin metrics).
    mode: String,
    /// Where artifacts live: the object-storage endpoint/bucket (s3) or the data
    /// directory (fs).
    #[serde(skip_serializing_if = "String::is_empty")]
    endpoint: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    bucket: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    prefix: String,
    /// Forklift's deduplicated, content-addressed blob footprint from the
    /// metadata database. Available for every backend.
    blob_count: i64,
    blob_bytes: i64,
    /// Live MinIO cluster metrics; absent for fs or when the MinIO Admin API is
    /// unreachable (`minio_error` then explains why).
    #[serde(skip_serializing_if = "Option::is_none")]
    minio: Option<MinIOStats>,
    #[serde(skip_serializing_if = "String::is_empty")]
    minio_error: String,
    /// The capacity of the volume the data directory lives on; set only for the
    /// fs backend, and absent when the filesystem cannot be measured (`fs_error`
    /// then explains why). AWS S3 has no such notion, so it stays absent there:
    /// the bucket is not a disk that fills up.
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

/// Mirrors [`storage::MinIOInfo`] for JSON delivery.
#[derive(Debug, Clone, Default, Serialize)]
struct MinIOStats {
    total_capacity_bytes: i64,
    used_bytes: i64,
    available_bytes: i64,
    usage_ratio: f64,
    logical_used_bytes: i64,
    object_count: i64,
    bucket_count: i64,
    online_drives: i64,
    offline_drives: i64,
    servers: i64,
    #[serde(skip_serializing_if = "String::is_empty")]
    version: String,
}

/// Reports the object-storage overview. Admin-only. The blob footprint always
/// comes from the metadata DB; MinIO cluster metrics are added when a MinIO
/// query closure is wired and reachable.
pub(super) async fn get_stats(State(h): State<Arc<Handler>>) -> Response {
    let (descriptor, minio) = {
        let injected = h.injected.read();
        (injected.storage.clone(), injected.minio.clone())
    };
    let backend = if descriptor.backend.is_empty() {
        "fs".to_string()
    } else {
        descriptor.backend.clone()
    };
    // The mode distinguishes filesystem / MinIO (S3-compatible endpoint, admin
    // API wired) / plain AWS S3, for the overview label.
    let mode = if backend == "s3" {
        if minio.is_some() { "minio" } else { "s3" }
    } else {
        "filesystem"
    };
    let mut out = StorageStats {
        backend: backend.clone(),
        mode: mode.to_string(),
        endpoint: descriptor.endpoint.clone(),
        bucket: descriptor.bucket.clone(),
        prefix: descriptor.prefix.clone(),
        ..Default::default()
    };
    match h.store.blob_stats().await {
        Ok((count, bytes)) => {
            out.blob_count = count;
            out.blob_bytes = bytes;
        }
        Err(err) => return map_error(err),
    }
    // Filesystem backend: report how full the volume is. This is the fs answer
    // to the capacity question MinIO answers below, and the only one available
    // for a PersistentVolume.
    if backend == "fs" && !descriptor.endpoint.is_empty() {
        match storage::disk_usage(&descriptor.endpoint) {
            Ok(disk) => out.fs = Some(disk),
            Err(err) => out.fs_error = err.to_string(),
        }
    }
    if let Some(minio) = minio {
        match minio().await {
            Ok(info) => {
                out.minio = Some(MinIOStats {
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
                });
            }
            Err(err) => out.minio_error = err,
        }
    }
    out.dangling = dangling_refs(&h, 0).await;
    write_json(StatusCode::OK, out)
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
                ..Default::default()
            },
            None,
        );
        srv
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
