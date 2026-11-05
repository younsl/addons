//! Fails when a chart CRD no longer matches the Rust types it is generated
//! from. Fix with `make crd`.

use std::path::Path;

use serde_json::Value;
use volume_autoscaler::autoscaler::CrdKind;

const CRD_DIR: &str = "charts/volume-autoscaler/templates/crds";

/// Mirrors `collapse_descriptions` in scripts/render-crd.py.
fn collapse_descriptions(node: &mut Value) {
    match node {
        Value::Object(map) => {
            for (key, value) in map.iter_mut() {
                match value {
                    Value::String(s) if key == "description" => {
                        *s = s.split_whitespace().collect::<Vec<_>>().join(" ");
                    }
                    _ => collapse_descriptions(value),
                }
            }
        }
        Value::Array(items) => items.iter_mut().for_each(collapse_descriptions),
        _ => {}
    }
}

/// The `spec` of a rendered chart CRD. The script writes it verbatim between
/// the Helm-owned metadata and the closing `{{- end }}`.
fn chart_spec(rendered: &str) -> Value {
    let start = rendered.find("\nspec:\n").expect("a top-level spec") + 1;
    let end = rendered.rfind("{{- end }}").expect("the closing end");
    let doc: Value = serde_yaml::from_str(&rendered[start..end]).expect("valid YAML");
    doc["spec"].clone()
}

#[test]
fn chart_crds_match_the_rust_types() {
    for kind in CrdKind::ALL {
        let mut generated = serde_json::to_value(kind.crd()).unwrap();
        collapse_descriptions(&mut generated);
        let name = generated["metadata"]["name"].as_str().unwrap().to_owned();
        let path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join(CRD_DIR)
            .join(format!("{name}.yaml"));
        let rendered = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
        assert!(
            chart_spec(&rendered) == generated["spec"],
            "{} is out of date, run `make crd`",
            path.display()
        );
    }
}
