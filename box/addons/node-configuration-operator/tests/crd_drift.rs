//! Fails when a chart CRD no longer matches the Rust types it is generated
//! from. Fix with `make crd`.

use std::path::Path;

use node_configuration_operator::crd::chart::templates;

#[test]
fn chart_crds_match_the_rust_types() {
    for (relative, content) in templates() {
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join(&relative);
        let on_disk = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
        assert!(
            on_disk == content,
            "{relative} is out of date, run `make crd`"
        );
    }
}
