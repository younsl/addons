//! Resolves where a Cargo proxy fetches a `.crate` from.
//!
//! A sparse registry does not have to serve its crates from the index host: the
//! index's `config.json` names the download location in `dl`, and crates.io
//! points it at `https://static.crates.io/crates` while the index lives on
//! `https://index.crates.io`. Joining the download path onto the index URL
//! therefore works only for registries that host both on one server, so the
//! proxy reads `dl` from upstream and expands it the way cargo does.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use tokio::io::AsyncReadExt as _;
use url::Url;

use super::router::{Resolved, join_upstream};
use super::uiupload_cargo::cargo_sparse_path;
use super::{FetchSpec, Kind, Manager};

/// How long an upstream `dl` template is reused before it is fetched again.
const DL_TTL: Duration = Duration::from_secs(15 * 60);
/// How long a failed `config.json` fetch is remembered, so a registry without
/// one is not asked on every download.
const DL_MISS_TTL: Duration = Duration::from_secs(60);
/// The largest upstream `config.json` or index entry file read here.
const MAX_DOC_BYTES: u64 = 1 << 20;

/// The template markers cargo substitutes in `dl`.
const MARKERS: [&str; 5] = [
    "{crate}",
    "{version}",
    "{prefix}",
    "{lowerprefix}",
    "{sha256-checksum}",
];

/// Upstream `dl` templates keyed by the upstream index URL. `None` records a
/// registry whose `config.json` could not be read or named no `dl`.
#[derive(Default)]
pub(crate) struct CargoDlCache(parking_lot::Mutex<HashMap<String, (Instant, Option<String>)>>);

impl Manager {
    /// The upstream URL of `name@version`'s `.crate` for a proxy repository.
    /// Falls back to the download path joined onto the index URL when upstream
    /// publishes no usable `dl`, which is correct for single-host registries.
    pub(crate) async fn cargo_upstream_download_url(
        &self,
        res: &Resolved,
        name: &str,
        version: &str,
    ) -> String {
        let fallback = || join_upstream(&res.repo.upstream_url, &res.path);
        let Some(dl) = self.cargo_upstream_dl(res).await else {
            return fallback();
        };
        let checksum = if dl.contains("{sha256-checksum}") {
            self.cargo_index_checksum(res, name, version).await
        } else {
            None
        };
        expand_dl(&dl, name, version, checksum.as_deref()).unwrap_or_else(fallback)
    }

    /// The upstream `dl` template, from cache or upstream `config.json`.
    async fn cargo_upstream_dl(&self, res: &Resolved) -> Option<String> {
        let key = res.repo.upstream_url.clone();
        if let Some((at, dl)) = self.cargo_dl.0.lock().get(&key)
            && at.elapsed() < if dl.is_some() { DL_TTL } else { DL_MISS_TTL }
        {
            return dl.clone();
        }
        let dl = self.fetch_cargo_dl(res).await;
        self.cargo_dl
            .0
            .lock()
            .insert(key, (Instant::now(), dl.clone()));
        dl
    }

    async fn fetch_cargo_dl(&self, res: &Resolved) -> Option<String> {
        let spec = FetchSpec {
            repo: res.repo.clone(),
            cfg: res.cfg.clone(),
            path: "config.json".to_string(),
            upstream_url: join_upstream(&res.repo.upstream_url, "config.json"),
            kind: Kind::Metadata,
            ..FetchSpec::blank()
        };
        let resp = match self.engine.upstream_get(&spec).await {
            Ok(resp) if resp.status().is_success() => resp,
            Ok(resp) => {
                tracing::warn!(repo = %res.repo.name, status = %resp.status(), "cargo: upstream config.json unavailable, joining download paths onto the index URL");
                return None;
            }
            Err(e) => {
                tracing::warn!(repo = %res.repo.name, err = %e, "cargo: upstream config.json fetch failed, joining download paths onto the index URL");
                return None;
            }
        };
        let body = read_bounded(resp).await?;
        #[derive(serde::Deserialize)]
        struct RegistryConfig {
            #[serde(default)]
            dl: String,
        }
        let config: RegistryConfig = serde_json::from_slice(&body).ok()?;
        (!config.dl.is_empty()).then_some(config.dl)
    }

    /// Reads `name@version`'s `cksum` from the proxied sparse-index entry,
    /// which cargo always fetches before the download.
    async fn cargo_index_checksum(
        &self,
        res: &Resolved,
        name: &str,
        version: &str,
    ) -> Option<String> {
        let artifact = self
            .store
            .get_artifact(res.repo.id, &cargo_sparse_path(name))
            .await
            .ok()?;
        let (reader, _) = self.engine.blobs.open(&artifact.blob_sha256).await.ok()?;
        let mut body = Vec::new();
        reader
            .take(MAX_DOC_BYTES)
            .read_to_end(&mut body)
            .await
            .ok()?;
        index_checksum(&body, version)
    }
}

/// Whether the upstream credentials may accompany a request to `target`. They
/// are presented only to the index host, the same rule redirects follow.
pub(crate) fn same_host(index: &str, target: &str) -> bool {
    match (Url::parse(index), Url::parse(target)) {
        (Ok(a), Ok(b)) => {
            a.host_str() == b.host_str() && a.port_or_known_default() == b.port_or_known_default()
        }
        _ => false,
    }
}

async fn read_bounded(resp: reqwest::Response) -> Option<Vec<u8>> {
    if resp.content_length().is_some_and(|n| n > MAX_DOC_BYTES) {
        return None;
    }
    let body = resp.bytes().await.ok()?;
    (body.len() as u64 <= MAX_DOC_BYTES).then(|| body.to_vec())
}

/// Expands a `dl` template as cargo does: markers are substituted, and a
/// template with none of them gets `/{crate}/{version}/download` appended.
/// `None` when the template needs a checksum that is not known.
pub(crate) fn expand_dl(
    dl: &str,
    name: &str,
    version: &str,
    checksum: Option<&str>,
) -> Option<String> {
    if !MARKERS.iter().any(|m| dl.contains(m)) {
        return Some(format!(
            "{}/{name}/{version}/download",
            dl.trim_end_matches('/')
        ));
    }
    let mut url = dl
        .replace("{crate}", name)
        .replace("{version}", version)
        .replace("{prefix}", &prefix(name))
        .replace("{lowerprefix}", &prefix(&name.to_lowercase()));
    if url.contains("{sha256-checksum}") {
        url = url.replace("{sha256-checksum}", checksum?);
    }
    Some(url)
}

/// Cargo's directory prefix for a crate name: `1`, `2`, `3/{c}` or
/// `{cr}/{at}`, keeping the name's case.
fn prefix(name: &str) -> String {
    match name.len() {
        1 => "1".to_string(),
        2 => "2".to_string(),
        3 => format!("3/{}", &name[..1]),
        _ => format!("{}/{}", &name[..2], &name[2..4]),
    }
}

/// The `cksum` of `version` in a sparse-index entry file.
fn index_checksum(body: &[u8], version: &str) -> Option<String> {
    #[derive(serde::Deserialize)]
    struct Line {
        vers: String,
        cksum: String,
    }
    String::from_utf8_lossy(body)
        .lines()
        .filter_map(|line| serde_json::from_str::<Line>(line).ok())
        .find(|line| line.vers == version)
        .map(|line| line.cksum)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expand_dl_follows_cargo() {
        // crates.io: no markers, so cargo's default suffix.
        assert_eq!(
            expand_dl("https://static.crates.io/crates", "serde", "1.0.228", None).as_deref(),
            Some("https://static.crates.io/crates/serde/1.0.228/download")
        );
        assert_eq!(
            expand_dl("https://dl.example.com/", "serde", "1.0.0", None).as_deref(),
            Some("https://dl.example.com/serde/1.0.0/download")
        );
        // Every marker, prefixes included.
        assert_eq!(
            expand_dl(
                "https://dl.example.com/{prefix}/{lowerprefix}/{crate}-{version}.crate?c={sha256-checksum}",
                "Serde",
                "1.0.0",
                Some("abc")
            )
            .as_deref(),
            Some("https://dl.example.com/Se/rd/se/rd/Serde-1.0.0.crate?c=abc")
        );
        assert_eq!(
            expand_dl(
                "https://dl.example.com/{sha256-checksum}",
                "a",
                "1.0.0",
                None
            ),
            None
        );
    }

    #[test]
    fn prefixes_match_the_sparse_layout() {
        assert_eq!(prefix("a"), "1");
        assert_eq!(prefix("ab"), "2");
        assert_eq!(prefix("abc"), "3/a");
        assert_eq!(prefix("cargo"), "ca/rg");
    }

    #[test]
    fn index_checksum_picks_the_version() {
        let body = b"{\"name\":\"a\",\"vers\":\"1.0.0\",\"cksum\":\"one\"}\n{\"name\":\"a\",\"vers\":\"1.1.0\",\"cksum\":\"two\"}\n";
        assert_eq!(index_checksum(body, "1.1.0").as_deref(), Some("two"));
        assert_eq!(index_checksum(body, "9.9.9"), None);
    }

    #[test]
    fn credentials_stay_on_the_index_host() {
        assert!(same_host(
            "https://index.example.com",
            "https://index.example.com/crates/a"
        ));
        assert!(!same_host(
            "https://index.crates.io",
            "https://static.crates.io/crates/a"
        ));
        assert!(!same_host(
            "https://index.example.com",
            "https://index.example.com:8443/a"
        ));
        assert!(!same_host("not a url", "https://index.example.com/a"));
    }
}
