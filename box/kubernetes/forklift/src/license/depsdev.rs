//! deps.dev (https://deps.dev) license source.

use std::collections::HashSet;
use std::time::Duration;

use async_trait::async_trait;
use futures_util::StreamExt;
use serde::{Deserialize, Deserializer};

use super::{Error, LicenseResult, Resolver, Result};

const MAX_BODY: usize = 4 << 20;

/// Resolves licenses via the deps.dev API (https://deps.dev), which exposes
/// per-version license metadata for every package system forklift proxies (npm,
/// Maven, Cargo, Go, PyPI) behind one schema. The endpoint is
/// operator-configured and trusted, so a plain client is used; the caller may
/// pass an SSRF-guarded client for hardened deployments.
#[derive(Debug, Clone)]
pub struct DepsDev {
    url: String,
    client: reqwest::Client,
}

pub type DepsDevResolver = DepsDev;

impl DepsDev {
    /// Builds a resolver against `base_url` (e.g. https://api.deps.dev). A
    /// `None` client gets a default with a short timeout.
    pub fn new(base_url: &str, client: Option<reqwest::Client>) -> Self {
        let client = client.unwrap_or_else(|| {
            reqwest::Client::builder()
                .timeout(Duration::from_secs(10))
                .build()
                .unwrap_or_default()
        });
        DepsDev {
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

/// The subset of the deps.dev GetVersion response we use.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct DepsDevVersion {
    #[serde(deserialize_with = "null_default")]
    licenses: Vec<String>,
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
impl Resolver for DepsDev {
    /// Asks deps.dev for the licenses declared by the exact version. `system` is
    /// the deps.dev system name (npm, maven, cargo, go, pypi), `pkg` the package
    /// name (Maven uses "group:artifact"), and `version` the exact version
    /// string.
    async fn resolve(&self, system: &str, pkg: &str, version: &str) -> Result<LicenseResult> {
        if system.is_empty() || pkg.is_empty() || version.is_empty() {
            return Ok(LicenseResult::default());
        }
        let url = format!(
            "{}/v3/systems/{}/packages/{}/versions/{}",
            self.url,
            escape_segment(system),
            escape_segment(pkg),
            escape_segment(version)
        );
        let resp = self.client.get(url).send().await?;
        let status = resp.status();
        if status == reqwest::StatusCode::NOT_FOUND {
            // Unknown coordinate to deps.dev: resolved, but no license data.
            return Ok(LicenseResult::default());
        }
        if !status.is_success() {
            return Err(Error::Status(status.as_u16()));
        }
        let raw = read_limited(resp, MAX_BODY).await?;
        let doc: DepsDevVersion = serde_json::from_slice(&raw)?;
        let mut out = LicenseResult::default();
        let mut seen: HashSet<&str> = HashSet::new();
        for l in &doc.licenses {
            let l = l.trim();
            if l.is_empty() || !seen.insert(l) {
                continue;
            }
            out.licenses.push(l.to_string());
        }
        Ok(out)
    }

    /// Names the data source, recorded on each resolution it produces.
    fn source(&self) -> &str {
        "deps.dev"
    }
}

/// Percent-encodes a URL path segment per RFC 3986, encoding every character
/// outside the unreserved set. It also encodes "/"
/// and ":", which appear in Go module paths and Maven coordinates and must not
/// be read as path separators by deps.dev.
pub(crate) fn escape_segment(s: &str) -> String {
    const UPPERHEX: &[u8; 16] = b"0123456789ABCDEF";
    let mut b = String::with_capacity(s.len());
    for &c in s.as_bytes() {
        if c.is_ascii_alphanumeric() || matches!(c, b'-' | b'.' | b'_' | b'~') {
            b.push(c as char);
            continue;
        }
        b.push('%');
        b.push(UPPERHEX[(c >> 4) as usize] as char);
        b.push(UPPERHEX[(c & 0xf) as usize] as char);
    }
    b
}

#[cfg(test)]
pub(crate) mod tests {
    use wiremock::matchers::any;
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use crate::license::depsdev::escape_segment;
    use crate::license::*;

    fn install_crypto() {
        rustls::crypto::ring::default_provider()
            .install_default()
            .ok();
    }

    #[tokio::test]
    async fn deps_dev_resolve() {
        install_crypto();
        let srv = MockServer::start().await;
        Mock::given(any())
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("Content-Type", "application/json")
                .set_body_string(
                    r#"{"versionKey":{"system":"NPM","name":"lodash","version":"4.17.21"},"licenses":["MIT","MIT"]}"#,
                ),
        )
        .mount(&srv)
        .await;

        let r = DepsDev::new(&srv.uri(), Some(reqwest::Client::new()));
        let res = r.resolve("npm", "lodash", "4.17.21").await.unwrap();
        // Deduplicated to a single MIT.
        assert_eq!(res.licenses, vec!["MIT"], "licenses, want [MIT]");
        let reqs = srv.received_requests().await.unwrap();
        assert_eq!(
            reqs[0].url.path(),
            "/v3/systems/npm/packages/lodash/versions/4.17.21"
        );
    }

    #[tokio::test]
    async fn deps_dev_resolve_encodes_coordinates() {
        install_crypto();
        let srv = MockServer::start().await;
        Mock::given(any())
            .respond_with(
                ResponseTemplate::new(200).set_body_string(r#"{"licenses":["Apache-2.0"]}"#),
            )
            .mount(&srv)
            .await;

        let r = DepsDev::new(&srv.uri(), Some(reqwest::Client::new()));
        r.resolve("maven", "com.google.guava:guava", "32.0.0")
            .await
            .unwrap();
        let reqs = srv.received_requests().await.unwrap();
        assert_eq!(
            reqs[0].url.path(),
            "/v3/systems/maven/packages/com.google.guava%3Aguava/versions/32.0.0"
        );
        // The escaper itself: every byte outside the unreserved set is encoded,
        // "/" and ":" included.
        assert_eq!(escape_segment("github.com/a/b"), "github.com%2Fa%2Fb");
        assert_eq!(escape_segment("a b~_-."), "a%20b~_-.");
    }

    #[tokio::test]
    async fn deps_dev_resolve_not_found() {
        install_crypto();
        let srv = MockServer::start().await;
        Mock::given(any())
            .respond_with(ResponseTemplate::new(404).set_body_string("404 page not found\n"))
            .mount(&srv)
            .await;

        let r = DepsDev::new(&srv.uri(), Some(reqwest::Client::new()));
        let res = r
            .resolve("npm", "ghost", "9.9.9")
            .await
            .expect("404 should be a clean empty result, not an error");
        assert!(
            res.licenses.is_empty(),
            "licenses = {:?}, want empty",
            res.licenses
        );
    }
}
