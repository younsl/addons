# Development

```bash
make build          # Debug build
make release        # Optimized release build
make test           # Run tests
make fmt            # Format code
make lint           # Run clippy
make install        # Install to ~/.cargo/bin/
make crd            # Regenerate the chart CRD from the Rust types
```

The module layout behind each reconcile is shown below. See [Grafana annotations](grafana-annotations.md#architecture) for how the phase-transition hook drives notifications and annotations.

![Controller modules](assets/controller-modules.svg)

## CRD

The EKSUpgrade CRD in the chart is generated from `EKSUpgradeSpec` and `EKSUpgradeStatus` in `src/crd/`. Change the Rust types, then run `make crd`, which runs the `crdgen` binary to write the chart template. `crdgen` is a build tool and is not shipped in the image. `cargo test` fails when the chart CRD no longer matches the Rust types.

## Conclusion

`make test`, `make fmt`, and `make lint` are the checks a change has to pass (`make crd` first when a CRD type changed), and the pre-commit hook runs them for changed crates. Start from the module diagram above to find where a change belongs.
