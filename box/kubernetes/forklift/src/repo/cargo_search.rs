//! Serves `cargo search` (`GET api/v1/crates?q=<query>&per_page=<n>`) for a
//! hosted Cargo repository from its managed publications.
//!
//! Matching is on the crate name only, ignoring case and hyphen/underscore
//! differences, and every whitespace-separated term must occur in the name.
//! Crates whose every version is yanked are left out. Proxy repositories answer
//! `404`, so a group falls through to its hosted member.

use std::collections::BTreeMap;

use axum::response::{IntoResponse, Response};
use http::StatusCode;
use http::request::Parts;

use crate::meta;

use super::Manager;
use super::cargo::cargo_api_error;
use super::router::Resolved;
use super::uiupload_cargo::{cargo_canonical_name, cargo_description};

/// Results per page when cargo sends no `per_page` (cargo's own default).
const DEFAULT_PER_PAGE: usize = 10;
/// The largest page cargo can request (`cargo search --limit` caps at 100).
const MAX_PER_PAGE: usize = 100;

/// One crate in the result: its display name and highest non-yanked version.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CrateHit {
    pub(crate) name: String,
    pub(crate) max_version: String,
}

impl Manager {
    pub(crate) async fn cargo_search(&self, parts: &Parts, res: &Resolved) -> Response {
        if res.repo.r#type != meta::TYPE_HOSTED {
            return cargo_api_error(
                StatusCode::NOT_FOUND,
                "search is only available on hosted repositories",
            );
        }
        let (terms, per_page) = search_params(parts.uri.query().unwrap_or(""));
        let Ok(publications) = self.store.list_artifact_publications(res.repo.id).await else {
            return cargo_api_error(
                StatusCode::SERVICE_UNAVAILABLE,
                "crates could not be listed",
            );
        };
        let hits = rank_crates(&publications, &terms);
        let mut crates = Vec::new();
        for hit in hits.iter().take(per_page) {
            // Descriptions live on the crate artifact, so only the page shown
            // is looked up.
            let path = format!("api/v1/crates/{}/{}/download", hit.name, hit.max_version);
            let description = self
                .store
                .get_artifact(res.repo.id, &path)
                .await
                .map(|artifact| cargo_description(&artifact.metadata_json))
                .unwrap_or_default();
            crates.push(crate_summary_body(hit, &description));
        }
        axum::Json(search_body(crates, hits.len())).into_response()
    }
}

/// One search result (`CargoCrateSummary` in the OpenAPI document).
pub(crate) fn crate_summary_body(hit: &CrateHit, description: &str) -> serde_json::Value {
    serde_json::json!({
        "name": hit.name,
        "max_version": hit.max_version,
        "description": description,
    })
}

/// The search response (`CargoSearchResult`).
pub(crate) fn search_body(crates: Vec<serde_json::Value>, total: usize) -> serde_json::Value {
    serde_json::json!({
        "crates": crates,
        "meta": {"total": total},
    })
}

/// Reads the canonical search terms and the page size from the query string.
fn search_params(query: &str) -> (Vec<String>, usize) {
    let mut terms = Vec::new();
    let mut per_page = DEFAULT_PER_PAGE;
    for (key, value) in form_urlencoded::parse(query.as_bytes()) {
        match key.as_ref() {
            "q" => {
                terms = value.split_whitespace().map(cargo_canonical_name).collect();
            }
            "per_page" => {
                if let Ok(n) = value.parse::<usize>() {
                    per_page = n.clamp(1, MAX_PER_PAGE);
                }
            }
            _ => {}
        }
    }
    (terms, per_page)
}

/// Groups Cargo publications into crates, keeps those whose name contains
/// every term, and orders them: an exact name match first, then names that
/// start with the first term, then the rest, each group alphabetically.
pub(crate) fn rank_crates(
    publications: &[meta::ArtifactPublication],
    terms: &[String],
) -> Vec<CrateHit> {
    // canonical name -> (display name, highest non-yanked version)
    let mut crates: BTreeMap<String, (String, semver::Version)> = BTreeMap::new();
    for publication in publications {
        if publication.format != meta::FORMAT_CARGO || publication.yanked {
            continue;
        }
        // The coordinate keeps the crate's own spelling and the full version,
        // build metadata included, which is how the download path names it.
        let Some((name, version)) = publication.coordinate.rsplit_once('@') else {
            continue;
        };
        let Ok(version) = semver::Version::parse(version) else {
            continue;
        };
        let canonical = cargo_canonical_name(name);
        if !terms.iter().all(|term| canonical.contains(term.as_str())) {
            continue;
        }
        match crates.get_mut(&canonical) {
            Some(entry) if entry.1 >= version => {}
            Some(entry) => *entry = (name.to_string(), version),
            None => {
                crates.insert(canonical, (name.to_string(), version));
            }
        }
    }
    let exact = terms.join("-");
    let first = terms.first().map(String::as_str).unwrap_or("");
    let mut hits: Vec<(u8, String, CrateHit)> = crates
        .into_iter()
        .map(|(canonical, (name, version))| {
            let score = if !terms.is_empty() && canonical == exact {
                0
            } else if canonical.starts_with(first) {
                1
            } else {
                2
            };
            (
                score,
                canonical,
                CrateHit {
                    name,
                    max_version: version.to_string(),
                },
            )
        })
        .collect();
    hits.sort_by(|a, b| (a.0, &a.1).cmp(&(b.0, &b.1)));
    hits.into_iter().map(|(_, _, hit)| hit).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn publication(coordinate: &str, yanked: bool) -> meta::ArtifactPublication {
        meta::ArtifactPublication {
            format: meta::FORMAT_CARGO.to_string(),
            coordinate: coordinate.to_string(),
            yanked,
            ..Default::default()
        }
    }

    fn names(hits: &[CrateHit]) -> Vec<(&str, &str)> {
        hits.iter()
            .map(|hit| (hit.name.as_str(), hit.max_version.as_str()))
            .collect()
    }

    #[test]
    fn search_params_defaults_and_clamps() {
        assert_eq!(search_params(""), (Vec::new(), DEFAULT_PER_PAGE));
        assert_eq!(
            search_params("q=Serde_JSON+macros&per_page=500"),
            (
                vec!["serde-json".to_string(), "macros".to_string()],
                MAX_PER_PAGE
            )
        );
        assert_eq!(search_params("per_page=0").1, 1);
        assert_eq!(search_params("per_page=abc").1, DEFAULT_PER_PAGE);
    }

    #[test]
    fn rank_crates_orders_and_filters() {
        let publications = vec![
            publication("fl-widget@0.1.0", false),
            publication("fl-widget@0.2.0+build.7", false),
            publication("fl-widget@0.3.0", true),
            publication("widget@1.0.0", false),
            publication("widget-macros@1.0.0", false),
            publication("old-widget@1.0.0", true),
            publication("unrelated@1.0.0", false),
            publication("broken", false),
            meta::ArtifactPublication {
                format: meta::FORMAT_NPM.to_string(),
                coordinate: "widget-npm@1.0.0".to_string(),
                ..Default::default()
            },
        ];
        let terms = vec!["widget".to_string()];
        assert_eq!(
            names(&rank_crates(&publications, &terms)),
            vec![
                ("widget", "1.0.0"),
                ("widget-macros", "1.0.0"),
                ("fl-widget", "0.2.0+build.7"),
            ]
        );
        // Every term must match, and separators fold.
        let terms = vec!["widget".to_string(), "macros".to_string()];
        assert_eq!(
            names(&rank_crates(&publications, &terms)),
            vec![("widget-macros", "1.0.0")]
        );
        // No terms lists every live crate alphabetically.
        assert_eq!(rank_crates(&publications, &[]).len(), 4);
    }
}
