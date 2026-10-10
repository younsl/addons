//! Homebrew taps, formulae, and casks read from the prefix, matching what
//! `brew bundle dump` writes for them.

use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};
use std::fmt::Write as _;
use std::fs;
use std::hash::BuildHasher;
use std::path::{Path, PathBuf};

use serde::Deserialize;
use serde::de::IgnoredAny;

use super::brewfile::{quote, quote_list};

/// `trust.json` from the Homebrew user config. Entries are lowercased.
#[derive(Debug, Default, Deserialize)]
pub struct Trust {
    #[serde(default, rename = "trustedtaps")]
    taps: Vec<String>,
    #[serde(default, rename = "trustedformulae")]
    formulae: Vec<String>,
    #[serde(default, rename = "trustedcasks")]
    casks: Vec<String>,
    #[serde(default, rename = "trustedcommands")]
    commands: Vec<String>,
}

impl Trust {
    pub fn load(path: &Path) -> Self {
        let mut trust: Self = fs::read_to_string(path)
            .ok()
            .and_then(|text| serde_json::from_str(&text).ok())
            .unwrap_or_default();
        for list in [
            &mut trust.taps,
            &mut trust.formulae,
            &mut trust.casks,
            &mut trust.commands,
        ] {
            for entry in list.iter_mut() {
                *entry = entry.to_lowercase();
            }
        }
        trust
    }

    fn contains(list: &[String], name: &str) -> bool {
        list.contains(&name.to_lowercase())
    }

    fn tap(&self, name: &str) -> bool {
        Self::contains(&self.taps, name)
    }

    fn formula(&self, full_name: &str) -> bool {
        Self::contains(&self.formulae, full_name)
    }

    fn cask(&self, full_name: &str) -> bool {
        Self::contains(&self.casks, full_name)
    }

    /// Formulae and casks from a third-party tap only load when the tap or
    /// the item itself is trusted, so untrusted ones are left out of the dump.
    fn allows(&self, tap: Option<&str>, item_trusted: bool) -> bool {
        tap.is_none_or(|tap| tap.starts_with("homebrew/") || self.tap(tap) || item_trusted)
    }
}

/// Descriptions and keg-only flags of core formulae and casks from the API cache.
#[derive(Debug, Default)]
pub struct Api {
    formulae: HashMap<String, ApiFormula>,
    casks: HashMap<String, ApiCask>,
}

#[derive(Deserialize)]
struct Envelope {
    payload: String,
}

#[derive(Default, Deserialize)]
struct Payload {
    #[serde(default)]
    formulae: HashMap<String, ApiFormula>,
    #[serde(default)]
    casks: HashMap<String, ApiCask>,
}

#[derive(Debug, Deserialize)]
struct ApiFormula {
    desc: Option<String>,
    keg_only_args: Option<IgnoredAny>,
}

#[derive(Debug, Deserialize)]
struct ApiCask {
    desc: Option<String>,
}

impl Api {
    pub fn load(homebrew_cache: &Path) -> Self {
        let dir = homebrew_cache.join("api/internal");
        let mut envelopes: Vec<PathBuf> = fs::read_dir(&dir)
            .map(|entries| {
                entries
                    .filter_map(Result::ok)
                    .map(|e| e.path())
                    .filter(|p| {
                        p.file_name()
                            .and_then(|n| n.to_str())
                            .is_some_and(|n| n.starts_with("packages.") && n.ends_with(".jws.json"))
                    })
                    .collect()
            })
            .unwrap_or_default();
        envelopes.sort();
        envelopes
            .first()
            .and_then(|path| fs::read_to_string(path).ok())
            .and_then(|text| serde_json::from_str::<Envelope>(&text).ok())
            .and_then(|envelope| serde_json::from_str::<Payload>(&envelope.payload).ok())
            .map(|payload| Self {
                formulae: payload.formulae,
                casks: payload.casks,
            })
            .unwrap_or_default()
    }
}

#[derive(Debug, Deserialize)]
struct ReceiptSource {
    tap: Option<String>,
}

#[derive(Debug, Deserialize)]
struct Dependency {
    full_name: String,
}

#[derive(Debug, Default, Deserialize)]
struct Receipt {
    installed_on_request: Option<bool>,
    source: Option<ReceiptSource>,
    tapped_from: Option<String>,
    runtime_dependencies: Option<Vec<Dependency>>,
    used_options: Option<Vec<String>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Formula {
    pub name: String,
    pub full_name: String,
    tap: Option<String>,
    on_request: bool,
    deps: Vec<String>,
    options: Vec<String>,
    head: bool,
    keg: PathBuf,
}

fn core_tap(tap: Option<String>) -> Option<String> {
    tap.filter(|t| t != "homebrew/core" && t != "mxcl/master")
}

fn subdirs(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(dir)
        .map(|entries| {
            entries
                .filter_map(Result::ok)
                .filter(|e| {
                    fs::symlink_metadata(e.path()).is_ok_and(|m| m.is_dir())
                        && !e.file_name().to_string_lossy().starts_with('.')
                })
                .filter_map(|e| e.file_name().into_string().ok())
                .collect()
        })
        .unwrap_or_default();
    names.sort();
    names
}

/// Compare version strings numerically where both sides are digits.
pub fn compare_versions(a: &str, b: &str) -> Ordering {
    fn tokens(s: &str) -> Vec<&str> {
        let mut out = Vec::new();
        let mut start = 0;
        let bytes = s.as_bytes();
        for i in 1..=bytes.len() {
            if i == bytes.len() || bytes[i].is_ascii_digit() != bytes[i - 1].is_ascii_digit() {
                out.push(&s[start..i]);
                start = i;
            }
        }
        out
    }
    for (x, y) in tokens(a).into_iter().zip(tokens(b)) {
        let ord = match (x.parse::<u64>(), y.parse::<u64>()) {
            (Ok(x), Ok(y)) => x.cmp(&y),
            _ => x.cmp(y),
        };
        if ord != Ordering::Equal {
            return ord;
        }
    }
    a.len().cmp(&b.len())
}

fn pick_keg(prefix: &Path, name: &str, versions: &[String]) -> Option<String> {
    for link in [
        prefix.join("var/homebrew/linked").join(name),
        prefix.join("opt").join(name),
    ] {
        if let Some(version) = fs::read_link(&link)
            .ok()
            .and_then(|target| target.file_name().map(|v| v.to_string_lossy().into_owned()))
            .filter(|v| versions.contains(v))
        {
            return Some(version);
        }
    }
    versions
        .iter()
        .max_by(|a, b| compare_versions(a, b))
        .cloned()
}

pub fn installed_formulae(prefix: &Path, trust: &Trust) -> Vec<Formula> {
    let cellar = prefix.join("Cellar");
    let mut formulae = Vec::new();
    for name in subdirs(&cellar) {
        let rack = cellar.join(&name);
        let versions = subdirs(&rack);
        let Some(version) = pick_keg(prefix, &name, &versions) else {
            continue;
        };
        let keg = rack.join(&version);
        let receipt: Receipt = fs::read_to_string(keg.join("INSTALL_RECEIPT.json"))
            .ok()
            .and_then(|text| serde_json::from_str(&text).ok())
            .unwrap_or_default();
        let tap = core_tap(receipt.source.and_then(|s| s.tap).or(receipt.tapped_from));
        let full_name = tap
            .as_ref()
            .map_or_else(|| name.clone(), |tap| format!("{tap}/{name}"));
        if !trust.allows(tap.as_deref(), trust.formula(&full_name)) {
            continue;
        }
        formulae.push(Formula {
            on_request: receipt.installed_on_request.unwrap_or(false),
            deps: receipt
                .runtime_dependencies
                .unwrap_or_default()
                .into_iter()
                .map(|d| d.full_name)
                .collect(),
            options: receipt.used_options.unwrap_or_default(),
            head: version.starts_with("HEAD"),
            name,
            full_name,
            tap,
            keg,
        });
    }
    formulae
}

/// Dependencies before dependents, otherwise core formulae then tap formulae,
/// each in byte order: a depth-first post-order walk with sorted children.
pub fn dump_order(formulae: &[Formula]) -> Vec<&Formula> {
    fn visit<'a>(
        formula: &'a Formula,
        by_name: &HashMap<&str, &'a Formula>,
        visited: &mut HashSet<&'a str>,
        out: &mut Vec<&'a Formula>,
    ) {
        if !visited.insert(formula.full_name.as_str()) {
            return;
        }
        let mut deps: Vec<&String> = formula.deps.iter().collect();
        deps.sort();
        for dep in deps {
            if let Some(dep) = by_name.get(dep.as_str()) {
                visit(dep, by_name, visited, out);
            }
        }
        out.push(formula);
    }

    let mut sorted: Vec<&Formula> = formulae.iter().collect();
    sorted.sort_by(|a, b| (a.tap.is_some(), &a.full_name).cmp(&(b.tap.is_some(), &b.full_name)));
    let mut by_name: HashMap<&str, &Formula> = HashMap::new();
    for formula in &sorted {
        by_name.insert(formula.full_name.as_str(), formula);
    }
    for formula in &sorted {
        by_name.entry(formula.name.as_str()).or_insert(formula);
    }

    let mut visited = HashSet::new();
    let mut out = Vec::new();
    for formula in sorted {
        visit(formula, &by_name, &mut visited, &mut out);
    }
    out
}

/// The string literal after `desc` in a formula or cask Ruby file.
pub fn ruby_desc(source: &str) -> Option<String> {
    source.lines().find_map(|line| {
        let rest = line.trim_start().strip_prefix("desc")?;
        let rest = rest.trim_start();
        let quote = rest.chars().next().filter(|c| *c == '"' || *c == '\'')?;
        let mut out = String::new();
        let mut chars = rest[1..].chars();
        while let Some(c) = chars.next() {
            match c {
                '\\' => out.push(chars.next()?),
                c if c == quote => return Some(out),
                c => out.push(c),
            }
        }
        None
    })
}

fn ruby_keg_only(source: &str) -> bool {
    source
        .lines()
        .any(|line| line.trim_start().starts_with("keg_only"))
}

pub struct Context<'a> {
    pub prefix: &'a Path,
    pub home: &'a Path,
    pub launch_agents: &'a Path,
    pub trust: &'a Trust,
    pub api: &'a Api,
}

/// `brew` lines with their description comments, and the full names dumped.
pub fn brew_lines(ctx: &Context<'_>, formulae: &[Formula]) -> (Vec<String>, Vec<String>) {
    let mut lines = Vec::new();
    let mut dumped = Vec::new();
    for formula in dump_order(formulae) {
        if !formula.on_request || dumped.contains(&formula.full_name) {
            continue;
        }
        let ruby = formula.tap.as_ref().and_then(|_| {
            fs::read_to_string(
                formula
                    .keg
                    .join(".brew")
                    .join(format!("{}.rb", formula.name)),
            )
            .ok()
        });
        let api = formula
            .tap
            .is_none()
            .then(|| ctx.api.formulae.get(&formula.name))
            .flatten();
        let desc = ruby
            .as_deref()
            .and_then(ruby_desc)
            .or_else(|| api.and_then(|a| a.desc.clone()));
        if let Some(desc) = desc {
            lines.extend(desc.split('\n').map(|l| format!("# {l}")));
        }

        let mut line = format!("brew {}", quote(&formula.full_name));
        let mut args: Vec<String> = formula
            .options
            .iter()
            .map(|o| o.trim_start_matches("--").to_string())
            .collect();
        if formula.head {
            args.push("HEAD".to_string());
        }
        if !args.is_empty() {
            args.sort();
            let _ = write!(line, ", args: {}", quote_list(&args));
        }
        if formula.tap.is_none() && has_service(ctx.launch_agents, &formula.name) {
            line.push_str(", restart_service: :changed");
        }
        let linked =
            fs::symlink_metadata(ctx.prefix.join("var/homebrew/linked").join(&formula.name))
                .is_ok();
        let keg_only = ruby.as_deref().map_or_else(
            || api.is_some_and(|a| a.keg_only_args.is_some()),
            ruby_keg_only,
        );
        if linked && keg_only {
            line.push_str(", link: true");
        } else if !linked && !keg_only {
            line.push_str(", link: false");
        }
        if ctx.trust.formula(&formula.full_name) {
            line.push_str(", trusted: true");
        }
        lines.push(line);
        dumped.push(formula.full_name.clone());
    }
    (lines, dumped)
}

fn has_service(launch_agents: &Path, name: &str) -> bool {
    [
        format!("homebrew.mxcl.{name}.plist"),
        format!("sh.brew.{name}.plist"),
    ]
    .iter()
    .any(|plist| launch_agents.join(plist).exists())
}

#[derive(Debug, Default, Deserialize)]
struct CaskReceipt {
    source: Option<ReceiptSource>,
}

#[derive(Debug, Default, Deserialize)]
struct CaskConfig {
    #[serde(default)]
    explicit: serde_json::Map<String, serde_json::Value>,
}

/// Newest installed definition under `.metadata/<version>/<timestamp>/Casks/`.
fn cask_definition(cask_dir: &Path, token: &str) -> Option<PathBuf> {
    let metadata = cask_dir.join(".metadata");
    let mut found: Option<(String, PathBuf)> = None;
    for version in subdirs(&metadata) {
        for stamp in subdirs(&metadata.join(&version)) {
            for ext in ["json", "internal.json", "rb"] {
                let path = metadata
                    .join(&version)
                    .join(&stamp)
                    .join("Casks")
                    .join(format!("{token}.{ext}"));
                if path.is_file() && found.as_ref().is_none_or(|(best, _)| stamp > *best) {
                    found = Some((stamp.clone(), path));
                }
            }
        }
    }
    found.map(|(_, path)| path)
}

fn cask_args(cask_dir: &Path, home: &Path) -> Option<String> {
    let config: CaskConfig = fs::read_to_string(cask_dir.join(".metadata/config.json"))
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())?;
    let home = home.to_string_lossy();
    let mut keys: Vec<&String> = config.explicit.keys().collect();
    keys.sort();
    let pairs: Vec<String> = keys
        .into_iter()
        .filter_map(|key| {
            let value = &config.explicit[key];
            if key == "languages" {
                let langs: Vec<&str> = value
                    .as_array()?
                    .iter()
                    .filter_map(|v| v.as_str())
                    .collect();
                return Some(format!("language: {}", quote(&langs.join(","))));
            }
            let value = value.as_str()?.replace(home.as_ref(), "~");
            Some(format!("{key}: {}", quote(&value)))
        })
        .collect();
    (!pairs.is_empty()).then(|| format!("{{ {} }}", pairs.join(", ")))
}

/// `cask` lines with their description comments, and the full names dumped.
pub fn cask_lines(ctx: &Context<'_>) -> (Vec<String>, Vec<String>) {
    let caskroom = ctx.prefix.join("Caskroom");
    let mut lines = Vec::new();
    let mut dumped: Vec<String> = Vec::new();
    for token in subdirs(&caskroom) {
        let cask_dir = caskroom.join(&token);
        let Some(definition) = cask_definition(&cask_dir, &token) else {
            continue;
        };
        let receipt: CaskReceipt =
            fs::read_to_string(cask_dir.join(".metadata/INSTALL_RECEIPT.json"))
                .ok()
                .and_then(|text| serde_json::from_str(&text).ok())
                .unwrap_or_default();
        let tap = receipt
            .source
            .and_then(|s| s.tap)
            .filter(|t| t != "homebrew/cask");
        let full_name = tap
            .as_ref()
            .map_or_else(|| token.clone(), |tap| format!("{tap}/{token}"));
        if dumped.contains(&full_name)
            || !ctx.trust.allows(tap.as_deref(), ctx.trust.cask(&full_name))
        {
            continue;
        }
        let desc = if tap.is_some() {
            fs::read_to_string(&definition)
                .ok()
                .as_deref()
                .and_then(ruby_desc)
        } else {
            ctx.api.casks.get(&token).and_then(|c| c.desc.clone())
        };
        if let Some(desc) = desc {
            lines.push(format!("# {desc}"));
        }
        let mut line = format!("cask {}", quote(&full_name));
        if let Some(args) = cask_args(&cask_dir, ctx.home) {
            let _ = write!(line, ", args: {args}");
        }
        if ctx.trust.cask(&full_name) {
            line.push_str(", trusted: true");
        }
        lines.push(line);
        dumped.push(full_name);
    }
    (lines, dumped)
}

#[derive(Debug)]
struct Tap {
    name: String,
    user: String,
    repo: String,
    path: PathBuf,
}

impl Tap {
    fn default_remote(&self) -> String {
        let user = match self.user.to_lowercase().as_str() {
            "homebrew" => "Homebrew".to_string(),
            "linuxbrew" => "Linuxbrew".to_string(),
            _ => self.user.clone(),
        };
        format!("https://github.com/{user}/homebrew-{}", self.repo)
    }

    fn origin(&self) -> Option<String> {
        let config = fs::read_to_string(self.path.join(".git/config")).ok()?;
        let mut in_origin = false;
        for line in config.lines() {
            let line = line.trim();
            if line.starts_with('[') {
                in_origin = line == "[remote \"origin\"]";
            } else if in_origin
                && let Some((key, value)) = line.split_once('=')
                && key.trim() == "url"
            {
                return Some(value.trim().to_string());
            }
        }
        None
    }
}

fn taps(prefix: &Path) -> Vec<Tap> {
    let root = prefix.join("Library/Taps");
    let mut taps = Vec::new();
    for user in subdirs(&root) {
        for repo_dir in subdirs(&root.join(&user)) {
            let repo = repo_dir
                .strip_prefix("homebrew-")
                .or_else(|| repo_dir.strip_prefix("linuxbrew-"))
                .unwrap_or(&repo_dir)
                .to_lowercase();
            taps.push(Tap {
                name: format!("{user}/{repo}").to_lowercase(),
                path: root.join(&user).join(&repo_dir),
                user: user.clone(),
                repo,
            });
        }
    }
    taps
}

/// `tap` lines. `dumped` holds the brew and cask full names already written,
/// which partial trust entries skip.
pub fn tap_lines<S: BuildHasher>(
    prefix: &Path,
    trust: &Trust,
    dumped: &HashSet<String, S>,
) -> Vec<String> {
    let mut lines: Vec<String> = taps(prefix)
        .into_iter()
        .map(|tap| {
            let origin = tap.origin();
            let custom = origin.as_ref().is_some_and(|o| *o != tap.default_remote());
            let mut line = format!("tap {}", quote(&tap.name));
            if custom && let Some(origin) = &origin {
                let _ = write!(line, ", {}", quote(origin));
            }
            let trusted =
                (!custom && trust.tap(&tap.name)) || origin.as_ref().is_some_and(|o| trust.tap(o));
            if trusted {
                line.push_str(", trusted: true");
            } else if let Some(partial) = partial_trust(&tap.name, trust, dumped) {
                let _ = write!(line, ", trusted: {partial}");
            }
            line
        })
        .collect();
    lines.sort();
    lines.dedup();
    lines
}

fn partial_trust<S: BuildHasher>(
    tap: &str,
    trust: &Trust,
    dumped: &HashSet<String, S>,
) -> Option<String> {
    let groups: Vec<String> = [
        ("formulae", &trust.formulae),
        ("casks", &trust.casks),
        ("commands", &trust.commands),
    ]
    .into_iter()
    .filter_map(|(key, entries)| {
        let mut items: Vec<String> = entries
            .iter()
            .filter_map(|entry| {
                let (prefix, item) = entry.rsplit_once('/')?;
                (prefix == tap && !item.is_empty() && !dumped.contains(entry))
                    .then(|| item.to_string())
            })
            .collect();
        items.sort();
        items.dedup();
        (!items.is_empty()).then(|| format!("{key}: {}", quote_list(&items)))
    })
    .collect();
    (!groups.is_empty()).then(|| format!("{{ {} }}", groups.join(", ")))
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Prefix {
        _dir: tempfile::TempDir,
        root: PathBuf,
    }

    impl Prefix {
        fn new() -> Self {
            let dir = tempfile::tempdir().expect("tempdir");
            let root = dir.path().to_path_buf();
            Self { _dir: dir, root }
        }

        fn formula(&self, name: &str, version: &str, receipt: &str) -> PathBuf {
            let keg = self.root.join("Cellar").join(name).join(version);
            fs::create_dir_all(&keg).expect("keg");
            fs::write(keg.join("INSTALL_RECEIPT.json"), receipt).expect("receipt");
            keg
        }

        fn link(&self, name: &str, version: &str) {
            let linked = self.root.join("var/homebrew/linked");
            fs::create_dir_all(&linked).expect("linked");
            std::os::unix::fs::symlink(
                self.root.join("Cellar").join(name).join(version),
                linked.join(name),
            )
            .expect("symlink");
        }
    }

    fn api(entries: &[(&str, &str, bool)]) -> Api {
        Api {
            formulae: entries
                .iter()
                .map(|(name, desc, keg_only)| {
                    (
                        (*name).to_string(),
                        ApiFormula {
                            desc: Some((*desc).to_string()),
                            keg_only_args: keg_only.then_some(IgnoredAny),
                        },
                    )
                })
                .collect(),
            casks: HashMap::new(),
        }
    }

    fn trust(json: &str) -> Trust {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("trust.json");
        fs::write(&path, json).expect("trust");
        Trust::load(&path)
    }

    #[test]
    fn version_ordering_is_numeric() {
        assert_eq!(compare_versions("1.10.0", "1.9.2"), Ordering::Greater);
        assert_eq!(compare_versions("8.0", "8.0_1"), Ordering::Less);
        assert_eq!(compare_versions("2.0", "2.0"), Ordering::Equal);
        assert_eq!(compare_versions("1.0a", "1.0b"), Ordering::Less);
    }

    #[test]
    fn ruby_desc_reads_double_and_single_quotes() {
        assert_eq!(
            ruby_desc("class X\n  desc \"Say \\\"hi\\\"\"\nend"),
            Some("Say \"hi\"".to_string())
        );
        assert_eq!(ruby_desc("  desc 'plain'\n"), Some("plain".to_string()));
        assert_eq!(ruby_desc("  homepage \"x\"\n"), None);
        assert_eq!(ruby_desc("  desc \"unterminated\n"), None);
        assert!(ruby_keg_only("  keg_only :provided_by_macos\n"));
    }

    #[test]
    fn trust_loads_lowercased_and_tolerates_missing_file() {
        let t = trust(r#"{"trustedtaps":["Argoproj/Tap"],"trustedformulae":["x/y/z"]}"#);
        assert!(t.tap("argoproj/tap"));
        assert!(t.formula("X/Y/Z"));
        assert!(!t.cask("x/y/z"));
        assert_eq!(
            Trust::load(Path::new("/nonexistent/trust.json")).taps.len(),
            0
        );
        assert!(t.allows(None, false));
        assert!(t.allows(Some("homebrew/autoupdate"), false));
        assert!(t.allows(Some("argoproj/tap"), false));
        assert!(!t.allows(Some("other/tap"), false));
        assert!(t.allows(Some("other/tap"), true));
    }

    #[test]
    fn api_cache_reads_jws_envelope() {
        let dir = tempfile::tempdir().expect("tempdir");
        let internal = dir.path().join("api/internal");
        fs::create_dir_all(&internal).expect("dir");
        let payload = r#"{"formulae":{"curl":{"desc":"Get a file","keg_only_args":[]},"jq":{"desc":"JSON"}},"casks":{"slack":{"desc":"Team chat"}}}"#;
        let envelope = serde_json::json!({ "payload": payload, "signatures": [] });
        fs::write(
            internal.join("packages.arm64_x.jws.json"),
            envelope.to_string(),
        )
        .expect("write");

        let api = Api::load(dir.path());
        assert!(api.formulae["curl"].keg_only_args.is_some());
        assert!(api.formulae["jq"].keg_only_args.is_none());
        assert_eq!(api.casks["slack"].desc.as_deref(), Some("Team chat"));
        assert_eq!(Api::load(Path::new("/nonexistent")).formulae.len(), 0);
    }

    #[test]
    fn formulae_in_dependency_order_with_link_and_trust() {
        let p = Prefix::new();
        p.formula(
            "zlib",
            "1.3",
            r#"{"installed_on_request":false,"source":{"tap":"homebrew/core"}}"#,
        );
        p.formula(
            "curl",
            "8.0",
            r#"{"installed_on_request":true,"source":{"tap":"homebrew/core"},"runtime_dependencies":[{"full_name":"zlib"}]}"#,
        );
        p.formula(
            "aaa",
            "1.0",
            r#"{"installed_on_request":true,"runtime_dependencies":[{"full_name":"zlib"}],"used_options":["--with-x"]}"#,
        );
        p.link("aaa", "1.0");
        let tool = p.formula(
            "tool",
            "2.0",
            r#"{"installed_on_request":true,"source":{"tap":"acme/tap"}}"#,
        );
        fs::create_dir_all(tool.join(".brew")).expect("brew dir");
        fs::write(
            tool.join(".brew/tool.rb"),
            "class Tool\n  desc \"Acme tool\"\nend\n",
        )
        .expect("rb");
        p.link("tool", "2.0");
        p.formula(
            "hidden",
            "1.0",
            r#"{"installed_on_request":true,"source":{"tap":"evil/tap"}}"#,
        );

        let trust = trust(r#"{"trustedformulae":["acme/tap/tool"]}"#);
        let api = api(&[
            ("curl", "Get a file\nfrom anywhere", true),
            ("aaa", "Letters", false),
        ]);
        let formulae = installed_formulae(&p.root, &trust);
        assert!(
            formulae.iter().all(|f| f.name != "hidden"),
            "untrusted tap formula is dropped"
        );

        let order: Vec<&str> = dump_order(&formulae)
            .iter()
            .map(|f| f.full_name.as_str())
            .collect();
        assert_eq!(order, vec!["zlib", "aaa", "curl", "acme/tap/tool"]);

        let home = PathBuf::from("/home/dev");
        let agents = p.root.join("LaunchAgents");
        fs::create_dir_all(&agents).expect("agents");
        fs::write(agents.join("homebrew.mxcl.curl.plist"), "").expect("plist");
        let ctx = Context {
            prefix: &p.root,
            home: &home,
            launch_agents: &agents,
            trust: &trust,
            api: &api,
        };
        let (lines, dumped) = brew_lines(&ctx, &formulae);
        assert_eq!(
            lines,
            vec![
                "# Letters",
                "brew \"aaa\", args: [\"with-x\"]",
                "# Get a file",
                "# from anywhere",
                "brew \"curl\", restart_service: :changed",
                "# Acme tool",
                "brew \"acme/tap/tool\", trusted: true",
            ]
        );
        assert_eq!(dumped, vec!["aaa", "curl", "acme/tap/tool"]);
    }

    #[test]
    fn link_false_for_unlinked_regular_formula_and_head_args() {
        let p = Prefix::new();
        p.formula("jq", "HEAD-abc", r#"{"installed_on_request":true}"#);
        p.formula("old", "1.0", r#"{"installed_on_request":true}"#);
        p.formula("old", "2.0", r#"{"installed_on_request":true}"#);
        let trust = Trust::default();
        let api = Api::default();
        let home = PathBuf::from("/h");
        let ctx = Context {
            prefix: &p.root,
            home: &home,
            launch_agents: &p.root,
            trust: &trust,
            api: &api,
        };
        let formulae = installed_formulae(&p.root, &trust);
        assert!(
            formulae.iter().any(|f| f.keg.ends_with("old/2.0")),
            "highest version wins"
        );
        let (lines, _) = brew_lines(&ctx, &formulae);
        assert_eq!(
            lines,
            vec![
                "brew \"jq\", args: [\"HEAD\"], link: false",
                "brew \"old\", link: false"
            ]
        );
    }

    #[test]
    fn casks_with_tap_desc_args_and_trust() {
        let p = Prefix::new();
        let room = p.root.join("Caskroom");
        let slack = room.join("slack/.metadata/4.0/20260101000000.000/Casks");
        fs::create_dir_all(&slack).expect("slack");
        fs::write(slack.join("slack.json"), "{}").expect("json");
        let tool = room.join("tool/.metadata/1.0/20260101000000.000/Casks");
        fs::create_dir_all(&tool).expect("tool");
        fs::write(
            tool.join("tool.rb"),
            "cask \"tool\" do\n  desc \"Acme app\"\nend\n",
        )
        .expect("rb");
        fs::write(
            room.join("tool/.metadata/INSTALL_RECEIPT.json"),
            r#"{"source":{"tap":"acme/tap"}}"#,
        )
        .expect("receipt");
        fs::write(
            room.join("tool/.metadata/config.json"),
            r#"{"explicit":{"appdir":"/home/dev/Applications","languages":["en","ko"]}}"#,
        )
        .expect("config");
        fs::create_dir_all(room.join("broken")).expect("not installed");

        let trust = trust(r#"{"trustedtaps":["acme/tap"],"trustedcasks":["acme/tap/tool"]}"#);
        let mut api = Api::default();
        api.casks.insert(
            "slack".into(),
            ApiCask {
                desc: Some("Team chat".into()),
            },
        );
        let home = PathBuf::from("/home/dev");
        let ctx = Context {
            prefix: &p.root,
            home: &home,
            launch_agents: &p.root,
            trust: &trust,
            api: &api,
        };
        let (lines, dumped) = cask_lines(&ctx);
        assert_eq!(
            lines,
            vec![
                "# Team chat",
                "cask \"slack\"",
                "# Acme app",
                "cask \"acme/tap/tool\", args: { appdir: \"~/Applications\", language: \"en,ko\" }, trusted: true",
            ]
        );
        assert_eq!(dumped, vec!["slack", "acme/tap/tool"]);
    }

    #[test]
    fn taps_with_remote_trust_and_partial_trust() {
        let p = Prefix::new();
        let taps_root = p.root.join("Library/Taps");
        for (user, repo, origin) in [
            (
                "argoproj",
                "homebrew-tap",
                "https://github.com/argoproj/homebrew-tap",
            ),
            ("acme", "homebrew-tools", "git@example.com:acme/tools.git"),
            (
                "homebrew",
                "homebrew-autoupdate",
                "https://github.com/Homebrew/homebrew-autoupdate",
            ),
            (
                "part",
                "homebrew-ial",
                "https://github.com/part/homebrew-ial",
            ),
        ] {
            let git = taps_root.join(user).join(repo).join(".git");
            fs::create_dir_all(&git).expect("git");
            fs::write(
                git.join("config"),
                format!("[core]\n\turl = nope\n[remote \"origin\"]\n\turl = {origin}\n"),
            )
            .expect("config");
        }
        fs::create_dir_all(taps_root.join("argoproj/.hidden")).expect("hidden");

        let trust = trust(
            r#"{"trustedtaps":["argoproj/tap","acme/tools"],"trustedformulae":["part/ial/b","part/ial/a"],"trustedcasks":["part/ial/app"]}"#,
        );
        let dumped: HashSet<String> = ["part/ial/app".to_string()].into();
        assert_eq!(
            tap_lines(&p.root, &trust, &dumped),
            vec![
                "tap \"acme/tools\", \"git@example.com:acme/tools.git\"",
                "tap \"argoproj/tap\", trusted: true",
                "tap \"homebrew/autoupdate\"",
                "tap \"part/ial\", trusted: { formulae: [\"a\", \"b\"] }",
            ]
        );
    }
}
