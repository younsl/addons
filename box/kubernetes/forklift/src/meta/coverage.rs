//! Forklift coverage persistence: settings, the last scan result, the trend history and console
//! mutes. The domain types live in [`crate::coverage`].

use chrono::{Duration, Utc};
use rusqlite::params;

use crate::coverage::{self, HISTORY_RETENTION_DAYS, MuteScopes, MutedProject, Settings, Snapshot};

use super::time::{format_time, is_zero, now_rfc3339, parse_time};
use super::{Error, Result, Store};

/// How long a coverage snapshot is kept, taken from the domain constant so the
/// store, the API bound and the chart cannot drift apart into promising a
/// window the data does not cover.
pub(crate) const COVERAGE_HISTORY_RETENTION: Duration = Duration::days(HISTORY_RETENTION_DAYS);

const COVERAGE_SETTINGS_COLS: &str = "forklift_host, exclude_topics, scan_cron, timezone,
    auto_scan_enabled, report_enabled, receiver, skip_when_full_coverage, max_branches, since_days,
    use_search, updated_by, updated_at";

impl Store {
    /// Returns the stored coverage settings, or `None` when an administrator
    /// has never saved them, which the scanner reads as "run on the defaults"
    /// rather than as an error.
    pub async fn read_coverage_settings(&self) -> Result<Option<Settings>> {
        self.read(|conn| {
            let res = conn.query_row(
                &format!("SELECT {COVERAGE_SETTINGS_COLS} FROM coverage_settings WHERE id = 1"),
                [],
                |r| {
                    let exclude_topics: String = r.get(1)?;
                    let auto_scan: i64 = r.get(4)?;
                    let report_enabled: i64 = r.get(5)?;
                    let skip_full: i64 = r.get(7)?;
                    let use_search: i64 = r.get(10)?;
                    let updated_at: String = r.get(12)?;
                    Ok(Settings {
                        forklift_host: r.get(0)?,
                        exclude_topics: decode_string_list(&exclude_topics),
                        scan_cron: r.get(2)?,
                        timezone: r.get(3)?,
                        auto_scan_enabled: auto_scan != 0,
                        report_enabled: report_enabled != 0,
                        receiver: r.get(6)?,
                        skip_when_full_coverage: skip_full != 0,
                        max_branches: r.get(8)?,
                        since_days: r.get(9)?,
                        use_search: use_search != 0,
                        updated_by: r.get(11)?,
                        updated_at: parse_time(&updated_at),
                        ..Settings::default()
                    })
                },
            );
            match res {
                Ok(s) => Ok(Some(s)),
                Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
                Err(e) => Err(Error::sqlite("read coverage settings", e)),
            }
        })
        .await
    }

    /// Replaces the settings row and returns what was stored, with the write
    /// timestamp filled in.
    pub async fn write_coverage_settings(&self, mut input: Settings) -> Result<Settings> {
        let now = now_rfc3339();
        let row = input.clone();
        let stamp = now.clone();
        self.write(move |conn| {
            conn.execute(
                "INSERT INTO coverage_settings(id, forklift_host, exclude_topics, scan_cron, timezone,
             auto_scan_enabled, report_enabled, receiver, skip_when_full_coverage, max_branches, since_days,
             use_search, updated_by, updated_at)
         VALUES(1, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
         ON CONFLICT(id) DO UPDATE SET
             forklift_host = excluded.forklift_host,
             exclude_topics = excluded.exclude_topics,
             scan_cron = excluded.scan_cron, timezone = excluded.timezone,
             auto_scan_enabled = excluded.auto_scan_enabled,
             report_enabled = excluded.report_enabled, receiver = excluded.receiver,
             skip_when_full_coverage = excluded.skip_when_full_coverage,
             max_branches = excluded.max_branches, since_days = excluded.since_days,
             use_search = excluded.use_search,
             updated_by = excluded.updated_by, updated_at = excluded.updated_at",
                params![
                    row.forklift_host,
                    encode_string_list(&row.exclude_topics),
                    row.scan_cron,
                    row.timezone,
                    row.auto_scan_enabled,
                    row.report_enabled,
                    row.receiver,
                    row.skip_when_full_coverage,
                    row.max_branches,
                    row.since_days,
                    row.use_search,
                    row.updated_by,
                    stamp
                ],
            )
            .map(|_| ())
            .map_err(|e| Error::sqlite("write coverage settings", e))
        })
        .await?;
        input.updated_at = parse_time(&now);
        Ok(input)
    }

    /// Returns the last completed scan, or `None` when no scan has ever
    /// finished.
    pub async fn read_coverage_result(&self) -> Result<Option<coverage::Result>> {
        self.read(|conn| {
            let res = conn.query_row(
                "SELECT payload, scanned_at, duration_ms FROM coverage_results WHERE id = 1",
                [],
                |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, i64>(2)?,
                    ))
                },
            );
            let (payload, scanned_at, duration_ms) = match res {
                Ok(v) => v,
                Err(rusqlite::Error::QueryReturnedNoRows) => return Ok(None),
                Err(e) => return Err(Error::sqlite("read coverage result", e)),
            };
            let Ok(mut out) = serde_json::from_str::<coverage::Result>(&payload) else {
                // A corrupt row must not stop forklift from booting, and the
                // next scan overwrites it anyway. Report "no result" rather than
                // an error.
                return Ok(None);
            };
            out.scanned_at = parse_time(&scanned_at);
            out.duration_ms = duration_ms;
            Ok(Some(out))
        })
        .await
    }

    /// Replaces the stored scan result.
    pub async fn write_coverage_result(&self, r: coverage::Result) -> Result<()> {
        let payload = serde_json::to_string(&r)?;
        let scanned_at = format_time(r.scanned_at);
        let duration_ms = r.duration_ms;
        self.write(move |conn| {
            conn.execute(
                "INSERT INTO coverage_results(id, payload, scanned_at, duration_ms) VALUES(1, ?, ?, ?)
         ON CONFLICT(id) DO UPDATE SET payload = excluded.payload,
             scanned_at = excluded.scanned_at, duration_ms = excluded.duration_ms",
                params![payload, scanned_at, duration_ms],
            )
            .map(|_| ())
            .map_err(|e| Error::sqlite("write coverage result", e))
        })
        .await
    }

    /// Records one reading for the trend and drops readings past the retention
    /// window. An unset `scanned_at` (the epoch) is stamped with now.
    pub async fn add_coverage_snapshot(&self, snap: Snapshot) -> Result<()> {
        self.write(move |conn| {
            let scanned_at = if is_zero(snap.scanned_at) {
                Utc::now()
            } else {
                snap.scanned_at
            };
            conn.execute(
                "INSERT INTO coverage_history(target, applied, partial, not_applied, skipped, percent, scanned_at)
         VALUES(?, ?, ?, ?, ?, ?, ?)",
                params![
                    snap.target,
                    snap.applied,
                    snap.partial,
                    snap.not_applied,
                    snap.skipped,
                    snap.percent,
                    format_time(scanned_at)
                ],
            )
            .map_err(|e| Error::sqlite("add coverage snapshot", e))?;
            let cutoff = format_time(Utc::now() - COVERAGE_HISTORY_RETENTION);
            conn.execute(
                "DELETE FROM coverage_history WHERE scanned_at < ?",
                params![cutoff],
            )
            .map(|_| ())
            .map_err(|e| Error::sqlite("prune coverage history", e))
        })
        .await
    }

    /// Returns the snapshots from the last `days` days, oldest first, which is
    /// the order the trend chart plots them in. The window is capped at the
    /// retention: asking for more would quietly return less.
    pub async fn list_coverage_history(&self, days: i64) -> Result<Vec<Snapshot>> {
        let days = days.clamp(1, HISTORY_RETENTION_DAYS);
        self.read(move |conn| {
            let cutoff = format_time(Utc::now() - Duration::days(days));
            let mut stmt = conn
                .prepare(
                    "SELECT target, applied, partial, not_applied, skipped, percent, scanned_at
         FROM coverage_history WHERE scanned_at >= ? ORDER BY scanned_at ASC",
                )
                .map_err(|e| Error::sqlite("list coverage history", e))?;
            let out = stmt
                .query_map(params![cutoff], |r| {
                    let scanned_at: String = r.get(6)?;
                    Ok(Snapshot {
                        target: r.get(0)?,
                        applied: r.get(1)?,
                        partial: r.get(2)?,
                        not_applied: r.get(3)?,
                        skipped: r.get(4)?,
                        percent: r.get(5)?,
                        scanned_at: parse_time(&scanned_at),
                    })
                })
                .map_err(|e| Error::sqlite("list coverage history", e))?
                .collect::<rusqlite::Result<Vec<_>>>()
                .map_err(|e| Error::sqlite("list coverage history", e))?;
            Ok(out)
        })
        .await
    }

    /// Returns the projects muted from the console and which of their checks
    /// are muted.
    pub async fn list_coverage_muted(&self) -> Result<Vec<MutedProject>> {
        self.read(|conn| {
            let mut stmt = conn
                .prepare(
                    "SELECT project_path, scopes FROM coverage_muted ORDER BY project_path ASC",
                )
                .map_err(|e| Error::sqlite("list muted coverage projects", e))?;
            let out = stmt
                .query_map([], |r| {
                    let path: String = r.get(0)?;
                    let scopes: String = r.get(1)?;
                    // A row nobody can read is still a mute somebody asked for.
                    // Fall back to the whole project rather than quietly putting
                    // it back in the number.
                    let parsed = match coverage::parse_mute_scopes(&decode_string_list(&scopes)) {
                        Ok(p) if p.any() => p,
                        _ => MuteScopes {
                            ci: true,
                            registry: true,
                        },
                    };
                    Ok(MutedProject {
                        path,
                        scopes: parsed,
                    })
                })
                .map_err(|e| Error::sqlite("list muted coverage projects", e))?
                .collect::<rusqlite::Result<Vec<_>>>()
                .map_err(|e| Error::sqlite("list muted coverage projects", e))?;
            Ok(out)
        })
        .await
    }

    /// Mutes the named checks on a project. Re-muting one that is already muted
    /// rewrites the scopes, which is how a mute is narrowed or widened, but
    /// keeps the original attribution rather than rewriting who did it and
    /// when.
    pub async fn add_coverage_muted(
        &self,
        project_path: &str,
        by: &str,
        scopes: MuteScopes,
    ) -> Result<()> {
        let project_path = project_path.to_string();
        let by = by.to_string();
        let scopes = encode_string_list(&scopes.list());
        self.write(move |conn| {
            conn.execute(
                "INSERT INTO coverage_muted(project_path, muted_by, muted_at, scopes) VALUES(?, ?, ?, ?)
         ON CONFLICT(project_path) DO UPDATE SET scopes = excluded.scopes",
                params![project_path, by, now_rfc3339(), scopes],
            )
            .map(|_| ())
            .map_err(|e| Error::sqlite("mute coverage project", e))
        })
        .await
    }

    /// Puts a project back in scope.
    pub async fn remove_coverage_muted(&self, project_path: &str) -> Result<()> {
        let project_path = project_path.to_string();
        self.write(move |conn| {
            conn.execute(
                "DELETE FROM coverage_muted WHERE project_path = ?",
                params![project_path],
            )
            .map(|_| ())
            .map_err(|e| Error::sqlite("unmute coverage project", e))
        })
        .await
    }
}

/// Stores a list as a JSON array, always a valid array so a read never has to
/// handle a NULL or an empty string.
fn encode_string_list(v: &[String]) -> String {
    serde_json::to_string(v).unwrap_or_else(|_| "[]".to_string())
}

/// Reads a JSON array column, returning an empty list for anything unreadable
/// rather than failing the whole settings read.
fn decode_string_list(s: &str) -> Vec<String> {
    if s.is_empty() {
        return Vec::new();
    }
    serde_json::from_str::<Option<Vec<String>>>(s)
        .ok()
        .flatten()
        .unwrap_or_default()
}

#[cfg(test)]
pub(crate) mod tests {
    use chrono::{Duration, SubsecRound, Utc};
    use rusqlite::params;

    use crate::coverage;

    use crate::meta::coverage::COVERAGE_HISTORY_RETENTION;
    use crate::meta::time::{format_time, now_rfc3339};
    use crate::meta::*;

    #[tokio::test]
    async fn coverage_settings_round_trip() {
        let (s, _dir) = test_store().await;

        // A store that has never had settings saved reports "not found" rather than
        // an error, which the scanner reads as "run on the defaults".
        let found = s.read_coverage_settings().await.expect("fresh store");
        assert!(found.is_none(), "ReadCoverageSettings on a fresh store");

        let mut want = coverage::default_settings();
        want.forklift_host = "nexus.corp.example.org".into();
        want.exclude_topics = vec!["forklift.excluded".into(), "archived".into()];
        want.receiver = "platform-alerts".into();
        want.skip_when_full_coverage = true;
        want.use_search = true;
        want.max_branches = 4;
        want.updated_by = "admin".into();

        let saved = s
            .write_coverage_settings(want.clone())
            .await
            .expect("write_coverage_settings");
        assert!(
            !time::is_zero(saved.updated_at),
            "updated_at was not stamped on the write"
        );

        let got = s
            .read_coverage_settings()
            .await
            .expect("read_coverage_settings")
            .expect("settings found");
        assert_eq!(
            got.skip_when_full_coverage, want.skip_when_full_coverage,
            "read back {got:?}"
        );
        assert_eq!(got.use_search, want.use_search, "read back {got:?}");
        assert_eq!(got.max_branches, want.max_branches, "read back {got:?}");
        assert_eq!(got.updated_by, want.updated_by, "read back {got:?}");
        assert_eq!(got.forklift_host, want.forklift_host, "forklift_host");
        assert_eq!(got.exclude_topics.len(), 2, "exclude_topics");
        assert_eq!(got.exclude_topics[1], "archived", "exclude_topics");
        assert_eq!(got.receiver, "platform-alerts", "receiver");

        // The row is a singleton: a second write replaces it.
        want.max_branches = 9;
        s.write_coverage_settings(want)
            .await
            .expect("second write_coverage_settings");
        let got = s.read_coverage_settings().await.unwrap().unwrap();
        assert_eq!(got.max_branches, 9, "max_branches after the second write");
    }

    #[tokio::test]
    async fn coverage_settings_lists_are_never_null() {
        let (s, _dir) = test_store().await;
        let mut cfg = coverage::default_settings();
        cfg.exclude_topics = Vec::new();
        s.write_coverage_settings(cfg)
            .await
            .expect("write_coverage_settings");
        let got = s
            .read_coverage_settings()
            .await
            .expect("read_coverage_settings")
            .expect("settings found");
        // An unset list must come back as an empty one, so no caller has to guard
        // for the difference between "unset" and "empty".
        assert!(got.exclude_topics.is_empty(), "round-tripped as {got:?}");
        assert_eq!(
            serde_json::to_string(&got.exclude_topics).unwrap(),
            "[]",
            "an empty list serialises as [], not null"
        );
    }

    #[tokio::test]
    async fn coverage_result_round_trip() {
        let (s, _dir) = test_store().await;

        let found = s.read_coverage_result().await.expect("fresh store");
        assert!(found.is_none(), "ReadCoverageResult on a fresh store");

        let scanned_at = Utc::now().trunc_subsecs(3);
        let mut want = coverage::Result {
            projects: vec![coverage::Project {
                id: 1,
                path: "team/app".into(),
                group: "team".into(),
                name: "app".into(),
                applied: coverage::STATE_APPLIED,
                evidence: vec![".gitlab-ci.yml".into()],
                ..coverage::Project::default()
            }],
            excluded_projects: vec![coverage::ExcludedProject {
                id: 2,
                path: "legacy/thing".into(),
                reason: coverage::EXCLUDE_MUTED.into(),
                ..coverage::ExcludedProject::default()
            }],
            scanned_at,
            duration_ms: 1234,
            triggered_by: "alice".into(),
            forklift_host: "forklift.example.com".into(),
        };
        s.write_coverage_result(want.clone())
            .await
            .expect("write_coverage_result");

        let got = s
            .read_coverage_result()
            .await
            .expect("read_coverage_result")
            .expect("result found");
        assert_eq!(got.projects.len(), 1, "projects = {:?}", got.projects);
        assert_eq!(got.projects[0].path, "team/app", "projects");
        assert_eq!(got.projects[0].applied, coverage::STATE_APPLIED, "projects");
        assert_eq!(got.excluded_projects.len(), 1, "excluded");
        assert_eq!(
            got.excluded_projects[0].reason,
            coverage::EXCLUDE_MUTED,
            "excluded"
        );
        assert_eq!(got.triggered_by, "alice", "attribution");
        assert_eq!(got.duration_ms, 1234, "attribution");
        assert_eq!(got.scanned_at, scanned_at, "scanned_at");

        // The latest scan replaces the previous one rather than accumulating.
        want.projects = Vec::new();
        want.triggered_by = coverage::TRIGGER_SCHEDULE.into();
        s.write_coverage_result(want)
            .await
            .expect("second write_coverage_result");
        let got = s.read_coverage_result().await.unwrap().unwrap();
        assert!(got.projects.is_empty(), "result after the second write");
        assert_eq!(got.triggered_by, coverage::TRIGGER_SCHEDULE);
    }

    #[tokio::test]
    async fn coverage_result_survives_a_corrupt_row() {
        let (s, _dir) = test_store().await;
        s.write(|conn| {
        conn.execute(
            "INSERT INTO coverage_results(id, payload, scanned_at, duration_ms) VALUES(1, 'not json', ?, 0)",
            params![now_rfc3339()],
        )
        .map(|_| ())
        .map_err(|e| Error::sqlite("seed a corrupt row", e))
    })
    .await
    .expect("seed a corrupt row");
        // A corrupt row must not stop forklift from booting; the next scan
        // overwrites it anyway.
        let got = s
            .read_coverage_result()
            .await
            .expect("read_coverage_result on a corrupt row");
        assert!(got.is_none(), "corrupt row read back as {got:?}");
    }

    #[tokio::test]
    async fn coverage_history_window() {
        let (s, _dir) = test_store().await;

        let now = Utc::now();
        // One reading inside the retention window, one older, one now. The old one
        // is pruned by the writes that follow it, which is the policy: the trend
        // answers "did this move recently", so nothing older is kept to answer
        // with.
        for (i, at) in [
            now - Duration::days(coverage::HISTORY_RETENTION_DAYS + 6),
            now - Duration::days(3),
            now,
        ]
        .into_iter()
        .enumerate()
        {
            s.add_coverage_snapshot(coverage::Snapshot {
                target: 10,
                applied: i as i64,
                percent: i as i64 * 10,
                scanned_at: at,
                ..coverage::Snapshot::default()
            })
            .await
            .expect("add_coverage_snapshot");
        }

        let all = s
            .list_coverage_history(coverage::HISTORY_RETENTION_DAYS)
            .await
            .expect("list_coverage_history");
        assert_eq!(all.len(), 2, "snapshots in the window");
        // Oldest first, which is the order the trend chart plots them in.
        assert!(
            all[0].scanned_at < all[1].scanned_at,
            "history is not returned oldest first"
        );

        // A narrower window still narrows.
        let recent = s
            .list_coverage_history(1)
            .await
            .expect("list_coverage_history");
        assert_eq!(recent.len(), 1, "snapshots in the last day");

        // A wider one is capped rather than promising readings that were pruned.
        let wide = s
            .list_coverage_history(3650)
            .await
            .expect("list_coverage_history");
        assert_eq!(wide.len(), all.len(), "a window past the retention");

        // An empty window is still a list, never absent.
        let empty = s
            .list_coverage_history(0)
            .await
            .expect("list_coverage_history(0)");
        assert_eq!(
            empty.len(),
            recent.len(),
            "a zero window is clamped to one day"
        );
    }

    #[tokio::test]
    async fn coverage_history_retention() {
        let (s, _dir) = test_store().await;

        // Older than the retention window, inserted directly so the prune has
        // something to find.
        let old = Utc::now() - COVERAGE_HISTORY_RETENTION - Duration::hours(24);
        s.write(move |conn| {
        conn.execute(
            "INSERT INTO coverage_history(target, applied, partial, not_applied, skipped, percent, scanned_at)
             VALUES(1, 1, 0, 0, 0, 100, ?)",
            params![format_time(old)],
        )
        .map(|_| ())
        .map_err(|e| Error::sqlite("seed an old snapshot", e))
    })
    .await
    .expect("seed an old snapshot");
        s.add_coverage_snapshot(coverage::Snapshot {
            target: 1,
            applied: 1,
            percent: 100,
            ..coverage::Snapshot::default()
        })
        .await
        .expect("add_coverage_snapshot");

        let remaining: i64 = s
            .read(|conn| {
                conn.query_row("SELECT COUNT(*) FROM coverage_history", [], |r| r.get(0))
                    .map_err(|e| Error::sqlite("count", e))
            })
            .await
            .expect("count");
        assert_eq!(remaining, 1, "rows after the prune, want only the new one");
    }

    #[tokio::test]
    async fn coverage_muted() {
        let (s, _dir) = test_store().await;

        let got = s.list_coverage_muted().await.expect("fresh store");
        assert!(
            got.is_empty(),
            "list_coverage_muted on a fresh store = {got:?}"
        );

        let both = coverage::MuteScopes {
            ci: true,
            registry: true,
        };
        s.add_coverage_muted("team/b", "alice", both)
            .await
            .expect("add_coverage_muted");
        s.add_coverage_muted("team/a", "bob", both)
            .await
            .expect("add_coverage_muted");
        // Muting again narrows the scopes but keeps the original attribution rather
        // than rewriting it.
        s.add_coverage_muted(
            "team/a",
            "carol",
            coverage::MuteScopes {
                ci: true,
                registry: false,
            },
        )
        .await
        .expect("re-add_coverage_muted");
        let by: String = s
            .read(|conn| {
                conn.query_row(
                    "SELECT muted_by FROM coverage_muted WHERE project_path = 'team/a'",
                    [],
                    |r| r.get(0),
                )
                .map_err(|e| Error::sqlite("read muted_by", e))
            })
            .await
            .expect("read muted_by");
        assert_eq!(by, "bob", "muted_by, want the original bob");

        let got = s.list_coverage_muted().await.expect("list_coverage_muted");
        assert_eq!(got.len(), 2, "muted = {got:?}, want them sorted");
        assert_eq!(got[0].path, "team/a", "muted = {got:?}, want them sorted");
        assert_eq!(got[1].path, "team/b", "muted = {got:?}, want them sorted");
        assert_eq!(
            got[0].scopes,
            coverage::MuteScopes {
                ci: true,
                registry: false
            },
            "team/a scopes, want the narrowed mute"
        );
        assert_eq!(got[1].scopes, both, "team/b scopes, want every check muted");

        s.remove_coverage_muted("team/a")
            .await
            .expect("remove_coverage_muted");
        let got = s.list_coverage_muted().await.unwrap();
        assert_eq!(got.len(), 1, "muted after removal = {got:?}");
        assert_eq!(got[0].path, "team/b", "muted after removal = {got:?}");
        // Removing one that is not there is not an error.
        s.remove_coverage_muted("team/nope")
            .await
            .expect("remove_coverage_muted on a missing row");
    }

    #[tokio::test]
    async fn coverage_muted_unreadable_scopes_mute_the_whole_project() {
        let (s, _dir) = test_store().await;
        for (path, scopes) in [
            ("team/bad-json", "not json"),
            ("team/empty", "[]"),
            ("team/unknown", "[\"typo\"]"),
        ] {
            let path = path.to_string();
            let scopes = scopes.to_string();
            s.write(move |conn| {
            conn.execute(
                "INSERT INTO coverage_muted(project_path, muted_by, muted_at, scopes) VALUES(?, '', ?, ?)",
                params![path, now_rfc3339(), scopes],
            )
            .map(|_| ())
            .map_err(|e| Error::sqlite("seed mute", e))
        })
        .await
        .unwrap();
        }
        let got = s.list_coverage_muted().await.unwrap();
        assert_eq!(got.len(), 3);
        for m in got {
            assert!(m.scopes.all(), "{} fell back to a partial mute", m.path);
        }
    }

    /// Result rows written by earlier releases store an empty project list as
    /// `null`; reading one must yield the result rather than dropping it.
    #[tokio::test]
    async fn coverage_result_reads_null_lists() {
        let (s, _dir) = test_store().await;
        let payload = r#"{"projects":null,"excluded_projects":null,"scanned_at":"2026-09-07T10:00:46.152638Z","duration_ms":26,"triggered_by":"schedule","forklift_host":"forklift.example.com"}"#;
        s.write(move |conn| {
            conn.execute(
            "INSERT INTO coverage_results(id, payload, scanned_at, duration_ms) VALUES(1, ?, ?, ?)",
            params![payload, "2026-09-07T10:00:46.152638Z", 26],
        )
        .map(|_| ())
        .map_err(|e| Error::sqlite("insert", e))
        })
        .await
        .expect("insert legacy row");

        let got = s
            .read_coverage_result()
            .await
            .expect("read_coverage_result")
            .expect("legacy result is readable");
        assert!(got.projects.is_empty());
        assert!(got.excluded_projects.is_empty());
        assert_eq!(got.triggered_by, "schedule");
        assert_eq!(got.forklift_host, "forklift.example.com");
    }
}
