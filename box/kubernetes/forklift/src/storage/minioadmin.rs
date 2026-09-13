//! MinIO Admin API client for the Storage admin page.
//!
//! There is no Rust madmin, so the request is issued directly with the same shape: service name
//! `s3`, an `X-Amz-Content-Sha256` of the empty body, and the endpoint's scheme selecting TLS.

use std::time::SystemTime;

use aws_credential_types::Credentials;
use aws_sigv4::http_request::{
    PayloadChecksumKind, SignableBody, SignableRequest, SigningSettings, sign,
};
use aws_sigv4::sign::v4;
use serde::Deserialize;

use super::{Error, Result, S3Config};

/// `metrics=false` matches the default `ServerInfoOpts`, which forklift never overrode.
const ADMIN_INFO_PATH: &str = "/minio/admin/v3/info?metrics=false";

/// The region MinIO's own signature check falls back to when the deployment
/// has no region configured.
const DEFAULT_REGION: &str = "us-east-1";

/// A snapshot of a MinIO cluster's operational metadata, gathered from the
/// MinIO Admin API. It surfaces cluster capacity/usage, object and bucket
/// counts, and drive health for the Storage admin page. Only populated when the
/// object-storage backend points at a MinIO endpoint with static admin
/// credentials.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct MinIOInfo {
    /// Raw drive totals summed across every server's drives (raw, before
    /// erasure parity).
    pub total_capacity_bytes: i64,
    pub used_bytes: i64,
    pub available_bytes: i64,
    /// `used_bytes / total_capacity_bytes` in [0,1]; 0 when capacity is 0.
    pub usage_ratio: f64,
    /// The logical size of stored objects (`Usage.Size`), which differs from
    /// raw `used_bytes` because of erasure coding and overhead.
    pub logical_used_bytes: i64,
    pub object_count: i64,
    pub bucket_count: i64,
    pub online_drives: i64,
    pub offline_drives: i64,
    pub servers: i64,
    pub version: String,
}

/// Queries the MinIO Admin API for cluster operational metadata. It requires an
/// explicit S3 endpoint (MinIO, not AWS S3) and static credentials; without
/// them it returns [`Error::MinIOUnavailable`] so the caller can fall back to
/// the metadata-derived stats. The endpoint's scheme selects TLS.
pub async fn minio_admin_info(cfg: &S3Config) -> Result<MinIOInfo> {
    if cfg.endpoint.trim().is_empty() {
        return Err(Error::MinIOUnavailable);
    }
    if cfg.access_key_id.is_empty() || cfg.secret_access_key.is_empty() {
        // The admin API needs explicit keys: unlike the S3 data path it cannot
        // use the ambient AWS credential chain (IRSA/pod identity).
        return Err(Error::MinIOUnavailable);
    }
    let (host, secure) = parse_admin_endpoint(&cfg.endpoint)?;
    let scheme = if secure { "https" } else { "http" };
    let url = format!("{scheme}://{host}{ADMIN_INFO_PATH}");

    let region = if cfg.region.is_empty() {
        DEFAULT_REGION
    } else {
        cfg.region.as_str()
    };
    let request = sign_admin_request(&url, cfg, region)?;
    // The process-wide rustls provider is installed once at startup by the
    // server binary; building the client here only reads it.
    let client = reqwest::Client::builder()
        .build()
        .map_err(|e| Error::s3("minio admin client", e))?;
    let resp = client
        .execute(reqwest::Request::try_from(request).map_err(|e| Error::s3("admin request", e))?)
        .await
        .map_err(|e| Error::s3("minio admin info", e))?;
    let status = resp.status();
    let body = resp
        .bytes()
        .await
        .map_err(|e| Error::s3("minio admin info", e))?;
    if !status.is_success() {
        // madmin decodes the JSON/XML error body; the message is all the caller
        // surfaces, so keep it verbatim and bounded the way madmin bounds it.
        let mut text = String::from_utf8_lossy(&body).into_owned();
        if text.len() > 1024 {
            text.truncate(1021);
            text.push_str("...");
        }
        return Err(Error::Other(format!(
            "minio admin info: {}: {text}",
            status.as_u16()
        )));
    }
    let info: InfoMessage = serde_json::from_slice(&body)
        .map_err(|e| Error::Other(format!("minio admin info: {e}")))?;
    Ok(summarize(info))
}

/// Builds the signed admin request.
fn sign_admin_request(
    url: &str,
    cfg: &S3Config,
    region: &str,
) -> Result<http::Request<&'static [u8]>> {
    let identity = Credentials::from_keys(&cfg.access_key_id, &cfg.secret_access_key, None).into();
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

/// Folds the admin API response into the fields the Storage page renders.
fn summarize(info: InfoMessage) -> MinIOInfo {
    let mut out = MinIOInfo {
        logical_used_bytes: info.usage.size as i64,
        object_count: info.objects.count as i64,
        bucket_count: info.buckets.count as i64,
        online_drives: info.backend.online_disks,
        offline_drives: info.backend.offline_disks,
        servers: info.servers.len() as i64,
        ..MinIOInfo::default()
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
    out.total_capacity_bytes = total as i64;
    out.used_bytes = used as i64;
    out.available_bytes = avail as i64;
    if total > 0 {
        out.usage_ratio = used as f64 / total as f64;
    }
    // The erasure backend summary reports 0 drives on non-erasure (FS/single)
    // deployments; fall back to the per-drive tally so the count is still shown.
    if out.online_drives == 0 && out.offline_drives == 0 {
        out.online_drives = online_from_drives;
        out.offline_drives = offline_from_drives;
    }
    out
}

/// Splits an S3 endpoint URL into the `host:port` and TLS flag the admin API
/// call needs. A bare host (no scheme) is treated as non-TLS.
pub(crate) fn parse_admin_endpoint(endpoint: &str) -> Result<(String, bool)> {
    let e = endpoint.trim();
    if !e.contains("://") {
        return Ok((e.trim_end_matches('/').to_string(), false));
    }
    let u = url::Url::parse(e).map_err(|err| Error::Other(format!("parse endpoint: {err}")))?;
    let host = match (u.host_str(), u.port()) {
        (Some(h), Some(p)) => format!("{h}:{p}"),
        (Some(h), None) => h.to_string(),
        (None, _) => String::new(),
    };
    Ok((host, u.scheme() == "https"))
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
    use crate::storage::minioadmin::*;
    use serde_json::json;
    use wiremock::matchers::{method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    /// The server binary installs the process-wide rustls crypto provider at
    /// startup; a test that builds an HTTP client must do it itself first.
    fn install_crypto_provider() {
        let _ = rustls::crypto::ring::default_provider().install_default();
    }

    #[test]
    fn parse_admin_endpoint_cases() {
        for (input, want_host, want_secure) in [
            ("http://minio:9000", "minio:9000", false),
            ("https://s3.example.com", "s3.example.com", true),
            (
                "https://minio.example.com:9000/",
                "minio.example.com:9000",
                true,
            ),
            ("minio:9000", "minio:9000", false),
            ("minio:9000/", "minio:9000", false),
        ] {
            let (host, secure) = parse_admin_endpoint(input).expect(input);
            assert_eq!(
                (host.as_str(), secure),
                (want_host, want_secure),
                "input {input:?}"
            );
        }
    }

    /// `minio_admin_info` must refuse to run (rather than dial) when the backend is
    /// not a credentialed MinIO endpoint, so the caller can fall back to
    /// DB-derived stats.
    #[tokio::test]
    async fn minio_admin_info_unavailable() {
        install_crypto_provider();
        // No endpoint (plain AWS S3 or fs).
        let err = minio_admin_info(&S3Config {
            access_key_id: "a".into(),
            secret_access_key: "b".into(),
            ..S3Config::default()
        })
        .await
        .unwrap_err();
        assert!(
            matches!(err, Error::MinIOUnavailable),
            "empty endpoint: got {err:?}"
        );

        // Endpoint but no static credentials (IRSA/pod identity).
        let err = minio_admin_info(&S3Config {
            endpoint: "http://minio:9000".into(),
            ..S3Config::default()
        })
        .await
        .unwrap_err();
        assert!(
            matches!(err, Error::MinIOUnavailable),
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
                "version": "RELEASE.2024-01-01T00-00-00Z",
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

        let info = minio_admin_info(&S3Config {
            endpoint: server.uri(),
            access_key_id: "minioadmin".into(),
            secret_access_key: "minioadmin".into(),
            ..S3Config::default()
        })
        .await
        .expect("minio_admin_info");

        assert_eq!(info.bucket_count, 4);
        assert_eq!(info.object_count, 1200);
        assert_eq!(info.logical_used_bytes, 900);
        assert_eq!(info.total_capacity_bytes, 2000);
        assert_eq!(info.used_bytes, 500);
        assert_eq!(info.available_bytes, 1500);
        assert_eq!(info.usage_ratio, 0.25);
        // The erasure summary is non-zero, so the per-drive tally is not consulted.
        assert_eq!((info.online_drives, info.offline_drives), (3, 1));
        assert_eq!(info.servers, 1);
        assert_eq!(info.version, "RELEASE.2024-01-01T00-00-00Z");

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

        let err = minio_admin_info(&S3Config {
            endpoint: server.uri(),
            access_key_id: "a".into(),
            secret_access_key: "b".into(),
            ..S3Config::default()
        })
        .await
        .unwrap_err();
        assert!(err.to_string().contains("403"), "err = {err}");
        assert!(err.to_string().contains("access denied"), "err = {err}");
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
                "version": "RELEASE.2024-01-01T00-00-00Z",
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

        let info = minio_admin_info(&S3Config {
            endpoint: server.uri(),
            access_key_id: "a".into(),
            secret_access_key: "b".into(),
            ..S3Config::default()
        })
        .await
        .unwrap();
        // "OK" matches case-insensitively; the empty state counts as neither.
        assert_eq!((info.online_drives, info.offline_drives), (1, 1));
    }
}
