//! Writes the CRDs into the Helm chart (`make crd`). A build tool, never
//! shipped in the image.

use std::path::Path;

use anyhow::{Context as _, Result};
use node_configuration_operator::crd::chart::templates;

fn main() -> Result<()> {
    for (relative, content) in templates() {
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join(&relative);
        std::fs::write(&path, content).with_context(|| format!("write {}", path.display()))?;
        eprintln!("wrote {relative}");
    }
    Ok(())
}
