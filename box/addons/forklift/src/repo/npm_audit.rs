//! Serves `npm audit` and `pnpm audit` through the npm bulk advisory endpoint:
//!
//! ```text
//! POST /npm/{repo}/-/npm/v1/security/advisories/bulk
//! {"lodash": ["4.17.20"], ...}  ->  {"lodash": [{"id": ..., "vulnerable_versions": "<4.17.21", ...}]}
//! ```
//!
//! Answers come from the same OSV source the vulnerability gate uses and
//! honour the repository's `vuln.ignore` list, so the client report and the
//! gate agree. The request never leaves forklift for the npm registry. A hosted
//! repository answers with no advisories, and a group leaves out the packages
//! its hosted members publish, so an internal package is never matched against
//! a public package of the same name.

use std::collections::{BTreeMap, HashSet};
use std::io::Read as _;
use std::sync::Arc;
use std::time::Instant;

use axum::body::Body;
use axum::extract::Request;
use axum::response::Response;
use http::header::{CONTENT_ENCODING, CONTENT_TYPE};
use http::{HeaderValue, Method, StatusCode};
use serde::Serialize;

use crate::server::http_error;
use crate::vuln::{PackageAdvisory, Severity};
use crate::{auth, meta, repoconfig};

use super::Manager;

/// The route, relative to the repository root.
pub(crate) const BULK_PATH: &str = "-/npm/v1/security/advisories/bulk";

/// Caps the request body before and after gzip decoding. A lockfile of 5,000
/// packages is about 200 KiB.
const MAX_REQUEST_BYTES: usize = 8 << 20;
/// Large monorepo lockfiles list a few thousand packages.
const MAX_PACKAGES: usize = 10_000;
const MAX_NAME_LEN: usize = 214;

#[derive(Serialize)]
struct BulkAdvisory {
    id: u64,
    url: String,
    title: String,
    severity: &'static str,
    vulnerable_versions: String,
    cwe: Vec<String>,
    cvss: Cvss,
}

#[derive(Serialize)]
struct Cvss {
    score: f64,
    #[serde(rename = "vectorString")]
    vector_string: Option<String>,
}

/// Serves the npm bulk advisory endpoint.
pub(crate) async fn handle_npm_audit(m: Arc<Manager>, req: Request) -> Response {
    let started = Instant::now();
    let (parts, body) = req.into_parts();
    if parts.method != Method::POST {
        return http_error(StatusCode::METHOD_NOT_ALLOWED, "method not allowed");
    }
    let res = match m.resolve(&parts, meta::FORMAT_NPM).await {
        Ok(res) => res,
        Err(resp) => return resp,
    };
    if let Err(resp) = m.authorize(&parts, &res.repo.name, auth::ACTION_READ, res.cfg.public) {
        return resp;
    }
    let packages = match read_request(parts.headers.get(CONTENT_ENCODING), body).await {
        Ok(packages) => packages,
        Err(resp) => return *resp,
    };
    let repo = res.repo.name.clone();
    let observe = |result: &str| {
        m.npm_audits.with_label_values(&[&repo, result]).inc();
        m.npm_audit_seconds
            .with_label_values(&[result])
            .observe(started.elapsed().as_secs_f64());
    };

    if res.repo.r#type == meta::TYPE_HOSTED {
        observe("ok");
        return json(&BTreeMap::<String, Vec<BulkAdvisory>>::new());
    }
    let Some(scanner) = m.scanner.read().clone() else {
        observe("disabled");
        return http_error(
            StatusCode::NOT_IMPLEMENTED,
            "npm audit needs vulnerability scanning (FORKLIFT_OSV_URL)",
        );
    };

    let mut ignore: HashSet<String> = res.cfg.vuln.ignore.iter().cloned().collect();
    let mut internal = HashSet::new();
    if res.repo.r#type == meta::TYPE_GROUP {
        match m.group_audit_scope(&res.cfg.group.members, &packages).await {
            Ok((member_ignore, hosted)) => {
                ignore.extend(member_ignore);
                internal = hosted;
            }
            Err(err) => {
                tracing::error!(repo = %repo, err = %err, "npm audit member lookup failed");
                observe("error");
                return http_error(StatusCode::INTERNAL_SERVER_ERROR, "metadata error");
            }
        }
    }
    let names: Vec<String> = packages
        .keys()
        .filter(|name| valid_name(name) && !internal.contains(*name))
        .cloned()
        .collect();

    let found = match scanner.advisories("npm", &names).await {
        Ok(found) => found,
        Err(err) => {
            tracing::warn!(repo = %repo, packages = names.len(), err = %err, "npm audit advisory lookup failed");
            observe("error");
            return http_error(StatusCode::BAD_GATEWAY, "advisory lookup failed");
        }
    };

    let mut out: BTreeMap<String, Vec<BulkAdvisory>> = BTreeMap::new();
    for name in &names {
        let Some(list) = found.get(name) else {
            continue;
        };
        let versions = &packages[name];
        let matched: Vec<BulkAdvisory> = list
            .iter()
            .filter(|a| !a.ids().any(|id| ignore.contains(id)))
            .filter(|a| versions.iter().any(|v| a.affected.contains(v)))
            .map(bulk_advisory)
            .collect();
        if !matched.is_empty() {
            out.insert(name.clone(), matched);
        }
    }
    observe("ok");
    json(&out)
}

impl Manager {
    /// Collects the `vuln.ignore` lists of a group's proxy members and the
    /// requested packages its hosted members publish.
    async fn group_audit_scope(
        &self,
        members: &[String],
        packages: &BTreeMap<String, Vec<String>>,
    ) -> meta::Result<(Vec<String>, HashSet<String>)> {
        let mut ignore = Vec::new();
        let mut hosted = HashSet::new();
        for name in members {
            let member = match self.store.get_repository_by_name(name).await {
                Ok(member) => member,
                Err(e) if e.is_not_found() => continue,
                Err(e) => return Err(e),
            };
            if member.format != meta::FORMAT_NPM {
                continue;
            }
            if member.r#type == meta::TYPE_HOSTED {
                hosted.extend(
                    self.store
                        .existing_paths(member.id, packages.keys().cloned().collect())
                        .await?,
                );
            } else if let Ok(cfg) = repoconfig::parse(&member.config_json) {
                ignore.extend(cfg.vuln.ignore);
            }
        }
        Ok((ignore, hosted))
    }
}

/// Reads the `{name: [versions]}` request body, gzip-decoding it when the
/// client compressed it (npm does, pnpm does not).
async fn read_request(
    encoding: Option<&HeaderValue>,
    body: Body,
) -> Result<BTreeMap<String, Vec<String>>, Box<Response>> {
    let raw = axum::body::to_bytes(body, MAX_REQUEST_BYTES)
        .await
        .map_err(|_| {
            Box::new(http_error(
                StatusCode::PAYLOAD_TOO_LARGE,
                "audit request too large",
            ))
        })?;
    let raw = match encoding.and_then(|v| v.to_str().ok()) {
        Some(e) if e.eq_ignore_ascii_case("gzip") => {
            let mut out = Vec::new();
            flate2::read::GzDecoder::new(raw.as_ref())
                .take(MAX_REQUEST_BYTES as u64 + 1)
                .read_to_end(&mut out)
                .map_err(|_| Box::new(http_error(StatusCode::BAD_REQUEST, "invalid gzip body")))?;
            if out.len() > MAX_REQUEST_BYTES {
                return Err(Box::new(http_error(
                    StatusCode::PAYLOAD_TOO_LARGE,
                    "audit request too large",
                )));
            }
            out
        }
        None => raw.to_vec(),
        Some(_) => {
            return Err(Box::new(http_error(
                StatusCode::UNSUPPORTED_MEDIA_TYPE,
                "unsupported content encoding",
            )));
        }
    };
    let packages: BTreeMap<String, Vec<String>> = serde_json::from_slice(&raw).map_err(|_| {
        Box::new(http_error(
            StatusCode::BAD_REQUEST,
            "audit request must map package names to version lists",
        ))
    })?;
    if packages.len() > MAX_PACKAGES {
        return Err(Box::new(http_error(
            StatusCode::PAYLOAD_TOO_LARGE,
            "audit request lists too many packages",
        )));
    }
    Ok(packages)
}

/// Applies npm's name length limit and rejects whitespace and control
/// characters, so a name no registry can publish is never looked up.
fn valid_name(name: &str) -> bool {
    !name.is_empty() && name.len() <= MAX_NAME_LEN && name.bytes().all(|b| b.is_ascii_graphic())
}

fn bulk_advisory(a: &PackageAdvisory) -> BulkAdvisory {
    let url = a.ghsa().map_or_else(
        || format!("https://osv.dev/vulnerability/{}", a.id),
        |ghsa| format!("https://github.com/advisories/{ghsa}"),
    );
    BulkAdvisory {
        id: numeric_id(a.ghsa().unwrap_or(&a.id)),
        url,
        title: a.title.clone(),
        severity: npm_severity(a.severity),
        vulnerable_versions: a.affected.npm_range(),
        cwe: a.cwe.clone(),
        cvss: Cvss {
            score: a.score.unwrap_or(0.0),
            vector_string: (!a.vector.is_empty()).then(|| a.vector.clone()),
        },
    }
}

/// Maps an advisory id to the numeric id npm clients key their report by:
/// FNV-1a over the id, kept within a JavaScript safe integer.
fn numeric_id(id: &str) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in id.bytes() {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x0100_0000_01b3);
    }
    (h & ((1 << 53) - 1)).max(1)
}

const fn npm_severity(s: Severity) -> &'static str {
    match s {
        Severity::Critical => "critical",
        Severity::High => "high",
        Severity::Medium => "moderate",
        Severity::Low => "low",
        Severity::None => "info",
    }
}

fn json<T: Serialize>(v: &T) -> Response {
    let mut resp = Response::new(Body::from(serde_json::to_vec(v).unwrap_or_default()));
    resp.headers_mut()
        .insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
    resp
}

#[cfg(test)]
mod tests {
    use std::io::Write as _;

    use async_trait::async_trait;
    use axum::body::Body;
    use http::{Method, Request, StatusCode};
    use parking_lot::Mutex;
    use serde_json::Value;

    use crate::meta::{self, Artifact};
    use crate::repoconfig;
    use crate::testing::repo::{TestManager, call, mk_format_repo, mux, new_test_manager, send};
    use crate::vuln::{
        self, Affected, Event, Finding, PackageAdvisories, PackageAdvisory, Severity,
    };

    use super::*;

    const URL: &str = "/npm/npmjs/-/npm/v1/security/advisories/bulk";

    /// Serves fixed advisories and records the package names it is asked for.
    #[derive(Default)]
    struct Advisories {
        fail: bool,
        asked: Mutex<Vec<String>>,
    }

    fn advisory(id: &str, aliases: &[&str], sev: Severity, fixed: &str) -> PackageAdvisory {
        PackageAdvisory {
            id: id.to_string(),
            aliases: aliases.iter().map(ToString::to_string).collect(),
            title: format!("{id} title"),
            severity: sev,
            score: Some(7.5),
            vector: "CVSS:3.1/AV:N/AC:L/PR:N/UI:N/S:U/C:N/I:N/A:H".to_string(),
            cwe: vec!["CWE-400".to_string()],
            affected: Affected::new(
                vec![vec![
                    Event::Introduced("0".to_string()),
                    Event::Fixed(fixed.to_string()),
                ]],
                vec![],
            ),
        }
    }

    #[async_trait]
    impl vuln::Scanner for Advisories {
        async fn query(&self, _: &str, _: &str, _: &str) -> vuln::Result<Finding> {
            Ok(Finding::default())
        }
        fn source(&self) -> &'static str {
            "fake"
        }
        async fn advisories(
            &self,
            eco: &str,
            packages: &[String],
        ) -> vuln::Result<PackageAdvisories> {
            assert_eq!(eco, "npm");
            if self.fail {
                return Err(vuln::Error::Status(503));
            }
            self.asked.lock().extend(packages.iter().cloned());
            let mut out = PackageAdvisories::new();
            out.insert(
                "follow-redirects".to_string(),
                Arc::new(vec![
                    advisory(
                        "GHSA-q3jh-wrc6-jgq8",
                        &["CVE-2026-1"],
                        Severity::High,
                        "1.16.1",
                    ),
                    advisory("GHSA-old0-0000-0000", &[], Severity::Low, "1.0.0"),
                ]),
            );
            out.insert(
                "hono".to_string(),
                Arc::new(vec![advisory(
                    "MAL-2026-1",
                    &[],
                    Severity::Medium,
                    "4.13.11",
                )]),
            );
            Ok(out)
        }
    }

    async fn setup(scanner: Option<Arc<Advisories>>, cfg: repoconfig::Config) -> TestManager {
        let tm = new_test_manager().await;
        if let Some(s) = scanner {
            tm.manager.set_vuln_scanner(Some(s));
        }
        mk_format_repo(
            &tm.store,
            "npmjs",
            meta::FORMAT_NPM,
            meta::TYPE_PROXY,
            "http://upstream.invalid",
            cfg,
        )
        .await;
        tm
    }

    const BODY: &str =
        r#"{"follow-redirects":["1.16.0","1.16.1"],"hono":["4.13.7"],"left-pad":["1.3.0"]}"#;

    #[tokio::test]
    async fn proxy_answers_from_scanner() {
        let scanner = Arc::new(Advisories::default());
        let tm = setup(Some(Arc::clone(&scanner)), repoconfig::default()).await;
        let resp = call(&mux(&tm.manager), Method::POST, URL, BODY).await;
        assert_eq!(resp.status, StatusCode::OK, "{}", resp.text());
        assert_eq!(resp.header("content-type"), "application/json");
        let doc: Value = serde_json::from_slice(&resp.body).unwrap();

        let fr = doc["follow-redirects"].as_array().unwrap();
        assert_eq!(
            fr.len(),
            1,
            "the advisory fixed in 1.0.0 matches no version: {doc}"
        );
        assert_eq!(
            fr[0]["url"],
            "https://github.com/advisories/GHSA-q3jh-wrc6-jgq8"
        );
        assert_eq!(fr[0]["severity"], "high");
        assert_eq!(fr[0]["vulnerable_versions"], "<1.16.1");
        assert_eq!(fr[0]["title"], "GHSA-q3jh-wrc6-jgq8 title");
        assert_eq!(fr[0]["cwe"][0], "CWE-400");
        assert_eq!(fr[0]["cvss"]["score"], 7.5);
        assert_eq!(fr[0]["id"], numeric_id("GHSA-q3jh-wrc6-jgq8"));

        let hono = &doc["hono"][0];
        assert_eq!(hono["severity"], "moderate");
        assert_eq!(hono["url"], "https://osv.dev/vulnerability/MAL-2026-1");
        assert!(
            doc.get("left-pad").is_none(),
            "clean packages are omitted: {doc}"
        );

        let mut asked = scanner.asked.lock().clone();
        asked.sort();
        assert_eq!(asked, ["follow-redirects", "hono", "left-pad"]);
    }

    #[tokio::test]
    async fn ignore_list_matches_id_or_alias() {
        let mut cfg = repoconfig::default();
        cfg.vuln.ignore = vec!["CVE-2026-1".to_string(), "MAL-2026-1".to_string()];
        let tm = setup(Some(Arc::new(Advisories::default())), cfg).await;
        let resp = call(&mux(&tm.manager), Method::POST, URL, BODY).await;
        assert_eq!(resp.status, StatusCode::OK);
        assert_eq!(resp.text(), "{}");
    }

    #[tokio::test]
    async fn gzip_body() {
        let tm = setup(Some(Arc::new(Advisories::default())), repoconfig::default()).await;
        let mut enc = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        enc.write_all(BODY.as_bytes()).unwrap();
        let req = Request::builder()
            .method(Method::POST)
            .uri(URL)
            .header("content-encoding", "gzip")
            .body(Body::from(enc.finish().unwrap()))
            .unwrap();
        let resp = send(&mux(&tm.manager), req).await;
        assert_eq!(resp.status, StatusCode::OK, "{}", resp.text());
        assert!(resp.text().contains("follow-redirects"));

        let req = Request::builder()
            .method(Method::POST)
            .uri(URL)
            .header("content-encoding", "gzip")
            .body(Body::from("not gzip"))
            .unwrap();
        assert_eq!(
            send(&mux(&tm.manager), req).await.status,
            StatusCode::BAD_REQUEST
        );

        let req = Request::builder()
            .method(Method::POST)
            .uri(URL)
            .header("content-encoding", "br")
            .body(Body::from(BODY))
            .unwrap();
        assert_eq!(
            send(&mux(&tm.manager), req).await.status,
            StatusCode::UNSUPPORTED_MEDIA_TYPE
        );
    }

    #[tokio::test]
    async fn rejects_bad_requests() {
        let tm = setup(Some(Arc::new(Advisories::default())), repoconfig::default()).await;
        let h = mux(&tm.manager);
        assert_eq!(
            call(&h, Method::GET, URL, "").await.status,
            StatusCode::METHOD_NOT_ALLOWED
        );
        assert_eq!(
            call(&h, Method::POST, URL, r#"{"a":"1.0.0"}"#).await.status,
            StatusCode::BAD_REQUEST
        );
        assert_eq!(
            call(
                &h,
                Method::POST,
                "/npm/missing/-/npm/v1/security/advisories/bulk",
                "{}"
            )
            .await
            .status,
            StatusCode::NOT_FOUND
        );
    }

    #[tokio::test]
    async fn scanner_states() {
        let tm = setup(None, repoconfig::default()).await;
        let resp = call(&mux(&tm.manager), Method::POST, URL, BODY).await;
        assert_eq!(resp.status, StatusCode::NOT_IMPLEMENTED);

        let failing = Arc::new(Advisories {
            fail: true,
            ..Default::default()
        });
        tm.manager.set_vuln_scanner(Some(failing));
        let resp = call(&mux(&tm.manager), Method::POST, URL, BODY).await;
        assert_eq!(
            resp.status,
            StatusCode::BAD_GATEWAY,
            "a failed lookup is never a clean report"
        );
    }

    #[tokio::test]
    async fn hosted_answers_empty() {
        let tm = setup(Some(Arc::new(Advisories::default())), repoconfig::default()).await;
        mk_format_repo(
            &tm.store,
            "internal",
            meta::FORMAT_NPM,
            meta::TYPE_HOSTED,
            "",
            repoconfig::default(),
        )
        .await;
        let resp = call(
            &mux(&tm.manager),
            Method::POST,
            "/npm/internal/-/npm/v1/security/advisories/bulk",
            BODY,
        )
        .await;
        assert_eq!(resp.status, StatusCode::OK);
        assert_eq!(resp.text(), "{}");
    }

    #[tokio::test]
    async fn group_skips_hosted_packages_and_merges_ignores() {
        let scanner = Arc::new(Advisories::default());
        let mut proxy_cfg = repoconfig::default();
        proxy_cfg.vuln.ignore = vec!["MAL-2026-1".to_string()];
        let tm = setup(Some(Arc::clone(&scanner)), proxy_cfg).await;
        let hosted = mk_format_repo(
            &tm.store,
            "internal",
            meta::FORMAT_NPM,
            meta::TYPE_HOSTED,
            "",
            repoconfig::default(),
        )
        .await;
        tm.store
            .put_artifact(Artifact {
                repo_id: hosted.id,
                path: "follow-redirects".to_string(),
                blob_sha256: "digest-packument".to_string(),
                size: 2,
                ..Default::default()
            })
            .await
            .expect("put packument");
        let mut group_cfg = repoconfig::default();
        group_cfg.group.members = vec![
            "internal".to_string(),
            "npmjs".to_string(),
            "gone".to_string(),
        ];
        mk_format_repo(
            &tm.store,
            "npm-all",
            meta::FORMAT_NPM,
            meta::TYPE_GROUP,
            "",
            group_cfg,
        )
        .await;

        let resp = call(
            &mux(&tm.manager),
            Method::POST,
            "/npm/npm-all/-/npm/v1/security/advisories/bulk",
            BODY,
        )
        .await;
        assert_eq!(resp.status, StatusCode::OK, "{}", resp.text());
        assert_eq!(
            resp.text(),
            "{}",
            "hosted name skipped, member ignore applied"
        );
        let mut asked = scanner.asked.lock().clone();
        asked.sort();
        assert_eq!(
            asked,
            ["hono", "left-pad"],
            "internal package never looked up"
        );
    }

    #[tokio::test]
    async fn limits_package_count_and_names() {
        let scanner = Arc::new(Advisories::default());
        let tm = setup(Some(Arc::clone(&scanner)), repoconfig::default()).await;
        let h = mux(&tm.manager);
        let many: BTreeMap<String, Vec<String>> = (0..=MAX_PACKAGES)
            .map(|i| (format!("p{i}"), vec!["1.0.0".to_string()]))
            .collect();
        let resp = call(
            &h,
            Method::POST,
            URL,
            &serde_json::to_string(&many).unwrap(),
        )
        .await;
        assert_eq!(resp.status, StatusCode::PAYLOAD_TOO_LARGE);
        assert!(scanner.asked.lock().is_empty());

        let long = "a".repeat(MAX_NAME_LEN + 1);
        let body = format!(r#"{{"{long}":["1.0.0"],"bad name":["1.0.0"],"ok":["1.0.0"]}}"#);
        let resp = call(&h, Method::POST, URL, &body).await;
        assert_eq!(resp.status, StatusCode::OK);
        assert_eq!(*scanner.asked.lock(), ["ok"]);
    }

    #[test]
    fn numeric_id_is_stable_and_js_safe() {
        let a = numeric_id("GHSA-q3jh-wrc6-jgq8");
        assert_eq!(a, numeric_id("GHSA-q3jh-wrc6-jgq8"));
        assert_ne!(a, numeric_id("GHSA-8r9p-f939-6h3c"));
        assert!(a > 0 && a < (1 << 53));
        assert_eq!(npm_severity(Severity::None), "info");
        assert_eq!(npm_severity(Severity::Critical), "critical");
        assert_eq!(npm_severity(Severity::Low), "low");
    }

    #[test]
    fn advisory_without_vector() {
        let mut a = advisory("GHSA-aaaa-bbbb-cccc", &[], Severity::High, "2.0.0");
        a.vector = String::new();
        a.score = None;
        let b = bulk_advisory(&a);
        assert_eq!(b.cvss.score, 0.0);
        assert!(b.cvss.vector_string.is_none());
    }
}
