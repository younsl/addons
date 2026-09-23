//! Captures the rustc version at build time so the binary can log the toolchain
//! it was built with, and keeps the embedded web console in step with
//! `make web-build`.
use std::process::Command;

fn main() {
    let rustc = std::env::var("RUSTC").unwrap_or_else(|_| "rustc".into());
    let out = Command::new(rustc).arg("--version").output();
    let v = out
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_else(|| "rustc unknown".into());
    println!("cargo:rustc-env=FORKLIFT_RUSTC_VERSION={v}");
    // The web console build output is not committed. rust-embed needs the
    // folder to exist, and a missing rerun path would rebuild every time, so an
    // empty one is created, and watching it recompiles the binary after each
    // `make web-build`.
    let dist = std::path::Path::new("src/webui/dist");
    if let Err(e) = std::fs::create_dir_all(dist) {
        println!("cargo:warning=cannot create {}: {e}", dist.display());
    }
    println!("cargo:rerun-if-changed=src/webui/dist");
    println!("cargo:rerun-if-env-changed=FORKLIFT_VERSION");
    println!("cargo:rerun-if-env-changed=FORKLIFT_COMMIT");
}
