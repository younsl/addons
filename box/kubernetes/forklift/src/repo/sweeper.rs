//! Reclaims blob bytes whose reference count has dropped to zero.

use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};
use tokio_util::sync::CancellationToken;

use super::Engine;

impl Engine {
    /// Periodically reclaims blob bytes whose reference count has dropped to
    /// zero (after artifact deletion or cache eviction). It is leader-gated by
    /// the caller in HA mode so only one instance mutates the blob store.
    ///
    /// `grace` is how long a digest must have been unreferenced before its bytes
    /// may be reclaimed. Deleting bytes is irreversible while the metadata
    /// database is only asynchronously durable in the s3 backend (see
    /// `src/objstore/`), so the delay must outlast the replication lag:
    /// otherwise a rollback can restore an artifact row whose bytes are already
    /// gone, leaving a dangling reference that no code path can repair.
    pub async fn run_sweeper(
        self: Arc<Self>,
        cancel: CancellationToken,
        interval: Duration,
        grace: Duration,
    ) {
        let mut ticker = tokio::time::interval(interval);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        ticker.tick().await;
        loop {
            tokio::select! {
                _ = cancel.cancelled() => return,
                _ = ticker.tick() => self.sweep_once(grace).await,
            }
        }
    }

    pub(crate) async fn sweep_once(&self, grace: Duration) {
        let cutoff =
            self.now() - chrono::TimeDelta::from_std(grace).unwrap_or(chrono::TimeDelta::zero());
        let shas = match self.store.list_unreferenced_blobs(256, cutoff).await {
            Ok(shas) => shas,
            Err(err) => {
                tracing::error!(err = %err, "sweeper list failed");
                return;
            }
        };
        if shas.is_empty() {
            return;
        }
        let mut reclaimed = 0;
        for sha in &shas {
            if self.reclaim_blob(sha, cutoff).await {
                reclaimed += 1;
            }
        }
        if reclaimed > 0 {
            tracing::debug!(count = reclaimed, "sweeper reclaimed blobs");
        }
    }

    /// Deletes one unreferenced digest's record and then its bytes, and reports
    /// whether the bytes were reclaimed.
    ///
    /// The exclusion against reference-creating writes (`put` -> `put_artifact`,
    /// which hold the `gc_mu` read guard) is per digest, not per batch: the
    /// invariant that needs the lock is that no writer can re-reference a digest
    /// between the guarded record delete and the byte delete, and that window is
    /// one iteration. Locking the whole batch instead would stop every upload
    /// and proxy cache write for as long as a batch of blob deletions takes,
    /// which on an object-store backend is one API round-trip per digest.
    pub(crate) async fn reclaim_blob(&self, sha: &str, cutoff: DateTime<Utc>) -> bool {
        let _gc = self.gc_mu.write().await;
        // Delete the DB record first, guarded by ref_count <= 0 and the same
        // grace cutoff. If the digest was re-referenced since it was listed, the
        // guarded delete removes no row and we must leave the bytes: a live
        // artifact now points at them. Reclaim the bytes only after the record
        // is gone.
        match self.store.delete_blob_record(sha, cutoff).await {
            Ok(false) => return false,
            Ok(true) => {}
            Err(err) => {
                tracing::error!(sha, err = %err, "sweeper record delete failed");
                return false;
            }
        }
        if let Err(err) = self.blobs.delete(sha).await {
            tracing::error!(sha, err = %err, "sweeper blob delete failed");
            return false;
        }
        true
    }
}

#[cfg(test)]
pub(crate) mod tests {
    //!
    //! They store the artifact through `Engine::put` instead — the same code path the handler calls
    //! — and read it back through the store rather than the router.

    use std::time::Duration;

    use crate::meta;
    use crate::repoconfig::Config;

    use crate::testing::repo::{body, mk_repo, new_test_manager};

    /// The happy path: once an artifact is deleted and its blob's `ref_count` drops
    /// to zero, `sweep_once` removes both the blob record and its bytes.
    #[tokio::test]
    async fn sweeper_reclaims_unreferenced() {
        let tm = new_test_manager().await;
        let repo = mk_repo(
            &tm.store,
            "mvn-sweep",
            meta::TYPE_HOSTED,
            "",
            Config::default(),
        )
        .await;
        let path = "com/example/app/1.0/app-1.0.jar";

        tm.engine
            .put(
                &repo,
                path,
                "1.0",
                "application/java-archive",
                None,
                body("SWEEPBYTES"),
                "",
            )
            .await
            .expect("put");
        let art = tm
            .store
            .get_artifact(repo.id, path)
            .await
            .expect("get artifact");
        let digest = art.blob_sha256.clone();

        // Delete the artifact so the blob becomes unreferenced.
        tm.store
            .delete_artifact(repo.id, path)
            .await
            .expect("delete artifact");
        let shas = tm
            .store
            .list_unreferenced_blobs(10, chrono::Utc::now())
            .await
            .expect("list unreferenced");
        assert_eq!(shas, vec![digest.clone()], "unreferenced blobs");

        tm.engine.sweep_once(Duration::ZERO).await;

        assert!(
            tm.store.get_blob(&digest).await.is_err(),
            "blob record still present after sweep"
        );
        assert!(
            !tm.engine.blobs.exists(&digest).await.expect("exists"),
            "blob bytes still present after sweep"
        );
    }

    /// The sweeper never reclaims a blob that is still referenced: the guarded
    /// `delete_blob_record` removes no row, so the bytes stay and the artifact
    /// remains servable. This is the invariant the reordered delete (record first,
    /// bytes only when a row was actually removed) protects.
    #[tokio::test]
    async fn sweeper_keeps_referenced_blob() {
        let tm = new_test_manager().await;
        let repo = mk_repo(
            &tm.store,
            "mvn-keep",
            meta::TYPE_HOSTED,
            "",
            Config::default(),
        )
        .await;
        let path = "com/example/app/1.0/app-1.0.jar";

        tm.engine
            .put(
                &repo,
                path,
                "1.0",
                "application/java-archive",
                None,
                body("KEEPBYTES"),
                "",
            )
            .await
            .expect("put");
        let art = tm
            .store
            .get_artifact(repo.id, path)
            .await
            .expect("get artifact");

        tm.engine.sweep_once(Duration::ZERO).await;

        tm.store
            .get_blob(&art.blob_sha256)
            .await
            .expect("referenced blob record removed by sweep");
        assert!(
            tm.engine
                .blobs
                .exists(&art.blob_sha256)
                .await
                .expect("exists"),
            "referenced blob bytes removed by sweep"
        );
        // The artifact is still readable end to end.
        let (mut reader, size) = tm
            .engine
            .blobs
            .open(&art.blob_sha256)
            .await
            .expect("open blob");
        let mut buf = Vec::new();
        tokio::io::AsyncReadExt::read_to_end(&mut reader, &mut buf)
            .await
            .expect("read blob");
        assert_eq!(buf, b"KEEPBYTES");
        assert_eq!(size, 9);
    }
}
