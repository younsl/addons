//! Known-vulnerability scan results per package coordinate.

use std::collections::{HashMap, HashSet};

use chrono::{DateTime, Utc};
use rusqlite::{Row, params};
use serde::{Deserialize, Serialize};

use super::time::{format_time, now_rfc3339, parse_time};
use super::{Error, Result, Store};

/// One advisory matched by a scan: its id (CVE/GHSA/OSV), the derived severity
/// label, and a CVSS score string when known.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct VulnAdvisory {
    pub id: String,
    pub severity: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub score: String,
}

/// A stored vulnerability scan result for one package coordinate.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct VulnScan {
    pub ecosystem: String,
    pub package: String,
    pub version: String,
    /// critical|high|medium|low|none
    pub max_severity: String,
    pub vuln_ids: Vec<String>,
    /// per-severity advisory counts (label -> count)
    pub severity_counts: HashMap<String, i64>,
    /// per-advisory detail (id, severity, score)
    pub advisories: Vec<VulnAdvisory>,
    /// advisory-source query latency in milliseconds
    pub duration_ms: i64,
    /// advisory data source that produced the scan (e.g. "OSV")
    pub source: String,
    pub scanned_at: DateTime<Utc>,
}

const VULN_SELECT: &str = "SELECT ecosystem, package, version, max_severity, vuln_ids, severity_counts, advisories, duration_ms, source, scanned_at
           FROM vuln_scans";

impl Store {
    /// Records (or refreshes) a scan result for a coordinate. `counts` is the
    /// per-severity advisory histogram (label -> count); an empty map is stored
    /// as an empty object. `duration_ms` is how long the advisory-source query
    /// took. An empty `source` defaults to "OSV".
    #[allow(clippy::too_many_arguments)]
    pub async fn upsert_vuln_scan(
        &self,
        eco: &str,
        pkg: &str,
        version: &str,
        max_severity: &str,
        ids: &[String],
        counts: &HashMap<String, i64>,
        duration_ms: i64,
        advisories: &[VulnAdvisory],
        source: &str,
    ) -> Result<()> {
        let ids_json = serde_json::to_string(ids)?;
        let counts_json = serde_json::to_string(counts)?;
        let adv_json = serde_json::to_string(advisories)?;
        let eco = eco.to_string();
        let pkg = pkg.to_string();
        let version = version.to_string();
        let max_severity = max_severity.to_string();
        let source = if source.is_empty() {
            "OSV".to_string()
        } else {
            source.to_string()
        };
        self.write(move |conn| {
            conn.execute(
                "INSERT INTO vuln_scans(ecosystem, package, version, max_severity, vuln_ids, severity_counts, advisories, duration_ms, source, scanned_at)
         VALUES(?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
         ON CONFLICT(ecosystem, package, version) DO UPDATE SET
             max_severity = excluded.max_severity,
             vuln_ids = excluded.vuln_ids,
             severity_counts = excluded.severity_counts,
             advisories = excluded.advisories,
             duration_ms = excluded.duration_ms,
             source = excluded.source,
             scanned_at = excluded.scanned_at",
                params![
                    eco,
                    pkg,
                    version,
                    max_severity,
                    ids_json,
                    counts_json,
                    adv_json,
                    duration_ms,
                    source,
                    now_rfc3339()
                ],
            )
            .map(|_| ())
            .map_err(|e| Error::sqlite("upsert vuln scan", e))
        })
        .await
    }

    /// Returns a stored scan result, or [`Error::NotFound`] when the coordinate
    /// has not been scanned yet.
    pub async fn get_vuln_scan(&self, eco: &str, pkg: &str, version: &str) -> Result<VulnScan> {
        let eco = eco.to_string();
        let pkg = pkg.to_string();
        let version = version.to_string();
        self.read(move |conn| {
            conn.query_row(
                &format!("{VULN_SELECT} WHERE ecosystem = ? AND package = ? AND version = ?"),
                params![eco, pkg, version],
                scan_vuln,
            )
            .map_err(|e| Error::sqlite("get vuln scan", e))
        })
        .await
    }

    /// Returns up to `limit` scans last scanned before the cutoff, oldest first,
    /// so a re-scanner can refresh them against fresh advisory data.
    pub async fn list_stale_vuln_scans(
        &self,
        before: DateTime<Utc>,
        limit: i64,
    ) -> Result<Vec<VulnScan>> {
        self.read(move |conn| {
            let mut stmt = conn
                .prepare(&format!(
                    "{VULN_SELECT} WHERE scanned_at < ? ORDER BY scanned_at ASC LIMIT ?"
                ))
                .map_err(|e| Error::sqlite("list stale vuln scans", e))?;
            let out = stmt
                .query_map(params![format_time(before), limit], scan_vuln)
                .map_err(|e| Error::sqlite("list stale vuln scans", e))?
                .collect::<rusqlite::Result<Vec<_>>>()
                .map_err(|e| Error::sqlite("list stale vuln scans", e))?;
            Ok(out)
        })
        .await
    }

    /// Returns the set of coordinates that already have a stored scan, keyed as
    /// `ecosystem\x00package\x00version`, so a backfill can enqueue scans only
    /// for artifacts that have never been scanned.
    pub async fn scanned_keys(&self) -> Result<HashSet<String>> {
        self.read(|conn| {
            let mut stmt = conn
                .prepare("SELECT ecosystem, package, version FROM vuln_scans")
                .map_err(|e| Error::sqlite("scanned keys", e))?;
            let rows = stmt
                .query_map([], |r| {
                    Ok(format!(
                        "{}\x00{}\x00{}",
                        r.get::<_, String>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, String>(2)?
                    ))
                })
                .map_err(|e| Error::sqlite("scanned keys", e))?;
            let mut out = HashSet::new();
            for row in rows {
                out.insert(row.map_err(|e| Error::sqlite("scanned keys", e))?);
            }
            Ok(out)
        })
        .await
    }

    /// Returns every stored scan's max severity keyed as
    /// `ecosystem\x00package\x00version`, so callers can classify an artifact as
    /// scanned-and-clean (severity "none"), vulnerable (any other value), or
    /// unscanned (coordinate absent) without an N+1 lookup.
    pub async fn vuln_severity_by_coordinate(&self) -> Result<HashMap<String, String>> {
        self.read(|conn| {
            let mut stmt = conn
                .prepare("SELECT ecosystem, package, version, max_severity FROM vuln_scans")
                .map_err(|e| Error::sqlite("vuln severity by coordinate", e))?;
            let rows = stmt
                .query_map([], |r| {
                    Ok((
                        format!(
                            "{}\x00{}\x00{}",
                            r.get::<_, String>(0)?,
                            r.get::<_, String>(1)?,
                            r.get::<_, String>(2)?
                        ),
                        r.get::<_, String>(3)?,
                    ))
                })
                .map_err(|e| Error::sqlite("vuln severity by coordinate", e))?;
            let mut out = HashMap::new();
            for row in rows {
                let (key, sev) =
                    row.map_err(|e| Error::sqlite("vuln severity by coordinate", e))?;
                out.insert(key, sev);
            }
            Ok(out)
        })
        .await
    }
}

/// Reads one scan row.
fn scan_vuln(r: &Row<'_>) -> rusqlite::Result<VulnScan> {
    let ids: String = r.get(4)?;
    let counts: String = r.get(5)?;
    let advisories: String = r.get(6)?;
    let scanned: String = r.get(9)?;
    Ok(VulnScan {
        ecosystem: r.get(0)?,
        package: r.get(1)?,
        version: r.get(2)?,
        max_severity: r.get(3)?,
        vuln_ids: serde_json::from_str::<Option<Vec<String>>>(&ids)
            .ok()
            .flatten()
            .unwrap_or_default(),
        severity_counts: serde_json::from_str::<Option<HashMap<String, i64>>>(&counts)
            .ok()
            .flatten()
            .unwrap_or_default(),
        advisories: serde_json::from_str::<Option<Vec<VulnAdvisory>>>(&advisories)
            .ok()
            .flatten()
            .unwrap_or_default(),
        duration_ms: r.get(7)?,
        source: r.get(8)?,
        scanned_at: parse_time(&scanned),
    })
}

#[cfg(test)]
pub(crate) mod tests {
    use std::collections::HashMap;

    use chrono::{Duration, Utc};

    use crate::meta::*;

    #[tokio::test]
    async fn vuln_scan_store() {
        let (s, _dir) = test_store().await;

        // Missing coordinate -> NotFound.
        let err = s.get_vuln_scan("npm", "lodash", "1.0.0").await.unwrap_err();
        assert!(err.is_not_found(), "missing scan err = {err}");

        // Upsert + round-trip (ids survive JSON encoding).
        let counts: HashMap<String, i64> =
            HashMap::from([("high".to_string(), 1), ("medium".to_string(), 1)]);
        s.upsert_vuln_scan(
            "npm",
            "lodash",
            "1.0.0",
            "high",
            &["CVE-1".to_string(), "GHSA-2".to_string()],
            &counts,
            0,
            &[],
            "OSV",
        )
        .await
        .unwrap();
        let got = s.get_vuln_scan("npm", "lodash", "1.0.0").await.unwrap();
        assert_eq!(got.max_severity, "high", "round-trip = {got:?}");
        assert_eq!(got.vuln_ids.len(), 2, "round-trip = {got:?}");
        assert_eq!(got.vuln_ids[0], "CVE-1", "round-trip = {got:?}");

        // Upsert again refreshes severity/ids in place (no duplicate row).
        let counts: HashMap<String, i64> = HashMap::from([("critical".to_string(), 1)]);
        s.upsert_vuln_scan(
            "npm",
            "lodash",
            "1.0.0",
            "critical",
            &["CVE-1".to_string()],
            &counts,
            0,
            &[],
            "OSV",
        )
        .await
        .unwrap();
        let got = s.get_vuln_scan("npm", "lodash", "1.0.0").await.unwrap();
        assert_eq!(got.max_severity, "critical", "after refresh = {got:?}");
        assert_eq!(got.vuln_ids.len(), 1, "after refresh = {got:?}");

        // Stale listing: a far-future cutoff returns the row; a past cutoff does not.
        let stale = s
            .list_stale_vuln_scans(Utc::now() + Duration::hours(1), 10)
            .await
            .unwrap();
        assert_eq!(stale.len(), 1, "stale (future cutoff)");
        let old = s
            .list_stale_vuln_scans(Utc::now() - Duration::hours(1), 10)
            .await
            .unwrap();
        assert_eq!(old.len(), 0, "stale (past cutoff)");
    }

    #[tokio::test]
    async fn scanned_keys_store() {
        let (s, _dir) = test_store().await;
        let counts: HashMap<String, i64> = HashMap::new();
        s.upsert_vuln_scan(
            "npm",
            "left-pad",
            "1.3.0",
            "none",
            &[],
            &counts,
            12,
            &[VulnAdvisory {
                id: "GHSA-x".into(),
                severity: "low".into(),
                score: "3.1".into(),
            }],
            "",
        )
        .await
        .unwrap();

        let keys = s.scanned_keys().await.unwrap();
        assert!(
            keys.contains("npm\u{0}left-pad\u{0}1.3.0"),
            "keys = {keys:?}"
        );
        let sev = s.vuln_severity_by_coordinate().await.unwrap();
        assert_eq!(
            sev.get("npm\u{0}left-pad\u{0}1.3.0").map(String::as_str),
            Some("none")
        );

        // An empty source defaults to OSV, and the advisory list round-trips.
        let got = s.get_vuln_scan("npm", "left-pad", "1.3.0").await.unwrap();
        assert_eq!(got.source, "OSV");
        assert_eq!(got.duration_ms, 12);
        assert_eq!(got.advisories.len(), 1);
        assert_eq!(got.advisories[0].id, "GHSA-x");
        assert_eq!(got.advisories[0].score, "3.1");
    }
}
