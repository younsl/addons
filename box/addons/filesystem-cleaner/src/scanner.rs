//! Directory traversal collecting files that match the configured
//! include/exclude patterns.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use tracing::{info, warn};

use crate::bytesize;
use crate::matcher::Matcher;

/// A file eligible for deletion.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileInfo {
    pub path: PathBuf,
    pub size: u64,
}

/// Traverses directories and collects matching files.
#[derive(Debug)]
pub struct Scanner {
    matcher: Matcher,
}

impl Scanner {
    pub const fn new(matcher: Matcher) -> Self {
        Self { matcher }
    }

    /// Collects all files under `base` that match the include patterns and do
    /// not match the exclude patterns. Patterns see paths relative to `base`
    /// with forward slashes.
    pub fn scan(&self, base: &Path) -> Vec<FileInfo> {
        let mut files = Vec::new();
        self.walk(base, base, &mut files);
        files
    }

    fn walk(&self, base: &Path, dir: &Path, files: &mut Vec<FileInfo>) {
        let entries = match fs::read_dir(dir) {
            Ok(entries) => entries,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return,
            Err(e) => {
                warn!(path = %dir.display(), error = %e, "Error reading directory");
                return;
            }
        };

        let mut entries: Vec<fs::DirEntry> = entries
            .filter_map(|entry| match entry {
                Ok(entry) => Some(entry),
                Err(e) => {
                    warn!(path = %dir.display(), error = %e, "Error reading directory entry");
                    None
                }
            })
            .collect();
        entries.sort_by_key(fs::DirEntry::file_name);

        for entry in entries {
            let path = entry.path();
            let rel = relative_path(base, &path, &entry);

            // DirEntry::metadata uses lstat semantics, so symlinks are
            // reported as symlinks instead of their targets.
            let meta = match entry.metadata() {
                Ok(meta) => meta,
                Err(e) => {
                    warn!(path = %path.display(), error = %e, "Error reading metadata");
                    continue;
                }
            };
            let file_type = meta.file_type();

            // Skip symbolic links to prevent infinite loops and unintended
            // deletions outside target-paths.
            if file_type.is_symlink() {
                let target = fs::read_link(&path).map_or_else(
                    |_| "(target unreadable)".to_string(),
                    |target| target.display().to_string(),
                );
                info!(
                    symlink = %path.display(),
                    relative_path = %rel,
                    target = %target,
                    file_type = "symlink",
                    "Skipping symbolic link to prevent infinite loops and unintended deletions outside target-paths"
                );
                continue;
            }

            if file_type.is_dir() {
                if self.matcher.should_exclude(&rel) {
                    info!(
                        dir = %path.display(),
                        relative_path = %rel,
                        file_type = "directory",
                        "Skipping excluded directory"
                    );
                    continue;
                }
                self.walk(base, &path, files);
                continue;
            }

            if self.matcher.should_exclude(&rel) {
                info!(
                    file = %path.display(),
                    relative_path = %rel,
                    file_type = "file",
                    size = %bytesize::human(meta.len()),
                    "Skipping excluded file"
                );
                continue;
            }
            if !self.matcher.should_include(&rel) {
                continue;
            }

            files.push(FileInfo {
                path,
                size: meta.len(),
            });
        }
    }
}

fn relative_path(base: &Path, path: &Path, entry: &fs::DirEntry) -> String {
    path.strip_prefix(base).map_or_else(
        |_| entry.file_name().to_string_lossy().into_owned(),
        |rel| rel.to_string_lossy().into_owned(),
    )
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::os::unix::fs::symlink;
    use std::path::Path;

    use super::{FileInfo, Scanner};
    use crate::matcher::Matcher;

    const NONE: &[&str] = &[];

    fn create_file(base: &Path, rel: &str, content: &[u8]) {
        let full = base.join(rel);
        fs::create_dir_all(full.parent().expect("parent")).expect("create_dir_all");
        fs::write(full, content).expect("write");
    }

    fn scanner(include: &[&str], exclude: &[&str]) -> Scanner {
        Scanner::new(Matcher::new(include, exclude).expect("patterns compile"))
    }

    fn has_suffix(files: &[FileInfo], suffix: &str) -> bool {
        files.iter().any(|f| f.path.ends_with(suffix))
    }

    #[test]
    fn scan_with_exclude() {
        let dir = tempfile::tempdir().expect("tempdir");
        create_file(dir.path(), "test.txt", b"test");
        create_file(dir.path(), ".git/config", b"config");
        create_file(dir.path(), "node_modules/lib.js", b"js");

        let files = scanner(&["*"], &["**/.git/**", "**/node_modules/**"]).scan(dir.path());

        assert_eq!(files.len(), 1, "{files:?}");
        assert!(has_suffix(&files, "test.txt"));
    }

    #[test]
    fn scan_nested_directories() {
        let dir = tempfile::tempdir().expect("tempdir");
        create_file(dir.path(), "build/groovy-dsl/cache.jar", b"jar");
        create_file(dir.path(), "build/other/file.txt", b"txt");

        let files = scanner(&["*"], &["**/groovy-dsl/**"]).scan(dir.path());

        assert_eq!(files.len(), 1, "{files:?}");
        assert!(has_suffix(&files, "file.txt"));
    }

    #[test]
    fn scan_applies_include_after_exclude() {
        let dir = tempfile::tempdir().expect("tempdir");
        create_file(dir.path(), "app.log", b"log");
        create_file(dir.path(), "important.log", b"keep");
        create_file(dir.path(), "config.txt", b"txt");

        let files = scanner(&["*.log"], &["**/important.log"]).scan(dir.path());

        assert_eq!(files.len(), 1, "{files:?}");
        assert!(has_suffix(&files, "app.log"));
    }

    #[test]
    fn scan_nonexistent_path() {
        let files = scanner(&["*"], NONE).scan(Path::new("/does/not/exist/zzzz-test"));
        assert!(files.is_empty());
    }

    #[test]
    fn scan_reports_file_size_and_sorted_order() {
        let dir = tempfile::tempdir().expect("tempdir");
        create_file(dir.path(), "b.bin", b"12345");
        create_file(dir.path(), "a.bin", b"1");

        let files = scanner(&["*"], NONE).scan(dir.path());

        assert_eq!(files.len(), 2);
        assert!(files[0].path.ends_with("a.bin"));
        assert_eq!(files[0].size, 1);
        assert!(files[1].path.ends_with("b.bin"));
        assert_eq!(files[1].size, 5);
    }

    #[test]
    fn skip_symbolic_links() {
        let dir = tempfile::tempdir().expect("tempdir");
        create_file(dir.path(), "real_file.txt", b"content");
        create_file(dir.path(), "target/important.dat", b"important");
        symlink(dir.path().join("target"), dir.path().join("link_to_target")).expect("symlink");
        symlink(
            dir.path().join("real_file.txt"),
            dir.path().join("link_to_file"),
        )
        .expect("symlink");
        symlink("/does/not/exist", dir.path().join("dangling")).expect("symlink");

        let files = scanner(&["*"], NONE).scan(dir.path());

        assert!(has_suffix(&files, "real_file.txt"));
        assert!(has_suffix(&files, "important.dat"));
        // 2 files, not more: symlinks are neither traversed nor collected.
        assert_eq!(files.len(), 2, "{files:?}");
    }

    #[test]
    fn skip_circular_symlinks() {
        let dir = tempfile::tempdir().expect("tempdir");
        create_file(dir.path(), "dir/file.txt", b"test");
        symlink("..", dir.path().join("dir/link_to_parent")).expect("symlink");

        let files = scanner(&["*"], NONE).scan(dir.path());

        // Must complete without infinite recursion and find only dir/file.txt.
        assert_eq!(files.len(), 1, "{files:?}");
        assert!(has_suffix(&files, "file.txt"));
    }
}
