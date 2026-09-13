//! The shell scripts executed on target instances via SSM, kept as standalone
//! files for readability while shipping inside the static binary.

/// Prints the root filesystem used percentage (read-only).
pub const MEASURE_ROOT_FS: &str = include_str!("../scripts/measure-rootfs.sh");

/// Grows the root partition and extends the filesystem (ext2/3/4 via
/// `resize2fs`, XFS via `xfs_growfs`).
pub const RESIZE_ROOT_FS: &str = include_str!("../scripts/resize-rootfs.sh");

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scripts_are_embedded() {
        assert!(MEASURE_ROOT_FS.contains("df --output=pcent /"));
        assert!(RESIZE_ROOT_FS.contains("growpart"));
        assert!(RESIZE_ROOT_FS.contains("xfs_growfs"));
    }
}
