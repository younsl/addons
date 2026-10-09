# Development

Build, test, and package the addon from a local checkout.

## Build

```bash
make build          # debug binary into target/debug/
make test           # unit tests
make coverage       # enforce minimum line coverage (70%, cargo-llvm-cov)
make lint           # rustfmt check + clippy (pedantic and nursery as warnings, denied in CI)
make zigbuild       # static musl binaries for linux/amd64 and linux/arm64 (cargo-zigbuild)
make docker-build   # multi-arch scratch image from the zigbuild binaries
```
