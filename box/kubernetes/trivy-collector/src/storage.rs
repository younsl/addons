//! Storage layer for Trivy Collector
//!
//! Report data lives in SQLite on an `emptyDir` owned by the scraper, which is
//! the only process that opens the file. Everything a human authored lives in
//! Kubernetes objects instead, because an `emptyDir` starts empty on every
//! restart and authored state cannot be regenerated:
//!
//! | State      | Home                                    |
//! | ---------- | --------------------------------------- |
//! | reports    | `database` (scraper-local SQLite)       |
//! | notes      | `notes` (ConfigMap, watched into memory)|
//! | api tokens | `token_store` (Secret, watched)         |
//!
//! # Module Structure
//! - `database`: connection and lifecycle management
//! - `models`: data types shared by both store implementations
//! - `schema`: schema initialization
//! - `operations`: CRUD and query operations
//! - `extractors`: JSON metadata extraction helpers
//! - `store`: the `ReportStore` trait the web tier depends on
//! - `remote`: `ReportStore` over the scraper's internal HTTP API
//! - `notes`: ConfigMap-backed notes
//! - `token_store`: Secret-backed API tokens

use std::collections::BTreeMap;

mod dashboard;
mod database;
mod extractors;
mod models;
pub mod notes;
mod operations;
pub mod remote;
mod schema;
pub mod store;
pub mod token_store;

// Re-export public types
pub use dashboard::{TrendDataPoint, TrendMeta, TrendResponse};
pub use database::Database;
pub use models::{
    ClusterInfo, ComponentSearchResult, FullReport, QueryParams, ReportMeta, SbomComponentMatch,
    Stats, TokenInfo, VulnSearchResult, VulnSummary,
};
pub use notes::{NotesError, NotesStore};
pub use remote::RemoteStore;
pub use store::{ClusterSync, HydrationStatus, PagedResponse, ReportStore};
pub use token_store::{TokenError, TokenStore, ValidatedToken};

pub const MANAGED_BY_LABEL: &str = "app.kubernetes.io/managed-by";
pub const COMPONENT_LABEL: &str = "app.kubernetes.io/component";

/// Labels stamped on every Kubernetes object this crate creates, so an
/// operator can tell app-managed state from hand-written manifests.
pub fn managed_labels(component: &str) -> BTreeMap<String, String> {
    BTreeMap::from([
        (MANAGED_BY_LABEL.to_string(), "trivy-collector".to_string()),
        (COMPONENT_LABEL.to_string(), component.to_string()),
    ])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn managed_labels_identify_the_owner_and_component() {
        let labels = managed_labels("trivy-collector-notes");
        assert_eq!(labels[MANAGED_BY_LABEL], "trivy-collector");
        assert_eq!(labels[COMPONENT_LABEL], "trivy-collector-notes");
    }
}
