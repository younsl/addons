# tether

[![GitHub Container Registry](https://img.shields.io/badge/ghcr.io-younsl%2Ftether-000000?style=flat-square&logo=github&logoColor=white)](https://github.com/younsl/addons/pkgs/container/tether)
[![Rust](https://img.shields.io/badge/rust-1.99.0-000000?style=flat-square&logo=rust&logoColor=white)](https://www.rust-lang.org/)
[![GitHub license](https://img.shields.io/github/license/younsl/addons?style=flat-square&color=000000)](https://github.com/younsl/addons/blob/main/LICENSE)

A small server that keeps [dotfiles](https://github.com/younsl/dotfiles) symlinked into a home directory. The dotfiles bootstrap script starts it, and [links.toml](links.toml) is the single source of truth for which config links where. Every interval, or on request, it compares each target with its source and fixes the drift. Built with Rust 1.99.0 and shipped as a statically linked musl binary on a `scratch` image for `linux/amd64` and `linux/arm64`.

## How it works

The link spec ([links.toml](links.toml)) lives in this repository and is mounted at runtime. Neither the spec nor the dotfiles themselves are baked into the image, which holds only the binary.

| Target state | Action |
| --- | --- |
| Already links to the source | `in_sync`, nothing to do |
| Missing | `create` the link, parent directories included |
| Symlink to somewhere else | `relink` |
| Real file or directory | `backup` to `<backup_root>/<UTC timestamp>/`, then link. Nothing is ever deleted |
| Source missing | `missing_source`, target left alone |

A link with `per_entry = true` links each subdirectory of the source instead of the source itself, so entries other tools create in the target directory stay in place (`~/.claude/skills`). Links are absolute paths, so the home directory must be mounted at the same path as on the host.

## Usage

```bash
docker run -d --name tether \
  --user "$(id -u):$(id -g)" \
  -e HOME="$HOME" \
  -v "$HOME:$HOME" \
  -v "$PWD/links.toml:/etc/tether/links.toml:ro" \
  -p 127.0.0.1:8080:8080 \
  ghcr.io/younsl/tether:0.1.0
```

Start with `-e DRY_RUN=true` and read `/status` to see what the first reconcile would change. `--user` matches the owner of the mounted home, and the port stays on loopback because `/reconcile` is unauthenticated.

| Endpoint | Purpose |
| --- | --- |
| `GET /status` | Last reconcile report as JSON |
| `POST /reconcile` | Run a reconcile now (`202 Accepted`) |
| `GET /healthz` | Liveness |
| `GET /readyz` | Ready once the link spec loads |
| `GET /metrics` | Prometheus metrics (`tether_*`) |

```bash
curl -s -X POST localhost:8080/reconcile
curl -s localhost:8080/status | jq '.entries[] | select(.action != "in_sync")'
```

## Configuration

| Flag | Env | Default |
| --- | --- | --- |
| `--links-file` | `LINKS_FILE` | `/etc/tether/links.toml` |
| `--home` | `HOME` | required |
| `--reconcile-interval` | `RECONCILE_INTERVAL` | `5m` |
| `--dry-run` | `DRY_RUN` | `false` |
| `--port` | `PORT` | `8080` |
| `--log-level` | `LOG_LEVEL` | `info` |
| `--log-format` | `LOG_FORMAT` | `json` |

The spec is reread on every reconcile, so edits take effect without a restart. `~/` expands to `HOME`, and a relative `source` resolves against `source_root`.

## Scope

Only the symlink step moves here. Installing pre-commit hooks, Homebrew autoupdate, and Oh My Zsh need host tools a `scratch` image does not have, so they stay in the dotfiles bootstrap scripts.

## Development

```bash
make run        # dry-run reconcile of links.toml against the local home
make lint test coverage
make docker-build
```
