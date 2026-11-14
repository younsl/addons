# Development

## Layout

The component is one [Cargo workspace](https://doc.rust-lang.org/cargo/reference/workspaces.html) of three crates under `crates/`, next to the chart. The root `Cargo.toml` holds the version, the dependency versions, the lints, and the release profile, and one `Cargo.lock` pins both images to the same dependencies. Each image is built from the directory of its crate.

| Path | Role |
| --- | --- |
| `crates/operator/` | The controller: crate, Dockerfile, and the `crdgen` binary |
| `crates/agent/` | The agent: crate and Dockerfile |
| `crates/common/` | What both share: the resource types, the revision hash, and the process plumbing |
| `charts/` | The Helm chart, with the generated CRDs |
| `Cargo.toml` | The [workspace](https://doc.rust-lang.org/cargo/reference/workspaces.html): shared package fields, dependency versions, lints, and the release profile |

| Path | Role |
| --- | --- |
| `crates/common/src/crd.rs` | The NodeConfiguration types the CRD is generated from |
| `crates/common/src/crd/validate.rs` | Checks the schema cannot express, shared by the controller and the agent |
| `crates/common/src/crd/binding.rs` | The NodeConfigurationBinding type |
| `crates/common/src/payload.rs` | The revision hash |
| `crates/common/src/boot.rs` | Shared flags, logging, the shutdown signal, and the startup log with the build information |
| `crates/common/src/telemetry/` | Health and metrics server |
| `crates/operator/src/controller/rollout.rs` | Which node gets which revision, within the budget |
| `crates/operator/src/controller/status.rs` | Conditions and counts |
| `crates/operator/src/controller/cordon.rs` | Cordon and uncordon of a node that moves, with the Node events |
| `crates/operator/src/controller/cleanup.rs` | Bindings and cordons a deleted NodeConfiguration left behind |
| `crates/operator/src/controller/leader.rs` | Leader election over a Lease |
| `crates/operator/src/controller/agentset.rs` | The agent DaemonSet and its PodMonitor |
| `crates/operator/src/chart.rs` | Renders the chart CRDs |
| `crates/agent/src/reconcile.rs` | One run of a binding and its status |
| `crates/agent/src/hostfs.rs` | Path resolution and atomic writes inside the host mount |
| `crates/agent/src/rules.rs` | The rule actions |
| `crates/agent/src/script.rs` | The script runner and its host helper |

## Commands

| Command | Purpose |
| --- | --- |
| `make lint` | rustfmt check and clippy with warnings denied |
| `make test` | Unit tests of every crate and the CRD drift test |
| `make coverage` | Enforce 70 percent line coverage of the operator and the agent |
| `make crd` | Regenerate both chart CRDs from the Rust types |
| `make zigbuild` | Static musl binaries of both images for amd64 and arm64 |

The chart CRDs are generated. Change the Rust type, run `make crd`, and commit both. The drift test fails when they differ.

## Testing

The rule engine only needs a directory, so its tests run against a temporary directory that stands in for the host root. The rollout planner is a pure function over nodes and bindings. The controller and agent loops run against a mock API server.

## Release

Bump `org.opencontainers.image.version` in both `crates/operator/Dockerfile` and `crates/agent/Dockerfile` for the images, and `version` in `Chart.yaml` for the chart. The two images share a version, and the chart pulls both at its app version. Each release runs on push to `main`. The release workflow tests the crate of each image, so run `make test`, which covers `common` too, before a release.

## Conclusion

The Rust types are the source of the CRDs, the chart, and the checks, so a change starts there and `make crd`, `make lint`, and `make test` carry it through. A release is a version bump pushed to `main`, nothing else.
