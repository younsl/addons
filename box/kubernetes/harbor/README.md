# harbor

[![GitHub Container Registry](https://img.shields.io/badge/ghcr.io-harbor%2F*-black?style=flat-square&logo=docker&logoColor=white)](https://github.com/younsl?tab=packages&repo_name=addons&q=harbor)
[![Harbor](https://img.shields.io/badge/harbor-v2.15.2-black?style=flat-square&logo=harbor&logoColor=white)](https://github.com/goharbor/harbor/releases/tag/v2.15.2)
[![Platform](https://img.shields.io/badge/platform-linux%2Farm64-black?style=flat-square&logo=linux&logoColor=white)](https://aws.amazon.com/ec2/graviton/)
[![Rust](https://img.shields.io/badge/rust-1.98.1-black?style=flat-square&logo=rust&logoColor=white)](https://www.rust-lang.org/)
[![GitHub license](https://img.shields.io/github/license/younsl/addons?style=flat-square&color=black)](https://github.com/younsl/addons/blob/main/LICENSE)

A bundle of custom-built [Harbor](https://goharbor.io/) v2.15.2 images for [AWS Graviton](https://aws.amazon.com/ec2/graviton/) (`linux/arm64`). Upstream publishes the v2.15.x images for amd64 only, so this component rebuilds every image the [harbor-helm](https://github.com/goharbor/harbor-helm) chart deploys from the unmodified upstream source, driven by `harbor-arm64`, a small Rust CLI.

This is a stopgap. Harbor v2.16.0 officially supports `linux/arm64`: multi-arch support was merged to `main` in [goharbor/harbor#22311](https://github.com/goharbor/harbor/pull/22311), and the maintainers confirmed it ships with v2.16.0 in [goharbor/harbor#23558](https://github.com/goharbor/harbor/issues/23558). The [v2.16.0 release plan](https://github.com/goharbor/harbor/issues/24016) targets the end of October 2026. Once you upgrade to v2.16.0, switch back to the upstream images and delete this component.

## Images

Every image is single-architecture `linux/arm64`, tagged with the upstream Harbor version.

| Image | Chart value ([harbor-helm 1.19.2](https://github.com/goharbor/harbor-helm/releases/tag/v1.19.2)) |
| --- | --- |
| `ghcr.io/younsl/harbor/nginx-photon:v2.15.2` | `nginx.image` |
| `ghcr.io/younsl/harbor/harbor-portal:v2.15.2` | `portal.image` |
| `ghcr.io/younsl/harbor/harbor-core:v2.15.2` | `core.image` |
| `ghcr.io/younsl/harbor/harbor-jobservice:v2.15.2` | `jobservice.image` |
| `ghcr.io/younsl/harbor/registry-photon:v2.15.2` | `registry.registry.image` |
| `ghcr.io/younsl/harbor/harbor-registryctl:v2.15.2` | `registry.controller.image` |
| `ghcr.io/younsl/harbor/trivy-adapter-photon:v2.15.2` | `trivy.image` |
| `ghcr.io/younsl/harbor/harbor-db:v2.15.2` | `database.internal.image` |
| `ghcr.io/younsl/harbor/valkey-photon:v2.15.2` | `redis.internal.image` |
| `ghcr.io/younsl/harbor/harbor-exporter:v2.15.2` | `exporter.image` |

`prepare` and `harbor-log` are not built. Only the docker-compose installer uses them.

## Usage

`harbor-arm64 values` prints the chart overrides for every image. Layer it under your own values:

```bash
cargo run --quiet --manifest-path box/kubernetes/harbor/Cargo.toml -- values > values-arm64.yaml

helm repo add harbor https://helm.goharbor.io
helm upgrade --install harbor harbor/harbor --version 1.19.2 \
  -f values-arm64.yaml \
  -f values.yaml
```

The images do not run on amd64 nodes. Schedule every Harbor component onto Graviton nodes with the per-component `nodeSelector` (`kubernetes.io/arch: arm64`) in the chart values.

## How it works

`harbor-arm64 build` clones `goharbor/harbor` at the tag in [VERSION](VERSION) and runs the upstream Makefile on a native arm64 Docker host. The Makefile passes no `--platform`, so every compile container and image build runs on the host architecture. Emulation is not an option, the host itself must be arm64.

Only three things deviate from an upstream build:

| Deviation | Why |
| --- | --- |
| Build `goharbor/photon:5.0-legacy` locally from `make/photon/common/Dockerfile` | The published tag is amd64 only, so every `Dockerfile.base` would pull amd64 ([goharbor/harbor#23748](https://github.com/goharbor/harbor/issues/23748)). Base images are then built with `BUILD_BASE=true PULL_BASE_FROM_DOCKERHUB=false`. |
| `ENV GOARCH=amd64` becomes `arm64` in `make/photon/exporter/Dockerfile` | The exporter build stage pins the architecture. |
| Trivy asset `_Linux-64bit.tar.gz` becomes `_Linux-ARM64.tar.gz` in `Makefile` | The download URL names the x86_64 release asset. |

Each patch fails the build if its target text is missing, so an upstream change surfaces instead of silently producing amd64 artifacts.

Before anything is pushed, the build checks that every image reports `arm64` and reads the ELF header of each compiled or downloaded binary (core, jobservice, registryctl, registry, trivy, scanner-trivy, exporter) to confirm it targets aarch64. Image metadata alone would pass an amd64 binary copied into an arm64 base.

## Building

Requirements: a native arm64 Docker host (Linux on Graviton, or Docker Desktop on Apple Silicon), `git`, `make`, and Rust 1.98.1.

```bash
cd box/kubernetes/harbor

# Log every command without running anything
make dry-run

# Build, verify and push to a registry you are logged in to
cargo run --release -- build --push --registry registry.example.com/harbor
```

| Flag | Default | Purpose |
| --- | --- | --- |
| `--harbor-version` | [VERSION](VERSION) | Harbor git tag to build |
| `--registry` | `ghcr.io/younsl/harbor` (env `REGISTRY_PREFIX`) | Target image prefix |
| `--workdir` | `$TMPDIR/harbor-arm64-<tag>` | Source checkout, replaced on every run |
| `--push` | off | Push after tagging |
| `--dry-run` | off | Log commands instead of running them |

Compile containers run as root, so on Linux a previous checkout can hold root-owned files. Remove the workdir with `sudo` if a rerun cannot replace it.

## Release

Bumping [VERSION](VERSION) on `main` triggers [release-harbor-arm64.yml](../../../.github/workflows/release-harbor-arm64.yml). It builds on an `ubuntu-26.04-arm` runner and skips when every image already has the tag on GHCR. To rebuild a published version, dispatch the workflow with `force=true`.

## Development

```bash
make test       # unit tests
make coverage   # 70% line coverage gate (cargo-llvm-cov)
make lint       # rustfmt check + clippy with warnings denied
```

## License

Apache License 2.0. See [LICENSE](../../../LICENSE). Harbor itself is licensed under Apache License 2.0 by the Harbor authors.
