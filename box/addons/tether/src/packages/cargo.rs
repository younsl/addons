//! Crates installed with `cargo install`, from `.crates.toml` in `CARGO_HOME`.

use std::fs;
use std::path::Path;

use super::brewfile::quote;

/// `cargo` lines in name order, with `source:` for git installs.
pub fn lines(cargo_home: &Path) -> Vec<String> {
    let Some(text) = fs::read_to_string(cargo_home.join(".crates.toml")).ok() else {
        return Vec::new();
    };
    let Ok(table) = text.parse::<toml::Table>() else {
        return Vec::new();
    };
    let Some(v1) = table.get("v1").and_then(toml::Value::as_table) else {
        return Vec::new();
    };

    let mut crates: Vec<(String, String, Option<String>)> = v1
        .keys()
        .filter_map(|key| {
            let (name, rest) = key.split_once(' ')?;
            let (version, source) = rest.split_once(" (")?;
            let source = source.strip_suffix(')')?;
            let git = source
                .strip_prefix("git+")
                .map(|url| url.split_once('#').map_or(url, |(url, _)| url).to_string());
            Some((name.to_string(), version.to_string(), git))
        })
        .collect();
    crates.sort();
    crates.dedup_by(|a, b| a.0 == b.0 && a.2 == b.2);

    crates
        .into_iter()
        .map(|(name, _, git)| {
            git.map_or_else(
                || format!("cargo {}", quote(&name)),
                |url| format!("cargo {}, source: {}", quote(&name), quote(&url)),
            )
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registry_path_and_git_sources() {
        let dir = tempfile::tempdir().expect("tempdir");
        fs::write(
            dir.path().join(".crates.toml"),
            r#"[v1]
"cargo-edit 0.13.13 (registry+https://github.com/rust-lang/crates.io-index)" = ["cargo-add"]
"vlt 0.1.0 (path+file:///Users/dev/vlt)" = ["vlt"]
"tool 0.2.0 (git+https://github.com/acme/tool?branch=main#abc123)" = ["tool"]
"cargo-audit 0.22.1 (registry+https://github.com/rust-lang/crates.io-index)" = ["cargo-audit"]
"#,
        )
        .expect("write");
        assert_eq!(
            lines(dir.path()),
            vec![
                "cargo \"cargo-audit\"",
                "cargo \"cargo-edit\"",
                "cargo \"tool\", source: \"https://github.com/acme/tool?branch=main\"",
                "cargo \"vlt\"",
            ]
        );
        assert_eq!(lines(Path::new("/nonexistent")).len(), 0);
    }

    #[test]
    fn malformed_file_is_empty() {
        let dir = tempfile::tempdir().expect("tempdir");
        fs::write(dir.path().join(".crates.toml"), "not = [toml").expect("write");
        assert_eq!(lines(dir.path()).len(), 0);
        fs::write(dir.path().join(".crates.toml"), "other = 1\n").expect("write");
        assert_eq!(lines(dir.path()).len(), 0);
    }
}
