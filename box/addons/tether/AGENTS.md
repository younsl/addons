# AGENTS.md

Guidance for coding agents working in this directory. Only non-derivable conventions and traps live here.

## Overview

tether is two things in one directory: a Rust server (src/) that keeps symlinks in a home directory in place, and the personal macOS dotfiles it links (configs/, scripts/). configs/TOOL/ holds every config, with package manager configs under configs/package-managers/TOOL/ and Claude Code skills under configs/claude/skills/. [config.toml](config.toml) maps each one to its home target.

## Never ship or publish personal data

- The image holds only the binary. .dockerignore is an allowlist: everything is excluded, then only the two tether-linux binaries are let back in. Never widen it, never COPY anything else, and never include_str!/include_bytes! a file from configs/.
- This repository is public. Work-specific git includes, signing keys, and site values live in untracked local files: configs/git/config-work, .key files in configs/git/, and ~/.zshrc.local. .gitignore here keeps them out. Never commit company names, work gitdir paths, internal hosts, or ticket keys into configs/.

## Commits

[tether] TYPE(SCOPE): DETAIL. For a dotfiles change the scope is the tool ([tether] feat(nvim): add nvim-treesitter-textobjects via vim.pack), for a server change the module ([tether] fix(linker): ...).

## Server traps

- **HTTP handlers never touch the file system.** They read the last report and wake the reconcile loop through Shared. A handler that reads a configured path is flagged by CodeQL as rust/path-injection.
- **A reconcile in progress always finishes** before shutdown, so no target is left between remove and link.
- **Links are absolute paths**, so the container mounts $HOME at the same path as on the host.

## Dotfiles rules

- **Never edit symlinked targets** (~/.config/nvim, ~/.zshrc, ...). Edit the source in configs/ instead.
- **Hard-coded path**: config.toml and the scripts expect this repository at $HOME/github/younsl/addons.
- **Adding or moving a config needs a matching config.toml entry.** tether rereads it on every reconcile (every 5m, or curl -X POST 127.0.0.1:8080/reconcile).
- **Scripts carry no comments** and every script starts with set -euo pipefail.
- scripts/git/commit-history-cleaner.sh and the chc alias are **destructive**: they wipe all history and force push. Never run them in this monorepo.
- bootstrap-dotfiles.sh is #!/bin/zsh: prompts must use read "VAR?prompt". zsh's read -p reads from a coprocess and silently yields an empty value.
- configs/claude/scripts/statusbar is a local tool, not released. It still follows the repository fmt, clippy, and rust-version rules.

## Shell entry points

Single-letter aliases are load-bearing in daily use. Never rename or shadow them, and never define a new alias that collides. Defined in configs/zsh/.zshrc unless noted.

| Command | Expands to | Source |
|---|---|---|
| c | claude (often c --worktree) | alias |
| k | kubectl | oh-my-zsh kubectl plugin |
| s CONTEXT | switch CONTEXT (kubeswitch) | alias + switcher init zsh |
| j DIR | jump to a frecent directory | oh-my-zsh autojump plugin |
| tgp / tga / tgd / tgo / tgf / tgfu | terragrunt plan/apply/destroy/output/hclfmt/force-unlock | alias |
| git sweep | prune remote-tracking refs, then delete local [gone] branches (removes their worktrees first) | configs/git/config |
| git br | branches by last commit date, in a column table | configs/git/config |
| chc | commit-history-cleaner, **destructive** | alias + configs/zsh/functions/ |

k and j come from oh-my-zsh plugins, so the plugins=() list in .zshrc is load-bearing beyond completion. Dropping kubectl or autojump removes the command itself.

## Tool notes

- **zsh**: fast-syntax-highlighting must stay **last** in plugins. It wraps ZLE widgets and only sees widgets registered before it. Never add zsh-syntax-highlighting alongside it: the buffer is re-parsed twice per keystroke and the later one overwrites the earlier one's region_highlight.
- **mise**: owns every version-sensitive CLI. Add runtimes with mise use -g TOOL@VERSION so the pin lands in configs/package-managers/mise/config.toml.
- **istioctl**: pinned in mise, never brew. It is version-coupled to the control plane and brew's daily --upgrade would bump it past the mesh.
- **nvim**: plugins are managed by built-in vim.pack (configs/nvim/lua/config/pack.lua) and pinned in nvim-pack-lock.json. Auto-update runs daily on VimEnter and rewrites the lockfile, so commit it after it changes. Prefer a native Neovim feature over a plugin.
- **git**: per-org profiles selected by includeIf gitdir:. Work profiles go in the untracked config-work. GPG signing is on globally.
- **Brewfile**: the backup-brewfile pre-commit hook dumps it once per day (date in the untracked .brewfile-last-backup) and stages it.
