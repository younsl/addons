//! The `validate` and `status` subcommands. Both print tables to stdout for a
//! human running `kubectl exec`, and are not part of the Pod's log stream.

use std::fmt::Write as _;
use std::io::Write;

use anyhow::{Context as _, Result};
use chrono::Utc;

use crate::aws::{Client, DiscoverInput, TagFilter};
use crate::config::Config;
use crate::controller::{routing_mode, tag_filter_summary, up_down};
use crate::humanize;
use crate::window::{Window, WindowConfig};

/// Compiles the configured window.
pub fn window_from(cfg: &Config) -> Result<Window> {
    Window::new(&WindowConfig {
        timezone: cfg.maintenance_window.timezone.clone(),
        cron_schedule: cfg.maintenance_window.cron_schedule.clone(),
        duration: cfg.maintenance_window.duration.get(),
        min_remaining: cfg.maintenance_window.min_remaining.get(),
    })
    .context("invalid maintenance window")
}

/// Converts the configured opt-in tags to the AWS filter form.
#[must_use]
pub fn discover_input(cfg: &Config) -> DiscoverInput {
    DiscoverInput {
        tag_filters: cfg
            .targets
            .tag_filters
            .iter()
            .map(|f| TagFilter {
                key: f.key.clone(),
                value: f.value.clone(),
            })
            .collect(),
        exclude_ids: cfg.targets.exclude_connection_ids.clone(),
    }
}

/// Checks the config without touching AWS or Kubernetes and prints the
/// effective settings.
pub fn validate(cfg: &Config, out: &mut (dyn Write + Send)) -> Result<()> {
    let win = window_from(cfg)?;
    let now = Utc::now();
    let (open, detail) = win.open(now);
    let tz = win.timezone();
    let rows: Vec<(&str, String)> = vec![
        ("region", cfg.region.clone()),
        ("dry run", cfg.dry_run.to_string()),
        ("reconcile interval", cfg.reconcile_interval.to_string()),
        ("tag filters", tag_filter_summary(cfg)),
        ("maintenance window", win.to_string()),
        (
            "window open now",
            format!("{open}  {detail}").trim_end().to_string(),
        ),
        (
            "window next opens",
            win.next_open(now).map_or_else(
                || "never".to_string(),
                |n| humanize::clock(&n.with_timezone(&tz)),
            ),
        ),
        (
            "approvers",
            format!("{} Slack user(s)", cfg.approval.slack_user_ids.len()),
        ),
        ("approval timeout", cfg.approval.timeout.to_string()),
        (
            "peer min stable for",
            cfg.safety.peer_min_stable_for.to_string(),
        ),
        (
            "peer min accepted routes",
            cfg.safety.peer_min_accepted_routes.to_string(),
        ),
        (
            "per-connection cooldown",
            cfg.safety.per_connection_cooldown.to_string(),
        ),
        (
            "verify timeout",
            format!(
                "{} (poll {})",
                cfg.safety.verify_timeout, cfg.safety.verify_poll_interval
            ),
        ),
        ("escalate before", cfg.safety.escalate_before.to_string()),
        ("traffic gate", describe_traffic_gate(cfg)),
        (
            "state configmap",
            format!("{}/{}", cfg.pod_namespace, cfg.state_config_map_name),
        ),
    ];
    write_table(
        out,
        &rows
            .iter()
            .map(|(k, v)| vec![(*k).to_string(), v.clone()])
            .collect::<Vec<_>>(),
    )
}

/// Summarizes the gate in one line, including what an unreadable metric
/// source would mean.
fn describe_traffic_gate(cfg: &Config) -> String {
    let t = &cfg.traffic_gate;
    if !t.enabled {
        return "disabled (window and peer checks only)".to_string();
    }
    format!(
        "{}, quiet at or below P{:.0} of this window's own traffic, onError {}",
        t.endpoint, t.quiet_percentile, t.on_error
    )
}

/// Prints telemetry and pending maintenance. Read-only, so it answers "what
/// would this controller act on" without waiting for a window.
pub async fn status(cfg: &Config, client: &Client, out: &mut (dyn Write + Send)) -> Result<()> {
    let conns = client.discover(&discover_input(cfg)).await?;
    if conns.is_empty() {
        writeln!(out, "no VPN connections match the configured tag filters")?;
        return Ok(());
    }

    let mut rows = vec![
        [
            "CONNECTION",
            "NAME",
            "ROUTING",
            "TUNNEL",
            "STATUS",
            "ROUTES",
            "STABLE FOR",
            "LIFECYCLE",
            "PENDING",
            "AUTO-APPLY AFTER",
        ]
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>(),
    ];
    let now = Utc::now();
    let mut unmanaged = 0;
    for conn in &conns {
        for s in client.statuses(conn).await? {
            if !s.tunnel.lifecycle_control {
                unmanaged += 1;
            }
            rows.push(vec![
                conn.id.clone(),
                or_dash(&conn.name),
                routing_mode(conn).to_string(),
                s.tunnel.outside_ip.clone(),
                up_down(s.tunnel.up).to_string(),
                s.tunnel.accepted_routes.to_string(),
                truncate_duration(s.tunnel.stable_for(now)),
                if s.tunnel.lifecycle_control {
                    "on"
                } else {
                    "OFF"
                }
                .to_string(),
                if s.maintenance.pending { "yes" } else { "no" }.to_string(),
                s.maintenance.auto_applied_after.map_or_else(
                    || "-".to_string(),
                    |t| humanize::clock(&t.with_timezone(&chrono::Local)),
                ),
            ]);
        }
    }
    write_table(out, &rows)?;
    // Without lifecycle control a tunnel can never be taken over, so this is
    // called out rather than left to be inferred from the column.
    if unmanaged > 0 {
        writeln!(
            out,
            "\n{unmanaged} tunnel(s) have lifecycle control disabled and cannot be replaced early.\nEnable it per tunnel with: aws ec2 modify-vpn-tunnel-options --enable-tunnel-lifecycle-control"
        )?;
    }
    Ok(())
}

fn or_dash(s: &str) -> String {
    if s.is_empty() {
        "-".to_string()
    } else {
        s.to_string()
    }
}

fn truncate_duration(d: std::time::Duration) -> String {
    if d.is_zero() {
        "-".to_string()
    } else {
        humanize::go_duration(humanize::round_to_minute(d))
    }
}

/// Writes rows as columns padded to the widest cell, two spaces apart.
fn write_table(out: &mut (dyn Write + Send), rows: &[Vec<String>]) -> Result<()> {
    let cols = rows.iter().map(Vec::len).max().unwrap_or(0);
    let widths: Vec<usize> = (0..cols)
        .map(|c| {
            rows.iter()
                .map(|r| r.get(c).map_or(0, |s| s.chars().count()))
                .max()
                .unwrap_or(0)
        })
        .collect();
    for row in rows {
        let mut line = String::new();
        for (c, cell) in row.iter().enumerate() {
            if c + 1 == row.len() {
                line.push_str(cell);
            } else {
                let _ = write!(line, "{cell:<width$}  ", width = widths[c]);
            }
        }
        writeln!(out, "{}", line.trim_end())?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::aws::client::fake::*;
    use crate::config::{MINIMAL_YAML, parse, test_env};

    #[test]
    fn validate_prints_effective_settings() {
        let cfg = parse(MINIMAL_YAML, &test_env()).unwrap();
        let mut out = Vec::new();
        validate(&cfg, &mut out).unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(
            text.contains("region                    ap-northeast-2"),
            "{text}"
        );
        assert!(text.contains("dry run                   true"), "{text}");
        assert!(
            text.contains("tag filters               managed=true"),
            "{text}"
        );
        assert!(
            text.contains(
                "maintenance window        \"0 2 * * *\" for 3h0m0s (UTC), min remaining 30m0s"
            ),
            "{text}"
        );
        assert!(
            text.contains("verify timeout            30m0s (poll 10s)"),
            "{text}"
        );
        assert!(
            text.contains("traffic gate              disabled (window and peer checks only)"),
            "{text}"
        );
        assert!(
            text.contains(
                "state configmap           kube-system/aws-vpn-maintenance-handler-state"
            ),
            "{text}"
        );
        assert!(text.contains("window next opens"), "{text}");

        let gated = format!(
            "{MINIMAL_YAML}\ntrafficGate:\n  enabled: true\n  endpoint: https://mimir.example.com/prometheus\n  quietPercentile: 25\n  onError: allow\n"
        );
        let cfg = parse(&gated, &test_env()).unwrap();
        assert_eq!(
            describe_traffic_gate(&cfg),
            "https://mimir.example.com/prometheus, quiet at or below P25 of this window's own traffic, onError allow"
        );
    }

    #[test]
    fn validate_rejects_a_bad_window() {
        let mut cfg = parse(MINIMAL_YAML, &test_env()).unwrap();
        cfg.maintenance_window.timezone = "Nowhere".into();
        assert!(validate(&cfg, &mut Vec::new()).is_err());
    }

    #[tokio::test]
    async fn status_prints_tunnels_and_unmanaged_hint() {
        let cfg = parse(MINIMAL_YAML, &test_env()).unwrap();
        let ec2 = Arc::new(FakeEc2::default());
        *ec2.connections.lock().unwrap() = vec![vpn_connection("vpn-1", "prod", [true, false])];
        *ec2.statuses.lock().unwrap() = vec![(
            "vpn-1".into(),
            "1.1.1.1".into(),
            Some(pending(1_900_000_000)),
        )];
        let client = Client::from_parts(Box::new(ec2.clone()), None, None);
        let mut out = Vec::new();
        status(&cfg, &client, &mut out).await.unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(
            text.starts_with("CONNECTION  NAME  ROUTING  TUNNEL   STATUS  ROUTES  STABLE FOR"),
            "{text}"
        );
        assert!(
            text.contains("vpn-1       prod  bgp      1.1.1.1  UP      3"),
            "{text}"
        );
        assert!(text.contains("  on         yes      "), "{text}");
        assert!(text.contains("  OFF        no       -"), "{text}");
        assert!(
            text.contains("1 tunnel(s) have lifecycle control disabled"),
            "{text}"
        );

        *ec2.connections.lock().unwrap() = Vec::new();
        let mut out = Vec::new();
        status(&cfg, &client, &mut out).await.unwrap();
        assert_eq!(
            String::from_utf8(out).unwrap(),
            "no VPN connections match the configured tag filters\n"
        );

        *ec2.describe_error.lock().unwrap() = Some("denied".into());
        assert!(status(&cfg, &client, &mut Vec::new()).await.is_err());
    }

    #[test]
    fn table_and_helpers() {
        let mut out = Vec::new();
        write_table(
            &mut out,
            &[
                vec!["a".into(), "bb".into()],
                vec!["ccc".into(), "d".into()],
            ],
        )
        .unwrap();
        assert_eq!(String::from_utf8(out).unwrap(), "a    bb\nccc  d\n");
        assert_eq!(or_dash(""), "-");
        assert_eq!(truncate_duration(std::time::Duration::ZERO), "-");
        assert_eq!(
            truncate_duration(std::time::Duration::from_secs(3661)),
            "1h1m0s"
        );
    }
}
