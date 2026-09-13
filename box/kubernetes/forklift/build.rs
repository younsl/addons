//! Captures the rustc version at build time so the binary can log the toolchain
//! it was built with.
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
    println!("cargo:rerun-if-env-changed=FORKLIFT_VERSION");
    println!("cargo:rerun-if-env-changed=FORKLIFT_COMMIT");
}
