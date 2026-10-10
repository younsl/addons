//! Resolved license information per package coordinate.

use std::collections::HashSet;

use chrono::{DateTime, Utc};
use rusqlite::{Row, params};

use super::time::{format_time, now_rfc3339, parse_time};
use super::{Error, Result, Store};

/// A stored license-resolution result for one package coordinate.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LicenseScan {
    pub system: String,
    pub package: String,
    pub version: String,
    /// SPDX license expressions; empty when none reported
    pub licenses: Vec<String>,
    /// data source that produced the result (e.g. "deps.dev")
    pub source: String,
    pub resolved_at: DateTime<Utc>,
}

const LICENSE_SELECT: &str = "SELECT system, package, version, licenses, source, resolved_at
           FROM license_scans";

impl Store {
    /// Records (or refreshes) a license-resolution result for a coordinate. An
    /// empty `licenses` list is stored as an empty array; an empty `source`
    /// defaults to "deps.dev".
    pub async fn upsert_license_scan(
        &self,
        system: &str,
        pkg: &str,
        version: &str,
        licenses: &[String],
        source: &str,
    ) -> Result<()> {
        let lic_json = serde_json::to_string(licenses)?;
        let system = system.to_string();
        let pkg = pkg.to_string();
        let version = version.to_string();
        let source = if source.is_empty() {
            "deps.dev".to_string()
        } else {
            source.to_string()
        };
        self.write(move |conn| {
            conn.execute(
                "INSERT INTO license_scans(system, package, version, licenses, source, resolved_at)
         VALUES(?, ?, ?, ?, ?, ?)
         ON CONFLICT(system, package, version) DO UPDATE SET
             licenses = excluded.licenses,
             source = excluded.source,
             resolved_at = excluded.resolved_at",
                params![system, pkg, version, lic_json, source, now_rfc3339()],
            )
            .map(|_| ())
            .map_err(|e| Error::sqlite("upsert license scan", e))
        })
        .await
    }

    /// Returns a stored result, or [`Error::NotFound`] when the coordinate has
    /// not been resolved yet.
    pub async fn get_license_scan(
        &self,
        system: &str,
        pkg: &str,
        version: &str,
    ) -> Result<LicenseScan> {
        let system = system.to_string();
        let pkg = pkg.to_string();
        let version = version.to_string();
        self.read(move |conn| {
            conn.query_row(
                &format!("{LICENSE_SELECT} WHERE system = ? AND package = ? AND version = ?"),
                params![system, pkg, version],
                scan_license,
            )
            .map_err(|e| Error::sqlite("get license scan", e))
        })
        .await
    }

    /// Returns up to `limit` results last resolved before the cutoff, oldest
    /// first, so a re-resolver can refresh them against fresh data.
    pub async fn list_stale_license_scans(
        &self,
        before: DateTime<Utc>,
        limit: i64,
    ) -> Result<Vec<LicenseScan>> {
        self.read(move |conn| {
            let mut stmt = conn
                .prepare(&format!(
                    "{LICENSE_SELECT} WHERE resolved_at < ? ORDER BY resolved_at ASC LIMIT ?"
                ))
                .map_err(|e| Error::sqlite("list stale license scans", e))?;
            let out = stmt
                .query_map(params![format_time(before), limit], scan_license)
                .map_err(|e| Error::sqlite("list stale license scans", e))?
                .collect::<rusqlite::Result<Vec<_>>>()
                .map_err(|e| Error::sqlite("list stale license scans", e))?;
            Ok(out)
        })
        .await
    }

    /// Returns the set of coordinates that already have a stored result, keyed
    /// as `system\x00package\x00version`, so a backfill can enqueue only
    /// coordinates that have never been resolved.
    pub async fn resolved_license_keys(&self) -> Result<HashSet<String>> {
        self.read(|conn| {
            let mut stmt = conn
                .prepare("SELECT system, package, version FROM license_scans")
                .map_err(|e| Error::sqlite("resolved license keys", e))?;
            let rows = stmt
                .query_map([], |r| {
                    Ok(format!(
                        "{}\x00{}\x00{}",
                        r.get::<_, String>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, String>(2)?
                    ))
                })
                .map_err(|e| Error::sqlite("resolved license keys", e))?;
            let mut out = HashSet::new();
            for row in rows {
                out.insert(row.map_err(|e| Error::sqlite("resolved license keys", e))?);
            }
            Ok(out)
        })
        .await
    }
}

/// Reads one license row.
fn scan_license(r: &Row<'_>) -> rusqlite::Result<LicenseScan> {
    let licenses: String = r.get(3)?;
    let resolved: String = r.get(5)?;
    Ok(LicenseScan {
        system: r.get(0)?,
        package: r.get(1)?,
        version: r.get(2)?,
        licenses: serde_json::from_str::<Option<Vec<String>>>(&licenses)
            .ok()
            .flatten()
            .unwrap_or_default(),
        source: r.get(4)?,
        resolved_at: parse_time(&resolved),
    })
}
