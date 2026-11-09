//! Writes the CRD into the Helm chart (`make crd`). A build tool, never
//! shipped in the image.

use std::path::Path;

use anyhow::{Context as _, Result};
use kagent_gateway::crd::chart::{CHART_PATH, render};

fn main() -> Result<()> {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join(CHART_PATH);
    std::fs::write(&path, render()).with_context(|| format!("write {}", path.display()))?;
    eprintln!("wrote {CHART_PATH}");
    Ok(())
}
