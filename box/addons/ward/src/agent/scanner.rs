//! File scanning: the executable of each new process, and periodic full
//! scans of every container filesystem and configured host paths.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

use walkdir::WalkDir;

use crate::content::{Engine, EngineScanner};
use crate::policy::{FileInfo, Policy, Workload};
use crate::signature::Match;

const CACHE_MAX: usize = 50_000;
/// Files between two short sleeps of a full scan, so it stays a background
/// task on busy nodes.
const FULL_SCAN_BATCH: usize = 200;

/// A file's identity: device, inode, size, and modification time.
pub type FileKey = (u64, u64, u64, i64);

#[cfg(unix)]
pub fn file_key(m: &std::fs::Metadata) -> FileKey {
    use std::os::unix::fs::MetadataExt as _;
    (m.dev(), m.ino(), m.size(), m.mtime())
}

#[cfg(not(unix))]
pub fn file_key(m: &std::fs::Metadata) -> FileKey {
    (0, 0, m.len(), 0)
}

/// Results by file identity, for one database revision.
#[derive(Default)]
pub struct ScanCache {
    revision: String,
    entries: HashMap<FileKey, Cached>,
}

/// A cached result: clean, or the match.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Cached {
    Clean,
    Hit(Match),
}

impl ScanCache {
    pub fn get(&mut self, revision: &str, key: &FileKey) -> Option<Cached> {
        if self.revision != revision {
            self.revision = revision.to_string();
            self.entries.clear();
        }
        self.entries.get(key).cloned()
    }

    pub fn put(&mut self, key: FileKey, result: Option<Match>) {
        if self.entries.len() >= CACHE_MAX {
            self.entries.clear();
        }
        self.entries
            .insert(key, result.map_or(Cached::Clean, Cached::Hit));
    }
}

/// Converts a userspace `st_dev` to the kernel `dev_t` encoding the BPF LSM
/// sees (`major << 20 | minor`).
pub const fn kernel_dev(st_dev: u64) -> u64 {
    let major = ((st_dev >> 8) & 0xfff) | ((st_dev >> 32) & !0xfff);
    let minor = (st_dev & 0xff) | ((st_dev >> 12) & !0xff);
    (major << 20) | (minor & 0xf_ffff)
}

#[derive(Clone, Debug)]
pub struct Finding {
    pub found: Match,
    pub file: FileInfo,
    /// The root `file.path` is relative to, such as `/proc/<pid>/root`.
    pub root: PathBuf,
    pub workload: Option<Workload>,
    /// `(st_dev, ino)` of the file.
    pub inode: (u64, u64),
}

/// Scans one file through `scanner`, using and filling `cache`.
pub fn scan_path(
    scanner: &mut EngineScanner<'_>,
    revision: &str,
    cache: &Mutex<ScanCache>,
    path: &Path,
    max_size: u64,
) -> Option<(Match, FileInfo, (u64, u64))> {
    let meta = std::fs::metadata(path).ok()?;
    if !meta.is_file() || meta.len() > max_size {
        return None;
    }
    let key = file_key(&meta);
    match cache.lock().ok()?.get(revision, &key) {
        Some(Cached::Clean) => return None,
        Some(Cached::Hit(m)) => {
            let f = FileInfo {
                path: path.display().to_string(),
                sha256: Some(m.sha256.clone()),
                size: Some(meta.len()),
            };
            return Some((m, f, (key.0, key.1)));
        }
        None => {}
    }
    let data = std::fs::read(path).ok()?;
    let result = scanner.scan(&data);
    if let Ok(mut c) = cache.lock() {
        c.put(key, result.clone());
    }
    result.map(|m| {
        let f = FileInfo {
            path: path.display().to_string(),
            sha256: Some(m.sha256.clone()),
            size: Some(meta.len()),
        };
        (m, f, (key.0, key.1))
    })
}

/// One directory tree a full scan walks.
#[derive(Clone, Debug)]
pub struct ScanRoot {
    /// Where the tree is reached, such as `/proc/<pid>/root`.
    pub base: PathBuf,
    /// Directories under `base` to walk, `/` for everything.
    pub dirs: Vec<String>,
    pub workload: Option<Workload>,
}

/// Walks every root, staying on one filesystem per root, and returns the
/// malicious files. Stops early when `cancel` is set.
pub fn full_scan(
    engine: &Engine,
    roots: &[ScanRoot],
    policy: &Policy,
    cache: &Mutex<ScanCache>,
    cancel: &AtomicBool,
    scanned: &dyn Fn(),
) -> Vec<Finding> {
    let mut out = Vec::new();
    let mut scanner = engine.scanner();
    let mut n = 0usize;
    for root in roots {
        for dir in &root.dirs {
            let start = root.base.join(dir.trim_start_matches('/'));
            for entry in WalkDir::new(&start)
                .follow_links(false)
                .same_file_system(true)
                .into_iter()
                .filter_entry(|e| {
                    let rel = e
                        .path()
                        .strip_prefix(&root.base)
                        .unwrap_or_else(|_| e.path());
                    let inner = format!("/{}", rel.display());
                    !policy.path_excluded(&inner)
                })
                .filter_map(Result::ok)
            {
                if cancel.load(Ordering::Relaxed) {
                    return out;
                }
                if !entry.file_type().is_file() {
                    continue;
                }
                n += 1;
                if n.is_multiple_of(FULL_SCAN_BATCH) {
                    std::thread::sleep(std::time::Duration::from_millis(5));
                }
                scanned();
                if let Some((found, mut file, inode)) = scan_path(
                    &mut scanner,
                    &engine.revision,
                    cache,
                    entry.path(),
                    policy.scan.max_file_size_bytes,
                ) {
                    let rel = entry
                        .path()
                        .strip_prefix(&root.base)
                        .unwrap_or_else(|_| entry.path());
                    file.path = format!("/{}", rel.display());
                    out.push(Finding {
                        found,
                        file,
                        root: root.base.clone(),
                        workload: root.workload.clone(),
                        inode,
                    });
                }
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::content::EngineParts;
    use crate::content::tests::eicar;

    #[test]
    fn full_scan_finds_and_caches() {
        let engine = Engine::build(&EngineParts {
            revision: "r".into(),
            ..EngineParts::default()
        })
        .unwrap();
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("app/skip")).unwrap();
        std::fs::write(dir.path().join("app/eicar.com"), eicar()).unwrap();
        std::fs::write(dir.path().join("app/skip/eicar.com"), eicar()).unwrap();
        std::fs::write(dir.path().join("app/big"), vec![0u8; 4096]).unwrap();
        std::fs::write(dir.path().join("app/ok"), b"fine").unwrap();
        let mut policy = Policy::default();
        policy.scan.max_file_size_bytes = 1024;
        policy.exclusions.paths = vec!["/app/skip".into()];
        let cache = Mutex::new(ScanCache::default());
        let counter = std::sync::atomic::AtomicUsize::new(0);
        let roots = vec![ScanRoot {
            base: dir.path().to_path_buf(),
            dirs: vec!["/".into()],
            workload: None,
        }];
        let found = full_scan(
            &engine,
            &roots,
            &policy,
            &cache,
            &AtomicBool::new(false),
            &|| {
                counter.fetch_add(1, Ordering::Relaxed);
            },
        );
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].file.path, "/app/eicar.com");
        assert_eq!(found[0].found.name, "Ward.Test.EICAR");
        assert_eq!(counter.load(Ordering::Relaxed), 3);

        let mut s = engine.scanner();
        let hit = scan_path(&mut s, "r", &cache, &dir.path().join("app/eicar.com"), 1024);
        assert!(hit.is_some(), "cached hit");
        assert!(scan_path(&mut s, "r", &cache, &dir.path().join("app/ok"), 1024).is_none());
        assert!(scan_path(&mut s, "r", &cache, dir.path(), 1024).is_none());
        assert!(
            full_scan(
                &engine,
                &roots,
                &policy,
                &cache,
                &AtomicBool::new(true),
                &|| {}
            )
            .is_empty()
        );
    }

    #[test]
    fn cache_resets_per_revision() {
        let mut c = ScanCache::default();
        c.put((1, 2, 3, 4), None);
        assert_eq!(c.get("", &(1, 2, 3, 4)), Some(Cached::Clean));
        assert_eq!(c.get("other", &(1, 2, 3, 4)), None);
    }

    #[test]
    fn dev_encoding() {
        let st_dev = (8u64 << 8) | 1;
        assert_eq!(kernel_dev(st_dev), (8 << 20) | 1);
        let big = ((0x1234u64 & !0xfff) << 32) | ((0x1234 & 0xfff) << 8) | 0x56;
        assert_eq!(kernel_dev(big), (0x1234 << 20) | 0x56);
    }
}
