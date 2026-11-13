//! Every manifest under `examples/` parses and passes the controller checks.

use node_configuration_common::crd::{NodeConfiguration, validate};
use serde::Deserialize as _;

#[test]
fn examples_are_valid() {
    let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/../examples");
    let mut seen = 0;
    for entry in std::fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        let text = std::fs::read_to_string(&path).unwrap();
        for document in serde_yaml::Deserializer::from_str(&text) {
            let obj = NodeConfiguration::deserialize(document)
                .unwrap_or_else(|e| panic!("{}: {e}", path.display()));
            let name = obj.metadata.name.clone().unwrap_or_default();
            assert_eq!(
                validate::problems(&name, &obj.spec),
                Vec::<String>::new(),
                "{} {name}",
                path.display()
            );
            assert!(!obj.spec.rules.is_empty(), "{} {name}", path.display());
            seen += 1;
        }
    }
    assert!(seen >= 4, "only {seen} examples found");
}
