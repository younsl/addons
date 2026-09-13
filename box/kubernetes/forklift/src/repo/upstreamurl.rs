//! Resolves the upstream URL for a package coordinate.

use crate::meta::{FORMAT_CARGO, FORMAT_GO, FORMAT_MAVEN, FORMAT_NPM, FORMAT_OCI, FORMAT_PYPI};

/// Resolves the full upstream URL for a package coordinate (the canonical
/// per-format package string used by approvals), so a reviewer can open the
/// exact source a proxy would fetch from. Returns the bare upstream URL when the
/// format has no addressable package path.
pub fn upstream_package_url(format: &str, upstream_url: &str, pkg: &str) -> String {
    let up = upstream_url.trim_end_matches('/');
    if up.is_empty() || pkg.is_empty() {
        return up.to_string();
    }
    match format {
        FORMAT_NPM => format!("{up}/{pkg}"),
        FORMAT_MAVEN => {
            // "group:artifact" -> group dots become path segments.
            let Some((group, artifact)) = pkg.split_once(':') else {
                return up.to_string();
            };
            format!("{up}/{}/{artifact}/", group.replace('.', "/"))
        }
        FORMAT_GO => format!("{up}/{pkg}/@v/list"),
        FORMAT_PYPI => {
            // PEP 503: names are normalized to lowercase with runs of -_. as "-".
            let mut name = pkg.to_lowercase().replace(['_', '.'], "-");
            while name.contains("--") {
                name = name.replace("--", "-");
            }
            format!("{up}/{name}/")
        }
        // The approval coordinate is the OCI name; the upstream's tag-list
        // endpoint is the closest addressable page for a reviewer.
        FORMAT_OCI => format!("{up}/v2/{pkg}/tags/list"),
        FORMAT_CARGO => {
            // Sparse-index layout: 1/{c}, 2/{c}, 3/{first}/{c}, {c0c1}/{c2c3}/{c}.
            let c = pkg.to_lowercase();
            match c.len() {
                0 => up.to_string(),
                1 => format!("{up}/1/{c}"),
                2 => format!("{up}/2/{c}"),
                3 => format!("{up}/3/{}/{c}", &c[..1]),
                _ => format!("{up}/{}/{}/{c}", &c[..2], &c[2..4]),
            }
        }
        _ => up.to_string(),
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use crate::meta::{FORMAT_CARGO, FORMAT_GO, FORMAT_MAVEN, FORMAT_NPM, FORMAT_OCI, FORMAT_PYPI};

    use crate::repo::uiupload_cargo::cargo_canonical_name;
    use crate::repo::uiupload_npm::valid_semver_identifiers;
    use crate::repo::upstream_package_url;

    /// `upstream_package_url` is the link a reviewer follows out of an approval
    /// request, so each format's layout has to be right: a wrong URL sends a
    /// security reviewer to a 404 while they are deciding whether to allow a
    /// package.
    #[test]
    fn upstream_package_url_per_format() {
        let cases: Vec<(&str, &str, &str, &str, &str)> = vec![
            (
                "npm",
                FORMAT_NPM,
                "https://registry.npmjs.org/",
                "lodash",
                "https://registry.npmjs.org/lodash",
            ),
            (
                "npm scoped",
                FORMAT_NPM,
                "https://registry.npmjs.org",
                "@scope/pkg",
                "https://registry.npmjs.org/@scope/pkg",
            ),
            (
                "maven",
                FORMAT_MAVEN,
                "https://repo1.maven.org/maven2",
                "com.google.guava:guava",
                "https://repo1.maven.org/maven2/com/google/guava/guava/",
            ),
            (
                "maven without a separator",
                FORMAT_MAVEN,
                "https://repo1.maven.org/maven2",
                "guava",
                "https://repo1.maven.org/maven2",
            ),
            (
                "go",
                FORMAT_GO,
                "https://proxy.golang.org",
                "github.com/pkg/errors",
                "https://proxy.golang.org/github.com/pkg/errors/@v/list",
            ),
            // PEP 503 normalisation: lowercase, runs of -_. collapse to one hyphen.
            (
                "pypi",
                FORMAT_PYPI,
                "https://pypi.org/simple",
                "Flask",
                "https://pypi.org/simple/flask/",
            ),
            (
                "pypi normalised",
                FORMAT_PYPI,
                "https://pypi.org/simple",
                "zope.interface_extra",
                "https://pypi.org/simple/zope-interface-extra/",
            ),
            (
                "oci",
                FORMAT_OCI,
                "https://ghcr.io",
                "younsl/forklift",
                "https://ghcr.io/v2/younsl/forklift/tags/list",
            ),
            // Cargo's sparse index buckets by name length.
            (
                "cargo 1",
                FORMAT_CARGO,
                "https://index.crates.io",
                "a",
                "https://index.crates.io/1/a",
            ),
            (
                "cargo 2",
                FORMAT_CARGO,
                "https://index.crates.io",
                "ab",
                "https://index.crates.io/2/ab",
            ),
            (
                "cargo 3",
                FORMAT_CARGO,
                "https://index.crates.io",
                "abc",
                "https://index.crates.io/3/a/abc",
            ),
            (
                "cargo 4",
                FORMAT_CARGO,
                "https://index.crates.io",
                "Serde",
                "https://index.crates.io/se/rd/serde",
            ),
            (
                "cargo empty name",
                FORMAT_CARGO,
                "https://index.crates.io",
                "",
                "https://index.crates.io",
            ),
            (
                "unknown format",
                "conda",
                "https://example.invalid",
                "pkg",
                "https://example.invalid",
            ),
            ("no upstream", FORMAT_NPM, "", "lodash", ""),
        ];
        for (name, format, upstream, pkg, want) in cases {
            assert_eq!(
                upstream_package_url(format, upstream, pkg),
                want,
                "upstream_package_url({name})"
            );
        }
    }

    #[test]
    fn cargo_canonical_name_collapses_case_and_separators() {
        for (input, want) in [
            ("serde", "serde"),
            ("Serde", "serde"),
            ("serde_json", "serde-json"),
            ("SERDE_JSON", "serde-json"),
            ("serde-json", "serde-json"),
        ] {
            assert_eq!(
                cargo_canonical_name(input),
                want,
                "cargo_canonical_name({input:?})"
            );
        }
        // Names differing only by case or separator must collapse to one identity,
        // or two crates could occupy what cargo considers the same name.
        assert_eq!(
            cargo_canonical_name("Serde_JSON"),
            cargo_canonical_name("serde-json"),
            "case and separator variants are not the same canonical crate"
        );
    }

    /// The npm prerelease and build identifier rules decide which versions may be
    /// published, so both the accepted shapes and the rejections are pinned.
    #[test]
    fn valid_semver_identifiers_cases() {
        for (value, reject_leading_zero, want) in [
            ("alpha", true, true),
            ("alpha.1", true, true),
            ("0.3.7", true, true),
            ("x-y-z.42", true, true),
            ("", true, false),
            ("alpha..1", true, false),
            ("alpha_1", true, false),
            ("alpha+1", true, false),
            // A leading zero is rejected only where semver forbids it.
            ("01", true, false),
            ("01", false, true),
            ("0", true, true),
            ("0a", true, true),
        ] {
            assert_eq!(
                valid_semver_identifiers(value, reject_leading_zero),
                want,
                "valid_semver_identifiers({value:?}, reject_leading_zero={reject_leading_zero})"
            );
        }
    }
}
