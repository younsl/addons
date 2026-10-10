//! Package manager files regenerated from what is installed on disk. tether
//! runs in a container without brew or kubectl, so every source is a file.

pub mod brew;
pub mod brewfile;
pub mod cargo;
pub mod go;
pub mod krew;
pub mod npm;

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::spec::Packages;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct FileReport {
    pub path: PathBuf,
    pub entries: usize,
    /// The package list changed, so the file was rewritten (or would be in dry run).
    pub updated: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// Host directory this file needs that is not visible in the container.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub missing_mount: Option<PathBuf>,
}

pub fn sync(packages: &Packages, home: &Path, date: &str, dry_run: bool) -> Vec<FileReport> {
    let mut reports = Vec::new();
    if let Some(path) = &packages.brewfile {
        let generated = brewfile::generate(packages, home, date).map(|text| {
            let entries = brewfile::count_entries(&text);
            (text, entries)
        });
        let mut report = write_report(path, generated, dry_run);
        if !packages.homebrew_prefix.join("Cellar").is_dir() {
            report.missing_mount = Some(packages.homebrew_prefix.clone());
        }
        reports.push(report);
    }
    if let Some(path) = &packages.krewfile {
        let generated = krew::plugins(&packages.krew_root).map(|plugins| {
            let version = krew_version(&packages.homebrew_prefix);
            (krew::render(&plugins, &version, date), plugins.len())
        });
        reports.push(write_report(path, generated, dry_run));
    }
    reports
}

fn krew_version(homebrew_prefix: &Path) -> String {
    let mut versions: Vec<String> = fs::read_dir(homebrew_prefix.join("Cellar/krew"))
        .map(|dir| {
            dir.filter_map(Result::ok)
                .filter_map(|e| e.file_name().into_string().ok())
                .collect()
        })
        .unwrap_or_default();
    versions.sort_by(|a, b| brew::compare_versions(a, b));
    versions
        .pop()
        .map_or_else(|| "unknown".to_string(), |v| format!("v{v}"))
}

fn write_report(path: &Path, generated: io::Result<(String, usize)>, dry_run: bool) -> FileReport {
    let mut report = FileReport {
        path: path.to_path_buf(),
        entries: 0,
        updated: false,
        error: None,
        missing_mount: None,
    };
    match generated.and_then(|(text, entries)| {
        report.entries = entries;
        write_if_changed(path, &text, dry_run)
    }) {
        Ok(updated) => report.updated = updated,
        Err(err) => report.error = Some(err.to_string()),
    }
    report
}

/// Rewrite `path` only when its package lines change, so comment lines such as
/// the date header never churn the file on their own.
pub fn write_if_changed(path: &Path, content: &str, dry_run: bool) -> io::Result<bool> {
    let current = fs::read_to_string(path).unwrap_or_default();
    if package_lines(&current) == package_lines(content) {
        return Ok(false);
    }
    if !dry_run {
        let mut tmp = path.as_os_str().to_owned();
        tmp.push(".tether-tmp");
        let tmp = PathBuf::from(tmp);
        fs::write(&tmp, content)?;
        fs::rename(&tmp, path)?;
    }
    Ok(true)
}

fn package_lines(text: &str) -> Vec<&str> {
    text.lines()
        .filter(|line| !line.starts_with('#') && !line.trim().is_empty())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rewrites_only_when_package_lines_change() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("Brewfile");
        fs::write(&path, "# Date: old\nbrew \"jq\"\n").expect("write");

        assert!(!write_if_changed(&path, "# Date: new\nbrew \"jq\"\n", false).expect("same"));
        assert_eq!(
            fs::read_to_string(&path).expect("read"),
            "# Date: old\nbrew \"jq\"\n"
        );

        assert!(write_if_changed(&path, "brew \"jq\"\nbrew \"yq\"\n", true).expect("dry"));
        assert_eq!(
            fs::read_to_string(&path).expect("read"),
            "# Date: old\nbrew \"jq\"\n"
        );

        assert!(write_if_changed(&path, "brew \"jq\"\nbrew \"yq\"\n", false).expect("write"));
        assert_eq!(
            fs::read_to_string(&path).expect("read"),
            "brew \"jq\"\nbrew \"yq\"\n"
        );
        assert!(!dir.path().join("Brewfile.tether-tmp").exists());
    }

    #[test]
    fn missing_file_counts_as_changed() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("krewfile");
        assert!(write_if_changed(&path, "stern\n", false).expect("write"));
        assert_eq!(fs::read_to_string(&path).expect("read"), "stern\n");
    }

    #[test]
    fn write_report_captures_errors() {
        let dir = tempfile::tempdir().expect("tempdir");
        let report = write_report(
            &dir.path().join("x"),
            Err(io::Error::other("no receipts")),
            false,
        );
        assert_eq!(report.error.as_deref(), Some("no receipts"));
        assert!(!report.updated);
    }

    #[test]
    fn krew_version_from_cellar() {
        let dir = tempfile::tempdir().expect("tempdir");
        assert_eq!(krew_version(dir.path()), "unknown");
        fs::create_dir_all(dir.path().join("Cellar/krew/0.4.5")).expect("old");
        fs::create_dir_all(dir.path().join("Cellar/krew/0.10.0")).expect("new");
        assert_eq!(krew_version(dir.path()), "v0.10.0");
    }
}
