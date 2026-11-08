//! Detections in PostgreSQL. The manager still serves queries from memory,
//! the database replaces the file on the manager volume as the durable
//! copy, so detections survive the volume and other tools can query them.

use std::collections::VecDeque;
use std::path::PathBuf;
use std::time::Duration;

use rustls_pki_types::CertificateDer;
use rustls_pki_types::pem::PemObject as _;
use tokio::sync::{mpsc, watch};
use tokio_postgres::{Client, NoTls};

use crate::policy::Detection;

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS ward_detections (
    id BIGSERIAL PRIMARY KEY,
    received_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    node TEXT NOT NULL,
    detection JSONB NOT NULL
);
CREATE INDEX IF NOT EXISTS ward_detections_node_id ON ward_detections (node, id);
";
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const RETRY_MAX: Duration = Duration::from_secs(30);

#[derive(Debug, thiserror::Error)]
pub enum PgError {
    #[error("read {path}: {source}")]
    Read {
        path: String,
        source: std::io::Error,
    },
    #[error("CA bundle {0} holds no certificate")]
    NoCa(String),
    #[error("TLS: {0}")]
    Tls(#[from] rustls::Error),
    #[error(transparent)]
    Postgres(#[from] tokio_postgres::Error),
    #[error("stored detection: {0}")]
    Json(#[from] serde_json::Error),
}

/// Where the server is and how to log in.
#[derive(Clone, Debug)]
pub struct Settings {
    pub host: String,
    pub port: u16,
    pub database: String,
    pub user: String,
    pub password_file: PathBuf,
    /// CA bundle the server certificate must chain to. Without it the
    /// connection is not encrypted.
    pub ca_file: Option<PathBuf>,
}

fn read(path: &PathBuf) -> Result<String, PgError> {
    std::fs::read_to_string(path).map_err(|source| PgError::Read {
        path: path.display().to_string(),
        source,
    })
}

fn tls(ca_file: &PathBuf) -> Result<tokio_postgres_rustls::MakeRustlsConnect, PgError> {
    let pem = read(ca_file)?;
    let mut roots = rustls::RootCertStore::empty();
    for cert in CertificateDer::pem_slice_iter(pem.as_bytes()).filter_map(Result::ok) {
        roots.add(cert)?;
    }
    if roots.is_empty() {
        return Err(PgError::NoCa(ca_file.display().to_string()));
    }
    let config = rustls::ClientConfig::builder()
        .with_root_certificates(roots)
        .with_no_client_auth();
    Ok(tokio_postgres_rustls::MakeRustlsConnect::new(config))
}

/// Connects and creates the table when it is missing.
pub async fn connect(s: &Settings) -> Result<Client, PgError> {
    let mut config = tokio_postgres::Config::new();
    config
        .host(&s.host)
        .port(s.port)
        .dbname(&s.database)
        .user(&s.user)
        .password(read(&s.password_file)?.trim())
        .application_name("ward-manager")
        .connect_timeout(CONNECT_TIMEOUT);
    let client = if let Some(ca) = &s.ca_file {
        config.ssl_mode(tokio_postgres::config::SslMode::Require);
        let (client, conn) = config.connect(tls(ca)?).await?;
        tokio::spawn(async move {
            if let Err(e) = conn.await {
                tracing::warn!(error = %e, "postgresql connection");
            }
        });
        client
    } else {
        config.ssl_mode(tokio_postgres::config::SslMode::Disable);
        let (client, conn) = config.connect(NoTls).await?;
        tokio::spawn(async move {
            if let Err(e) = conn.await {
                tracing::warn!(error = %e, "postgresql connection");
            }
        });
        client
    };
    client.batch_execute(SCHEMA).await?;
    Ok(client)
}

/// Connects, retrying with backoff until it succeeds or shutdown.
pub async fn connect_retrying(s: &Settings, mut shutdown: watch::Receiver<bool>) -> Option<Client> {
    let mut wait = Duration::from_secs(1);
    loop {
        match connect(s).await {
            Ok(c) => return Some(c),
            Err(e) => tracing::warn!(error = %e, host = %s.host, "connect to postgresql"),
        }
        tokio::select! {
            () = tokio::time::sleep(wait) => {}
            _ = shutdown.changed() => return None,
        }
        wait = (wait * 2).min(RETRY_MAX);
    }
}

fn limit(n: usize) -> i64 {
    i64::try_from(n).unwrap_or(i64::MAX)
}

/// The newest `retention` detections, oldest first.
pub async fn load(client: &Client, retention: usize) -> Result<VecDeque<Detection>, PgError> {
    let rows = client
        .query(
            "SELECT detection FROM (SELECT id, detection FROM ward_detections ORDER BY id DESC LIMIT $1) t ORDER BY id",
            &[&limit(retention)],
        )
        .await?;
    rows.iter()
        .map(|r| {
            Ok(serde_json::from_value(
                r.try_get::<_, serde_json::Value>(0)?,
            )?)
        })
        .collect()
}

/// Inserts a batch, then drops what exceeds the node share and the
/// retention, the same bounds the memory store keeps.
pub async fn insert(
    client: &Client,
    batch: &[Detection],
    retention: usize,
    per_node: usize,
) -> Result<(), PgError> {
    if batch.is_empty() {
        return Ok(());
    }
    let nodes: Vec<String> = batch.iter().map(|d| d.node.clone()).collect();
    let docs = batch
        .iter()
        .map(serde_json::to_value)
        .collect::<Result<Vec<_>, _>>()?;
    client
        .execute(
            "INSERT INTO ward_detections (node, detection) SELECT * FROM UNNEST($1::text[], $2::jsonb[])",
            &[&nodes, &docs],
        )
        .await?;
    let mut distinct = nodes.clone();
    distinct.sort_unstable();
    distinct.dedup();
    for node in &distinct {
        client
            .execute(
                "DELETE FROM ward_detections WHERE node = $1 AND id <= (SELECT id FROM ward_detections WHERE node = $1 ORDER BY id DESC OFFSET $2 LIMIT 1)",
                &[node, &limit(per_node)],
            )
            .await?;
    }
    client
        .execute(
            "DELETE FROM ward_detections WHERE id <= (SELECT id FROM ward_detections ORDER BY id DESC OFFSET $1 LIMIT 1)",
            &[&limit(retention)],
        )
        .await?;
    Ok(())
}

/// Writes batches until the channel closes. A failed batch is retried once
/// on a fresh connection, then dropped from the database only: the memory
/// store still serves it.
pub async fn writer(
    settings: Settings,
    client: Client,
    retention: usize,
    per_node: usize,
    shutdown: watch::Receiver<bool>,
    mut rx: mpsc::UnboundedReceiver<Vec<Detection>>,
) {
    let mut client = client;
    while let Some(batch) = rx.recv().await {
        if insert(&client, &batch, retention, per_node).await.is_ok() {
            continue;
        }
        let Some(fresh) = connect_retrying(&settings, shutdown.clone()).await else {
            return;
        };
        client = fresh;
        if let Err(e) = insert(&client, &batch, retention, per_node).await {
            tracing::warn!(error = %e, count = batch.len(), "write detections to postgresql");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::policy::{Engine, Severity};

    #[test]
    fn tls_needs_a_ca_bundle() {
        crate::tls::install_provider();
        let dir = tempfile::tempdir().unwrap();
        let ca = dir.path().join("ca.crt");
        assert!(matches!(tls(&ca), Err(PgError::Read { .. })));
        std::fs::write(&ca, "not a certificate").unwrap();
        assert!(matches!(tls(&ca), Err(PgError::NoCa(_))));
        let pem = crate::tls::Ca::generate(chrono::Utc::now())
            .unwrap()
            .cert_pem;
        std::fs::write(&ca, pem).unwrap();
        assert!(tls(&ca).is_ok());
        assert_eq!(limit(usize::MAX), i64::MAX);
    }

    /// Runs against a real server when `WARD_TEST_POSTGRESQL` holds
    /// `host:port:password` of a throwaway database.
    #[tokio::test]
    async fn stores_and_prunes_detections() {
        let Ok(target) = std::env::var("WARD_TEST_POSTGRESQL") else {
            return;
        };
        let mut parts = target.splitn(3, ':');
        let (host, port, password) = (
            parts.next().unwrap().to_string(),
            parts.next().unwrap().parse().unwrap(),
            parts.next().unwrap().to_string(),
        );
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("password"), password).unwrap();
        let settings = Settings {
            host,
            port,
            database: "postgres".into(),
            user: "postgres".into(),
            password_file: dir.path().join("password"),
            ca_file: None,
        };
        let client = connect(&settings).await.unwrap();
        client
            .batch_execute("TRUNCATE ward_detections")
            .await
            .unwrap();
        let det = |node: &str, rule: &str| {
            Detection::new(node, Engine::Behavior, rule, Severity::High, String::new())
        };
        let batch: Vec<Detection> = (0..5).map(|i| det("noisy", &format!("r{i}"))).collect();
        insert(&client, &batch, 6, 3).await.unwrap();
        insert(&client, &[det("quiet", "q1"), det("quiet", "q2")], 4, 3)
            .await
            .unwrap();
        let rules: Vec<String> = load(&client, 10)
            .await
            .unwrap()
            .into_iter()
            .map(|d| d.rule)
            .collect();
        assert_eq!(rules, vec!["r3", "r4", "q1", "q2"]);
        assert_eq!(load(&client, 1).await.unwrap()[0].rule, "q2");
        insert(&client, &[], 4, 3).await.unwrap();
    }
}
