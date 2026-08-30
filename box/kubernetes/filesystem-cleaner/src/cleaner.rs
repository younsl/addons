//! Cleanup orchestration: monitors disk usage, schedules cleanup runs (once
//! or on an interval), and deletes files collected by the scanner.

use std::fs;
use std::path::Path;
use std::time::{Duration, Instant};

use tokio::time::{self, MissedTickBehavior};
use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn};

use crate::bytesize;
use crate::config::{CleanupMode, Config};
use crate::disk;
use crate::matcher::{Matcher, PatternError};
use crate::scanner::Scanner;

/// Runs cleanup cycles against the configured target paths.
#[derive(Debug)]
pub struct Cleaner {
    config: Config,
    scanner: Scanner,
    shutdown: CancellationToken,
}

impl Cleaner {
    /// Creates a cleaner, compiling the configured glob patterns. Cancelling
    /// `shutdown` stops the interval loop and interrupts an in-flight
    /// deletion pass between files.
    pub fn new(config: Config, shutdown: CancellationToken) -> Result<Self, PatternError> {
        let matcher = Matcher::new(&config.include_patterns, &config.exclude_patterns)?;
        Ok(Self {
            config,
            scanner: Scanner::new(matcher),
            shutdown,
        })
    }

    /// Executes the cleaner in the configured mode until it finishes (once
    /// mode) or the shutdown token is cancelled (interval mode).
    pub async fn run(&self) {
        match self.config.cleanup_mode {
            CleanupMode::Once => {
                info!("Running in 'once' mode - single cleanup execution");
                self.perform_cleanup();
                info!("Cleanup completed, exiting");
            }
            CleanupMode::Interval => {
                info!(
                    interval_minutes = self.config.check_interval_minutes,
                    "Running in 'interval' mode - periodic cleanup"
                );
                self.perform_cleanup();

                let period =
                    Duration::from_secs(self.config.check_interval_minutes.saturating_mul(60));
                let mut ticker = time::interval(period);
                ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);
                // The first tick completes immediately; the cycle above already covered it.
                ticker.tick().await;

                loop {
                    tokio::select! {
                        biased;
                        () = self.shutdown.cancelled() => {
                            info!("Cleaner stopped");
                            return;
                        }
                        _ = ticker.tick() => self.perform_cleanup(),
                    }
                }
            }
        }
    }

    fn interrupted(&self) -> bool {
        self.shutdown.is_cancelled()
    }

    /// Runs one cleanup cycle over all target paths.
    fn perform_cleanup(&self) {
        info!("Starting cleanup cycle");
        let start = Instant::now();

        for path in &self.config.target_paths {
            let usage = Self::disk_usage_percent(path);
            let threshold = self.config.usage_threshold_percent;

            if usage > f64::from(threshold) {
                warn!(
                    path = %path.display(),
                    usage,
                    threshold,
                    cleanup_mode = %self.config.cleanup_mode,
                    dry_run = self.config.dry_run,
                    "Disk usage exceeds threshold, starting cleanup"
                );
                self.clean_path(path);
            } else {
                info!(
                    path = %path.display(),
                    usage,
                    threshold,
                    cleanup_mode = %self.config.cleanup_mode,
                    "Disk usage is below threshold, skipping cleanup"
                );
            }
        }

        info!(
            duration_secs = start.elapsed().as_secs(),
            "Cleanup cycle completed"
        );
    }

    /// Returns the used percentage of the filesystem containing `path`, or 0
    /// when the filesystem cannot be inspected.
    fn disk_usage_percent(path: &Path) -> f64 {
        match disk::usage_percent(path) {
            Ok(usage) => usage,
            Err(e) => {
                error!(path = %path.display(), error = %e, "Failed to get disk usage");
                0.0
            }
        }
    }

    /// Deletes matching files under `base`.
    fn clean_path(&self, base: &Path) {
        if let Err(e) = fs::metadata(base) {
            error!(path = %base.display(), error = %e, "Path does not exist");
            return;
        }

        let initial_usage = Self::disk_usage_percent(base);

        let files = self.scanner.scan(base);
        if files.is_empty() {
            info!(
                path = %base.display(),
                initial_usage_percent = initial_usage,
                "No files to clean"
            );
            return;
        }

        let total_size: u64 = files.iter().map(|f| f.size).sum();
        info!(
            path = %base.display(),
            initial_usage_percent = initial_usage,
            file_count = files.len(),
            total_size = %bytesize::human(total_size),
            "Starting cleanup operation"
        );

        let mut deleted_count = 0_usize;
        let mut freed_space = 0_u64;

        for file in &files {
            if self.interrupted() {
                info!("Cleanup interrupted by shutdown");
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

            if let Err(e) = fs::remove_file(&file.path) {
                error!(file = %file.path.display(), error = %e, "Failed to delete file");
                continue;
            }
            info!(
                file = %file.path.display(),
                size = %bytesize::human(file.size),
                "File deleted successfully"
            );
            deleted_count += 1;
            freed_space += file.size;
        }

        let final_usage = Self::disk_usage_percent(base);
        let usage_reduction = initial_usage - final_usage;

        if self.config.dry_run {
            info!(
                path = %base.display(),
                initial_usage_percent = initial_usage,
                final_usage_percent = final_usage,
                usage_reduction,
                would_delete = files.len(),
                "Cleanup completed (DRY-RUN)"
            );
            return;
        }
        info!(
            path = %base.display(),
            initial_usage_percent = initial_usage,
            final_usage_percent = final_usage,
            usage_reduction,
            deleted_count,
            freed_space = %bytesize::human(freed_space),
            "Cleanup completed successfully"
        );
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::time::Duration;

    use tokio_util::sync::CancellationToken;

    use super::Cleaner;
    use crate::config::{CleanupMode, Config, LogLevel};

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

    fn cleaner(config: Config) -> (Cleaner, CancellationToken) {
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
        let (c, token) = cleaner(make_config(Path::new("/tmp"), 80, CleanupMode::Once, true));
        assert!(!c.interrupted());
        token.cancel();
        assert!(c.interrupted());
    }

    #[test]
    fn disk_usage_percent_for_existing_and_missing_paths() {
        let dir = tempfile::tempdir().expect("tempdir");
        let usage = Cleaner::disk_usage_percent(dir.path());
        assert!((0.0..=100.0).contains(&usage), "usage {usage}");
        // Nonexistent paths cannot be statvfs'd; usage falls back to 0.
        let missing = Cleaner::disk_usage_percent(Path::new("relative/nonexistent"));
        assert!(missing.abs() < f64::EPSILON, "usage {missing}");
    }

    #[test]
    fn clean_path_nonexistent_returns() {
        let target = Path::new("/does/not/exist/zzzz-test");
        let (c, _token) = cleaner(make_config(target, 0, CleanupMode::Once, true));
        c.clean_path(target);
    }

    #[test]
    fn clean_path_empty_directory() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (c, _token) = cleaner(make_config(dir.path(), 0, CleanupMode::Once, false));
        c.clean_path(dir.path());
        assert!(dir.path().exists());
    }

    #[test]
    fn clean_path_dry_run_preserves_files() {
        let dir = tempfile::tempdir().expect("tempdir");
        let keep1 = create_file(dir.path(), "keep1.txt", b"hello");
        let keep2 = create_file(dir.path(), "keep2.txt", b"world");

        let (c, _token) = cleaner(make_config(dir.path(), 0, CleanupMode::Once, true));
        c.clean_path(dir.path());

        assert!(keep1.exists());
        assert!(keep2.exists());
    }

    #[test]
    fn clean_path_deletes_files() {
        let dir = tempfile::tempdir().expect("tempdir");
        let files = [
            create_file(dir.path(), "delete1.txt", b"hello"),
            create_file(dir.path(), "delete2.txt", b"world"),
            create_file(dir.path(), "sub/delete3.txt", b"nested"),
        ];

        let (c, _token) = cleaner(make_config(dir.path(), 0, CleanupMode::Once, false));
        c.clean_path(dir.path());

        for file in &files {
            assert!(
                !file.exists(),
                "{} should have been deleted",
                file.display()
            );
        }
        assert!(
            dir.path().join("sub").exists(),
            "directories are never removed"
        );
    }

    #[test]
    fn clean_path_respects_cancellation() {
        let dir = tempfile::tempdir().expect("tempdir");
        let file1 = create_file(dir.path(), "file1.txt", b"a");
        let file2 = create_file(dir.path(), "file2.txt", b"b");

        let (c, token) = cleaner(make_config(dir.path(), 0, CleanupMode::Once, false));
        token.cancel();
        c.clean_path(dir.path());

        // The deletion loop bails at the stop check before touching the files.
        assert!(file1.exists());
        assert!(file2.exists());
    }

    #[test]
    fn perform_cleanup_below_threshold_skips() {
        let dir = tempfile::tempdir().expect("tempdir");
        let keep = create_file(dir.path(), "keep.txt", b"data");

        let (c, _token) = cleaner(make_config(dir.path(), 100, CleanupMode::Once, false));
        c.perform_cleanup();

        assert!(keep.exists());
    }

    #[test]
    fn perform_cleanup_exceeding_threshold_cleans() {
        let dir = tempfile::tempdir().expect("tempdir");
        let doomed = create_file(dir.path(), "to-delete.txt", b"bytes");

        let (c, _token) = cleaner(make_config(dir.path(), 0, CleanupMode::Once, false));
        c.perform_cleanup();

        assert!(!doomed.exists());
    }

    #[tokio::test]
    async fn run_once_mode_executes_and_returns() {
        let dir = tempfile::tempdir().expect("tempdir");
        let doomed = create_file(dir.path(), "once.txt", b"x");

        let (c, _token) = cleaner(make_config(dir.path(), 0, CleanupMode::Once, false));
        c.run().await;

        assert!(!doomed.exists());
    }

    #[tokio::test]
    async fn run_interval_mode_returns_on_cancel() {
        let dir = tempfile::tempdir().expect("tempdir");
        let token = CancellationToken::new();
        let c = Cleaner::new(
            make_config(dir.path(), 100, CleanupMode::Interval, true),
            token.clone(),
        )
        .expect("cleaner");
        token.cancel();

        tokio::time::timeout(Duration::from_secs(5), c.run())
            .await
            .expect("run() did not exit within 5s after cancellation");
    }

    #[tokio::test]
    async fn run_interval_mode_stops_when_cancelled_while_waiting() {
        let dir = tempfile::tempdir().expect("tempdir");
        let token = CancellationToken::new();
        let c = Cleaner::new(
            make_config(dir.path(), 100, CleanupMode::Interval, true),
            token.clone(),
        )
        .expect("cleaner");

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
