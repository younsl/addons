# box

Everything this repository ships. Each component is one directory, grouped by what it ships as.

## Directory structure

```
box/
├── addons/   container images, most with a Helm chart in charts/
├── charts/   standalone Helm charts with no image of their own
└── cli/      command-line tools for a workstation
```

### addons

Long-running [Kubernetes operators](https://kubernetes.io/docs/concepts/extend-kubernetes/operator/), exporters, and services that run in a cluster. Each one is a static Rust binary on a [scratch](https://hub.docker.com/_/scratch) image, usually with its own chart under `charts/`. Images go to `ghcr.io/younsl/{name}` and charts to `ghcr.io/younsl/charts/{name}`.

### charts

[Helm](https://helm.sh/) charts that only package Kubernetes resources, such as policies, node pools, and mesh config, with no image of their own. Published as [OCI artifacts](https://helm.sh/docs/topics/registries/) to `ghcr.io/younsl/charts/{chart}`.

### cli

Command-line and [TUI](https://ratatui.rs/) tools that run on a workstation, not in a cluster. Each ships as a native binary.

## Conventions

- Rust images are cross-compiled with [cargo-zigbuild](https://github.com/rust-cross/cargo-zigbuild) for `linux/amd64` and `linux/arm64`.
- A release runs when a version value changes on `main`: the Dockerfile `org.opencontainers.image.version` label, `version` in `Chart.yaml`, `version` in `Cargo.toml`, or `addons/harbor/VERSION`.
- Charts have no `helm repo` index. List versions with [crane](https://github.com/google/go-containerregistry/blob/main/cmd/crane/README.md): `crane ls ghcr.io/younsl/charts/{chart}`.
- A new component goes where its output belongs: an image in `addons/`, a chart only in `charts/`, a local tool in `cli/`.
