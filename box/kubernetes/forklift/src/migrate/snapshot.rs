//! The source metadata snapshot, staged locally so the bytes that are checked
//! are the bytes that are uploaded.

use std::collections::BTreeMap;
use std::path::Path;

use rusqlite::{Connection, OpenFlags, OptionalExtension};
use tokio::io::AsyncWriteExt;

use super::{Endpoint, Error, Result};

/// Every table whose rows name a blob the store must hold.
const BLOB_REFERENCES: &[(&str, &str, Option<&str>)] = &[
    ("blobs", "sha256", Some("size")),
    ("artifacts", "blob_sha256", None),
    ("group_metadata_cache", "blob_sha256", None),
    ("artifact_upload_staged_blobs", "sha256", Some("size")),
];

pub struct StagedSnapshot {
    pub file: tempfile::NamedTempFile,
    pub size: u64,
    pub etag: Option<String>,
    /// Digest to size; `None` when only a table without a size column names it.
    pub blobs: BTreeMap<String, Option<i64>>,
    /// Digests named by a row but absent from `blobs`.
    pub unrecorded: usize,
}

pub async fn download(src: &Endpoint, staging: &Path) -> Result<StagedSnapshot> {
    let mut obj = src.objects.get_object(&src.bucket, &src.meta_key).await?;
    let file = tempfile::NamedTempFile::new_in(staging).map_err(|e| io("stage metadata", e))?;
    let mut out = tokio::fs::File::create(file.path())
        .await
        .map_err(|e| io("stage metadata", e))?;
    let size = tokio::io::copy(&mut obj.body, &mut out)
        .await
        .map_err(|e| io("download metadata", e))?;
    out.flush().await.map_err(|e| io("stage metadata", e))?;
    out.sync_all().await.map_err(|e| io("stage metadata", e))?;
    if let Some(want) = obj.content_length
        && want != size as i64
    {
        return Err(Error::Refused(format!(
            "metadata snapshot download is truncated ({size} of {want} bytes)"
        )));
    }
    tracing::info!(
        size = %super::preflight::bytes(size),
        "checking the metadata snapshot's integrity"
    );
    let path = file.path().to_path_buf();
    let (blobs, unrecorded) = tokio::task::spawn_blocking(move || inspect(&path))
        .await
        .map_err(|e| Error::Refused(format!("inspect metadata: {e}")))??;
    Ok(StagedSnapshot {
        file,
        size,
        etag: obj.e_tag,
        blobs,
        unrecorded,
    })
}

type Inspected = (BTreeMap<String, Option<i64>>, usize);

pub(crate) fn inspect(path: &Path) -> Result<Inspected> {
    let sql = |e: rusqlite::Error| {
        Error::Refused(format!(
            "metadata snapshot is not a readable SQLite database: {e}"
        ))
    };
    let conn = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY).map_err(sql)?;
    let verdict: String = conn
        .query_row("PRAGMA integrity_check", [], |r| r.get(0))
        .map_err(sql)?;
    if verdict != "ok" {
        return Err(Error::Refused(format!(
            "metadata snapshot failed integrity_check: {verdict}"
        )));
    }
    let has_table = |name: &str| -> Result<bool> {
        conn.query_row(
            "SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?",
            [name],
            |_| Ok(()),
        )
        .optional()
        .map(|o| o.is_some())
        .map_err(sql)
    };
    if !has_table("blobs")? {
        return Err(Error::Refused(
            "metadata snapshot has no blobs table; it is not a forklift database".into(),
        ));
    }
    let mut out: BTreeMap<String, Option<i64>> = BTreeMap::new();
    let mut recorded = 0usize;
    for (table, column, size) in BLOB_REFERENCES {
        if !has_table(table)? {
            continue;
        }
        let query = match size {
            Some(size) => format!("SELECT DISTINCT {column}, {size} FROM {table}"),
            None => format!("SELECT DISTINCT {column}, NULL FROM {table}"),
        };
        let mut stmt = conn.prepare(&query).map_err(sql)?;
        let rows = stmt
            .query_map([], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, Option<i64>>(1)?))
            })
            .map_err(sql)?;
        for row in rows {
            let (digest, size) = row.map_err(sql)?;
            if !crate::storage::valid_digest(&digest) {
                return Err(Error::Refused(format!(
                    "metadata snapshot holds an invalid digest {digest:?} in {table}.{column}"
                )));
            }
            let slot = out.entry(digest).or_insert(None);
            if slot.is_none() {
                *slot = size;
            }
        }
        if *table == "blobs" {
            recorded = out.len();
        }
    }
    let total = out.len();
    Ok((out, total - recorded))
}

fn io(op: &'static str, source: std::io::Error) -> Error {
    Error::Io { op, source }
}

#[cfg(test)]
pub(crate) mod tests {
    use crate::migrate::snapshot::*;

    pub(crate) fn digest(n: u8) -> String {
        format!("{:064x}", n)
    }

    pub(crate) fn database(path: &Path, blobs: &[(String, i64)], dangling: &[String]) {
        let conn = Connection::open(path).unwrap();
        conn.execute_batch(
            "CREATE TABLE blobs (sha256 TEXT PRIMARY KEY, size INTEGER, ref_count INTEGER, created_at TEXT);
             CREATE TABLE artifacts (id INTEGER PRIMARY KEY, blob_sha256 TEXT);
             CREATE TABLE artifact_upload_staged_blobs (upload_id TEXT, sha256 TEXT, size INTEGER);",
        )
        .unwrap();
        for (d, size) in blobs {
            conn.execute(
                "INSERT INTO blobs VALUES (?, ?, 1, '')",
                rusqlite::params![d, size],
            )
            .unwrap();
            conn.execute("INSERT INTO artifacts (blob_sha256) VALUES (?)", [d])
                .unwrap();
        }
        for d in dangling {
            conn.execute("INSERT INTO artifacts (blob_sha256) VALUES (?)", [d])
                .unwrap();
        }
    }

    #[test]
    fn collects_every_reference_with_sizes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("db");
        database(&path, &[(digest(1), 10), (digest(2), 20)], &[digest(3)]);
        let conn = Connection::open(&path).unwrap();
        conn.execute(
            "INSERT INTO artifact_upload_staged_blobs VALUES ('u', ?, 30)",
            [digest(4)],
        )
        .unwrap();
        drop(conn);

        let (blobs, unrecorded) = inspect(&path).unwrap();
        assert_eq!(blobs.len(), 4);
        assert_eq!(blobs[&digest(1)], Some(10));
        assert_eq!(blobs[&digest(3)], None, "artifact without a blobs row");
        assert_eq!(blobs[&digest(4)], Some(30));
        assert_eq!(unrecorded, 2);
    }

    #[test]
    fn rejects_garbage_and_foreign_databases() {
        let dir = tempfile::tempdir().unwrap();
        let garbage = dir.path().join("garbage");
        std::fs::write(&garbage, b"not sqlite at all, definitely not").unwrap();
        let err = inspect(&garbage).unwrap_err().to_string();
        assert!(err.contains("not a readable SQLite"), "{err}");

        let foreign = dir.path().join("foreign");
        Connection::open(&foreign)
            .unwrap()
            .execute_batch("CREATE TABLE t (x)")
            .unwrap();
        let err = inspect(&foreign).unwrap_err().to_string();
        assert!(err.contains("no blobs table"), "{err}");
    }

    #[test]
    fn rejects_invalid_digests() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("db");
        database(&path, &[("../../etc/passwd".into(), 1)], &[]);
        let err = inspect(&path).unwrap_err().to_string();
        assert!(err.contains("invalid digest"), "{err}");
    }
}
