//! Every migration run, succeeded, failed or dry, leaves a record in the target
//! bucket under `<prefix>/meta/migrations/<id>.json`, so administrators can
//! review the history in the Storage page after the cutover. Ids sort by
//! finish time. A run that cannot write to the target leaves no record; its
//! Job log is then the only trace.

use std::sync::Arc;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use tokio::io::AsyncReadExt;

use super::{Check, Endpoint, Error, Options, RunProgress};
use crate::objstore::{ObjectApi, PutBody, PutObjectInput};

/// Records listed at most; older ones stay in the bucket.
pub const HISTORY_LIMIT: usize = 100;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MigrationLocation {
    /// Provider id such as `minio` or `seaweedfs`; empty in records written before it was kept.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub provider: String,
    pub endpoint: String,
    pub bucket: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub prefix: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MigrationCheck {
    pub id: String,
    pub name: String,
    pub status: String,
    pub detail: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub latency_us: Option<u64>,
}

impl From<&Check> for MigrationCheck {
    fn from(c: &Check) -> Self {
        MigrationCheck {
            id: c.id.to_string(),
            name: c.name.to_string(),
            status: serde_json::to_value(c.status)
                .ok()
                .and_then(|v| v.as_str().map(str::to_owned))
                .unwrap_or_default(),
            detail: c.detail.clone(),
            latency_us: c.latency_us,
        }
    }
}

/// One check reduced to what the history table draws.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MigrationCheckStatus {
    pub id: String,
    pub name: String,
    pub status: String,
}

impl From<&MigrationCheck> for MigrationCheckStatus {
    fn from(c: &MigrationCheck) -> Self {
        MigrationCheckStatus {
            id: c.id.clone(),
            name: c.name.clone(),
            status: c.status.clone(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MigrationSettings {
    pub concurrency: usize,
    pub dry_run: bool,
    pub overwrite_meta: bool,
    pub allow_missing_source_blobs: bool,
    pub require_conditional_writes: bool,
    pub verify: String,
}

/// Where the record object itself is kept. The API fills it in on read and it is never written.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MigrationStoredAt {
    pub provider: String,
    pub endpoint: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub region: String,
    pub bucket: String,
    pub key: String,
    pub uri: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub size_bytes: Option<u64>,
}

impl MigrationStoredAt {
    pub fn new(
        provider: &str,
        endpoint: &str,
        region: &str,
        bucket: &str,
        bucket_prefix: &str,
        id: &str,
    ) -> Self {
        let key = key(bucket_prefix, id);
        MigrationStoredAt {
            provider: provider.to_string(),
            endpoint: endpoint.to_string(),
            region: region.to_string(),
            bucket: bucket.to_string(),
            uri: format!("s3://{bucket}/{key}"),
            key,
            size_bytes: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MigrationRecord {
    pub id: String,
    /// `succeeded`, `failed` or `dry_run`.
    pub outcome: String,
    /// The stage a failed run stopped in: preflight, copy, verify, upload or postflight.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failed_stage: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// What postflight removed from the target after a failure.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remediation: Option<String>,
    pub started_at: DateTime<Utc>,
    pub finished_at: DateTime<Utc>,
    pub duration_ms: u64,
    pub forklift_version: String,
    pub source: MigrationLocation,
    pub target: MigrationLocation,
    pub settings: MigrationSettings,
    pub required: i64,
    pub copied: i64,
    pub skipped: i64,
    pub bytes_copied: i64,
    pub verified_blobs: i64,
    pub meta_copied: bool,
    pub preflight_summary: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub postflight_summary: String,
    pub preflight: Vec<MigrationCheck>,
    pub postflight: Vec<MigrationCheck>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stored_at: Option<MigrationStoredAt>,
    /// Size of the record object as read, so the API can report it.
    #[serde(skip)]
    pub stored_bytes: Option<u64>,
}

/// The fields the history table shows, without the per-check detail.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MigrationSummary {
    pub id: String,
    pub outcome: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failed_stage: Option<String>,
    pub started_at: DateTime<Utc>,
    pub finished_at: DateTime<Utc>,
    pub duration_ms: u64,
    pub source: MigrationLocation,
    pub target: MigrationLocation,
    pub required: i64,
    pub copied: i64,
    pub skipped: i64,
    pub bytes_copied: i64,
    pub preflight_summary: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub postflight_summary: String,
    pub preflight: Vec<MigrationCheckStatus>,
    pub postflight: Vec<MigrationCheckStatus>,
}

impl From<&MigrationRecord> for MigrationSummary {
    fn from(r: &MigrationRecord) -> Self {
        MigrationSummary {
            id: r.id.clone(),
            outcome: r.outcome.clone(),
            failed_stage: r.failed_stage.clone(),
            started_at: r.started_at,
            finished_at: r.finished_at,
            duration_ms: r.duration_ms,
            source: r.source.clone(),
            target: r.target.clone(),
            required: r.required,
            copied: r.copied,
            skipped: r.skipped,
            bytes_copied: r.bytes_copied,
            preflight_summary: r.preflight_summary.clone(),
            postflight_summary: r.postflight_summary.clone(),
            preflight: r.preflight.iter().map(Into::into).collect(),
            postflight: r.postflight.iter().map(Into::into).collect(),
        }
    }
}

pub fn key(bucket_prefix: &str, id: &str) -> String {
    format!("{}{id}.json", prefix(bucket_prefix))
}

pub fn prefix(bucket_prefix: &str) -> String {
    let p = bucket_prefix.trim_matches('/');
    if p.is_empty() {
        "meta/migrations/".to_string()
    } else {
        format!("{p}/meta/migrations/")
    }
}

pub fn new_id(finished_at: DateTime<Utc>) -> String {
    let suffix = uuid::Uuid::new_v4().simple().to_string();
    format!("{}-{}", finished_at.format("%Y%m%dT%H%M%SZ"), &suffix[..8])
}

/// Ids are generated by [`new_id`]; anything else is refused so a request
/// can never name a key outside the history prefix.
pub fn valid_id(id: &str) -> bool {
    let b = id.as_bytes();
    b.len() == 25
        && b[8] == b'T'
        && b[15] == b'Z'
        && b[16] == b'-'
        && b[..8].iter().chain(&b[9..15]).all(u8::is_ascii_digit)
        && b[17..]
            .iter()
            .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
}

fn location(e: &Endpoint) -> MigrationLocation {
    MigrationLocation {
        provider: e.provider.clone(),
        endpoint: e.endpoint.clone(),
        bucket: e.bucket.clone(),
        prefix: e.prefix.clone(),
    }
}

pub(crate) fn build(
    src: &Endpoint,
    dst: &Endpoint,
    opts: &Options,
    progress: &RunProgress,
    error: Option<&Error>,
    started_at: DateTime<Utc>,
) -> MigrationRecord {
    let finished_at = Utc::now();
    let r = &progress.report;
    let outcome = match (error, opts.dry_run) {
        (Some(_), _) => "failed",
        (None, true) => "dry_run",
        (None, false) => "succeeded",
    };
    let error_text = error.map(|e| match e {
        Error::Preflight(checks) => super::preflight::summary(checks),
        Error::Postflight { checks, .. } => super::preflight::summary(checks),
        other => other.to_string(),
    });
    MigrationRecord {
        id: new_id(finished_at),
        outcome: outcome.into(),
        failed_stage: error.map(|_| progress.stage.to_string()),
        error: error_text,
        remediation: progress.remediation.clone(),
        started_at,
        finished_at,
        duration_ms: (finished_at - started_at).num_milliseconds().max(0) as u64,
        forklift_version: crate::version::string(),
        source: location(src),
        target: location(dst),
        settings: MigrationSettings {
            concurrency: opts.concurrency,
            dry_run: opts.dry_run,
            overwrite_meta: opts.overwrite_meta,
            allow_missing_source_blobs: opts.allow_missing_source_blobs,
            require_conditional_writes: opts.require_conditional_writes,
            verify: opts.verify.to_string(),
        },
        required: r.required,
        copied: r.copied,
        skipped: r.skipped,
        bytes_copied: r.bytes_copied,
        verified_blobs: r.verified_blobs,
        meta_copied: r.meta_copied,
        preflight_summary: if r.preflight.is_empty() {
            String::new()
        } else {
            super::preflight::summary(&r.preflight)
        },
        postflight_summary: if r.postflight.is_empty() {
            String::new()
        } else {
            super::preflight::summary(&r.postflight)
        },
        preflight: r.preflight.iter().map(Into::into).collect(),
        postflight: r.postflight.iter().map(Into::into).collect(),
        stored_at: None,
        stored_bytes: None,
    }
}

pub async fn write(
    objects: &dyn ObjectApi,
    bucket: &str,
    bucket_prefix: &str,
    record: &MigrationRecord,
) -> Result<(), String> {
    let body = serde_json::to_vec_pretty(record).map_err(|e| e.to_string())?;
    objects
        .put_object(PutObjectInput {
            bucket: bucket.to_string(),
            key: key(bucket_prefix, &record.id),
            content_length: body.len() as i64,
            body: PutBody::Bytes(body),
            metadata: Default::default(),
            if_match: None,
            if_none_match: None,
        })
        .await
        .map(|_| ())
        .map_err(|e| e.to_string())
}

/// Read access to the migration history for the Storage page.
#[async_trait]
pub trait History: Send + Sync {
    /// Newest first, at most [`HISTORY_LIMIT`].
    async fn list(&self) -> Result<Vec<MigrationSummary>, String>;
    async fn get(&self, id: &str) -> Result<Option<MigrationRecord>, String>;
}

pub struct S3History {
    client: aws_sdk_s3::Client,
    bucket: String,
    prefix: String,
}

impl S3History {
    pub fn new(client: aws_sdk_s3::Client, bucket: &str, bucket_prefix: &str) -> Arc<S3History> {
        Arc::new(S3History {
            client,
            bucket: bucket.to_string(),
            prefix: prefix(bucket_prefix),
        })
    }

    async fn ids(&self) -> Result<Vec<String>, String> {
        let mut ids = Vec::new();
        let mut token: Option<String> = None;
        loop {
            let page = self
                .client
                .list_objects_v2()
                .bucket(&self.bucket)
                .prefix(&self.prefix)
                .set_continuation_token(token.take())
                .send()
                .await
                .map_err(|e| {
                    format!(
                        "list migration records: {}",
                        aws_sdk_s3::error::DisplayErrorContext(&e)
                    )
                })?;
            for obj in page.contents() {
                if let Some(id) = obj
                    .key()
                    .and_then(|k| k.strip_prefix(&self.prefix))
                    .and_then(|k| k.strip_suffix(".json"))
                    && valid_id(id)
                {
                    ids.push(id.to_string());
                }
            }
            match page.next_continuation_token() {
                Some(t) if page.is_truncated() == Some(true) => token = Some(t.to_string()),
                _ => break,
            }
        }
        ids.sort_unstable_by(|a, b| b.cmp(a));
        ids.truncate(HISTORY_LIMIT);
        Ok(ids)
    }

    async fn read(&self, id: &str) -> Result<Option<MigrationRecord>, String> {
        let key = format!("{}{id}.json", self.prefix);
        let out = match self
            .client
            .get_object()
            .bucket(&self.bucket)
            .key(&key)
            .send()
            .await
        {
            Ok(out) => out,
            Err(e) if e.raw_response().map(|r| r.status().as_u16()) == Some(404) => {
                return Ok(None);
            }
            Err(e) => {
                return Err(format!(
                    "read {key}: {}",
                    aws_sdk_s3::error::DisplayErrorContext(&e)
                ));
            }
        };
        let mut body = Vec::new();
        out.body
            .into_async_read()
            .read_to_end(&mut body)
            .await
            .map_err(|e| format!("read {key}: {e}"))?;
        let mut rec: MigrationRecord =
            serde_json::from_slice(&body).map_err(|e| format!("parse {key}: {e}"))?;
        rec.stored_bytes = Some(body.len() as u64);
        Ok(Some(rec))
    }
}

#[async_trait]
impl History for S3History {
    async fn list(&self) -> Result<Vec<MigrationSummary>, String> {
        let mut out = Vec::new();
        for id in self.ids().await? {
            match self.read(&id).await {
                Ok(Some(rec)) => out.push(MigrationSummary::from(&rec)),
                Ok(None) => {}
                Err(e) => tracing::warn!(id, err = %e, "skip unreadable migration record"),
            }
        }
        Ok(out)
    }

    async fn get(&self, id: &str) -> Result<Option<MigrationRecord>, String> {
        if !valid_id(id) {
            return Ok(None);
        }
        self.read(id).await
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use chrono::TimeZone;

    use crate::migrate::record::*;

    pub(crate) fn sample(id: &str, outcome: &str) -> MigrationRecord {
        let at = Utc.with_ymd_and_hms(2026, 10, 3, 1, 2, 3).unwrap();
        MigrationRecord {
            id: id.into(),
            outcome: outcome.into(),
            failed_stage: (outcome == "failed").then(|| "postflight".into()),
            error: (outcome == "failed").then(|| "postflight: 4 checks, 3 passed".into()),
            remediation: None,
            started_at: at - chrono::Duration::seconds(90),
            finished_at: at,
            duration_ms: 90_000,
            forklift_version: "0.14.0".into(),
            source: MigrationLocation {
                provider: "minio".into(),
                endpoint: "http://minio:9000".into(),
                bucket: "forklift".into(),
                prefix: String::new(),
            },
            target: MigrationLocation {
                provider: "seaweedfs".into(),
                endpoint: "http://seaweedfs:8333".into(),
                bucket: "forklift".into(),
                prefix: String::new(),
            },
            settings: MigrationSettings {
                concurrency: 8,
                dry_run: false,
                overwrite_meta: false,
                allow_missing_source_blobs: false,
                require_conditional_writes: true,
                verify: "sample 5%".into(),
            },
            required: 31,
            copied: 31,
            skipped: 0,
            bytes_copied: 8_473_536,
            verified_blobs: 20,
            meta_copied: outcome == "succeeded",
            preflight_summary: "preflight: 12 checks, 12 passed, 0 warned, 0 failed, 0 skipped"
                .into(),
            postflight_summary: "postflight: 4 checks, 4 passed, 0 warned, 0 failed, 0 skipped"
                .into(),
            preflight: vec![MigrationCheck {
                id: "PF01".into(),
                name: "lease".into(),
                status: "pass".into(),
                detail: "held".into(),
                latency_us: Some(3700),
            }],
            postflight: Vec::new(),
            stored_at: None,
            stored_bytes: None,
        }
    }

    #[test]
    fn ids_sort_by_time_and_are_validated() {
        let a = new_id(Utc.with_ymd_and_hms(2026, 10, 3, 1, 2, 3).unwrap());
        let b = new_id(Utc.with_ymd_and_hms(2026, 10, 3, 11, 0, 0).unwrap());
        assert!(a.starts_with("20261003T010203Z-") && a.len() == 25, "{a}");
        assert!(a < b);
        assert!(valid_id(&a));
        for bad in [
            "../../meta/forklift",
            "20261003T010203Z-ABCDEF12",
            "20261003T010203Z",
            "x".repeat(25).as_str(),
        ] {
            assert!(!valid_id(bad), "{bad}");
        }
        assert_eq!(prefix("/team/"), "team/meta/migrations/");
    }

    #[test]
    fn checks_convert_with_their_status() {
        let c = Check::fail("lease", "held elsewhere").took(std::time::Duration::from_micros(12));
        let m = MigrationCheck::from(&c);
        assert_eq!(
            (m.id.as_str(), m.status.as_str(), m.latency_us),
            ("PF01", "fail", Some(12))
        );
    }

    #[test]
    fn records_round_trip_and_summarise() {
        let r = sample("20261003T010203Z-0123abcd", "failed");
        let back: MigrationRecord =
            serde_json::from_slice(&serde_json::to_vec(&r).unwrap()).unwrap();
        assert_eq!(back, r);
        let s = MigrationSummary::from(&r);
        assert_eq!(
            (s.outcome.as_str(), s.failed_stage.as_deref()),
            ("failed", Some("postflight"))
        );
    }

    #[tokio::test]
    async fn s3_history_lists_newest_first_and_gets_by_id() {
        use wiremock::matchers::{method, path, path_regex, query_param};
        use wiremock::{Mock, MockServer, ResponseTemplate};
        let _ = rustls::crypto::ring::default_provider().install_default();
        let server = MockServer::start().await;
        let list = r#"<?xml version="1.0" encoding="UTF-8"?>
<ListBucketResult><Name>b</Name><Prefix>meta/migrations/</Prefix><KeyCount>3</KeyCount><IsTruncated>false</IsTruncated>
<Contents><Key>meta/migrations/20261001T000000Z-aaaaaaaa.json</Key><Size>1</Size></Contents>
<Contents><Key>meta/migrations/20261003T010203Z-0123abcd.json</Key><Size>1</Size></Contents>
<Contents><Key>meta/migrations/notes.txt</Key><Size>1</Size></Contents>
</ListBucketResult>"#;
        Mock::given(method("GET"))
            .and(path_regex("^/b/?$"))
            .and(query_param("list-type", "2"))
            .respond_with(ResponseTemplate::new(200).set_body_string(list))
            .mount(&server)
            .await;
        let newer = sample("20261003T010203Z-0123abcd", "succeeded");
        let older = sample("20261001T000000Z-aaaaaaaa", "failed");
        for r in [&newer, &older] {
            Mock::given(method("GET"))
                .and(path(format!("/b/meta/migrations/{}.json", r.id)))
                .respond_with(ResponseTemplate::new(200).set_body_json(r))
                .mount(&server)
                .await;
        }
        Mock::given(method("GET"))
            .and(path("/b/meta/migrations/20261002T000000Z-bbbbbbbb.json"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&server)
            .await;
        let client = crate::storage::new_s3_client(&crate::storage::S3Config {
            bucket: "b".into(),
            region: "us-east-1".into(),
            endpoint: server.uri(),
            force_path_style: true,
            access_key_id: "a".into(),
            secret_access_key: "s".into(),
            ..Default::default()
        })
        .await
        .unwrap();
        let history = S3History::new(client, "b", "");
        let listed = history.list().await.unwrap();
        let ids: Vec<&str> = listed.iter().map(|s| s.id.as_str()).collect();
        assert_eq!(
            ids,
            ["20261003T010203Z-0123abcd", "20261001T000000Z-aaaaaaaa"]
        );
        let got = history.get(&newer.id).await.unwrap().unwrap();
        assert_eq!(
            got.stored_bytes,
            Some(serde_json::to_vec(&newer).unwrap().len() as u64)
        );
        assert_eq!(
            MigrationRecord {
                stored_bytes: None,
                ..got
            },
            newer
        );
        assert_eq!(
            history.get("20261002T000000Z-bbbbbbbb").await.unwrap(),
            None
        );
        assert_eq!(
            history.get("../meta/forklift").await.unwrap(),
            None,
            "invalid id never reaches S3"
        );
    }
}
