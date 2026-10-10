//! Cleanup orchestration: one cycle checks disk usage for every target path
//! and deletes matching files where usage exceeds the threshold.
//!
//! Filesystem access goes through [`DiskUsage`] and [`FileRemover`], so the
//! policy in this module is exercised in tests with fixed usage figures and
//! a recording remover instead of the live filesystem.

pub mod report;

use std::fs;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn};

use crate::bytesize;
use crate::config::Config;
use crate::disk::{DiskUsage, Statvfs};
use crate::error::PatternError;
use crate::matcher::Matcher;
use crate::remover::{FileRemover, FsRemover};
use crate::scanner::Scanner;
use crate::schedule;

use self::report::{CleanStats, CycleReport, Outcome, PathReport};

/// Runs cleanup cycles against the configured target paths.
pub struct Cleaner {
    config: Config,
    scanner: Scanner,
    disk: Arc<dyn DiskUsage>,
    remover: Arc<dyn FileRemover>,
    shutdown: CancellationToken,
}

impl Cleaner {
    /// Creates a cleaner backed by `statvfs(2)` and `std::fs`. Cancelling
    /// `shutdown` stops the interval loop and interrupts an in-flight
    /// deletion pass between files.
    pub fn new(config: Config, shutdown: CancellationToken) -> Result<Self, PatternError> {
        Self::with_backends(config, Arc::new(Statvfs), Arc::new(FsRemover), shutdown)
    }

    /// Creates a cleaner with explicit filesystem backends.
    pub fn with_backends(
        config: Config,
        disk: Arc<dyn DiskUsage>,
        remover: Arc<dyn FileRemover>,
        shutdown: CancellationToken,
    ) -> Result<Self, PatternError> {
        let matcher = Matcher::new(&config.include_patterns, &config.exclude_patterns)?;
        Ok(Self {
            config,
            scanner: Scanner::new(matcher),
            disk,
            remover,
            shutdown,
        })
    }

    /// Executes the cleaner in the configured mode until it finishes (once
    /// mode) or the shutdown token is cancelled (interval mode).
    pub async fn run(&self) {
        let period = Duration::from_secs(self.config.check_interval_minutes.saturating_mul(60));
        schedule::run(
            self.config.cleanup_mode,
            period,
            self.shutdown.clone(),
            || {
                self.perform_cleanup();
            },
        )
        .await;
    }

    fn interrupted(&self) -> bool {
        self.shutdown.is_cancelled()
    }

    /// Runs one cleanup cycle over all target paths.
    fn perform_cleanup(&self) -> CycleReport {
        info!("Starting cleanup cycle");
        let start = Instant::now();
        let threshold = self.config.usage_threshold_percent;

        let paths = self
            .config
            .target_paths
            .iter()
            .map(|path| {
                let usage = self.disk_usage_percent(path);
                let outcome = if usage > f64::from(threshold) {
                    warn!(
                        path = %path.display(),
                        usage,
                        threshold,
                        cleanup_mode = %self.config.cleanup_mode,
                        dry_run = self.config.dry_run,
                        "Disk usage exceeds threshold, starting cleanup"
                    );
                    self.clean_path(path)
                } else {
                    info!(
                        path = %path.display(),
                        usage,
                        threshold,
                        cleanup_mode = %self.config.cleanup_mode,
                        "Disk usage is below threshold, skipping cleanup"
                    );
                    Outcome::BelowThreshold
                };
                PathReport {
                    path: path.clone(),
                    usage,
                    outcome,
                }
            })
            .collect();

        let duration = start.elapsed();
        info!(
            duration_secs = duration.as_secs(),
            "Cleanup cycle completed"
        );
        CycleReport { paths, duration }
    }

    /// Returns the used percentage of the filesystem containing `path`, or 0
    /// when the filesystem cannot be inspected.
    fn disk_usage_percent(&self, path: &Path) -> f64 {
        match self.disk.usage_percent(path) {
            Ok(usage) => usage,
            Err(e) => {
                error!(path = %path.display(), error = %e, "Failed to get disk usage");
                0.0
            }
        }
    }

    /// Deletes matching files under `base`.
    fn clean_path(&self, base: &Path) -> Outcome {
        if let Err(e) = fs::metadata(base) {
            error!(path = %base.display(), error = %e, "Path does not exist");
            return Outcome::Missing;
        }

        let initial_usage = self.disk_usage_percent(base);
        let files = self.scanner.scan(base);
        let mut stats = CleanStats {
            initial_usage,
            final_usage: initial_usage,
            candidates: files.len(),
            candidate_bytes: files.iter().map(|f| f.size).sum(),
            dry_run: self.config.dry_run,
            ..CleanStats::default()
        };

        if files.is_empty() {
            info!(
                path = %base.display(),
                initial_usage_percent = initial_usage,
                "No files to clean"
            );
            return Outcome::Cleaned(stats);
        }

        info!(
            path = %base.display(),
            initial_usage_percent = initial_usage,
            file_count = stats.candidates,
            total_size = %bytesize::human(stats.candidate_bytes),
            "Starting cleanup operation"
        );

        for file in &files {
            if self.interrupted() {
                info!("Cleanup interrupted by shutdown");
                stats.interrupted = true;
                break;
            }

            if self.config.dry_run {
                info!(
                    file = %file.path.display(),
                    size = %bytesize::human(file.size),
                    "[DRY-RUN] Would delete file"
                );
                continue;
            }

            match self.remover.remove(&file.path) {
                Ok(()) => {
                    info!(
                        file = %file.path.display(),
                        size = %bytesize::human(file.size),
                        "File deleted successfully"
                    );
                    stats.deleted += 1;
                    stats.freed_bytes += file.size;
                }
                Err(e) => {
                    error!(file = %file.path.display(), error = %e, "Failed to delete file");
                    stats.failed += 1;
                }
            }
        }

        stats.final_usage = self.disk_usage_percent(base);
        self.log_completion(base, &stats);
        Outcome::Cleaned(stats)
    }

    fn log_completion(&self, base: &Path, stats: &CleanStats) {
        if self.config.dry_run {
            info!(
                path = %base.display(),
                initial_usage_percent = stats.initial_usage,
                final_usage_percent = stats.final_usage,
                usage_reduction = stats.usage_reduction(),
                would_delete = stats.candidates,
                "Cleanup completed (DRY-RUN)"
            );
            return;
        }
        info!(
            path = %base.display(),
            initial_usage_percent = stats.initial_usage,
            final_usage_percent = stats.final_usage,
            usage_reduction = stats.usage_reduction(),
            deleted_count = stats.deleted,
            failed_count = stats.failed,
            freed_space = %bytesize::human(stats.freed_bytes),
            "Cleanup completed successfully"
        );
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::io;
    use std::path::{Path, PathBuf};
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use tokio_util::sync::CancellationToken;

    use super::Cleaner;
    use super::report::Outcome;
    use crate::config::{CleanupMode, Config, LogLevel};
    use crate::disk::DiskUsage;
    use crate::remover::FileRemover;

    /// Reports a fixed usage for every path.
    struct FixedUsage(f64);

    impl DiskUsage for FixedUsage {
        fn usage_percent(&self, _: &Path) -> io::Result<f64> {
            Ok(self.0)
        }
    }

    /// Fails every usage query, like a path on an unmounted filesystem.
    struct BrokenUsage;

    impl DiskUsage for BrokenUsage {
        fn usage_percent(&self, _: &Path) -> io::Result<f64> {
            Err(io::Error::other("statvfs unavailable"))
        }
    }

    /// Records removal requests without touching the filesystem and fails
    /// for paths ending in `fail_suffix`.
    #[derive(Default)]
    struct RecordingRemover {
        removed: Mutex<Vec<PathBuf>>,
        fail_suffix: Option<&'static str>,
    }

    impl RecordingRemover {
        fn failing_on(suffix: &'static str) -> Self {
            Self {
                removed: Mutex::default(),
                fail_suffix: Some(suffix),
            }
        }

        fn removed(&self) -> Vec<PathBuf> {
            self.removed.lock().expect("lock").clone()
        }
    }

    impl FileRemover for RecordingRemover {
        fn remove(&self, path: &Path) -> io::Result<()> {
            if self
                .fail_suffix
                .is_some_and(|suffix| path.ends_with(suffix))
            {
                return Err(io::Error::from(io::ErrorKind::PermissionDenied));
            }
            self.removed.lock().expect("lock").push(path.to_path_buf());
            Ok(())
        }
    }

    fn make_config(target: &Path, threshold: u8, mode: CleanupMode, dry_run: bool) -> Config {
        Config {
            target_paths: vec![target.to_path_buf()],
            usage_threshold_percent: threshold,
            check_interval_minutes: 1,
            include_patterns: vec!["*".to_string()],
            exclude_patterns: Vec::new(),
            cleanup_mode: mode,
            dry_run,
            log_level: LogLevel::Info,
        }
    }

    /// A cleaner with fake backends: fixed `usage`, recording remover.
    fn fake_cleaner(
        config: Config,
        usage: f64,
        remover: Arc<RecordingRemover>,
    ) -> (Cleaner, CancellationToken) {
        let token = CancellationToken::new();
        let cleaner =
            Cleaner::with_backends(config, Arc::new(FixedUsage(usage)), remover, token.clone())
                .expect("cleaner");
        (cleaner, token)
    }

    /// A cleaner wired to the real filesystem backends.
    fn real_cleaner(config: Config) -> (Cleaner, CancellationToken) {
        let token = CancellationToken::new();
        let cleaner = Cleaner::new(config, token.clone()).expect("cleaner");
        (cleaner, token)
    }

    fn create_file(base: &Path, rel: &str, content: &[u8]) -> PathBuf {
        let full = base.join(rel);
        fs::create_dir_all(full.parent().expect("parent")).expect("create_dir_all");
        fs::write(&full, content).expect("write");
        full
    }

    fn single_outcome(cleaner: &Cleaner) -> Outcome {
        let report = cleaner.perform_cleanup();
        assert_eq!(report.paths.len(), 1);
        report.paths.into_iter().next().expect("one path").outcome
    }

    #[test]
    fn new_rejects_invalid_patterns() {
        let mut cfg = make_config(Path::new("/tmp"), 80, CleanupMode::Once, true);
        cfg.include_patterns = vec!["[invalid".to_string()];
        assert!(Cleaner::new(cfg.clone(), CancellationToken::new()).is_err());

        cfg.include_patterns = vec!["*".to_string()];
        cfg.exclude_patterns = vec!["[invalid".to_string()];
        assert!(Cleaner::new(cfg, CancellationToken::new()).is_err());
    }

    #[test]
    fn cancelled_token_sets_interrupted() {
        let (c, token) = real_cleaner(make_config(Path::new("/tmp"), 80, CleanupMode::Once, true));
        assert!(!c.interrupted());
        token.cancel();
        assert!(c.interrupted());
    }

    #[test]
    fn disk_usage_failure_falls_back_to_zero() {
        let cfg = make_config(Path::new("/tmp"), 0, CleanupMode::Once, true);
        let c = Cleaner::with_backends(
            cfg,
            Arc::new(BrokenUsage),
            Arc::new(RecordingRemover::default()),
            CancellationToken::new(),
        )
        .expect("cleaner");
        let usage = c.disk_usage_percent(Path::new("/tmp"));
        assert!(usage.abs() < f64::EPSILON, "usage {usage}");
        // 0 is not above a threshold of 0, so the path is skipped.
        assert_eq!(single_outcome(&c), Outcome::BelowThreshold);
    }

    #[test]
    fn threshold_comparison_is_strict() {
        let dir = tempfile::tempdir().expect("tempdir");
        create_file(dir.path(), "file.txt", b"x");

        let remover = Arc::new(RecordingRemover::default());
        let (at_threshold, _) = fake_cleaner(
            make_config(dir.path(), 50, CleanupMode::Once, false),
            50.0,
            Arc::clone(&remover),
        );
        assert_eq!(single_outcome(&at_threshold), Outcome::BelowThreshold);
        assert!(remover.removed().is_empty());

        let (above, _) = fake_cleaner(
            make_config(dir.path(), 50, CleanupMode::Once, false),
            50.1,
            Arc::clone(&remover),
        );
        let Outcome::Cleaned(stats) = single_outcome(&above) else {
            panic!("expected Cleaned");
        };
        assert_eq!(stats.deleted, 1);
        assert_eq!(remover.removed().len(), 1);
    }

    #[test]
    fn missing_target_path_is_reported() {
        let target = Path::new("/does/not/exist/zzzz-test");
        let (c, _) = fake_cleaner(
            make_config(target, 0, CleanupMode::Once, false),
            99.0,
            Arc::new(RecordingRemover::default()),
        );
        assert_eq!(single_outcome(&c), Outcome::Missing);
    }

    #[test]
    fn empty_directory_yields_no_candidates() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (c, _) = fake_cleaner(
            make_config(dir.path(), 0, CleanupMode::Once, false),
            99.0,
            Arc::new(RecordingRemover::default()),
        );
        let Outcome::Cleaned(stats) = single_outcome(&c) else {
            panic!("expected Cleaned");
        };
        assert_eq!(stats.candidates, 0);
        assert_eq!(stats.deleted, 0);
        assert!(dir.path().exists());
    }

    #[test]
    fn dry_run_lists_candidates_without_removing() {
        let dir = tempfile::tempdir().expect("tempdir");
        create_file(dir.path(), "keep1.txt", b"hello");
        create_file(dir.path(), "keep2.txt", b"world");

        let remover = Arc::new(RecordingRemover::default());
        let (c, _) = fake_cleaner(
            make_config(dir.path(), 0, CleanupMode::Once, true),
            99.0,
            Arc::clone(&remover),
        );
        let Outcome::Cleaned(stats) = single_outcome(&c) else {
            panic!("expected Cleaned");
        };

        assert!(stats.dry_run);
        assert_eq!(stats.candidates, 2);
        assert_eq!(stats.candidate_bytes, 10);
        assert_eq!(stats.deleted, 0);
        assert_eq!(stats.freed_bytes, 0);
        assert!(remover.removed().is_empty());
    }

    #[test]
    fn deletes_every_candidate_and_counts_bytes() {
        let dir = tempfile::tempdir().expect("tempdir");
        let files = [
            create_file(dir.path(), "delete1.txt", b"hello"),
            create_file(dir.path(), "delete2.txt", b"world"),
            create_file(dir.path(), "sub/delete3.txt", b"nested"),
        ];

        let remover = Arc::new(RecordingRemover::default());
        let (c, _) = fake_cleaner(
            make_config(dir.path(), 0, CleanupMode::Once, false),
            99.0,
            Arc::clone(&remover),
        );
        let Outcome::Cleaned(stats) = single_outcome(&c) else {
            panic!("expected Cleaned");
        };

        assert_eq!(stats.deleted, 3);
        assert_eq!(stats.failed, 0);
        assert_eq!(stats.freed_bytes, 16);
        assert!(!stats.interrupted);
        let mut recorded = remover.removed();
        recorded.sort();
        let mut expected = files.to_vec();
        expected.sort();
        assert_eq!(recorded, expected);
    }

    #[test]
    fn removal_failure_is_counted_and_does_not_abort() {
        let dir = tempfile::tempdir().expect("tempdir");
        create_file(dir.path(), "a.txt", b"1");
        create_file(dir.path(), "locked.txt", b"22");
        create_file(dir.path(), "z.txt", b"333");

        let remover = Arc::new(RecordingRemover::failing_on("locked.txt"));
        let (c, _) = fake_cleaner(
            make_config(dir.path(), 0, CleanupMode::Once, false),
            99.0,
            Arc::clone(&remover),
        );
        let Outcome::Cleaned(stats) = single_outcome(&c) else {
            panic!("expected Cleaned");
        };

        assert_eq!(stats.candidates, 3);
        assert_eq!(stats.deleted, 2);
        assert_eq!(stats.failed, 1);
        assert_eq!(stats.freed_bytes, 4);
        assert_eq!(remover.removed().len(), 2);
    }

    #[test]
    fn shutdown_interrupts_before_first_removal() {
        let dir = tempfile::tempdir().expect("tempdir");
        create_file(dir.path(), "file1.txt", b"a");
        create_file(dir.path(), "file2.txt", b"b");

        let remover = Arc::new(RecordingRemover::default());
        let (c, token) = fake_cleaner(
            make_config(dir.path(), 0, CleanupMode::Once, false),
            99.0,
            Arc::clone(&remover),
        );
        token.cancel();
        let Outcome::Cleaned(stats) = single_outcome(&c) else {
            panic!("expected Cleaned");
        };

        assert!(stats.interrupted);
        assert_eq!(stats.deleted, 0);
        assert!(remover.removed().is_empty());
    }

    #[test]
    fn cycle_reports_every_target_path() {
        let present = tempfile::tempdir().expect("tempdir");
        create_file(present.path(), "file.txt", b"x");
        let mut cfg = make_config(present.path(), 0, CleanupMode::Once, false);
        cfg.target_paths
            .push(PathBuf::from("/does/not/exist/zzzz-test"));

        let (c, _) = fake_cleaner(cfg, 99.0, Arc::new(RecordingRemover::default()));
        let report = c.perform_cleanup();

        assert_eq!(report.paths.len(), 2);
        assert!(matches!(report.paths[0].outcome, Outcome::Cleaned(_)));
        assert_eq!(report.paths[1].outcome, Outcome::Missing);
    }

    #[test]
    fn default_backends_delete_real_files() {
        let dir = tempfile::tempdir().expect("tempdir");
        let doomed = create_file(dir.path(), "doomed.txt", b"bytes");
        let (c, _) = real_cleaner(make_config(dir.path(), 0, CleanupMode::Once, false));

        let Outcome::Cleaned(stats) = single_outcome(&c) else {
            panic!("expected Cleaned");
        };

        assert_eq!(stats.deleted, 1);
        assert!(!doomed.exists());
        assert!((0.0..=100.0).contains(&stats.initial_usage));
    }

    #[test]
    fn default_backends_keep_files_above_threshold_100() {
        let dir = tempfile::tempdir().expect("tempdir");
        let keep = create_file(dir.path(), "keep.txt", b"data");
        let (c, _) = real_cleaner(make_config(dir.path(), 100, CleanupMode::Once, false));

        assert_eq!(single_outcome(&c), Outcome::BelowThreshold);
        assert!(keep.exists());
    }

    #[tokio::test]
    async fn run_once_mode_executes_and_returns() {
        let dir = tempfile::tempdir().expect("tempdir");
        let doomed = create_file(dir.path(), "once.txt", b"x");

        let (c, _) = real_cleaner(make_config(dir.path(), 0, CleanupMode::Once, false));
        c.run().await;

        assert!(!doomed.exists());
    }

    #[tokio::test]
    async fn run_interval_mode_returns_on_cancel() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (c, token) = real_cleaner(make_config(dir.path(), 100, CleanupMode::Interval, true));

        let stopper = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(50)).await;
            token.cancel();
        });
        tokio::time::timeout(Duration::from_secs(5), c.run())
            .await
            .expect("run() did not exit after cancellation");
        stopper.await.expect("stopper");
    }
}
