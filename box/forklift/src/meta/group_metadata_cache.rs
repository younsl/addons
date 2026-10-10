//! Cached aggregate representations of group repository indexes, reference counted against the
//! blob table like any artifact.

use chrono::Utc;
use rusqlite::{OptionalExtension, params};

use super::publication::{adjust_ref_runner, with_immediate_tx};
use super::{Error, GroupMetadataCache, Result, Store, format_time, now_rfc3339, parse_time};

impl Store {
    /// Returns the cached representation for one group path when it exists,
    /// was built for `config_revision`, and has not expired; anything else is
    /// [`Error::NotFound`] so the caller rebuilds.
    pub async fn get_group_metadata_cache(
        &self,
        group_repo_id: i64,
        path: &str,
        representation: &str,
        config_revision: &str,
    ) -> Result<GroupMetadataCache> {
        let (path, representation, config_revision) = (
            path.to_string(),
            representation.to_string(),
            config_revision.to_string(),
        );
        self.read(move |conn| {
            let cache = conn
                .query_row(
                    "SELECT group_repo_id, path, representation, blob_sha256, size,
		sources_json, config_revision, expires_at, updated_at FROM group_metadata_cache
		WHERE group_repo_id = ? AND path = ? AND representation = ? AND config_revision = ?",
                    params![group_repo_id, path, representation, config_revision],
                    |r| {
                        let expires: String = r.get(7)?;
                        let updated: String = r.get(8)?;
                        Ok(GroupMetadataCache {
                            group_repo_id: r.get(0)?,
                            path: r.get(1)?,
                            representation: r.get(2)?,
                            blob_sha256: r.get(3)?,
                            size: r.get(4)?,
                            sources_json: r.get(5)?,
                            config_revision: r.get(6)?,
                            expires_at: parse_time(&expires),
                            updated_at: parse_time(&updated),
                        })
                    },
                )
                .map_err(|e| Error::sqlite("get group metadata cache", e))?;
            if cache.expires_at <= Utc::now() {
                return Err(Error::NotFound);
            }
            Ok(cache)
        })
        .await
    }

    /// Stores (or replaces) one cached representation, moving the blob
    /// reference from the previous digest to the new one inside an immediate
    /// transaction.
    pub async fn put_group_metadata_cache(&self, cache: GroupMetadataCache) -> Result<()> {
        self.write(move |conn| {
            with_immediate_tx(conn, |q| {
                let old: Option<String> = q
                    .query_row(
                        "SELECT blob_sha256 FROM group_metadata_cache
				WHERE group_repo_id = ? AND path = ? AND representation = ?",
                        params![cache.group_repo_id, cache.path, cache.representation],
                        |r| r.get(0),
                    )
                    .optional()
                    .map_err(|e| Error::sqlite("lookup group metadata cache", e))?;
                let cached_at = now_rfc3339();
                q.execute(
                    "INSERT INTO blobs(sha256, size, ref_count, created_at, unreferenced_since) VALUES(?, ?, 0, ?, ?)
				ON CONFLICT(sha256) DO UPDATE SET size = excluded.size",
                    params![cache.blob_sha256, cache.size, cached_at, cached_at],
                )
                .map_err(|e| Error::sqlite("ensure group metadata blob", e))?;
                if old.as_deref().unwrap_or("") != cache.blob_sha256 {
                    if let Some(old) = &old {
                        adjust_ref_runner(q, old, -1)?;
                    }
                    adjust_ref_runner(q, &cache.blob_sha256, 1)?;
                }
                q.execute(
                    "INSERT INTO group_metadata_cache(group_repo_id, path, representation, blob_sha256,
				size, sources_json, config_revision, expires_at, updated_at) VALUES(?, ?, ?, ?, ?, ?, ?, ?, ?)
				ON CONFLICT(group_repo_id, path, representation) DO UPDATE SET blob_sha256 = excluded.blob_sha256,
				size = excluded.size, sources_json = excluded.sources_json, config_revision = excluded.config_revision,
				expires_at = excluded.expires_at, updated_at = excluded.updated_at",
                    params![
                        cache.group_repo_id,
                        cache.path,
                        cache.representation,
                        cache.blob_sha256,
                        cache.size,
                        cache.sources_json,
                        cache.config_revision,
                        format_time(cache.expires_at),
                        now_rfc3339()
                    ],
                )
                .map(|_| ())
                .map_err(|e| Error::sqlite("put group metadata cache", e))
            })
        })
        .await
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use chrono::{Duration, Utc};

    use crate::meta::*;

    #[tokio::test]
    async fn group_metadata_cache_invalidated_by_member_write() {
        let (store, _dir) = test_store().await;
        let member = store
            .create_repository(Repository {
                name: "npm-hosted".into(),
                format: FORMAT_NPM.into(),
                r#type: TYPE_HOSTED.into(),
                ..Repository::default()
            })
            .await
            .unwrap();
        let group = store
            .create_repository(Repository {
                name: "npm-public".into(),
                format: FORMAT_NPM.into(),
                r#type: TYPE_GROUP.into(),
                config_json: r#"{"group":{"members":["npm-hosted"]}}"#.into(),
                ..Repository::default()
            })
            .await
            .unwrap();
        let cache = GroupMetadataCache {
            group_repo_id: group.id,
            path: "widget".into(),
            representation: "npm:default".into(),
            blob_sha256: "cached-packument".into(),
            size: 12,
            sources_json: "[]".into(),
            config_revision: "r1".into(),
            expires_at: Utc::now() + Duration::hours(1),
            ..GroupMetadataCache::default()
        };
        store.put_group_metadata_cache(cache.clone()).await.unwrap();
        store
            .get_group_metadata_cache(
                group.id,
                &cache.path,
                &cache.representation,
                &cache.config_revision,
            )
            .await
            .unwrap();
        store
            .put_artifact(Artifact {
                repo_id: member.id,
                path: "widget".into(),
                blob_sha256: "new-packument".into(),
                size: 10,
                ..Artifact::default()
            })
            .await
            .unwrap();
        let err = store
            .get_group_metadata_cache(
                group.id,
                &cache.path,
                &cache.representation,
                &cache.config_revision,
            )
            .await
            .unwrap_err();
        assert!(err.is_not_found(), "cache survived member write: {err}");
    }

    #[tokio::test]
    async fn group_metadata_cache_invalidated_by_group_config_update() {
        let (store, _dir) = test_store().await;
        let group = store
            .create_repository(Repository {
                name: "go-public".into(),
                format: FORMAT_GO.into(),
                r#type: TYPE_GROUP.into(),
                config_json: r#"{"group":{"members":["go-hosted"]}}"#.into(),
                ..Repository::default()
            })
            .await
            .unwrap();
        let cache = GroupMetadataCache {
            group_repo_id: group.id,
            path: "example.com/mod/@v/list".into(),
            representation: "go-list:default".into(),
            blob_sha256: "cached-list".into(),
            size: 7,
            sources_json: "[]".into(),
            config_revision: "r1".into(),
            expires_at: Utc::now() + Duration::hours(1),
            ..GroupMetadataCache::default()
        };
        store.put_group_metadata_cache(cache.clone()).await.unwrap();
        store
            .update_repository_config(group.id, "", r#"{"group":{"members":["go-hosted"]}}"#)
            .await
            .unwrap();
        let err = store
            .get_group_metadata_cache(
                group.id,
                &cache.path,
                &cache.representation,
                &cache.config_revision,
            )
            .await
            .unwrap_err();
        assert!(
            err.is_not_found(),
            "cache survived group config update: {err}"
        );
    }
}
