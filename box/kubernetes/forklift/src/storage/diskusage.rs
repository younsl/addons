//! Filesystem capacity for the data directory (the fs backend's answer to the
//! MinIO admin metrics).

use std::io;
use std::path::Path;

use serde::{Deserialize, Serialize};

/// The capacity of the filesystem a data directory lives on. It is the
/// filesystem backend's counterpart to [`super::MinIOInfo`]: the same "how full
/// is the thing artifacts are written to" question, answered for a
/// PersistentVolume.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Disk {
    /// The directory the numbers were measured through.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub path: String,
    /// The filesystem size.
    pub total_bytes: i64,
    /// What is occupied.
    pub used_bytes: i64,
    /// What this process may still write (excluding any root reserve, so
    /// used + available can fall short of total).
    pub available_bytes: i64,
    /// `used_bytes / total_bytes` in [0,1], 0 when the size is unknown.
    pub usage_ratio: f64,
}

/// Reports capacity for the filesystem holding `path`. Sizes are the
/// operator's view of the volume, not forklift's own footprint: on Kubernetes a
/// PersistentVolume is what fills up, and it fills up for reasons (other
/// files, snapshots, a shared mount) that the blob table cannot see.
///
/// Used is derived from total minus free rather than read directly, and total
/// counts every block including the root reserve, so the numbers line up with
/// what `df` reports for the same mount.
#[cfg(unix)]
pub fn disk_usage(path: impl AsRef<Path>) -> io::Result<Disk> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;

    let path = path.as_ref();
    let c_path = CString::new(path.as_os_str().as_bytes())
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;
    // SAFETY: statvfs only writes into the zeroed struct we hand it and reads
    // the NUL-terminated path; both outlive the call.
    let st = unsafe {
        let mut st: libc::statvfs = std::mem::zeroed();
        if libc::statvfs(c_path.as_ptr(), &mut st) != 0 {
            return Err(io::Error::last_os_error());
        }
        st
    };
    let bsize = st.f_frsize as i64;
    let total = st.f_blocks as i64 * bsize;
    // f_bavail excludes blocks reserved for root: it is what a process running
    // as forklift can actually still write.
    let available = st.f_bavail as i64 * bsize;
    let used = total - st.f_bfree as i64 * bsize;
    let mut d = Disk {
        path: path.to_string_lossy().into_owned(),
        total_bytes: total,
        used_bytes: used,
        available_bytes: available,
        usage_ratio: 0.0,
    };
    if total > 0 {
        d.usage_ratio = used as f64 / total as f64;
    }
    Ok(d)
}

/// Unavailable outside unix; callers treat the error as "no filesystem
/// metrics" and fall back to the blob footprint alone.
#[cfg(not(unix))]
pub fn disk_usage(_path: impl AsRef<Path>) -> io::Result<Disk> {
    Err(io::Error::other(
        "filesystem capacity is not available on this platform",
    ))
}

#[cfg(test)]
pub(crate) mod tests {
    use crate::storage::diskusage::*;

    /// Checks the measured volume is internally consistent: a real size, a used
    /// figure that fits inside it, and a ratio that matches the two.
    #[test]
    fn disk_usage_is_self_consistent() {
        let dir = tempfile::tempdir().unwrap();
        let d = disk_usage(dir.path()).expect("disk_usage");
        assert!(d.total_bytes > 0, "total = {}", d.total_bytes);
        assert!(
            d.used_bytes >= 0 && d.used_bytes <= d.total_bytes,
            "used = {}, want within [0, {}]",
            d.used_bytes,
            d.total_bytes
        );
        assert!(
            d.available_bytes >= 0 && d.available_bytes <= d.total_bytes,
            "available = {}, want within [0, {}]",
            d.available_bytes,
            d.total_bytes
        );
        let want = d.used_bytes as f64 / d.total_bytes as f64;
        assert!(
            (d.usage_ratio - want).abs() <= 1e-9,
            "usage ratio = {}, want {want}",
            d.usage_ratio
        );
    }

    /// An unmeasurable path must report an error rather than a zeroed volume that
    /// would render as 0% used.
    #[test]
    fn disk_usage_missing_path() {
        let dir = tempfile::tempdir().unwrap();
        assert!(
            disk_usage(dir.path().join("does-not-exist")).is_err(),
            "expected an error for a missing path"
        );
    }
}
