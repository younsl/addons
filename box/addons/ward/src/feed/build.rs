//! `ward feed build` and `ward feed check`: assemble a database from local
//! source files, prune what matches known-good files, and gate it on a
//! false-positive corpus.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use chrono::Utc;
use walkdir::WalkDir;

use crate::content::{
    ContentError, ENGINE_VERSION, Engine, EngineParts, FeedDb, HashSet32, Manifest, Source,
    YaraSource, compile_yara,
};
use crate::policy::Severity;
use crate::signature::{Match, Signature, SignatureDb, sha256_hex};

const CORPUS_MAX_FILE: u64 = 64 * 1024 * 1024;
const YARA_PREFIX: &str = "yara:";
/// Optional provenance in a `--yara-dir` or `--hash-dir`, a JSON `Source`.
pub const SOURCE_META: &str = "source.json";

#[derive(Debug, thiserror::Error)]
pub enum BuildError {
    #[error("read {path}: {source}")]
    Read {
        path: String,
        source: std::io::Error,
    },
    #[error("parse {path}: {reason}")]
    Parse { path: String, reason: String },
    #[error(transparent)]
    Content(#[from] ContentError),
    #[error("{hits} corpus file(s) matched: {first}")]
    FalsePositive { hits: usize, first: String },
    #[error("pruning {count} entries against the corpus, more than --max-pruned {max}: {first}")]
    TooManyPruned {
        count: usize,
        max: usize,
        first: String,
    },
    #[error("{path} matched {name}, which is not feed content and cannot be pruned")]
    Unprunable { path: String, name: String },
}

#[derive(Debug, Default)]
pub struct BuildInput {
    pub revision: Option<String>,
    /// Directories of SHA-256 lists (`*.txt`, one digest per line), each
    /// one hash set named after its `source.json`.
    pub hash_dirs: Vec<PathBuf>,
    pub yara_dirs: Vec<PathBuf>,
    /// Leave out YARA rules that only match non-Linux file formats.
    pub yara_linux_only: bool,
    pub signatures: Vec<PathBuf>,
    /// Known-good SHA-256 lists, removed from every hash in the feed.
    pub allowlists: Vec<PathBuf>,
    /// Known-good file trees. Feed hashes found here are removed and rules
    /// that match here are disabled.
    pub corpus: Vec<PathBuf>,
    /// The most hashes, YARA rules, and signatures the corpus may prune.
    pub max_pruned: usize,
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct BuildReport {
    pub hashes: usize,
    pub signatures: usize,
    pub yara_files: usize,
    pub yara_rules: usize,
    pub dropped_yara: Vec<String>,
    /// YARA rules left out because they only match non-Linux file formats.
    pub foreign_yara: usize,
    /// Feed hashes removed because an allowlist holds them.
    pub allowlisted: usize,
    /// Hashes removed, rules disabled, and signatures dropped because they
    /// matched the corpus, each with the corpus file it matched.
    pub pruned: Vec<String>,
}

fn read(path: &Path) -> Result<String, BuildError> {
    std::fs::read_to_string(path).map_err(|source| BuildError::Read {
        path: path.display().to_string(),
        source,
    })
}

fn decode_digest(hex: &str) -> Option<[u8; 32]> {
    if hex.len() != 64 {
        return None;
    }
    let mut h = [0u8; 32];
    for (i, b) in h.iter_mut().enumerate() {
        *b = u8::from_str_radix(hex.get(i * 2..i * 2 + 2)?, 16).ok()?;
    }
    Some(h)
}

/// SHA-256 digests from a hash dump: one hex digest per line, `#` comments
/// and anything else skipped.
pub fn parse_hash_dump(text: &str) -> Vec<[u8; 32]> {
    text.lines()
        .map(|l| l.trim().trim_matches('"'))
        .filter(|l| !l.starts_with('#'))
        .filter_map(decode_digest)
        .collect()
}

fn files_with(dir: &Path, extensions: &[&str]) -> Vec<PathBuf> {
    let mut files: Vec<PathBuf> = WalkDir::new(dir)
        .follow_links(false)
        .into_iter()
        .filter_map(Result::ok)
        .filter(|e| e.file_type().is_file())
        .map(walkdir::DirEntry::into_path)
        .filter(|p| {
            p.extension()
                .is_some_and(|e| extensions.iter().any(|x| e.eq_ignore_ascii_case(x)))
        })
        .collect();
    files.sort();
    files
}

/// The provenance in `dir/source.json`, or a bare source named after `dir`.
fn source_meta(dir: &Path) -> Result<Source, BuildError> {
    let meta_path = dir.join(SOURCE_META);
    if meta_path.is_file() {
        return serde_json::from_str::<Source>(&read(&meta_path)?).map_err(|e| BuildError::Parse {
            path: meta_path.display().to_string(),
            reason: e.to_string(),
        });
    }
    Ok(Source {
        name: dir
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default(),
        ..Source::default()
    })
}

/// One hash set from the digest lists under `dir`.
fn collect_hashes(dir: &Path) -> Result<(Source, HashSet32), BuildError> {
    let mut source = source_meta(dir)?;
    let mut hashes = Vec::new();
    let mut digest = sha2::Sha256::default();
    let files = files_with(dir, &["txt"]);
    if files.is_empty() {
        return Err(BuildError::Parse {
            path: dir.display().to_string(),
            reason: "no .txt digest lists".into(),
        });
    }
    for file in files {
        let text = read(&file)?;
        sha2::Digest::update(&mut digest, text.as_bytes());
        hashes.extend(parse_hash_dump(&text));
    }
    hashes.sort_unstable();
    hashes.dedup();
    if source.sha256.is_empty() {
        source.sha256 = format!("{:x}", sha2::Digest::finalize(digest));
    }
    let set = HashSet32::pack(
        &source.name,
        Severity::High,
        &format!("SHA-256 listed by {}", source.name),
        &hashes,
    );
    Ok((source, set))
}

fn corpus_files(corpus: &[PathBuf]) -> Vec<PathBuf> {
    corpus
        .iter()
        .flat_map(|dir| WalkDir::new(dir).follow_links(false).into_iter())
        .filter_map(Result::ok)
        .filter(|e| {
            e.file_type().is_file() && e.metadata().is_ok_and(|m| m.len() <= CORPUS_MAX_FILE)
        })
        .map(walkdir::DirEntry::into_path)
        .collect()
}

/// Scans each distinct file once and returns how many were scanned and
/// every match with its path.
fn scan_corpus(
    db: &FeedDb,
    files: &[PathBuf],
) -> Result<(usize, Vec<(PathBuf, Match)>), BuildError> {
    let engine = Engine::build(&EngineParts {
        revision: db.manifest.revision.clone(),
        feed: Some(db),
        yara: true,
        ..EngineParts::default()
    })?;
    engine.self_test()?;
    let mut scanner = engine.scanner();
    let mut seen = HashSet::new();
    let mut hits = Vec::new();
    for path in files {
        let Ok(data) = std::fs::read(path) else {
            continue;
        };
        if !seen.insert(sha256_hex(&data)) {
            continue;
        }
        if let Some(m) = scanner.scan(&data) {
            hits.push((path.clone(), m));
        }
    }
    Ok((seen.len(), hits))
}

/// Removes `drop` from every hash set and signature hash list, drops
/// signatures left with nothing to match, and returns the hashes removed.
fn remove_hashes(
    db: &mut FeedDb,
    drop: &HashSet<[u8; 32]>,
) -> Result<HashSet<[u8; 32]>, BuildError> {
    let mut removed = HashSet::new();
    if drop.is_empty() {
        return Ok(removed);
    }
    for set in &mut db.hash_sets {
        let digests = set.digests()?;
        let before = digests.len();
        let kept: Vec<[u8; 32]> = digests
            .into_iter()
            .filter(|h| {
                let hit = drop.contains(h);
                if hit {
                    removed.insert(*h);
                }
                !hit
            })
            .collect();
        if kept.len() != before {
            *set = HashSet32::pack(&set.name, set.severity, &set.description, &kept);
        }
    }
    for sig in &mut db.signatures {
        sig.sha256.retain(|hex| {
            let hit = decode_digest(&hex.to_ascii_lowercase()).filter(|h| drop.contains(h));
            if let Some(h) = hit {
                removed.insert(h);
            }
            hit.is_none()
        });
    }
    db.signatures
        .retain(|s| !(s.sha256.is_empty() && s.strings.is_empty() && s.hex.is_empty()));
    Ok(removed)
}

/// Prunes `db` until no corpus file matches: feed hashes of corpus files
/// are removed, YARA rules and feed signatures that match are disabled.
/// Pruning only removes detections, so later rounds rescan hits only.
fn prune(db: &mut FeedDb, input: &BuildInput, report: &mut BuildReport) -> Result<(), BuildError> {
    let (_, mut hits) = scan_corpus(db, &corpus_files(&input.corpus))?;
    while !hits.is_empty() {
        let candidates: HashSet<[u8; 32]> = hits
            .iter()
            .filter_map(|(_, m)| decode_digest(&m.sha256))
            .collect();
        let removed = remove_hashes(db, &candidates)?;
        let mut progressed = !removed.is_empty();
        for (path, m) in &hits {
            if decode_digest(&m.sha256).is_some_and(|h| removed.contains(&h)) {
                report
                    .pruned
                    .push(format!("hash {} {} ({})", m.sha256, m.name, path.display()));
                continue;
            }
            if let Some(rule) = m.name.strip_prefix(YARA_PREFIX) {
                if !db.yara_disabled.iter().any(|r| r == rule) {
                    db.yara_disabled.push(rule.to_string());
                    report
                        .pruned
                        .push(format!("rule {} ({})", m.name, path.display()));
                    progressed = true;
                }
            } else if db.signatures.iter().any(|s| s.name == m.name) {
                db.signatures.retain(|s| s.name != m.name);
                report
                    .pruned
                    .push(format!("signature {} ({})", m.name, path.display()));
                progressed = true;
            }
        }
        if report.pruned.len() > input.max_pruned {
            return Err(BuildError::TooManyPruned {
                count: report.pruned.len(),
                max: input.max_pruned,
                first: report.pruned[0].clone(),
            });
        }
        if !progressed {
            let (path, m) = &hits[0];
            return Err(BuildError::Unprunable {
                path: path.display().to_string(),
                name: m.name.clone(),
            });
        }
        let paths: Vec<PathBuf> = hits.into_iter().map(|(p, _)| p).collect();
        hits = scan_corpus(db, &paths)?.1;
    }
    db.yara_disabled.sort();
    Ok(())
}

/// Reads the rule files under `dir` into `yara`, keeping each file that
/// compiles at least one rule, and returns the source with its provenance
/// from `source.json` when present.
fn collect_yara(
    dir: &Path,
    linux_only: bool,
    yara: &mut Vec<YaraSource>,
    report: &mut BuildReport,
) -> Result<Source, BuildError> {
    let mut rules = 0usize;
    for file in files_with(dir, &["yar", "yara"]) {
        let mut text = read(&file)?;
        if linux_only {
            let (kept, foreign) = super::platform::linux_rules(&text);
            report.foreign_yara += foreign.len();
            text = kept;
        }
        let src = YaraSource {
            name: file
                .strip_prefix(dir)
                .unwrap_or(&file)
                .display()
                .to_string(),
            source: text,
        };
        let (compiled, dropped) = compile_yara(std::slice::from_ref(&src));
        report.dropped_yara.extend(dropped);
        if let Some(compiled) = compiled {
            rules += compiled.iter().len();
            report.yara_files += 1;
            yara.push(src);
        }
    }
    report.yara_rules += rules;
    let mut source = source_meta(dir)?;
    source.count = rules as u64;
    Ok(source)
}

pub fn build(input: &BuildInput) -> Result<(FeedDb, BuildReport), BuildError> {
    let mut report = BuildReport::default();
    let mut sources = Vec::new();
    let mut hash_sets = Vec::new();
    for dir in &input.hash_dirs {
        let (source, set) = collect_hashes(dir)?;
        sources.push(source);
        hash_sets.push(set);
    }

    let mut signatures: Vec<Signature> = Vec::new();
    for path in &input.signatures {
        let list: Vec<Signature> =
            serde_yaml::from_str(&read(path)?).map_err(|e| BuildError::Parse {
                path: path.display().to_string(),
                reason: e.to_string(),
            })?;
        SignatureDb::compile(&list).map_err(|e| BuildError::Parse {
            path: path.display().to_string(),
            reason: e.to_string(),
        })?;
        sources.push(Source {
            name: path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default(),
            count: list.len() as u64,
            ..Source::default()
        });
        signatures.extend(list);
    }

    let mut yara = Vec::new();
    for dir in &input.yara_dirs {
        sources.push(collect_yara(
            dir,
            input.yara_linux_only,
            &mut yara,
            &mut report,
        )?);
    }

    let mut db = FeedDb {
        manifest: Manifest {
            revision: input
                .revision
                .clone()
                .unwrap_or_else(|| Utc::now().format("%Y%m%d.%H%M").to_string()),
            created: Utc::now(),
            min_engine_version: ENGINE_VERSION,
            sources,
        },
        signatures,
        hash_sets,
        yara,
        yara_disabled: Vec::new(),
    };

    finish(&mut db, input, &mut report)?;
    Ok((db, report))
}

/// Applies the allowlists and the corpus, then counts what is left.
fn finish(db: &mut FeedDb, input: &BuildInput, report: &mut BuildReport) -> Result<(), BuildError> {
    let mut allow = HashSet::new();
    for path in &input.allowlists {
        allow.extend(parse_hash_dump(&read(path)?));
    }
    report.allowlisted += remove_hashes(db, &allow)?.len();
    if !input.corpus.is_empty() {
        prune(db, input, report)?;
    }

    for set in &db.hash_sets {
        let count = set.digests()?.len();
        report.hashes += count;
        if let Some(source) = db
            .manifest
            .sources
            .iter_mut()
            .find(|s| s.name == set.name && s.count == 0)
        {
            source.count = count as u64;
        }
    }
    report.signatures = db.signatures.len();
    Ok(())
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct CheckReport {
    pub scanned: usize,
    pub hits: Vec<String>,
}

/// Compiles `db`, runs the EICAR self-test, and scans every distinct corpus
/// file. Any corpus match is a false positive.
pub fn check(db: &FeedDb, corpus: &[PathBuf]) -> Result<CheckReport, BuildError> {
    let (scanned, hits) = scan_corpus(db, &corpus_files(corpus))?;
    let report = CheckReport {
        scanned,
        hits: hits
            .into_iter()
            .map(|(p, m)| format!("{} ({})", p.display(), m.name))
            .collect(),
    };
    if let Some(first) = report.hits.first() {
        return Err(BuildError::FalsePositive {
            hits: report.hits.len(),
            first: first.clone(),
        });
    }
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hash_dumps_skip_comments_and_junk() {
        let a = "a".repeat(64);
        let text = format!(
            "# header\n{a}\n\"{}\"\nnot a hash\n{}\n",
            "b".repeat(64),
            "z".repeat(64)
        );
        let h = parse_hash_dump(&text);
        assert_eq!(h.len(), 2);
        assert_eq!(h[0], [0xaa; 32]);
    }

    #[test]
    fn builds_and_checks_a_database() {
        let dir = tempfile::tempdir().unwrap();
        let mb = dir.path().join("mb");
        std::fs::create_dir_all(&mb).unwrap();
        std::fs::write(
            mb.join("mb.txt"),
            format!("# x\n{}\n{}\n", "c".repeat(64), "c".repeat(64)),
        )
        .unwrap();
        let ydir = dir.path().join("yara");
        std::fs::create_dir_all(ydir.join("sub")).unwrap();
        std::fs::write(
            ydir.join("sub/good.yar"),
            r#"rule FeedEvil { strings: $a = "FEEDEVILMARKER" condition: $a }"#,
        )
        .unwrap();
        std::fs::write(ydir.join("bad.yara"), "rule {").unwrap();
        std::fs::write(ydir.join("readme.txt"), "x").unwrap();
        let sigs = dir.path().join("sigs.yaml");
        std::fs::write(
            &sigs,
            "- name: Feed.Pattern\n  severity: Low\n  strings: [patternmarker]\n",
        )
        .unwrap();
        let (db, report) = build(&BuildInput {
            revision: Some("test".into()),
            hash_dirs: vec![mb],
            yara_dirs: vec![ydir],
            signatures: vec![sigs],
            ..BuildInput::default()
        })
        .unwrap();
        assert_eq!(db.manifest.revision, "test");
        assert_eq!(report.hashes, 1);
        assert_eq!(report.signatures, 1);
        assert_eq!(report.yara_files, 1);
        assert_eq!(report.yara_rules, 1);
        assert_eq!(report.dropped_yara.len(), 1);
        assert_eq!(db.manifest.sources.len(), 3);
        assert_eq!(db.manifest.sources[0].sha256.len(), 64);
        assert_eq!(db.manifest.sources[2].name, "yara");
        assert_eq!(db.manifest.sources[2].count, 1);

        let clean = dir.path().join("corpus");
        std::fs::create_dir_all(&clean).unwrap();
        std::fs::write(clean.join("ok.bin"), b"hello").unwrap();
        let r = check(&db, std::slice::from_ref(&clean)).unwrap();
        assert_eq!(r.scanned, 1);
        std::fs::write(clean.join("bad.bin"), b"..FEEDEVILMARKER..").unwrap();
        assert!(matches!(
            check(&db, &[clean]),
            Err(BuildError::FalsePositive { hits: 1, .. })
        ));

        let broken = dir.path().join("broken.yaml");
        std::fs::write(&broken, "- name: X\n  severity: Low\n").unwrap();
        assert!(
            build(&BuildInput {
                signatures: vec![broken],
                ..BuildInput::default()
            })
            .is_err()
        );
        assert!(
            build(&BuildInput {
                hash_dirs: vec![dir.path().join("missing")],
                ..BuildInput::default()
            })
            .is_err()
        );
    }

    #[test]
    fn prunes_known_good_hashes_and_rules() {
        let dir = tempfile::tempdir().unwrap();
        let corpus = dir.path().join("corpus");
        std::fs::create_dir_all(corpus.join("usr/bin")).unwrap();
        let tool = b"a legitimate tool".to_vec();
        let packed = b"..BENIGNMARKER..".to_vec();
        let lib = b"lib with SIGMARKER".to_vec();
        std::fs::write(corpus.join("usr/bin/tool"), &tool).unwrap();
        std::fs::write(corpus.join("usr/bin/tool-copy"), &tool).unwrap();
        std::fs::write(corpus.join("usr/bin/packed"), &packed).unwrap();
        std::fs::write(corpus.join("usr/bin/lib.so"), &lib).unwrap();

        let allowed = "d".repeat(64);
        let evil = "e".repeat(64);
        let mb = dir.path().join("mb");
        std::fs::create_dir_all(&mb).unwrap();
        std::fs::write(
            mb.join("a.txt"),
            format!("{}\n{allowed}\n", sha256_hex(&tool)),
        )
        .unwrap();
        std::fs::write(mb.join("b.txt"), format!("{evil}\n")).unwrap();
        let allow = dir.path().join("allow.txt");
        std::fs::write(&allow, format!("# local exceptions\n{allowed}\n")).unwrap();
        let ydir = dir.path().join("yara");
        std::fs::create_dir_all(&ydir).unwrap();
        std::fs::write(
            ydir.join("core.yar"),
            r#"rule Noisy { strings: $a = "BENIGNMARKER" condition: $a } rule Real { strings: $a = "REALEVILMARKER" condition: $a }"#,
        )
        .unwrap();
        let sigs = dir.path().join("sigs.yaml");
        std::fs::write(
            &sigs,
            format!(
                "- name: Feed.Noisy\n  severity: Low\n  strings: [SIGMARKER]\n- name: Feed.Hashes\n  severity: High\n  sha256: [\"{}\", \"{evil}\"]\n",
                sha256_hex(&tool).to_uppercase()
            ),
        )
        .unwrap();

        let input = BuildInput {
            revision: Some("p".into()),
            hash_dirs: vec![mb],
            yara_dirs: vec![ydir],
            signatures: vec![sigs],
            allowlists: vec![allow],
            corpus: vec![corpus.clone()],
            max_pruned: 5,
            ..BuildInput::default()
        };
        let (db, report) = build(&input).unwrap();
        assert_eq!(report.hashes, 1, "only the unknown hash stays");
        assert_eq!(report.allowlisted, 1);
        assert_eq!(db.manifest.sources[0].count, 1);
        assert_eq!(db.yara_disabled, vec!["default.Noisy".to_string()]);
        assert_eq!(report.pruned.len(), 3, "{:?}", report.pruned);
        assert!(report.pruned.iter().any(|p| p.starts_with("hash ")));
        assert_eq!(report.signatures, 1);
        assert_eq!(db.signatures[0].name, "Feed.Hashes");
        assert_eq!(db.signatures[0].sha256, vec![evil]);
        let checked = check(&db, std::slice::from_ref(&corpus)).unwrap();
        assert_eq!(checked.scanned, 3, "duplicate files are scanned once");

        let engine = Engine::build(&EngineParts {
            feed: Some(&db),
            yara: true,
            ..EngineParts::default()
        })
        .unwrap();
        assert_eq!(
            engine.scanner().scan(b"REALEVILMARKER").unwrap().name,
            "yara:default.Real"
        );

        assert!(matches!(
            build(&BuildInput {
                max_pruned: 2,
                ..input
            }),
            Err(BuildError::TooManyPruned {
                count: 3,
                max: 2,
                ..
            })
        ));

        std::fs::write(corpus.join("eicar.com"), crate::content::tests::eicar()).unwrap();
        assert!(matches!(
            build(&BuildInput {
                corpus: vec![corpus],
                max_pruned: 5,
                ..BuildInput::default()
            }),
            Err(BuildError::Unprunable { .. })
        ));
    }

    #[test]
    fn yara_provenance_and_partial_files() {
        let dir = tempfile::tempdir().unwrap();
        let ydir = dir.path().join("forge");
        std::fs::create_dir_all(&ydir).unwrap();
        std::fs::write(
            ydir.join("core.yar"),
            "rule Kept { condition: false }\nrule Broken { condition: nope }\n",
        )
        .unwrap();
        std::fs::write(
            ydir.join(SOURCE_META),
            r#"{"name":"YARA Forge core","license":"per rule","url":"https://example.com","version":"20261108","sha256":"abc"}"#,
        )
        .unwrap();
        let (db, report) = build(&BuildInput {
            yara_dirs: vec![ydir.clone()],
            ..BuildInput::default()
        })
        .unwrap();
        assert_eq!(report.yara_files, 1);
        assert_eq!(report.yara_rules, 1);
        assert_eq!(report.dropped_yara.len(), 1);
        let src = &db.manifest.sources[0];
        assert_eq!(src.name, "YARA Forge core");
        assert_eq!(src.version, "20261108");
        assert_eq!(src.sha256, "abc");
        assert_eq!(src.count, 1);
        assert_eq!(FeedDb::decode(&db.encode()).unwrap(), db);

        std::fs::write(
            ydir.join("win.yar"),
            "rule Win { condition: false }\nrule WinOnly\n{\n\tcondition:\n\t\tuint16( 0 ) == 0x5a4d\n}\n",
        )
        .unwrap();
        let (_, report) = build(&BuildInput {
            yara_dirs: vec![ydir.clone()],
            yara_linux_only: true,
            ..BuildInput::default()
        })
        .unwrap();
        assert_eq!(report.foreign_yara, 1);
        assert_eq!(report.yara_rules, 2);

        std::fs::write(ydir.join(SOURCE_META), "{").unwrap();
        assert!(matches!(
            build(&BuildInput {
                yara_dirs: vec![ydir],
                ..BuildInput::default()
            }),
            Err(BuildError::Parse { .. })
        ));
    }
}
