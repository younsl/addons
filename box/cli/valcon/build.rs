use std::path::Path;
use std::process::Command;

fn git(args: &[&str]) -> Option<String> {
    let out = Command::new("git").args(args).output().ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8(out.stdout).ok()?.trim().to_owned();
    (!s.is_empty()).then_some(s)
}

fn rerun_if_exists(path: Option<String>) {
    if let Some(p) = path
        && Path::new(&p).exists()
    {
        println!("cargo:rerun-if-changed={p}");
    }
}

fn main() {
    println!("cargo:rerun-if-changed=build.rs");

    let commit = git(&["rev-parse", "--short", "HEAD"]).unwrap_or_else(|| "unknown".into());
    let date = Command::new("date")
        .args(["-u", "+%Y-%m-%dT%H:%M:%SZ"])
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "unknown".into());

    println!("cargo:rustc-env=BUILD_COMMIT={commit}");
    println!("cargo:rustc-env=BUILD_DATE={date}");

    rerun_if_exists(git(&[
        "rev-parse",
        "--path-format=absolute",
        "--git-path",
        "HEAD",
    ]));
    if let Some(head_ref) = git(&["symbolic-ref", "-q", "HEAD"]) {
        rerun_if_exists(git(&[
            "rev-parse",
            "--path-format=absolute",
            "--git-path",
            &head_ref,
        ]));
    }
}
