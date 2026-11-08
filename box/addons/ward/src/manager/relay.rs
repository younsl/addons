//! The feed relay: the database revisions the rollout names, pulled once
//! from the registry, verified, cached on the manager volume, and served to
//! agents from memory.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

use crate::feed::oci::Registry;
use crate::feed::verify;
use crate::policy::Policy;
use crate::rollout::{ContentRef, Rollout};

const DB: &str = "db.json.gz";
const SIG: &str = "db.sig";

#[derive(Clone)]
pub struct Blob {
    pub revision: String,
    pub db: Arc<Vec<u8>>,
    pub sig: Arc<Vec<u8>>,
}

#[derive(Default)]
pub struct Relay {
    blobs: RwLock<HashMap<String, Blob>>,
}

fn dir(root: &Path, digest: &str) -> PathBuf {
    root.join(digest.replace(':', "-"))
}

impl Relay {
    pub fn get(&self, digest: &str) -> Option<Blob> {
        self.blobs.read().ok()?.get(digest).cloned()
    }

    /// Caches every revision the rollout names and drops the rest.
    pub async fn sync(
        &self,
        root: &Path,
        rollout: &Rollout,
        policy: &Policy,
        registry: &Registry,
    ) -> Result<(), String> {
        let wanted: Vec<&ContentRef> = [rollout.stable.as_ref(), rollout.target.as_ref()]
            .into_iter()
            .flatten()
            .collect();
        let (key, insecure) = policy.feed.as_ref().map_or((None, false), |f| {
            (f.public_key.as_deref(), f.insecure_skip_verify)
        });
        let mut errors = Vec::new();
        for c in &wanted {
            if self.get(&c.digest).is_some() {
                continue;
            }
            let d = dir(root, &c.digest);
            let cached = std::fs::read(d.join(DB))
                .ok()
                .map(|db| (db, std::fs::read(d.join(SIG)).unwrap_or_default()));
            let (db, sig) = match cached {
                Some(hit) => hit,
                None => {
                    let db = registry
                        .blob_from(&c.repository, &c.digest, crate::content::MAX_DB_BYTES)
                        .await;
                    let sig = if c.signature_digest.is_empty() {
                        Ok(Vec::new())
                    } else {
                        registry
                            .blob_from(&c.repository, &c.signature_digest, 64 * 1024)
                            .await
                    };
                    match (db, sig) {
                        (Ok(db), Ok(sig)) => (db, sig),
                        (Err(e), _) | (_, Err(e)) => {
                            errors.push(format!("{}: {e}", c.revision));
                            continue;
                        }
                    }
                }
            };
            if let Err(e) = verify(c, &db, &sig, key, insecure) {
                errors.push(format!("{}: {e}", c.revision));
                continue;
            }
            if std::fs::create_dir_all(&d).is_ok() {
                let _ = std::fs::write(d.join(DB), &db);
                let _ = std::fs::write(d.join(SIG), &sig);
            }
            if let Ok(mut b) = self.blobs.write() {
                b.insert(
                    c.digest.clone(),
                    Blob {
                        revision: c.revision.clone(),
                        db: Arc::new(db),
                        sig: Arc::new(sig),
                    },
                );
            }
        }
        let keep: Vec<String> = wanted.iter().map(|c| c.digest.clone()).collect();
        if let Ok(mut b) = self.blobs.write() {
            b.retain(|k, _| keep.contains(k));
        }
        if let Ok(rd) = std::fs::read_dir(root) {
            let keep: Vec<String> = keep.iter().map(|k| k.replace(':', "-")).collect();
            for e in rd.filter_map(Result::ok) {
                if e.path().is_dir()
                    && !keep.contains(&e.file_name().to_string_lossy().into_owned())
                {
                    let _ = std::fs::remove_dir_all(e.path());
                }
            }
        }
        if errors.is_empty() {
            Ok(())
        } else {
            Err(errors.join("; "))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::content::oci_digest;
    use crate::content::tests::sample_db;
    use crate::crd::{FeedSpec, RolloutSpec};

    #[tokio::test]
    async fn serves_cached_revisions_and_prunes() {
        crate::tls::install_provider();
        let root = tempfile::tempdir().unwrap();
        let bytes = sample_db().encode();
        let c = ContentRef {
            revision: "r1".into(),
            digest: oci_digest(&bytes),
            repository: "localhost:1/x/ward-db".into(),
            ..ContentRef::default()
        };
        let d = dir(root.path(), &c.digest);
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join(DB), &bytes).unwrap();
        std::fs::create_dir_all(root.path().join("sha256-old")).unwrap();
        let policy = Policy {
            feed: Some(FeedSpec {
                image: "localhost:1/x/ward-db".into(),
                public_key: None,
                insecure_skip_verify: true,
                interval: "6h".into(),
                pull_secret_ref: None,
                rollout: RolloutSpec::default(),
            }),
            ..Policy::default()
        };
        let rollout = Rollout {
            stable: Some(c.clone()),
            ..Rollout::default()
        };
        let relay = Relay::default();
        let reg = Registry::new(None).unwrap();
        relay
            .sync(root.path(), &rollout, &policy, &reg)
            .await
            .unwrap();
        assert_eq!(relay.get(&c.digest).unwrap().revision, "r1");
        assert!(!root.path().join("sha256-old").exists());

        let missing = Rollout {
            target: Some(ContentRef {
                revision: "r2".into(),
                digest: "sha256:ffff".into(),
                repository: "localhost:1/x/ward-db".into(),
                ..ContentRef::default()
            }),
            ..rollout
        };
        let err = relay
            .sync(root.path(), &missing, &policy, &reg)
            .await
            .unwrap_err();
        assert!(err.starts_with("r2"), "{err}");
        assert!(relay.get(&c.digest).is_some());
        relay
            .sync(root.path(), &Rollout::default(), &policy, &reg)
            .await
            .unwrap();
        assert!(relay.get(&c.digest).is_none());
    }
}
