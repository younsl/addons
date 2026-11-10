//! The operational subcommands: validate and unused. They need no running
//! controller and print to stdout.

use std::io::Write;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context as _, Result};

use crate::config::{self, Config, Env};
use crate::humanize::{go_duration, human_bytes, round_duration};
use crate::pvscan::{self, DiscardRecorder, Finding, KIND_PVC, Scanner};

/// Bounds every CLI call that contacts AWS or the Kubernetes API.
const CLI_TIMEOUT: Duration = Duration::from_mins(1);

/// Loads the config at `path`, wrapping a failure so the command exits
/// non-zero with a clear message.
pub fn load_config(path: &Path, env: &Env) -> Result<Config> {
    config::load(path, env).with_context(|| format!("invalid config {}", path.display()))
}

/// Loads and validates the config file and reports the outcome. It never
/// contacts AWS or the Kubernetes API.
pub fn run_validate(path: &Path, env: &Env, out: &mut impl Write) -> Result<()> {
    let cfg = load_config(path, env)?;
    for w in &cfg.warnings {
        writeln!(out, "warning: {w}")?;
    }
    writeln!(
        out,
        "config {} is valid: region={}",
        path.display(),
        cfg.region
    )?;
    Ok(())
}

/// Lists the `PersistentVolumeClaims` and `PersistentVolumes` no workload is
/// using, grouped by kind and sorted longest-unused first. It reads the
/// Kubernetes API and never writes.
pub async fn run_unused(path: &Path, env: &Env, all: bool, out: &mut impl Write) -> Result<()> {
    let cfg = load_config(path, env)?;
    let client = crate::k8s::in_cluster_client()
        .context("in-cluster Kubernetes access is required to scan volumes")?;
    let scanner = Scanner::new(
        cfg.dry_run,
        Arc::new(pvscan::KubeClient::new(client)),
        Arc::new(DiscardRecorder),
        None,
    );
    let findings = tokio::time::timeout(CLI_TIMEOUT, scanner.findings())
        .await
        .context("read volume inventory: timed out")??;
    render_unused(&findings, all, out)
}

fn render_unused(findings: &[Finding], all: bool, out: &mut impl Write) -> Result<()> {
    let mut unused: Vec<&Finding> = findings
        .iter()
        .filter(|f| f.unused && (all || f.reportable))
        .collect();
    unused.sort_by(|a, b| a.kind.cmp(&b.kind).then(b.age.cmp(&a.age)));

    let mut table = Table::new();
    table.row(
        [
            "KIND",
            "NAMESPACE",
            "NAME",
            "REASON",
            "UNUSED_FOR",
            "CAPACITY",
            "STORAGE_CLASS",
            "EBS_VOLUME",
            "BOUND_TO",
        ]
        .iter()
        .map(ToString::to_string)
        .collect(),
    );
    let mut total: i64 = 0;
    for f in &unused {
        total += f.capacity_bytes;
        table.row(vec![
            f.kind.clone(),
            or_dash(&f.namespace),
            f.name.clone(),
            f.reason.clone(),
            go_duration(round_duration(f.age, Duration::from_mins(1))),
            human_bytes(f.capacity_bytes),
            or_dash(&f.storage_class),
            or_dash(&f.volume_id),
            or_dash(&f.bound_to()),
        ]);
    }
    if unused.is_empty() {
        table.row(vec![
            "(none)".into(),
            String::new(),
            String::new(),
            String::new(),
            String::new(),
            String::new(),
            String::new(),
            String::new(),
            String::new(),
        ]);
    }
    table.render(out)?;
    writeln!(
        out,
        "\n{} unused {} holding {}, out of {} objects scanned (minUnusedAge {})",
        unused.len(),
        pluralize(unused.len(), "object", "objects"),
        human_bytes(total),
        findings.len(),
        go_duration(pvscan::MIN_UNUSED_AGE)
    )?;
    let _ = KIND_PVC;
    Ok(())
}

/// A left-aligned column table with two spaces of padding, in the style of
/// Go's `text/tabwriter`: the last cell of a row is not padded.
struct Table {
    rows: Vec<Vec<String>>,
}

impl Table {
    const fn new() -> Self {
        Self { rows: Vec::new() }
    }

    fn row(&mut self, cells: Vec<String>) {
        self.rows.push(cells);
    }

    fn render(&self, out: &mut impl Write) -> Result<()> {
        let columns = self.rows.iter().map(Vec::len).max().unwrap_or(0);
        let mut widths = vec![0usize; columns];
        for row in &self.rows {
            for (i, cell) in row.iter().enumerate() {
                // The last cell of a row ends the line and is not part of the
                // column alignment.
                if i + 1 < row.len() {
                    widths[i] = widths[i].max(cell.chars().count());
                }
            }
        }
        for row in &self.rows {
            let mut line = String::new();
            for (i, cell) in row.iter().enumerate() {
                line.push_str(cell);
                if i + 1 < row.len() {
                    let pad = widths[i] + 2 - cell.chars().count();
                    line.extend(std::iter::repeat_n(' ', pad));
                }
            }
            writeln!(out, "{}", line.trim_end())?;
        }
        Ok(())
    }
}

/// Returns `singular` when `n == 1`, else `plural`.
const fn pluralize<'a>(n: usize, singular: &'a str, plural: &'a str) -> &'a str {
    if n == 1 { singular } else { plural }
}

/// Renders an empty column as a dash, so a blank cell always means "no
/// value" rather than a rendering slip.
fn or_dash(s: &str) -> String {
    if s.is_empty() { "-".into() } else { s.into() }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pvscan::{KIND_PV, REASON_NO_CONSUMER_POD, REASON_RELEASED};

    fn write_example() -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.yaml");
        std::fs::copy(
            Path::new(env!("CARGO_MANIFEST_DIR")).join("config.example.yaml"),
            &path,
        )
        .unwrap();
        (dir, path)
    }

    #[test]
    fn validate_prints_summary_and_fails_on_bad_config() {
        let (_dir, path) = write_example();
        let mut out = Vec::new();
        run_validate(&path, &Env::default(), &mut out).unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(
            text.ends_with("is valid: region=ap-northeast-2\n"),
            "{text}"
        );

        let fixed = path.with_file_name("fixed.yaml");
        std::fs::write(&fixed, "region: r\npolicies: []\n").unwrap();
        let mut out = Vec::new();
        run_validate(&fixed, &Env::default(), &mut out).unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(
            text.starts_with("warning: policies is no longer a config key"),
            "{text}"
        );
        assert!(text.contains("is valid: region=r"), "{text}");

        let bad = path.with_file_name("bad.yaml");
        std::fs::write(&bad, "dryRun: true\n").unwrap();
        let err = run_validate(&bad, &Env::default(), &mut Vec::new()).unwrap_err();
        assert!(format!("{err:#}").contains("invalid config"), "{err:#}");
        assert!(format!("{err:#}").contains("region is required"), "{err:#}");
        let err = load_config(&path.with_file_name("missing.yaml"), &Env::default()).unwrap_err();
        assert!(format!("{err:#}").contains("read config file"));
    }

    #[test]
    fn unused_table() {
        let gib = 1024 * 1024 * 1024;
        let findings = vec![
            Finding {
                kind: KIND_PVC.into(),
                namespace: "legacy".into(),
                name: "uploads".into(),
                unused: true,
                reportable: true,
                reason: REASON_NO_CONSUMER_POD.into(),
                age: Duration::from_secs(72 * 3600 + 20),
                capacity_bytes: 20 * gib,
                storage_class: "gp3".into(),
                volume_id: "vol-1".into(),
                volume_name: "pvc-77c1e004".into(),
                ..Finding::default()
            },
            Finding {
                kind: KIND_PV.into(),
                name: "pvc-9f2c1a4b".into(),
                unused: true,
                reportable: true,
                reason: REASON_RELEASED.into(),
                age: Duration::from_hours(512),
                capacity_bytes: 100 * gib,
                storage_class: "gp3".into(),
                volume_id: "vol-2".into(),
                claim_namespace: "legacy".into(),
                claim_name: "reports".into(),
                ..Finding::default()
            },
            Finding {
                kind: KIND_PVC.into(),
                namespace: "new".into(),
                name: "fresh".into(),
                unused: true,
                reportable: false,
                reason: REASON_NO_CONSUMER_POD.into(),
                age: Duration::from_mins(1),
                ..Finding::default()
            },
            Finding {
                kind: KIND_PVC.into(),
                namespace: "app".into(),
                name: "used".into(),
                ..Finding::default()
            },
        ];
        let mut out = Vec::new();
        render_unused(&findings, false, &mut out).unwrap();
        let text = String::from_utf8(out).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(
            lines[0],
            "KIND                   NAMESPACE  NAME          REASON           UNUSED_FOR  CAPACITY  STORAGE_CLASS  EBS_VOLUME  BOUND_TO"
        );
        assert_eq!(
            lines[1],
            "persistentvolume       -          pvc-9f2c1a4b  released         512h0m0s    100.0Gi   gp3            vol-2       legacy/reports"
        );
        assert_eq!(
            lines[2],
            "persistentvolumeclaim  legacy     uploads       no_consumer_pod  72h0m0s     20.0Gi    gp3            vol-1       pvc-77c1e004"
        );
        assert_eq!(lines[3], "");
        assert_eq!(
            lines[4],
            "2 unused objects holding 120.0Gi, out of 4 objects scanned (minUnusedAge 24h0m0s)"
        );

        let mut out = Vec::new();
        render_unused(&findings, true, &mut out).unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("new        fresh"), "{text}");
        assert!(
            text.contains("1m0s        0B        -              -           -"),
            "{text}"
        );
        assert!(text.contains("3 unused objects"));

        let mut out = Vec::new();
        render_unused(&[], false, &mut out).unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("\n(none)\n"), "{text}");
        assert!(text.contains("0 unused objects holding 0B, out of 0 objects scanned"));
    }

    #[test]
    fn helpers() {
        assert_eq!(pluralize(1, "a", "b"), "a");
        assert_eq!(pluralize(2, "a", "b"), "b");
        assert_eq!(or_dash(""), "-");
        assert_eq!(or_dash("x"), "x");
    }
}
