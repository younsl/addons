//! Signature (anti-malware) matching: SHA-256 indicators and byte patterns.
//! Built-in patterns are stored as hex so the agent binary never contains
//! the raw bytes it hunts for.

use std::collections::HashMap;

use kube::KubeSchema;
use memchr::memmem;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::policy::Severity;

#[derive(Debug, thiserror::Error)]
pub enum SignatureError {
    #[error("signature {name}: {reason}")]
    Invalid { name: String, reason: String },
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq, JsonSchema)]
pub enum MatchCondition {
    #[default]
    All,
    Any,
}

/// One signature. A file matches when its hash is listed, or when its
/// patterns (`strings` and `hex`) satisfy `condition`.
#[derive(KubeSchema, Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[x_kube(
    validation = Rule::new("(has(self.sha256) && size(self.sha256) > 0) || (has(self.strings) && size(self.strings) > 0) || (has(self.hex) && size(self.hex) > 0)").message("needs sha256, strings, or hex")
)]
#[serde(rename_all = "camelCase")]
pub struct Signature {
    #[schemars(length(min = 1, max = 253))]
    pub name: String,
    pub severity: Severity,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub description: String,
    /// File hashes, 64 hex digits each.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    #[schemars(length(max = 10_000), inner(regex(pattern = r"^[a-fA-F0-9]{64}$")))]
    pub sha256: Vec<String>,
    /// Literal byte strings that must occur in the file.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    #[schemars(length(max = 64), inner(length(min = 4, max = 1024)))]
    pub strings: Vec<String>,
    /// Hex byte patterns that must occur in the file.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    #[schemars(
        length(max = 64),
        inner(length(min = 8, max = 2048), regex(pattern = r"^[0-9a-fA-F\s]+$"))
    )]
    pub hex: Vec<String>,
    /// Whether `All` patterns or `Any` one of them must match.
    #[serde(default)]
    pub condition: MatchCondition,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Match {
    pub name: String,
    pub severity: Severity,
    pub description: String,
    pub sha256: String,
}

struct Compiled {
    name: String,
    severity: Severity,
    description: String,
    patterns: Vec<Vec<u8>>,
    condition: MatchCondition,
}

/// A compiled signature set.
#[derive(Default)]
pub struct SignatureDb {
    by_hash: HashMap<String, usize>,
    sigs: Vec<Compiled>,
}

fn decode_hex(name: &str, s: &str) -> Result<Vec<u8>, SignatureError> {
    let invalid = |reason: &str| SignatureError::Invalid {
        name: name.to_string(),
        reason: reason.to_string(),
    };
    let clean: String = s.chars().filter(|c| !c.is_whitespace()).collect();
    if clean.is_empty() || !clean.len().is_multiple_of(2) {
        return Err(invalid("hex pattern must have an even, non-zero length"));
    }
    (0..clean.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&clean[i..i + 2], 16).map_err(|_| invalid("bad hex digit")))
        .collect()
}

impl SignatureDb {
    pub fn compile(sigs: &[Signature]) -> Result<Self, SignatureError> {
        let mut db = Self::default();
        for sig in sigs {
            let invalid = |reason: &str| SignatureError::Invalid {
                name: sig.name.clone(),
                reason: reason.to_string(),
            };
            if sig.name.is_empty() {
                return Err(invalid("name is empty"));
            }
            let mut patterns: Vec<Vec<u8>> = sig
                .strings
                .iter()
                .filter(|s| !s.is_empty())
                .map(|s| s.as_bytes().to_vec())
                .collect();
            for h in &sig.hex {
                patterns.push(decode_hex(&sig.name, h)?);
            }
            if patterns.is_empty() && sig.sha256.is_empty() {
                return Err(invalid("needs sha256, strings, or hex"));
            }
            let idx = db.sigs.len();
            for h in &sig.sha256 {
                let h = h.trim().to_ascii_lowercase();
                if h.len() != 64 || !h.chars().all(|c| c.is_ascii_hexdigit()) {
                    return Err(invalid("sha256 must be 64 hex digits"));
                }
                db.by_hash.insert(h, idx);
            }
            db.sigs.push(Compiled {
                name: sig.name.clone(),
                severity: sig.severity,
                description: sig.description.clone(),
                patterns,
                condition: sig.condition,
            });
        }
        Ok(db)
    }

    pub const fn len(&self) -> usize {
        self.sigs.len()
    }

    fn hit(&self, idx: usize, sha256: String) -> Match {
        let s = &self.sigs[idx];
        Match {
            name: s.name.clone(),
            severity: s.severity,
            description: s.description.clone(),
            sha256,
        }
    }

    /// The most severe signature `data` matches.
    pub fn scan(&self, data: &[u8]) -> Option<Match> {
        let sha256 = sha256_hex(data);
        let mut best: Option<usize> = self.by_hash.get(&sha256).copied();
        for (idx, sig) in self.sigs.iter().enumerate() {
            if sig.patterns.is_empty() {
                continue;
            }
            let mut found = sig.patterns.iter().map(|p| memmem::find(data, p).is_some());
            let hit = match sig.condition {
                MatchCondition::All => found.all(|f| f),
                MatchCondition::Any => found.any(|f| f),
            };
            if hit && best.is_none_or(|b| self.sigs[b].severity < sig.severity) {
                best = Some(idx);
            }
        }
        best.map(|idx| self.hit(idx, sha256))
    }
}

pub fn sha256_hex(data: &[u8]) -> String {
    format!("{:x}", Sha256::digest(data))
}

/// Signatures every agent carries.
pub fn builtin() -> Vec<Signature> {
    let sig = |name: &str, severity, description: &str| Signature {
        name: name.to_string(),
        severity,
        description: description.to_string(),
        sha256: Vec::new(),
        strings: Vec::new(),
        hex: Vec::new(),
        condition: MatchCondition::All,
    };
    vec![
        Signature {
            sha256: vec!["275a021bbfb6489e54d471899f7db9d1663fc695ec2fe2a2c4538aabf651fd0f".into()],
            ..sig(
                "Ward.Test.EICAR",
                Severity::High,
                "EICAR anti-malware test file",
            )
        },
        Signature {
            hex: vec![
                "646f6e6174652e76322e786d7269672e636f6d".into(),
                "584d526967".into(),
            ],
            ..sig(
                "Ward.Miner.XMRig",
                Severity::Critical,
                "XMRig cryptocurrency miner",
            )
        },
        Signature {
            hex: vec![
                "6576616c286261736536345f6465636f646528245f504f5354".into(),
                "6576616c28245f504f53545b".into(),
            ],
            condition: MatchCondition::Any,
            ..sig(
                "Ward.Webshell.PHPEval",
                Severity::High,
                "PHP web shell evaluating request input",
            )
        },
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn eicar() -> Vec<u8> {
        let mut v = b"X5O!P%@AP[4\\PZX54(P^)7CC)7}$".to_vec();
        v.extend_from_slice(b"EICAR-STANDARD-ANTIVIRUS-TEST-FILE!$H+H*");
        v
    }

    #[test]
    fn builtins_compile_and_match() {
        let db = SignatureDb::compile(&builtin()).unwrap();
        assert_eq!(db.len(), 3);
        let m = db.scan(&eicar()).unwrap();
        assert_eq!(m.name, "Ward.Test.EICAR");
        assert_eq!(m.severity, Severity::High);
        assert_eq!(m.sha256.len(), 64);

        let miner = b"....XMRig 6.21 ... donate.v2.xmrig.com ....";
        assert_eq!(db.scan(miner).unwrap().name, "Ward.Miner.XMRig");
        assert!(db.scan(b"XMRig alone").is_none(), "All needs both");

        let shell = b"<?php eval($_POST['c']); ?>";
        assert_eq!(db.scan(shell).unwrap().name, "Ward.Webshell.PHPEval");
        assert!(db.scan(b"hello world").is_none());
    }

    #[test]
    fn most_severe_wins() {
        let sigs = vec![
            Signature {
                name: "low".into(),
                severity: Severity::Low,
                description: String::new(),
                sha256: vec![sha256_hex(b"abc")],
                strings: vec![],
                hex: vec![],
                condition: MatchCondition::All,
            },
            Signature {
                name: "crit".into(),
                severity: Severity::Critical,
                description: String::new(),
                sha256: vec![],
                strings: vec!["b".into()],
                hex: vec![],
                condition: MatchCondition::Any,
            },
        ];
        let db = SignatureDb::compile(&sigs).unwrap();
        assert_eq!(db.scan(b"abc").unwrap().name, "crit");
    }

    #[test]
    fn invalid_signatures_are_rejected() {
        let base = Signature {
            name: "x".into(),
            severity: Severity::Low,
            description: String::new(),
            sha256: vec![],
            strings: vec![],
            hex: vec![],
            condition: MatchCondition::All,
        };
        for bad in [
            base.clone(),
            Signature {
                name: String::new(),
                strings: vec!["a".into()],
                ..base.clone()
            },
            Signature {
                hex: vec!["abc".into()],
                ..base.clone()
            },
            Signature {
                hex: vec!["zz".into()],
                ..base.clone()
            },
            Signature {
                sha256: vec!["deadbeef".into()],
                ..base.clone()
            },
        ] {
            let err = SignatureDb::compile(&[bad]).err().unwrap();
            assert!(err.to_string().starts_with("signature "), "{err}");
        }
        assert!(
            SignatureDb::compile(&[Signature {
                hex: vec!["de ad".into()],
                ..base
            }])
            .is_ok()
        );
    }
}
