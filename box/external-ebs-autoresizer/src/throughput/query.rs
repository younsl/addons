//! The `PromQL` expressions the recommender evaluates. The same expressions
//! run unchanged against Prometheus and Mimir: both are instant queries over
//! a subquery, using only functions in the core `PromQL` language.
//!
//! Node exporter is the metric source rather than `CloudWatch` because
//! throughput sizing needs the peak, not the mean. `CloudWatch` publishes EBS
//! volume metrics at 1-minute granularity at best, so a burst that saturates
//! a 125 MiB/s baseline for ten seconds averages out to roughly 30 MiB/s and
//! is invisible. A 15-30s scrape keeps it.

/// The divisor that converts the byte-rate the node exporter counters
/// produce into the MiB/s unit gp3 throughput is provisioned in.
const BYTES_PER_MIB_QUERY: u64 = 1 << 20;

/// How many node names go into one query's node matcher. It bounds the
/// expression size on a large cluster; the total work is the same either way.
pub const NODE_BATCH: usize = 200;

/// Builds the two `PromQL` expressions the recommender evaluates.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Query {
    /// The label on the node exporter series that carries the Kubernetes
    /// Node name.
    pub node_label: String,
    /// Selects which block devices count toward a node's throughput.
    pub device_regex: String,
    /// The range passed to `rate()`.
    pub rate_window: String,
    /// How far back the observation window reaches.
    pub lookback: String,
    /// The subquery resolution.
    pub step: String,
    /// The quantile of the per-step throughput taken as the peak.
    pub quantile: f64,
}

impl Query {
    /// The query for the peak throughput in MiB/s of each named node over the
    /// observation window.
    #[must_use]
    pub fn peak(&self, node_names: &[String]) -> String {
        format!(
            "quantile_over_time({}, ({})[{}:{}]) / {BYTES_PER_MIB_QUERY}",
            format_quantile(self.quantile),
            self.byte_rate(node_names),
            self.lookback,
            self.step
        )
    }

    /// The query for how many data points back each node's peak. It is the
    /// confidence signal: a node created an hour ago cannot support a
    /// recommendation drawn from a seven-day window.
    #[must_use]
    pub fn sample_count(&self, node_names: &[String]) -> String {
        format!(
            "count_over_time(({})[{}:{}])",
            self.byte_rate(node_names),
            self.lookback,
            self.step
        )
    }

    /// A cheap instant query counting how many node exporter disk series the
    /// backend holds at all, ignoring which node they belong to. It tells
    /// "this backend has no node exporter data" apart from "it has data but
    /// none of it is labelled with these node names".
    #[must_use]
    pub fn presence(&self) -> String {
        format!(
            "count(node_disk_read_bytes_total{{device=~{}}})",
            quote(&self.device_regex)
        )
    }

    /// The per-node read+write byte rate expression both observation queries
    /// wrap. The inner sum totals a node's devices within one scrape target;
    /// the outer max then collapses whatever targets remain for that node
    /// name instead of adding them, which keeps a shared metrics backend
    /// honest without a tenancy matcher and stops a restarted node exporter
    /// Pod from double-counting its node.
    fn byte_rate(&self, node_names: &[String]) -> String {
        let selector = self.selector(node_names);
        let mut inner = self.node_label.clone();
        // Grouping by the target as well as the node is what separates two
        // sources for the same node name. When the node name is already
        // carried by the target label there is nothing to separate.
        if self.node_label != "instance" {
            inner.push_str(", instance");
        }
        format!(
            "max by ({}) (sum by ({inner}) (rate(node_disk_read_bytes_total{{{selector}}}[{}]) + rate(node_disk_written_bytes_total{{{selector}}}[{}])))",
            self.node_label, self.rate_window, self.rate_window
        )
    }

    /// The label matchers applied to both counters: the device matcher and,
    /// when node names are given, an exact-alternation matcher on the node
    /// label. Scoping by name is what replaces a configured tenancy matcher.
    fn selector(&self, node_names: &[String]) -> String {
        let device = format!("device=~{}", quote(&self.device_regex));
        if node_names.is_empty() {
            return device;
        }
        format!(
            "{}=~{},{device}",
            self.node_label,
            quote(&node_alternation(node_names))
        )
    }
}

/// Builds an anchored alternation of the node names. Every name is escaped:
/// a Kubernetes node name is a DNS name, and its dots would otherwise be
/// regex wildcards that match unrelated series.
fn node_alternation(node_names: &[String]) -> String {
    node_names
        .iter()
        .map(|n| quote_meta(n))
        .collect::<Vec<_>>()
        .join("|")
}

/// Escapes regex metacharacters the way Go's `regexp.QuoteMeta` does, so the
/// generated query is byte-for-byte what the Go version sent. A hyphen is not
/// special outside a character class and is left alone.
fn quote_meta(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if "\\.+*?()|[]{}^$".contains(c) {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// Renders a quantile the way `strconv.FormatFloat(q, 'f', -1, 64)` does:
/// the shortest decimal form, never exponential notation.
fn format_quantile(q: f64) -> String {
    let s = format!("{q}");
    if s.contains('e') {
        format!("{q:.6}")
    } else {
        s
    }
}

/// Renders a Go-style double-quoted string literal.
fn quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\t' => out.push_str("\\t"),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn query(label: &str) -> Query {
        Query {
            node_label: label.into(),
            device_regex: "nvme[0-9]+n[0-9]+|xvd[a-z]+|sd[a-z]+".into(),
            rate_window: "1m".into(),
            lookback: "7d".into(),
            step: "1m".into(),
            quantile: 0.99,
        }
    }

    #[test]
    fn peak_and_sample_count() {
        let q = query("node");
        let names = vec!["ip-10-0-1-5.ec2.internal".to_string(), "b".to_string()];
        let peak = q.peak(&names);
        assert_eq!(
            peak,
            // The alternation is a PromQL string literal, so the regex escape
            // is itself escaped, exactly as strconv.Quote rendered it before.
            "quantile_over_time(0.99, (max by (node) (sum by (node, instance) (rate(node_disk_read_bytes_total{node=~\"ip-10-0-1-5\\\\.ec2\\\\.internal|b\",device=~\"nvme[0-9]+n[0-9]+|xvd[a-z]+|sd[a-z]+\"}[1m]) + rate(node_disk_written_bytes_total{node=~\"ip-10-0-1-5\\\\.ec2\\\\.internal|b\",device=~\"nvme[0-9]+n[0-9]+|xvd[a-z]+|sd[a-z]+\"}[1m]))))[7d:1m]) / 1048576"
        );
        let count = q.sample_count(&names);
        assert!(
            count.starts_with("count_over_time((max by (node)"),
            "{count}"
        );
        assert!(count.ends_with("[7d:1m])"), "{count}");
        assert!(!count.contains("quantile"));
    }

    #[test]
    fn instance_label_groups_by_node_alone() {
        let q = query("instance");
        let peak = q.peak(&["a".into()]);
        assert!(
            peak.contains("max by (instance) (sum by (instance) ("),
            "{peak}"
        );
        assert!(!peak.contains("instance, instance"));
    }

    #[test]
    fn unscoped_selector_and_presence() {
        let q = query("node");
        assert_eq!(
            q.selector(&[]),
            "device=~\"nvme[0-9]+n[0-9]+|xvd[a-z]+|sd[a-z]+\""
        );
        assert_eq!(
            q.presence(),
            "count(node_disk_read_bytes_total{device=~\"nvme[0-9]+n[0-9]+|xvd[a-z]+|sd[a-z]+\"})"
        );
    }

    #[test]
    fn device_regex_cannot_break_out_of_the_literal() {
        let mut q = query("node");
        q.device_regex = "x\"} or vector(1) #".into();
        let sel = q.selector(&[]);
        assert_eq!(sel, "device=~\"x\\\"} or vector(1) #\"");
    }

    #[test]
    fn alternation_escapes_and_quantile_format() {
        assert_eq!(
            node_alternation(&["a.b".into(), "c+d".into(), "x-y".into()]),
            "a\\.b|c\\+d|x-y"
        );
        assert_eq!(quote_meta("a\\b$"), "a\\\\b\\$");
        assert_eq!(format_quantile(0.99), "0.99");
        assert_eq!(format_quantile(1.0), "1");
        assert_eq!(format_quantile(0.000_001), "0.000001");
        assert_eq!(quote("a\\b\n\t"), "\"a\\\\b\\n\\t\"");
    }
}
