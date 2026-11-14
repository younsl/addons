//! Shared metadata-store test harness.

use crate::meta::Store;

/// A fresh temp-backed store. The `TempDir` must outlive the store.
pub async fn test_store() -> (Store, tempfile::TempDir) {
    Store::open_temp().await.expect("open temp store")
}
