//! `ward feed build` and `ward feed check`: assemble a database from local
//! source files and gate it on a false-positive corpus.

use std::path::{Path, PathBuf};

use chrono::Utc;
use walkdir::WalkDir;

use crate::content::{
    ContentError, ENGINE_VERSION, Engine, EngineParts, FeedDb, HashSet32, Manifest, Source,
    YaraSource, compile_yara,
};
use crate::policy::Severity;
use crate::signature::{Signature, SignatureDb};

const CORPUS_MAX_FILE: u64 = 64 * 1024 * 1024;

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
}

#[derive(Debug, Default)]
pub struct BuildInput {
    pub revision: Option<String>,
    pub malwarebazaar: Vec<PathBuf>,
    pub yara_dirs: Vec<PathBuf>,
    pub signatures: Vec<PathBuf>,
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct BuildReport {
    pub hashes: usize,
    pub signatures: usize,
    pub yara_files: usize,
    pub dropped_yara: Vec<String>,
}

fn read(path: &Path) -> Result<String, BuildError> {
    std::fs::read_to_string(path).map_err(|source| BuildError::Read {
        path: path.display().to_string(),
        source,
    })
}

/// SHA-256 digests from a hash dump: one hex digest per line, `#` comments
/// and anything else skipped.
pub fn parse_hash_dump(text: &str) -> Vec<[u8; 32]> {
    text.lines()
        .map(|l| l.trim().trim_matches('"'))
        .filter(|l| l.len() == 64 && !l.starts_with('#'))
        .filter_map(|l| {
            let mut h = [0u8; 32];
            for (i, b) in h.iter_mut().enumerate() {
                *b = u8::from_str_radix(&l[i * 2..i * 2 + 2], 16).ok()?;
            }
            Some(h)
        })
        .collect()
}

fn yara_files(dir: &Path) -> Vec<PathBuf> {
    let mut files: Vec<PathBuf> = WalkDir::new(dir)
        .follow_links(false)
        .into_iter()
        .filter_map(Result::ok)
        .filter(|e| e.file_type().is_file())
        .map(walkdir::DirEntry::into_path)
        .filter(|p| {
            p.extension()
                .is_some_and(|e| e.eq_ignore_ascii_case("yar") || e.eq_ignore_ascii_case("yara"))
        })
        .collect();
    files.sort();
    files
}

pub fn build(input: &BuildInput) -> Result<(FeedDb, BuildReport), BuildError> {
    let mut report = BuildReport::default();
    let mut sources = Vec::new();
    let mut hash_sets = Vec::new();
    for path in &input.malwarebazaar {
        let mut hashes = parse_hash_dump(&read(path)?);
        hashes.sort_unstable();
        hashes.dedup();
        report.hashes += hashes.len();
        sources.push(Source {
            name: "MalwareBazaar".into(),
            license: "CC0-1.0".into(),
            url: "https://bazaar.abuse.ch".into(),
            count: hashes.len() as u64,
        });
        hash_sets.push(HashSet32::pack(
            "MalwareBazaar",
            Severity::High,
            "SHA-256 listed by abuse.ch MalwareBazaar",
            &hashes,
        ));
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
    report.signatures = signatures.len();

    let mut yara = Vec::new();
    for dir in &input.yara_dirs {
        let mut kept = 0u64;
        for file in yara_files(dir) {
            let src = YaraSource {
                name: file
                    .strip_prefix(dir)
                    .unwrap_or(&file)
                    .display()
                    .to_string(),
                source: read(&file)?,
            };
            let (_, dropped) = compile_yara(std::slice::from_ref(&src));
            if dropped.is_empty() {
                kept += 1;
                yara.push(src);
            } else {
                report.dropped_yara.extend(dropped);
            }
        }
        report.yara_files += usize::try_from(kept).unwrap_or(usize::MAX);
        sources.push(Source {
            name: dir
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default(),
            count: kept,
            ..Source::default()
        });
    }

    let db = FeedDb {
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
    };
    Ok((db, report))
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct CheckReport {
    pub scanned: usize,
    pub hits: Vec<String>,
}

/// Compiles `db`, runs the EICAR self-test, and scans every corpus file.
/// Any corpus match is a false positive.
pub fn check(db: &FeedDb, corpus: &[PathBuf]) -> Result<CheckReport, BuildError> {
    let engine = Engine::build(&EngineParts {
        revision: db.manifest.revision.clone(),
        feed: Some(db),
        yara: true,
        ..EngineParts::default()
    })?;
    engine.self_test()?;
    let mut scanner = engine.scanner();
    let mut report = CheckReport::default();
    for dir in corpus {
        for entry in WalkDir::new(dir)
            .follow_links(false)
            .into_iter()
            .filter_map(Result::ok)
        {
            if !entry.file_type().is_file()
                || entry.metadata().map_or(true, |m| m.len() > CORPUS_MAX_FILE)
            {
                continue;
            }
            let Ok(data) = std::fs::read(entry.path()) else {
                continue;
            };
            report.scanned += 1;
            if let Some(m) = scanner.scan(&data) {
                report
                    .hits
                    .push(format!("{} ({})", entry.path().display(), m.name));
            }
        }
    }
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
        let mb = dir.path().join("mb.txt");
        std::fs::write(
            &mb,
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
            malwarebazaar: vec![mb],
            yara_dirs: vec![ydir],
            signatures: vec![sigs],
        })
        .unwrap();
        assert_eq!(db.manifest.revision, "test");
        assert_eq!(report.hashes, 1);
        assert_eq!(report.signatures, 1);
        assert_eq!(report.yara_files, 1);
        assert_eq!(report.dropped_yara.len(), 1);
        assert_eq!(db.manifest.sources.len(), 3);

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
                malwarebazaar: vec![dir.path().join("missing")],
                ..BuildInput::default()
            })
            .is_err()
        );
    }
}
