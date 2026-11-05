//! Filesystem usage reporting.

use std::io;
use std::path::Path;

/// Source of filesystem usage figures.
///
/// The cleanup policy depends only on this trait, so tests can drive it with
/// fixed values instead of whatever the live filesystem happens to report.
pub trait DiskUsage: Send + Sync {
    /// Returns the used-space percentage (0-100) of the filesystem containing
    /// `path`.
    fn usage_percent(&self, path: &Path) -> io::Result<f64>;
}

/// `statvfs(2)`-backed usage.
///
/// Usage is computed as `(total - available) / total`, where available is the
/// space usable by unprivileged processes (`f_bavail`). This matches the
/// behavior the tool has always shipped with and can differ from `df`.
#[derive(Debug, Default, Clone, Copy)]
pub struct Statvfs;

impl DiskUsage for Statvfs {
    // `fsblkcnt_t` is `u64` on Linux but `u32` on macOS, so the `u64::from`
    // calls are required on one target and flagged as useless on the other.
    #[allow(clippy::useless_conversion)]
    #[expect(clippy::cast_precision_loss)]
    fn usage_percent(&self, path: &Path) -> io::Result<f64> {
        let st = nix::sys::statvfs::statvfs(path)?;
        let total = u64::from(st.blocks());
        if total == 0 {
            return Ok(0.0);
        }
        let available = u64::from(st.blocks_available());
        let used = total.saturating_sub(available);
        Ok(used as f64 / total as f64 * 100.0)
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::{DiskUsage, Statvfs};

    #[test]
    fn reports_usage_within_range_for_existing_path() {
        let dir = tempfile::tempdir().expect("tempdir");
        let usage = Statvfs.usage_percent(dir.path()).expect("statvfs");
        assert!((0.0..=100.0).contains(&usage), "usage {usage} out of range");
    }

    #[test]
    fn fails_for_nonexistent_path() {
        assert!(
            Statvfs
                .usage_percent(Path::new("/does/not/exist/zzzz-test"))
                .is_err()
        );
        assert!(
            Statvfs
                .usage_percent(Path::new("relative/nonexistent"))
                .is_err()
        );
    }
}
