//! Writes every CRD into the Helm chart (`make crd`). A build tool, never
//! shipped in the image.

use std::path::Path;

use anyhow::{Context as _, Result};
use ward::crd::CrdKind;
use ward::crd::chart::{chart_path, render};

fn main() -> Result<()> {
    for kind in CrdKind::ALL {
        let rel = chart_path(kind);
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join(&rel);
        std::fs::write(&path, render(kind)).with_context(|| format!("write {}", path.display()))?;
        eprintln!("wrote {rel}");
    }
    Ok(())
}
