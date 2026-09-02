//! Tool input types for the embedded MCP server.
//!
//! Every list-style tool takes `limit` and `offset`. Limits are clamped on the
//! server so a single tool call can never pull an unbounded slice of the
//! database into an LLM context window.

use rmcp::schemars::{self, JsonSchema};
use serde::Deserialize;

/// Default page size when a tool call omits `limit`.
pub const DEFAULT_LIMIT: i64 = 20;
/// Upper bound for report and search listings.
pub const MAX_LIST_LIMIT: i64 = 100;
/// Upper bound for per-report item listings (vulnerabilities, components).
pub const MAX_ITEM_LIMIT: i64 = 200;

/// Clamp a caller-supplied limit into `1..=max`, falling back to
/// [`DEFAULT_LIMIT`] when absent or non-positive.
pub fn clamp_limit(limit: Option<i64>, max: i64) -> i64 {
    match limit {
        Some(l) if l > 0 => l.min(max),
        _ => DEFAULT_LIMIT.min(max),
    }
}

/// Normalise an offset: negative or missing becomes 0.
pub fn clamp_offset(offset: Option<i64>) -> i64 {
    offset.unwrap_or(0).max(0)
}

/// Normalise severity names to the upper-case form stored in Trivy reports.
/// Unknown values are dropped so a typo cannot silently widen a filter.
pub fn normalize_severities(input: Option<Vec<String>>) -> Option<Vec<String>> {
    let out: Vec<String> = input?
        .into_iter()
        .map(|s| s.trim().to_ascii_uppercase())
        .filter(|s| {
            matches!(
                s.as_str(),
                "CRITICAL" | "HIGH" | "MEDIUM" | "LOW" | "UNKNOWN"
            )
        })
        .collect();
    if out.is_empty() { None } else { Some(out) }
}

#[derive(Debug, Default, Deserialize, JsonSchema)]
pub struct ListNamespacesParams {
    /// Restrict to one cluster. Omit for all clusters.
    pub cluster: Option<String>,
}

#[derive(Debug, Default, Deserialize, JsonSchema)]
pub struct ListReportsParams {
    /// Filter by cluster name (exact match).
    pub cluster: Option<String>,
    /// Filter by Kubernetes namespace (exact match).
    pub namespace: Option<String>,
    /// Filter by application name (substring match).
    pub app: Option<String>,
    /// Filter by container image (substring match).
    pub image: Option<String>,
    /// Vulnerability reports only: keep reports that have at least one finding
    /// at any of these severities (CRITICAL, HIGH, MEDIUM, LOW).
    pub severity: Option<Vec<String>>,
    /// SBOM reports only: keep reports containing a component whose name
    /// matches this substring.
    pub component: Option<String>,
    /// Page size, 1-100. Default 20.
    pub limit: Option<i64>,
    /// Number of rows to skip. Default 0.
    pub offset: Option<i64>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct GetVulnerabilityReportParams {
    /// Cluster name as shown by list_clusters.
    pub cluster: String,
    /// Kubernetes namespace of the report.
    pub namespace: String,
    /// VulnerabilityReport resource name.
    pub name: String,
    /// Return only findings at these severities (CRITICAL, HIGH, MEDIUM, LOW,
    /// UNKNOWN). Omit for all.
    pub severity: Option<Vec<String>>,
    /// Return only findings that have a fixed version available.
    pub fixed_only: Option<bool>,
    /// Page size for the vulnerability list, 1-200. Default 20.
    pub limit: Option<i64>,
    /// Number of vulnerabilities to skip. Default 0.
    pub offset: Option<i64>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct GetSbomReportParams {
    /// Cluster name as shown by list_clusters.
    pub cluster: String,
    /// Kubernetes namespace of the report.
    pub namespace: String,
    /// SbomReport resource name.
    pub name: String,
    /// Keep only components whose name contains this substring.
    pub component: Option<String>,
    /// Page size for the component list, 1-200. Default 20.
    pub limit: Option<i64>,
    /// Number of components to skip. Default 0.
    pub offset: Option<i64>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct SearchVulnerabilitiesParams {
    /// CVE identifier or package name, substring match (e.g. "CVE-2024-3094",
    /// "openssl").
    pub query: String,
    /// Page size, 1-100. Default 20.
    pub limit: Option<i64>,
    /// Number of rows to skip. Default 0.
    pub offset: Option<i64>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct SearchSbomComponentsParams {
    /// Component (package) name, substring match (e.g. "log4j-core").
    pub component: String,
    /// Keep only matches at exactly this version.
    pub version: Option<String>,
    /// Page size, 1-100. Default 20.
    pub limit: Option<i64>,
    /// Number of rows to skip. Default 0.
    pub offset: Option<i64>,
}

#[derive(Debug, Default, Deserialize, JsonSchema)]
pub struct ListAlertRulesParams {
    /// Keep only rules whose matched package name contains this substring
    /// (e.g. "log4j").
    pub package: Option<String>,
    /// Keep only rules that are enabled. Omit for all rules.
    pub enabled_only: Option<bool>,
    /// Keep only rules the evaluator refuses to act on, i.e. whose Ready
    /// condition is False. Use this to find rules that look active but never
    /// fire.
    pub not_ready_only: Option<bool>,
    /// Page size, 1-100. Default 20.
    pub limit: Option<i64>,
    /// Number of rules to skip. Default 0.
    pub offset: Option<i64>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct GetAlertRuleParams {
    /// Rule name as shown by list_alert_rules.
    pub name: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clamp_limit_defaults_and_bounds() {
        assert_eq!(clamp_limit(None, MAX_LIST_LIMIT), DEFAULT_LIMIT);
        assert_eq!(clamp_limit(Some(0), MAX_LIST_LIMIT), DEFAULT_LIMIT);
        assert_eq!(clamp_limit(Some(-5), MAX_LIST_LIMIT), DEFAULT_LIMIT);
        assert_eq!(clamp_limit(Some(7), MAX_LIST_LIMIT), 7);
        assert_eq!(clamp_limit(Some(10_000), MAX_LIST_LIMIT), MAX_LIST_LIMIT);
        assert_eq!(clamp_limit(Some(10_000), MAX_ITEM_LIMIT), MAX_ITEM_LIMIT);
    }

    #[test]
    fn clamp_offset_floors_at_zero() {
        assert_eq!(clamp_offset(None), 0);
        assert_eq!(clamp_offset(Some(-1)), 0);
        assert_eq!(clamp_offset(Some(40)), 40);
    }

    #[test]
    fn severities_normalised_and_filtered() {
        let out = normalize_severities(Some(vec![
            " critical ".into(),
            "High".into(),
            "bogus".into(),
        ]));
        assert_eq!(out, Some(vec!["CRITICAL".to_string(), "HIGH".to_string()]));
        assert_eq!(normalize_severities(Some(vec!["nope".into()])), None);
        assert_eq!(normalize_severities(None), None);
    }

    #[test]
    fn params_deserialize_with_defaults() {
        let p: ListReportsParams = serde_json::from_str("{}").unwrap();
        assert!(p.cluster.is_none());
        assert!(p.limit.is_none());

        let p: GetVulnerabilityReportParams =
            serde_json::from_str(r#"{"cluster":"c","namespace":"n","name":"r","fixed_only":true}"#)
                .unwrap();
        assert_eq!(p.fixed_only, Some(true));
        assert!(p.severity.is_none());
    }
}
