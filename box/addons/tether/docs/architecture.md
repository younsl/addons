# Architecture

How tether reconciles symlinks and what ends up in its image.

## Reconcile loop

One loop owns every file system change. It runs on start, then on every RECONCILE_INTERVAL tick or POST /reconcile request. Requests made while a run is in progress collapse into one more run. Each run rereads the link spec, plans one step per target, applies the steps, and publishes a report that /status and /metrics serve.

| Target state | Action |
| --- | --- |
| Already links to the source | in_sync, nothing to do |
| Missing | create the link, parent directories included |
| Symlink to somewhere else | relink |
| Real file or directory | backup to a UTC timestamp directory under backup_root, then link. Nothing is ever deleted |
| Source missing | missing_source, target left alone |
| per_entry target is not a real directory | make_dir, then one link per source subdirectory |

A link with per_entry = true links each subdirectory of the source instead of the source itself, so entries other tools create in the target directory stay in place (~/.claude/skills).

## Package files

When config.toml has a packages section, each reconcile also regenerates the Brewfile and krewfile from disk, because the container has no brew or kubectl to ask. The Brewfile matches brew bundle dump line for line: taps, then formulae in dependency order, casks, go, cargo, krew, and npm entries, with descriptions from the Homebrew API cache. A file is rewritten only when its package lines change.

## Console

The binary serves a console at / that shows the last reconcile, filters links by result, and runs a reconcile on demand. It is plain HTML, CSS, and JavaScript compiled into the binary. Clicking a link opens a read-only viewer with syntax highlighting from highlight.js, loaded from cdnjs with a pinned integrity hash.

The viewer reads from a snapshot the controller takes on each reconcile, not from disk, so no request ever reaches a file path. The snapshot holds only files listed in the repository's git index, read directly from .git/index. Everything shown is therefore already public in the repository, and untracked local files stay out.

## Code layout

The source follows kube-rs/controller-rs: lib.rs holds the error type and module exports, main.rs only wires things together, controller.rs owns the reconcile loop and the shared State, and web/ serves the console, API, health, and metrics.

## Safety

- HTTP handlers never touch the file system. They read the last report and wake the loop, so no request input reaches a path.
- The console and its API answer only when the Host header is localhost, 127.0.0.1, or [::1], and a write needs a same-origin request. Another site can neither read them through DNS rebinding nor post to them. Health and metrics stay open.
- The Brewfile is never written when the Homebrew prefix is not mounted, so an empty listing cannot replace the real file.
- A run in progress always finishes before shutdown, so no target is left between remove and link.
- A failed step is reported on its own entry and the remaining steps still run.
- DRY_RUN=true plans and reports without changing anything.

## Image contents

The image is scratch with one file, /app/tether, running as 65532:65532. The link spec and the dotfiles are mounted at runtime and never baked in: .dockerignore is an allowlist that lets only the two tether-linux binaries into the build context.

Links are absolute paths, so the container mounts $HOME at the same path as on the host. A link created inside the container then resolves on the host too.
