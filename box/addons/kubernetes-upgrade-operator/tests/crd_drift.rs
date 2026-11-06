//! Fails when the chart CRD no longer matches the Rust types it is generated
//! from. Fix with `make crd`.

use std::path::Path;

use kuo::crd::chart::{CHART_CRD_PATH, render};

#[test]
fn chart_crd_matches_the_rust_types() {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join(CHART_CRD_PATH);
    let on_disk =
        std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
    assert!(
        on_disk == render(),
        "{CHART_CRD_PATH} is out of date, run `make crd`"
    );
}
