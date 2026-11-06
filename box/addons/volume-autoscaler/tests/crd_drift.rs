//! Fails when a chart CRD no longer matches the Rust types it is generated
//! from. Fix with `make crd`.

use std::path::Path;

use volume_autoscaler::autoscaler::CrdKind;
use volume_autoscaler::autoscaler::chart::{chart_path, render};

#[test]
fn chart_crds_match_the_rust_types() {
    for kind in CrdKind::ALL {
        let rel = chart_path(kind);
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join(&rel);
        let on_disk = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
        assert!(
            on_disk == render(kind),
            "{rel} is out of date, run `make crd`"
        );
    }
}
