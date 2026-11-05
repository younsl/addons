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
    // CI passes the SHA explicitly; local builds read it from git. Both fall
    // back to "unknown" so the build never fails on metadata.
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
    println!("cargo:rerun-if-changed=.git/HEAD");
}
