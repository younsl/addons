//! Command-line flags and environment variables resolved into the runtime
//! configuration. Flags take precedence over environment variables, which
//! take precedence over built-in defaults.

use std::fmt;
use std::path::PathBuf;
use std::str::FromStr;

use clap::{Parser, ValueEnum};

use crate::error::ConfigError;

const LONG_VERSION: &str = concat!(
    env!("CARGO_PKG_VERSION"),
    "\ncommit: ",
    env!("BUILD_COMMIT"),
    "\nbuilt: ",
    env!("BUILD_DATE"),
);

const DEFAULT_TARGET_PATHS: &str = "/home/runner/_work";
const DEFAULT_THRESHOLD: i64 = 80;
const DEFAULT_INTERVAL: i64 = 10;
const DEFAULT_INCLUDE: &str = "*";
const DEFAULT_EXCLUDE: &str = "**/.git/**,**/node_modules/**,*.log";

/// Selects between a single cleanup run and periodic cleanup.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
#[value(rename_all = "lower")]
pub enum CleanupMode {
    /// Single cleanup, then exit (init container).
    Once,
    /// Periodic cleanup (sidecar container).
    Interval,
}

impl fmt::Display for CleanupMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Once => "once",
            Self::Interval => "interval",
        })
    }
}

impl FromStr for CleanupMode {
    type Err = String;

    /// Parses a case-insensitive cleanup mode string.
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        <Self as ValueEnum>::from_str(s, true).map_err(|_| format!("invalid cleanup mode: {s}"))
    }
}

/// Minimum severity emitted by the logger.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
#[value(rename_all = "lower")]
pub enum LogLevel {
    Trace,
    Debug,
    Info,
    Warn,
    Error,
}

impl LogLevel {
    pub const fn as_tracing(self) -> tracing::Level {
        match self {
            Self::Trace => tracing::Level::TRACE,
            Self::Debug => tracing::Level::DEBUG,
            Self::Info => tracing::Level::INFO,
            Self::Warn => tracing::Level::WARN,
            Self::Error => tracing::Level::ERROR,
        }
    }
}

impl fmt::Display for LogLevel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Trace => "trace",
            Self::Debug => "debug",
            Self::Info => "info",
            Self::Warn => "warn",
            Self::Error => "error",
        })
    }
}

impl FromStr for LogLevel {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        <Self as ValueEnum>::from_str(s, true).map_err(|_| format!("invalid log level: {s}"))
    }
}

/// Raw command-line flags. Every value is optional so that [`Config::resolve`]
/// can fall back to environment variables and then to defaults.
#[derive(Parser, Debug, Default)]
#[command(name = "filesystem-cleaner", version = LONG_VERSION, about)]
pub struct Args {
    #[arg(
        long,
        value_name = "PATHS",
        help = "Target filesystem paths to clean (comma-separated) [env: TARGET_PATHS] [default: /home/runner/_work]"
    )]
    target_paths: Option<String>,

    #[arg(
        long,
        value_name = "PERCENT",
        allow_negative_numbers = true,
        help = "Disk usage percentage threshold to trigger cleanup (0-100) [env: USAGE_THRESHOLD_PERCENT] [default: 80]"
    )]
    usage_threshold_percent: Option<i64>,

    #[arg(
        long,
        value_name = "MINUTES",
        allow_negative_numbers = true,
        help = "Interval between cleanup checks in minutes [env: CHECK_INTERVAL_MINUTES] [default: 10]"
    )]
    check_interval_minutes: Option<i64>,

    #[arg(
        long,
        value_name = "PATTERNS",
        help = "Glob patterns to include for deletion (e.g., *.tmp, **/cache/**) [env: INCLUDE_PATTERNS] [default: *]"
    )]
    include_patterns: Option<String>,

    #[arg(
        long,
        value_name = "PATTERNS",
        help = "Glob patterns to exclude from deletion (e.g., **/.git/**, **/node_modules/**) [env: EXCLUDE_PATTERNS] [default: **/.git/**,**/node_modules/**,*.log]"
    )]
    exclude_patterns: Option<String>,

    #[arg(
        long,
        value_enum,
        ignore_case = true,
        value_name = "MODE",
        help = "Cleanup mode: 'once' or 'interval' [env: CLEANUP_MODE] [default: interval]"
    )]
    cleanup_mode: Option<CleanupMode>,

    #[arg(
        long,
        num_args = 0..=1,
        default_missing_value = "true",
        value_name = "BOOL",
        help = "Dry run mode - no files will be deleted [env: DRY_RUN] [default: false]"
    )]
    dry_run: Option<bool>,

    #[arg(
        long,
        value_enum,
        ignore_case = true,
        value_name = "LEVEL",
        help = "Log level (trace, debug, info, warn, error) [env: LOG_LEVEL] [default: info]"
    )]
    log_level: Option<LogLevel>,
}

/// Validated runtime configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    pub target_paths: Vec<PathBuf>,
    pub usage_threshold_percent: u8,
    pub check_interval_minutes: u64,
    pub include_patterns: Vec<String>,
    pub exclude_patterns: Vec<String>,
    pub cleanup_mode: CleanupMode,
    pub dry_run: bool,
    pub log_level: LogLevel,
}

impl Config {
    /// Builds a validated configuration from parsed flags, falling back to
    /// the environment (`env` returns the value of a variable, if set) and
    /// then to built-in defaults.
    pub fn resolve<F>(args: Args, env: F) -> Result<Self, ConfigError>
    where
        F: Fn(&str) -> Option<String>,
    {
        let target_paths = split_list(&resolve_string(
            args.target_paths,
            &env,
            "TARGET_PATHS",
            DEFAULT_TARGET_PATHS,
        ));
        if target_paths.is_empty() {
            return Err(ConfigError::EmptyTargetPaths);
        }

        let threshold = resolve_parsed(
            args.usage_threshold_percent,
            &env,
            "USAGE_THRESHOLD_PERCENT",
            DEFAULT_THRESHOLD,
        )?;
        let usage_threshold_percent = match u8::try_from(threshold) {
            Ok(v) if v <= 100 => v,
            _ => return Err(ConfigError::ThresholdOutOfRange(threshold)),
        };

        let interval = resolve_parsed(
            args.check_interval_minutes,
            &env,
            "CHECK_INTERVAL_MINUTES",
            DEFAULT_INTERVAL,
        )?;
        let check_interval_minutes = match u64::try_from(interval) {
            Ok(v) if v >= 1 => v,
            _ => return Err(ConfigError::IntervalTooSmall(interval)),
        };

        let include_patterns = split_list(&resolve_string(
            args.include_patterns,
            &env,
            "INCLUDE_PATTERNS",
            DEFAULT_INCLUDE,
        ));
        let exclude_patterns = split_list(&resolve_string(
            args.exclude_patterns,
            &env,
            "EXCLUDE_PATTERNS",
            DEFAULT_EXCLUDE,
        ));
        let cleanup_mode = resolve_parsed(
            args.cleanup_mode,
            &env,
            "CLEANUP_MODE",
            CleanupMode::Interval,
        )?;
        let dry_run = match args.dry_run {
            Some(v) => v,
            None => env_bool(&env, "DRY_RUN")?.unwrap_or(false),
        };
        let log_level = resolve_parsed(args.log_level, &env, "LOG_LEVEL", LogLevel::Info)?;

        Ok(Self {
            target_paths: target_paths.into_iter().map(PathBuf::from).collect(),
            usage_threshold_percent,
            check_interval_minutes,
            include_patterns,
            exclude_patterns,
            cleanup_mode,
            dry_run,
            log_level,
        })
    }
}

/// Splits a comma-separated string, trimming whitespace and dropping empty
/// entries.
fn split_list(s: &str) -> Vec<String> {
    s.split(',')
        .map(str::trim)
        .filter(|part| !part.is_empty())
        .map(str::to_string)
        .collect()
}

fn resolve_string<F>(flag: Option<String>, env: &F, key: &str, default: &str) -> String
where
    F: Fn(&str) -> Option<String>,
{
    flag.or_else(|| env(key))
        .unwrap_or_else(|| default.to_string())
}

fn resolve_parsed<T, F>(
    flag: Option<T>,
    env: &F,
    key: &'static str,
    default: T,
) -> Result<T, ConfigError>
where
    T: FromStr,
    T::Err: fmt::Display,
    F: Fn(&str) -> Option<String>,
{
    if let Some(value) = flag {
        return Ok(value);
    }
    env(key).map_or(Ok(default), |raw| {
        raw.parse().map_err(|e: T::Err| ConfigError::Env {
            key,
            reason: e.to_string(),
        })
    })
}

fn env_bool<F>(env: &F, key: &'static str) -> Result<Option<bool>, ConfigError>
where
    F: Fn(&str) -> Option<String>,
{
    env(key)
        .map(|raw| {
            parse_bool(&raw).ok_or_else(|| ConfigError::Env {
                key,
                reason: format!("invalid boolean {raw:?}"),
            })
        })
        .transpose()
}

/// Accepts the same spellings as Go's `strconv.ParseBool`, which the previous
/// implementation used for `DRY_RUN`.
fn parse_bool(s: &str) -> Option<bool> {
    match s {
        "1" | "t" | "T" | "TRUE" | "true" | "True" => Some(true),
        "0" | "f" | "F" | "FALSE" | "false" | "False" => Some(false),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use clap::Parser;
    use clap::error::ErrorKind;

    use super::{Args, CleanupMode, Config, LogLevel};
    use crate::error::ConfigError;

    fn no_env(_: &str) -> Option<String> {
        None
    }

    fn env_of(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let map: HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect();
        move |key| map.get(key).cloned()
    }

    fn args(flags: &[&str]) -> Args {
        let argv = std::iter::once("filesystem-cleaner").chain(flags.iter().copied());
        Args::try_parse_from(argv).expect("flags parse")
    }

    #[test]
    fn cleanup_mode_parses_case_insensitively() {
        assert_eq!("once".parse::<CleanupMode>(), Ok(CleanupMode::Once));
        assert_eq!("interval".parse::<CleanupMode>(), Ok(CleanupMode::Interval));
        assert_eq!("ONCE".parse::<CleanupMode>(), Ok(CleanupMode::Once));
        assert_eq!("Interval".parse::<CleanupMode>(), Ok(CleanupMode::Interval));
        assert_eq!(
            "invalid".parse::<CleanupMode>(),
            Err("invalid cleanup mode: invalid".to_string())
        );
        assert_eq!(CleanupMode::Once.to_string(), "once");
        assert_eq!(CleanupMode::Interval.to_string(), "interval");
    }

    #[test]
    fn log_level_maps_to_tracing() {
        let cases = [
            ("trace", LogLevel::Trace, tracing::Level::TRACE),
            ("DEBUG", LogLevel::Debug, tracing::Level::DEBUG),
            ("info", LogLevel::Info, tracing::Level::INFO),
            ("Warn", LogLevel::Warn, tracing::Level::WARN),
            ("error", LogLevel::Error, tracing::Level::ERROR),
        ];
        for (input, level, tracing_level) in cases {
            assert_eq!(input.parse::<LogLevel>(), Ok(level), "{input}");
            assert_eq!(level.as_tracing(), tracing_level);
            assert_eq!(level.to_string(), input.to_lowercase());
        }
        assert_eq!(
            "bogus".parse::<LogLevel>(),
            Err("invalid log level: bogus".to_string())
        );
    }

    #[test]
    fn defaults() {
        let cfg = Config::resolve(args(&[]), no_env).expect("resolve");
        assert_eq!(
            cfg.target_paths,
            vec![std::path::PathBuf::from("/home/runner/_work")]
        );
        assert_eq!(cfg.usage_threshold_percent, 80);
        assert_eq!(cfg.check_interval_minutes, 10);
        assert_eq!(cfg.include_patterns, vec!["*"]);
        assert_eq!(
            cfg.exclude_patterns,
            vec!["**/.git/**", "**/node_modules/**", "*.log"]
        );
        assert_eq!(cfg.cleanup_mode, CleanupMode::Interval);
        assert!(!cfg.dry_run);
        assert_eq!(cfg.log_level, LogLevel::Info);
    }

    #[test]
    fn flags() {
        let cfg = Config::resolve(
            args(&[
                "--target-paths=/tmp, /var/tmp,",
                "--usage-threshold-percent=70",
                "--check-interval-minutes=5",
                "--include-patterns=*.tmp,*.cache",
                "--exclude-patterns=**/keep/**",
                "--cleanup-mode=ONCE",
                "--dry-run",
                "--log-level=Debug",
            ]),
            no_env,
        )
        .expect("resolve");
        assert_eq!(cfg.target_paths.len(), 2);
        assert_eq!(cfg.target_paths[1], std::path::PathBuf::from("/var/tmp"));
        assert_eq!(cfg.usage_threshold_percent, 70);
        assert_eq!(cfg.check_interval_minutes, 5);
        assert_eq!(cfg.include_patterns, vec!["*.tmp", "*.cache"]);
        assert_eq!(cfg.exclude_patterns, vec!["**/keep/**"]);
        assert_eq!(cfg.cleanup_mode, CleanupMode::Once);
        assert!(cfg.dry_run);
        assert_eq!(cfg.log_level, LogLevel::Debug);
    }

    #[test]
    fn env_overrides_defaults() {
        let env = env_of(&[
            ("TARGET_PATHS", "/data"),
            ("USAGE_THRESHOLD_PERCENT", "50"),
            ("CHECK_INTERVAL_MINUTES", "3"),
            ("INCLUDE_PATTERNS", "*.bin"),
            ("EXCLUDE_PATTERNS", "**/.git/**"),
            ("CLEANUP_MODE", "once"),
            ("DRY_RUN", "true"),
            ("LOG_LEVEL", "warn"),
        ]);
        let cfg = Config::resolve(args(&[]), env).expect("resolve");
        assert_eq!(cfg.target_paths, vec![std::path::PathBuf::from("/data")]);
        assert_eq!(cfg.usage_threshold_percent, 50);
        assert_eq!(cfg.check_interval_minutes, 3);
        assert_eq!(cfg.include_patterns, vec!["*.bin"]);
        assert_eq!(cfg.exclude_patterns, vec!["**/.git/**"]);
        assert_eq!(cfg.cleanup_mode, CleanupMode::Once);
        assert!(cfg.dry_run);
        assert_eq!(cfg.log_level, LogLevel::Warn);
    }

    #[test]
    fn flag_beats_env() {
        let env = env_of(&[("USAGE_THRESHOLD_PERCENT", "50"), ("DRY_RUN", "true")]);
        let cfg = Config::resolve(
            args(&["--usage-threshold-percent=90", "--dry-run=false"]),
            env,
        )
        .expect("resolve");
        assert_eq!(cfg.usage_threshold_percent, 90);
        assert!(!cfg.dry_run);
    }

    #[test]
    fn dry_run_env_accepts_go_bool_spellings() {
        for (raw, want) in [
            ("1", true),
            ("T", true),
            ("True", true),
            ("0", false),
            ("f", false),
            ("FALSE", false),
        ] {
            let cfg = Config::resolve(args(&[]), env_of(&[("DRY_RUN", raw)])).expect("resolve");
            assert_eq!(cfg.dry_run, want, "DRY_RUN={raw}");
        }
        let err = Config::resolve(args(&[]), env_of(&[("DRY_RUN", "yes")])).unwrap_err();
        assert!(matches!(err, ConfigError::Env { key: "DRY_RUN", .. }));
        assert_eq!(
            err.to_string(),
            "environment variable DRY_RUN: invalid boolean \"yes\""
        );
    }

    #[test]
    fn invalid_env_integer_is_reported_with_key() {
        let err =
            Config::resolve(args(&[]), env_of(&[("USAGE_THRESHOLD_PERCENT", "abc")])).unwrap_err();
        assert!(matches!(
            err,
            ConfigError::Env {
                key: "USAGE_THRESHOLD_PERCENT",
                ..
            }
        ));
        let err =
            Config::resolve(args(&[]), env_of(&[("CHECK_INTERVAL_MINUTES", "x")])).unwrap_err();
        assert!(matches!(
            err,
            ConfigError::Env {
                key: "CHECK_INTERVAL_MINUTES",
                ..
            }
        ));
        let err = Config::resolve(args(&[]), env_of(&[("CLEANUP_MODE", "sometimes")])).unwrap_err();
        assert_eq!(
            err.to_string(),
            "environment variable CLEANUP_MODE: invalid cleanup mode: sometimes"
        );
        let err = Config::resolve(args(&[]), env_of(&[("LOG_LEVEL", "loud")])).unwrap_err();
        assert_eq!(
            err.to_string(),
            "environment variable LOG_LEVEL: invalid log level: loud"
        );
    }

    #[test]
    fn threshold_range_is_validated() {
        for raw in ["101", "-1"] {
            let flag = format!("--usage-threshold-percent={raw}");
            let err = Config::resolve(args(&[&flag]), no_env).unwrap_err();
            assert_eq!(err, ConfigError::ThresholdOutOfRange(raw.parse().unwrap()));
        }
        for raw in ["0", "100"] {
            let flag = format!("--usage-threshold-percent={raw}");
            assert!(Config::resolve(args(&[&flag]), no_env).is_ok());
        }
        assert_eq!(
            ConfigError::ThresholdOutOfRange(101).to_string(),
            "usage-threshold-percent must be between 0 and 100, got 101"
        );
    }

    #[test]
    fn interval_minimum_is_validated() {
        let err = Config::resolve(args(&["--check-interval-minutes=0"]), no_env).unwrap_err();
        assert_eq!(err, ConfigError::IntervalTooSmall(0));
        assert_eq!(
            err.to_string(),
            "check-interval-minutes must be at least 1, got 0"
        );
        let err =
            Config::resolve(args(&[]), env_of(&[("CHECK_INTERVAL_MINUTES", "-5")])).unwrap_err();
        assert_eq!(err, ConfigError::IntervalTooSmall(-5));
    }

    #[test]
    fn empty_target_paths_are_rejected() {
        let err = Config::resolve(args(&["--target-paths=, ,"]), no_env).unwrap_err();
        assert_eq!(err, ConfigError::EmptyTargetPaths);
        assert_eq!(err.to_string(), "target-paths must not be empty");
    }

    #[test]
    fn invalid_flag_values_fail_at_parse_time() {
        for flags in [
            &["--cleanup-mode=sometimes"][..],
            &["--log-level=loud"],
            &["--usage-threshold-percent=abc"],
            &["--dry-run=maybe"],
        ] {
            let argv = std::iter::once("filesystem-cleaner").chain(flags.iter().copied());
            assert!(Args::try_parse_from(argv).is_err(), "{flags:?}");
        }
    }

    #[test]
    fn version_flag_reports_commit() {
        let err = Args::try_parse_from(["filesystem-cleaner", "--version"]).unwrap_err();
        assert_eq!(err.kind(), ErrorKind::DisplayVersion);
        let rendered = err.to_string();
        assert!(rendered.starts_with(concat!("filesystem-cleaner ", env!("CARGO_PKG_VERSION"))));
        assert!(rendered.contains("commit: "));
    }
}
