//! The agent's mutual TLS client to the manager.

use std::path::{Path, PathBuf};
use std::sync::RwLock;
use std::time::{Duration, Instant};

use crate::policy::Detection;

const REBUILD_EVERY: Duration = Duration::from_hours(1);

#[derive(Debug, thiserror::Error)]
pub enum ClientError {
    #[error(transparent)]
    Tls(#[from] crate::tls::TlsError),
    #[error("manager request {path}: {reason}")]
    Http { path: String, reason: String },
}

pub struct ManagerClient {
    base: String,
    /// The projected `ServiceAccount` token with the manager audience. The
    /// kubelet rotates it, so it is read for every request.
    token_file: PathBuf,
    tls_dir: PathBuf,
    http: RwLock<(reqwest::Client, Instant)>,
}

fn build(tls_dir: &Path) -> Result<reqwest::Client, ClientError> {
    let tls = crate::tls::client_config(tls_dir)?;
    reqwest::Client::builder()
        .tls_backend_preconfigured(tls)
        .timeout(Duration::from_mins(2))
        .build()
        .map_err(|e| ClientError::Http {
            path: "client".into(),
            reason: e.to_string(),
        })
}

impl ManagerClient {
    pub fn new(base: &str, tls_dir: &Path, token_file: &Path) -> Result<Self, ClientError> {
        Ok(Self {
            base: base.trim_end_matches('/').to_string(),
            token_file: token_file.to_path_buf(),
            tls_dir: tls_dir.to_path_buf(),
            http: RwLock::new((build(tls_dir)?, Instant::now())),
        })
    }

    fn token(&self) -> String {
        std::fs::read_to_string(&self.token_file)
            .map(|t| t.trim().to_string())
            .unwrap_or_default()
    }

    /// The client, rebuilt hourly so renewed certificates are picked up.
    fn http(&self) -> reqwest::Client {
        if let Ok(g) = self.http.read()
            && g.1.elapsed() < REBUILD_EVERY
        {
            return g.0.clone();
        }
        if let (Ok(c), Ok(mut g)) = (build(&self.tls_dir), self.http.write()) {
            *g = (c, Instant::now());
        }
        self.http.read().map(|g| g.0.clone()).unwrap_or_default()
    }

    async fn get(&self, path: &str) -> Result<Vec<u8>, ClientError> {
        let err = |reason: String| ClientError::Http {
            path: path.to_string(),
            reason,
        };
        let res = self
            .http()
            .get(format!("{}{path}", self.base))
            .bearer_auth(self.token())
            .send()
            .await
            .map_err(|e| err(e.to_string()))?;
        if !res.status().is_success() {
            return Err(err(format!("status {}", res.status())));
        }
        Ok(res.bytes().await.map_err(|e| err(e.to_string()))?.to_vec())
    }

    pub async fn content(&self, digest: &str) -> Result<Vec<u8>, ClientError> {
        self.get(&format!("/v1/content/{digest}")).await
    }

    pub async fn signature(&self, digest: &str) -> Result<Vec<u8>, ClientError> {
        self.get(&format!("/v1/content/{digest}/signature")).await
    }

    pub async fn post_detections(&self, batch: &[Detection]) -> Result<(), ClientError> {
        let path = "/v1/detections";
        let res = self
            .http()
            .post(format!("{}{path}", self.base))
            .bearer_auth(self.token())
            .json(batch)
            .send()
            .await
            .map_err(|e| ClientError::Http {
                path: path.into(),
                reason: e.to_string(),
            })?;
        if res.status().is_success() {
            Ok(())
        } else {
            Err(ClientError::Http {
                path: path.into(),
                reason: format!("status {}", res.status()),
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn needs_tls_material() {
        crate::tls::install_provider();
        let dir = tempfile::tempdir().unwrap();
        assert!(matches!(
            ManagerClient::new("https://m:8443", dir.path(), &dir.path().join("token")),
            Err(ClientError::Tls(_))
        ));
    }
}
