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

## Safety

- HTTP handlers never touch the file system. They read the last report and wake the loop, so no request input reaches a path.
- A run in progress always finishes before shutdown, so no target is left between remove and link.
- A failed step is reported on its own entry and the remaining steps still run.
- DRY_RUN=true plans and reports without changing anything.

## Image contents

The image is scratch with one file, /app/tether, running as 65532:65532. The link spec and the dotfiles are mounted at runtime and never baked in: .dockerignore is an allowlist that lets only the two tether-linux binaries into the build context.

Links are absolute paths, so the container mounts $HOME at the same path as on the host. A link created inside the container then resolves on the host too.
