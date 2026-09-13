//! Per-artifact operator labels. `ErrLabelLimit` is [`Error::LabelLimit`].

use std::collections::HashMap;

use chrono::{DateTime, Utc};
use rusqlite::{Row, params};

use super::search::like_contains;
use super::time::{now_rfc3339, parse_time};
use super::{Error, Result, Store};

/// How many labels one artifact may carry. Labels are an operator annotation,
/// not a data store; the cap keeps one path from growing an unbounded list
/// that no listing can render.
pub const MAX_ARTIFACT_LABELS: i64 = 20;

/// The longest accepted label value.
pub const MAX_ARTIFACT_LABEL_LEN: usize = 64;

/// One operator tag on a stored artifact, identified the way every format
/// identifies an artifact: repository plus path.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ArtifactLabel {
    pub repo_id: i64,
    pub path: String,
    pub label: String,
    pub created_by: String,
    pub created_at: DateTime<Utc>,
}

/// One label matched by a global search, carrying the owning repository so the
/// caller can authorize and link the result.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ArtifactLabelHit {
    pub repo_id: i64,
    pub repo_name: String,
    pub path: String,
    pub label: String,
}

/// Folds a label to its stored form. Only surrounding whitespace is removed:
/// the accepted charset includes both cases, so casing is the author's to
/// choose and is stored as typed.
pub fn normalize_artifact_label(label: &str) -> String {
    label.trim().to_string()
}

/// Reports whether a normalized label is acceptable: either a bare key or a
/// "key:value" pair, where key and value are 1 or more ASCII letters, digits,
/// '-' or '_', and the whole label is at most [`MAX_ARTIFACT_LABEL_LEN`] bytes.
///
/// The charset is deliberately narrow. A label travels through search terms,
/// URLs and CLI output, so anything that would need quoting or escaping there
/// is out, and the single ':' is the only structure, which keeps "key:value"
/// unambiguous.
pub fn valid_artifact_label(label: &str) -> bool {
    if label.is_empty() || label.len() > MAX_ARTIFACT_LABEL_LEN {
        return false;
    }
    match label.split_once(':') {
        Some((key, value)) => valid_label_part(key) && valid_label_part(value),
        None => valid_label_part(label),
    }
}

/// Reports whether one side of a label is a non-empty run of ASCII letters,
/// digits, '-' and '_'.
fn valid_label_part(part: &str) -> bool {
    !part.is_empty()
        && part
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

impl Store {
    /// Attaches a label to the artifact at (`repo_id`, `path`). The caller has
    /// already normalized and authorized it. [`Error::NotFound`] means there is
    /// no such artifact, [`Error::Conflict`] that the label is already there,
    /// and [`Error::LabelLimit`] that the artifact is full.
    pub async fn add_artifact_label(&self, mut l: ArtifactLabel) -> Result<ArtifactLabel> {
        let row = l.clone();
        let now = self
            .write(move |conn| {
                let tx = conn
                    .transaction()
                    .map_err(|e| Error::sqlite("begin artifact label", e))?;

                // The insert's foreign key would refuse a missing artifact
                // anyway; checking first turns "FOREIGN KEY constraint failed"
                // into the 404 the API reports.
                let exists: i64 = tx
                    .query_row(
                        "SELECT COUNT(*) FROM artifacts WHERE repo_id = ? AND path = ?",
                        params![row.repo_id, row.path],
                        |r| r.get(0),
                    )
                    .map_err(|e| Error::sqlite("check artifact for label", e))?;
                if exists == 0 {
                    return Err(Error::NotFound);
                }
                let count: i64 = tx
                    .query_row(
                        "SELECT COUNT(*) FROM artifact_labels WHERE repo_id = ? AND path = ?",
                        params![row.repo_id, row.path],
                        |r| r.get(0),
                    )
                    .map_err(|e| Error::sqlite("count artifact labels", e))?;
                if count >= MAX_ARTIFACT_LABELS {
                    return Err(Error::LabelLimit);
                }
                let now = now_rfc3339();
                tx.execute(
                    "INSERT INTO artifact_labels(repo_id, path, label, created_by, created_at) VALUES(?, ?, ?, ?, ?)",
                    params![row.repo_id, row.path, row.label, row.created_by, now],
                )
                .map_err(|e| Error::sqlite("insert artifact label", e))?;
                tx.commit()
                    .map_err(|e| Error::sqlite("commit artifact label", e))?;
                Ok(now)
            })
            .await?;
        l.created_at = parse_time(&now);
        Ok(l)
    }

    /// Removes one label from an artifact, reporting [`Error::NotFound`] when it
    /// was not there.
    pub async fn delete_artifact_label(&self, repo_id: i64, path: &str, label: &str) -> Result<()> {
        let path = path.to_string();
        let label = label.to_string();
        self.write(move |conn| {
            let n = conn
                .execute(
                    "DELETE FROM artifact_labels WHERE repo_id = ? AND path = ? AND label = ?",
                    params![repo_id, path, label],
                )
                .map_err(|e| Error::sqlite("delete artifact label", e))?;
            if n == 0 {
                return Err(Error::NotFound);
            }
            Ok(())
        })
        .await
    }

    /// Returns one artifact's labels, oldest first.
    pub async fn list_artifact_labels(
        &self,
        repo_id: i64,
        path: &str,
    ) -> Result<Vec<ArtifactLabel>> {
        let path = path.to_string();
        self.read(move |conn| {
            let mut stmt = conn
                .prepare(
                    "SELECT repo_id, path, label, created_by, created_at
		   FROM artifact_labels WHERE repo_id = ? AND path = ? ORDER BY id",
                )
                .map_err(|e| Error::sqlite("list artifact labels", e))?;
            let out = stmt
                .query_map(params![repo_id, path], scan_artifact_label)
                .map_err(|e| Error::sqlite("list artifact labels", e))?
                .collect::<rusqlite::Result<Vec<_>>>()
                .map_err(|e| Error::sqlite("scan artifact label", e))?;
            Ok(out)
        })
        .await
    }

    /// Returns every label in a repository keyed by artifact path. The artifact
    /// browser renders a page of rows at a time; one query for the repository
    /// keeps that from turning into a query per row.
    pub async fn artifact_labels_by_path(
        &self,
        repo_id: i64,
    ) -> Result<HashMap<String, Vec<ArtifactLabel>>> {
        self.read(move |conn| {
            let mut stmt = conn
                .prepare(
                    "SELECT repo_id, path, label, created_by, created_at
		   FROM artifact_labels WHERE repo_id = ? ORDER BY id",
                )
                .map_err(|e| Error::sqlite("list repository artifact labels", e))?;
            let rows = stmt
                .query_map(params![repo_id], scan_artifact_label)
                .map_err(|e| Error::sqlite("list repository artifact labels", e))?;
            let mut out: HashMap<String, Vec<ArtifactLabel>> = HashMap::new();
            for row in rows {
                let l = row.map_err(|e| Error::sqlite("scan artifact label", e))?;
                out.entry(l.path.clone()).or_default().push(l);
            }
            Ok(out)
        })
        .await
    }

    /// Returns labels whose text contains `q`, across every repository, so the
    /// sidebar search can offer them as their own section. Authorization is
    /// the caller's job: each hit names its repository.
    pub async fn search_artifact_labels(
        &self,
        q: &str,
        limit: i64,
    ) -> Result<Vec<ArtifactLabelHit>> {
        let pattern = like_contains(q);
        self.read(move |conn| {
            let mut stmt = conn
                .prepare(
                    "SELECT l.repo_id, r.name, l.path, l.label
		   FROM artifact_labels l JOIN repositories r ON r.id = l.repo_id
		  WHERE l.label LIKE ? ESCAPE '\\'
		  ORDER BY l.label, r.name, l.path LIMIT ?",
                )
                .map_err(|e| Error::sqlite("search artifact labels", e))?;
            let out = stmt
                .query_map(params![pattern, limit], |r| {
                    Ok(ArtifactLabelHit {
                        repo_id: r.get(0)?,
                        repo_name: r.get(1)?,
                        path: r.get(2)?,
                        label: r.get(3)?,
                    })
                })
                .map_err(|e| Error::sqlite("search artifact labels", e))?
                .collect::<rusqlite::Result<Vec<_>>>()
                .map_err(|e| Error::sqlite("scan artifact label hit", e))?;
            Ok(out)
        })
        .await
    }

    /// Returns, per repository name, how many labels contain `q`, so the caller
    /// can report an exact total over the repositories the principal may read
    /// alongside a truncated hit list.
    pub async fn search_artifact_label_counts_by_repo(
        &self,
        q: &str,
    ) -> Result<HashMap<String, i64>> {
        let pattern = like_contains(q);
        self.read(move |conn| {
            let mut stmt = conn
                .prepare(
                    "SELECT r.name, COUNT(*)
		   FROM artifact_labels l JOIN repositories r ON r.id = l.repo_id
		  WHERE l.label LIKE ? ESCAPE '\\'
		  GROUP BY r.name",
                )
                .map_err(|e| Error::sqlite("count artifact label hits", e))?;
            let rows = stmt
                .query_map(params![pattern], |r| {
                    Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?))
                })
                .map_err(|e| Error::sqlite("count artifact label hits", e))?;
            let mut out = HashMap::new();
            for row in rows {
                let (name, n) = row.map_err(|e| Error::sqlite("scan artifact label count", e))?;
                out.insert(name, n);
            }
            Ok(out)
        })
        .await
    }
}

fn scan_artifact_label(r: &Row<'_>) -> rusqlite::Result<ArtifactLabel> {
    let created: String = r.get(4)?;
    Ok(ArtifactLabel {
        repo_id: r.get(0)?,
        path: r.get(1)?,
        label: r.get(2)?,
        created_by: r.get(3)?,
        created_at: parse_time(&created),
    })
}

#[cfg(test)]
pub(crate) mod tests {
    //! Artifact label persistence and bulk operation tests.

    use rusqlite::params;

    use crate::meta::time::now_rfc3339;
    use crate::meta::*;

    const LABEL_PATH: &str = "com/example/app/1.0.0/app-1.0.0.jar";

    /// Creates a repository with one artifact and returns both ids, which is the
    /// state every label operation needs.
    async fn seed_label_artifact(s: &Store) -> (i64, String) {
        let repo_id = insert_repository(s, "maven-hosted", FORMAT_MAVEN, TYPE_HOSTED).await;
        insert_artifact(s, repo_id, LABEL_PATH, "sha-app", 12, "bob").await;
        (repo_id, LABEL_PATH.to_string())
    }

    /// Inserts a repository row directly, returning its id.
    async fn insert_repository(s: &Store, name: &str, format: &str, r#type: &str) -> i64 {
        let name = name.to_string();
        let format = format.to_string();
        let kind = r#type.to_string();
        s.write(move |conn| {
        let now = now_rfc3339();
        conn.execute(
            "INSERT INTO repositories(name, format, type, upstream_url, config_json, created_at, updated_at)
             VALUES(?, ?, ?, '', '{}', ?, ?)",
            params![name, format, kind, now, now],
        )
        .map_err(|e| Error::sqlite("seed repository", e))?;
        Ok(conn.last_insert_rowid())
    })
    .await
    .expect("seed repository")
    }

    /// Inserts a blob and the artifact row referencing it.
    async fn insert_artifact(s: &Store, repo_id: i64, path: &str, sha: &str, size: i64, by: &str) {
        let path = path.to_string();
        let sha = sha.to_string();
        let by = by.to_string();
        s.write(move |conn| {
        let now = now_rfc3339();
        conn.execute(
            "INSERT INTO blobs(sha256, size, ref_count, created_at) VALUES(?, ?, 1, ?)
             ON CONFLICT(sha256) DO UPDATE SET ref_count = ref_count + 1",
            params![sha, size, now],
        )
        .map_err(|e| Error::sqlite("seed blob", e))?;
        conn.execute(
            "INSERT INTO artifacts(repo_id, path, version, blob_sha256, size, cached_at, last_accessed_at, updated_at, cached_by)
             VALUES(?, ?, '1.0.0', ?, ?, ?, ?, ?, ?)",
            params![repo_id, path, sha, size, now, now, now, by],
        )
        .map_err(|e| Error::sqlite("seed artifact", e))?;
        Ok(())
    })
    .await
    .expect("seed artifact")
    }

    /// Removes an artifact row, which is what every removal path (delete, force
    /// delete, purge, idle reaper, LRU eviction) ends in.
    async fn delete_artifact_row(s: &Store, repo_id: i64, path: &str) {
        let path = path.to_string();
        s.write(move |conn| {
            conn.execute(
                "DELETE FROM artifacts WHERE repo_id = ? AND path = ?",
                params![repo_id, path],
            )
            .map(|_| ())
            .map_err(|e| Error::sqlite("delete artifact", e))
        })
        .await
        .expect("delete artifact")
    }

    #[tokio::test]
    async fn artifact_label_lifecycle() {
        let (s, _dir) = test_store().await;
        let (repo_id, path) = seed_label_artifact(&s).await;

        let stored = s
            .add_artifact_label(ArtifactLabel {
                repo_id,
                path: path.clone(),
                label: "keep-forever".into(),
                created_by: "bob".into(),
                ..ArtifactLabel::default()
            })
            .await
            .expect("add");
        assert!(
            !time::is_zero(stored.created_at),
            "stored label = {stored:?}"
        );
        assert_eq!(stored.created_by, "bob", "stored label = {stored:?}");

        let labels = s.list_artifact_labels(repo_id, &path).await.unwrap();
        assert_eq!(labels.len(), 1, "list = {labels:?}");
        assert_eq!(labels[0].label, "keep-forever", "list = {labels:?}");

        // The same label twice is a conflict, not a second row.
        let err = s
            .add_artifact_label(ArtifactLabel {
                repo_id,
                path: path.clone(),
                label: "keep-forever".into(),
                ..ArtifactLabel::default()
            })
            .await
            .unwrap_err();
        assert!(err.is_conflict(), "duplicate = {err}, want Conflict");

        // A path with no artifact is a 404 rather than a foreign-key error.
        let err = s
            .add_artifact_label(ArtifactLabel {
                repo_id,
                path: "nope.jar".into(),
                label: "x".into(),
                ..ArtifactLabel::default()
            })
            .await
            .unwrap_err();
        assert!(
            err.is_not_found(),
            "unknown artifact = {err}, want NotFound"
        );

        s.delete_artifact_label(repo_id, &path, "keep-forever")
            .await
            .expect("delete");
        let err = s
            .delete_artifact_label(repo_id, &path, "keep-forever")
            .await
            .unwrap_err();
        assert!(err.is_not_found(), "delete again = {err}, want NotFound");
    }

    #[tokio::test]
    async fn artifact_label_limit() {
        let (s, _dir) = test_store().await;
        let (repo_id, path) = seed_label_artifact(&s).await;

        for i in 0..MAX_ARTIFACT_LABELS {
            s.add_artifact_label(ArtifactLabel {
                repo_id,
                path: path.clone(),
                label: format!("tag-{i}"),
                ..ArtifactLabel::default()
            })
            .await
            .unwrap_or_else(|e| panic!("add {i}: {e}"));
        }
        let err = s
            .add_artifact_label(ArtifactLabel {
                repo_id,
                path: path.clone(),
                label: "one-too-many".into(),
                ..ArtifactLabel::default()
            })
            .await
            .unwrap_err();
        assert!(
            matches!(err, Error::LabelLimit),
            "over limit = {err}, want LabelLimit"
        );
    }

    /// A label must not outlive the artifact it describes: the foreign key removes
    /// it with the artifact, whichever removal path ran.
    #[tokio::test]
    async fn artifact_labels_removed_with_artifact() {
        let (s, _dir) = test_store().await;
        let (repo_id, path) = seed_label_artifact(&s).await;
        s.add_artifact_label(ArtifactLabel {
            repo_id,
            path: path.clone(),
            label: "audited".into(),
            ..ArtifactLabel::default()
        })
        .await
        .expect("add");
        delete_artifact_row(&s, repo_id, &path).await;
        let labels = s.list_artifact_labels(repo_id, &path).await.expect("list");
        assert!(
            labels.is_empty(),
            "labels survived the artifact: {labels:?}"
        );
    }

    #[tokio::test]
    async fn artifact_label_normalize_and_validate() {
        // Trimmed, but never re-cased: both cases are in the charset.
        for (input, want, valid) in [
            ("  Team:Payments ", "Team:Payments", true),
            ("keep-forever", "keep-forever", true),
            ("release_1", "release_1", true),
            ("A1", "A1", true),
            ("", "", false),
            ("has space", "has space", false),
            ("sbom/verified", "sbom/verified", false),
            ("release.1", "release.1", false),
            ("team:", "team:", false),
            (":payments", ":payments", false),
            ("a:b:c", "a:b:c", false),
            ("quote\"d", "quote\"d", false),
            ("한글", "한글", false),
        ] {
            let got = normalize_artifact_label(input);
            assert_eq!(got, want, "normalize_artifact_label({input:?})");
            assert_eq!(
                valid_artifact_label(&got),
                valid,
                "valid_artifact_label({got:?})"
            );
        }
        let long = "a".repeat(MAX_ARTIFACT_LABEL_LEN + 1);
        assert!(
            !valid_artifact_label(&long),
            "a label longer than {MAX_ARTIFACT_LABEL_LEN} characters was accepted"
        );
    }

    /// Labels are searchable globally (the sidebar's own section), with an exact
    /// per-repository count alongside the truncated hit list.
    ///
    #[tokio::test]
    async fn artifact_label_search() {
        let (s, _dir) = test_store().await;
        let (repo_id, path) = seed_label_artifact(&s).await;
        insert_artifact(
            &s,
            repo_id,
            "com/example/other/1.0.0/other-1.0.0.jar",
            "sha-other",
            8,
            "",
        )
        .await;
        s.add_artifact_label(ArtifactLabel {
            repo_id,
            path: path.clone(),
            label: "keep-forever".into(),
            created_by: "bob".into(),
            ..ArtifactLabel::default()
        })
        .await
        .expect("add");

        let hits = s.search_artifact_labels("keep", 10).await.unwrap();
        assert_eq!(hits.len(), 1, "global label search = {hits:?}");
        assert_eq!(hits[0].repo_name, "maven-hosted", "hit = {:?}", hits[0]);
        assert_eq!(hits[0].path, path, "hit = {:?}", hits[0]);
        assert_eq!(hits[0].label, "keep-forever", "hit = {:?}", hits[0]);

        let counts = s
            .search_artifact_label_counts_by_repo("keep")
            .await
            .unwrap();
        assert_eq!(counts.get("maven-hosted"), Some(&1), "label counts");

        // A label on one artifact must not widen the result to its neighbours.
        let hits = s.search_artifact_labels("other", 10).await.unwrap();
        assert!(hits.is_empty(), "unrelated search = {hits:?}");
    }

    #[tokio::test]
    async fn artifact_labels_by_path_groups_per_artifact() {
        let (s, _dir) = test_store().await;
        let (repo_id, path) = seed_label_artifact(&s).await;
        let other = "com/example/other/1.0.0/other-1.0.0.jar";
        insert_artifact(&s, repo_id, other, "sha-other", 8, "").await;

        for label in ["team_a", "keep"] {
            s.add_artifact_label(ArtifactLabel {
                repo_id,
                path: path.clone(),
                label: label.into(),
                ..ArtifactLabel::default()
            })
            .await
            .unwrap();
        }
        s.add_artifact_label(ArtifactLabel {
            repo_id,
            path: other.into(),
            label: "team_b".into(),
            ..ArtifactLabel::default()
        })
        .await
        .unwrap();

        let by_path = s.artifact_labels_by_path(repo_id).await.unwrap();
        assert_eq!(by_path.len(), 2);
        // Oldest first within a path, which is insertion order.
        let mine: Vec<&str> = by_path[&path].iter().map(|l| l.label.as_str()).collect();
        assert_eq!(mine, vec!["team_a", "keep"]);
        assert_eq!(by_path[other].len(), 1);
    }

    #[tokio::test]
    async fn artifact_label_search_escapes_wildcards() {
        let (s, _dir) = test_store().await;
        let (repo_id, path) = seed_label_artifact(&s).await;
        s.add_artifact_label(ArtifactLabel {
            repo_id,
            path,
            label: "keep-forever".into(),
            ..ArtifactLabel::default()
        })
        .await
        .unwrap();

        let hits = s.search_artifact_labels("%", 10).await.unwrap();
        assert!(hits.is_empty(), "a bare % must not match everything");
        let hits = s.search_artifact_labels("kee_", 10).await.unwrap();
        assert!(hits.is_empty(), "_ must not act as a single-char wildcard");
    }
}
