# Usage

Deploy Harbor on Graviton nodes with the arm64 images through harbor-helm.

`harbor-arm64 values` prints the chart overrides for every image. Layer it under your own values:

```bash
cargo run --quiet --manifest-path box/addons/harbor/Cargo.toml -- values > values-arm64.yaml

helm repo add harbor https://helm.goharbor.io
helm upgrade --install harbor harbor/harbor --version 1.19.2 \
  -f values-arm64.yaml \
  -f values.yaml
```

The images do not run on amd64 nodes. Schedule every Harbor component onto Graviton nodes with the per-component `nodeSelector` (`kubernetes.io/arch: arm64`) in the chart values.

## Conclusion

Generate the overrides with harbor-arm64 values, layer them under your own values, and pin every component to arm64 nodes.
