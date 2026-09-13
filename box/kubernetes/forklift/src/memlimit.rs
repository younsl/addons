//! Reads the container's cgroup memory limit at startup.
//!
use std::path::Path;
use std::sync::OnceLock;

/// Leaves headroom below the hard cgroup limit for non-heap memory the runtime
/// does not track (thread stacks, mmapped files, FFI).
const RATIO: f64 = 0.9;

/// Guards against absurdly small (or misread) limits that would make any
/// limit-driven sizing degenerate.
const MIN_LIMIT: i64 = 64 << 20;

/// The soft memory limit derived from the cgroup, in bytes; `None` when no
/// finite cgroup limit was found or [`apply`] never ran.
static LIMIT: OnceLock<Option<u64>> = OnceLock::new();

/// The soft memory limit [`apply`] derived from the cgroup, in bytes.
///
/// It is informational here: nothing in the Rust build enforces it.
pub fn limit() -> Option<u64> {
    LIMIT.get().copied().flatten()
}

/// Records the soft memory limit as `RATIO` × the cgroup memory limit.
///
pub fn apply() {
    let Some(limit) = cgroup_limit(Path::new("/sys/fs/cgroup")) else {
        return;
    };
    let soft = (((limit as f64) * RATIO) as i64).max(MIN_LIMIT);
    let _ = LIMIT.set(Some(soft as u64));
    tracing::info!(
        cgroup_limit_bytes = limit,
        memory_budget_bytes = soft,
        "memory budget observed from cgroup"
    );
}

/// Reads the container memory limit from the cgroup filesystem rooted at
/// `root`, supporting both v2 (`memory.max`) and v1
/// (`memory/memory.limit_in_bytes`). Returns `None` when no finite limit is
/// set.
pub(crate) fn cgroup_limit(root: &Path) -> Option<i64> {
    for p in [
        root.join("memory.max"),
        root.join("memory").join("memory.limit_in_bytes"),
    ] {
        let Ok(raw) = std::fs::read(&p) else {
            continue;
        };
        let s = String::from_utf8_lossy(&raw);
        let s = s.trim();
        if s == "max" {
            // cgroup v2: no limit configured.
            return None;
        }
        let n = s.parse::<i64>().ok()?;
        if n <= 0 {
            return None;
        }
        // cgroup v1 reports "unlimited" as a huge page-rounded value.
        if n >= 1i64 << 62 {
            return None;
        }
        return Some(n);
    }
    None
}

#[cfg(test)]
pub(crate) mod tests {
    //! The cgroup root stays a parameter so the parsing is testable without a container.

    use std::path::Path;

    use crate::memlimit::*;

    fn write_file(path: &Path, content: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, content).unwrap();
    }

    #[test]
    fn cgroup_limit_v2() {
        let dir = tempfile::tempdir().unwrap();
        write_file(&dir.path().join("memory.max"), "268435456\n");
        assert_eq!(cgroup_limit(dir.path()), Some(268435456), "cgroup_limit");
    }

    #[test]
    fn cgroup_limit_v2_unlimited() {
        let dir = tempfile::tempdir().unwrap();
        write_file(&dir.path().join("memory.max"), "max\n");
        assert!(
            cgroup_limit(dir.path()).is_none(),
            "unlimited v2 cgroup should report no limit"
        );
    }

    #[test]
    fn cgroup_limit_v1() {
        let dir = tempfile::tempdir().unwrap();
        write_file(
            &dir.path().join("memory").join("memory.limit_in_bytes"),
            "536870912\n",
        );
        assert_eq!(cgroup_limit(dir.path()), Some(536870912), "cgroup_limit");
    }

    #[test]
    fn cgroup_limit_v1_unlimited() {
        let dir = tempfile::tempdir().unwrap();
        write_file(
            &dir.path().join("memory").join("memory.limit_in_bytes"),
            "9223372036854771712\n",
        );
        assert!(
            cgroup_limit(dir.path()).is_none(),
            "unlimited v1 cgroup should report no limit"
        );
    }

    #[test]
    fn cgroup_limit_absent() {
        let dir = tempfile::tempdir().unwrap();
        assert!(
            cgroup_limit(dir.path()).is_none(),
            "missing cgroup files should report no limit"
        );
    }

    /// The Rust-only half of the deviation: `apply` never enforces anything, and
    /// on a host without a cgroup limit it records nothing.
    #[test]
    fn limit_is_unset_without_a_cgroup_limit() {
        if cgroup_limit(Path::new("/sys/fs/cgroup")).is_none() {
            apply();
            assert_eq!(limit(), None);
        }
    }
}
