//! Reads the security insight analyzer needs in bulk, and the reports it
//! writes.

use std::collections::HashMap;

use chrono::{DateTime, Utc};
use rusqlite::{OptionalExtension, params};

use super::repository::query_all;
use super::time::{format_time, parse_time};
use super::{Error, Result, Store};

/// One versioned artifact with the fields the analyzer weighs.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct InsightArtifact {
    pub repo_id: i64,
    pub path: String,
    pub version: String,
    pub size: i64,
    pub cached_at: DateTime<Utc>,
    pub last_accessed_at: DateTime<Utc>,
    pub cached_by: String,
}

/// A stored vulnerability scan reduced to what a finding shows.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct VulnSummary {
    pub max_severity: String,
    pub vuln_ids: Vec<String>,
    /// Each advisory with its own severity label.
    pub advisories: Vec<super::VulnAdvisory>,
}

/// Audit entries grouped by the fields a security finding keys on.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AuditCount {
    pub event: String,
    pub status: i64,
    pub repo_name: String,
    pub username: String,
    pub client_ip: String,
    pub path: String,
    pub count: i64,
}

/// One stored analyzer run: the trend row and, for the newest run, the report.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct InsightReportRow {
    pub generated_at: DateTime<Utc>,
    pub score: i64,
    pub summary_json: String,
    pub report_json: String,
}

/// One check's state in one analyzer run.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct InsightCheckPoint {
    pub check_id: String,
    pub generated_at: DateTime<Utc>,
    pub status: String,
    /// Empty when the check passed.
    pub severity: String,
    pub flagged: i64,
}

/// One acknowledged insight check.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct InsightAck {
    pub check_id: String,
    pub note: String,
    /// What the check flagged when it was acknowledged.
    pub flagged: i64,
    pub acknowledged_by: String,
    pub acknowledged_at: DateTime<Utc>,
    pub expires_at: DateTime<Utc>,
}

/// The stored security insight settings. Callers fill the defaults in when
/// nothing has been saved yet.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct InsightSettingsRow {
    pub auto_analyze_enabled: bool,
    pub interval_minutes: i64,
    pub report_enabled: bool,
    pub receiver: String,
    pub report_cron: String,
    pub timezone: String,
    pub min_severity: String,
    pub skip_when_unchanged: bool,
    pub updated_by: String,
    pub updated_at: DateTime<Utc>,
}

/// Keys a coordinate the way `vuln_severity_by_coordinate` does.
pub fn coordinate_key(ecosystem: &str, package: &str, version: &str) -> String {
    format!("{ecosystem}\u{0}{package}\u{0}{version}")
}

impl Store {
    /// Returns every artifact that carries a version, across all repositories.
    pub async fn insight_artifacts(&self) -> Result<Vec<InsightArtifact>> {
        self.read(|conn| {
            query_all(
                conn,
                "insight artifacts",
                "SELECT repo_id, path, version, size, cached_at, last_accessed_at, cached_by
                   FROM artifacts WHERE version != ''",
                [],
                |r| {
                    Ok(InsightArtifact {
                        repo_id: r.get(0)?,
                        path: r.get(1)?,
                        version: r.get(2)?,
                        size: r.get(3)?,
                        cached_at: parse_time(&r.get::<_, String>(4)?),
                        last_accessed_at: parse_time(&r.get::<_, String>(5)?),
                        cached_by: r.get(6)?,
                    })
                },
            )
        })
        .await
    }

    /// Counts the artifacts per repository not served since `cutoff`, with their
    /// total size.
    pub async fn stale_artifact_totals(
        &self,
        cutoff: DateTime<Utc>,
    ) -> Result<HashMap<i64, (i64, i64)>> {
        let cutoff = format_time(cutoff);
        self.read(move |conn| {
            let rows = query_all(
                conn,
                "stale artifact totals",
                "SELECT repo_id, COUNT(*), COALESCE(SUM(size), 0) FROM artifacts
                  WHERE last_accessed_at < ? GROUP BY repo_id",
                params![cutoff],
                |r| Ok((r.get::<_, i64>(0)?, (r.get(1)?, r.get(2)?))),
            )?;
            Ok(rows.into_iter().collect())
        })
        .await
    }

    /// Returns every stored vulnerability scan keyed by [`coordinate_key`].
    pub async fn vuln_scan_index(&self) -> Result<HashMap<String, VulnSummary>> {
        self.read(|conn| {
            let rows = query_all(
                conn,
                "vuln scan index",
                "SELECT ecosystem, package, version, max_severity, vuln_ids, advisories FROM vuln_scans",
                [],
                |r| {
                    let ids: String = r.get(4)?;
                    Ok((
                        coordinate_key(
                            &r.get::<_, String>(0)?,
                            &r.get::<_, String>(1)?,
                            &r.get::<_, String>(2)?,
                        ),
                        VulnSummary {
                            max_severity: r.get(3)?,
                            vuln_ids: serde_json::from_str::<Option<Vec<String>>>(&ids)
                                .ok()
                                .flatten()
                                .unwrap_or_default(),
                            advisories: serde_json::from_str::<Option<Vec<super::VulnAdvisory>>>(
                                &r.get::<_, String>(5)?,
                            )
                            .ok()
                            .flatten()
                            .unwrap_or_default(),
                        },
                    ))
                },
            )?;
            Ok(rows.into_iter().collect())
        })
        .await
    }

    /// Returns every resolved license list keyed by [`coordinate_key`].
    pub async fn license_index(&self) -> Result<HashMap<String, Vec<String>>> {
        self.read(|conn| {
            let rows = query_all(
                conn,
                "license index",
                "SELECT system, package, version, licenses FROM license_scans",
                [],
                |r| {
                    let licenses: String = r.get(3)?;
                    Ok((
                        coordinate_key(
                            &r.get::<_, String>(0)?,
                            &r.get::<_, String>(1)?,
                            &r.get::<_, String>(2)?,
                        ),
                        serde_json::from_str::<Vec<String>>(&licenses).unwrap_or_default(),
                    ))
                },
            )?;
            Ok(rows.into_iter().collect())
        })
        .await
    }

    /// Groups the security-relevant audit entries since `since`: refused
    /// requests (401, 403) and policy blocks.
    pub async fn audit_security_counts(&self, since: DateTime<Utc>) -> Result<Vec<AuditCount>> {
        let since = format_time(since);
        self.read(move |conn| {
            query_all(
                conn,
                "audit security counts",
                "SELECT event, status, repo_name, username, client_ip, path, COUNT(*)
                   FROM audit_logs
                  WHERE created_at >= ?
                    AND (status IN (401, 403) OR event IN ('vuln.block', 'license.block'))
                  GROUP BY event, status, repo_name, username, client_ip, path",
                params![since],
                |r| {
                    Ok(AuditCount {
                        event: r.get(0)?,
                        status: r.get(1)?,
                        repo_name: r.get(2)?,
                        username: r.get(3)?,
                        client_ip: r.get(4)?,
                        path: r.get(5)?,
                        count: r.get(6)?,
                    })
                },
            )
        })
        .await
    }

    /// Stores one analyzer run, blanks the report of every older run, and drops
    /// trend rows generated before `keep_since`.
    pub async fn insert_insight_report(
        &self,
        row: InsightReportRow,
        checks: Vec<InsightCheckPoint>,
        keep_since: DateTime<Utc>,
    ) -> Result<()> {
        let keep_since = format_time(keep_since);
        self.write(move |conn| {
            let tx = conn
                .transaction()
                .map_err(|e| Error::sqlite("insert insight report", e))?;
            tx.execute(
                "UPDATE security_insight_reports SET report_json = '' WHERE report_json != ''",
                [],
            )
            .map_err(|e| Error::sqlite("blank insight reports", e))?;
            tx.execute(
                "INSERT INTO security_insight_reports(generated_at, score, summary_json, report_json)
                 VALUES(?, ?, ?, ?)",
                params![
                    format_time(row.generated_at),
                    row.score,
                    row.summary_json,
                    row.report_json
                ],
            )
            .map_err(|e| Error::sqlite("insert insight report", e))?;
            for point in &checks {
                tx.execute(
                    "INSERT OR REPLACE INTO insight_check_history(check_id, generated_at, status, severity, flagged)
                     VALUES(?, ?, ?, ?, ?)",
                    params![
                        point.check_id,
                        format_time(point.generated_at),
                        point.status,
                        point.severity,
                        point.flagged
                    ],
                )
                .map_err(|e| Error::sqlite("insert insight check history", e))?;
            }
            tx.execute(
                "DELETE FROM security_insight_reports WHERE generated_at < ?",
                params![keep_since],
            )
            .map_err(|e| Error::sqlite("prune insight reports", e))?;
            tx.execute(
                "DELETE FROM insight_check_history WHERE generated_at < ?",
                params![keep_since],
            )
            .map_err(|e| Error::sqlite("prune insight check history", e))?;
            tx.commit()
                .map_err(|e| Error::sqlite("insert insight report", e))
        })
        .await
    }

    /// Returns one check's states since `since`, oldest first.
    pub async fn insight_check_history(
        &self,
        check_id: &str,
        since: DateTime<Utc>,
    ) -> Result<Vec<InsightCheckPoint>> {
        let (check_id, since) = (check_id.to_string(), format_time(since));
        self.read(move |conn| {
            query_all(
                conn,
                "insight check history",
                "SELECT check_id, generated_at, status, severity, flagged FROM insight_check_history
                  WHERE check_id = ? AND generated_at >= ? ORDER BY generated_at ASC",
                params![check_id, since],
                |r| {
                    Ok(InsightCheckPoint {
                        check_id: r.get(0)?,
                        generated_at: parse_time(&r.get::<_, String>(1)?),
                        status: r.get(2)?,
                        severity: r.get(3)?,
                        flagged: r.get(4)?,
                    })
                },
            )
        })
        .await
    }

    /// Returns the newest analyzer run with its full report.
    /// [`Error::NotFound`] before the first run.
    pub async fn latest_insight_report(&self) -> Result<InsightReportRow> {
        self.read(|conn| {
            conn.query_row(
                "SELECT generated_at, score, summary_json, report_json
                   FROM security_insight_reports ORDER BY id DESC LIMIT 1",
                [],
                |r| {
                    Ok(InsightReportRow {
                        generated_at: parse_time(&r.get::<_, String>(0)?),
                        score: r.get(1)?,
                        summary_json: r.get(2)?,
                        report_json: r.get(3)?,
                    })
                },
            )
            .map_err(|e| Error::sqlite("latest insight report", e))
        })
        .await
    }

    /// Returns the trend rows generated since `since`, oldest first, without
    /// their reports.
    pub async fn insight_history(&self, since: DateTime<Utc>) -> Result<Vec<InsightReportRow>> {
        let since = format_time(since);
        self.read(move |conn| {
            query_all(
                conn,
                "insight history",
                "SELECT generated_at, score, summary_json FROM security_insight_reports
                  WHERE generated_at >= ? ORDER BY generated_at ASC",
                params![since],
                |r| {
                    Ok(InsightReportRow {
                        generated_at: parse_time(&r.get::<_, String>(0)?),
                        score: r.get(1)?,
                        summary_json: r.get(2)?,
                        report_json: String::new(),
                    })
                },
            )
        })
        .await
    }
}

impl Store {
    /// Acknowledges a check, replacing any earlier acknowledgement of it.
    pub async fn upsert_insight_ack(&self, ack: InsightAck) -> Result<()> {
        self.write(move |conn| {
            conn.execute(
                "INSERT INTO insight_acknowledgements(check_id, note, flagged, acknowledged_by, acknowledged_at, expires_at)
                 VALUES(?, ?, ?, ?, ?, ?)
                 ON CONFLICT(check_id) DO UPDATE SET
                     note = excluded.note,
                     flagged = excluded.flagged,
                     acknowledged_by = excluded.acknowledged_by,
                     acknowledged_at = excluded.acknowledged_at,
                     expires_at = excluded.expires_at",
                params![
                    ack.check_id,
                    ack.note,
                    ack.flagged,
                    ack.acknowledged_by,
                    format_time(ack.acknowledged_at),
                    format_time(ack.expires_at)
                ],
            )
            .map(|_| ())
            .map_err(|e| Error::sqlite("upsert insight ack", e))
        })
        .await
    }

    /// Removes a check's acknowledgement. [`Error::NotFound`] when there is
    /// none.
    pub async fn delete_insight_ack(&self, check_id: &str) -> Result<()> {
        let check_id = check_id.to_string();
        self.write(move |conn| {
            let n = conn
                .execute(
                    "DELETE FROM insight_acknowledgements WHERE check_id = ?",
                    params![check_id],
                )
                .map_err(|e| Error::sqlite("delete insight ack", e))?;
            if n == 0 {
                return Err(Error::NotFound);
            }
            Ok(())
        })
        .await
    }

    /// Returns every acknowledgement, expired ones included: the console says
    /// when one lapsed.
    pub async fn list_insight_acks(&self) -> Result<Vec<InsightAck>> {
        self.read(|conn| {
            query_all(
                conn,
                "list insight acks",
                "SELECT check_id, note, flagged, acknowledged_by, acknowledged_at, expires_at
                   FROM insight_acknowledgements ORDER BY check_id",
                [],
                |r| {
                    Ok(InsightAck {
                        check_id: r.get(0)?,
                        note: r.get(1)?,
                        flagged: r.get(2)?,
                        acknowledged_by: r.get(3)?,
                        acknowledged_at: parse_time(&r.get::<_, String>(4)?),
                        expires_at: parse_time(&r.get::<_, String>(5)?),
                    })
                },
            )
        })
        .await
    }
}

impl Store {
    /// Returns the saved insight settings, `None` before the first save.
    pub async fn read_insight_settings(&self) -> Result<Option<InsightSettingsRow>> {
        self.read(|conn| {
            conn.query_row(
                "SELECT auto_analyze_enabled, interval_minutes, report_enabled, receiver, report_cron,
                        timezone, min_severity, skip_when_unchanged, updated_by, updated_at
                   FROM insight_settings WHERE id = 1",
                [],
                |r| {
                    Ok(InsightSettingsRow {
                        auto_analyze_enabled: r.get::<_, i64>(0)? != 0,
                        interval_minutes: r.get(1)?,
                        report_enabled: r.get::<_, i64>(2)? != 0,
                        receiver: r.get(3)?,
                        report_cron: r.get(4)?,
                        timezone: r.get(5)?,
                        min_severity: r.get(6)?,
                        skip_when_unchanged: r.get::<_, i64>(7)? != 0,
                        updated_by: r.get(8)?,
                        updated_at: parse_time(&r.get::<_, String>(9)?),
                    })
                },
            )
            .optional()
            .map_err(|e| Error::sqlite("read insight settings", e))
        })
        .await
    }

    /// Saves the insight settings, keeping the last scheduled alarm's state.
    pub async fn write_insight_settings(&self, row: InsightSettingsRow) -> Result<()> {
        self.write(move |conn| {
            conn.execute(
                "INSERT INTO insight_settings(id, auto_analyze_enabled, interval_minutes, report_enabled,
                     receiver, report_cron, timezone, min_severity, skip_when_unchanged, updated_by, updated_at)
                 VALUES(1, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
                 ON CONFLICT(id) DO UPDATE SET
                     auto_analyze_enabled = excluded.auto_analyze_enabled,
                     interval_minutes = excluded.interval_minutes,
                     report_enabled = excluded.report_enabled,
                     receiver = excluded.receiver,
                     report_cron = excluded.report_cron,
                     timezone = excluded.timezone,
                     min_severity = excluded.min_severity,
                     skip_when_unchanged = excluded.skip_when_unchanged,
                     updated_by = excluded.updated_by,
                     updated_at = excluded.updated_at",
                params![
                    i64::from(row.auto_analyze_enabled),
                    row.interval_minutes,
                    i64::from(row.report_enabled),
                    row.receiver,
                    row.report_cron,
                    row.timezone,
                    row.min_severity,
                    i64::from(row.skip_when_unchanged),
                    row.updated_by,
                    format_time(row.updated_at)
                ],
            )
            .map(|_| ())
            .map_err(|e| Error::sqlite("write insight settings", e))
        })
        .await
    }

    /// Returns what each check flagged at the last scheduled alarm.
    pub async fn insight_report_state(&self) -> Result<HashMap<String, i64>> {
        let raw: Option<String> = self
            .read(|conn| {
                conn.query_row(
                    "SELECT last_report_state FROM insight_settings WHERE id = 1",
                    [],
                    |r| r.get(0),
                )
                .optional()
                .map_err(|e| Error::sqlite("read insight report state", e))
            })
            .await?;
        Ok(raw
            .and_then(|raw| serde_json::from_str(&raw).ok())
            .unwrap_or_default())
    }

    /// Records what the scheduled alarm just reported. Creates the settings row
    /// with its defaults when nothing has been saved yet.
    pub async fn write_insight_report_state(&self, state: &HashMap<String, i64>) -> Result<()> {
        let raw = serde_json::to_string(state).unwrap_or_else(|_| "{}".to_string());
        self.write(move |conn| {
            conn.execute(
                "INSERT INTO insight_settings(id, last_report_state) VALUES(1, ?)
                 ON CONFLICT(id) DO UPDATE SET last_report_state = excluded.last_report_state",
                params![raw],
            )
            .map(|_| ())
            .map_err(|e| Error::sqlite("write insight report state", e))
        })
        .await
    }

    /// Returns when the newest report was generated, `None` before the first.
    pub async fn latest_insight_generated_at(&self) -> Result<Option<DateTime<Utc>>> {
        let raw: Option<String> = self
            .read(|conn| {
                conn.query_row(
                    "SELECT generated_at FROM security_insight_reports ORDER BY id DESC LIMIT 1",
                    [],
                    |r| r.get(0),
                )
                .optional()
                .map_err(|e| Error::sqlite("latest insight time", e))
            })
            .await?;
        Ok(raw.map(|raw| parse_time(&raw)))
    }
}

#[cfg(test)]
mod tests {
    use chrono::TimeDelta;

    use super::*;
    use crate::meta::{Artifact, AuditLog, Repository};
    use crate::testing::meta::test_store;

    #[tokio::test]
    async fn insight_reads_and_reports() {
        let (s, _dir) = test_store().await;
        let repo = s
            .create_repository(Repository {
                name: "npm-hosted".to_string(),
                format: crate::meta::FORMAT_NPM.to_string(),
                r#type: crate::meta::TYPE_HOSTED.to_string(),
                ..Default::default()
            })
            .await
            .unwrap();
        let now = Utc::now();
        for (path, version, idle) in [("a/-/a-1.0.0.tgz", "1.0.0", 200), ("a", "", 1)] {
            s.put_artifact(Artifact {
                repo_id: repo.id,
                path: path.to_string(),
                version: version.to_string(),
                blob_sha256: path.to_string(),
                size: 7,
                cached_by: "alice".to_string(),
                cached_at: now - TimeDelta::days(idle),
                last_accessed_at: now - TimeDelta::days(idle),
                ..Default::default()
            })
            .await
            .unwrap();
        }
        let arts = s.insight_artifacts().await.unwrap();
        assert_eq!(arts.len(), 1, "only versioned rows: {arts:?}");
        assert_eq!(arts[0].cached_by, "alice");
        let stale = s
            .stale_artifact_totals(now - TimeDelta::days(180))
            .await
            .unwrap();
        assert_eq!(stale.get(&repo.id), Some(&(1, 7)));

        s.upsert_vuln_scan(
            "npm",
            "a",
            "1.0.0",
            "high",
            &["GHSA-1".to_string()],
            &HashMap::new(),
            0,
            &[crate::meta::VulnAdvisory {
                id: "GHSA-1".to_string(),
                severity: "high".to_string(),
                score: String::new(),
            }],
            "",
        )
        .await
        .unwrap();
        let vulns = s.vuln_scan_index().await.unwrap();
        let v = &vulns[&coordinate_key("npm", "a", "1.0.0")];
        assert_eq!(v.max_severity, "high");
        assert_eq!(v.vuln_ids, vec!["GHSA-1".to_string()]);
        assert_eq!(v.advisories[0].severity, "high");

        s.upsert_license_scan("NPM", "a", "1.0.0", &["GPL-3.0-only".to_string()], "")
            .await
            .unwrap();
        let licenses = s.license_index().await.unwrap();
        assert_eq!(
            licenses[&coordinate_key("NPM", "a", "1.0.0")],
            vec!["GPL-3.0-only".to_string()]
        );

        for (status, event) in [(401, "download"), (200, "download"), (403, "vuln.block")] {
            s.insert_audit_log(AuditLog {
                repo_name: "npm-hosted".to_string(),
                event: event.to_string(),
                status,
                client_ip: "10.0.0.1".to_string(),
                ..Default::default()
            })
            .await
            .unwrap();
        }
        let counts = s
            .audit_security_counts(now - TimeDelta::hours(1))
            .await
            .unwrap();
        assert_eq!(counts.len(), 2, "refused and blocked only: {counts:?}");

        let err = s.latest_insight_report().await.unwrap_err();
        assert!(err.is_not_found(), "{err}");
        for (age, score) in [(40, 50), (2, 70), (0, 90)] {
            s.insert_insight_report(
                InsightReportRow {
                    generated_at: now - TimeDelta::days(age),
                    score,
                    summary_json: "{}".to_string(),
                    report_json: format!("{{\"score\":{score}}}"),
                },
                vec![InsightCheckPoint {
                    check_id: "public-repositories".to_string(),
                    generated_at: now - TimeDelta::days(age),
                    status: "action".to_string(),
                    severity: "high".to_string(),
                    flagged: score / 10,
                }],
                now - TimeDelta::days(30),
            )
            .await
            .unwrap();
        }
        let latest = s.latest_insight_report().await.unwrap();
        assert_eq!(latest.score, 90);
        assert_eq!(latest.report_json, "{\"score\":90}");
        s.upsert_insight_ack(InsightAck {
            check_id: "public-repositories".to_string(),
            note: "intended".to_string(),
            flagged: 1,
            acknowledged_by: "admin".to_string(),
            acknowledged_at: now,
            expires_at: now + TimeDelta::days(30),
        })
        .await
        .unwrap();
        s.upsert_insight_ack(InsightAck {
            check_id: "public-repositories".to_string(),
            note: "still intended".to_string(),
            flagged: 2,
            acknowledged_by: "sec".to_string(),
            acknowledged_at: now,
            expires_at: now + TimeDelta::days(7),
        })
        .await
        .unwrap();
        let acks = s.list_insight_acks().await.unwrap();
        assert_eq!(acks.len(), 1, "re-acknowledging replaces");
        assert_eq!(
            (acks[0].note.as_str(), acks[0].flagged),
            ("still intended", 2)
        );
        s.delete_insight_ack("public-repositories").await.unwrap();
        assert!(
            s.delete_insight_ack("public-repositories")
                .await
                .unwrap_err()
                .is_not_found()
        );

        let points = s
            .insight_check_history("public-repositories", now - TimeDelta::days(90))
            .await
            .unwrap();
        assert_eq!(
            points.iter().map(|p| p.flagged).collect::<Vec<_>>(),
            vec![7, 9],
            "check history is pruned with the reports"
        );

        let history = s.insight_history(now - TimeDelta::days(90)).await.unwrap();
        assert_eq!(
            history.iter().map(|h| h.score).collect::<Vec<_>>(),
            vec![70, 90],
            "rows past the keep window are pruned"
        );
    }
}
