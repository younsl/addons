//! The signature database: the format the feed ships, its signature check,
//! and the engine agents compile it into.

pub mod index;

use std::collections::{HashMap, HashSet};
use std::io::Read as _;
use std::path::Path;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as B64;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::crd::Indicators;
use crate::policy::Severity;
use crate::signature::{Match, Signature, SignatureDb, SignatureError, builtin, sha256_hex};

pub use index::HashIndex;

pub const MEDIA_TYPE_DB: &str = "application/vnd.ward.db.v1+gzip";
pub const MEDIA_TYPE_SIG: &str = "application/vnd.ward.db.signature.v1";
/// The database format this binary reads. A feed that needs a newer engine
/// is refused instead of half-loaded.
pub const ENGINE_VERSION: u32 = 1;
/// Upper bound of a decompressed database, against gzip bombs.
pub const MAX_DB_BYTES: u64 = 1 << 30;

#[derive(Debug, thiserror::Error)]
pub enum ContentError {
    #[error("database is not valid gzip JSON: {0}")]
    Decode(String),
    #[error("database needs engine version {need}, this agent has {ENGINE_VERSION}")]
    Engine { need: u32 },
    #[error("signature check failed: {0}")]
    Signature(String),
    #[error("digest mismatch: expected {expected}, got {actual}")]
    Digest { expected: String, actual: String },
    #[error(transparent)]
    Rules(#[from] SignatureError),
    #[error("hash index: {0}")]
    Index(#[from] std::io::Error),
    #[error("self test failed: {0}")]
    SelfTest(String),
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct Source {
    pub name: String,
    #[serde(default)]
    pub license: String,
    #[serde(default)]
    pub url: String,
    /// Upstream release the content came from, when it has one.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub version: String,
    /// SHA-256 of the downloaded upstream artifact.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub sha256: String,
    #[serde(default)]
    pub count: u64,
    /// Severity of the hash set a `--hash-dir` source becomes, `High` when
    /// unset. `Protect` acts on `High` and above only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub severity: Option<Severity>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct Manifest {
    pub revision: String,
    pub created: DateTime<Utc>,
    pub min_engine_version: u32,
    #[serde(default)]
    pub sources: Vec<Source>,
}

/// A large set of hashes under one detection name, packed as base64 of the
/// concatenated 32-byte digests.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct HashSet32 {
    pub name: String,
    pub severity: Severity,
    #[serde(default)]
    pub description: String,
    pub data: String,
}

impl HashSet32 {
    pub fn pack(name: &str, severity: Severity, description: &str, hashes: &[[u8; 32]]) -> Self {
        let mut raw = Vec::with_capacity(hashes.len() * 32);
        for h in hashes {
            raw.extend_from_slice(h);
        }
        Self {
            name: name.to_string(),
            severity,
            description: description.to_string(),
            data: B64.encode(raw),
        }
    }

    fn raw(&self) -> Result<Vec<u8>, ContentError> {
        let raw = B64
            .decode(&self.data)
            .map_err(|e| ContentError::Decode(format!("hash set {}: {e}", self.name)))?;
        if raw.len() % 32 != 0 {
            return Err(ContentError::Decode(format!(
                "hash set {} is not a multiple of 32 bytes",
                self.name
            )));
        }
        Ok(raw)
    }

    pub fn digests(&self) -> Result<Vec<[u8; 32]>, ContentError> {
        Ok(self.raw()?.as_chunks::<32>().0.to_vec())
    }

    pub fn count(&self) -> Result<usize, ContentError> {
        Ok(self.raw()?.len() / 32)
    }
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct YaraSource {
    pub name: String,
    pub source: String,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct FeedDb {
    pub manifest: Manifest,
    #[serde(default)]
    pub signatures: Vec<Signature>,
    #[serde(default)]
    pub hash_sets: Vec<HashSet32>,
    #[serde(default)]
    pub yara: Vec<YaraSource>,
    /// YARA rules, as `namespace.identifier`, that the engine ignores
    /// because they matched the false-positive corpus at build time.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub yara_disabled: Vec<String>,
}

impl FeedDb {
    pub fn encode(&self) -> Vec<u8> {
        let json = serde_json::to_vec(self).unwrap_or_default();
        let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::best());
        let _ = std::io::Write::write_all(&mut gz, &json);
        gz.finish().unwrap_or_default()
    }

    /// Decodes without holding the decompressed JSON in memory.
    pub fn decode(bytes: &[u8]) -> Result<Self, ContentError> {
        let json = std::io::BufReader::new(flate2::read::GzDecoder::new(bytes).take(MAX_DB_BYTES));
        let db: Self =
            serde_json::from_reader(json).map_err(|e| ContentError::Decode(e.to_string()))?;
        if db.manifest.min_engine_version > ENGINE_VERSION {
            return Err(ContentError::Engine {
                need: db.manifest.min_engine_version,
            });
        }
        Ok(db)
    }

    /// Checks what can fail an engine build, without building one: hash sets
    /// are whole digests and signatures compile. YARA rules are left to the
    /// agents, which drop a rule that fails and keep the rest.
    pub fn validate(&self) -> Result<(), ContentError> {
        for set in &self.hash_sets {
            set.count()?;
        }
        SignatureDb::compile(&self.signatures)?;
        Ok(())
    }
}

/// `sha256:<hex>` of `data`, the OCI digest form.
pub fn oci_digest(data: &[u8]) -> String {
    format!("sha256:{}", sha256_hex(data))
}

pub fn check_digest(data: &[u8], expected: &str) -> Result<(), ContentError> {
    let actual = oci_digest(data);
    if actual == expected {
        Ok(())
    } else {
        Err(ContentError::Digest {
            expected: expected.to_string(),
            actual,
        })
    }
}

fn pem_body(pem: &str) -> Result<Vec<u8>, ContentError> {
    let body: String = pem
        .lines()
        .filter(|l| !l.starts_with("-----"))
        .map(str::trim)
        .collect();
    B64.decode(body)
        .map_err(|e| ContentError::Signature(format!("public key is not PEM: {e}")))
}

/// The uncompressed EC point of a P-256 `SubjectPublicKeyInfo`.
fn p256_point(spki: &[u8]) -> Result<&[u8], ContentError> {
    const P256_SPKI_PREFIX: [u8; 26] = [
        0x30, 0x59, 0x30, 0x13, 0x06, 0x07, 0x2a, 0x86, 0x48, 0xce, 0x3d, 0x02, 0x01, 0x06, 0x08,
        0x2a, 0x86, 0x48, 0xce, 0x3d, 0x03, 0x01, 0x07, 0x03, 0x42, 0x00,
    ];
    if spki.len() == 91 && spki[..26] == P256_SPKI_PREFIX && spki[26] == 0x04 {
        Ok(&spki[26..])
    } else {
        Err(ContentError::Signature(
            "public key must be an ECDSA P-256 SubjectPublicKeyInfo".into(),
        ))
    }
}

/// Verifies an ECDSA P-256 SHA-256 signature over `data`. The signature may
/// be DER or base64 of DER, as `cosign sign-blob` writes it.
pub fn verify_signature(data: &[u8], sig: &[u8], public_key_pem: &str) -> Result<(), ContentError> {
    use aws_lc_rs::signature::{ECDSA_P256_SHA256_ASN1, UnparsedPublicKey};
    let spki = pem_body(public_key_pem)?;
    let point = p256_point(&spki)?;
    let der = if sig.first() == Some(&0x30) {
        sig.to_vec()
    } else {
        let text: Vec<u8> = sig
            .iter()
            .copied()
            .filter(|b| !b.is_ascii_whitespace())
            .collect();
        B64.decode(&text).map_err(|e| {
            ContentError::Signature(format!("signature is neither DER nor base64: {e}"))
        })?
    };
    UnparsedPublicKey::new(&ECDSA_P256_SHA256_ASN1, point)
        .verify(data, &der)
        .map_err(|_| ContentError::Signature("signature does not verify".into()))
}

/// Severity of a YARA rule from its `severity` or `score` metadata.
fn yara_severity(rule: &yara_x::Rule<'_, '_>) -> Severity {
    let mut severity = Severity::Medium;
    for (key, value) in rule.metadata() {
        match (key, value) {
            ("severity", yara_x::MetaValue::String(s)) => {
                if let Some(s) = Severity::parse(s) {
                    return s;
                }
            }
            ("score", yara_x::MetaValue::Integer(score)) => {
                severity = match score {
                    80.. => Severity::Critical,
                    60..80 => Severity::High,
                    40..60 => Severity::Medium,
                    _ => Severity::Low,
                };
            }
            _ => {}
        }
    }
    severity
}

fn yara_description(rule: &yara_x::Rule<'_, '_>) -> String {
    rule.metadata()
        .find_map(|(k, v)| match (k, v) {
            ("description", yara_x::MetaValue::String(s)) => Some(s.to_string()),
            _ => None,
        })
        .unwrap_or_default()
}

/// Compiles YARA sources, dropping any that fail. Returns the rules and the
/// names of the sources that were dropped with their errors.
/// `<source>:<line>: <code> <title>` for one compile error.
fn describe_yara_error(src: &YaraSource, e: &yara_x::errors::CompileError) -> String {
    let line = e.labels().next().map_or(0, |l| {
        src.source
            .get(..l.span().start())
            .map_or(0, |before| before.matches('\n').count())
            + 1
    });
    format!("{}:{line}: {} {}", src.name, e.code(), e.title())
}

/// Compiles every source into one rule set. A rule that fails to compile is
/// left out and reported, the other rules in its source still compile.
pub fn compile_yara(sources: &[YaraSource]) -> (Option<yara_x::Rules>, Vec<String>) {
    if sources.is_empty() {
        return (None, Vec::new());
    }
    let mut dropped = Vec::new();
    let mut compiler = yara_x::Compiler::new();
    for src in sources {
        let before = compiler.errors().len();
        if compiler.add_source(src.source.as_str()).is_err() {
            dropped.extend(
                compiler.errors()[before..]
                    .iter()
                    .map(|e| describe_yara_error(src, e)),
            );
        }
    }
    let rules = compiler.build();
    if rules.iter().len() == 0 {
        (None, dropped)
    } else {
        (Some(rules), dropped)
    }
}

/// Everything an agent compiles into one engine.
#[derive(Default)]
pub struct EngineParts<'a> {
    pub revision: String,
    pub feed: Option<&'a FeedDb>,
    pub custom_signatures: &'a [Signature],
    pub custom_yara: &'a [YaraSource],
    pub indicators: Option<&'a Indicators>,
    pub yara: bool,
    /// Where the feed hash index is written. Without it, feed hash sets are
    /// held in memory.
    pub index_dir: Option<&'a Path>,
}

/// A compiled database. Precedence: allow indicators, block indicators,
/// then the most severe of every other source.
pub struct Engine {
    pub revision: String,
    sigs: SignatureDb,
    index: Option<HashIndex>,
    yara: Option<yara_x::Rules>,
    yara_disabled: HashSet<String>,
    allow: HashSet<String>,
    block: HashMap<String, String>,
    pub dropped_yara: Vec<String>,
    pub signature_count: usize,
}

impl Engine {
    pub fn build(parts: &EngineParts<'_>) -> Result<Self, ContentError> {
        let mut sigs = builtin();
        sigs.extend_from_slice(parts.custom_signatures);
        let mut yara_sources: Vec<YaraSource> = parts.custom_yara.to_vec();
        let mut yara_disabled = HashSet::new();
        let mut index = None;
        if let Some(db) = parts.feed {
            sigs.extend(db.signatures.iter().cloned());
            if parts.yara {
                yara_sources.extend(db.yara.iter().cloned());
                yara_disabled.extend(db.yara_disabled.iter().cloned());
            }
            let mut entries = Vec::new();
            let mut names = Vec::new();
            for set in &db.hash_sets {
                let idx = u16::try_from(names.len())
                    .map_err(|_| ContentError::Decode("too many hash sets".into()))?;
                names.push((set.name.clone(), set.severity, set.description.clone()));
                entries.extend(set.digests()?.into_iter().map(|h| (h, idx)));
            }
            if !entries.is_empty() {
                index = Some(HashIndex::build(parts.index_dir, entries, names)?);
            }
        }
        if !parts.yara {
            yara_sources.clear();
        }
        let db = SignatureDb::compile(&sigs)?;
        let (yara, dropped_yara) = compile_yara(&yara_sources);
        let yara_count = yara.as_ref().map_or(0, |r| {
            r.iter()
                .filter(|rule| {
                    !yara_disabled.contains(&format!("{}.{}", rule.namespace(), rule.identifier()))
                })
                .count()
        });
        let (allow, block): (HashSet<String>, HashMap<String, String>) =
            parts.indicators.map_or_else(Default::default, |i| {
                (
                    i.allow
                        .iter()
                        .map(|x| x.sha256.to_ascii_lowercase())
                        .collect(),
                    i.block
                        .iter()
                        .map(|x| (x.sha256.to_ascii_lowercase(), x.comment.clone()))
                        .collect(),
                )
            });
        let signature_count =
            db.len() + yara_count + index.as_ref().map_or(0, HashIndex::len) + block.len();
        Ok(Self {
            revision: parts.revision.clone(),
            sigs: db,
            index,
            yara,
            yara_disabled,
            allow,
            block,
            dropped_yara,
            signature_count,
        })
    }

    pub fn scanner(&self) -> EngineScanner<'_> {
        EngineScanner {
            engine: self,
            yara: self.yara.as_ref().map(|r| {
                let mut s = yara_x::Scanner::new(r);
                s.set_timeout(std::time::Duration::from_secs(10));
                s
            }),
        }
    }

    /// The EICAR test file must match, unless an operator allowlisted it.
    pub fn self_test(&self) -> Result<(), ContentError> {
        let mut eicar = b"X5O!P%@AP[4\\PZX54(P^)7CC)7}$".to_vec();
        eicar.extend_from_slice(b"EICAR-STANDARD-ANTIVIRUS-TEST-FILE!$H+H*");
        if self.allow.contains(&sha256_hex(&eicar)) {
            return Ok(());
        }
        match self.scanner().scan(&eicar) {
            Some(_) => Ok(()),
            None => Err(ContentError::SelfTest("EICAR did not match".into())),
        }
    }
}

/// Scans many files with one YARA scanner.
pub struct EngineScanner<'a> {
    engine: &'a Engine,
    yara: Option<yara_x::Scanner<'a>>,
}

impl EngineScanner<'_> {
    pub fn scan(&mut self, data: &[u8]) -> Option<Match> {
        let e = self.engine;
        let sha256 = sha256_hex(data);
        if e.allow.contains(&sha256) {
            return None;
        }
        if let Some(comment) = e.block.get(&sha256) {
            return Some(Match {
                name: if comment.is_empty() {
                    "Ward.Indicator.Block".into()
                } else {
                    comment.clone()
                },
                severity: Severity::Critical,
                description: "hash listed in spec.indicators.block".into(),
                sha256,
            });
        }
        let mut best = e.sigs.scan(data);
        let mut consider = |m: Match| {
            if best.as_ref().is_none_or(|b| b.severity < m.severity) {
                best = Some(m);
            }
        };
        if let Some(index) = &e.index
            && let Some((name, severity, description)) = index.lookup_hex(&sha256)
        {
            consider(Match {
                name,
                severity,
                description,
                sha256: sha256.clone(),
            });
        }
        if let Some(scanner) = self.yara.as_mut()
            && let Ok(results) = scanner.scan(data)
        {
            for rule in results.matching_rules() {
                let id = format!("{}.{}", rule.namespace(), rule.identifier());
                if e.yara_disabled.contains(&id) {
                    continue;
                }
                consider(Match {
                    name: format!("yara:{id}"),
                    severity: yara_severity(&rule),
                    description: yara_description(&rule),
                    sha256: sha256.clone(),
                });
            }
        }
        best
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::crd::Indicator;

    pub fn eicar() -> Vec<u8> {
        let mut v = b"X5O!P%@AP[4\\PZX54(P^)7CC)7}$".to_vec();
        v.extend_from_slice(b"EICAR-STANDARD-ANTIVIRUS-TEST-FILE!$H+H*");
        v
    }

    pub fn sample_db() -> FeedDb {
        let bad: [u8; 32] = sha2::Digest::finalize(
            <sha2::Sha256 as sha2::Digest>::new_with_prefix(b"known bad payload"),
        )
        .into();
        FeedDb {
            manifest: Manifest {
                revision: "r1".into(),
                created: Utc::now(),
                min_engine_version: 1,
                sources: vec![Source {
                    name: "test".into(),
                    license: "CC0-1.0".into(),
                    count: 1,
                    ..Source::default()
                }],
            },
            signatures: vec![],
            hash_sets: vec![HashSet32::pack(
                "Feed.Bad",
                Severity::High,
                "listed",
                &[bad],
            )],
            yara: vec![YaraSource {
                name: "t.yar".into(),
                source: r#"rule Evil { meta: score = 85 description = "evil marker" strings: $a = "EVILMARKER" condition: $a }"#.into(),
            }],
            yara_disabled: vec![],
        }
    }

    #[test]
    fn db_round_trips_and_refuses_newer_engines() {
        let db = sample_db();
        let bytes = db.encode();
        assert_eq!(FeedDb::decode(&bytes).unwrap(), db);
        let mut newer = db;
        newer.manifest.min_engine_version = ENGINE_VERSION + 1;
        assert!(matches!(
            FeedDb::decode(&newer.encode()),
            Err(ContentError::Engine { .. })
        ));
        assert!(matches!(
            FeedDb::decode(b"nope"),
            Err(ContentError::Decode(_))
        ));
        assert!(check_digest(&bytes, &oci_digest(&bytes)).is_ok());
        assert!(check_digest(&bytes, "sha256:00").is_err());
    }

    #[test]
    fn hash_sets_pack_and_reject_bad_data() {
        let s = HashSet32::pack("x", Severity::Low, "", &[[1; 32], [2; 32]]);
        assert_eq!(s.digests().unwrap(), vec![[1; 32], [2; 32]]);
        assert_eq!(s.count().unwrap(), 2);
        let bad = HashSet32 {
            data: B64.encode([0u8; 31]),
            ..s.clone()
        };
        assert!(bad.digests().is_err());
        let mut db = sample_db();
        db.validate().unwrap();
        db.hash_sets.push(bad);
        assert!(db.validate().is_err());
        let worse = HashSet32 {
            data: "!!".into(),
            ..s
        };
        assert!(worse.digests().is_err());
    }

    #[test]
    fn engine_matches_every_source_in_order() {
        let db = sample_db();
        let dir = tempfile::tempdir().unwrap();
        let indicators = Indicators {
            allow: vec![Indicator {
                sha256: sha256_hex(b"allowed EVILMARKER"),
                comment: String::new(),
            }],
            block: vec![Indicator {
                sha256: sha256_hex(b"blocked"),
                comment: "Team.Block".into(),
            }],
        };
        let custom = vec![Signature {
            name: "Custom.One".into(),
            severity: Severity::Low,
            description: String::new(),
            sha256: vec![],
            strings: vec!["custom-marker".into()],
            hex: vec![],
            condition: crate::signature::MatchCondition::All,
        }];
        let engine = Engine::build(&EngineParts {
            revision: "sha256:abc".into(),
            feed: Some(&db),
            custom_signatures: &custom,
            custom_yara: &[],
            indicators: Some(&indicators),
            yara: true,
            index_dir: Some(dir.path()),
        })
        .unwrap();
        engine.self_test().unwrap();
        assert!(engine.dropped_yara.is_empty());
        assert!(engine.signature_count >= 6);
        let mut s = engine.scanner();
        assert_eq!(s.scan(&eicar()).unwrap().name, "Ward.Test.EICAR");
        assert_eq!(s.scan(b"known bad payload").unwrap().name, "Feed.Bad");
        let y = s.scan(b"xx EVILMARKER xx").unwrap();
        assert_eq!(y.name, "yara:default.Evil");
        assert_eq!(y.severity, Severity::Critical);
        assert_eq!(y.description, "evil marker");
        assert_eq!(s.scan(b"a custom-marker").unwrap().name, "Custom.One");
        assert_eq!(s.scan(b"blocked").unwrap().name, "Team.Block");
        assert!(s.scan(b"allowed EVILMARKER").is_none());
        assert!(s.scan(b"clean").is_none());

        let no_yara = Engine::build(&EngineParts {
            feed: Some(&db),
            yara: false,
            ..EngineParts::default()
        })
        .unwrap();
        assert!(no_yara.scanner().scan(b"EVILMARKER").is_none());
        assert_eq!(
            no_yara.scanner().scan(b"known bad payload").unwrap().name,
            "Feed.Bad"
        );

        let disabled_db = FeedDb {
            yara_disabled: vec!["default.Evil".into()],
            ..db
        };
        let disabled = Engine::build(&EngineParts {
            feed: Some(&disabled_db),
            yara: true,
            ..EngineParts::default()
        })
        .unwrap();
        assert!(disabled.scanner().scan(b"EVILMARKER").is_none());
        assert_eq!(
            disabled.signature_count, no_yara.signature_count,
            "a disabled rule is not counted"
        );
    }

    #[test]
    fn self_test_fails_without_eicar_and_passes_when_allowed() {
        let engine = Engine {
            revision: String::new(),
            sigs: SignatureDb::default(),
            index: None,
            yara: None,
            yara_disabled: HashSet::new(),
            allow: HashSet::new(),
            block: HashMap::new(),
            dropped_yara: vec![],
            signature_count: 0,
        };
        assert!(matches!(engine.self_test(), Err(ContentError::SelfTest(_))));
        let allowed = Engine {
            allow: std::iter::once(sha256_hex(&eicar())).collect(),
            ..engine
        };
        allowed.self_test().unwrap();
    }

    #[test]
    fn bad_yara_is_dropped_not_fatal() {
        let (rules, dropped) = compile_yara(&[
            YaraSource {
                name: "bad.yar".into(),
                source: "rule {".into(),
            },
            YaraSource {
                name: "meta.yar".into(),
                source: r#"rule Low { meta: score = 10 severity = "nope" condition: true } rule Hi { meta: severity = "high" condition: false } rule Mid { meta: score = 50 condition: false } rule H2 { meta: score = 65 condition: false }"#.into(),
            },
        ]);
        assert_eq!(dropped.len(), 1);
        assert!(dropped[0].starts_with("bad.yar"));
        let rules = rules.unwrap();
        let sev: HashMap<String, Severity> = rules
            .iter()
            .map(|r| (r.identifier().to_string(), yara_severity(&r)))
            .collect();
        assert_eq!(sev["Low"], Severity::Low);
        assert_eq!(sev["Hi"], Severity::High);
        assert_eq!(sev["Mid"], Severity::Medium);
        assert_eq!(sev["H2"], Severity::High);
        assert!(compile_yara(&[]).0.is_none());
    }

    #[test]
    fn one_bad_rule_does_not_drop_its_file() {
        let (rules, dropped) = compile_yara(&[YaraSource {
            name: "core.yar".into(),
            source: "rule Good { condition: true }\nrule Bad { condition: nope }\nrule Also { condition: true }".into(),
        }]);
        let ids: Vec<String> = rules
            .unwrap()
            .iter()
            .map(|r| r.identifier().to_string())
            .collect();
        assert_eq!(ids, vec!["Good", "Also"]);
        assert_eq!(dropped.len(), 1);
        assert!(dropped[0].starts_with("core.yar:2: "), "{}", dropped[0]);
    }

    pub const TEST_PUBLIC_KEY: &str =
        "-----BEGIN PUBLIC KEY-----\nMFkwEwYHKoZIzj0CAQYIKoZIzj0DAQcDQgAE";

    #[test]
    fn signatures_verify_in_der_and_base64() {
        use aws_lc_rs::rand::SystemRandom;
        use aws_lc_rs::signature::{ECDSA_P256_SHA256_ASN1_SIGNING, EcdsaKeyPair, KeyPair as _};
        let rng = SystemRandom::new();
        let key = EcdsaKeyPair::generate(&ECDSA_P256_SHA256_ASN1_SIGNING).unwrap();
        let point = key.public_key().as_ref().to_vec();
        let mut spki = vec![
            0x30, 0x59, 0x30, 0x13, 0x06, 0x07, 0x2a, 0x86, 0x48, 0xce, 0x3d, 0x02, 0x01, 0x06,
            0x08, 0x2a, 0x86, 0x48, 0xce, 0x3d, 0x03, 0x01, 0x07, 0x03, 0x42, 0x00,
        ];
        spki.extend_from_slice(&point);
        let pem = format!(
            "-----BEGIN PUBLIC KEY-----\n{}\n-----END PUBLIC KEY-----\n",
            B64.encode(&spki)
        );
        let data = b"database bytes";
        let der = key.sign(&rng, data).unwrap().as_ref().to_vec();
        verify_signature(data, &der, &pem).unwrap();
        verify_signature(data, format!("{}\n", B64.encode(&der)).as_bytes(), &pem).unwrap();
        assert!(verify_signature(b"tampered", &der, &pem).is_err());
        assert!(verify_signature(data, b"@@@", &pem).is_err());
        assert!(verify_signature(data, &der, "not pem !!").is_err());
        assert!(verify_signature(data, &der, TEST_PUBLIC_KEY).is_err());
    }
}
