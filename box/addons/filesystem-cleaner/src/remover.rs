//! File removal behind a trait so deletion can be observed or faked in tests.

use std::fs;
use std::io;
use std::path::Path;

/// Deletes a single file.
pub trait FileRemover: Send + Sync {
    fn remove(&self, path: &Path) -> io::Result<()>;
}

/// Removes files with [`fs::remove_file`], which never removes directories
/// and unlinks a symlink itself rather than its target. The scanner never
/// hands over symlinks, so in practice only regular files reach this point.
#[derive(Debug, Default, Clone, Copy)]
pub struct FsRemover;

impl FileRemover for FsRemover {
    fn remove(&self, path: &Path) -> io::Result<()> {
        fs::remove_file(path)
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::{FileRemover, FsRemover};

    #[test]
    fn removes_regular_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let file = dir.path().join("gone.txt");
        fs::write(&file, b"bye").expect("write");

        FsRemover.remove(&file).expect("remove");

        assert!(!file.exists());
    }

    #[test]
    fn refuses_directories_and_missing_paths() {
        let dir = tempfile::tempdir().expect("tempdir");
        let sub = dir.path().join("sub");
        fs::create_dir(&sub).expect("create_dir");

        assert!(FsRemover.remove(&sub).is_err());
        assert!(sub.exists());
        assert!(FsRemover.remove(&dir.path().join("missing")).is_err());
    }
}
