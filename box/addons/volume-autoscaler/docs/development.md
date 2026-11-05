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
make crd            # regenerate both chart CRDs from src/autoscaler/*/crd.rs (needs PyYAML)
```

## Conclusion

The Makefile covers the whole local loop, from `make build` and `make test` to the static multi-arch scratch image built by `make zigbuild` and `make docker-build`. Run `make crd` after changing a type in src/autoscaler/*/crd.rs, and run `make lint` and `make coverage` before pushing, since clippy warnings are denied in CI and line coverage must stay at or above 70%.
