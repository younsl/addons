//! The agent's signature database: fetched from the manager or the
//! registry, verified, compiled, self-tested, and swapped in without a
//! restart. The last known good revision is kept on the node so a restart
//! without the manager or the registry is still protected.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, RwLock};

use crate::content::{Engine, EngineParts, FeedDb};
use crate::feed::oci::Registry;
use crate::feed::verify;
use crate::policy::{Policy, revision_of};
use crate::rollout::ContentRef;

use super::client::ManagerClient;

const CURRENT: &str = "current";
const DB_FILE: &str = "db.json.gz";
const SIG_FILE: &str = "db.sig";
const REF_FILE: &str = "ref.json";

pub enum Fetch {
    Manager(Arc<ManagerClient>),
    Registry(Registry),
    None,
}

impl Fetch {
    async fn get(&self, c: &ContentRef) -> Result<(Vec<u8>, Vec<u8>), String> {
        match self {
            Self::Manager(m) => {
                let db = m.content(&c.digest).await.map_err(|e| e.to_string())?;
                let sig = if c.signature_digest.is_empty() {
                    Vec::new()
                } else {
                    m.signature(&c.digest).await.map_err(|e| e.to_string())?
                };
                Ok((db, sig))
            }
            Self::Registry(r) => {
                let db = r
                    .blob_from(&c.repository, &c.digest, crate::content::MAX_DB_BYTES)
                    .await
                    .map_err(|e| e.to_string())?;
                let sig = if c.signature_digest.is_empty() {
                    Vec::new()
                } else {
                    r.blob_from(&c.repository, &c.signature_digest, 64 * 1024)
                        .await
                        .map_err(|e| e.to_string())?
                };
                Ok((db, sig))
            }
            Self::None => Err("no manager and no registry access configured".into()),
        }
    }
}

/// Everything in the policy that changes the compiled engine.
pub fn policy_key(p: &Policy) -> String {
    revision_of(&[
        serde_json::to_string(&p.custom_signatures)
            .unwrap_or_default()
            .as_bytes(),
        serde_json::to_string(&p.custom_yara)
            .unwrap_or_default()
            .as_bytes(),
        serde_json::to_string(&p.indicators)
            .unwrap_or_default()
            .as_bytes(),
        &[u8::from(p.scan.yara)],
    ])
}

fn digest_dir(root: &Path, digest: &str) -> PathBuf {
    root.join(digest.replace(':', "-"))
}

pub struct ContentStore {
    dir: PathBuf,
    engine: RwLock<Arc<Engine>>,
    key: Mutex<String>,
    active: Mutex<Option<String>>,
    error: Mutex<Option<String>>,
}

impl ContentStore {
    pub fn new(state: &Path) -> Self {
        let engine =
            Engine::build(&EngineParts::default()).unwrap_or_else(|_| unreachable_builtin());
        Self {
            dir: state.join("content"),
            engine: RwLock::new(Arc::new(engine)),
            key: Mutex::new(String::new()),
            active: Mutex::new(None),
            error: Mutex::new(None),
        }
    }

    pub fn engine(&self) -> Arc<Engine> {
        self.engine
            .read()
            .map_or_else(|_| Arc::new(unreachable_builtin()), |e| e.clone())
    }

    pub fn active(&self) -> Option<String> {
        self.active.lock().ok().and_then(|a| a.clone())
    }

    pub fn error(&self) -> Option<String> {
        self.error.lock().ok().and_then(|e| e.clone())
    }

    /// The revision recorded as last known good on the node.
    pub fn last_known_good(&self) -> Option<ContentRef> {
        let digest = std::fs::read_to_string(self.dir.join(CURRENT)).ok()?;
        let dir = digest_dir(&self.dir, digest.trim());
        serde_json::from_slice(&std::fs::read(dir.join(REF_FILE)).ok()?).ok()
    }

    fn cached(&self, c: &ContentRef) -> Option<(Vec<u8>, Vec<u8>)> {
        let dir = digest_dir(&self.dir, &c.digest);
        let db = std::fs::read(dir.join(DB_FILE)).ok()?;
        let sig = std::fs::read(dir.join(SIG_FILE)).unwrap_or_default();
        Some((db, sig))
    }

    fn persist(&self, c: &ContentRef, db: &[u8], sig: &[u8]) -> std::io::Result<()> {
        let dir = digest_dir(&self.dir, &c.digest);
        std::fs::create_dir_all(&dir)?;
        std::fs::write(dir.join(DB_FILE), db)?;
        std::fs::write(dir.join(SIG_FILE), sig)?;
        std::fs::write(
            dir.join(REF_FILE),
            serde_json::to_vec(c).unwrap_or_default(),
        )?;
        Ok(())
    }

    /// Keeps the active and the previous revision, removes the rest.
    fn prune(&self, keep: &[&str]) {
        let Ok(rd) = std::fs::read_dir(&self.dir) else {
            return;
        };
        let keep: Vec<String> = keep.iter().map(|d| d.replace(':', "-")).collect();
        for e in rd.filter_map(Result::ok) {
            let name = e.file_name().to_string_lossy().into_owned();
            if e.path().is_dir() && !keep.contains(&name) {
                let _ = std::fs::remove_dir_all(e.path());
            }
        }
    }

    /// Brings the engine to `assigned` under `policy`. Returns whether it
    /// changed. On failure the running engine stays and the error is kept
    /// for the agent's Lease.
    pub async fn sync(
        &self,
        assigned: Option<&ContentRef>,
        policy: &Policy,
        fetch: &Fetch,
    ) -> Result<bool, String> {
        let key = format!(
            "{}|{}",
            assigned.map_or("-", |c| c.digest.as_str()),
            policy_key(policy)
        );
        if self.key.lock().is_ok_and(|k| *k == key) {
            return Ok(false);
        }
        let result = self.load(assigned, policy, fetch).await;
        match &result {
            Ok(()) => {
                if let Ok(mut k) = self.key.lock() {
                    *k = key;
                }
                if let Ok(mut e) = self.error.lock() {
                    *e = None;
                }
            }
            Err(err) => {
                if let Ok(mut e) = self.error.lock() {
                    *e = Some(err.clone());
                }
            }
        }
        result.map(|()| true)
    }

    async fn load(
        &self,
        assigned: Option<&ContentRef>,
        policy: &Policy,
        fetch: &Fetch,
    ) -> Result<(), String> {
        let (public_key, insecure) = policy.feed.as_ref().map_or((None, false), |f| {
            (f.public_key.clone(), f.insecure_skip_verify)
        });
        let db: Option<FeedDb> = match assigned {
            None => None,
            Some(c) => {
                let (db_bytes, sig) = match self.cached(c) {
                    Some(hit) => hit,
                    None => {
                        let got = fetch.get(c).await?;
                        verify(c, &got.0, &got.1, public_key.as_deref(), insecure)
                            .map_err(|e| e.to_string())?;
                        self.persist(c, &got.0, &got.1).map_err(|e| e.to_string())?;
                        got
                    }
                };
                let c2 = c.clone();
                let key = public_key.clone();
                let db = tokio::task::spawn_blocking(move || {
                    verify(&c2, &db_bytes, &sig, key.as_deref(), insecure)
                })
                .await
                .map_err(|e| e.to_string())?
                .map_err(|e| e.to_string())?;
                Some(db)
            }
        };
        let revision = assigned.map_or_else(|| "builtin".to_string(), |c| c.digest.clone());
        let index_dir = assigned.map(|c| digest_dir(&self.dir, &c.digest));
        let policy = policy.clone();
        let engine = tokio::task::spawn_blocking(move || {
            let engine = Engine::build(&EngineParts {
                revision,
                feed: db.as_ref(),
                custom_signatures: &policy.custom_signatures,
                custom_yara: &policy.custom_yara,
                indicators: Some(&policy.indicators),
                yara: policy.scan.yara,
                index_dir: index_dir.as_deref(),
            })?;
            engine.self_test()?;
            Ok::<_, crate::content::ContentError>(engine)
        })
        .await
        .map_err(|e| e.to_string())?
        .map_err(|e| e.to_string())?;
        tracing::info!(
            revision = %engine.revision,
            signatures = engine.signature_count,
            dropped_yara = engine.dropped_yara.len(),
            "signature engine compiled"
        );
        if let Ok(mut e) = self.engine.write() {
            *e = Arc::new(engine);
        }
        let previous = self.active();
        if let Some(c) = assigned {
            std::fs::create_dir_all(&self.dir).map_err(|e| e.to_string())?;
            std::fs::write(self.dir.join(CURRENT), &c.digest).map_err(|e| e.to_string())?;
            let mut keep = vec![c.digest.as_str()];
            if let Some(p) = previous.as_deref() {
                keep.push(p);
            }
            self.prune(&keep);
        }
        if let Ok(mut a) = self.active.lock() {
            *a = assigned.map(|c| c.digest.clone());
        }
        Ok(())
    }
}

/// The built-in signatures always compile. Their test proves it, so a
/// failure here is a programming error surfaced as an empty engine.
fn unreachable_builtin() -> Engine {
    Engine::build(&EngineParts {
        yara: false,
        ..EngineParts::default()
    })
    .unwrap_or_else(|e| panic!("built-in signatures must compile: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::content::oci_digest;
    use crate::content::tests::sample_db;
    use crate::crd::FeedSpec;

    fn policy() -> Policy {
        let mut p = Policy::default();
        p.scan.yara = true;
        p.feed = Some(FeedSpec {
            image: "ghcr.io/x/ward-db:latest".into(),
            public_key: None,
            insecure_skip_verify: true,
            interval: "6h".into(),
            pull_secret_ref: None,
            rollout: crate::crd::RolloutSpec::default(),
        });
        p
    }

    #[tokio::test]
    async fn swaps_caches_and_keeps_last_known_good() {
        let state = tempfile::tempdir().unwrap();
        let store = ContentStore::new(state.path());
        assert!(
            store
                .engine()
                .scanner()
                .scan(b"known bad payload")
                .is_none()
        );
        let bytes = sample_db().encode();
        let c = ContentRef {
            revision: "r1".into(),
            digest: oci_digest(&bytes),
            repository: "ghcr.io/x/ward-db".into(),
            ..ContentRef::default()
        };
        let p = policy();
        assert!(store.sync(Some(&c), &p, &Fetch::None).await.is_err());
        assert!(store.error().unwrap().contains("no manager"));

        store.persist(&c, &bytes, &[]).unwrap();
        assert!(store.sync(Some(&c), &p, &Fetch::None).await.unwrap());
        assert!(!store.sync(Some(&c), &p, &Fetch::None).await.unwrap());
        assert!(store.error().is_none());
        assert_eq!(store.active().as_deref(), Some(c.digest.as_str()));
        assert_eq!(
            store
                .engine()
                .scanner()
                .scan(b"known bad payload")
                .unwrap()
                .name,
            "Feed.Bad"
        );
        assert_eq!(store.last_known_good().unwrap(), c);

        let reopened = ContentStore::new(state.path());
        let lkg = reopened.last_known_good().unwrap();
        assert!(reopened.sync(Some(&lkg), &p, &Fetch::None).await.unwrap());

        let mut p2 = p.clone();
        p2.scan.yara = false;
        assert_ne!(policy_key(&p), policy_key(&p2));
        assert!(store.sync(None, &p2, &Fetch::None).await.unwrap());
        assert!(store.active().is_none());
        assert_eq!(store.engine().revision, "builtin");
    }

    #[tokio::test]
    async fn rejects_tampered_bytes() {
        let state = tempfile::tempdir().unwrap();
        let store = ContentStore::new(state.path());
        let c = ContentRef {
            digest: "sha256:00".into(),
            ..ContentRef::default()
        };
        store.persist(&c, b"tampered", &[]).unwrap();
        let err = store
            .sync(Some(&c), &policy(), &Fetch::None)
            .await
            .unwrap_err();
        assert!(err.contains("digest mismatch"), "{err}");
    }
}
