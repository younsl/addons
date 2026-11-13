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

fn rerun_if_exists(path: Option<String>) {
    if let Some(path) = path.filter(|p| Path::new(p).exists()) {
        println!("cargo:rerun-if-changed={path}");
    }
}

fn main() {
    let commit = std::env::var("VERGEN_GIT_SHA")
        .ok()
        .map(|s| s.chars().take(7).collect())
        .or_else(|| run("git", &["rev-parse", "--short", "HEAD"]))
        .unwrap_or_else(|| "unknown".to_string());
    let date = run("date", &["-u", "+%Y-%m-%d"]).unwrap_or_else(|| "unknown".to_string());
    let rustc = std::env::var("RUSTC").unwrap_or_else(|_| "rustc".to_string());
    let rustc_version = run(&rustc, &["--version"])
        .and_then(|s| s.split_whitespace().nth(1).map(str::to_string))
        .unwrap_or_else(|| "unknown".to_string());

    println!("cargo:rustc-env=BUILD_COMMIT={commit}");
    println!("cargo:rustc-env=BUILD_DATE={date}");
    println!("cargo:rustc-env=BUILD_RUSTC_VERSION={rustc_version}");
    println!("cargo:rerun-if-env-changed=VERGEN_GIT_SHA");

    let git_path = |name: &str| {
        run(
            "git",
            &["rev-parse", "--path-format=absolute", "--git-path", name],
        )
    };
    rerun_if_exists(git_path("HEAD"));
    rerun_if_exists(run("git", &["symbolic-ref", "-q", "HEAD"]).and_then(|r| git_path(&r)));
}
