//! Keeps the embedded web console in step with `make web-build`.

fn main() {
    // The web console build output is not committed. rust-embed needs the
    // folder to exist, and a missing rerun path would rebuild every time, so an
    // empty one is created, and watching it recompiles the binary after each
    // `make web-build`.
    let dist = std::path::Path::new("src/webui/dist");
    if let Err(e) = std::fs::create_dir_all(dist) {
        println!("cargo:warning=cannot create {}: {e}", dist.display());
    }
    println!("cargo:rerun-if-changed=src/webui/dist");
}
