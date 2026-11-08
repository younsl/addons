use std::path::Path;
use std::process::Command;

fn run(cmd: &str, args: &[&str]) -> Option<String> {
    Command::new(cmd)
        .args(args)
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

fn main() {
    let commit = std::env::var("VERGEN_GIT_SHA")
        .ok()
        .map(|s| s.chars().take(7).collect())
        .or_else(|| run("git", &["rev-parse", "--short", "HEAD"]))
        .unwrap_or_else(|| "unknown".to_string());
    let date = run("date", &["-u", "+%Y-%m-%d"]).unwrap_or_else(|| "unknown".to_string());

    println!("cargo:rustc-env=BUILD_COMMIT={commit}");
    println!("cargo:rustc-env=BUILD_DATE={date}");
    println!("cargo:rerun-if-env-changed=VERGEN_GIT_SHA");

    let head = run(
        "git",
        &["rev-parse", "--path-format=absolute", "--git-path", "HEAD"],
    );
    let branch = run("git", &["symbolic-ref", "-q", "HEAD"]).and_then(|r| {
        run(
            "git",
            &["rev-parse", "--path-format=absolute", "--git-path", &r],
        )
    });
    embed_ebpf();

    for path in [head, branch].into_iter().flatten() {
        if Path::new(&path).exists() {
            println!("cargo:rerun-if-changed={path}");
        }
    }
}

/// Copies the eBPF object for the target architecture into `OUT_DIR`, or an
/// empty file when it was not built, which makes the agent poll `/proc`.
fn embed_ebpf() {
    let out = Path::new(&std::env::var("OUT_DIR").unwrap_or_default()).join("ward.bpf.o");
    let arch = std::env::var("CARGO_CFG_TARGET_ARCH").unwrap_or_default();
    let src = std::env::var("WARD_EBPF_OBJECT")
        .unwrap_or_else(|_| format!("ebpf/target/{arch}/bpfel-unknown-none/release/ward"));
    println!("cargo:rerun-if-env-changed=WARD_EBPF_OBJECT");
    println!("cargo:rerun-if-changed={src}");
    let bytes = std::fs::read(&src).unwrap_or_default();
    if bytes.is_empty() {
        println!("cargo:warning=no eBPF object at {src}, the agent will poll /proc");
    }
    let _ = std::fs::write(out, bytes);
}
