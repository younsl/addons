//! Cluster admin APIs of S3-compatible object stores, normalised into
//! [`ClusterInfo`] for the Storage admin page.

use std::sync::Arc;

use async_trait::async_trait;

use super::{Error, Result};

pub mod garage;
pub mod minio;
pub mod seaweedfs;

pub type AdminFactory = fn(&AdminConfig) -> Result<Arc<dyn ClusterAdmin>>;

/// Adding a store means a submodule with its [`ClusterAdmin`] and one entry in
/// [`PROVIDERS`]; configuration, validation and the Storage page read the table.
#[derive(Debug)]
pub struct ProviderSpec {
    pub id: &'static str,
    pub display_name: &'static str,
    pub admin: Option<AdminFactory>,
}

pub const AWS: ProviderSpec = ProviderSpec {
    id: "aws",
    display_name: "Amazon S3",
    admin: None,
};

pub const GENERIC: ProviderSpec = ProviderSpec {
    id: "generic",
    display_name: "S3-compatible",
    admin: None,
};

pub static PROVIDERS: &[ProviderSpec] = &[
    AWS,
    minio::MINIO,
    minio::RUSTFS,
    seaweedfs::SEAWEEDFS,
    garage::GARAGE,
    GENERIC,
];

pub fn provider(id: &str) -> Option<&'static ProviderSpec> {
    PROVIDERS.iter().find(|p| p.id == id)
}

pub fn provider_ids() -> Vec<&'static str> {
    PROVIDERS.iter().map(|p| p.id).collect()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdminConfig {
    pub s3_endpoint: String,
    /// Used when the admin API is not served on the S3 endpoint.
    pub admin_endpoint: String,
    pub region: String,
    pub access_key_id: String,
    pub secret_access_key: String,
    pub admin_token: String,
}

/// Counts a store does not report are `None`, so the page can tell "unknown"
/// from zero.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ClusterInfo {
    /// Raw capacity across every storage unit, before replication or parity.
    pub total_capacity_bytes: i64,
    pub used_bytes: i64,
    pub available_bytes: i64,
    pub usage_ratio: f64,
    pub logical_used_bytes: Option<i64>,
    pub object_count: Option<i64>,
    pub bucket_count: Option<i64>,
    /// Storage units: drives (MinIO, RustFS), volume-server disks
    /// (SeaweedFS) or storage nodes (Garage).
    pub online_drives: i64,
    pub offline_drives: i64,
    pub servers: i64,
    pub version: String,
}

impl ClusterInfo {
    pub(crate) fn set_capacity(&mut self, total: u64, used: u64, available: u64) {
        self.total_capacity_bytes = total as i64;
        self.used_bytes = used as i64;
        self.available_bytes = available as i64;
        self.usage_ratio = if total > 0 {
            used as f64 / total as f64
        } else {
            0.0
        };
    }
}

#[async_trait]
pub trait ClusterAdmin: Send + Sync {
    async fn cluster_info(&self) -> Result<ClusterInfo>;
}

/// `Ok(None)` for a provider without an admin API.
pub fn cluster_admin(
    spec: &ProviderSpec,
    cfg: &AdminConfig,
) -> Result<Option<Arc<dyn ClusterAdmin>>> {
    spec.admin.map(|build| build(cfg)).transpose()
}

/// A bare host (no scheme) is treated as non-TLS.
pub(crate) fn parse_endpoint(endpoint: &str) -> Result<(String, bool)> {
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

pub(crate) fn base_url(endpoint: &str) -> Result<String> {
    let (host, secure) = parse_endpoint(endpoint)?;
    if host.is_empty() {
        return Err(Error::Other(format!("endpoint {endpoint:?} has no host")));
    }
    let scheme = if secure { "https" } else { "http" };
    Ok(format!("{scheme}://{host}"))
}

/// Non-2xx responses become an error carrying the status and a bounded body.
pub(crate) async fn fetch(
    client: &reqwest::Client,
    req: reqwest::Request,
    op: &'static str,
) -> Result<bytes::Bytes> {
    let resp = client.execute(req).await.map_err(|e| Error::s3(op, e))?;
    let status = resp.status();
    let body = resp.bytes().await.map_err(|e| Error::s3(op, e))?;
    if !status.is_success() {
        let mut text = String::from_utf8_lossy(&body).into_owned();
        if text.len() > 1024 {
            let mut cut = 1021;
            while !text.is_char_boundary(cut) {
                cut -= 1;
            }
            text.truncate(cut);
            text.push_str("...");
        }
        return Err(Error::Other(format!("{op}: {}: {text}", status.as_u16())));
    }
    Ok(body)
}

pub(crate) fn decode<T: serde::de::DeserializeOwned>(body: &[u8], op: &'static str) -> Result<T> {
    serde_json::from_slice(body).map_err(|e| Error::Other(format!("{op}: {e}")))
}

/// The process-wide rustls provider is installed once at startup by the server
/// binary; building the client here only reads it.
pub(crate) fn http_client() -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|e| Error::s3("admin client", e))
}

#[cfg(test)]
pub(crate) mod tests {
    use crate::storage::admin::*;

    /// The server binary installs the process-wide rustls crypto provider at
    /// startup; a test that builds an HTTP client must do it itself first.
    pub(crate) fn install_crypto_provider() {
        let _ = rustls::crypto::ring::default_provider().install_default();
    }

    pub(crate) fn config() -> AdminConfig {
        AdminConfig {
            s3_endpoint: String::new(),
            admin_endpoint: String::new(),
            region: String::new(),
            access_key_id: String::new(),
            secret_access_key: String::new(),
            admin_token: String::new(),
        }
    }

    #[test]
    fn registry_ids_are_unique_and_resolvable() {
        let ids = provider_ids();
        let mut sorted = ids.clone();
        sorted.sort();
        sorted.dedup();
        assert_eq!(sorted.len(), ids.len(), "duplicate provider id in {ids:?}");
        for id in ids {
            assert_eq!(provider(id).map(|p| p.id), Some(id));
        }
        assert!(provider("ceph").is_none());
    }

    #[test]
    fn parse_endpoint_cases() {
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
            let (host, secure) = parse_endpoint(input).expect(input);
            assert_eq!(
                (host.as_str(), secure),
                (want_host, want_secure),
                "input {input:?}"
            );
        }
        assert_eq!(
            base_url("https://a.example.com:9/x").unwrap(),
            "https://a.example.com:9"
        );
        assert!(base_url("").is_err());
    }

    #[test]
    fn providers_without_admin_api_yield_none() {
        for spec in [&AWS, &GENERIC] {
            assert!(
                cluster_admin(spec, &config()).unwrap().is_none(),
                "{}",
                spec.id
            );
        }
    }

    #[test]
    fn missing_settings_are_reported() {
        install_crypto_provider();
        for spec in PROVIDERS.iter().filter(|p| p.admin.is_some()) {
            let err = cluster_admin(spec, &config()).map(|_| ()).unwrap_err();
            assert!(
                matches!(err, Error::AdminUnavailable(_)),
                "{}: {err:?}",
                spec.id
            );
        }
    }

    #[test]
    fn set_capacity_derives_ratio() {
        let mut info = ClusterInfo::default();
        info.set_capacity(200, 50, 150);
        assert_eq!(info.usage_ratio, 0.25);
        info.set_capacity(0, 0, 0);
        assert_eq!(info.usage_ratio, 0.0);
    }

    #[tokio::test]
    async fn fetch_bounds_error_bodies() {
        install_crypto_provider();
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .respond_with(wiremock::ResponseTemplate::new(500).set_body_string("é".repeat(800)))
            .mount(&server)
            .await;
        let client = http_client().unwrap();
        let req = client.get(server.uri()).build().unwrap();
        let err = fetch(&client, req, "op").await.unwrap_err().to_string();
        assert!(err.starts_with("op: 500: "), "err = {err}");
        assert!(err.ends_with("..."), "err = {err}");
    }
}
