//! The operational subcommands: validate, policies, instances, unused. They
//! need no running controller and print tables to stdout.

use std::collections::HashMap;
use std::io::Write;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context as _, Result};

use crate::awsx::{Clients, Instance, TagFilter};
use crate::config::{self, Config, Env, GROW_MODE_ABSOLUTE};
use crate::humanize::{go_duration, human_bytes, round_duration};
use crate::policy::{DEFAULT_POLICY_NAME, Effective, Resolver};
use crate::pvscan::{self, DiscardRecorder, Finding, KIND_PVC, Scanner};

/// Bounds every CLI call that contacts AWS or the Kubernetes API.
const CLI_TIMEOUT: Duration = Duration::from_mins(1);

/// Loads the config at `path` and builds the policy resolver, wrapping either
/// failure so the command exits non-zero with a clear message.
pub fn load_resolver(path: &Path, env: &Env) -> Result<(Config, Resolver)> {
    let cfg =
        config::load(path, env).with_context(|| format!("invalid config {}", path.display()))?;
    let resolver =
        Resolver::new(&cfg).with_context(|| format!("invalid config {}", path.display()))?;
    Ok((cfg, resolver))
}

/// Loads and validates the config file (including every policy) and reports
/// the outcome. It never contacts AWS.
pub fn run_validate(path: &Path, env: &Env, out: &mut impl Write) -> Result<()> {
    let (cfg, _) = load_resolver(path, env)?;
    writeln!(
        out,
        "config {} is valid: region={}, {} named resize {} plus the default",
        path.display(),
        cfg.region,
        cfg.policies.len(),
        pluralize(cfg.policies.len(), "policy", "policies")
    )?;
    Ok(())
}

/// Prints every resize policy and its effective settings, highest weight
/// first, then the default policy. When `with_count` is set it discovers
/// target instances via AWS and adds a MATCHED column.
pub async fn run_policies(
    path: &Path,
    env: &Env,
    with_count: bool,
    out: &mut impl Write,
) -> Result<()> {
    let (cfg, resolver) = load_resolver(path, env)?;
    let counts = if with_count {
        discover_policy_counts(&cfg, &resolver).await?
    } else {
        HashMap::new()
    };
    let mut names = resolver.names();
    names.sort_by_key(|n| std::cmp::Reverse(weight_of(&cfg, n)));

    let mut table = Table::new();
    let mut header = vec![
        "POLICY",
        "WEIGHT",
        "SELECTOR",
        "PAUSED",
        "ALERT",
        "THRESHOLD%",
        "GROW",
        "MAX_GIB",
    ];
    if with_count {
        header.push("MATCHED");
    }
    table.row(header.iter().map(ToString::to_string).collect());
    let mut row = |name: &str, weight: String, selector: String, eff: &Effective| {
        let mut cells = vec![
            name.to_string(),
            weight,
            selector,
            eff.paused.to_string(),
            eff.alert_enabled.to_string(),
            eff.usage_threshold_percent.to_string(),
            grow_summary(eff),
            eff.max_volume_size_gib.to_string(),
        ];
        if with_count {
            cells.push(counts.get(name).copied().unwrap_or(0).to_string());
        }
        table.row(cells);
    };
    for name in &names {
        let eff = resolver
            .effective_of(name)
            .unwrap_or_else(|| resolver.default());
        row(
            name,
            weight_of(&cfg, name).to_string(),
            selector_of(&cfg, name),
            &eff,
        );
    }
    row(
        DEFAULT_POLICY_NAME,
        "-".into(),
        "(instances matching no policy)".into(),
        &resolver.default(),
    );
    table.render(out)?;
    Ok(())
}

/// Initializes AWS clients and discovers the target instances for `cfg`. It
/// is the shared discovery step behind every CLI subcommand that contacts
/// AWS.
async fn discover_instances(cfg: &Config) -> Result<Vec<Instance>> {
    let clients = tokio::time::timeout(CLI_TIMEOUT, Clients::new(&cfg.region))
        .await
        .context("initialize AWS clients: timed out")?;
    let filters: Vec<TagFilter> = cfg
        .tag_filters
        .iter()
        .map(|f| TagFilter {
            key: f.key.clone(),
            value: f.value.clone(),
        })
        .collect();
    tokio::time::timeout(
        CLI_TIMEOUT,
        clients.describe_target_instances(&filters, cfg.exclude_eks_nodes),
    )
    .await
    .context("discover instances: timed out")?
    .context("discover instances")
}

/// Discovers target instances and tallies how many each policy matches,
/// seeding every policy (and default) to 0.
async fn discover_policy_counts(
    cfg: &Config,
    resolver: &Resolver,
) -> Result<HashMap<String, usize>> {
    let instances = discover_instances(cfg).await?;
    Ok(policy_counts(&instances, resolver))
}

fn policy_counts(instances: &[Instance], resolver: &Resolver) -> HashMap<String, usize> {
    let mut counts: HashMap<String, usize> = HashMap::from([(DEFAULT_POLICY_NAME.to_string(), 0)]);
    for name in resolver.names() {
        counts.insert(name, 0);
    }
    for inst in instances {
        *counts
            .entry(resolver.resolve(&inst.name, &inst.tags).policy)
            .or_default() += 1;
    }
    counts
}

/// Discovers target instances via AWS and lists them grouped by the policy
/// each one matches.
pub async fn run_instances(path: &Path, env: &Env, out: &mut impl Write) -> Result<()> {
    let (cfg, resolver) = load_resolver(path, env)?;
    let instances = discover_instances(&cfg).await?;
    render_instances(&cfg, &resolver, &instances, out)
}

fn render_instances(
    cfg: &Config,
    resolver: &Resolver,
    instances: &[Instance],
    out: &mut impl Write,
) -> Result<()> {
    let mut by_policy: HashMap<String, Vec<&Instance>> = HashMap::new();
    for inst in instances {
        by_policy
            .entry(resolver.resolve(&inst.name, &inst.tags).policy)
            .or_default()
            .push(inst);
    }
    let mut table = Table::new();
    table.row(
        ["POLICY", "INSTANCE_ID", "NAME", "ROOT_VOLUME", "SIZE_GIB"]
            .iter()
            .map(ToString::to_string)
            .collect(),
    );
    let mut names = resolver.names();
    names.push(DEFAULT_POLICY_NAME.to_string());
    for name in names {
        match by_policy.get(&name) {
            None => table.row(vec![
                name.clone(),
                "(none)".into(),
                String::new(),
                String::new(),
                String::new(),
            ]),
            Some(list) => {
                for inst in list {
                    table.row(vec![
                        name.clone(),
                        inst.id.clone(),
                        inst.name.clone(),
                        inst.root_volume_id.clone(),
                        inst.root_volume_size_gib.to_string(),
                    ]);
                }
            }
        }
    }
    table.render(out)?;
    writeln!(
        out,
        "\n{} {} discovered in {}",
        instances.len(),
        pluralize(instances.len(), "instance", "instances"),
        cfg.region
    )?;
    Ok(())
}

/// Lists the `PersistentVolumeClaims` and `PersistentVolumes` no workload is
/// using, grouped by kind and sorted longest-unused first. It reads the
/// Kubernetes API and never writes.
pub async fn run_unused(path: &Path, env: &Env, all: bool, out: &mut impl Write) -> Result<()> {
    let (cfg, _) = load_resolver(path, env)?;
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

/// Renders the growth setting compactly for the policy table.
fn grow_summary(eff: &Effective) -> String {
    if eff.grow_mode == GROW_MODE_ABSOLUTE {
        format!("absolute +{}GiB", eff.grow_amount_gib)
    } else {
        format!("percent +{}%", eff.grow_percent)
    }
}

/// The configured weight of a named policy (0 if not found).
fn weight_of(cfg: &Config, name: &str) -> i32 {
    cfg.policies
        .iter()
        .find(|p| p.name == name)
        .map_or(0, |p| p.weight)
}

/// Renders a policy's `instanceSelector` compactly for the table.
fn selector_of(cfg: &Config, name: &str) -> String {
    let Some(p) = cfg.policies.iter().find(|p| p.name == name) else {
        return String::new();
    };
    let mut parts: Vec<String> = p
        .instance_selector
        .tags
        .iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect();
    let mut out = parts.join(",");
    if !p.instance_selector.name_regex.is_empty() {
        if !out.is_empty() {
            out.push_str(" & ");
        }
        out.push_str("name~");
        out.push_str(&p.instance_selector.name_regex);
    }
    parts.clear();
    out
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
            text.ends_with(
                "is valid: region=ap-northeast-2, 2 named resize policies plus the default\n"
            ),
            "{text}"
        );

        let bad = path.with_file_name("bad.yaml");
        std::fs::write(&bad, "region: r\ndefaultPolicy:\n  usageThresholdPercent: 80\n  growMode: percent\npolicies:\n  - name: x\n").unwrap();
        let err = run_validate(&bad, &Env::default(), &mut Vec::new()).unwrap_err();
        assert!(format!("{err:#}").contains("invalid config"), "{err:#}");
        assert!(
            format!("{err:#}").contains("instanceSelector requires"),
            "{err:#}"
        );
        let err = load_resolver(&path.with_file_name("missing.yaml"), &Env::default()).unwrap_err();
        assert!(format!("{err:#}").contains("read config file"));
    }

    #[tokio::test]
    async fn policies_table() {
        let (_dir, path) = write_example();
        let mut out = Vec::new();
        run_policies(&path, &Env::default(), false, &mut out)
            .await
            .unwrap();
        let text = String::from_utf8(out).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(
            lines[0],
            "POLICY   WEIGHT  SELECTOR                        PAUSED  ALERT  THRESHOLD%  GROW             MAX_GIB"
        );
        assert_eq!(
            lines[1],
            "bastion  5       name~bastion                    true    false  60          percent +10%     1000"
        );
        assert_eq!(
            lines[2],
            "shared   1       name~^shared-                   false   true   80          absolute +50GiB  1000"
        );
        assert_eq!(
            lines[3],
            "default  -       (instances matching no policy)  false   true   80          percent +10%     1000"
        );
    }

    #[test]
    fn instances_table_and_counts() {
        let (_dir, path) = write_example();
        let (cfg, resolver) = load_resolver(&path, &Env::default()).unwrap();
        let inst = |id: &str, name: &str, size: i32| Instance {
            id: id.into(),
            name: name.into(),
            tags: BTreeMap::from([("Name".to_string(), name.to_string())]),
            root_device_name: "/dev/xvda".into(),
            root_volume_id: format!("vol-{id}"),
            root_volume_size_gib: size,
        };
        let instances = vec![
            inst("i-1", "bastion-01", 30),
            inst("i-2", "shared-web", 120),
            inst("i-3", "shared-db", 500),
        ];
        let counts = policy_counts(&instances, &resolver);
        assert_eq!(
            counts,
            HashMap::from([
                ("bastion".to_string(), 1usize),
                ("shared".to_string(), 2),
                ("default".to_string(), 0)
            ])
        );
        let mut out = Vec::new();
        render_instances(&cfg, &resolver, &instances, &mut out).unwrap();
        let text = String::from_utf8(out).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(
            lines[0],
            "POLICY   INSTANCE_ID  NAME        ROOT_VOLUME  SIZE_GIB"
        );
        assert_eq!(
            lines[1],
            "bastion  i-1          bastion-01  vol-i-1      30"
        );
        assert_eq!(
            lines[2],
            "shared   i-2          shared-web  vol-i-2      120"
        );
        assert_eq!(
            lines[3],
            "shared   i-3          shared-db   vol-i-3      500"
        );
        assert_eq!(lines[4], "default  (none)");
        assert_eq!(lines[5], "");
        assert_eq!(lines[6], "3 instances discovered in ap-northeast-2");
        let mut out = Vec::new();
        render_instances(&cfg, &resolver, &instances[..1], &mut out).unwrap();
        assert!(
            String::from_utf8(out)
                .unwrap()
                .ends_with("1 instance discovered in ap-northeast-2\n")
        );
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
        let (_dir, path) = write_example();
        let (cfg, _) = load_resolver(&path, &Env::default()).unwrap();
        assert_eq!(weight_of(&cfg, "bastion"), 5);
        assert_eq!(weight_of(&cfg, "missing"), 0);
        assert_eq!(selector_of(&cfg, "shared"), "name~^shared-");
        assert_eq!(selector_of(&cfg, "missing"), "");
        let mut cfg2 = cfg;
        cfg2.policies[0].instance_selector.tags = BTreeMap::from([
            ("Role".to_string(), "db".to_string()),
            ("Env".to_string(), "prod".to_string()),
        ]);
        assert_eq!(
            selector_of(&cfg2, "bastion"),
            "Env=prod,Role=db & name~bastion"
        );
        cfg2.policies[0].instance_selector.name_regex = String::new();
        assert_eq!(selector_of(&cfg2, "bastion"), "Env=prod,Role=db");
        assert_eq!(pluralize(1, "a", "b"), "a");
        assert_eq!(pluralize(2, "a", "b"), "b");
        assert_eq!(or_dash(""), "-");
        assert_eq!(or_dash("x"), "x");
    }
}
