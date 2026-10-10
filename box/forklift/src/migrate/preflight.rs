//! Every check a migration must pass before its first write. All of them run
//! and are reported together, so one rerun fixes every problem at once.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::time::{Duration, Instant};

use futures_util::{StreamExt, stream};
use serde::Serialize;

use super::snapshot::{self, StagedSnapshot};
use super::{Endpoint, Error, Options, Result, has_meta};
use crate::objstore::{ConditionalWrites, PutBody, PutObjectInput, probe_conditional_writes};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    Pass,
    Warn,
    Fail,
    Skip,
}

/// Stable identifiers: a number always names the same check, whatever runs.
pub const CHECKS: &[(&str, &str)] = &[
    ("PF01", "lease"),
    ("PF02", "writers-stopped"),
    ("PF03", "locations"),
    ("PF04", "source-snapshot"),
    ("PF05", "snapshot-integrity"),
    ("PF06", "source-blobs"),
    ("PF07", "target-blobs"),
    ("PF08", "target-write"),
    ("PF09", "target-conditional-writes"),
    ("PF10", "target-metadata"),
    ("PF11", "target-capacity"),
    ("PF12", "staging-space"),
];

fn id_of(name: &str) -> &'static str {
    CHECKS
        .iter()
        .chain(super::postflight::CHECKS)
        .find(|(_, n)| *n == name)
        .map_or("PF??", |(id, _)| *id)
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Check {
    pub id: &'static str,
    pub name: &'static str,
    pub status: Status,
    pub detail: String,
    /// Wall time of the requests behind a check that talks to a service.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub latency_us: Option<u64>,
}

impl Check {
    pub(crate) fn new(name: &'static str, status: Status, detail: impl Into<String>) -> Self {
        Self {
            id: id_of(name),
            name,
            status,
            detail: detail.into(),
            latency_us: None,
        }
    }

    #[must_use]
    pub const fn took(mut self, elapsed: Duration) -> Self {
        self.latency_us = Some(elapsed.as_micros() as u64);
        self
    }

    pub fn pass(name: &'static str, detail: impl Into<String>) -> Self {
        Self::new(name, Status::Pass, detail)
    }

    pub fn warn(name: &'static str, detail: impl Into<String>) -> Self {
        Self::new(name, Status::Warn, detail)
    }

    pub fn fail(name: &'static str, detail: impl Into<String>) -> Self {
        Self::new(name, Status::Fail, detail)
    }
}

impl Status {
    pub(crate) const fn tag(self) -> &'static str {
        match self {
            Self::Pass => "PASS",
            Self::Warn => "WARN",
            Self::Fail => "FAIL",
            Self::Skip => "SKIP",
        }
    }
}

/// Logs a check the moment it finishes, so a run that spends minutes on one
/// check shows where it is before the report prints.
pub(crate) fn log(c: &Check) {
    let took = c
        .latency_us
        .map(|us| duration(Duration::from_micros(us)))
        .unwrap_or_default();
    tracing::info!(
        id = c.id,
        check = c.name,
        status = c.status.tag(),
        took = %took,
        detail = %c.detail,
        "check finished"
    );
}

/// Records a finished check and logs it.
pub(crate) fn push(checks: &mut Vec<Check>, c: Check) {
    log(&c);
    checks.push(c);
}

/// One line per check in identifier order, then the one-line summary.
pub fn render(checks: &[Check]) -> String {
    let mut sorted: Vec<&Check> = checks.iter().collect();
    sorted.sort_by_key(|c| c.id);
    let mut lines: Vec<String> = sorted
        .iter()
        .map(|c| {
            let tag = c.status.tag();
            match c.latency_us {
                Some(us) => format!(
                    "  [{tag}] {} {:<26} {} (took {})",
                    c.id,
                    c.name,
                    c.detail,
                    duration(Duration::from_micros(us))
                ),
                None => format!("  [{tag}] {} {:<26} {}", c.id, c.name, c.detail),
            }
        })
        .collect();
    lines.push(summary(checks));
    lines.join("\n")
}

pub fn bytes(n: u64) -> String {
    const UNITS: [&str; 6] = ["B", "KiB", "MiB", "GiB", "TiB", "PiB"];
    let mut v = n as f64;
    let mut u = 0;
    while v >= 1024.0 && u < UNITS.len() - 1 {
        v /= 1024.0;
        u += 1;
    }
    if u == 0 {
        format!("{n} B")
    } else {
        format!("{v:.1} {}", UNITS[u])
    }
}

pub fn duration(d: Duration) -> String {
    let ms = d.as_secs_f64() * 1000.0;
    if ms < 10.0 {
        format!("{ms:.1} ms")
    } else if ms < 1000.0 {
        format!("{} ms", ms.round() as u64)
    } else {
        format!("{:.2} s", ms / 1000.0)
    }
}

pub fn summary(checks: &[Check]) -> String {
    let count = |s| checks.iter().filter(|c| c.status == s).count();
    let failed: Vec<&str> = {
        let mut ids: Vec<&str> = checks
            .iter()
            .filter(|c| c.status == Status::Fail)
            .map(|c| c.id)
            .collect();
        ids.sort_unstable();
        ids
    };
    let stage = if !checks.is_empty() && checks.iter().all(|c| c.id.starts_with("PV")) {
        "postflight"
    } else {
        "preflight"
    };
    let mut line = format!(
        "{stage}: {} checks, {} passed, {} warned, {} failed, {} skipped",
        checks.len(),
        count(Status::Pass),
        count(Status::Warn),
        count(Status::Fail),
        count(Status::Skip)
    );
    if !failed.is_empty() {
        line.push_str(&format!(" ({})", failed.join(", ")));
    }
    line
}

pub struct Plan {
    pub checks: Vec<Check>,
    pub snapshot: StagedSnapshot,
    /// Blobs the metadata names that the target does not hold yet.
    pub missing: BTreeMap<String, Option<i64>>,
    /// Named blobs already missing in the source, carried over broken.
    pub source_missing: BTreeSet<String>,
}

/// Bytes of free staging space kept beyond what the copy needs.
const STAGING_HEADROOM: u64 = 64 << 20;

pub async fn run(
    src: &Endpoint,
    dst: &Endpoint,
    opts: &Options,
    mut checks: Vec<Check>,
) -> Result<Plan> {
    checks.iter().for_each(log);
    push(&mut checks, locations(src, dst));

    tracing::info!(key = %src.meta_key, "preflight: downloading the source metadata snapshot");
    let started = Instant::now();
    let downloaded = snapshot::download(src, &opts.staging).await;
    let took = started.elapsed();
    let snapshot = match downloaded {
        Ok(s) => {
            push(
                &mut checks,
                Check::pass(
                    "source-snapshot",
                    format!("{} ({})", src.meta_key, bytes(s.size)),
                )
                .took(took),
            );
            let detail = format!(
                "integrity ok, {} blobs named{}",
                s.blobs.len(),
                if s.unrecorded > 0 {
                    format!(", {} without a blobs row", s.unrecorded)
                } else {
                    String::new()
                }
            );
            push(&mut checks, Check::pass("snapshot-integrity", detail));
            Some(s)
        }
        Err(Error::Object(e)) if e.is_not_found() => {
            push(
                &mut checks,
                Check::fail(
                    "source-snapshot",
                    format!("no metadata snapshot at {}", src.meta_key),
                )
                .took(took),
            );
            None
        }
        Err(Error::Refused(msg)) => {
            push(
                &mut checks,
                Check::pass("source-snapshot", src.meta_key.clone()).took(took),
            );
            push(&mut checks, Check::fail("snapshot-integrity", msg));
            None
        }
        Err(e) => {
            push(
                &mut checks,
                Check::fail("source-snapshot", e.to_string()).took(took),
            );
            None
        }
    };

    let mut missing = BTreeMap::new();
    let mut source_missing = BTreeSet::new();
    if let Some(snap) = &snapshot {
        let Absent {
            missing: src_missing,
            error: src_err,
            took: src_took,
            latencies: src_lat,
        } = {
            tracing::info!(
                blobs = snap.blobs.len(),
                "preflight: checking the named blobs in the source"
            );
            absent(src, &snap.blobs, opts.concurrency).await
        };
        let src_rate = per_request(&src_lat);
        push(&mut checks, match (src_err, src_missing.len()) {
            (Some(e), _) => Check::fail("source-blobs", format!("cannot check the source: {e}")),
            (None, 0) => Check::pass(
                "source-blobs",
                format!("all {} named blobs present{src_rate}", snap.blobs.len()),
            ),
            (None, n) => {
                let detail = format!(
                    "{n} named blobs are missing in the source (first: {})",
                    src_missing
                        .keys()
                        .next()
                        .map(String::as_str)
                        .unwrap_or_default()
                );
                if opts.allow_missing_source_blobs {
                    Check::warn(
                        "source-blobs",
                        format!("{detail}; allowed, they stay broken"),
                    )
                } else {
                    Check::fail(
                        "source-blobs",
                        format!(
                            "{detail}; pass --allow-missing-source-blobs to carry them over broken"
                        ),
                    )
                }
            }
        }.took(src_took));
        let Absent {
            missing: dst_missing,
            error: dst_err,
            took: dst_took,
            latencies: dst_lat,
        } = {
            tracing::info!(
                blobs = snap.blobs.len(),
                "preflight: checking the named blobs in the target"
            );
            absent(dst, &snap.blobs, opts.concurrency).await
        };
        let dst_rate = per_request(&dst_lat);
        push(
            &mut checks,
            match dst_err {
                Some(e) => Check::fail("target-blobs", format!("cannot check the target: {e}")),
                None => Check::pass(
                    "target-blobs",
                    format!(
                        "{} of {} named blobs already present{dst_rate}",
                        snap.blobs.len() - dst_missing.len(),
                        snap.blobs.len()
                    ),
                ),
            }
            .took(dst_took),
        );
        missing = dst_missing
            .into_iter()
            .filter(|(d, _)| !src_missing.contains_key(d))
            .collect();
        source_missing = src_missing.into_keys().collect();
    }

    push(&mut checks, target_write(dst).await);
    push(
        &mut checks,
        target_conditional_writes(dst, opts.require_conditional_writes).await,
    );
    push(&mut checks, target_metadata(dst, opts.overwrite_meta).await);

    let need: u64 = missing.values().map(|s| s.unwrap_or(0).max(0) as u64).sum();
    push(&mut checks, target_capacity(dst, need).await);
    push(&mut checks, staging_space(opts, &missing));

    for (id, name) in CHECKS {
        if !checks.iter().any(|c| c.id == *id) {
            push(
                &mut checks,
                Check::new(name, Status::Skip, "not run: an earlier check failed"),
            );
        }
    }
    checks.sort_by_key(|c| c.id);
    if checks.iter().any(|c| c.status == Status::Fail) {
        return Err(Error::Preflight(checks));
    }
    let snapshot = snapshot.expect("a passing preflight has a snapshot");
    Ok(Plan {
        checks,
        snapshot,
        missing,
        source_missing,
    })
}

fn normalized_prefix(p: &str) -> String {
    let p = p.trim_matches('/');
    if p.is_empty() {
        String::new()
    } else {
        format!("{p}/")
    }
}

/// The same bucket on the same endpoint overlaps when either prefix contains
/// the other: the copy would then read what it writes.
pub(crate) fn locations(src: &Endpoint, dst: &Endpoint) -> Check {
    let same_store = src.endpoint.eq_ignore_ascii_case(&dst.endpoint) && src.bucket == dst.bucket;
    let (a, b) = (
        normalized_prefix(&src.prefix),
        normalized_prefix(&dst.prefix),
    );
    if same_store && (a.starts_with(&b) || b.starts_with(&a)) {
        return Check::fail(
            "locations",
            format!(
                "source and target overlap ({} / {})",
                src.describe(),
                dst.describe()
            ),
        );
    }
    Check::pass(
        "locations",
        format!("{} -> {}", src.describe(), dst.describe()),
    )
}

struct Absent {
    missing: BTreeMap<String, Option<i64>>,
    error: Option<String>,
    took: Duration,
    latencies: Vec<Duration>,
}

pub(crate) fn per_request(latencies: &[Duration]) -> String {
    if latencies.is_empty() {
        return String::new();
    }
    let total: Duration = latencies.iter().sum();
    let max = latencies.iter().max().copied().unwrap_or_default();
    format!(
        ", {} HEAD requests, avg {}, max {}",
        latencies.len(),
        duration(total / latencies.len() as u32),
        duration(max)
    )
}

async fn absent(e: &Endpoint, blobs: &BTreeMap<String, Option<i64>>, concurrency: usize) -> Absent {
    let started = Instant::now();
    let results: Vec<_> = stream::iter(blobs.iter())
        .map(|(d, s)| async move {
            let one = Instant::now();
            let r = e.blobs.exists(d).await;
            (d.clone(), *s, r, one.elapsed())
        })
        .buffer_unordered(concurrency.max(1))
        .collect()
        .await;
    let mut out = BTreeMap::new();
    let mut first_err = None;
    let mut latencies = Vec::with_capacity(results.len());
    for (d, size, r, took) in results {
        latencies.push(took);
        match r {
            Ok(true) => {}
            Ok(false) => {
                out.insert(d, size);
            }
            Err(err) => {
                first_err.get_or_insert_with(|| err.to_string());
            }
        }
    }
    Absent {
        missing: out,
        error: first_err,
        took: started.elapsed(),
        latencies,
    }
}

fn probe_key(dst: &Endpoint, kind: &str) -> String {
    format!(
        "{}.forklift-migrate/{kind}-{}",
        normalized_prefix(&dst.prefix),
        uuid::Uuid::new_v4()
    )
}

/// Writes, reads back and deletes a probe object, which catches read-only
/// credentials, a missing bucket and a store that mangles uploads.
async fn target_write(dst: &Endpoint) -> Check {
    let key = probe_key(dst, "write");
    let started = Instant::now();
    let body: Vec<u8> = (0..4096u32).map(|i| (i * 31 % 251) as u8).collect();
    let result = async {
        dst.objects
            .put_object(PutObjectInput {
                bucket: dst.bucket.clone(),
                key: key.clone(),
                body: PutBody::Bytes(body.clone()),
                content_length: body.len() as i64,
                metadata: HashMap::default(),
                if_match: None,
                if_none_match: None,
            })
            .await
            .map_err(|e| format!("put: {e}"))?;
        let mut obj = dst
            .objects
            .get_object(&dst.bucket, &key)
            .await
            .map_err(|e| format!("get: {e}"))?;
        let mut got = Vec::new();
        tokio::io::AsyncReadExt::read_to_end(&mut obj.body, &mut got)
            .await
            .map_err(|e| format!("read: {e}"))?;
        if got != body {
            return Err(format!(
                "read back {} bytes that differ from the {} written",
                got.len(),
                body.len()
            ));
        }
        Ok(())
    }
    .await;
    let cleanup = dst.objects.delete_object(&dst.bucket, &key).await;
    let check = match (result, cleanup) {
        (Ok(()), Ok(())) => Check::pass(
            "target-write",
            format!(
                "{} written, read back and deleted",
                bytes(body.len() as u64)
            ),
        ),
        (Ok(()), Err(e)) => Check::fail("target-write", format!("delete: {e}")),
        (Err(e), _) => Check::fail("target-write", e),
    };
    check.took(started.elapsed())
}

async fn target_conditional_writes(dst: &Endpoint, required: bool) -> Check {
    let key = probe_key(dst, "conditional");
    let started = Instant::now();
    let probed = probe_conditional_writes(dst.objects.as_ref(), &dst.bucket, &key).await;
    let check = match probed {
        Ok(ConditionalWrites::Enforced) => Check::pass(
            "target-conditional-writes",
            "enforced; HA can run on the target",
        ),
        Ok(ConditionalWrites::Ignored { reason }) if required => Check::fail(
            "target-conditional-writes",
            format!("{reason}; the target deployment runs HA, which needs them"),
        ),
        Ok(ConditionalWrites::Ignored { reason }) => Check::warn(
            "target-conditional-writes",
            format!("{reason}; run the target with a single replica"),
        ),
        Err(e) => Check::fail("target-conditional-writes", e.to_string()),
    };
    check.took(started.elapsed())
}

async fn target_metadata(dst: &Endpoint, overwrite: bool) -> Check {
    let started = Instant::now();
    let found = has_meta(dst).await;
    let check = match found {
        Ok(false) => Check::pass("target-metadata", "no snapshot yet"),
        Ok(true) if overwrite => {
            Check::warn("target-metadata", "a snapshot exists and will be replaced")
        }
        Ok(true) => Check::fail(
            "target-metadata",
            "a snapshot already exists; pass --overwrite-meta to replace it",
        ),
        Err(e) => Check::fail("target-metadata", e.to_string()),
    };
    check.took(started.elapsed())
}

async fn target_capacity(dst: &Endpoint, need: u64) -> Check {
    let need_h = bytes(need);
    let Some(admin) = &dst.admin else {
        return Check::warn(
            "target-capacity",
            format!("{need_h} to copy; not checked, the target has no admin API configured"),
        );
    };
    let started = Instant::now();
    let info = admin.cluster_info().await;
    let check = match info {
        Ok(info) if info.total_capacity_bytes == 0 => Check::warn(
            "target-capacity",
            format!("{need_h} to copy; the target reports no capacity"),
        ),
        Ok(info) if (info.available_bytes as u64) < need => Check::fail(
            "target-capacity",
            format!(
                "{need_h} to copy, {} available",
                bytes(info.available_bytes as u64)
            ),
        ),
        Ok(info) => Check::pass(
            "target-capacity",
            format!(
                "{need_h} to copy, {} available",
                bytes(info.available_bytes as u64)
            ),
        ),
        Err(e) => Check::warn(
            "target-capacity",
            format!("{need_h} to copy; admin API: {e}"),
        ),
    };
    check.took(started.elapsed())
}

/// Each concurrent copy stages one blob, so the largest `concurrency` blobs
/// bound what the staging directory must hold at once.
fn staging_space(opts: &Options, missing: &BTreeMap<String, Option<i64>>) -> Check {
    let mut sizes: Vec<u64> = missing
        .values()
        .map(|s| s.unwrap_or(0).max(0) as u64)
        .collect();
    sizes.sort_unstable_by(|a, b| b.cmp(a));
    let need: u64 = sizes.iter().take(opts.concurrency.max(1)).sum::<u64>() + STAGING_HEADROOM;
    match crate::storage::disk_usage(&opts.staging) {
        Ok(disk) if (disk.available_bytes as u64) < need => Check::fail(
            "staging-space",
            format!(
                "{} needs {} free, {} available; lower --concurrency or grow the volume",
                opts.staging.display(),
                bytes(need),
                bytes(disk.available_bytes as u64)
            ),
        ),
        Ok(disk) => Check::pass(
            "staging-space",
            format!(
                "{} has {} free, {} needed",
                opts.staging.display(),
                bytes(disk.available_bytes as u64),
                bytes(need)
            ),
        ),
        Err(e) => Check::warn(
            "staging-space",
            format!("cannot measure {}: {e}", opts.staging.display()),
        ),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use async_trait::async_trait;

    use crate::migrate::preflight::*;
    use crate::migrate::tests::{Objects, Side, endpoint, ok_guard, opts, seeded, side};
    use crate::migrate::{Options, migrate};
    use crate::storage::{BlobStore, ClusterAdmin, ClusterInfo};

    fn names(checks: &[Check], status: Status) -> Vec<&'static str> {
        checks
            .iter()
            .filter(|c| c.status == status)
            .map(|c| c.name)
            .collect()
    }

    async fn preflight_of(src: &Side, dst: &Side, o: &Options) -> Vec<Check> {
        match migrate(&endpoint(src), &endpoint(dst), o, ok_guard()).await {
            Ok(r) => r.preflight,
            Err(Error::Preflight(checks)) => checks,
            Err(e) => panic!("unexpected error: {e}"),
        }
    }

    #[test]
    fn units_are_human_readable() {
        assert_eq!(bytes(512), "512 B");
        assert_eq!(bytes(344_064), "336.0 KiB");
        assert_eq!(bytes(29_794_553_856), "27.7 GiB");
        assert_eq!(duration(Duration::from_micros(340)), "0.3 ms");
        assert_eq!(duration(Duration::from_millis(34)), "34 ms");
        assert_eq!(
            per_request(&[Duration::from_millis(1), Duration::from_millis(3)]),
            ", 2 HEAD requests, avg 2.0 ms, max 3.0 ms"
        );
        assert_eq!(duration(Duration::from_millis(2346)), "2.35 s");
        let c = Check::pass("target-write", "ok").took(Duration::from_millis(12));
        assert!(render(&[c]).contains("ok (took 12 ms)"));
    }

    #[test]
    fn identifiers_are_unique_and_cover_every_check() {
        let mut ids: Vec<&str> = CHECKS.iter().map(|(id, _)| *id).collect();
        ids.dedup();
        assert_eq!(ids.len(), CHECKS.len());
        assert_eq!(Check::pass("staging-space", "").id, "PF12");
        assert_eq!(Check::fail("unknown", "").id, "PF??");
    }

    #[test]
    fn summary_counts_and_names_failures() {
        let checks = vec![
            Check::fail("target-write", "x"),
            Check::pass("lease", "y"),
            Check::warn("target-capacity", "z"),
            Check::fail("locations", "w"),
        ];
        assert_eq!(
            summary(&checks),
            "preflight: 4 checks, 1 passed, 1 warned, 2 failed, 0 skipped (PF03, PF08)"
        );
        let text = render(&checks);
        let lines: Vec<&str> = text.lines().collect();
        assert!(lines[0].contains("PF01 lease"), "{text}");
        assert!(lines[1].contains("[FAIL] PF03 locations"), "{text}");
        assert_eq!(lines.len(), 5);
        assert!(lines[4].starts_with("preflight: 4 checks"));
    }

    #[tokio::test]
    async fn a_full_run_reports_all_twelve_checks() {
        let (src, _) = seeded().await;
        let dst = side("http://dst");
        let staging = tempfile::tempdir().unwrap();
        let guard = crate::migrate::Guard {
            checks: vec![
                Check::pass("lease", "t"),
                Check::pass("writers-stopped", "t"),
            ],
            held: Box::new(|| true),
        };
        let logs = crate::server::logging::tests::BufWriter::default();
        let subscriber = tracing_subscriber::fmt()
            .with_writer(logs.clone())
            .event_format(crate::server::logging::SlogFormat::Json)
            .finish();
        let _log = tracing::subscriber::set_default(subscriber);
        let report = migrate(
            &endpoint(&src),
            &endpoint(&dst),
            &Options {
                dry_run: true,
                ..opts(staging.path())
            },
            guard,
        )
        .await
        .unwrap();
        let mut got: Vec<&str> = report.preflight.iter().map(|c| c.id).collect();
        got.sort_unstable();
        let want: Vec<&str> = CHECKS.iter().map(|(id, _)| *id).collect();
        assert_eq!(got, want);

        // Each check is logged as it finishes, with the slow steps announced
        // before they start, so a long run is never silent.
        let text = String::from_utf8(logs.0.lock().unwrap().clone()).unwrap();
        let lines: Vec<serde_json::Value> = text
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        for id in &want {
            assert!(
                lines
                    .iter()
                    .any(|l| l["msg"] == "check finished" && l["id"] == *id),
                "{id} not logged:\n{text}"
            );
        }
        let pos = |msg: &str| {
            lines
                .iter()
                .position(|l| l["msg"] == msg)
                .unwrap_or_else(|| panic!("no {msg:?}:\n{text}"))
        };
        assert!(
            pos("preflight: downloading the source metadata snapshot")
                < pos("checking the metadata snapshot's integrity")
        );
        assert!(lines.iter().all(|l| l["component"] == "migrate"), "{text}");
    }

    #[test]
    fn overlapping_locations_fail() {
        let a = side("http://S3:9000");
        let mut src = endpoint(&a);
        let mut dst = endpoint(&a);
        dst.endpoint = "http://s3:9000".into();
        assert_eq!(
            locations(&src, &dst).status,
            Status::Fail,
            "same prefix, case differs"
        );
        src.prefix = "team".into();
        dst.prefix = "team/new".into();
        assert_eq!(locations(&src, &dst).status, Status::Fail, "nested prefix");
        dst.prefix = "teamx".into();
        assert_eq!(locations(&src, &dst).status, Status::Pass, "sibling prefix");
        dst.bucket = "other".into();
        dst.prefix = "team".into();
        assert_eq!(locations(&src, &dst).status, Status::Pass, "other bucket");
    }

    #[tokio::test]
    async fn every_failure_is_reported_together() {
        let empty = side("http://src");
        let dst = side("http://src");
        let staging = tempfile::tempdir().unwrap();
        let checks = preflight_of(&empty, &dst, &opts(staging.path())).await;
        let failed = names(&checks, Status::Fail);
        assert!(failed.contains(&"locations"), "{failed:?}");
        assert!(failed.contains(&"source-snapshot"), "{failed:?}");
        assert!(names(&checks, Status::Pass).contains(&"target-write"));
        assert_eq!(checks.len(), CHECKS.len(), "every check accounted for");
        assert!(names(&checks, Status::Skip).contains(&"source-blobs"));
        assert!(
            names(&checks, Status::Skip).contains(&"lease"),
            "no lease in this guard"
        );
        let text = render(&checks);
        assert!(text.contains("[FAIL] PF03 locations"), "{text}");
        assert!(text.ends_with("skipped (PF03, PF04)"), "{text}");
    }

    #[tokio::test]
    async fn corrupt_snapshot_fails_integrity() {
        let src = side("http://src");
        src.objects.items.lock().insert(
            "meta/forklift.db".into(),
            (
                b"definitely not sqlite, 32 bytes!".to_vec(),
                "\"e\"".into(),
                HashMap::default(),
            ),
        );
        let staging = tempfile::tempdir().unwrap();
        let checks = preflight_of(&src, &side("http://dst"), &opts(staging.path())).await;
        assert!(
            names(&checks, Status::Fail).contains(&"snapshot-integrity"),
            "{checks:?}"
        );
    }

    #[tokio::test]
    async fn missing_source_blobs_fail_unless_allowed() {
        let (src, digests) = seeded().await;
        src.blobs.delete(&digests[0]).await.unwrap();
        let dst = side("http://dst");
        let staging = tempfile::tempdir().unwrap();

        let checks = preflight_of(&src, &dst, &opts(staging.path())).await;
        assert!(
            names(&checks, Status::Fail).contains(&"source-blobs"),
            "{checks:?}"
        );

        let allowed = Options {
            allow_missing_source_blobs: true,
            ..opts(staging.path())
        };
        let report = migrate(&endpoint(&src), &endpoint(&dst), &allowed, ok_guard())
            .await
            .unwrap();
        assert!(names(&report.preflight, Status::Warn).contains(&"source-blobs"));
        assert_eq!(report.copied, 4, "the missing one is not attempted");
    }

    #[tokio::test]
    async fn existing_target_metadata_needs_overwrite() {
        let (src, _) = seeded().await;
        let dst = side("http://dst");
        dst.objects.items.lock().insert(
            "meta/forklift.db".into(),
            (b"x".to_vec(), "\"x\"".into(), HashMap::default()),
        );
        let staging = tempfile::tempdir().unwrap();
        let checks = preflight_of(&src, &dst, &opts(staging.path())).await;
        assert!(names(&checks, Status::Fail).contains(&"target-metadata"));
        let checks = preflight_of(
            &src,
            &dst,
            &Options {
                overwrite_meta: true,
                dry_run: true,
                ..opts(staging.path())
            },
        )
        .await;
        assert!(names(&checks, Status::Warn).contains(&"target-metadata"));
    }

    #[tokio::test]
    async fn ignored_conditional_writes_fail_only_when_required() {
        let (src, _) = seeded().await;
        let dst = Side {
            objects: Arc::new(Objects {
                ignore_conditions: true,
                ..Objects::default()
            }),
            ..side("http://dst")
        };
        let staging = tempfile::tempdir().unwrap();
        let dry = Options {
            dry_run: true,
            ..opts(staging.path())
        };
        let checks = preflight_of(&src, &dst, &dry).await;
        assert!(names(&checks, Status::Warn).contains(&"target-conditional-writes"));
        let checks = preflight_of(
            &src,
            &dst,
            &Options {
                require_conditional_writes: true,
                ..dry
            },
        )
        .await;
        assert!(names(&checks, Status::Fail).contains(&"target-conditional-writes"));
    }

    struct Full(i64);

    #[async_trait]
    impl ClusterAdmin for Full {
        async fn cluster_info(&self) -> crate::storage::Result<ClusterInfo> {
            let mut info = ClusterInfo::default();
            info.set_capacity(1000, 1000 - self.0 as u64, self.0 as u64);
            Ok(info)
        }
    }

    #[tokio::test]
    async fn target_capacity_uses_the_admin_api() {
        let (src, _) = seeded().await;
        let dst = side("http://dst");
        let staging = tempfile::tempdir().unwrap();
        let dry = Options {
            dry_run: true,
            ..opts(staging.path())
        };
        for (free, want) in [(10, Status::Fail), (500, Status::Pass)] {
            let mut d = endpoint(&dst);
            d.admin = Some(Arc::new(Full(free)));
            let checks = match migrate(&endpoint(&src), &d, &dry, ok_guard()).await {
                Ok(r) => r.preflight,
                Err(Error::Preflight(c)) => c,
                Err(e) => panic!("{e}"),
            };
            let cap = checks.iter().find(|c| c.name == "target-capacity").unwrap();
            assert_eq!(cap.status, want, "free={free}: {cap:?}");
        }
    }

    #[test]
    fn staging_space_counts_the_largest_concurrent_blobs() {
        let staging = tempfile::tempdir().unwrap();
        let o = Options {
            concurrency: 2,
            ..opts(staging.path())
        };
        let small: BTreeMap<String, Option<i64>> =
            [("a".to_string(), Some(10)), ("b".to_string(), None)].into();
        assert_eq!(staging_space(&o, &small).status, Status::Pass);
        let huge: BTreeMap<String, Option<i64>> = [
            ("a".to_string(), Some(i64::MAX / 4)),
            ("b".to_string(), Some(i64::MAX / 4)),
        ]
        .into();
        assert_eq!(staging_space(&o, &huge).status, Status::Fail);
    }
}
