//! The optional manager: feed relay, detection store, and APIs.

mod api;
mod auth;
mod relay;
mod store;

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};
use std::time::Duration;

use anyhow::Context as _;
use prometheus_client::registry::Registry;
use tokio::sync::{mpsc, watch};

use crate::feed::oci::{Reference, Registry as OciRegistry, credentials};
use crate::observability::{Health, router, serve, stopped};
use crate::policy::{POLICY_FILE, Policy};
use crate::rollout::{ROLLOUT_FILE, Rollout};

pub use auth::AUDIENCE;

#[derive(Clone, Debug)]
pub struct ManagerArgs {
    pub agent_addr: SocketAddr,
    pub admin_addr: SocketAddr,
    pub health_addr: SocketAddr,
    pub data_dir: PathBuf,
    pub policy_dir: PathBuf,
    pub rollout_dir: PathBuf,
    pub tls_dir: PathBuf,
    pub token_file: PathBuf,
    pub registry_auth: Option<PathBuf>,
    pub retention: usize,
    /// Namespace of the agent `ServiceAccount` whose tokens are accepted.
    pub namespace: String,
}

const RELAY_EVERY: Duration = Duration::from_secs(30);
const TLS_RELOAD_EVERY: Duration = Duration::from_mins(10);

fn read_json<T: serde::de::DeserializeOwned + Default>(path: &Path) -> T {
    std::fs::read(path)
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default()
}

fn registry(auth: Option<&Path>, policy: &Policy) -> anyhow::Result<OciRegistry> {
    let creds = auth.and_then(|p| {
        let cfg = std::fs::read(p).ok()?;
        let image = policy.feed.as_ref()?.image.clone();
        credentials(&cfg, &Reference::parse(&image).ok()?.registry)
    });
    Ok(OciRegistry::new(creds)?)
}

pub async fn run(args: ManagerArgs, shutdown: watch::Receiver<bool>) -> anyhow::Result<()> {
    let client = crate::k8s::client().await?;
    let auth = auth::Authenticator::token_review(
        client,
        &args.namespace,
        crate::operator::resources::AGENT_SERVICE_ACCOUNT,
    );
    serve_with(args, auth, shutdown).await
}

async fn serve_with(
    args: ManagerArgs,
    auth: auth::Authenticator,
    shutdown: watch::Receiver<bool>,
) -> anyhow::Result<()> {
    let mut reg = Registry::default();
    let metrics = api::ManagerMetrics::register(&mut reg);
    let health = Health::new(reg);
    tokio::spawn(serve(
        args.health_addr,
        router(health.clone()),
        shutdown.clone(),
    ));

    std::fs::create_dir_all(&args.data_dir).context("data dir")?;
    let path = args.data_dir.join(store::FILE);
    let retention = args.retention.max(100);
    let (items, lines) = store::load(&path, retention);
    let (tx, rx) = mpsc::unbounded_channel();
    tokio::spawn(store::writer(path, retention, lines, rx));
    let store = Arc::new(store::Store::new(items, retention, tx));
    #[allow(clippy::cast_possible_wrap)]
    metrics.stored.set(store.len() as i64);

    let token = std::fs::read_to_string(&args.token_file)
        .map(|t| t.trim().to_string())
        .unwrap_or_default();
    if token.is_empty() {
        tracing::warn!(file = %args.token_file.display(), "no API token, the admin API rejects every request");
    }
    let relay = Arc::new(relay::Relay::default());
    let state = Arc::new(api::AppState {
        store,
        relay: relay.clone(),
        agents: RwLock::new(BTreeMap::new()),
        token,
        auth,
        metrics,
    });

    {
        let args = args.clone();
        let mut shutdown = shutdown.clone();
        tokio::spawn(async move {
            let content = args.data_dir.join("content");
            loop {
                let policy: Policy = read_json(&args.policy_dir.join(POLICY_FILE));
                let rollout: Rollout = read_json(&args.rollout_dir.join(ROLLOUT_FILE));
                match registry(args.registry_auth.as_deref(), &policy) {
                    Ok(r) => {
                        if let Err(e) = relay.sync(&content, &rollout, &policy, &r).await {
                            tracing::warn!(error = %e, "feed relay");
                        }
                    }
                    Err(e) => tracing::warn!(error = %e, "registry client"),
                }
                tokio::select! {
                    () = tokio::time::sleep(RELAY_EVERY) => {}
                    _ = shutdown.changed() => return,
                }
            }
        });
    }

    tokio::spawn(serve(
        args.admin_addr,
        api::admin_router(state.clone()),
        shutdown.clone(),
    ));

    let tls = axum_server::tls_rustls::RustlsConfig::from_config(
        crate::tls::server_config(&args.tls_dir).context("manager TLS")?,
    );
    {
        let (tls, dir, mut shutdown) = (tls.clone(), args.tls_dir.clone(), shutdown.clone());
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    () = tokio::time::sleep(TLS_RELOAD_EVERY) => {}
                    _ = shutdown.changed() => return,
                }
                match crate::tls::server_config(&dir) {
                    Ok(cfg) => tls.reload_from_config(cfg),
                    Err(e) => tracing::warn!(error = %e, "reload manager TLS"),
                }
            }
        });
    }
    let handle = axum_server::Handle::new();
    {
        let (handle, shutdown) = (handle.clone(), shutdown.clone());
        tokio::spawn(async move {
            stopped(shutdown).await;
            handle.graceful_shutdown(Some(Duration::from_secs(10)));
        });
    }
    health.set_ready(true);
    tracing::info!(addr = %args.agent_addr, "manager agent API listening");
    axum_server::bind_rustls(args.agent_addr, tls)
        .handle(handle)
        .serve(api::agent_router(state).into_make_service())
        .await
        .context("agent API")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use chrono::Utc;

    use super::*;
    use crate::agent::client_for_tests as client;
    use crate::policy::{Detection, Engine, Severity};
    use rustls_pki_types::pem::PemObject as _;

    use crate::tls::{CA_FILE, CERT_FILE, Ca, KEY_FILE};

    fn free_addr() -> SocketAddr {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        l.local_addr().unwrap()
    }

    fn write(dir: &Path, cert: &str, key: &str, ca: &str) {
        std::fs::write(dir.join(CERT_FILE), cert).unwrap();
        std::fs::write(dir.join(KEY_FILE), key).unwrap();
        std::fs::write(dir.join(CA_FILE), ca).unwrap();
    }

    /// A CA, then server and agent TLS directories issued from it.
    fn pki() -> (Ca, tempfile::TempDir, tempfile::TempDir) {
        let now = Utc::now();
        let ca = Ca::generate(now).unwrap();
        let server = ca
            .issue("ward-manager", &["localhost".into()], true, now)
            .unwrap();
        let agent = ca.issue("ward-agent", &[], false, now).unwrap();
        let (server_dir, agent_dir) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        write(
            server_dir.path(),
            &server.cert_pem,
            &server.key_pem,
            &ca.cert_pem,
        );
        write(
            agent_dir.path(),
            &agent.cert_pem,
            &agent.key_pem,
            &ca.cert_pem,
        );
        (ca, server_dir, agent_dir)
    }

    #[tokio::test]
    async fn agents_reach_the_manager_over_mutual_tls() {
        crate::tls::install_provider();
        let (ca, server_dir, agent_dir) = pki();
        let (data, token_dir) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        std::fs::write(token_dir.path().join("token"), "secret\n").unwrap();
        let args = ManagerArgs {
            agent_addr: free_addr(),
            admin_addr: free_addr(),
            health_addr: free_addr(),
            data_dir: data.path().to_path_buf(),
            policy_dir: data.path().join("none"),
            rollout_dir: data.path().join("none"),
            tls_dir: server_dir.path().to_path_buf(),
            token_file: token_dir.path().join("token"),
            registry_auth: None,
            retention: 100,
            namespace: "ward-system".into(),
        };
        let auth = auth::Authenticator::Static(std::collections::HashMap::from([(
            "agent-token".to_string(),
            "node-a".to_string(),
        )]));
        let (stop_tx, stop_rx) = watch::channel(false);
        let task = tokio::spawn(serve_with(args.clone(), auth, stop_rx));
        tokio::time::sleep(Duration::from_millis(300)).await;

        let url = format!("https://localhost:{}", args.agent_addr.port());
        std::fs::write(token_dir.path().join("agent"), "agent-token\n").unwrap();
        let c = client(&url, agent_dir.path(), &token_dir.path().join("agent"));
        let d = Detection::new(
            "spoofed-node",
            Engine::Behavior,
            "reverse-shell",
            Severity::Critical,
            String::new(),
        );
        c.post_detections(&[d]).await.unwrap();
        assert!(c.content("sha256:none").await.is_err());
        assert!(c.signature("sha256:none").await.is_err());

        let plain = reqwest::Client::builder()
            .tls_backend_preconfigured(
                rustls::ClientConfig::builder()
                    .with_root_certificates({
                        let mut r = rustls::RootCertStore::empty();
                        r.add(
                            rustls_pki_types::CertificateDer::from_pem_slice(
                                ca.cert_pem.as_bytes(),
                            )
                            .unwrap(),
                        )
                        .unwrap();
                        r
                    })
                    .with_no_client_auth(),
            )
            .build()
            .unwrap();
        assert!(
            plain
                .get(format!("{url}/v1/content/x"))
                .send()
                .await
                .is_err(),
            "no client cert"
        );

        let admin = format!("http://{}/v1/detections", args.admin_addr);
        let body = reqwest::Client::new()
            .get(&admin)
            .bearer_auth("secret")
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap();
        assert!(body.contains("reverse-shell"), "{body}");
        assert!(
            body.contains("node-a") && !body.contains("spoofed-node"),
            "{body}"
        );
        let anon = client(&url, agent_dir.path(), &token_dir.path().join("missing"));
        assert!(
            anon.post_detections(&[]).await.is_err(),
            "no token, no access"
        );

        stop_tx.send(true).unwrap();
        task.await.unwrap().unwrap();
        assert!(data.path().join(store::FILE).exists());
    }
}
