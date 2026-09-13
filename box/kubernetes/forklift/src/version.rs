/// Semantic version of this build, "dev" when not injected.
pub const VERSION: &str = match option_env!("FORKLIFT_VERSION") {
    Some(v) => v,
    None => "dev",
};

/// Git commit of this build, "none" when not injected.
pub const COMMIT: &str = match option_env!("FORKLIFT_COMMIT") {
    Some(v) => v,
    None => "none",
};

/// Rust toolchain the binary was built with, e.g. "rustc 1.98.1".
pub const RUSTC: &str = match option_env!("FORKLIFT_RUSTC_VERSION") {
    Some(v) => v,
    None => "rustc unknown",
};

/// Human-readable version string: `<version> (<commit>)`.
pub fn string() -> String {
    format!("{VERSION} ({COMMIT})")
}

/// The Rust toolchain the binary was built with.
pub fn rust() -> &'static str {
    RUSTC
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn string_combines_version_and_commit() {
        let s = string();
        assert!(s.starts_with(VERSION));
        assert!(s.ends_with(&format!("({COMMIT})")));
    }

    #[test]
    fn rust_reports_toolchain() {
        assert!(rust().starts_with("rustc"));
    }
}
