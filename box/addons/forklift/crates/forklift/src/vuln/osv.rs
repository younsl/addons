//! OSV (https://osv.dev) advisory source.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use dashmap::DashMap;
use futures_util::{StreamExt, TryStreamExt};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use super::cvss::cvss_base_score;
use super::{
    Advisory, Affected, Event, Finding, PackageAdvisories, PackageAdvisory, Result, Scanner,
    Severity,
};

const MAX_BODY: usize = 8 << 20;

/// OSV's per-request query limit for `/v1/querybatch`.
const BATCH_SIZE: usize = 1000;
/// Bounds the follow-up pages fetched for a package with many advisories.
const MAX_PAGES: usize = 5;
const DETAIL_CONCURRENCY: usize = 16;
/// How long a package's advisory list is reused before OSV is asked again.
const ADVISORY_TTL: Duration = Duration::from_secs(3600);
const MAX_CACHED_PACKAGES: usize = 100_000;
/// Bounds how many uncached lookups run against OSV at once, across requests.
const MAX_CONCURRENT_LOOKUPS: usize = 4;

#[derive(Debug)]
struct Cached {
    at: Instant,
    list: Arc<Vec<PackageAdvisory>>,
}

/// Queries the OSV database (https://osv.dev) for a coordinate. The endpoint is
/// operator-configured and trusted, so a plain client is used; the caller may
/// pass an SSRF-guarded client for hardened deployments.
#[derive(Debug, Clone)]
pub struct Osv {
    url: String,
    client: reqwest::Client,
    cache: Arc<DashMap<(String, String), Cached>>,
    lookups: Arc<tokio::sync::Semaphore>,
}

pub type OsvScanner = Osv;

impl Osv {
    /// Builds a scanner against `base_url` (e.g. https://api.osv.dev). A `None`
    /// client gets a default with a short timeout.
    pub fn new(base_url: &str, client: Option<reqwest::Client>) -> Self {
        let client = client.unwrap_or_else(|| {
            crate::outbound::client_builder()
                .timeout(Duration::from_secs(10))
                .build()
                .unwrap_or_default()
        });
        Self {
            url: base_url.trim_end_matches('/').to_string(),
            client,
            cache: Arc::new(DashMap::new()),
            lookups: Arc::new(tokio::sync::Semaphore::new(MAX_CONCURRENT_LOOKUPS)),
        }
    }

    async fn post_json<B: Serialize + Sync, R: DeserializeOwned>(
        &self,
        path: &str,
        body: &B,
    ) -> Result<R> {
        let resp = self
            .client
            .post(format!("{}{path}", self.url))
            .json(body)
            .send()
            .await?;
        decode(resp).await
    }

    async fn vuln(&self, id: &str) -> Result<OsvVuln> {
        let resp = self
            .client
            .get(format!("{}/v1/vulns/{id}", self.url))
            .send()
            .await?;
        decode(resp).await
    }

    async fn vuln_entry(&self, id: String) -> Result<(String, OsvVuln)> {
        let v = self.vuln(&id).await?;
        Ok((id, v))
    }

    /// Lists the advisory ids affecting each package, aligned with `names`.
    async fn batch_ids(&self, ecosystem: &str, names: &[&str]) -> Result<Vec<Vec<String>>> {
        let mut ids = vec![Vec::new(); names.len()];
        let mut pending: Vec<(usize, String)> =
            (0..names.len()).map(|i| (i, String::new())).collect();
        for _ in 0..MAX_PAGES {
            if pending.is_empty() {
                break;
            }
            let mut next = Vec::new();
            for chunk in pending.chunks(BATCH_SIZE) {
                let batch = OsvBatch {
                    queries: chunk
                        .iter()
                        .map(|(i, token)| OsvBatchQuery {
                            package: OsvPackage {
                                name: names[*i],
                                ecosystem,
                            },
                            page_token: token,
                        })
                        .collect(),
                };
                let doc: OsvBatchResponse = self.post_json("/v1/querybatch", &batch).await?;
                for ((i, _), result) in chunk.iter().zip(doc.results) {
                    ids[*i].extend(result.vulns.into_iter().map(|v| v.id));
                    if !result.next_page_token.is_empty() {
                        next.push((*i, result.next_page_token));
                    }
                }
            }
            pending = next;
        }
        Ok(ids)
    }

    /// Drops expired entries once the cache is over its cap, then the oldest
    /// ones, so a burst of new names never empties the whole cache.
    fn prune_cache(&self, now: Instant, cap: usize) {
        if self.cache.len() <= cap {
            return;
        }
        self.cache
            .retain(|_, c| now.duration_since(c.at) < ADVISORY_TTL);
        let excess = self.cache.len().saturating_sub(cap);
        if excess == 0 {
            return;
        }
        let mut ages: Vec<(Instant, (String, String))> = self
            .cache
            .iter()
            .map(|e| (e.value().at, e.key().clone()))
            .collect();
        ages.sort_unstable_by_key(|(at, _)| *at);
        for (_, key) in ages.into_iter().take(excess) {
            self.cache.remove(&key);
        }
    }

    fn cached(
        &self,
        ecosystem: &str,
        name: &str,
        now: Instant,
    ) -> Option<Arc<Vec<PackageAdvisory>>> {
        self.cache
            .get(&(ecosystem.to_string(), name.to_string()))
            .filter(|hit| now.duration_since(hit.at) < ADVISORY_TTL)
            .map(|hit| Arc::clone(&hit.list))
    }
}

async fn decode<R: DeserializeOwned>(resp: reqwest::Response) -> Result<R> {
    let status = resp.status();
    if !status.is_success() {
        return Err(super::Error::Status(status.as_u16()));
    }
    let raw = read_limited(resp, MAX_BODY).await?;
    Ok(serde_json::from_slice(&raw)?)
}

/// Accepts only the characters OSV ids use, so an id from a response cannot
/// change the path of the detail request.
fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b':'))
}

#[derive(Serialize)]
struct OsvBatch<'a> {
    queries: Vec<OsvBatchQuery<'a>>,
}

#[derive(Serialize)]
struct OsvBatchQuery<'a> {
    package: OsvPackage<'a>,
    #[serde(skip_serializing_if = "str::is_empty")]
    page_token: &'a str,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct OsvBatchResponse {
    #[serde(deserialize_with = "crate::repoconfig::null_default")]
    results: Vec<OsvBatchResult>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct OsvBatchResult {
    #[serde(deserialize_with = "crate::repoconfig::null_default")]
    vulns: Vec<OsvVulnRef>,
    next_page_token: String,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct OsvVulnRef {
    id: String,
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
    #[serde(deserialize_with = "crate::repoconfig::null_default")]
    vulns: Vec<OsvVuln>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub(crate) struct OsvVuln {
    pub(crate) id: String,
    #[serde(deserialize_with = "crate::repoconfig::null_default")]
    pub(crate) aliases: Vec<String>,
    pub(crate) withdrawn: String,
    #[serde(deserialize_with = "crate::repoconfig::null_default")]
    pub(crate) severity: Vec<OsvSeverity>,
    #[serde(deserialize_with = "crate::repoconfig::null_default")]
    pub(crate) database_specific: OsvDatabaseSpecific,
    pub(crate) summary: String,
    #[serde(deserialize_with = "crate::repoconfig::null_default")]
    pub(crate) affected: Vec<OsvAffected>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub(crate) struct OsvAffected {
    #[serde(deserialize_with = "crate::repoconfig::null_default")]
    pub(crate) package: OsvAffectedPackage,
    #[serde(deserialize_with = "crate::repoconfig::null_default")]
    pub(crate) ranges: Vec<OsvRange>,
    #[serde(deserialize_with = "crate::repoconfig::null_default")]
    pub(crate) versions: Vec<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub(crate) struct OsvAffectedPackage {
    pub(crate) name: String,
    pub(crate) ecosystem: String,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub(crate) struct OsvRange {
    #[serde(rename = "type")]
    pub(crate) r#type: String,
    #[serde(deserialize_with = "crate::repoconfig::null_default")]
    pub(crate) events: Vec<OsvEvent>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub(crate) struct OsvEvent {
    pub(crate) introduced: String,
    pub(crate) fixed: String,
    pub(crate) last_affected: String,
}

impl OsvEvent {
    fn event(&self) -> Option<Event> {
        if !self.introduced.is_empty() {
            Some(Event::Introduced(self.introduced.clone()))
        } else if !self.fixed.is_empty() {
            Some(Event::Fixed(self.fixed.clone()))
        } else if !self.last_affected.is_empty() {
            Some(Event::LastAffected(self.last_affected.clone()))
        } else {
            None
        }
    }
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
    #[serde(deserialize_with = "crate::repoconfig::null_default")]
    pub(crate) cwe_ids: Vec<String>,
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
        let doc: OsvResponse = self.post_json("/v1/query", &query).await?;

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
    fn source(&self) -> &'static str {
        "OSV"
    }

    /// Lists each package's advisories with `/v1/querybatch` and reads every
    /// new advisory once through `/v1/vulns/{id}`. Results are cached per
    /// package for [`ADVISORY_TTL`], and any failed request fails the whole
    /// lookup so a caller never mistakes it for a clean result.
    async fn advisories(&self, ecosystem: &str, packages: &[String]) -> Result<PackageAdvisories> {
        let now = Instant::now();
        let mut out = PackageAdvisories::new();
        let mut missing: Vec<&str> = Vec::new();
        for name in packages {
            if name.is_empty() || out.contains_key(name) {
                continue;
            }
            match self.cached(ecosystem, name, now) {
                Some(list) => {
                    out.insert(name.clone(), list);
                }
                None => missing.push(name),
            }
        }
        missing.sort_unstable();
        missing.dedup();
        if missing.is_empty() {
            return Ok(out);
        }

        // Concurrent audits of the same lockfile wait here, then reuse what
        // the first one cached.
        let _permit = self.lookups.acquire().await;
        missing.retain(|name| match self.cached(ecosystem, name, now) {
            Some(list) => {
                out.insert((*name).to_string(), list);
                false
            }
            None => true,
        });
        if missing.is_empty() {
            return Ok(out);
        }

        let ids = self.batch_ids(ecosystem, &missing).await?;
        let mut unique: Vec<String> = ids
            .iter()
            .flatten()
            .filter(|id| valid_id(id))
            .cloned()
            .collect();
        unique.sort_unstable();
        unique.dedup();
        let details: HashMap<String, OsvVuln> = futures_util::stream::iter(unique)
            .map(|id| self.vuln_entry(id))
            .buffer_unordered(DETAIL_CONCURRENCY)
            .try_collect()
            .await?;

        for (name, ids) in missing.into_iter().zip(ids) {
            let mut seen = HashSet::new();
            let list: Vec<PackageAdvisory> = ids
                .iter()
                .filter(|id| seen.insert(id.as_str()))
                .filter_map(|id| details.get(id))
                .filter(|v| v.withdrawn.is_empty())
                .filter_map(|v| package_advisory(v, ecosystem, name))
                .collect();
            let list = Arc::new(list);
            self.cache.insert(
                (ecosystem.to_string(), name.to_string()),
                Cached {
                    at: now,
                    list: Arc::clone(&list),
                },
            );
            out.insert(name.to_string(), list);
        }
        self.prune_cache(now, MAX_CACHED_PACKAGES);
        Ok(out)
    }
}

/// Builds the advisory as it applies to `name`, or `None` when the record
/// lists no affected version of it.
fn package_advisory(v: &OsvVuln, ecosystem: &str, name: &str) -> Option<PackageAdvisory> {
    let mut ranges = Vec::new();
    let mut versions = Vec::new();
    for a in v
        .affected
        .iter()
        .filter(|a| a.package.ecosystem == ecosystem && a.package.name == name)
    {
        for r in &a.ranges {
            if r.r#type == "SEMVER" || r.r#type == "ECOSYSTEM" {
                ranges.push(r.events.iter().filter_map(OsvEvent::event).collect());
            }
        }
        versions.extend(a.versions.iter().cloned());
    }
    let affected = Affected::new(ranges, versions);
    if affected.is_empty() {
        return None;
    }
    let (score, vector) = cvss_of(v);
    Some(PackageAdvisory {
        id: v.id.clone(),
        aliases: v.aliases.clone(),
        title: if v.summary.is_empty() {
            v.id.clone()
        } else {
            v.summary.clone()
        },
        severity: severity_of(v),
        score,
        vector,
        cwe: v.database_specific.cwe_ids.clone(),
        affected,
    })
}

/// Returns the CVSS base score and vector of an advisory: the score computed
/// from the first parseable vector, or a plain numeric score. A vector whose
/// score cannot be computed here (CVSS v4) is still returned without a score.
fn cvss_of(v: &OsvVuln) -> (Option<f64>, String) {
    let mut vector = String::new();
    for s in &v.severity {
        let raw = s.score.trim();
        if let Some(bs) = cvss_base_score(raw) {
            return (Some(bs), raw.to_string());
        }
        if let Ok(n) = raw.parse::<f64>() {
            return (Some(n), vector);
        }
        if vector.is_empty() && raw.starts_with("CVSS:") {
            vector = raw.to_string();
        }
    }
    (None, vector)
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
        rustls::crypto::aws_lc_rs::default_provider()
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

    #[tokio::test]
    async fn osv_advisories() {
        install_crypto();
        let srv = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/querybatch"))
            .respond_with(ResponseTemplate::new(200).set_body_string(
                r#"{"results":[
                    {"vulns":[{"id":"GHSA-aaaa-aaaa-aaaa"},{"id":"GHSA-gone-gone-gone"},{"id":"../x"}]},
                    {},
                    {"vulns":[{"id":"GHSA-aaaa-aaaa-aaaa"}]}
                ]}"#,
            ))
            .expect(1)
            .mount(&srv)
            .await;
        Mock::given(method("GET"))
            .and(path("/v1/vulns/GHSA-aaaa-aaaa-aaaa"))
            .respond_with(ResponseTemplate::new(200).set_body_string(
                r#"{"id":"GHSA-aaaa-aaaa-aaaa","aliases":["CVE-2026-7"],"summary":"redirect leak",
                    "severity":[{"type":"CVSS_V3","score":"CVSS:3.1/AV:N/AC:L/PR:N/UI:N/S:U/C:H/I:H/A:H"}],
                    "database_specific":{"severity":"HIGH","cwe_ids":["CWE-200"]},
                    "affected":[
                        {"package":{"ecosystem":"npm","name":"follow-redirects"},
                         "ranges":[{"type":"SEMVER","events":[{"introduced":"1.16.0"},{"fixed":"1.16.1"}]}]},
                        {"package":{"ecosystem":"npm","name":"other"},"versions":["0.1.0"]}
                    ]}"#,
            ))
            .expect(1)
            .mount(&srv)
            .await;
        Mock::given(method("GET"))
            .and(path("/v1/vulns/GHSA-gone-gone-gone"))
            .respond_with(ResponseTemplate::new(200).set_body_string(
                r#"{"id":"GHSA-gone-gone-gone","withdrawn":"2026-01-01T00:00:00Z",
                    "affected":[{"package":{"ecosystem":"npm","name":"follow-redirects"},"versions":["1.0.0"]}]}"#,
            ))
            .mount(&srv)
            .await;

        let osv = Osv::new(&srv.uri(), None);
        let names = vec![
            "follow-redirects".to_string(),
            "left-pad".to_string(),
            "other".to_string(),
            "follow-redirects".to_string(),
            String::new(),
        ];
        let got = osv.advisories("npm", &names).await.unwrap();
        let fr = &got["follow-redirects"];
        assert_eq!(fr.len(), 1, "withdrawn advisory dropped: {fr:?}");
        let a = &fr[0];
        assert_eq!(a.ghsa(), Some("GHSA-aaaa-aaaa-aaaa"));
        assert_eq!(a.title, "redirect leak");
        assert_eq!(a.severity, Severity::High);
        assert_eq!(a.score, Some(9.8));
        assert!(a.vector.starts_with("CVSS:3.1/"));
        assert_eq!(a.cwe, ["CWE-200"]);
        assert_eq!(a.affected.npm_range(), ">=1.16.0 <1.16.1");
        assert!(got["left-pad"].is_empty());
        assert_eq!(got["other"][0].affected.npm_range(), "0.1.0");
        assert_eq!(
            got["other"][0].ids().collect::<Vec<_>>(),
            ["GHSA-aaaa-aaaa-aaaa", "CVE-2026-7"]
        );

        // Served from the cache: the mocks above expect exactly one call each.
        let again = osv.advisories("npm", &names[..1]).await.unwrap();
        assert_eq!(again["follow-redirects"].len(), 1);
    }

    #[tokio::test]
    async fn osv_advisories_pages_and_failures() {
        install_crypto();
        let srv = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/querybatch"))
            .and(wiremock::matchers::body_string_contains("page-2"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_string(r#"{"results":[{"vulns":[{"id":"GHSA-page-two0-0000"}]}]}"#),
            )
            .mount(&srv)
            .await;
        Mock::given(method("POST"))
            .and(path("/v1/querybatch"))
            .respond_with(ResponseTemplate::new(200).set_body_string(
                r#"{"results":[{"vulns":[{"id":"GHSA-page-one0-0000"}],"next_page_token":"page-2"}]}"#,
            ))
            .mount(&srv)
            .await;
        Mock::given(method("GET"))
            .and(path("/v1/vulns/GHSA-page-one0-0000"))
            .respond_with(ResponseTemplate::new(200).set_body_string(
                r#"{"id":"GHSA-page-one0-0000","severity":[{"type":"CVSS_V3","score":"5.0"}],
                    "affected":[{"package":{"ecosystem":"npm","name":"p"},"ranges":[{"type":"SEMVER","events":[{"introduced":"0"},{"last_affected":"1.0.0"}]}]}]}"#,
            ))
            .mount(&srv)
            .await;
        Mock::given(method("GET"))
            .and(path("/v1/vulns/GHSA-page-two0-0000"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&srv)
            .await;

        let osv = Osv::new(&srv.uri(), None);
        let err = osv.advisories("npm", &["p".to_string()]).await.unwrap_err();
        assert!(matches!(err, crate::vuln::Error::Status(500)), "{err}");

        Mock::given(method("GET"))
            .and(path("/v1/vulns/GHSA-page-two0-0000"))
            .respond_with(
                ResponseTemplate::new(200).set_body_string(r#"{"id":"GHSA-page-two0-0000"}"#),
            )
            .with_priority(1)
            .mount(&srv)
            .await;
        let got = osv.advisories("npm", &["p".to_string()]).await.unwrap();
        let p = &got["p"];
        assert_eq!(
            p.len(),
            1,
            "the second page's record lists no npm range: {p:?}"
        );
        assert_eq!(p[0].score, Some(5.0));
        assert_eq!(p[0].vector, "");
        assert_eq!(p[0].affected.npm_range(), "<=1.0.0");
        assert_eq!(p[0].title, "GHSA-page-one0-0000");
    }

    #[test]
    fn cvss_of_keeps_v4_vector() {
        use super::{OsvSeverity, OsvVuln};

        let sev = |score: &str| OsvSeverity {
            r#type: "CVSS_V4".into(),
            score: score.into(),
        };
        let v4 = "CVSS:4.0/AV:N/AC:L/AT:N/PR:N/UI:N/VC:H/VI:N/VA:N/SC:N/SI:N/SA:N";
        let only_v4 = OsvVuln {
            severity: vec![sev(v4)],
            ..Default::default()
        };
        assert_eq!(super::cvss_of(&only_v4), (None, v4.to_string()));
        let with_v3 = OsvVuln {
            severity: vec![sev(v4), sev("CVSS:3.1/AV:N/AC:L/PR:N/UI:N/S:U/C:H/I:H/A:H")],
            ..Default::default()
        };
        assert_eq!(super::cvss_of(&with_v3).0, Some(9.8));
    }

    #[tokio::test]
    async fn default_scanner_has_no_advisory_lookup() {
        struct Plain;
        #[async_trait::async_trait]
        impl Scanner for Plain {
            async fn query(&self, _: &str, _: &str, _: &str) -> crate::vuln::Result<Finding> {
                Ok(Finding::default())
            }
            fn source(&self) -> &'static str {
                "plain"
            }
        }
        let err = Plain.advisories("npm", &[]).await.unwrap_err();
        assert!(matches!(err, crate::vuln::Error::Unsupported));
        assert_eq!(Plain.source(), "plain");
    }

    #[test]
    fn prune_cache_evicts_oldest_only() {
        use std::sync::Arc;
        use std::time::{Duration, Instant};

        use super::ADVISORY_TTL;

        let osv = Osv::new("http://invalid.example", None);
        let base = Instant::now();
        for i in 0..5u64 {
            osv.cache.insert(
                ("npm".to_string(), format!("p{i}")),
                super::Cached {
                    at: base + Duration::from_secs(i),
                    list: Arc::new(Vec::new()),
                },
            );
        }
        osv.prune_cache(base + Duration::from_secs(10), 3);
        let mut left: Vec<String> = osv.cache.iter().map(|e| e.key().1.clone()).collect();
        left.sort();
        assert_eq!(left, ["p2", "p3", "p4"]);

        osv.prune_cache(base + ADVISORY_TTL + Duration::from_secs(3), 2);
        let left: Vec<String> = osv.cache.iter().map(|e| e.key().1.clone()).collect();
        assert_eq!(left, ["p4"], "expired entries go first");
    }

    #[test]
    fn valid_id_cases() {
        assert!(super::valid_id("GHSA-q3jh-wrc6-jgq8"));
        assert!(super::valid_id("PYSEC-2026.1"));
        assert!(!super::valid_id("../etc"));
        assert!(!super::valid_id("a/b"));
        assert!(!super::valid_id(""));
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
