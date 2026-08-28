//! The one question the Kubernetes API cannot answer: which images a pending
//! sync would actually deploy.
//!
//! The desired manifests live in git, so they are only reachable through Argo
//! CD's own cached comparison. `managed-resources` serves that cache without
//! forcing a repo-server render, and the query is narrowed to workload kinds so
//! the payload stays small enough to answer inside an admission timeout.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use futures_util::future::join_all;
use serde::Deserialize;
use thiserror::Error;

use crate::config::ArgoCd;
use crate::gate::{ImageRef, extract_images};

/// The shape of `GET /api/v1/applications/{name}/managed-resources`. Argo CD
/// serializes the nested manifests as JSON strings rather than objects.
#[derive(Debug, Deserialize)]
struct ManagedResources {
    #[serde(default)]
    items: Vec<ManagedResourceItem>,
}

/// One entry of the managed-resources response.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ManagedResourceItem {
    /// The desired manifest, empty for a resource that only exists live.
    #[serde(default)]
    pub target_state: String,
}

/// The shape of `GET /api/v1/session/userinfo`.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct UserInfo {
    #[serde(default)]
    logged_in: bool,
    #[serde(default)]
    username: String,
}

/// Why a desired image lookup failed.
#[derive(Debug, Error)]
pub enum ApiError {
    #[error("read argocd ca bundle {path}: {source}")]
    ReadCa {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("argocd ca bundle {path} contains no certificate")]
    EmptyCa { path: String },
    #[error("build argocd http client: {0}")]
    Client(#[source] reqwest::Error),
    #[error("no argocd api token is mounted at {0}")]
    NoToken(String),
    #[error("call argocd api: {0}")]
    Call(#[source] reqwest::Error),
    #[error("argocd api returned {status} for {endpoint}")]
    Status { status: u16, endpoint: &'static str },
    #[error("decode {endpoint}: {source}")]
    Decode {
        endpoint: &'static str,
        #[source]
        source: reqwest::Error,
    },
    #[error("argocd api reports the mounted token is not logged in")]
    NotLoggedIn,
    #[error("kind {kind}: {source}")]
    Kind {
        kind: String,
        #[source]
        source: Box<Self>,
    },
}

/// Resolves the images a pending sync would deploy.
#[async_trait]
pub trait ImageResolver: Send + Sync {
    async fn desired_images(&self, app: &str) -> Result<Vec<ImageRef>, ApiError>;
}

#[derive(Debug)]
struct CacheEntry {
    fetched_at: Instant,
    images: Vec<ImageRef>,
}

/// The argocd-server client behind [`ImageResolver`].
#[derive(Debug)]
pub struct DesiredImageClient {
    http: reqwest::Client,
    base: String,
    token_path: String,
    kinds: Vec<String>,
    cache_ttl: Duration,
    cache: Mutex<HashMap<String, CacheEntry>>,
}

impl DesiredImageClient {
    /// Builds the client. The API token is not read here but on every lookup,
    /// so a missing one is not fatal at construction time: the pod may start
    /// before the Secret is projected, and the configured onError policy
    /// decides what a failed lookup means until it arrives.
    pub fn new(cfg: &ArgoCd, kinds: &[String]) -> Result<Self, ApiError> {
        // reqwest is built without a bundled provider so the whole process
        // shares aws-lc-rs. Installing is idempotent, so the first caller wins.
        let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
        let mut builder = reqwest::Client::builder()
            .timeout(Duration::from_secs(
                cfg.timeout_seconds.max(0).unsigned_abs(),
            ))
            .min_tls_version(reqwest::tls::Version::TLS_1_2)
            // Operators opt into this explicitly. The default is verification
            // with the CA file.
            .danger_accept_invalid_certs(cfg.insecure_skip_verify);

        if !cfg.ca_file.is_empty() && !cfg.insecure_skip_verify {
            let pem = std::fs::read(&cfg.ca_file).map_err(|source| ApiError::ReadCa {
                path: cfg.ca_file.clone(),
                source,
            })?;
            let certs = reqwest::Certificate::from_pem_bundle(&pem).map_err(ApiError::Client)?;
            if certs.is_empty() {
                return Err(ApiError::EmptyCa {
                    path: cfg.ca_file.clone(),
                });
            }
            for cert in certs {
                builder = builder.add_root_certificate(cert);
            }
        }

        Ok(Self {
            http: builder.build().map_err(ApiError::Client)?,
            base: cfg.server_address.trim_end_matches('/').to_string(),
            token_path: cfg.token_path.clone(),
            kinds: kinds.to_vec(),
            cache_ttl: Duration::from_secs(cfg.cache_ttl_seconds.max(0).unsigned_abs()),
            cache: Mutex::new(HashMap::new()),
        })
    }

    /// Reads the mounted credential on every call rather than caching the one
    /// present at startup, so a rotated Secret takes effect without a restart.
    ///
    /// That matters more than the read costs: imageTag.onError defaults to
    /// deny, so a credential the process can no longer use does not degrade the
    /// gate, it closes it on every gated application at once.
    fn token(&self) -> String {
        std::fs::read_to_string(&self.token_path)
            .map(|raw| raw.trim().to_string())
            .unwrap_or_default()
    }

    /// Reports whether a token is mounted right now, so startup can warn
    /// instead of failing every lookup silently.
    #[must_use]
    pub fn has_token(&self) -> bool {
        !self.token().is_empty()
    }

    /// Reports whether argocd-server accepts the mounted token right now.
    ///
    /// `has_token` only proves a file exists, so a revoked or expired token
    /// starts up clean and surfaces as a denied production sync instead. Asking
    /// argocd-server moves that to deploy time. It stays a report: readiness
    /// must not depend on it, or this webhook's availability becomes
    /// argocd-server's.
    pub async fn probe(&self) -> Result<String, ApiError> {
        const ENDPOINT: &str = "session/userinfo";
        let token = self.token();
        if token.is_empty() {
            return Err(ApiError::NoToken(self.token_path.clone()));
        }
        let resp = self
            .http
            .get(format!("{}/api/v1/session/userinfo", self.base))
            .bearer_auth(&token)
            .header(reqwest::header::ACCEPT, "application/json")
            .send()
            .await
            .map_err(ApiError::Call)?;
        if !resp.status().is_success() {
            return Err(ApiError::Status {
                status: resp.status().as_u16(),
                endpoint: ENDPOINT,
            });
        }
        let body: UserInfo = resp.json().await.map_err(|source| ApiError::Decode {
            endpoint: ENDPOINT,
            source,
        })?;
        if !body.logged_in {
            return Err(ApiError::NotLoggedIn);
        }
        Ok(body.username)
    }

    async fn fetch_kind(
        &self,
        app: &str,
        kind: &str,
        token: &str,
    ) -> Result<Vec<ImageRef>, ApiError> {
        const ENDPOINT: &str = "managed-resources";
        let resp = self
            .http
            .get(format!(
                "{}/api/v1/applications/{app}/managed-resources",
                self.base
            ))
            .query(&[("kind", kind)])
            .bearer_auth(token)
            .header(reqwest::header::ACCEPT, "application/json")
            .send()
            .await
            .map_err(ApiError::Call)?;
        if !resp.status().is_success() {
            return Err(ApiError::Status {
                status: resp.status().as_u16(),
                endpoint: ENDPOINT,
            });
        }
        let body: ManagedResources = resp.json().await.map_err(|source| ApiError::Decode {
            endpoint: ENDPOINT,
            source,
        })?;
        Ok(images_from_managed_resources(&body.items))
    }

    fn cached(&self, app: &str) -> Option<Vec<ImageRef>> {
        if self.cache_ttl.is_zero() {
            return None;
        }
        let cache = self.cache.lock().ok()?;
        let entry = cache.get(app)?;
        if entry.fetched_at.elapsed() >= self.cache_ttl {
            return None;
        }
        let images = entry.images.clone();
        drop(cache);
        Some(images)
    }

    fn store(&self, app: &str, images: &[ImageRef]) {
        if self.cache_ttl.is_zero() {
            return;
        }
        if let Ok(mut cache) = self.cache.lock() {
            cache.insert(
                app.to_string(),
                CacheEntry {
                    fetched_at: Instant::now(),
                    images: images.to_vec(),
                },
            );
        }
    }
}

#[async_trait]
impl ImageResolver for DesiredImageClient {
    async fn desired_images(&self, app: &str) -> Result<Vec<ImageRef>, ApiError> {
        if let Some(images) = self.cached(app) {
            return Ok(images);
        }
        // Read once for the whole lookup. Re-reading per kind would let a
        // rotation land mid-flight and answer one question with two different
        // credentials.
        let token = self.token();
        if token.is_empty() {
            return Err(ApiError::NoToken(self.token_path.clone()));
        }

        let results = join_all(
            self.kinds
                .iter()
                .map(|kind| self.fetch_kind(app, kind, &token)),
        )
        .await;

        // A partial answer is worse than none: the kind that failed to load
        // could be exactly the workload whose tag differs.
        let mut images = Vec::new();
        for (kind, result) in self.kinds.iter().zip(results) {
            match result {
                Ok(found) => images.extend(found),
                Err(source) => {
                    return Err(ApiError::Kind {
                        kind: kind.clone(),
                        source: Box::new(source),
                    });
                }
            }
        }

        self.store(app, &images);
        Ok(images)
    }
}

/// Extracts container images from the desired manifests of a managed-resources
/// response.
#[must_use]
pub fn images_from_managed_resources(items: &[ManagedResourceItem]) -> Vec<ImageRef> {
    items
        .iter()
        .filter(|item| !item.target_state.trim().is_empty())
        // A manifest Argo CD could render but this gate cannot parse is not
        // worth failing the whole lookup over. The remaining kinds still
        // produce a comparison.
        .filter_map(|item| serde_json::from_str::<serde_json::Value>(&item.target_state).ok())
        .flat_map(|manifest| extract_images(&manifest))
        .collect()
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use wiremock::matchers::{header, method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;

    fn item(manifest: &str) -> ManagedResourceItem {
        ManagedResourceItem {
            target_state: manifest.to_string(),
        }
    }

    fn client_for(
        server: &MockServer,
        token_dir: &tempfile::TempDir,
        ttl: i64,
    ) -> DesiredImageClient {
        let token_path = token_dir.path().join("token");
        let mut f = std::fs::File::create(&token_path).unwrap();
        f.write_all(b"secret-token\n").unwrap();
        let cfg = ArgoCd {
            server_address: format!("{}/", server.uri()),
            ca_file: String::new(),
            token_path: token_path.display().to_string(),
            timeout_seconds: 2,
            cache_ttl_seconds: ttl,
            ..ArgoCd::default()
        };
        DesiredImageClient::new(&cfg, &["Deployment".to_string(), "Rollout".to_string()]).unwrap()
    }

    fn deployment(image: &str) -> String {
        serde_json::json!({
            "items": [{"targetState": format!(r#"{{"spec":{{"template":{{"spec":{{"containers":[{{"image":"{image}"}}]}}}}}}}}"#)}]
        })
        .to_string()
    }

    #[test]
    fn images_from_managed_resources_skips_blank_and_unparsable() {
        let items = vec![
            item(""),
            item("not json"),
            item(r#"{"spec":{"containers":[{"image":"a/b:1"}]}}"#),
        ];
        let images = images_from_managed_resources(&items);
        assert_eq!(images.len(), 1);
        assert_eq!(images[0].tag, "1");
    }

    #[tokio::test]
    async fn desired_images_merges_kinds_and_caches() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v1/applications/prd-api/managed-resources"))
            .and(query_param("kind", "Deployment"))
            .and(header("Authorization", "Bearer secret-token"))
            .respond_with(ResponseTemplate::new(200).set_body_string(deployment("r/api:1")))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/v1/applications/prd-api/managed-resources"))
            .and(query_param("kind", "Rollout"))
            .respond_with(ResponseTemplate::new(200).set_body_string(deployment("r/worker:2")))
            .expect(1)
            .mount(&server)
            .await;
        let dir = tempfile::tempdir().unwrap();
        let client = client_for(&server, &dir, 60);
        assert!(client.has_token());

        let first = client.desired_images("prd-api").await.unwrap();
        assert_eq!(first.len(), 2);
        let second = client.desired_images("prd-api").await.unwrap();
        assert_eq!(first, second, "second call is served from the cache");
    }

    #[tokio::test]
    async fn desired_images_fails_whole_lookup_on_partial_failure() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(query_param("kind", "Deployment"))
            .respond_with(ResponseTemplate::new(200).set_body_string(deployment("r/api:1")))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(query_param("kind", "Rollout"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;
        let dir = tempfile::tempdir().unwrap();
        let client = client_for(&server, &dir, 0);
        let err = client.desired_images("prd-api").await.unwrap_err();
        assert!(
            matches!(err, ApiError::Kind { ref kind, .. } if kind == "Rollout"),
            "{err}"
        );
        assert!(
            err.to_string()
                .contains("returned 500 for managed-resources"),
            "{err}"
        );
        assert!(client.cached("prd-api").is_none(), "ttl 0 never caches");
    }

    #[tokio::test]
    async fn desired_images_decode_failure_and_missing_token() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200).set_body_string("nope"))
            .mount(&server)
            .await;
        let dir = tempfile::tempdir().unwrap();
        let client = client_for(&server, &dir, 60);
        let err = client.desired_images("prd-api").await.unwrap_err();
        assert!(
            err.to_string().contains("decode managed-resources"),
            "{err}"
        );

        std::fs::remove_file(dir.path().join("token")).unwrap();
        assert!(!client.has_token());
        let err = client.desired_images("prd-api").await.unwrap_err();
        assert!(matches!(err, ApiError::NoToken(_)), "{err}");
        let err = client.probe().await.unwrap_err();
        assert!(matches!(err, ApiError::NoToken(_)), "{err}");
    }

    #[tokio::test]
    async fn probe_reports_account_and_failures() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v1/session/userinfo"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_string(r#"{"loggedIn":true,"username":"gate"}"#),
            )
            .up_to_n_times(1)
            .mount(&server)
            .await;
        let dir = tempfile::tempdir().unwrap();
        let client = client_for(&server, &dir, 0);
        assert_eq!(client.probe().await.unwrap(), "gate");

        server.reset().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"loggedIn":false}"#))
            .mount(&server)
            .await;
        assert!(matches!(
            client.probe().await.unwrap_err(),
            ApiError::NotLoggedIn
        ));

        server.reset().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(401))
            .mount(&server)
            .await;
        let err = client.probe().await.unwrap_err();
        assert!(
            err.to_string().contains("401 for session/userinfo"),
            "{err}"
        );

        server.reset().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200).set_body_string("{"))
            .mount(&server)
            .await;
        let err = client.probe().await.unwrap_err();
        assert!(err.to_string().contains("decode session/userinfo"), "{err}");
    }

    #[tokio::test]
    async fn connection_failure_is_a_call_error() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("token"), "t").unwrap();
        let cfg = ArgoCd {
            server_address: "http://127.0.0.1:1".to_string(),
            ca_file: String::new(),
            token_path: dir.path().join("token").display().to_string(),
            ..ArgoCd::default()
        };
        let client = DesiredImageClient::new(&cfg, &["Deployment".to_string()]).unwrap();
        let err = client.desired_images("x").await.unwrap_err();
        assert!(
            err.to_string()
                .starts_with("kind Deployment: call argocd api"),
            "{err}"
        );
    }

    #[test]
    fn ca_bundle_errors() {
        let cfg = ArgoCd {
            ca_file: "/nonexistent/ca.crt".to_string(),
            ..ArgoCd::default()
        };
        let err = DesiredImageClient::new(&cfg, &[]).unwrap_err();
        assert!(matches!(err, ApiError::ReadCa { .. }), "{err}");

        let dir = tempfile::tempdir().unwrap();
        let ca = dir.path().join("ca.crt");
        std::fs::write(&ca, "not a certificate").unwrap();
        let cfg = ArgoCd {
            ca_file: ca.display().to_string(),
            ..ArgoCd::default()
        };
        let err = DesiredImageClient::new(&cfg, &[]).unwrap_err();
        assert!(
            matches!(err, ApiError::EmptyCa { .. } | ApiError::Client(_)),
            "{err}"
        );

        let cert = rcgen::generate_simple_self_signed(vec!["argocd-server".to_string()]).unwrap();
        std::fs::write(&ca, cert.cert.pem()).unwrap();
        let cfg = ArgoCd {
            ca_file: ca.display().to_string(),
            ..ArgoCd::default()
        };
        assert!(DesiredImageClient::new(&cfg, &[]).is_ok());

        let insecure = ArgoCd {
            ca_file: "/nonexistent/ca.crt".to_string(),
            insecure_skip_verify: true,
            ..ArgoCd::default()
        };
        assert!(
            DesiredImageClient::new(&insecure, &[]).is_ok(),
            "ca file is ignored when skipping verification"
        );
    }
}
