# Dotfiles

The personal macOS dotfiles that tether links into place.

## Layout

| Path | Contents |
| --- | --- |
| [configs](../configs) | One directory per tool, package managers under configs/package-managers/, Claude Code skills under configs/claude/skills/ |
| [scripts](../scripts) | Bootstrap, repo cloning, krew and Brewfile backup |
| [config.toml](../config.toml) | Which config links to which home target |

Dotfile managers such as [chezmoi](https://www.chezmoi.io/) and [Nix](https://nixos.org/) are intentionally not used.

## Fresh machine

```bash
git clone https://github.com/younsl/addons ~/github/younsl/addons
~/github/younsl/addons/box/addons/tether/scripts/bootstrap/bootstrap-dotfiles.sh
~/github/younsl/addons/box/addons/tether/scripts/bootstrap/bootstrap-omz.sh
```

bootstrap-dotfiles.sh starts tether with docker or podman, installs the repository pre-commit hooks, and turns on Homebrew autoupdate. bootstrap-omz.sh installs Oh My Zsh and its plugins. Both stay host scripts because a scratch image has none of those tools.

## Adding a config

1. Add it under configs/TOOL/.
2. Add a [[links]] entry to config.toml.
3. curl -s -X POST 127.0.0.1:8080/reconcile, or wait for the next interval.

## Local-only files

This repository is public. These files are loaded at runtime but gitignored, and never committed:

| File | Purpose |
| --- | --- |
| configs/git/config-work | Work git settings, including work includeIf gitdir: profiles. Included by configs/git/config |
| .key files in configs/git/ | Signing key settings |
| ~/.zshrc.local | Site values such as Jira and registry hosts for Claude Code skills, sourced last by .zshrc |

## Brewfile backup

The backup-brewfile pre-commit hook dumps configs/package-managers/brew/Brewfile once per day, appends a package count summary, and stages it. The last run date is kept in the untracked .brewfile-last-backup.
