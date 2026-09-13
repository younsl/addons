//! OSV (https://osv.dev) advisory source.

use std::collections::HashSet;
use std::time::Duration;

use async_trait::async_trait;
use futures_util::StreamExt;
use serde::{Deserialize, Deserializer, Serialize};

use super::cvss::cvss_base_score;
use super::{Advisory, Finding, Result, Scanner, Severity};

const MAX_BODY: usize = 8 << 20;

/// Queries the OSV database (https://osv.dev) for a coordinate. The endpoint is
/// operator-configured and trusted, so a plain client is used; the caller may
/// pass an SSRF-guarded client for hardened deployments.
#[derive(Debug, Clone)]
pub struct Osv {
    url: String,
    client: reqwest::Client,
}

pub type OsvScanner = Osv;

impl Osv {
    /// Builds a scanner against `base_url` (e.g. https://api.osv.dev). A `None`
    /// client gets a default with a short timeout.
    pub fn new(base_url: &str, client: Option<reqwest::Client>) -> Self {
        let client = client.unwrap_or_else(|| {
            reqwest::Client::builder()
                .timeout(Duration::from_secs(10))
                .build()
                .unwrap_or_default()
        });
        Osv {
            url: base_url.trim_end_matches('/').to_string(),
            client,
        }
    }
}

fn null_default<'de, D, T>(d: D) -> std::result::Result<T, D::Error>
where
    D: Deserializer<'de>,
    T: Default + Deserialize<'de>,
{
    Ok(Option::<T>::deserialize(d)?.unwrap_or_default())
}

#[derive(Serialize)]
struct OsvQuery<'a> {
    /// Omitted for a package-level query: OSV then returns every advisory
    /// affecting the package across all versions, used to surface a
    /// vulnerability signal for an approval decision when the exact requested
    /// version is not known (e.g. a blocked npm packument).
    #[serde(skip_serializing_if = "str::is_empty")]
    version: &'a str,
    package: OsvPackage<'a>,
}

#[derive(Serialize)]
struct OsvPackage<'a> {
    name: &'a str,
    ecosystem: &'a str,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct OsvResponse {
    #[serde(deserialize_with = "null_default")]
    vulns: Vec<OsvVuln>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub(crate) struct OsvVuln {
    pub(crate) id: String,
    #[serde(deserialize_with = "null_default")]
    pub(crate) aliases: Vec<String>,
    pub(crate) withdrawn: String,
    #[serde(deserialize_with = "null_default")]
    pub(crate) severity: Vec<OsvSeverity>,
    #[serde(deserialize_with = "null_default")]
    pub(crate) database_specific: OsvDatabaseSpecific,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub(crate) struct OsvSeverity {
    #[serde(rename = "type")]
    pub(crate) r#type: String,
    pub(crate) score: String,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub(crate) struct OsvDatabaseSpecific {
    pub(crate) severity: String,
}

/// Reads at most `limit` bytes of the response body, discarding the rest.
async fn read_limited(resp: reqwest::Response, limit: usize) -> reqwest::Result<Vec<u8>> {
    let mut stream = resp.bytes_stream();
    let mut buf = Vec::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk?;
        let remaining = limit - buf.len();
        if chunk.len() >= remaining {
            buf.extend_from_slice(&chunk[..remaining]);
            break;
        }
        buf.extend_from_slice(&chunk);
    }
    Ok(buf)
}

#[async_trait]
impl Scanner for Osv {
    /// Asks OSV which advisories affect the exact version. OSV resolves the
    /// affected-version ranges server-side, avoiding error-prone local version
    /// comparison across ecosystem version schemes. An empty version performs a
    /// package-level query (every advisory for the package, any version), so an
    /// approval reviewer still gets a vulnerability signal when the requested
    /// version is unknown.
    async fn query(&self, ecosystem: &str, pkg: &str, version: &str) -> Result<Finding> {
        if pkg.is_empty() {
            return Ok(Finding::default());
        }
        let query = OsvQuery {
            version,
            package: OsvPackage {
                name: pkg,
                ecosystem,
            },
        };
        let resp = self
            .client
            .post(format!("{}/v1/query", self.url))
            .json(&query)
            .send()
            .await?;
        let status = resp.status();
        if !status.is_success() {
            return Err(super::Error::Status(status.as_u16()));
        }
        let raw = read_limited(resp, MAX_BODY).await?;
        let doc: OsvResponse = serde_json::from_slice(&raw)?;

        let mut f = Finding::default();
        let mut seen: HashSet<String> = HashSet::new();
        for v in &doc.vulns {
            if !v.withdrawn.is_empty() {
                // retracted advisory: ignore
                continue;
            }
            let mut id = v.id.as_str();
            // Prefer a CVE alias for readability when present.
            if let Some(a) = v.aliases.iter().find(|a| a.starts_with("CVE-")) {
                id = a;
            }
            if id.is_empty() || !seen.insert(id.to_string()) {
                continue;
            }
            f.ids.push(id.to_string());
            let sev = severity_of(v);
            f.advisories.push(Advisory {
                id: id.to_string(),
                severity: sev.to_string(),
                score: score_of(v),
            });
            if sev > Severity::None && (sev as usize) < f.counts.len() {
                f.counts[sev as usize] += 1;
            }
            if sev > f.max {
                f.max = sev;
            }
        }
        Ok(f)
    }

    /// Names the advisory data source, recorded on each scan it produces.
    fn source(&self) -> &str {
        "OSV"
    }
}

/// Returns a display score for an advisory: the CVSS base score computed from a
/// CVSS vector string, a plain numeric score as given, or "" when the source
/// provides no usable score.
pub(crate) fn score_of(v: &OsvVuln) -> String {
    for s in &v.severity {
        let raw = s.score.trim();
        if raw.is_empty() {
            continue;
        }
        if let Some(bs) = cvss_base_score(raw) {
            return format!("{bs:.1}");
        }
        if raw.parse::<f64>().is_ok() {
            return raw.to_string();
        }
    }
    String::new()
}

/// Derives a severity for one advisory. It prefers the GHSA-style
/// database_specific label, then a numeric CVSS score; malware (MAL-) advisories
/// are always critical, and a known advisory with no parseable severity is
/// treated conservatively as High rather than silently ignored.
pub(crate) fn severity_of(v: &OsvVuln) -> Severity {
    match v.database_specific.severity.to_uppercase().as_str() {
        "CRITICAL" => return Severity::Critical,
        "HIGH" => return Severity::High,
        "MODERATE" | "MEDIUM" => return Severity::Medium,
        "LOW" => return Severity::Low,
        _ => {}
    }
    for s in &v.severity {
        if let Ok(score) = s.score.parse::<f64>() {
            return bucket_cvss(score);
        }
    }
    if v.id.starts_with("MAL-") {
        return Severity::Critical;
    }
    Severity::High
}

/// Buckets a numeric CVSS score into a [`Severity`].
pub(crate) fn bucket_cvss(score: f64) -> Severity {
    if score >= 9.0 {
        Severity::Critical
    } else if score >= 7.0 {
        Severity::High
    } else if score >= 4.0 {
        Severity::Medium
    } else if score > 0.0 {
        Severity::Low
    } else {
        Severity::None
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use crate::vuln::osv::bucket_cvss;
    use crate::vuln::*;

    fn install_crypto() {
        rustls::crypto::ring::default_provider()
            .install_default()
            .ok();
    }

    #[tokio::test]
    async fn osv_query() {
        install_crypto();
        let srv = MockServer::start().await;
        Mock::given(method("POST"))
        .and(path("/v1/query"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("Content-Type", "application/json")
                .set_body_string(
                    r#"{"vulns":[
			{"id":"GHSA-aaaa","aliases":["CVE-2026-1111"],"database_specific":{"severity":"HIGH"}},
			{"id":"GHSA-bbbb","withdrawn":"2026-01-01T00:00:00Z","database_specific":{"severity":"CRITICAL"}},
			{"id":"MAL-2026-9","database_specific":{"severity":""}}
		]}"#,
                ),
        )
        .mount(&srv)
        .await;
        Mock::given(wiremock::matchers::any())
            .respond_with(ResponseTemplate::new(404))
            .mount(&srv)
            .await;

        let f = Osv::new(&srv.uri(), None)
            .query("npm", "left-pad", "1.0.0")
            .await
            .unwrap();
        // Two non-withdrawn advisories; the withdrawn CRITICAL one is excluded.
        assert_eq!(f.ids.len(), 2, "ids = {:?}, want 2", f.ids);
        // The CVE alias is preferred over the GHSA id.
        assert_eq!(f.ids[0], "CVE-2026-1111", "first id, want CVE alias");
        // MAL- advisory forces critical even without a severity label.
        assert_eq!(f.max, Severity::Critical, "max severity, want critical");
        // Per-severity histogram: one HIGH and one CRITICAL (withdrawn one excluded).
        assert_eq!(f.count(Severity::High), 1, "counts = {:?}", f.counts);
        assert_eq!(f.count(Severity::Critical), 1, "counts = {:?}", f.counts);
        let sc = f.severity_counts();
        assert_eq!(sc.get("high"), Some(&1), "severity counts = {sc:?}");
        assert_eq!(sc.get("critical"), Some(&1), "severity counts = {sc:?}");
        assert_eq!(sc.len(), 2, "severity counts = {sc:?}");
    }

    #[tokio::test]
    async fn osv_query_clean() {
        install_crypto();
        let srv = MockServer::start().await;
        Mock::given(wiremock::matchers::any())
            .respond_with(ResponseTemplate::new(200).set_body_string("{}"))
            .mount(&srv)
            .await;
        let f = Osv::new(&srv.uri(), None)
            .query("Go", "example.com/m", "v1.0.0")
            .await
            .unwrap();
        assert!(
            f.ids.is_empty() && f.max == Severity::None,
            "clean coordinate = {f:?}"
        );
    }

    #[test]
    fn severity_round_trip() {
        for s in [
            Severity::None,
            Severity::Low,
            Severity::Medium,
            Severity::High,
            Severity::Critical,
        ] {
            let got = parse_severity(s.as_str());
            assert_eq!(got, s, "round-trip {s:?} -> {:?} -> {got:?}", s.as_str());
        }
        assert_eq!(
            parse_severity("bogus"),
            Severity::None,
            "unknown label should parse to none"
        );
        assert_eq!(SEV_NONE, Severity::None);
        assert_eq!(SEV_CRITICAL, Severity::Critical);
    }

    #[test]
    fn bucket_cvss_cases() {
        let cases = [
            (9.8, Severity::Critical),
            (7.5, Severity::High),
            (5.0, Severity::Medium),
            (2.0, Severity::Low),
            (0.0, Severity::None),
        ];
        for (score, want) in cases {
            let got = bucket_cvss(score);
            assert_eq!(got, want, "bucket_cvss({score}) = {got:?}, want {want:?}");
        }
    }

    #[tokio::test]
    async fn query_empty_coordinate() {
        install_crypto();
        // No HTTP call should happen without a package name.
        let f = Osv::new("http://invalid.example", None)
            .query("npm", "", "")
            .await
            .unwrap();
        assert!(f.ids.is_empty(), "empty coordinate = {f:?}");
    }

    #[tokio::test]
    async fn osv_query_package_level() {
        install_crypto();
        // An empty version performs a package-level query: the request must omit the
        // "version" field so OSV returns advisories across all versions.
        let srv = MockServer::start().await;
        Mock::given(wiremock::matchers::any())
            .respond_with(ResponseTemplate::new(200).set_body_string(
                r#"{"vulns":[{"id":"GHSA-pkg","database_specific":{"severity":"MODERATE"}}]}"#,
            ))
            .mount(&srv)
            .await;

        let f = Osv::new(&srv.uri(), None)
            .query("npm", "express", "")
            .await
            .unwrap();
        let reqs = srv.received_requests().await.unwrap();
        assert_eq!(reqs.len(), 1);
        let body: serde_json::Value = serde_json::from_slice(&reqs[0].body).unwrap();
        assert!(
            body.get("version").is_none(),
            "package-level query must omit the version field: {body}"
        );
        assert_eq!(body["package"]["name"], "express");
        assert_eq!(body["package"]["ecosystem"], "npm");
        assert!(
            f.ids.len() == 1 && f.max == Severity::Medium,
            "package-level finding = {f:?}"
        );
    }
}
