//! File integrity monitoring of host files, after Wazuh syscheck: a baseline
//! of hashes and attributes persisted on the node, checked on every interval
//! and, with real-time monitoring, on every change the kernel reports. Text
//! files keep a copy so a change can be reported as a diff.

use std::collections::BTreeMap;
use std::os::unix::fs::MetadataExt as _;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use walkdir::WalkDir;

use super::scanner::kernel_dev;
use crate::signature::sha256_hex;

pub const BASELINE_FILE: &str = "integrity.json";
const CONTENT_DIR: &str = "integrity-content";
const MAX_HASH: u64 = 64 * 1024 * 1024;
/// Larger text files are not copied, so their changes carry no diff.
const MAX_DIFF_FILE: u64 = 256 * 1024;
/// A longer diff is cut, the detection says so.
const MAX_DIFF: usize = 8 * 1024;

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

/// What the baseline keeps of one file.
#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
pub struct Entry {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sha256: Option<String>,
    pub size: u64,
    pub mode: u32,
    pub uid: u32,
    pub gid: u32,
    pub mtime: i64,
    /// Kernel `dev_t` and inode, the identity the whodata hooks report.
    pub dev: u64,
    pub ino: u64,
}

impl Entry {
    /// The attributes that differ, in Wazuh's `changed_attributes` terms.
    pub fn changed(&self, other: &Self) -> Vec<&'static str> {
        let mut out = Vec::new();
        if self.size != other.size {
            out.push("size");
        }
        if self.mode != other.mode {
            out.push("permission");
        }
        if self.uid != other.uid {
            out.push("uid");
        }
        if self.gid != other.gid {
            out.push("gid");
        }
        if self.mtime != other.mtime {
            out.push("mtime");
        }
        if self.ino != other.ino || self.dev != other.dev {
            out.push("inode");
        }
        if self.sha256 != other.sha256 {
            out.push("sha256");
        }
        out
    }
}

/// Host path to its entry.
pub type Baseline = BTreeMap<String, Entry>;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Limits {
    /// Directory levels below a watched directory, as Wazuh `recursion_level`.
    pub recursion_level: usize,
    /// Files the baseline holds at most, as Wazuh `file_limit`.
    pub file_limit: usize,
}

/// One change, with the attributes that changed for a modification.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Diff {
    pub path: String,
    pub change: Change,
    pub attributes: Vec<&'static str>,
}

/// `path` made absolute and canonical without touching the file system:
/// empty and `.` components dropped, `..` applied. Baseline keys, roots, and
/// `noDiff` rules all go through it, so `/etc//shadow` and `/etc/shadow`
/// are the same file to every check.
pub fn normalize(path: &str) -> String {
    let mut parts: Vec<&str> = Vec::new();
    for c in path.split('/') {
        match c {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            c => parts.push(c),
        }
    }
    format!("/{}", parts.join("/"))
}

pub fn normalize_all(paths: &[String]) -> Vec<String> {
    paths.iter().map(|p| normalize(p)).collect()
}

fn host(host_root: &Path, path: &str) -> PathBuf {
    host_root.join(path.trim_start_matches('/'))
}

/// The entry of one host file, `None` when it is missing or not a file.
/// Opens a regular file without following a symlink in its last component,
/// so a watched file swapped for a link to a secret is never read through.
/// Size and type come from the open descriptor, not a second lookup.
fn open_file(real: &Path) -> Option<(std::fs::File, std::fs::Metadata)> {
    use std::os::unix::fs::OpenOptionsExt as _;
    let flags = nix::fcntl::OFlag::O_NOFOLLOW | nix::fcntl::OFlag::O_NONBLOCK;
    let f = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(flags.bits())
        .open(real)
        .ok()?;
    let m = f.metadata().ok()?;
    m.is_file().then_some((f, m))
}

/// Up to `max` bytes of a regular file, read from one descriptor.
fn read_file(real: &Path, max: u64) -> Option<(Vec<u8>, std::fs::Metadata)> {
    use std::io::Read as _;
    let (f, m) = open_file(real)?;
    if m.len() > max {
        return None;
    }
    let mut data = Vec::new();
    f.take(max).read_to_end(&mut data).ok()?;
    Some((data, m))
}

pub fn entry_of(real: &Path) -> Option<Entry> {
    let m = std::fs::symlink_metadata(real).ok()?;
    if !m.is_file() {
        return None;
    }
    let sha256 = read_file(real, MAX_HASH)
        .filter(|(_, opened)| opened.ino() == m.ino() && opened.dev() == m.dev())
        .map(|(d, _)| sha256_hex(&d));
    Some(Entry {
        sha256,
        size: m.len(),
        mode: m.mode(),
        uid: m.uid(),
        gid: m.gid(),
        mtime: m.mtime(),
        dev: kernel_dev(m.dev()),
        ino: m.ino(),
    })
}

/// Adds every file under `path` (a file or a directory walked down to the
/// recursion level) to `out`. A file named as a root (`explicit`) always
/// goes in, files found by walking stop at the file limit, so flooding a
/// watched directory cannot push a named file out of the baseline. Returns
/// whether the limit left files out.
fn collect(
    host_root: &Path,
    path: &str,
    levels: usize,
    limits: Limits,
    explicit: bool,
    out: &mut Baseline,
) -> bool {
    let real = host(host_root, path);
    let Ok(m) = std::fs::symlink_metadata(&real) else {
        return false;
    };
    if !m.is_dir() {
        if !explicit && out.len() >= limits.file_limit {
            return true;
        }
        if let Some(e) = entry_of(&real) {
            out.insert(path.to_string(), e);
        }
        return false;
    }
    for e in WalkDir::new(&real)
        .follow_links(false)
        .min_depth(1)
        .max_depth(levels.saturating_add(1))
        .into_iter()
        .filter_map(Result::ok)
        .filter(|e| e.file_type().is_file())
    {
        if out.len() >= limits.file_limit {
            return true;
        }
        let rel = e.path().strip_prefix(&real).unwrap_or_else(|_| e.path());
        if let Some(entry) = entry_of(e.path()) {
            out.insert(
                format!("{}/{}", path.trim_end_matches('/'), rel.display()),
                entry,
            );
        }
    }
    false
}

/// The baseline of every watched path under `host_root`, and whether the
/// file limit left files out. Named files go in before any directory walk.
pub fn snapshot(host_root: &Path, roots: &[String], limits: Limits) -> (Baseline, bool) {
    let mut out = Baseline::new();
    let mut truncated = false;
    let (dirs, files): (Vec<&String>, Vec<&String>) =
        roots.iter().partition(|r| host(host_root, r).is_dir());
    for r in files.into_iter().chain(dirs) {
        truncated |= collect(host_root, r, limits.recursion_level, limits, true, &mut out);
    }
    (out, truncated)
}

/// The watched root that covers `path`, with the directory levels left
/// below `path`.
fn covering(roots: &[String], path: &str, limits: Limits) -> Option<usize> {
    roots.iter().find_map(|r| {
        let r = r.trim_end_matches('/');
        if path == r {
            return Some(limits.recursion_level);
        }
        let rest = path.strip_prefix(r)?.strip_prefix('/')?;
        let depth = rest.matches('/').count();
        (depth <= limits.recursion_level).then(|| limits.recursion_level - depth)
    })
}

/// `base` with the `changed` host paths checked again: each path, and
/// everything under it when it is or was a directory, is replaced by what
/// is there now. Paths outside every root are ignored.
pub fn rescan(
    host_root: &Path,
    roots: &[String],
    limits: Limits,
    base: &Baseline,
    changed: &[String],
) -> (Baseline, bool) {
    let mut out = base.clone();
    let mut truncated = false;
    for path in changed {
        let Some(levels) = covering(roots, path, limits) else {
            continue;
        };
        let prefix = format!("{}/", path.trim_end_matches('/'));
        out.retain(|k, _| k != path && !k.starts_with(&prefix));
        let explicit = roots.iter().any(|r| r == path);
        truncated |= collect(host_root, path, levels, limits, explicit, &mut out);
    }
    (out, truncated)
}

pub fn diff(old: &Baseline, new: &Baseline) -> Vec<Diff> {
    let mut out = Vec::new();
    for (k, v) in new {
        match old.get(k) {
            None => out.push(Diff {
                path: k.clone(),
                change: Change::Created,
                attributes: Vec::new(),
            }),
            Some(o) if o != v => out.push(Diff {
                path: k.clone(),
                change: Change::Modified,
                attributes: o.changed(v),
            }),
            _ => {}
        }
    }
    for k in old.keys() {
        if !new.contains_key(k) {
            out.push(Diff {
                path: k.clone(),
                change: Change::Deleted,
                attributes: Vec::new(),
            });
        }
    }
    out
}

/// Kernel `(dev_t, ino)` of every watched file and of every directory a
/// change can happen in: the watched directories and the parents of the
/// watched files.
pub fn watched_inodes(
    host_root: &Path,
    roots: &[String],
    limits: Limits,
    base: &Baseline,
) -> Vec<(u64, u64)> {
    let mut out: Vec<(u64, u64)> = base.values().map(|e| (e.dev, e.ino)).collect();
    let id = |p: &Path| {
        std::fs::metadata(p)
            .ok()
            .map(|m| (kernel_dev(m.dev()), m.ino()))
    };
    for r in roots {
        let real = host(host_root, r);
        if real.is_dir() {
            out.extend(
                WalkDir::new(&real)
                    .follow_links(false)
                    .max_depth(limits.recursion_level.saturating_add(1))
                    .into_iter()
                    .filter_map(Result::ok)
                    .filter(|e| e.file_type().is_dir())
                    .filter_map(|e| id(e.path())),
            );
        } else if let Some(parent) = real.parent() {
            out.extend(id(parent));
        }
    }
    out.sort_unstable();
    out.dedup();
    out
}

pub fn load(state: &Path) -> Option<Baseline> {
    let b = std::fs::read(state.join(BASELINE_FILE)).ok()?;
    let parsed = serde_json::from_slice(&b);
    if parsed.is_err() {
        tracing::info!("integrity baseline is in an older format, building a new one");
    }
    parsed.ok()
}

pub fn save(state: &Path, b: &Baseline) -> std::io::Result<()> {
    std::fs::create_dir_all(state)?;
    std::fs::write(
        state.join(BASELINE_FILE),
        serde_json::to_vec(b).unwrap_or_default(),
    )
}

/// Copies of watched text files, so the next change can be shown as a
/// diff, as Wazuh `report_changes`. Each copy is named by the hash of its
/// path and readable by the agent only.
pub struct Contents {
    dir: PathBuf,
}

impl Contents {
    pub fn new(state: &Path) -> Self {
        Self {
            dir: state.join(CONTENT_DIR),
        }
    }

    fn file(&self, path: &str) -> PathBuf {
        self.dir.join(sha256_hex(path.as_bytes()))
    }

    /// The text of a host file small enough to keep, `None` for binaries
    /// and for anything holding a private key, whatever `noDiff` says.
    pub fn text(real: &Path) -> Option<String> {
        let (data, _) = read_file(real, MAX_DIFF_FILE)?;
        if data.contains(&0)
            || SECRET_MARKERS
                .iter()
                .any(|m| memchr::memmem::find(&data, m).is_some())
        {
            return None;
        }
        String::from_utf8(data).ok()
    }

    pub fn previous(&self, path: &str) -> Option<String> {
        std::fs::read_to_string(self.file(path)).ok()
    }

    pub fn keep(&self, path: &str, text: Option<&str>) {
        let file = self.file(path);
        let Some(text) = text else {
            let _ = std::fs::remove_file(file);
            return;
        };
        let write = || -> std::io::Result<()> {
            use std::io::Write as _;
            use std::os::unix::fs::OpenOptionsExt as _;
            std::fs::create_dir_all(&self.dir)?;
            let tmp = file.with_extension("tmp");
            let mut f = std::fs::OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .mode(0o600)
                .open(&tmp)?;
            f.write_all(text.as_bytes())?;
            std::fs::rename(tmp, &file)
        };
        if let Err(e) = write() {
            tracing::warn!(error = %e, path, "keep integrity content");
        }
    }
}

/// Content that marks a file as a secret store: it is never copied, so it
/// never shows up in a diff. PEM private keys, base64 PEM as in kubeconfig
/// `client-key-data`, and that key itself.
const SECRET_MARKERS: [&[u8]; 3] = [b"PRIVATE KEY-----", b"LS0tLS1CRUdJTi", b"client-key-data"];
/// Keys whose values a diff line never shows.
const SECRET_KEYS: [&str; 8] = [
    "password",
    "passwd",
    "secret",
    "token",
    "apikey",
    "api_key",
    "api-key",
    "credential",
];

/// The diff line with the value of a `key = value` or `key: value` pair
/// replaced when the key names a secret.
fn redact(line: &str) -> String {
    let lower = line.to_ascii_lowercase();
    let Some(at) = lower.find(['=', ':']) else {
        return line.to_string();
    };
    if SECRET_KEYS.iter().any(|k| lower[..at].contains(k)) {
        format!("{} <redacted>", &line[..=at])
    } else {
        line.to_string()
    }
}

/// A unified diff of two texts, values of secret looking keys redacted,
/// cut at `MAX_DIFF` bytes.
pub fn unified_diff(path: &str, old: &str, new: &str) -> Option<String> {
    let raw = similar::TextDiff::from_lines(old, new)
        .unified_diff()
        .context_radius(3)
        .header(path, path)
        .to_string();
    let diff: String = raw
        .split_inclusive('\n')
        .map(|l| {
            let body = l.trim_end_matches('\n');
            let redacted = if body.starts_with("+++") || body.starts_with("---") {
                body.to_string()
            } else if let Some(rest) = body.strip_prefix(['+', '-', ' ']) {
                format!("{}{}", &body[..1], redact(rest))
            } else {
                body.to_string()
            };
            if l.ends_with('\n') {
                format!("{redacted}\n")
            } else {
                redacted
            }
        })
        .collect();
    if diff.is_empty() {
        return None;
    }
    if diff.len() <= MAX_DIFF {
        return Some(diff);
    }
    let mut end = MAX_DIFF;
    while !diff.is_char_boundary(end) {
        end -= 1;
    }
    Some(format!(
        "{}\n... diff cut at {MAX_DIFF} bytes\n",
        &diff[..end]
    ))
}

/// Whether `path` matches a `noDiff` entry: the path itself or anything
/// under it.
pub fn no_diff(rules: &[String], path: &str) -> bool {
    let path = normalize(path);
    rules.iter().any(|r| {
        let r = normalize(r);
        r == "/"
            || path == r
            || path
                .strip_prefix(&r)
                .is_some_and(|rest| rest.starts_with('/'))
    })
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt as _;

    use super::*;

    const LIMITS: Limits = Limits {
        recursion_level: 256,
        file_limit: 100_000,
    };

    #[test]
    fn detects_every_change_and_persists() {
        let host = tempfile::tempdir().unwrap();
        let state = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(host.path().join("etc/cron.d/deep")).unwrap();
        std::fs::write(host.path().join("etc/passwd"), "root:x:0:0").unwrap();
        std::fs::write(host.path().join("etc/cron.d/a"), "x").unwrap();
        std::fs::write(host.path().join("etc/cron.d/deep/c"), "z").unwrap();
        let paths = vec![
            "/etc/passwd".to_string(),
            "/etc/cron.d".into(),
            "/etc/missing".into(),
        ];
        let (before, _) = snapshot(host.path(), &paths, LIMITS);
        assert_eq!(before.len(), 3, "recursive: {before:?}");
        save(state.path(), &before).unwrap();
        assert_eq!(load(state.path()).unwrap(), before);

        std::fs::write(host.path().join("etc/passwd"), "root:x:0:0\nevil:x:0:0").unwrap();
        std::fs::write(host.path().join("etc/cron.d/b"), "y").unwrap();
        std::fs::remove_file(host.path().join("etc/cron.d/a")).unwrap();
        std::fs::set_permissions(
            host.path().join("etc/cron.d/deep/c"),
            std::fs::Permissions::from_mode(0o777),
        )
        .unwrap();
        let (after, _) = snapshot(host.path(), &paths, LIMITS);
        let mut d = diff(&before, &after);
        d.sort_by(|a, b| a.path.cmp(&b.path));
        let summary: Vec<(&str, Change)> = d.iter().map(|x| (x.path.as_str(), x.change)).collect();
        assert_eq!(
            summary,
            vec![
                ("/etc/cron.d/a", Change::Deleted),
                ("/etc/cron.d/b", Change::Created),
                ("/etc/cron.d/deep/c", Change::Modified),
                ("/etc/passwd", Change::Modified),
            ]
        );
        assert_eq!(d[2].attributes, vec!["permission"]);
        assert!(d[3].attributes.contains(&"sha256"));
        assert!(d[3].attributes.contains(&"size"));
        assert_eq!(Change::Modified.as_str(), "modified");
        assert_eq!(Change::Created.as_str(), "created");
        assert_eq!(Change::Deleted.as_str(), "deleted");
        assert!(load(host.path()).is_none());
        std::fs::write(state.path().join(BASELINE_FILE), r#"{"/etc/passwd":"abc"}"#).unwrap();
        assert!(load(state.path()).is_none(), "the old format starts over");
    }

    #[test]
    fn rescan_touches_only_changed_paths_and_limits_hold() {
        let host = tempfile::tempdir().unwrap();
        let h = host.path();
        std::fs::create_dir_all(h.join("w/sub/deeper")).unwrap();
        std::fs::write(h.join("w/a"), "1").unwrap();
        std::fs::write(h.join("w/sub/b"), "2").unwrap();
        std::fs::write(h.join("w/sub/deeper/c"), "3").unwrap();
        let roots = vec!["/w".to_string()];
        let shallow = Limits {
            recursion_level: 1,
            file_limit: 100,
        };
        let (base, truncated) = snapshot(h, &roots, shallow);
        assert!(!truncated);
        assert_eq!(base.keys().collect::<Vec<_>>(), vec!["/w/a", "/w/sub/b"]);
        let tight = Limits {
            recursion_level: 8,
            file_limit: 2,
        };
        let (few, truncated) = snapshot(h, &roots, tight);
        assert_eq!(few.len(), 2);
        assert!(truncated, "the limit is reported");
        std::fs::write(h.join("named"), "keep").unwrap();
        let (named, _) = snapshot(h, &["/w".into(), "/named".into()], tight);
        assert!(
            named.contains_key("/named"),
            "a named file survives a flooded directory"
        );

        std::fs::write(h.join("w/a"), "changed").unwrap();
        std::fs::remove_dir_all(h.join("w/sub")).unwrap();
        std::fs::write(h.join("outside"), "x").unwrap();
        let (only_a, _) = rescan(h, &roots, shallow, &base, &["/w/a".into()]);
        assert_eq!(
            diff(&base, &only_a).len(),
            1,
            "unlisted paths keep their entries"
        );
        let (both, _) = rescan(
            h,
            &roots,
            shallow,
            &base,
            &["/w/a".into(), "/w/sub".into(), "/outside".into()],
        );
        let mut d: Vec<(String, Change)> = diff(&base, &both)
            .into_iter()
            .map(|x| (x.path, x.change))
            .collect();
        d.sort();
        assert_eq!(
            d,
            vec![
                ("/w/a".to_string(), Change::Modified),
                ("/w/sub/b".to_string(), Change::Deleted),
            ]
        );
        assert_eq!(covering(&roots, "/w/sub/deeper/c", shallow), None);
        assert_eq!(covering(&roots, "/w", shallow), Some(1));

        let inodes = watched_inodes(h, &["/w".into(), "/outside".into()], shallow, &both);
        let w = std::fs::metadata(h.join("w")).unwrap();
        assert!(
            inodes.contains(&(kernel_dev(w.dev()), w.ino())),
            "the watched directory"
        );
        let parent = std::fs::metadata(h).unwrap();
        assert!(
            inodes.contains(&(kernel_dev(parent.dev()), parent.ino())),
            "parent of a file"
        );
    }

    #[test]
    fn keeps_text_and_reports_a_diff() {
        let host = tempfile::tempdir().unwrap();
        let state = tempfile::tempdir().unwrap();
        let file = host.path().join("sudoers");
        std::fs::write(&file, "root ALL=(ALL) ALL\n").unwrap();
        let contents = Contents::new(state.path());
        assert!(contents.previous("/etc/sudoers").is_none());
        contents.keep("/etc/sudoers", Contents::text(&file).as_deref());
        let kept = std::fs::metadata(contents.file("/etc/sudoers")).unwrap();
        assert_eq!(kept.permissions().mode() & 0o777, 0o600);

        std::fs::write(&file, "root ALL=(ALL) ALL\nevil ALL=(ALL) NOPASSWD: ALL\n").unwrap();
        let old = contents.previous("/etc/sudoers").unwrap();
        let diff = unified_diff("/etc/sudoers", &old, &Contents::text(&file).unwrap()).unwrap();
        assert!(diff.contains("+evil ALL=(ALL) NOPASSWD: ALL"), "{diff}");
        assert!(unified_diff("/x", "same\n", "same\n").is_none());
        let long = unified_diff("/x", "", &"line\n".repeat(5000)).unwrap();
        assert!(long.ends_with("diff cut at 8192 bytes\n"));

        std::fs::write(&file, b"bin\0ary").unwrap();
        assert!(Contents::text(&file).is_none());
        let secret = host.path().join("shadow");
        std::fs::write(&secret, "root:$6$hash:19000::::::\n").unwrap();
        let link = host.path().join("swapped");
        std::os::unix::fs::symlink(&secret, &link).unwrap();
        assert!(
            Contents::text(&link).is_none(),
            "a symlink is never read through"
        );
        assert!(entry_of(&link).is_none());
        std::fs::write(&file, "-----BEGIN OPENSSH PRIVATE KEY-----\nabc\n").unwrap();
        assert!(Contents::text(&file).is_none(), "keys never get copied");
        std::fs::write(
            &file,
            "users:\n- user:\n    client-key-data: LS0tLS1CRUdJTiBF\n",
        )
        .unwrap();
        assert!(
            Contents::text(&file).is_none(),
            "kubeconfig keys never get copied"
        );
        let diff = unified_diff(
            "/etc/app.conf",
            "user=app\npassword=old\nPasswordAuthentication no\n",
            "user=app\npassword=n3w-s3cret\nPasswordAuthentication yes\napi_token: abc\n",
        )
        .unwrap();
        assert!(
            !diff.contains("n3w-s3cret") && !diff.contains("=old") && !diff.contains("abc"),
            "{diff}"
        );
        assert!(diff.contains("+password= <redacted>"), "{diff}");
        assert!(diff.contains("+PasswordAuthentication yes"), "{diff}");
        assert_eq!(normalize("/etc//./cron.d/../shadow/"), "/etc/shadow");
        assert_eq!(normalize("etc"), "/etc");
        assert_eq!(normalize_all(&["/a//b".into()]), vec!["/a/b".to_string()]);
        contents.keep("/etc/sudoers", None);
        assert!(contents.previous("/etc/sudoers").is_none());

        let rules = vec!["/etc/shadow".to_string(), "/etc/ssl/private/".into()];
        assert!(no_diff(&rules, "/etc/shadow"));
        assert!(
            no_diff(&rules, "/etc//shadow"),
            "non-canonical paths still match"
        );
        assert!(no_diff(&rules, "/etc/./ssl/../shadow"));
        assert!(no_diff(&["/etc/./shadow".to_string()], "/etc/shadow"));
        assert!(no_diff(&rules, "/etc/ssl/private/key.pem"));
        assert!(!no_diff(&rules, "/etc/shadow-"));
        assert!(!no_diff(&rules, "/etc/passwd"));
    }
}
