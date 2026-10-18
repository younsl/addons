# Building

Build, verify, and push the images yourself.

Requirements: a native arm64 Docker host (Linux on Graviton, or Docker Desktop on Apple Silicon), `git`, `make`, and Rust 1.99.1.

```bash
cd box/addons/harbor

# Log every command without running anything
make dry-run

# Build, verify and push to a registry you are logged in to
cargo run --release -- build --push --registry registry.example.com/harbor
```

| Flag | Default | Purpose |
| --- | --- | --- |
| `--harbor-version` | [VERSION](../VERSION) | Harbor git tag to build |
| `--registry` | `ghcr.io/younsl/harbor` (env `REGISTRY_PREFIX`) | Target image prefix |
| `--workdir` | `$TMPDIR/harbor-arm64-<tag>` | Source checkout, replaced on every run |
| `--push` | off | Push after tagging |
| `--dry-run` | off | Log commands instead of running them |

Compile containers run as root, so on Linux a previous checkout can hold root-owned files. Remove the workdir with `sudo` if a rerun cannot replace it.

## Conclusion

Build on a native arm64 Docker host. Run make dry-run first, then build with --push and your registry prefix.
