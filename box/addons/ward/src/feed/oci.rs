//! A minimal OCI distribution client: resolve a reference to the database
//! and signature layers, and download blobs with digest checks. Anonymous
//! or token auth (the Docker registry token flow), with optional
//! credentials from a `kubernetes.io/dockerconfigjson` Secret.

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as B64;
use futures::StreamExt as _;
use serde::Deserialize;

use crate::content::{MEDIA_TYPE_DB, MEDIA_TYPE_SIG, check_digest};

const MANIFEST_ACCEPT: &str = "application/vnd.oci.image.manifest.v1+json";

#[derive(Debug, thiserror::Error)]
pub enum OciError {
    #[error("invalid reference {0}")]
    Reference(String),
    #[error("registry request {url}: {reason}")]
    Http { url: String, reason: String },
    #[error("manifest has no {MEDIA_TYPE_DB} layer")]
    NoDatabase,
    #[error("blob {digest} exceeds {limit} bytes")]
    TooLarge { digest: String, limit: u64 },
    #[error(transparent)]
    Digest(#[from] crate::content::ContentError),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Reference {
    pub registry: String,
    pub repository: String,
    /// A tag, or `sha256:<hex>`.
    pub tag: String,
}

impl Reference {
    pub fn parse(s: &str) -> Result<Self, OciError> {
        let bad = || OciError::Reference(s.to_string());
        let (registry, rest) = s.split_once('/').ok_or_else(bad)?;
        if !registry.contains('.') && !registry.contains(':') && registry != "localhost" {
            return Err(bad());
        }
        let (repository, tag) = if let Some((repo, digest)) = rest.split_once('@') {
            (repo.to_string(), digest.to_string())
        } else {
            match rest.rsplit_once(':') {
                Some((repo, tag)) if !tag.contains('/') => (repo.to_string(), tag.to_string()),
                _ => (rest.to_string(), "latest".to_string()),
            }
        };
        if repository.is_empty() || tag.is_empty() {
            return Err(bad());
        }
        Ok(Self {
            registry: registry.to_string(),
            repository,
            tag,
        })
    }

    pub fn repository_url(&self) -> String {
        format!("{}/{}", self.registry, self.repository)
    }

    fn base(&self) -> String {
        let scheme = if self.registry.starts_with("localhost") || self.registry.starts_with("127.")
        {
            "http"
        } else {
            "https"
        };
        format!("{scheme}://{}/v2/{}", self.registry, self.repository)
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct Descriptor {
    pub media_type: String,
    pub digest: String,
    pub size: u64,
}

#[derive(Debug, Deserialize)]
struct Manifest {
    #[serde(default)]
    layers: Vec<Descriptor>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Resolved {
    pub db: Descriptor,
    pub signature: Option<Descriptor>,
}

/// Picks the database and signature layers of a manifest.
pub fn layers(manifest: &[u8]) -> Result<Resolved, OciError> {
    let m: Manifest = serde_json::from_slice(manifest).map_err(|e| OciError::Http {
        url: "manifest".into(),
        reason: e.to_string(),
    })?;
    let db = m
        .layers
        .iter()
        .find(|l| l.media_type == MEDIA_TYPE_DB)
        .cloned()
        .ok_or(OciError::NoDatabase)?;
    let signature = m
        .layers
        .iter()
        .find(|l| l.media_type == MEDIA_TYPE_SIG)
        .cloned();
    Ok(Resolved { db, signature })
}

/// Credentials for `registry` from a dockerconfigjson document.
pub fn credentials(dockerconfig: &[u8], registry: &str) -> Option<(String, String)> {
    #[derive(Deserialize)]
    struct Auth {
        #[serde(default, rename = "auth")]
        encoded: Option<String>,
        #[serde(default)]
        username: Option<String>,
        #[serde(default)]
        password: Option<String>,
    }
    #[derive(Deserialize)]
    struct Config {
        auths: std::collections::HashMap<String, Auth>,
    }
    let cfg: Config = serde_json::from_slice(dockerconfig).ok()?;
    let entry = cfg.auths.iter().find_map(|(k, v)| {
        let host = k
            .trim_start_matches("https://")
            .trim_start_matches("http://")
            .split('/')
            .next()
            .unwrap_or_default();
        (host == registry).then_some(v)
    })?;
    if let (Some(u), Some(p)) = (&entry.username, &entry.password) {
        return Some((u.clone(), p.clone()));
    }
    let raw = B64.decode(entry.encoded.as_ref()?).ok()?;
    let s = String::from_utf8(raw).ok()?;
    let (u, p) = s.split_once(':')?;
    Some((u.to_string(), p.to_string()))
}

/// Parses `Bearer realm="...",service="...",scope="..."`.
fn bearer_params(header: &str) -> Option<(String, Vec<(String, String)>)> {
    let rest = header.strip_prefix("Bearer ")?;
    let mut realm = None;
    let mut params = Vec::new();
    for part in rest.split(',') {
        let (k, v) = part.trim().split_once('=')?;
        let v = v.trim_matches('"').to_string();
        if k == "realm" {
            realm = Some(v);
        } else {
            params.push((k.to_string(), v));
        }
    }
    Some((realm?, params))
}

#[derive(Clone)]
pub struct Registry {
    http: reqwest::Client,
    credentials: Option<(String, String)>,
}

impl Registry {
    pub fn new(credentials: Option<(String, String)>) -> Result<Self, OciError> {
        let http = reqwest::Client::builder()
            .user_agent(concat!("ward/", env!("CARGO_PKG_VERSION")))
            .timeout(std::time::Duration::from_mins(5))
            .build()
            .map_err(|e| OciError::Http {
                url: "client".into(),
                reason: e.to_string(),
            })?;
        Ok(Self { http, credentials })
    }

    async fn token(&self, r: &Reference, header: &str) -> Option<String> {
        #[derive(Deserialize)]
        struct Token {
            #[serde(default)]
            token: Option<String>,
            #[serde(default)]
            access_token: Option<String>,
        }
        let (realm, mut params) = bearer_params(header)?;
        if !params.iter().any(|(k, _)| k == "scope") {
            params.push(("scope".into(), format!("repository:{}:pull", r.repository)));
        }
        let mut url = reqwest::Url::parse(&realm).ok()?;
        url.query_pairs_mut().extend_pairs(&params);
        let mut req = self.http.get(url);
        if let Some((u, p)) = &self.credentials {
            req = req.basic_auth(u, Some(p));
        }
        let t: Token = req.send().await.ok()?.json().await.ok()?;
        t.token.or(t.access_token)
    }

    async fn get(
        &self,
        r: &Reference,
        url: &str,
        accept: Option<&str>,
    ) -> Result<reqwest::Response, OciError> {
        let http_err = |reason: String| OciError::Http {
            url: url.to_string(),
            reason,
        };
        let build = |auth: Option<&str>| {
            let mut req = self.http.get(url);
            if let Some(a) = accept {
                req = req.header(reqwest::header::ACCEPT, a);
            }
            match auth {
                Some(token) => req.bearer_auth(token),
                None => match &self.credentials {
                    Some((u, p)) if !url.contains("/blobs/") => req.basic_auth(u, Some(p)),
                    _ => req,
                },
            }
        };
        let res = build(None)
            .send()
            .await
            .map_err(|e| http_err(e.to_string()))?;
        let res = if res.status() == reqwest::StatusCode::UNAUTHORIZED {
            let header = res
                .headers()
                .get(reqwest::header::WWW_AUTHENTICATE)
                .and_then(|h| h.to_str().ok())
                .unwrap_or_default()
                .to_string();
            let token = self
                .token(r, &header)
                .await
                .ok_or_else(|| http_err("unauthorized and no token".into()))?;
            build(Some(&token))
                .send()
                .await
                .map_err(|e| http_err(e.to_string()))?
        } else {
            res
        };
        if !res.status().is_success() {
            return Err(http_err(format!("status {}", res.status())));
        }
        Ok(res)
    }

    pub async fn resolve(&self, r: &Reference) -> Result<Resolved, OciError> {
        let url = format!("{}/manifests/{}", r.base(), r.tag);
        let body = self
            .get(r, &url, Some(MANIFEST_ACCEPT))
            .await?
            .bytes()
            .await
            .map_err(|e| OciError::Http {
                url: url.clone(),
                reason: e.to_string(),
            })?;
        layers(&body)
    }

    /// Downloads a blob of at most `limit` bytes and checks its digest.
    pub async fn blob(&self, r: &Reference, digest: &str, limit: u64) -> Result<Vec<u8>, OciError> {
        let url = format!("{}/blobs/{digest}", r.base());
        let res = self.get(r, &url, None).await?;
        let mut out = Vec::new();
        let mut stream = res.bytes_stream();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|e| OciError::Http {
                url: url.clone(),
                reason: e.to_string(),
            })?;
            if (out.len() + chunk.len()) as u64 > limit {
                return Err(OciError::TooLarge {
                    digest: digest.to_string(),
                    limit,
                });
            }
            out.extend_from_slice(&chunk);
        }
        check_digest(&out, digest)?;
        Ok(out)
    }

    /// Downloads a blob from `repository` (`registry/repo`) by digest.
    pub async fn blob_from(
        &self,
        repository: &str,
        digest: &str,
        limit: u64,
    ) -> Result<Vec<u8>, OciError> {
        let r = Reference::parse(&format!("{repository}@{digest}"))?;
        self.blob(&r, digest, limit).await
    }
}

#[cfg(test)]
mod tests {
    use axum::Router;
    use axum::http::{HeaderMap, StatusCode, header};
    use axum::response::IntoResponse;
    use axum::routing::get;

    use super::*;
    use crate::content::{MEDIA_TYPE_DB, MEDIA_TYPE_SIG, oci_digest};

    #[test]
    fn references_parse() {
        let r = Reference::parse("ghcr.io/younsl/ward-db:latest").unwrap();
        assert_eq!(r.registry, "ghcr.io");
        assert_eq!(r.repository, "younsl/ward-db");
        assert_eq!(r.tag, "latest");
        assert_eq!(r.repository_url(), "ghcr.io/younsl/ward-db");
        assert_eq!(r.base(), "https://ghcr.io/v2/younsl/ward-db");
        let d = Reference::parse("localhost:5000/ward-db@sha256:abc").unwrap();
        assert_eq!(d.tag, "sha256:abc");
        assert!(d.base().starts_with("http://"));
        assert_eq!(
            Reference::parse("harbor.example.com/x/ward-db")
                .unwrap()
                .tag,
            "latest"
        );
        for bad in ["ward-db", "library/ward-db:1", "ghcr.io/", "ghcr.io/x@"] {
            assert!(Reference::parse(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn layers_and_credentials() {
        let m = format!(
            r#"{{"layers":[{{"mediaType":"{MEDIA_TYPE_DB}","digest":"sha256:a","size":3}},{{"mediaType":"{MEDIA_TYPE_SIG}","digest":"sha256:b","size":1}}]}}"#
        );
        let r = layers(m.as_bytes()).unwrap();
        assert_eq!(r.db.digest, "sha256:a");
        assert_eq!(r.signature.unwrap().digest, "sha256:b");
        assert!(matches!(
            layers(br#"{"layers":[]}"#),
            Err(OciError::NoDatabase)
        ));
        assert!(layers(b"x").is_err());

        let auth = B64.encode("user:pass");
        let cfg = format!(
            r#"{{"auths":{{"https://ghcr.io":{{"auth":"{auth}"}},"harbor.example.com":{{"username":"u","password":"p"}}}}}}"#
        );
        assert_eq!(
            credentials(cfg.as_bytes(), "ghcr.io"),
            Some(("user".into(), "pass".into()))
        );
        assert_eq!(
            credentials(cfg.as_bytes(), "harbor.example.com"),
            Some(("u".into(), "p".into()))
        );
        assert_eq!(credentials(cfg.as_bytes(), "other.io"), None);
        assert_eq!(credentials(b"{}", "ghcr.io"), None);

        let (realm, params) = bearer_params(
            r#"Bearer realm="https://ghcr.io/token",service="ghcr.io",scope="repository:x:pull""#,
        )
        .unwrap();
        assert_eq!(realm, "https://ghcr.io/token");
        assert_eq!(params.len(), 2);
        assert!(bearer_params("Basic realm=x").is_none());
    }

    #[tokio::test]
    async fn pulls_through_the_token_flow() {
        let db = b"database".to_vec();
        let digest = oci_digest(&db);
        let manifest = format!(
            r#"{{"layers":[{{"mediaType":"{MEDIA_TYPE_DB}","digest":"{digest}","size":8}}]}}"#
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let realm = format!("http://127.0.0.1:{port}/token");
        let blob = db.clone();
        let app = Router::new()
            .route("/token", get(|| async { r#"{"token":"t"}"# }))
            .route(
                "/v2/x/ward-db/manifests/latest",
                get(move |h: HeaderMap| {
                    let manifest = manifest.clone();
                    let realm = realm.clone();
                    async move {
                        if h.get(header::AUTHORIZATION).is_none() {
                            return (
                                StatusCode::UNAUTHORIZED,
                                [(
                                    header::WWW_AUTHENTICATE,
                                    format!(r#"Bearer realm="{realm}",service="test""#),
                                )],
                                String::new(),
                            )
                                .into_response();
                        }
                        manifest.into_response()
                    }
                }),
            )
            .route(
                &format!("/v2/x/ward-db/blobs/{digest}"),
                get(move || {
                    let b = blob.clone();
                    async move { b }
                }),
            );
        tokio::spawn(async move { axum::serve(listener, app).await });
        crate::tls::install_provider();
        let reg = Registry::new(Some(("u".into(), "p".into()))).unwrap();
        let r = Reference::parse(&format!("localhost:{port}/x/ward-db")).unwrap();
        let resolved = reg.resolve(&r).await.unwrap();
        assert_eq!(resolved.db.digest, digest);
        assert!(resolved.signature.is_none());
        assert_eq!(reg.blob(&r, &digest, 100).await.unwrap(), db);
        assert!(matches!(
            reg.blob(&r, &digest, 2).await,
            Err(OciError::TooLarge { .. })
        ));
        assert_eq!(
            reg.blob_from(&format!("localhost:{port}/x/ward-db"), &digest, 100)
                .await
                .unwrap(),
            db
        );
        assert!(reg.blob(&r, "sha256:missing", 100).await.is_err());
    }
}
