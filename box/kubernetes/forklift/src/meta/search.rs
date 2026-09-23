//! Substring and regex search over artifacts, for the sidebar global search and the
//! per-repository artifact table.
//!

use std::collections::HashMap;

use chrono::{DateTime, Utc};
use regex::Regex;
use rusqlite::types::Value;
use rusqlite::{params, params_from_iter};

use super::artifact::scan_artifact;
use super::repository::query_all;
use super::{Artifact, Error, Result, Store};

/// One artifact matched by a global search, carrying the owning repository so
/// the caller can authorize and link the result.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ArtifactSearchHit {
    pub repo_id: i64,
    pub repo_name: String,
    pub path: String,
    pub size: i64,
}

/// Builds a case-insensitive LIKE pattern matching `q` anywhere, escaping LIKE
/// metacharacters so user input cannot widen the match.
pub(crate) fn like_contains(q: &str) -> String {
    let escaped = q
        .replace('\\', "\\\\")
        .replace('%', "\\%")
        .replace('_', "\\_");
    format!("%{escaped}%")
}

/// Matches the artifact scanner's column order (see `artifact::scan_artifact`).
pub(crate) const ARTIFACT_COLS: &str = "id, repo_id, path, version, blob_sha256, size, content_type, metadata_json, published_at, cached_at, last_accessed_at, updated_at, cached_by, last_accessed_by, COALESCE(publication_id, ''), artifact_role";

/// Renders a stored RFC3339 timestamp column the way the UI shows it
/// ("YYYY-MM-DD HH:MM:SS"), so substring search matches what users see.
pub(crate) fn search_time_expr(col: &str) -> String {
    format!("REPLACE(SUBSTR({col},1,19),'T',' ')")
}

/// [`search_time_expr`] for values already parsed into a timestamp.
pub(crate) fn search_time(t: DateTime<Utc>) -> String {
    t.format("%Y-%m-%d %H:%M:%S").to_string()
}

/// Concatenates the textual columns the artifacts table renders, for regex
/// matching in Rust. Labels are part of the row the reader sees, so they are
/// part of what a search term matches.
fn artifact_search_text(a: &Artifact, labels: &[String]) -> String {
    let mut fields = vec![
        a.path.clone(),
        a.version.clone(),
        a.content_type.clone(),
        a.cached_by.clone(),
        a.last_accessed_by.clone(),
        search_time(a.cached_at),
        search_time(a.last_accessed_at),
    ];
    fields.extend(labels.iter().cloned());
    fields.join("\n")
}

/// Builds the WHERE clause for a repository-scoped artifact substring search.
/// An empty `q` matches every artifact.
fn artifact_search_where(repo_id: i64, q: &str) -> (String, Vec<Value>) {
    let mut where_clause = String::from("repo_id = ?");
    let mut args = vec![Value::Integer(repo_id)];
    if !q.is_empty() {
        let pat = like_contains(q);
        // The label subquery keeps a search term that names a label finding the
        // artifacts carrying it, which is how the labels are meant to be used.
        where_clause.push_str(&format!(
            " AND (path LIKE ? ESCAPE '\\' OR version LIKE ? ESCAPE '\\' OR content_type LIKE ? ESCAPE '\\' OR cached_by LIKE ? ESCAPE '\\' OR last_accessed_by LIKE ? ESCAPE '\\'
		 OR {} LIKE ? ESCAPE '\\' OR {} LIKE ? ESCAPE '\\'
		 OR EXISTS (SELECT 1 FROM artifact_labels l WHERE l.repo_id = artifacts.repo_id AND l.path = artifacts.path AND l.label LIKE ? ESCAPE '\\'))",
            search_time_expr("cached_at"),
            search_time_expr("last_accessed_at")
        ));
        for _ in 0..8 {
            args.push(Value::Text(pat.clone()));
        }
    }
    (where_clause, args)
}

impl Store {
    /// Returns artifacts whose path contains `q`, across all repositories.
    /// Authorization is the caller's job: hits include the repository name so
    /// the API layer can drop repositories the principal cannot read.
    pub async fn search_artifacts(&self, q: &str, limit: i64) -> Result<Vec<ArtifactSearchHit>> {
        let pat = like_contains(q);
        self.read(move |conn| {
            query_all(
                conn,
                "search artifacts",
                "SELECT a.repo_id, r.name, a.path, a.size
		   FROM artifacts a JOIN repositories r ON r.id = a.repo_id
		  WHERE a.path LIKE ? ESCAPE '\\'
		  ORDER BY r.name, a.path LIMIT ?",
                params![pat, limit],
                |r| {
                    Ok(ArtifactSearchHit {
                        repo_id: r.get(0)?,
                        repo_name: r.get(1)?,
                        path: r.get(2)?,
                        size: r.get(3)?,
                    })
                },
            )
        })
        .await
    }

    /// Returns, per repository name, how many artifact paths contain `q`. The
    /// API layer sums the repositories the principal can read to report an
    /// exact total alongside the truncated hit list.
    pub async fn search_artifact_counts_by_repo(&self, q: &str) -> Result<HashMap<String, i64>> {
        let pat = like_contains(q);
        self.read(move |conn| {
            let rows = query_all(
                conn,
                "count artifact hits",
                "SELECT r.name, COUNT(*)
		   FROM artifacts a JOIN repositories r ON r.id = a.repo_id
		  WHERE a.path LIKE ? ESCAPE '\\'
		  GROUP BY r.name",
                params![pat],
                |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)),
            )?;
            Ok(rows.into_iter().collect())
        })
        .await
    }

    /// Returns one page of a repository's artifacts whose rendered columns
    /// (path, version, content type, cached-by, timestamps, labels) contain
    /// `q`, most-recently-accessed first, plus the total match count.
    pub async fn search_repo_artifacts(
        &self,
        repo_id: i64,
        q: &str,
        limit: i64,
        offset: i64,
    ) -> Result<(Vec<Artifact>, i64)> {
        let (where_clause, args) = artifact_search_where(repo_id, q);
        self.search_repo_artifacts_where(where_clause, args, limit, offset)
            .await
    }

    /// [`Store::search_repo_artifacts`] narrowed to artifacts carrying at least
    /// one label, which is the Statistics tab's labeling-coverage drill-down.
    pub async fn search_repo_labeled_artifacts(
        &self,
        repo_id: i64,
        q: &str,
        limit: i64,
        offset: i64,
    ) -> Result<(Vec<Artifact>, i64)> {
        let (mut where_clause, args) = artifact_search_where(repo_id, q);
        where_clause.push_str(
            " AND EXISTS (SELECT 1 FROM artifact_labels l WHERE l.repo_id = artifacts.repo_id AND l.path = artifacts.path)",
        );
        self.search_repo_artifacts_where(where_clause, args, limit, offset)
            .await
    }

    async fn search_repo_artifacts_where(
        &self,
        where_clause: String,
        args: Vec<Value>,
        limit: i64,
        offset: i64,
    ) -> Result<(Vec<Artifact>, i64)> {
        self.read(move |conn| {
            let total: i64 = conn
                .query_row(
                    &format!("SELECT COUNT(*) FROM artifacts WHERE {where_clause}"),
                    params_from_iter(args.iter()),
                    |r| r.get(0),
                )
                .map_err(|e| Error::sqlite("count artifact matches", e))?;
            let mut page_args = args;
            page_args.push(Value::Integer(limit));
            page_args.push(Value::Integer(offset));
            let out = query_all(
                conn,
                "search repo artifacts",
                &format!(
                    "SELECT {ARTIFACT_COLS} FROM artifacts WHERE {where_clause} ORDER BY last_accessed_at DESC, id DESC LIMIT ? OFFSET ?"
                ),
                params_from_iter(page_args.iter()),
                scan_artifact,
            )?;
            Ok((out, total))
        })
        .await
    }

    /// [`Store::search_repo_artifacts`] with `re` matched against the same
    /// rendered columns. Matching runs in Rust (SQLite carries no regexp
    /// function), streaming rows most-recently-accessed first and keeping only
    /// the requested page, so memory stays bounded on large repositories.
    pub async fn search_repo_artifacts_regex(
        &self,
        repo_id: i64,
        re: &Regex,
        limit: i64,
        offset: i64,
    ) -> Result<(Vec<Artifact>, i64)> {
        let re = re.clone();
        self.read(move |conn| {
            // SQLite cannot run the regex, so the labels cannot be matched in a
            // subquery either; the repository's labels are read once up front and
            // matched alongside the row's own columns.
            let mut labels: HashMap<String, Vec<String>> = HashMap::new();
            for (path, label) in query_all(
                conn,
                "list repository artifact labels",
                "SELECT path, label FROM artifact_labels WHERE repo_id = ? ORDER BY id",
                params![repo_id],
                |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)),
            )? {
                labels.entry(path).or_default().push(label);
            }
            let mut stmt = conn
                .prepare(&format!(
                    "SELECT {ARTIFACT_COLS} FROM artifacts WHERE repo_id = ? ORDER BY last_accessed_at DESC, id DESC"
                ))
                .map_err(|e| Error::sqlite("search repo artifacts", e))?;
            let rows = stmt
                .query_map(params![repo_id], scan_artifact)
                .map_err(|e| Error::sqlite("search repo artifacts", e))?;
            let mut total: i64 = 0;
            let mut out: Vec<Artifact> = Vec::new();
            for row in rows {
                let a = row.map_err(|e| Error::sqlite("scan artifact match", e))?;
                let row_labels = labels.get(&a.path).map(Vec::as_slice).unwrap_or(&[]);
                if !re.is_match(&artifact_search_text(&a, row_labels)) {
                    continue;
                }
                if total >= offset && (out.len() as i64) < limit {
                    out.push(a);
                }
                total += 1;
            }
            Ok((out, total))
        })
        .await
    }
}
