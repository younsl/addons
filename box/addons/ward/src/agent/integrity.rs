//! File integrity monitoring of host files: a hash baseline persisted on
//! the node, compared on every interval.

use std::collections::BTreeMap;
use std::path::Path;

use crate::signature::sha256_hex;

pub const BASELINE_FILE: &str = "integrity.json";
const MAX_FILE: u64 = 16 * 1024 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Change {
    Created,
    Modified,
    Deleted,
}

impl Change {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Created => "created",
            Self::Modified => "modified",
            Self::Deleted => "deleted",
        }
    }
}

/// Path inside the host to its SHA-256.
pub type Baseline = BTreeMap<String, String>;

fn hash_file(p: &Path) -> Option<String> {
    let m = std::fs::metadata(p).ok()?;
    if !m.is_file() || m.len() > MAX_FILE {
        return None;
    }
    std::fs::read(p).ok().map(|d| sha256_hex(&d))
}

/// Hashes each watched path under `host_root`. A directory contributes its
/// files one level deep.
pub fn snapshot(host_root: &Path, paths: &[String]) -> Baseline {
    let mut out = Baseline::new();
    for p in paths {
        let real = host_root.join(p.trim_start_matches('/'));
        if real.is_dir() {
            let Ok(rd) = std::fs::read_dir(&real) else {
                continue;
            };
            for e in rd.filter_map(Result::ok) {
                if let Some(h) = hash_file(&e.path()) {
                    out.insert(
                        format!(
                            "{}/{}",
                            p.trim_end_matches('/'),
                            e.file_name().to_string_lossy()
                        ),
                        h,
                    );
                }
            }
        } else if let Some(h) = hash_file(&real) {
            out.insert(p.clone(), h);
        }
    }
    out
}

pub fn diff(old: &Baseline, new: &Baseline) -> Vec<(String, Change)> {
    let mut out = Vec::new();
    for (k, v) in new {
        match old.get(k) {
            None => out.push((k.clone(), Change::Created)),
            Some(o) if o != v => out.push((k.clone(), Change::Modified)),
            _ => {}
        }
    }
    for k in old.keys() {
        if !new.contains_key(k) {
            out.push((k.clone(), Change::Deleted));
        }
    }
    out
}

pub fn load(state: &Path) -> Option<Baseline> {
    serde_json::from_slice(&std::fs::read(state.join(BASELINE_FILE)).ok()?).ok()
}

pub fn save(state: &Path, b: &Baseline) -> std::io::Result<()> {
    std::fs::create_dir_all(state)?;
    std::fs::write(
        state.join(BASELINE_FILE),
        serde_json::to_vec(b).unwrap_or_default(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_every_change_and_persists() {
        let host = tempfile::tempdir().unwrap();
        let state = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(host.path().join("etc/cron.d")).unwrap();
        std::fs::write(host.path().join("etc/passwd"), "root:x:0:0").unwrap();
        std::fs::write(host.path().join("etc/cron.d/a"), "x").unwrap();
        let paths = vec![
            "/etc/passwd".to_string(),
            "/etc/cron.d".into(),
            "/etc/missing".into(),
        ];
        let before = snapshot(host.path(), &paths);
        assert_eq!(before.len(), 2);
        save(state.path(), &before).unwrap();
        assert_eq!(load(state.path()).unwrap(), before);

        std::fs::write(host.path().join("etc/passwd"), "root:x:0:0\nevil:x:0:0").unwrap();
        std::fs::write(host.path().join("etc/cron.d/b"), "y").unwrap();
        std::fs::remove_file(host.path().join("etc/cron.d/a")).unwrap();
        let after = snapshot(host.path(), &paths);
        let mut d = diff(&before, &after);
        d.sort();
        assert_eq!(
            d,
            vec![
                ("/etc/cron.d/a".to_string(), Change::Deleted),
                ("/etc/cron.d/b".to_string(), Change::Created),
                ("/etc/passwd".to_string(), Change::Modified),
            ]
        );
        assert_eq!(Change::Modified.as_str(), "modified");
        assert_eq!(Change::Created.as_str(), "created");
        assert_eq!(Change::Deleted.as_str(), "deleted");
        assert!(load(host.path()).is_none());
    }
}
