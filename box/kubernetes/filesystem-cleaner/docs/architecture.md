# Architecture

This document explains how filesystem-cleaner is built and organized. You'll learn:

- **What each component does** - Clear responsibilities for every module
- **How they work together** - Data flow from CLI input to file deletion
- **Why this design** - Single responsibility principle in action

If you're contributing code, debugging, or just curious about the internals, start here.

## Design Philosophy

filesystem-cleaner follows the Unix philosophy: **"Do one thing and do it well"**. Each module has a single, well-defined responsibility.

## Component Overview

```
┌──────────────────────────┐
│ src/main.rs              │  Entry point - CLI initialization & signal handling
└────────────┬─────────────┘
             │
             ▼
┌──────────────────────────┐
│ src/cleaner.rs           │  Orchestrator - Schedules cleanup & monitors disk usage
└────────────┬─────────────┘
             │
      ┌──────┴──────┬──────────────┐
      ▼             ▼              ▼
┌───────────┐ ┌───────────┐ ┌───────────┐
│ src/      │ │ src/      │ │ src/      │
│ matcher   │ │ scanner   │ │ disk      │
│ Pattern   │ │ Directory │ │ Filesystem│
│ matching  │ │ traversal │ │ usage     │
└───────────┘ └───────────┘ └───────────┘
```

## Components

### src/main.rs
**Responsibility**: Application entry point

- Parse CLI arguments via `config`
- Set up structured logging with `tracing` and `tracing-subscriber`
- Handle shutdown signals (SIGTERM, SIGINT) through a `CancellationToken`
- Start the cleaner on the tokio runtime

### src/config.rs
**Responsibility**: Configuration management

- Define CLI flags with `clap` (derive API)
- Resolve environment variable fallbacks (flag > env > default) through an injectable lookup so precedence is unit-tested without touching the process environment
- Validate configuration values (threshold range, interval minimum, mode, log level)
- Define `CleanupMode` (`once`/`interval`) and `LogLevel`

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

Reports the used-space percentage of the filesystem containing a path via `statvfs(2)` (through the `nix` crate). Usage is `(total - available) / total`, where available is the space usable by unprivileged processes.

### src/cleaner.rs
**Responsibility**: Cleanup orchestration

Coordinates all components to perform the actual cleanup operation.

**Key Responsibilities**:
- **Scheduling**: Run once or periodically based on `CleanupMode`
- **Disk monitoring**: Check if usage exceeds the threshold
- **Coordination**: Use the scanner to find files, then delete them
- **Logging**: Report cleanup progress and results

**Workflow**:
```
1. Check disk usage
   ↓
2. If > threshold:
   ├─> Scan target path for matching files
   ├─> Delete files (or dry-run)
   └─> Log results (freed space, file count)
3. If interval mode:
   └─> Wait for the next tick and repeat
```

### src/bytesize.rs
**Responsibility**: Human-readable byte formatting for log output (e.g. `1.5 MiB`).

### build.rs
**Responsibility**: Build-time metadata. Injects `BUILD_COMMIT` and `BUILD_DATE` so `--version` reports the commit the binary was built from.

## Data Flow

```
User → CLI Args → Config → Cleaner
                              ↓
                    ┌─────────┴─────────┐
                    ↓                   ↓
            Disk Monitor          Matcher + Scanner
                    ↓                   ↓
            Threshold Check       File Collection
                    ↓                   ↓
                    └─────────┬─────────┘
                              ↓
                        File Deletion
                              ↓
                      Logging & Results
```

## Design Principles

### 1. Single Responsibility Principle
Each module does **one thing only**:
- `matcher` - Pattern matching
- `scanner` - File traversal
- `disk` - Filesystem usage
- `cleaner` - Orchestration

### 2. Dependency Direction
```
cleaner → scanner → matcher
   ↓
config, disk
```

Dependencies flow in one direction. Lower-level modules (`matcher`, `scanner`, `disk`) don't know about higher-level ones (`cleaner`).

### 3. Testability
Each module has its own `#[cfg(test)]` unit tests using `tempfile` for real filesystem operations. CI runs the suite in debug and release and enforces 70% line coverage with `cargo-llvm-cov`.

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
→ Modify `src/cleaner.rs` only

Each change is **isolated to one module**, making the codebase easy to maintain and extend.

## Performance Considerations

- **Scanner**: Traverses directories only once per cleanup cycle
- **Matcher**: Glob patterns are compiled into `GlobSet`s once at startup
- **Memory**: Files are collected in memory before deletion (acceptable for typical workspace sizes)
- **Binary**: Statically linked musl binary cross-compiled with cargo-zigbuild, packaged on `scratch`
