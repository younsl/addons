# Architecture

This document explains how filesystem-cleaner is built and organized. You'll learn:

- **What each component does** - Clear responsibilities for every module
- **How they work together** - Data flow from CLI input to file deletion
- **Why this design** - Single responsibility principle in action

If you're contributing code, debugging, or just curious about the internals, start here.

## Design Philosophy

filesystem-cleaner follows the Unix philosophy: **"Do one thing and do it well"**. Each module has a single, well-defined responsibility, and everything that touches the filesystem sits behind a small trait so the policy can be tested without it.

## Component Overview

```
┌──────────────────────────┐
│ src/main.rs              │  Entry point - CLI initialization & signal handling
└────────────┬─────────────┘
             │
             ▼
┌──────────────────────────┐      ┌──────────────────────────┐
│ src/cleaner.rs           │─────▶│ src/schedule.rs          │  once / interval loop
│ Orchestrator             │      └──────────────────────────┘
└────────────┬─────────────┘
             │
   ┌─────────┼─────────────┬──────────────┐
   ▼         ▼             ▼              ▼
┌────────┐ ┌────────┐ ┌───────────┐ ┌─────────────┐
│ matcher│ │ scanner│ │ disk      │ │ remover     │
│ Glob   │ │ Walk   │ │ DiskUsage │ │ FileRemover │
│ match  │ │ tree   │ │ trait     │ │ trait       │
└────────┘ └────────┘ └───────────┘ └─────────────┘
```

## Components

### src/main.rs
**Responsibility**: Application entry point

- Parse CLI arguments via `config`
- Set up structured logging with `tracing` and `tracing-subscriber`
- Handle shutdown signals (SIGTERM, SIGINT) through a `CancellationToken`
- Start the cleaner on the tokio runtime with the real backends (`Statvfs`, `FsRemover`)

### src/config.rs
**Responsibility**: Configuration management

- Define CLI flags with `clap` (derive API)
- Resolve environment variable fallbacks (flag > env > default) through an injectable lookup so precedence is unit-tested without touching the process environment
- Validate configuration values (threshold range, interval minimum, mode, log level)
- Define `CleanupMode` (`once`/`interval`) and `LogLevel`

### src/error.rs
**Responsibility**: Error types shared across modules (`ConfigError`, `PatternError`), defined with `thiserror`.

### src/matcher.rs
**Responsibility**: Pattern matching logic

Answers one question: *"Does this relative path match the configured glob patterns?"*

Glob patterns are compiled into `globset` sets at startup using the crate's default settings: `*` and `?` match across `/`, and `**` as a full component matches zero or more path components. These semantics are what deployed patterns rely on, see [Glob Pattern Guide](glob-patterns.md).

**Key Methods**:
- `should_exclude(rel) -> bool` - Check if path matches exclude patterns
- `should_include(rel) -> bool` - Check if path matches include patterns

### src/scanner.rs
**Responsibility**: File system traversal

Walks directory trees and collects files based on pattern rules.

**How it works**:
1. Start from the target path
2. For each entry (sorted by name for deterministic logs):
   - Calculate the relative path (forward slashes)
   - Skip symbolic links (prevents infinite loops and deletions outside target paths)
   - If directory and not excluded, recurse
   - If file, keep it when it passes exclude then include filters
3. Return the list of files to delete with their sizes

### src/disk.rs
**Responsibility**: Filesystem usage

Defines the `DiskUsage` trait and its production implementation `Statvfs`, which reports the used-space percentage of the filesystem containing a path via `statvfs(2)` (through the `nix` crate). Usage is `(total - available) / total`, where available is the space usable by unprivileged processes. Tests implement the trait with fixed values to drive the threshold policy deterministically.

### src/remover.rs
**Responsibility**: File deletion

Defines the `FileRemover` trait and its production implementation `FsRemover` (`std::fs::remove_file`). Tests implement the trait with a recorder that captures requested paths and can fail on demand, which is how the "deletion failed" and "interrupted by shutdown" branches are covered.

### src/schedule.rs
**Responsibility**: Scheduling

Runs a cleanup cycle according to `CleanupMode`: exactly once, or immediately and then every interval until the shutdown token is cancelled. It knows nothing about files or disks, so a new mode (a cron expression, a usage-triggered wake-up) only touches this module. Tests run it on tokio's paused clock.

### src/cleaner.rs
**Responsibility**: Cleanup orchestration

Coordinates all components to perform the actual cleanup operation and returns a structured `CycleReport` (see `cleaner/report.rs`) for every cycle.

**Key Responsibilities**:
- **Disk monitoring**: Check if usage exceeds the threshold through `DiskUsage`
- **Coordination**: Use the scanner to find files, then delete them through `FileRemover` (or list them in dry-run mode)
- **Reporting**: Return per-path outcomes (`BelowThreshold`, `Missing`, `Cleaned` with counters) and log them

**Workflow**:
```
1. Check disk usage
   ↓
2. If > threshold:
   ├─> Scan target path for matching files
   ├─> Delete files (or dry-run)
   └─> Log results (freed space, file count)
3. If interval mode:
   └─> Wait for the next tick and repeat (src/schedule.rs)
```

### src/cleaner/report.rs
**Responsibility**: Result types for a cleanup cycle (`CycleReport`, `PathReport`, `Outcome`, `CleanStats`). Logging and tests consume the same data, and a future metrics or summary output can too.

### src/bytesize.rs
**Responsibility**: Human-readable byte formatting for log output (e.g. `1.5 MiB`).

### build.rs
**Responsibility**: Build-time metadata. Injects `BUILD_COMMIT` and `BUILD_DATE` so `--version` reports the commit the binary was built from.

## Data Flow

```
User → CLI Args → Config → Cleaner ──▶ schedule (once / interval)
                              ↓
                    ┌─────────┴─────────┐
                    ↓                   ↓
            DiskUsage probe       Matcher + Scanner
                    ↓                   ↓
            Threshold Check       File Collection
                    ↓                   ↓
                    └─────────┬─────────┘
                              ↓
                     FileRemover (or dry-run)
                              ↓
                   CycleReport → Logging
```

## Design Principles

### 1. Single Responsibility Principle
Each module does **one thing only**:
- `matcher` - Pattern matching
- `scanner` - File traversal
- `disk` - Filesystem usage
- `remover` - File deletion
- `schedule` - When cycles run
- `cleaner` - Orchestration and reporting

### 2. Dependency Direction
```
cleaner → schedule
cleaner → scanner → matcher
cleaner → disk, remover, cleaner/report
config, error ← (used by all)
```

Dependencies flow in one direction. Lower-level modules (`matcher`, `scanner`, `disk`, `remover`) don't know about higher-level ones (`cleaner`, `schedule`).

### 3. Testability
Each module has its own `#[cfg(test)]` unit tests. Filesystem access is behind `DiskUsage` and `FileRemover`, so the cleanup policy is tested with fixed usage figures and a recording remover through `Cleaner::with_backends`, while a few tests wire the real backends (`Cleaner::new`) against `tempfile` directories. CI runs the suite in debug and release and enforces 70% line coverage with `cargo-llvm-cov`.

### 4. Unix Philosophy
> "Write programs that do one thing and do it well. Write programs to work together."

- Small, focused modules
- Clear interfaces between components
- Easy to understand, test, and modify

## Adding New Features

**Want to add a new pattern type?**
→ Modify `src/matcher.rs` only

**Want to change directory traversal logic?**
→ Modify `src/scanner.rs` only

**Want to add a new scheduling mode?**
→ Add a `CleanupMode` variant and handle it in `src/schedule.rs`

**Want a different usage source or deletion strategy?**
→ Implement `DiskUsage` or `FileRemover` and pass it to `Cleaner::with_backends`

**Want to expose cleanup results (metrics, summary output)?**
→ Consume `CycleReport` from `src/cleaner/report.rs`

Each change is **isolated to one module**, making the codebase easy to maintain and extend.

## Performance Considerations

- **Scanner**: Traverses directories only once per cleanup cycle
- **Matcher**: Glob patterns are compiled into `GlobSet`s once at startup
- **Memory**: Files are collected in memory before deletion (acceptable for typical workspace sizes)
- **Binary**: Statically linked musl binary cross-compiled with cargo-zigbuild, packaged on `scratch`
