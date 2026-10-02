//! MinIO Admin API client, also used for RustFS, which serves the same API
//! under `/rustfs/admin`.
//!
//! There is no Rust madmin, so the request is issued directly with the same shape: service name
//! `s3`, an `X-Amz-Content-Sha256` of the empty body, and the endpoint's scheme selecting TLS.

use std::sync::Arc;
use std::time::SystemTime;

use async_trait::async_trait;
use aws_credential_types::Credentials;
use aws_sigv4::http_request::{
    PayloadChecksumKind, SignableBody, SignableRequest, SigningSettings, sign,
};
use aws_sigv4::sign::v4;
use serde::Deserialize;

use super::{
    AdminConfig, ClusterAdmin, ClusterInfo, ProviderSpec, base_url, decode, fetch, http_client,
};
use crate::storage::{Error, Result};

pub const MINIO: ProviderSpec = ProviderSpec {
    id: "minio",
    display_name: "MinIO",
    admin: Some(|cfg| Ok(Arc::new(MinioAdmin::new(cfg, &MINIO_API)?))),
};

pub const RUSTFS: ProviderSpec = ProviderSpec {
    id: "rustfs",
    display_name: "RustFS",
    admin: Some(|cfg| Ok(Arc::new(MinioAdmin::new(cfg, &RUSTFS_API)?))),
};

#[derive(Debug)]
pub struct AdminApi {
    /// `metrics=false` matches the default `ServerInfoOpts`, which forklift never overrode.
    info_path: &'static str,
    op: &'static str,
}

const MINIO_API: AdminApi = AdminApi {
    info_path: "/minio/admin/v3/info?metrics=false",
    op: "minio admin info",
};

const RUSTFS_API: AdminApi = AdminApi {
    info_path: "/rustfs/admin/v3/info?metrics=false",
    op: "rustfs admin info",
};

/// The region MinIO's own signature check falls back to when the deployment
/// has no region configured.
const DEFAULT_REGION: &str = "us-east-1";

/// Needs static keys: unlike the S3 data path the admin API cannot use the
/// ambient AWS credential chain (IRSA/pod identity).
pub struct MinioAdmin {
    url: String,
    op: &'static str,
    region: String,
    access_key_id: String,
    secret_access_key: String,
    client: reqwest::Client,
}

impl MinioAdmin {
    pub fn new(cfg: &AdminConfig, api: &AdminApi) -> Result<MinioAdmin> {
        if cfg.s3_endpoint.trim().is_empty() {
            return Err(Error::AdminUnavailable(
                "the admin api is served on the s3 endpoint, which is not set".into(),
            ));
        }
        if cfg.access_key_id.is_empty() || cfg.secret_access_key.is_empty() {
            return Err(Error::AdminUnavailable(
                "the admin api needs static s3 credentials".into(),
            ));
        }
        Ok(MinioAdmin {
            url: format!("{}{}", base_url(&cfg.s3_endpoint)?, api.info_path),
            op: api.op,
            region: if cfg.region.is_empty() {
                DEFAULT_REGION.to_string()
            } else {
                cfg.region.clone()
            },
            access_key_id: cfg.access_key_id.clone(),
            secret_access_key: cfg.secret_access_key.clone(),
            client: http_client()?,
        })
    }
}

#[async_trait]
impl ClusterAdmin for MinioAdmin {
    async fn cluster_info(&self) -> Result<ClusterInfo> {
        let request = sign_admin_request(
            &self.url,
            &self.access_key_id,
            &self.secret_access_key,
            &self.region,
        )?;
        let request =
            reqwest::Request::try_from(request).map_err(|e| Error::s3("admin request", e))?;
        let body = fetch(&self.client, request, self.op).await?;
        let reply: InfoReply = decode(&body, self.op)?;
        Ok(summarize(match reply {
            InfoReply::Wrapped { info } => info,
            InfoReply::Bare(info) => info,
        }))
    }
}

/// Builds the signed admin request.
fn sign_admin_request(
    url: &str,
    access_key_id: &str,
    secret_access_key: &str,
    region: &str,
) -> Result<http::Request<&'static [u8]>> {
    let identity = Credentials::from_keys(access_key_id, secret_access_key, None).into();
    let mut settings = SigningSettings::default();
    // madmin sets X-Amz-Content-Sha256 on every request, including this empty
    // GET body; MinIO's signature check includes the header.
    settings.payload_checksum_kind = PayloadChecksumKind::XAmzSha256;
    let params = v4::SigningParams::builder()
        .identity(&identity)
        .region(region)
        .name("s3")
        .time(SystemTime::now())
        .settings(settings)
        .build()
        .map_err(|e| Error::Other(format!("sign admin request: {e}")))?
        .into();

    let mut request = http::Request::builder()
        .method("GET")
        .uri(url)
        .body(b"".as_slice())
        .map_err(|e| Error::Other(format!("sign admin request: {e}")))?;
    let signable = SignableRequest::new(
        "GET",
        url,
        std::iter::empty(),
        SignableBody::Bytes(b"".as_slice()),
    )
    .map_err(|e| Error::Other(format!("sign admin request: {e}")))?;
    sign(signable, &params)
        .map_err(|e| Error::Other(format!("sign admin request: {e}")))?
        .into_parts()
        .0
        .apply_to_request_http1x(&mut request);
    Ok(request)
}

fn summarize(info: InfoMessage) -> ClusterInfo {
    let mut out = ClusterInfo {
        logical_used_bytes: Some(info.usage.size as i64),
        object_count: Some(info.objects.count as i64),
        bucket_count: Some(info.buckets.count as i64),
        online_drives: info.backend.online_disks,
        offline_drives: info.backend.offline_disks,
        servers: info.servers.len() as i64,
        ..ClusterInfo::default()
    };
    let (mut total, mut used, mut avail): (u64, u64, u64) = (0, 0, 0);
    let (mut online_from_drives, mut offline_from_drives) = (0i64, 0i64);
    for s in &info.servers {
        if out.version.is_empty() {
            out.version = s.version.clone();
        }
        for d in &s.drives {
            total += d.total_space;
            used += d.used_space;
            avail += d.available_space;
            if d.state.eq_ignore_ascii_case("ok") {
                online_from_drives += 1;
            } else if !d.state.is_empty() {
                offline_from_drives += 1;
            }
        }
    }
    out.set_capacity(total, used, avail);
    // The erasure backend summary reports 0 drives on non-erasure (FS/single)
    // deployments; fall back to the per-drive tally so the count is still shown.
    if out.online_drives == 0 && out.offline_drives == 0 {
        out.online_drives = online_from_drives;
        out.offline_drives = offline_from_drives;
    }
    out
}

/// RustFS wraps madmin's message as `{"info": …}`; MinIO sends it bare.
#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum InfoReply {
    Wrapped { info: InfoMessage },
    Bare(InfoMessage),
}

/// The subset of madmin's `InfoMessage` forklift reads. Field names are the
/// wire names MinIO emits.
#[derive(Debug, Default, Deserialize)]
struct InfoMessage {
    #[serde(default)]
    buckets: Counted,
    #[serde(default)]
    objects: Counted,
    #[serde(default)]
    usage: Usage,
    #[serde(default)]
    backend: ErasureBackend,
    #[serde(default)]
    servers: Vec<ServerProperties>,
}

/// madmin's `Buckets`/`Objects`, which are both a single count.
#[derive(Debug, Default, Deserialize)]
struct Counted {
    #[serde(default)]
    count: u64,
}

#[derive(Debug, Default, Deserialize)]
struct Usage {
    #[serde(default)]
    size: u64,
}

#[derive(Debug, Default, Deserialize)]
struct ErasureBackend {
    #[serde(rename = "onlineDisks", default)]
    online_disks: i64,
    #[serde(rename = "offlineDisks", default)]
    offline_disks: i64,
}

#[derive(Debug, Default, Deserialize)]
struct ServerProperties {
    #[serde(default)]
    version: String,
    /// madmin names the field `Disks` but tags it `drives` on the wire.
    #[serde(rename = "drives", default)]
    drives: Vec<Drive>,
}

#[derive(Debug, Default, Deserialize)]
struct Drive {
    #[serde(default)]
    state: String,
    #[serde(rename = "totalspace", default)]
    total_space: u64,
    #[serde(rename = "usedspace", default)]
    used_space: u64,
    #[serde(rename = "availspace", default)]
    available_space: u64,
}

#[cfg(test)]
pub(crate) mod tests {
    use serde_json::json;
    use wiremock::matchers::{method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use crate::storage::admin::minio::*;
    use crate::storage::admin::tests::{config, install_crypto_provider};

    fn admin(spec: &ProviderSpec, endpoint: &str) -> Arc<dyn ClusterAdmin> {
        let cfg = AdminConfig {
            s3_endpoint: endpoint.to_string(),
            access_key_id: "minioadmin".into(),
            secret_access_key: "minioadmin".into(),
            ..config()
        };
        (spec.admin.unwrap())(&cfg).unwrap()
    }

    /// The client must refuse to build (rather than dial) when the backend is
    /// not a credentialed endpoint, so the page can say what is missing.
    #[test]
    fn minio_admin_unavailable() {
        install_crypto_provider();
        let err = MinioAdmin::new(
            &AdminConfig {
                access_key_id: "a".into(),
                secret_access_key: "b".into(),
                ..config()
            },
            &MINIO_API,
        )
        .map(|_| ())
        .unwrap_err();
        assert!(
            matches!(&err, Error::AdminUnavailable(m) if m.contains("endpoint")),
            "empty endpoint: got {err:?}"
        );

        let err = MinioAdmin::new(
            &AdminConfig {
                s3_endpoint: "http://minio:9000".into(),
                ..config()
            },
            &MINIO_API,
        )
        .map(|_| ())
        .unwrap_err();
        assert!(
            matches!(&err, Error::AdminUnavailable(m) if m.contains("credentials")),
            "no creds: got {err:?}"
        );
    }

    /// The admin call madmin made: a SigV4-signed GET of
    /// `/minio/admin/v3/info?metrics=false`, whose JSON maps onto the fields the
    /// Storage page renders.
    #[tokio::test]
    async fn minio_admin_info_reads_cluster_stats() {
        install_crypto_provider();
        let server = MockServer::start().await;
        let body = json!({
            "buckets": {"count": 4},
            "objects": {"count": 1200},
            "usage": {"size": 900},
            "backend": {"backendType": "Erasure", "onlineDisks": 3, "offlineDisks": 1},
            "servers": [{
                "version": "2024.01.01",
                "drives": [
                    {"state": "ok", "totalspace": 1000, "usedspace": 400, "availspace": 600},
                    {"state": "offline", "totalspace": 1000, "usedspace": 100, "availspace": 900}
                ]
            }]
        });
        Mock::given(method("GET"))
            .and(path("/minio/admin/v3/info"))
            .and(query_param("metrics", "false"))
            .respond_with(ResponseTemplate::new(200).set_body_json(body))
            .expect(1)
            .mount(&server)
            .await;

        let admin = admin(&MINIO, &server.uri());
        let info = admin.cluster_info().await.expect("cluster_info");

        assert_eq!(info.bucket_count, Some(4));
        assert_eq!(info.object_count, Some(1200));
        assert_eq!(info.logical_used_bytes, Some(900));
        assert_eq!(info.total_capacity_bytes, 2000);
        assert_eq!(info.used_bytes, 500);
        assert_eq!(info.available_bytes, 1500);
        assert_eq!(info.usage_ratio, 0.25);
        // The erasure summary is non-zero, so the per-drive tally is not consulted.
        assert_eq!((info.online_drives, info.offline_drives), (3, 1));
        assert_eq!(info.servers, 1);
        assert_eq!(info.version, "2024.01.01");

        // The request must carry a SigV4 header signed for service "s3" in the
        // default region, plus the empty-body payload hash madmin always sent.
        let requests = server.received_requests().await.unwrap();
        assert_eq!(requests.len(), 1);
        let auth = requests[0]
            .headers
            .get("authorization")
            .expect("Authorization header")
            .to_str()
            .unwrap();
        assert!(
            auth.starts_with("AWS4-HMAC-SHA256 Credential=minioadmin/"),
            "authorization = {auth}"
        );
        assert!(
            auth.contains("/us-east-1/s3/aws4_request"),
            "authorization scope = {auth}"
        );
        assert!(auth.contains("SignedHeaders="), "authorization = {auth}");
        assert!(auth.contains("Signature="), "authorization = {auth}");
        assert!(
            requests[0].headers.contains_key("x-amz-content-sha256"),
            "missing X-Amz-Content-Sha256"
        );
    }

    /// RustFS serves the same API under its own path prefix.
    #[tokio::test]
    async fn rustfs_admin_info_uses_rustfs_path() {
        install_crypto_provider();
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/rustfs/admin/v3/info"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "info": {
                    "buckets": {"count": 3, "error": null},
                    "objects": {"count": 7, "error": null},
                    "usage": {"size": 70, "error": null},
                    "backend": {"backendType": "Erasure", "onlineDisks": 1, "offlineDisks": 0},
                    "servers": [{"version": "2026-09-16T10:00:00+00:00@1.0.0", "drives": [
                        {"state": "ok", "totalspace": 100, "usedspace": 10, "availspace": 90}
                    ]}]
                },
                "admin_discovery": {},
                "bitrotSelftest": "passed"
            })))
            .expect(1)
            .mount(&server)
            .await;

        let admin = admin(&RUSTFS, &server.uri());
        let info = admin.cluster_info().await.expect("cluster_info");
        assert_eq!(info.version, "2026-09-16T10:00:00+00:00@1.0.0");
        assert_eq!((info.online_drives, info.offline_drives), (1, 0));
        assert_eq!(info.bucket_count, Some(3));
        assert_eq!(info.object_count, Some(7));
        assert_eq!(info.total_capacity_bytes, 100);
    }

    /// A non-2xx admin response surfaces the server's message instead of a
    /// zero-valued snapshot.
    #[tokio::test]
    async fn minio_admin_info_reports_http_errors() {
        install_crypto_provider();
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/minio/admin/v3/info"))
            .respond_with(ResponseTemplate::new(403).set_body_string("access denied"))
            .mount(&server)
            .await;

        let admin = admin(&MINIO, &server.uri());
        let err = admin.cluster_info().await.unwrap_err().to_string();
        assert!(err.contains("minio admin info: 403"), "err = {err}");
        assert!(err.contains("access denied"), "err = {err}");
    }

    /// A non-erasure (FS/single) deployment reports 0 drives in the backend
    /// summary; the per-drive tally fills the count in.
    #[tokio::test]
    async fn minio_admin_info_falls_back_to_drive_tally() {
        install_crypto_provider();
        let server = MockServer::start().await;
        let body = json!({
            "backend": {"backendType": "FS", "onlineDisks": 0, "offlineDisks": 0},
            "servers": [{
                "version": "2024.01.01",
                "drives": [
                    {"state": "OK", "totalspace": 10, "usedspace": 1, "availspace": 9},
                    {"state": "offline"},
                    {"state": ""}
                ]
            }]
        });
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200).set_body_json(body))
            .mount(&server)
            .await;

        let admin = admin(&MINIO, &server.uri());
        let info = admin.cluster_info().await.unwrap();
        // "OK" matches case-insensitively; the empty state counts as neither.
        assert_eq!((info.online_drives, info.offline_drives), (1, 1));
    }
}
