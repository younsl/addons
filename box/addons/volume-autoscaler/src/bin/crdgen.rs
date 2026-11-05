//! Prints a `CustomResourceDefinition` as JSON for `make crd` to render into
//! the chart. A build tool, never shipped in the image.

use std::io::Write as _;

use anyhow::Result;
use clap::Parser;
use volume_autoscaler::autoscaler::CrdKind;

#[derive(Parser)]
#[command(name = "crdgen", about = "Print a CustomResourceDefinition as JSON")]
struct Args {
    /// The custom resource to print
    #[arg(value_enum)]
    kind: CrdKind,
}

fn main() -> Result<()> {
    let args = Args::parse();
    let mut out = std::io::stdout().lock();
    serde_json::to_writer_pretty(&mut out, &args.kind.crd())?;
    writeln!(out)?;
    Ok(())
}
