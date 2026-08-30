//! Filesystem usage reporting via `statvfs(2)`.

use std::path::Path;

use nix::sys::statvfs::statvfs;

/// Returns the used-space percentage (0-100) of the filesystem containing
/// `path`.
///
/// Usage is computed as `(total - available) / total`, where available is the
/// space usable by unprivileged processes (`f_bavail`). This matches the
/// behavior the tool has always shipped with and can differ from `df`.
#[expect(clippy::cast_precision_loss)]
pub fn usage_percent(path: &Path) -> Result<f64, nix::Error> {
    let st = statvfs(path)?;
    let total = u64::from(st.blocks());
    if total == 0 {
        return Ok(0.0);
    }
    let available = u64::from(st.blocks_available());
    let used = total.saturating_sub(available);
    Ok(used as f64 / total as f64 * 100.0)
}

#[cfg(test)]
mod tests {
    use super::usage_percent;

    #[test]
    fn reports_usage_within_range_for_existing_path() {
        let dir = tempfile::tempdir().expect("tempdir");
        let usage = usage_percent(dir.path()).expect("statvfs");
        assert!((0.0..=100.0).contains(&usage), "usage {usage} out of range");
    }

    #[test]
    fn fails_for_nonexistent_path() {
        assert!(usage_percent("/does/not/exist/zzzz-test".as_ref()).is_err());
        assert!(usage_percent("relative/nonexistent".as_ref()).is_err());
    }
}
