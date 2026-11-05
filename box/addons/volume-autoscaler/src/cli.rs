//! The operational subcommands: validate, autoscalers, instances, unused,
//! crd. They need no running controller and print to stdout.

use std::collections::HashMap;
use std::io::Write;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context as _, Result};
use kube::CustomResourceExt as _;

use crate::autoscaler::instance_volume::api::{AutoscalerApi, KubeAutoscalers};
use crate::autoscaler::instance_volume::crd::InstanceVolumeAutoscaler;
use crate::autoscaler::instance_volume::policy::{Effective, Resolver, Step};
use crate::autoscaler::persistent_volume_claim::crd::PersistentVolumeClaimAutoscaler;
use crate::awsx::{Clients, Instance};
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
    writeln!(
        out,
        "config {} is valid: region={}",
        path.display(),
        cfg.region
    )?;
    Ok(())
}

/// A custom resource the `crd` subcommand prints.
#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
pub enum CrdKind {
    InstanceVolumeAutoscaler,
    PersistentVolumeClaimAutoscaler,
}

/// Prints the `CustomResourceDefinition` of `kind` as JSON, which `make crd`
/// renders into the chart.
pub fn run_crd(kind: CrdKind, out: &mut impl Write) -> Result<()> {
    let crd = match kind {
        CrdKind::InstanceVolumeAutoscaler => InstanceVolumeAutoscaler::crd(),
        CrdKind::PersistentVolumeClaimAutoscaler => PersistentVolumeClaimAutoscaler::crd(),
    };
    serde_json::to_writer_pretty(&mut *out, &crd)?;
    writeln!(out)?;
    Ok(())
}

/// Lists the autoscalers in the cluster through the kubeconfig or in-cluster
/// credentials.
async fn list_autoscalers() -> Result<Vec<InstanceVolumeAutoscaler>> {
    let client = tokio::time::timeout(CLI_TIMEOUT, crate::k8s::client())
        .await
        .context("connect to Kubernetes: timed out")??;
    let api = KubeAutoscalers::new(client);
    tokio::time::timeout(CLI_TIMEOUT, api.list())
        .await
        .context("list InstanceVolumeAutoscalers: timed out")?
        .map_err(|e| anyhow::anyhow!("list InstanceVolumeAutoscalers: {e}"))
}

/// Prints every autoscaler and its effective settings in name order, then
/// the rejected ones. With `with_count` it discovers instances via AWS and
/// adds a MATCHED column.
pub async fn run_autoscalers(
    path: &Path,
    env: &Env,
    with_count: bool,
    out: &mut impl Write,
) -> Result<()> {
    let cfg = load_config(path, env)?;
    let resolver = Resolver::new(&list_autoscalers().await?);
    let counts = if with_count {
        Some(autoscaler_counts(
            &discover_instances(&cfg).await?,
            &resolver,
        ))
    } else {
        None
    };
    render_autoscalers(&resolver, counts.as_ref(), out)
}

fn render_autoscalers(
    resolver: &Resolver,
    counts: Option<&HashMap<String, usize>>,
    out: &mut impl Write,
) -> Result<()> {
    let mut table = Table::new();
    let mut header = vec![
        "NAME",
        "TARGET",
        "UTILIZATION%",
        "SCALE_UP",
        "MAX_GIB",
        "COOLDOWN",
        "ALERT",
    ];
    if counts.is_some() {
        header.push("MATCHED");
    }
    table.row(header.iter().map(ToString::to_string).collect());
    for c in &resolver.autoscalers {
        let eff = &c.effective;
        let mut cells = vec![
            eff.policy.clone(),
            c.target.to_string(),
            eff.usage_threshold_percent.to_string(),
            scale_up_summary(eff),
            eff.max_volume_size_gib.to_string(),
            crate::autoscaler::format_cooldown(eff.cooldown),
            eff.alert_enabled.to_string(),
        ];
        if let Some(counts) = counts {
            cells.push(counts.get(&eff.policy).copied().unwrap_or(0).to_string());
        }
        table.row(cells);
    }
    for (name, err) in &resolver.invalid {
        table.row(vec![name.clone(), format!("(invalid: {err})")]);
    }
    if resolver.autoscalers.is_empty() && resolver.invalid.is_empty() {
        table.row(vec!["(none)".into()]);
    }
    table.render(out)?;
    Ok(())
}

/// Initializes AWS clients and discovers the candidate instances for `cfg`.
/// It is the shared discovery step behind every CLI subcommand that contacts
/// AWS.
async fn discover_instances(cfg: &Config) -> Result<Vec<Instance>> {
    let clients = tokio::time::timeout(CLI_TIMEOUT, Clients::new(&cfg.region))
        .await
        .context("initialize AWS clients: timed out")?;
    tokio::time::timeout(CLI_TIMEOUT, clients.describe_target_instances())
        .await
        .context("discover instances: timed out")?
        .context("discover instances")
}

/// How many instances each autoscaler would reconcile, every autoscaler
/// seeded at 0. Instances held back by an ambiguous autoscaler do not count.
fn autoscaler_counts(instances: &[Instance], resolver: &Resolver) -> HashMap<String, usize> {
    let mut counts: HashMap<String, usize> = resolver.names().into_iter().map(|n| (n, 0)).collect();
    for (_, eff) in resolver.plan(instances).managed {
        *counts.entry(eff.policy).or_default() += 1;
    }
    counts
}

/// Discovers candidate instances via AWS and lists each with the autoscaler
/// that targets it.
pub async fn run_instances(path: &Path, env: &Env, out: &mut impl Write) -> Result<()> {
    let cfg = load_config(path, env)?;
    let resolver = Resolver::new(&list_autoscalers().await?);
    let instances = discover_instances(&cfg).await?;
    render_instances(&cfg, &resolver, &instances, out)
}

fn render_instances(
    cfg: &Config,
    resolver: &Resolver,
    instances: &[Instance],
    out: &mut impl Write,
) -> Result<()> {
    let plan = resolver.plan(instances);
    let mut owner: Vec<String> = vec!["(none)".into(); instances.len()];
    for (i, eff) in &plan.managed {
        owner[*i].clone_from(&eff.policy);
    }
    for (i, names) in &plan.refused {
        owner[*i] = format!("(ambiguous: {})", names.join(","));
    }
    let mut rows: Vec<(&String, &Instance)> = owner.iter().zip(instances).collect();
    rows.sort_by(|a, b| {
        let none = |s: &str| s.starts_with('(');
        none(a.0)
            .cmp(&none(b.0))
            .then_with(|| a.0.cmp(b.0))
            .then_with(|| a.1.id.cmp(&b.1.id))
    });
    let mut table = Table::new();
    table.row(
        [
            "AUTOSCALER",
            "INSTANCE_ID",
            "NAME",
            "ROOT_VOLUME",
            "SIZE_GIB",
        ]
        .iter()
        .map(ToString::to_string)
        .collect(),
    );
    for (name, inst) in rows {
        table.row(vec![
            name.clone(),
            inst.id.clone(),
            or_dash(&inst.name),
            or_dash(&inst.root_volume_id),
            inst.root_volume_size_gib.to_string(),
        ]);
    }
    table.render(out)?;
    writeln!(
        out,
        "\n{} {} discovered in {}, {} targeted",
        instances.len(),
        pluralize(instances.len(), "instance", "instances"),
        cfg.region,
        plan.managed.len()
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

/// Renders the scale-up rules compactly, e.g. `max(10%,1GiB)`.
fn scale_up_summary(eff: &Effective) -> String {
    if eff.paused {
        return "disabled".into();
    }
    let steps: Vec<String> = eff
        .steps
        .iter()
        .map(|s| match s {
            Step::Percent(p) => format!("{p}%"),
            Step::GiB(g) => format!("{g}GiB"),
        })
        .collect();
    format!(
        "{}({})",
        if eff.select_min { "min" } else { "max" },
        steps.join(",")
    )
}

/// Renders an empty column as a dash, so a blank cell always means "no
/// value" rather than a rendering slip.
fn or_dash(s: &str) -> String {
    if s.is_empty() { "-".into() } else { s.into() }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;
    use crate::autoscaler::instance_volume::policy::tests::{
        by_id, by_tags, iva, with_scale_up, with_utilization,
    };
    use crate::autoscaler::{ScalingPolicyType, SelectPolicy};
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

        let bad = path.with_file_name("bad.yaml");
        std::fs::write(&bad, "region: r\npolicies: []\n").unwrap();
        let err = run_validate(&bad, &Env::default(), &mut Vec::new()).unwrap_err();
        assert!(format!("{err:#}").contains("invalid config"), "{err:#}");
        assert!(
            format!("{err:#}").contains("autoscaler resources"),
            "{err:#}"
        );
        let err = load_config(&path.with_file_name("missing.yaml"), &Env::default()).unwrap_err();
        assert!(format!("{err:#}").contains("read config file"));
    }

    #[test]
    fn crd_prints_the_definition() {
        for (kind, name) in [
            (
                CrdKind::InstanceVolumeAutoscaler,
                "InstanceVolumeAutoscaler",
            ),
            (
                CrdKind::PersistentVolumeClaimAutoscaler,
                "PersistentVolumeClaimAutoscaler",
            ),
        ] {
            let mut out = Vec::new();
            run_crd(kind, &mut out).unwrap();
            let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
            assert_eq!(v["kind"], "CustomResourceDefinition");
            assert_eq!(v["spec"]["names"]["kind"], name);
        }
    }

    fn resolver() -> Resolver {
        let mut muted = with_scale_up(
            with_utilization(iva("db", by_tags(&[("Role", "db")])), 70),
            Some(SelectPolicy::Min),
            &[
                (ScalingPolicyType::GiB, 50),
                (ScalingPolicyType::Percent, 20),
            ],
        );
        muted.spec.alert_enabled = false;
        Resolver::new(&[
            iva("bastion", by_id("i-0000000a")),
            muted,
            with_scale_up(
                iva("off", by_id("i-0000000e")),
                Some(SelectPolicy::Disabled),
                &[],
            ),
            iva("broken", by_id("web-1")),
        ])
    }

    fn inst(id: &str, name: &str, role: &str, size: i32) -> Instance {
        let mut tags = BTreeMap::from([("Name".to_string(), name.to_string())]);
        if !role.is_empty() {
            tags.insert("Role".into(), role.into());
        }
        Instance {
            id: id.into(),
            name: name.into(),
            tags,
            root_device_name: "/dev/xvda".into(),
            root_volume_id: format!("vol-{id}"),
            root_volume_size_gib: size,
        }
    }

    #[test]
    fn autoscalers_table() {
        let r = resolver();
        let mut out = Vec::new();
        render_autoscalers(&r, None, &mut out).unwrap();
        let text = String::from_utf8(out).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(
            lines,
            vec![
                "NAME     TARGET               UTILIZATION%  SCALE_UP        MAX_GIB  COOLDOWN  ALERT",
                "bastion  Instance/i-0000000a  80            max(10%,1GiB)   1000     6h        true",
                "db       TagSelector/Role=db  70            min(50GiB,20%)  1000     6h        false",
                "off      Instance/i-0000000e  80            disabled        1000     6h        true",
                "broken   (invalid: scaleTargetRef.name \"web-1\" is not an EC2 instance ID)",
            ]
        );
        let counts = HashMap::from([("db".to_string(), 2usize)]);
        let mut out = Vec::new();
        render_autoscalers(&r, Some(&counts), &mut out).unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(
            text.lines().next().unwrap().ends_with("ALERT  MATCHED"),
            "{text}"
        );
        assert!(text.contains("false  2"), "{text}");

        let mut out = Vec::new();
        render_autoscalers(&Resolver::default(), None, &mut out).unwrap();
        assert!(String::from_utf8(out).unwrap().ends_with("(none)\n"));
    }

    #[test]
    fn instances_table_and_counts() {
        let (_dir, path) = write_example();
        let cfg = load_config(&path, &Env::default()).unwrap();
        let r = resolver();
        let instances = vec![
            inst("i-0000000a", "bastion-01", "", 30),
            inst("i-0000000c", "db-2", "db", 500),
            inst("i-0000000b", "db-1", "db", 120),
            inst("i-0000000d", "", "", 8),
        ];
        assert_eq!(
            autoscaler_counts(&instances, &r),
            HashMap::from([
                ("bastion".to_string(), 1usize),
                ("db".to_string(), 2),
                ("off".to_string(), 0)
            ])
        );
        let mut out = Vec::new();
        render_instances(&cfg, &r, &instances, &mut out).unwrap();
        let text = String::from_utf8(out).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(
            lines,
            vec![
                "AUTOSCALER  INSTANCE_ID  NAME        ROOT_VOLUME     SIZE_GIB",
                "bastion     i-0000000a   bastion-01  vol-i-0000000a  30",
                "db          i-0000000b   db-1        vol-i-0000000b  120",
                "db          i-0000000c   db-2        vol-i-0000000c  500",
                "(none)      i-0000000d   -           vol-i-0000000d  8",
                "",
                "4 instances discovered in ap-northeast-2, 3 targeted",
            ]
        );

        let overlap = Resolver::new(&[
            iva("a", by_id("i-0000000a")),
            iva("b", by_tags(&[("Name", "bastion-01")])),
        ]);
        let mut out = Vec::new();
        render_instances(&cfg, &overlap, &instances[..1], &mut out).unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("(ambiguous: a,b)  i-0000000a"), "{text}");
        assert!(text.ends_with("1 instance discovered in ap-northeast-2, 0 targeted\n"));
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
