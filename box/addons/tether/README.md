# tether

[![GitHub Container Registry](https://img.shields.io/badge/ghcr.io-younsl%2Ftether-000000?style=flat-square&logo=github&logoColor=white)](https://github.com/younsl/addons/pkgs/container/tether)
[![Rust](https://img.shields.io/badge/rust-1.99.0-000000?style=flat-square&logo=rust&logoColor=white)](https://www.rust-lang.org/)
[![GitHub license](https://img.shields.io/github/license/younsl/addons?style=flat-square&color=000000)](https://github.com/younsl/addons/blob/main/LICENSE)

A small server that keeps personal macOS dotfiles symlinked into a home directory, plus the dotfiles themselves. Shipped as a statically linked Rust 1.99.0 binary on a scratch image for linux/amd64 and linux/arm64. The image holds only the binary, never the dotfiles.

## Quick start

```bash
git clone https://github.com/younsl/addons ~/github/younsl/addons
~/github/younsl/addons/box/addons/tether/scripts/bootstrap/bootstrap-dotfiles.sh
curl -s 127.0.0.1:8080/api/status
```

## Docs

- [Architecture](docs/architecture.md): reconcile loop, actions, image contents
- [Usage](docs/usage.md): run, console, API
- [Configuration](docs/configuration.md): settings, config.toml, mounts, load errors
- [Dotfiles](docs/dotfiles.md): layout, fresh machine, local-only files

## Development

```bash
make run        # dry-run reconcile of config.toml against the local home
make lint test coverage
make docker-build
```
