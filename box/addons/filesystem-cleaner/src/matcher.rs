//! Include/exclude glob matching for paths relative to a target path.
//!
//! The semantics are those of the `globset` crate with its default settings
//! (`literal_separator = false`), which the tool has always shipped with and
//! which deployed patterns rely on:
//!   - `*` and `?` match any characters, including `/`
//!   - `**` as a full path component matches zero or more components
//!     (leading `**/`, trailing `/**`, middle `/**/`, or bare `**`)
//!   - character classes like `[abc]` and negation `[!abc]` are supported
//!   - brace alternation like `{a,b}` is supported
//!   - `\` escapes the next character
//!
//! Do not switch to `literal_separator = true` or a `path.Match`-style
//! matcher: exclude patterns like `*.log` must match at any depth.

use globset::{Glob, GlobSet, GlobSetBuilder};

use crate::error::PatternError;

/// Decides whether a relative path should be included in or excluded from a
/// cleanup run.
#[derive(Debug, Clone)]
pub struct Matcher {
    include: GlobSet,
    exclude: GlobSet,
}

impl Matcher {
    /// Compiles include and exclude glob patterns.
    pub fn new<I, E>(include: &[I], exclude: &[E]) -> Result<Self, PatternError>
    where
        I: AsRef<str>,
        E: AsRef<str>,
    {
        Ok(Self {
            include: compile(include).map_err(PatternError::Include)?,
            exclude: compile(exclude).map_err(PatternError::Exclude)?,
        })
    }

    /// Reports whether the path matches any include pattern.
    pub fn should_include(&self, rel: &str) -> bool {
        self.include.is_match(rel)
    }

    /// Reports whether the path matches any exclude pattern.
    pub fn should_exclude(&self, rel: &str) -> bool {
        self.exclude.is_match(rel)
    }
}

fn compile<S: AsRef<str>>(patterns: &[S]) -> Result<GlobSet, globset::Error> {
    let mut builder = GlobSetBuilder::new();
    for pattern in patterns {
        builder.add(Glob::new(pattern.as_ref())?);
    }
    builder.build()
}

#[cfg(test)]
mod tests {
    use super::Matcher;
    use crate::error::PatternError;

    const NONE: &[&str] = &[];

    fn matcher(include: &[&str], exclude: &[&str]) -> Matcher {
        Matcher::new(include, exclude).expect("patterns compile")
    }

    #[test]
    fn exclude_patterns() {
        let m = matcher(&["*"], &["**/.git/**", "**/node_modules/**"]);
        assert!(m.should_exclude("project1/.git/config"));
        assert!(m.should_exclude("src/node_modules/lib.js"));
        assert!(!m.should_exclude("src/main.rs"));
    }

    #[test]
    fn exclude_at_root() {
        let m = matcher(&["*"], &["**/.git/**"]);
        assert!(m.should_exclude(".git/config"));
    }

    #[test]
    fn include_patterns() {
        let m = matcher(&["*.txt"], NONE);
        assert!(m.should_include("file.txt"));
        assert!(m.should_include("readme.txt"));
        assert!(!m.should_include("file.rs"));
    }

    #[test]
    fn empty_include_matches_nothing() {
        let m = matcher(NONE, NONE);
        assert!(!m.should_include("file.txt"));
        assert!(!m.should_exclude("file.txt"));
    }

    #[test]
    fn nested_doublestar_patterns() {
        let m = matcher(&["*"], &["**/groovy-dsl/**"]);
        assert!(m.should_exclude("build/groovy-dsl/cache.jar"));
        assert!(m.should_exclude("a/b/c/groovy-dsl/file.txt"));
        assert!(!m.should_exclude("build/other/file.jar"));
    }

    #[test]
    fn simple_filename_pattern_matches_root_only() {
        let m = matcher(&["*"], &["app.log"]);
        assert!(m.should_exclude("app.log"));
        assert!(!m.should_exclude("project1/app.log"));
    }

    #[test]
    fn star_matches_across_separators() {
        let m = matcher(&["*"], &["*.log"]);
        assert!(m.should_exclude("app.log"));
        assert!(m.should_exclude("project1/debug.log"));
        assert!(!m.should_exclude("app.txt"));
    }

    #[test]
    fn single_level_pattern() {
        let m = matcher(&["*"], &["*/node_modules/*"]);
        assert!(m.should_exclude("project2/node_modules/lib.js"));
        assert!(m.should_exclude("a/b/node_modules/lib.js"));
    }

    #[test]
    fn middle_doublestar_pattern() {
        let m = matcher(&["a/**/b"], NONE);
        assert!(m.should_include("a/b"));
        assert!(m.should_include("a/x/y/b"));
        assert!(!m.should_include("a/x"));
    }

    #[test]
    fn character_class_pattern() {
        let m = matcher(&["file[0-9].txt"], &["[!a]*.tmp"]);
        assert!(m.should_include("file1.txt"));
        assert!(!m.should_include("filex.txt"));
        assert!(m.should_exclude("b123.tmp"));
        assert!(!m.should_exclude("a123.tmp"));
    }

    #[test]
    fn brace_alternation() {
        let m = matcher(&["file.{txt,md}"], NONE);
        assert!(m.should_include("file.txt"));
        assert!(m.should_include("file.md"));
        assert!(!m.should_include("file.rs"));
    }

    #[test]
    fn unbalanced_braces_are_rejected() {
        assert!(Matcher::new(&["file.{txt"], NONE).is_err());
        assert!(Matcher::new(&["file.txt}"], NONE).is_err());
    }

    #[test]
    fn invalid_include_pattern_is_reported_as_include() {
        let err = Matcher::new(&["[invalid"], NONE).unwrap_err();
        assert!(matches!(err, PatternError::Include(_)));
        assert!(err.to_string().starts_with("invalid include pattern: "));
    }

    #[test]
    fn invalid_exclude_pattern_is_reported_as_exclude() {
        let err = Matcher::new(&["*"], &["[invalid"]).unwrap_err();
        assert!(matches!(err, PatternError::Exclude(_)));
        assert!(err.to_string().starts_with("invalid exclude pattern: "));
    }

    #[test]
    fn trailing_backslash_is_rejected() {
        assert!(Matcher::new(&["foo\\"], NONE).is_err());
    }

    #[test]
    fn escaped_wildcard_is_literal() {
        let m = matcher(&["literal\\*star"], NONE);
        assert!(m.should_include("literal*star"));
        assert!(!m.should_include("literalXstar"));
    }
}
