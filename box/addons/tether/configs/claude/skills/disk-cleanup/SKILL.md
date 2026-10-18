---
name: disk-cleanup
description: Reclaim local filesystem free space on a macOS workstation by clearing rebuildable build artifacts, package manager caches, and container storage, while leaving in-use caches and user data untouched.
when_to_use: Freeing disk space on the local machine, e.g. "디스크 공간 확보", "로컬 디스크 정리", "디스크 사용률 90% 넘음", "캐시 정리", "빌드 산출물 정리". Not for node or volume disk on servers or Kubernetes (use k8s-manifest or terraform-generator) or for writing a new cleanup script (use bash-generator).
license: Apache-2.0
compatibility: macOS with APFS; optional go, cargo, rustup, uv, npm, brew, podman
metadata:
  version: "1.0.0"
  category: action
  related: bash-generator done-check
allowed-tools: Bash(df *) Bash(du *) Bash(pgrep *) Bash(rustup *) Bash(podman *) Bash(uv cache *) Bash(npm cache *) Bash(brew cleanup *) Bash(go clean *) Read Grep
user-invocable: true
disable-model-invocation: false
---

# Disk Cleanup

## Output Requirements

- Baseline and result both come from `df -h /System/Volumes/Data`; `/` is the sealed system snapshot and always looks nearly empty
- Measure before deleting: `du -sh` per candidate, largest first, and report the reclaimable total before acting
- Only rebuildable data is deleted: build outputs, download caches, unused images, unreferenced toolchains
- Runs without confirmation pauses for the Safe tier; the Ask tier is listed in the report, not deleted
- An item in use by a live process is skipped, never forced; the report names the blocking process by name only
- Process listings use `pgrep -l` or trimmed args; full command lines can carry inline secrets (passwords, tokens) and never go into the report
- The report never prints file contents, env values, or credential paths beyond the directory name

## Tiers

| Tier | Target | Command | Guard |
|------|--------|---------|-------|
| Safe | Rust `target/`, Zig cache, go-build, Go module cache, cargo registry | addons `scripts/clean-build-artifacts.sh` (dry run, then `--yes`) | No `cargo`/`rustc`/`go build` running; a `cargo run` dev server keeps working but rebuilds from scratch on restart |
| Safe | Go module cache outside the script | `go clean -modcache` | Files are 0444; plain `rm -rf` fails |
| Safe | Homebrew downloads and old kegs | `brew cleanup --prune=all -s` | |
| Safe | npm cache | `npm cache clean --force` | |
| Safe | Terragrunt cache | `rm -rf ~/Library/Caches/terragrunt` | No `terragrunt` running |
| Safe | Rust toolchains | `rustup toolchain uninstall <ver>` | Keep `stable`, the default, and every channel in any `rust-toolchain.toml` under `~/github` |
| Safe | Podman dangling images and build cache | `podman image prune -f --build-cache` then `podman machine ssh sudo fstrim -av` | Without `fstrim` the VM disk file does not shrink |
| Safe | uv cache | `uv cache prune`, else `uv cache clean` | Skip when `uvx` processes (e.g. MCP servers) run from `~/.cache/uv/archive-v0`; the lock times out and forcing breaks them |
| Ask | All unused podman images | `podman image prune -af` then `fstrim` | Also removes locally built `localhost/*` images |
| Ask | ML model caches (`~/.cache/huggingface`, `~/.cache/whisper`) | `rm -rf` | No transcription or inference process running; models re-download |
| Ask | Browser test runtimes (`ms-playwright`, `puppeteer`) | `rm -rf` | Re-download on next test run |
| Ask | `node_modules` and `dist` | script `--node` | Reinstall before next build |

## Never Touch

| Path | Reason |
|------|--------|
| `~/.ssh`, `~/.aws`, `~/.kube/config`, `~/.gnupg`, `~/.docker/config.json` | Credentials |
| `~/Library/Application Support`, browser profile and cache dirs | App state, logins |
| `~/Downloads`, `~/Documents`, `~/Desktop` | User data |
| `~/.claude/projects`, `~/.claude/history.jsonl` | Session history used for recall |
| Podman volumes and running containers' images | Persistent data |
| Git working trees, stashes, untracked files | Uncommitted work |

## Reference

| Symptom | Cause |
|---------|-------|
| `podman image prune` freed GB but `~/.local/share/containers` unchanged | VM disk is sparse; run `fstrim -av` inside the machine |
| `rm: ~/.cargo/registry/src/...: Directory not empty` | A cargo build is writing into it; rerun after the build ends |
| `uv cache clean` fails on lock timeout | Long-lived `uvx` processes hold the cache |
| `rm -rf ~/go/pkg/mod` permission denied | Read-only module files; use `go clean -modcache` |

## Report

- Capacity line: before and after `Capacity`, `Used`, `Avail`
- Table of reclaimed size per item
- Skipped items with the blocking reason
- Remaining Ask-tier candidates with size and the exact command to run

## Validation

- `df -h /System/Volumes/Data` shows lower `Used` than baseline
- `rustup toolchain list` still contains every channel referenced by `rust-toolchain.toml`
- `podman ps` lists the same containers as before cleanup
- `pgrep -l` shows previously running MCP or dev servers still alive
