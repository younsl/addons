# Development

## Layout

| Path | Role |
| --- | --- |
| `src/crd.rs` | The NodeConfiguration types the CRD is generated from |
| `src/crd/validate.rs` | Checks the schema cannot express, shared by the controller and the agent |
| `src/crd/binding.rs` | The NodeConfigurationBinding type |
| `src/payload.rs` | The revision hash |
| `src/controller/rollout.rs` | Which node gets which revision, within the budget |
| `src/controller/status.rs` | Conditions and counts |
| `src/controller/cordon.rs` | Cordon and uncordon of a node that moves, with the Node events |
| `src/controller/cleanup.rs` | Bindings and cordons a deleted NodeConfiguration left behind |
| `src/controller/leader.rs` | Leader election over a Lease |
| `src/agent/hostfs.rs` | Path resolution and atomic writes inside the host mount |
| `src/agent/rules.rs` | The rule actions |
| `src/agent/script.rs` | The script runner and its host helper |
| `src/telemetry/` | Health and metrics server |

## Commands

| Command | Purpose |
| --- | --- |
| `make lint` | rustfmt check and clippy with warnings denied |
| `make test` | Unit tests and the CRD drift test |
| `make coverage` | Enforce 70 percent line coverage |
| `make crd` | Regenerate both chart CRDs from the Rust types |
| `make zigbuild` | Static musl binaries for amd64 and arm64 |

The chart CRDs are generated. Change the Rust type, run `make crd`, and commit both. The drift test fails when they differ.

## Testing

The rule engine only needs a directory, so its tests run against a temporary directory that stands in for the host root. The rollout planner is a pure function over nodes and bindings. The controller and agent loops run against a mock API server.

## Release

Bump `org.opencontainers.image.version` in the Dockerfile for the image and `version` in `Chart.yaml` for the chart. Each release runs on push to `main`.

## Conclusion

The Rust types are the source of the CRDs, the chart, and the checks, so a change starts there and `make crd`, `make lint`, and `make test` carry it through. A release is a version bump pushed to `main`, nothing else.
