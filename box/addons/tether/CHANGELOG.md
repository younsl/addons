# Changelog

Notable changes per release. Container image versions come from the org.opencontainers.image.version label in the Dockerfile.

## Unreleased

### Added

- The Logs page shows the log level in effect, read from the new GET /api/log-level.
- The config file in the console footer opens in the read-only viewer, and it stays viewable when it fails to parse.
- Reconcile results log how long they took (duration_ms and duration) and shutdown logs the uptime (uptime_secs and uptime), each as a number plus a human readable value.

### Changed

- The warning and error count next to Logs in the navigation is gone.

### Fixed

- The console footer shows the reconcile interval exactly. It used to round to whole minutes, so 30 seconds read as 1 minutes.

## 0.1.0 (2026-10-11)

First release. 0.1.0 was rebuilt at the same version as features landed, so builds pulled at different times differ. The current 0.1.0 image is built from 3dda4fe.

| Build | Change |
| --- | --- |
| c80d57b | Link server only. Reads LINKS_FILE, default /etc/tether/links.toml, and serves /status and /reconcile at the root |
| ddbde2e | Prints an ASCII logo to stderr on startup |
| 14ebc54 | The spec becomes config.toml, read from CONFIG_FILE and --config-file, default /etc/tether/config.toml |
| 7191fbe | Web console, file viewer, package file sync, and the controller layout |
| 3dda4fe | Logs page, and the JSON API moves under /api |

Pin a digest rather than the tag if a deployment depends on one of these builds.

### Added

- A reconcile loop that keeps each source in config.toml linked to its home target, every 5 minutes and on POST /api/reconcile. A missing target is created, a symlink to somewhere else is relinked, and a real file in the way is moved to a timestamped backup directory, never deleted. Dry run reports the plan without changing anything.
- Per-entry links, which link each subdirectory of a source on its own so entries other tools add to the target directory stay in place.
- Brewfile and krewfile regeneration from disk on every reconcile. The Brewfile matches brew bundle dump line for line, and a file is rewritten only when its package lines change. When the Homebrew prefix is not mounted, tether logs a warning once and leaves the Brewfile as it is.
- A web console at / with the last reconcile, links filtered by result, package files, and a reconcile button.
- A read-only file viewer with syntax highlighting for every link source and package file.
- A Logs page at /logs with the last 1000 log events, level filters, a live tail, and a copy button on each line that appears on hover.
- A guide in the console for sharing the Homebrew prefix with the Podman VM.
- JSON API: status, info, tree, file, logs, reconcile, and log-level under /api. Health at /healthz and /readyz, Prometheus metrics at /metrics.
- PUT /api/log-level changes the log filter at runtime.
- The personal macOS dotfiles under configs/ and scripts/, moved from the younsl/dotfiles repository, with a bootstrap script that starts tether with docker or podman.

### Security

- The console and its API answer only when the Host header is localhost, 127.0.0.1, or [::1], and a write needs a same-origin request. Another site can neither read them through DNS rebinding nor post to them.
- HTTP handlers never read the file system. The viewer serves a snapshot the controller takes on each reconcile.
- The viewer shows only files tracked in git, read from .git/index, so untracked local files such as work git settings and signing keys never reach the browser.
- The console pages carry a strict Content-Security-Policy, and highlight.js loads from cdnjs with a pinned version and an integrity hash.
- The image holds only the static binary. The dotfiles and config.toml are mounted at runtime and never copied in.
