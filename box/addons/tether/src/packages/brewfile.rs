//! Brewfile in `brew bundle dump` order, with a package count footer.

use std::collections::HashSet;
use std::fmt::Write as _;
use std::io;
use std::path::Path;

use super::{brew, cargo, go, krew, npm};
use crate::spec::Packages;

/// A Ruby string literal, as `String#inspect` writes simple names.
pub fn quote(value: &str) -> String {
    let mut out = String::with_capacity(value.len() + 2);
    out.push('"');
    let mut chars = value.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\t' => out.push_str("\\t"),
            '#' if matches!(chars.peek(), Some('{' | '$' | '@')) => out.push_str("\\#"),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

pub fn quote_list(items: &[String]) -> String {
    let quoted: Vec<String> = items.iter().map(|i| quote(i)).collect();
    format!("[{}]", quoted.join(", "))
}

/// Generate the Brewfile. Fails when the Homebrew prefix is not mounted, so an
/// empty listing never overwrites the real file.
pub fn generate(packages: &Packages, home: &Path, date: &str) -> io::Result<String> {
    let prefix = &packages.homebrew_prefix;
    if !prefix.join("Cellar").is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!(
                "{} has no Cellar, mount the Homebrew prefix",
                prefix.display()
            ),
        ));
    }
    let trust = brew::Trust::load(&packages.trust_file);
    let api = brew::Api::load(&packages.homebrew_cache);
    let ctx = brew::Context {
        prefix,
        home,
        launch_agents: &packages.launch_agents,
        trust: &trust,
        api: &api,
    };

    let formulae = brew::installed_formulae(prefix, &trust);
    let (brews, brew_names) = brew::brew_lines(&ctx, &formulae);
    let (casks, cask_names) = brew::cask_lines(&ctx);
    let dumped: HashSet<String> = brew_names.into_iter().chain(cask_names).collect();
    let taps = brew::tap_lines(prefix, &trust, &dumped);

    let simple = |kind: &str, names: Vec<String>| -> Vec<String> {
        names
            .iter()
            .map(|n| format!("{kind} {}", quote(n)))
            .collect()
    };
    let sections = [
        taps,
        brews,
        casks,
        simple("go", go::packages(&packages.go_bin)),
        cargo::lines(&packages.cargo_home),
        simple(
            "krew",
            krew::plugins(&packages.krew_root).unwrap_or_default(),
        ),
        simple(
            "npm",
            packages
                .npm_prefix
                .as_deref()
                .map(npm::packages)
                .unwrap_or_default(),
        ),
    ];
    let lines: Vec<String> = sections.into_iter().flatten().collect();
    let mut body = lines.join("\n");
    body.push('\n');
    body.push_str(&footer(&body, date));
    Ok(body)
}

fn footer(body: &str, date: &str) -> String {
    let mut out =
        String::from("#-------------------------------\n# PACKAGE INSTALLATION SUMMARY\n");
    let _ = write!(out, "# Date: {date}\n#-------------------------------\n");
    for section in ["tap", "brew", "cask", "mas", "vscode"] {
        let prefix = format!("{section} ");
        let count = body.lines().filter(|l| l.starts_with(&prefix)).count();
        let _ = writeln!(out, "# Installed {section} count: {count}");
    }
    out.push_str("#-------------------------------\n");
    out
}

pub fn count_entries(text: &str) -> usize {
    text.lines()
        .filter(|l| !l.starts_with('#') && !l.trim().is_empty())
        .count()
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::PathBuf;

    use super::*;

    #[test]
    fn quote_escapes_like_ruby_inspect() {
        assert_eq!(quote("jq"), "\"jq\"");
        assert_eq!(quote("a\"b\\c"), "\"a\\\"b\\\\c\"");
        assert_eq!(quote("#{x} #y"), "\"\\#{x} #y\"");
        assert_eq!(quote("a\nb\t"), "\"a\\nb\\t\"");
        assert_eq!(quote_list(&["a".into(), "b".into()]), "[\"a\", \"b\"]");
    }

    fn packages(root: &Path) -> Packages {
        Packages {
            brewfile: None,
            krewfile: None,
            homebrew_prefix: root.join("brew"),
            homebrew_cache: root.join("cache"),
            trust_file: root.join("trust.json"),
            launch_agents: root.join("agents"),
            krew_root: root.join("krew"),
            cargo_home: root.join("cargo"),
            go_bin: root.join("go/bin"),
            npm_prefix: Some(root.join("node")),
        }
    }

    #[test]
    fn refuses_without_homebrew_prefix() {
        let dir = tempfile::tempdir().expect("tempdir");
        let err = generate(&packages(dir.path()), dir.path(), "2026-01-01").expect_err("no prefix");
        assert_eq!(err.kind(), io::ErrorKind::NotFound);
    }

    #[test]
    fn sections_in_dump_order_with_footer() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        let keg = root.join("brew/Cellar/jq/1.7");
        fs::create_dir_all(&keg).expect("keg");
        fs::write(
            keg.join("INSTALL_RECEIPT.json"),
            r#"{"installed_on_request":true}"#,
        )
        .expect("receipt");
        let linked = root.join("brew/var/homebrew/linked");
        fs::create_dir_all(&linked).expect("linked");
        std::os::unix::fs::symlink(&keg, linked.join("jq")).expect("link");
        fs::create_dir_all(root.join("brew/Library/Taps/acme/homebrew-tap")).expect("tap");
        fs::create_dir_all(root.join("krew/receipts")).expect("krew");
        fs::write(
            root.join("krew/receipts/stern.yaml"),
            "status:\n  source:\n    name: default\n",
        )
        .expect("receipt");
        fs::create_dir_all(root.join("cargo")).expect("cargo");
        fs::write(
            root.join("cargo/.crates.toml"),
            "[v1]\n\"vlt 0.1.0 (path+file:///x)\" = [\"vlt\"]\n",
        )
        .expect("crates");
        fs::create_dir_all(root.join("node/lib/node_modules/corepack")).expect("npm");

        let text =
            generate(&packages(root), &PathBuf::from("/home/dev"), "2026-01-02").expect("generate");
        assert_eq!(
            text,
            "tap \"acme/tap\"\nbrew \"jq\"\ncargo \"vlt\"\nkrew \"stern\"\nnpm \"corepack\"\n\
#-------------------------------\n# PACKAGE INSTALLATION SUMMARY\n# Date: 2026-01-02\n\
#-------------------------------\n# Installed tap count: 1\n# Installed brew count: 1\n\
# Installed cask count: 0\n# Installed mas count: 0\n# Installed vscode count: 0\n\
#-------------------------------\n"
        );
        assert_eq!(count_entries(&text), 5);
    }
}
