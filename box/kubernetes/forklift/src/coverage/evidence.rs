//! What counts as wiring: which files are read, what in them is a live
//! reference to forklift rather than a commented-out one, and what the matches
//! add up to. Extending the scan to another ecosystem is a change to this file
//! and to nothing else.

use std::collections::{BTreeSet, HashSet};
use std::sync::LazyLock;

use regex::Regex;

/// ecosystem is one package manager or image builder the scan knows about. All
/// four things a new one needs are here: the files it configures, how those
/// files write comments, the format name to report, and the filename shapes that
/// imply it when a matched file never names a URL.
pub(crate) struct Ecosystem {
    /// format is the repository format reported for a project wired this way.
    pub(crate) format: &'static str,
    /// files matches the names, without a directory, that can pin a registry.
    pub(crate) files: &'static str,
    /// comments is how those files comment out a line. An empty value means the
    /// default, which is "#".
    pub(crate) comments: CommentSyntax,
    /// hints matches anywhere in the joined evidence paths, and is what names
    /// the format when no forklift URL was found to read it from.
    pub(crate) hints: &'static str,
}

/// The ecosystems the scan understands. Adding one is an entry here and nothing
/// else: the file matcher, the comment syntax and the format naming are all
/// derived from this table.
pub(crate) static ECOSYSTEMS: LazyLock<Vec<Ecosystem>> = LazyLock::new(|| {
    vec![
        Ecosystem {
            format: "npm",
            files: r"\.npmrc|\.yarnrc|\.yarnrc\.yml|package\.json|pnpm-workspace\.yaml",
            hints: r"\.npmrc|\.yarnrc|package\.json|pnpm-workspace",
            comments: CommentSyntax {
                line: vec!["#", ";"],
                block: vec![],
                exact: vec![".npmrc", ".yarnrc"],
            },
        },
        Ecosystem {
            format: "maven",
            files: r"settings\.xml|pom\.xml|build\.gradle|build\.gradle\.kts|settings\.gradle|settings\.gradle\.kts|gradle\.properties|init\.gradle|init\.gradle\.kts",
            hints: r"gradle|pom\.xml|settings\.xml",
            comments: CommentSyntax {
                line: vec!["#", ";"],
                block: vec![],
                exact: vec!["gradle.properties"],
            },
        },
        Ecosystem {
            format: "pypi",
            files: r"pip\.conf|pip\.ini|requirements[A-Za-z0-9_.-]*\.txt|pyproject\.toml|poetry\.toml",
            hints: r"pip\.(conf|ini)|requirements.*\.txt|pyproject|poetry",
            comments: CommentSyntax {
                line: vec!["#", ";"],
                block: vec![],
                exact: vec!["pip.conf", "pip.ini"],
            },
        },
        Ecosystem {
            format: "dockerfile",
            files: r"Dockerfile[A-Za-z0-9_.-]*",
            hints: r"Dockerfile",
            comments: CommentSyntax::empty(),
        },
    ]
});

// What counts as evidence. The two file classes are the two halves of a wired
// project: a CI entrypoint that builds through forklift, and a package-manager
// or image-build file that pins the registry it resolves from.

/// ci_file_re matches GitLab CI entrypoints, including .gitlab-ci.&lt;suffix&gt;.yml
/// and the ci/ and .gitlab/ci/ directories.
pub(crate) static CI_FILE_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(^|/)\.gitlab-ci([-.][A-Za-z0-9_-]+)?\.ya?ml$|^(\.gitlab/)?ci/.*\.ya?ml$")
        .expect("ci file regex")
});

/// registry_file_re matches the configuration files of every known ecosystem.
pub(crate) static REGISTRY_FILE_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(&format!("(^|/)({})$", join_ecosystems(|e| e.files))).expect("registry file regex")
});

/// format_hint_res name a format from the filenames alone, in the order the
/// formats are reported in.
pub(crate) static FORMAT_HINT_RES: LazyLock<Vec<FormatHint>> = LazyLock::new(compile_format_hints);

/// vendor_re drops committed dependency trees and build output, which mention a
/// registry host without the project having chosen it.
pub(crate) static VENDOR_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(^|/)(node_modules|vendor|\.git|dist|build/generated)/").expect("vendor regex")
});

/// token_re matches the credential a CI job references when it authenticates to
/// forklift without naming the host inline.
pub(crate) static TOKEN_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"FORKLIFT_[A-Z0-9_]*TOKEN").expect("token regex"));

fn join_ecosystems(pick: fn(&Ecosystem) -> &'static str) -> String {
    ECOSYSTEMS.iter().map(pick).collect::<Vec<_>>().join("|")
}

pub(crate) struct FormatHint {
    pub(crate) format: &'static str,
    pub(crate) matcher: Regex,
}

fn compile_format_hints() -> Vec<FormatHint> {
    ECOSYSTEMS
        .iter()
        .map(|e| FormatHint {
            format: e.format,
            matcher: Regex::new(e.hints).expect("format hint regex"),
        })
        .collect()
}

/// verdict is what one branch yielded.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Verdict {
    pub(crate) has_ci: bool,
    pub(crate) hit: bool,
    pub(crate) ci_wired: bool,
    pub(crate) registry_pinned: bool,
    pub(crate) format: String,
    pub(crate) evidence: Vec<String>,
}

/// commentSyntax is how one family of configuration files writes a comment.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct CommentSyntax {
    /// line prefixes a comment that runs to the end of the line.
    pub(crate) line: Vec<&'static str>,
    /// block are open/close pairs, which matter because the host can sit on a
    /// line of its own inside one.
    pub(crate) block: Vec<(&'static str, &'static str)>,
    /// exact are the filenames this syntax is chosen for, for the configuration
    /// files whose name carries no extension to recognise them by.
    pub(crate) exact: Vec<&'static str>,
}

impl CommentSyntax {
    /// The zero value: no comments are stripped at all.
    pub(crate) const fn empty() -> CommentSyntax {
        CommentSyntax {
            line: Vec::new(),
            block: Vec::new(),
            exact: Vec::new(),
        }
    }
}

/// comment_syntax_for picks the comment syntax by filename. The families are the
/// ones the scan reads; anything unrecognised falls back to "#", which is what
/// every remaining candidate file uses.
pub(crate) fn comment_syntax_for(path: &str) -> CommentSyntax {
    let base = match path.rfind('/') {
        Some(i) => &path[i + 1..],
        None => path,
    };
    // Markup and source files first: their syntax follows the extension, and it
    // is the block comments that matter, since the host can sit on a line of its
    // own inside one.
    if base.ends_with(".xml") {
        return CommentSyntax {
            line: vec![],
            block: vec![("<!--", "-->")],
            exact: vec![],
        };
    }
    if base.ends_with(".gradle") || base.ends_with(".gradle.kts") {
        return CommentSyntax {
            line: vec!["//"],
            block: vec![("/*", "*/")],
            exact: vec![],
        };
    }
    if base.ends_with(".json") {
        // JSON has no comments, so nothing may be stripped: a "//" inside a
        // string is data.
        return CommentSyntax::empty();
    }
    for e in ECOSYSTEMS.iter() {
        if e.comments.exact.contains(&base) {
            return e.comments.clone();
        }
    }
    // Everything else left: YAML, Dockerfiles, TOML and requirement lists.
    CommentSyntax {
        line: vec!["#"],
        block: vec![],
        exact: vec![],
    }
}

/// active_body drops what a reader would call commented out, so a migration that
/// was rolled back and left behind as a comment stops counting as wiring.
///
/// Two deliberate limits. Only a line whose first non-space character opens a
/// comment is dropped, never the tail of a line after one: a "#" inside a shell
/// command or a quoted value is not a comment, and cutting there would lose real
/// configuration. And block comments are removed by scanning for the delimiters
/// rather than by parsing the file, since every one of these files is a template
/// that a real parser would choke on.
pub(crate) fn active_body(path: &str, body: &str) -> String {
    let syntax = comment_syntax_for(path);
    let mut body = body.to_string();
    for (open, close) in &syntax.block {
        body = strip_blocks(&body, open, close);
    }
    if syntax.line.is_empty() {
        return body;
    }
    let kept: Vec<&str> = body
        .split('\n')
        .filter(|line| {
            let trimmed = line.trim();
            !syntax.line.iter().any(|prefix| trimmed.starts_with(prefix))
        })
        .collect();
    kept.join("\n")
}

/// strip_blocks removes every open..close span. An unterminated open takes the
/// rest of the file with it, which is what the file itself means by it.
fn strip_blocks(body: &str, open: &str, close: &str) -> String {
    let mut out = String::new();
    let mut body = body;
    loop {
        let start = match body.find(open) {
            Some(i) => i,
            None => {
                out.push_str(body);
                return out;
            }
        };
        out.push_str(&body[..start]);
        let rest = &body[start + open.len()..];
        match rest.find(close) {
            Some(i) => body = &rest[i + close.len()..],
            None => return out,
        }
    }
}

/// body_matches is what makes a file evidence: outside its comments, it names
/// the forklift host, or it references the forklift credential a job
/// authenticates with.
pub(crate) fn body_matches(host: &str, path: &str, body: &str) -> bool {
    if host.is_empty() {
        return false;
    }
    let active = active_body(path, body);
    active.contains(host) || TOKEN_RE.is_match(&active)
}

/// evidenceSet accumulates the files on one ref that reference forklift, keeping
/// each match next to the text the verdict is derived from. Both ways of reading
/// a project, the per-branch walk and the blob search, collect into one of these
/// so they cannot drift into judging the same files differently.
pub(crate) struct EvidenceSet {
    host: String,
    paths: Vec<String>,
    bodies: Vec<String>,
    seen: HashSet<String>,
}

pub(crate) fn new_evidence_set(host: &str) -> EvidenceSet {
    EvidenceSet {
        host: host.to_string(),
        paths: Vec::new(),
        bodies: Vec::new(),
        seen: HashSet::new(),
    }
}

impl EvidenceSet {
    /// add records a file when it is live evidence, reporting whether it was.
    pub(crate) fn add(&mut self, path: &str, body: &str) -> bool {
        if !body_matches(&self.host, path, body) {
            return false;
        }
        if self.seen.contains(path) {
            return true;
        }
        self.seen.insert(path.to_string());
        self.paths.push(path.to_string());
        // The comments are dropped here as well as in the match, so a
        // commented-out URL cannot name the repository format either.
        self.bodies.push(active_body(path, body));
        true
    }

    pub(crate) fn empty(&self) -> bool {
        self.paths.is_empty()
    }

    /// verdict summarises what was collected. Only call it on a non-empty set:
    /// an empty one is "nothing found", which is not a hit.
    pub(crate) fn verdict(&self) -> Verdict {
        let mut v = summarize_evidence(&self.host, &self.paths, &self.bodies);
        v.hit = true;
        v
    }
}

/// summarize_evidence splits the matched files into the two halves of the wiring
/// and derives the repository formats in play.
pub(crate) fn summarize_evidence(host: &str, paths: &[String], bodies: &[String]) -> Verdict {
    let mut sorted = paths.to_vec();
    sorted.sort();

    let mut v = Verdict {
        evidence: sorted.clone(),
        ..Verdict::default()
    };
    for p in &sorted {
        if CI_FILE_RE.is_match(p) {
            v.ci_wired = true;
        } else {
            v.registry_pinned = true;
        }
    }

    // The first path segment after the host is the repository format:
    //   https://forklift.example.com/npm/npmjs/  ->  npm
    let mut formats: BTreeSet<String> = BTreeSet::new();
    if !host.is_empty() {
        let re = Regex::new(&format!("{}/([a-z0-9-]+)", regex::escape(host)))
            .expect("format regex from an escaped host");
        let joined = bodies.join("\n");
        for caps in re.captures_iter(&joined) {
            formats.insert(caps[1].to_string());
        }
    }
    if !formats.is_empty() {
        v.format = formats.into_iter().collect::<Vec<_>>().join("/");
    } else {
        v.format = classify_by_filename(&sorted);
    }
    v
}

/// classify_by_filename guesses the format from the matched filenames, for the
/// case where a file authenticates with the token but never names a host.
pub(crate) fn classify_by_filename(paths: &[String]) -> String {
    let joined = paths.join(",");
    FORMAT_HINT_RES
        .iter()
        .filter(|hint| hint.matcher.is_match(&joined))
        .map(|hint| hint.format)
        .collect::<Vec<_>>()
        .join("/")
}

#[cfg(test)]
pub(crate) mod tests {
    use crate::coverage::evidence::{
        ECOSYSTEMS, FORMAT_HINT_RES, REGISTRY_FILE_RE, body_matches, classify_by_filename,
        comment_syntax_for,
    };
    use crate::coverage::scan::tests::{
        HOST, fake_gitlab, fake_project, new_mem_store, new_test_scanner, with_host,
    };
    use crate::coverage::types::STATE_NOT_APPLIED;

    /// A rolled-back migration leaves its forklift line behind as a comment. Reading
    /// that as wiring is the one false positive this scan can produce on its own, so
    /// the comment cases are pinned here alongside the ones that must keep matching.
    #[test]
    fn body_matches_ignores_comments() {
        const HOST: &str = "forklift.example.com";
        let cases: Vec<(&str, &str, String, bool)> = vec![
            (
                "npmrc commented out",
                ".npmrc",
                format!(
                    "# registry=https://{HOST}/npm/npmjs/\nregistry=https://registry.npmjs.org/\n"
                ),
                false,
            ),
            (
                "npmrc semicolon comment",
                ".npmrc",
                format!("; registry=https://{HOST}/npm/npmjs/\n"),
                false,
            ),
            (
                "npmrc live line",
                ".npmrc",
                format!("# use the internal mirror\nregistry=https://{HOST}/npm/npmjs/\n"),
                true,
            ),
            (
                // A "#" after real configuration is a trailing note, not a comment
                // line, and cutting there would lose the line it annotates.
                "trailing note on a live line",
                ".gitlab-ci.yml",
                format!("image: {HOST}/docker/base:1 # pinned\n"),
                true,
            ),
            (
                "xml block comment",
                "settings.xml",
                format!(
                    "<settings>\n<!--\n<mirror><url>https://{HOST}/maven/central/</url></mirror>\n-->\n</settings>\n"
                ),
                false,
            ),
            (
                "xml live mirror",
                "settings.xml",
                format!(
                    "<settings><mirror><url>https://{HOST}/maven/central/</url></mirror></settings>\n"
                ),
                true,
            ),
            (
                "gradle line comment",
                "build.gradle",
                format!("// maven {{ url 'https://{HOST}/maven/central/' }}\n"),
                false,
            ),
            (
                "gradle block comment",
                "build.gradle.kts",
                format!("/*\nmaven(\"https://{HOST}/maven/central/\")\n*/\n"),
                false,
            ),
            (
                // JSON has no comments, so nothing may be stripped from one.
                "json keeps a slash-slash string",
                "package.json",
                format!("{{\"publishConfig\":{{\"registry\":\"https://{HOST}/npm/npmjs/\"}}}}\n"),
                true,
            ),
            (
                "commented token",
                ".gitlab-ci.yml",
                "# - echo $FORKLIFT_CI_TOKEN\n".to_string(),
                false,
            ),
            (
                "live token",
                ".gitlab-ci.yml",
                "script:\n  - echo $FORKLIFT_CI_TOKEN\n".to_string(),
                true,
            ),
            (
                // An unterminated block takes the rest of the file with it, which is
                // what the file itself means by it.
                "unterminated xml comment",
                "pom.xml",
                format!("<project>\n<!-- <url>https://{HOST}/maven/central/</url>\n</project>\n"),
                false,
            ),
        ];
        for (name, path, body, want) in cases {
            assert_eq!(
                body_matches(HOST, path, &body),
                want,
                "{name}: body_matches"
            );
        }
    }

    /// The verdict follows the same rule end to end: a project whose only forklift
    /// reference is commented out is not wired.
    #[tokio::test]
    async fn scan_ignores_commented_out_wiring() {
        let srv = fake_gitlab(vec![fake_project(
        1,
        "team/rolled-back",
        &["main"],
        &[
            (
                "main:.gitlab-ci.yml",
                format!("# image: {HOST}/docker/base:1\nimage: docker.io/library/node:22\n"),
            ),
            (
                "main:.npmrc",
                format!(
                    "# registry=https://{HOST}/npm/npmjs/\nregistry=https://registry.npmjs.org/\n"
                ),
            ),
        ],
    )])
    .await;
        let scanner = new_test_scanner(new_mem_store(with_host(HOST)), &srv, None);
        scanner.scan("tester").await.expect("Scan");
        let p = scanner
            .project("team/rolled-back")
            .expect("the project is missing from the scan");
        assert!(
            p.applied == STATE_NOT_APPLIED && !p.ci_wired && !p.registry_pinned,
            "verdict = {p:?}, want nothing counted from commented-out lines"
        );
    }

    /// The ecosystem table is the extension point: everything the scan knows about a
    /// package manager is one entry. These pin what an entry has to produce, so a new
    /// one that matches no file, or reports no format, fails here rather than by
    /// quietly under-counting a whole ecosystem.
    #[test]
    fn ecosystem_table() {
        assert_eq!(
            FORMAT_HINT_RES.len(),
            ECOSYSTEMS.len(),
            "format hints vs ecosystems"
        );
        let mut seen: Vec<&str> = Vec::new();
        for e in ECOSYSTEMS.iter() {
            assert!(
                !e.format.is_empty() && !e.files.is_empty() && !e.hints.is_empty(),
                "ecosystem {} is missing a format, a file pattern or a hint",
                e.format
            );
            assert!(
                !seen.contains(&e.format),
                "format {:?} is claimed by two ecosystems",
                e.format
            );
            seen.push(e.format);
        }

        for (path, format) in [
            (".npmrc", "npm"),
            ("sub/package.json", "npm"),
            ("pom.xml", "maven"),
            ("build.gradle.kts", "maven"),
            ("pip.conf", "pypi"),
            ("requirements-dev.txt", "pypi"),
            ("docker/Dockerfile.ci", "dockerfile"),
        ] {
            assert!(
                REGISTRY_FILE_RE.is_match(path),
                "{path:?} is not read as a registry file"
            );
            assert_eq!(
                classify_by_filename(&[path.to_string()]),
                format,
                "classify_by_filename({path:?})"
            );
        }
        // Formats are reported in table order, so one project wired two ways reads
        // the same on every scan.
        assert_eq!(
            classify_by_filename(&["pom.xml".to_string(), ".npmrc".to_string()]),
            "npm/maven",
            "classify_by_filename of two ecosystems"
        );
        for path in ["README.md", "src/main.go", ".gitlab-ci.yml"] {
            assert!(
                !REGISTRY_FILE_RE.is_match(path),
                "{path:?} must not count as a registry file"
            );
        }
    }

    #[test]
    fn comment_syntax_by_filename() {
        let cases: Vec<(&str, Vec<&str>, usize)> = vec![
            ("settings.xml", vec![], 1),
            ("build.gradle", vec!["//"], 1),
            ("package.json", vec![], 0),
            (".npmrc", vec!["#", ";"], 0),
            ("a/b/pip.conf", vec!["#", ";"], 0),
            (".gitlab-ci.yml", vec!["#"], 0),
            ("Dockerfile", vec!["#"], 0),
        ];
        for (path, line, block) in cases {
            let got = comment_syntax_for(path);
            assert_eq!(
                got.block.len(),
                block,
                "comment_syntax_for({path:?}) blocks"
            );
            assert_eq!(got.line, line, "comment_syntax_for({path:?}) line");
        }
    }
}
