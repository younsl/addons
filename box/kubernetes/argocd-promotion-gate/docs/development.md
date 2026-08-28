# Development

## Overview

Where the code lives, what each module is allowed to depend on, how to run the gate against a real cluster, and the end to end tier. Also the tests that pin decisions which are easy to undo by accident.

For anyone changing the code. Behaviour and settings live in [docs/configuration.md](configuration.md) instead.

```bash
make            # fmt, lint, test, build
make lint       # cargo fmt --check + cargo clippy --all-targets -- -D warnings
make test       # cargo test
make coverage   # cargo llvm-cov, enforce the 70% floor
make zigbuild   # static linux/amd64 and linux/arm64 binaries via cargo-zigbuild
```

The toolchain is Rust 1.98.0, edition 2024. Cross-compilation uses [cargo-zigbuild](https://github.com/rust-cross/cargo-zigbuild) against the musl targets, which is what the release workflow does too, so `make zigbuild` reproduces the shipped binary locally.

## Layout

| Module | Responsibility | External deps |
| --- | --- | --- |
| `src/gate/` | the rules: chain, image parsing, verdict | `serde` |
| `src/config.rs` | load and validate the config file | `serde_yaml` |
| `src/argocd/` | read Applications, resolve desired images | `kube`, `reqwest` |
| `src/engine.rs` | gather facts, delegate the verdict | the above |
| `src/admission/` | AdmissionReview in, verdict out | `axum` |
| `src/extension.rs` | read-only API for the UI panel | `axum` |
| `src/events.rs` | Kubernetes Events for blocked and warned verdicts | `kube`, `k8s-openapi` |
| `src/servingcert.rs` | load and reload the webhook keypair | `rustls`, `x509-parser` |
| `src/uiextension.rs` | the embedded extension script and its tar layout | `tar` |
| `src/observability/` | Prometheus registry and metric set | `prometheus-client` |
| `src/app.rs` | startup logging, listeners, graceful shutdown | `axum-server` |

`src/gate/` holds no I/O at all. Every fact a verdict depends on arrives in a `gate::Input`, which is why the rules are covered by table tests with no cluster and no fake client.

The engine exists so the webhook and the UI API cannot diverge. Both call `Engine::evaluate`. Neither has rules of its own.

## Running against a live cluster

The gate needs a serving certificate for the webhook listener. It is loaded before either listener opens, so a local run needs a pair even when only the plain HTTP read paths are exercised:

```bash
openssl req -x509 -newkey rsa:2048 -nodes -days 1 \
  -subj '/CN=localhost' -keyout /tmp/tls.key -out /tmp/tls.crt

cat > /tmp/gate.yaml <<'EOF'
chain: [stage, prod]
gatedEnvs: [prod]
imageTag:
  enabled: false
EOF

cargo run -- \
  --config /tmp/gate.yaml \
  --kubeconfig ~/.kube/config \
  --tls-cert-file /tmp/tls.crt --tls-key-file /tmp/tls.key \
  --log-format text --log-level debug &

curl -s 'localhost:8080/api/v1/gate?app=prod-payment-api' | jq
curl -s localhost:8080/api/v1/config | jq
```

With `imageTag.enabled: true`, point `argocd.serverAddress` at a port-forwarded argocd-server and mint a token as described in [configuration.md](configuration.md).

## Exercising the webhook by hand

The endpoint takes an ordinary `AdmissionReview`, so a denial can be reproduced without touching Argo CD:

```bash
curl -sk https://localhost:8443/validate \
  -H 'Content-Type: application/json' \
  -d '{
    "apiVersion": "admission.k8s.io/v1",
    "kind": "AdmissionReview",
    "request": {
      "uid": "test-1",
      "name": "prod-payment-api",
      "namespace": "argocd",
      "operation": "UPDATE",
      "userInfo": {"username": "system:serviceaccount:argocd:argocd-server"},
      "object": {
        "metadata": {"name": "prod-payment-api"},
        "spec": {"project": "prod"},
        "operation": {"sync": {}, "initiatedBy": {"username": "dev@example.com"}}
      },
      "oldObject": {"metadata": {"name": "prod-payment-api"}, "spec": {"project": "prod"}}
    }
  }' | jq
```

Drop the `operation` field from `object` and the response flips to an unconditional allow: that is the "not a sync request" path, which every status write from the application controller takes.

## Testing conventions

Unit tests live in a `#[cfg(test)]` module in the same file. Table tests where the cases are genuinely parallel, named tests otherwise. Assertions say what broke rather than dumping a struct, because the failure message is the only thing a future reader gets.

Nothing talks to a cluster. `AppReader`, `ImageResolver`, and `EventSink` are traits with one Kubernetes implementation each and an in-memory double under `engine::testing` and `events::testing`. The argocd-server client is tested against [wiremock](https://docs.rs/wiremock), and the certificate reloader against pairs minted with [rcgen](https://docs.rs/rcgen). HTTP handlers are driven through `tower::ServiceExt::oneshot` with no socket.

Tests worth keeping in mind when changing behaviour, because each pins a decision that is easy to undo by accident:

- `engine::tests::kubernetes_failure_is_not_a_missing_upstream`. A read failure must never be mistaken for an absent upstream, which would open the gate on any API hiccup.
- `engine::tests::missing_upstream_is_allowed_and_skips_images`. The reverse case, where a missing upstream must reach the gate as "absent", not as an error, or every app without a counterpart hits the `onError` policy.
- `engine::tests::skips_image_lookup_when_upstream_already_fails`. The remote call must not happen on a path that is already a denial.
- `argocd::api::tests::desired_images_fails_whole_lookup_on_partial_failure`. A partial image list would let a mismatch through on the kind that failed to load.
- `admission::handler::tests::fails_open_on_malformed_input`. The gate cannot judge what it cannot parse, and refusing everything would take all syncs down with it.
- `admission::handler::tests::dry_run_computes_but_never_writes`. `sideEffects: NoneOnDryRun` is a promise the handler has to keep by itself.

## End to end

`hack/e2e` drives a local kind cluster. Nothing in it touches the real kubeconfig: each mode writes its own `.kubeconfig-*` file, and every script refuses to run against a context that is not on localhost.

| Script | What it does |
| --- | --- |
| `up.sh` | cluster with the Application CRD, fixtures, and the gate. No Argo CD, because a running controller would keep rewriting the statuses the tests set |
| `test.sh` | the assertions, driven through real admission: denials, exemptions, the match conditions, the panel API agreeing with the webhook, the metrics |
| `tag-test.sh` | the image tag comparison, with the gate built on the host and `stub-argocd-server.py` standing in for the one Argo CD route it calls |
| `up-argocd.sh` | a second cluster with a real Argo CD and the UI extension wired in, for pressing Sync by hand |
| `setup-rollback.sh` | drives that cluster to the state a rollback test starts from |
| `down.sh` | removes both clusters and their kubeconfigs |

```bash
hack/e2e/up.sh && hack/e2e/test.sh && hack/e2e/tag-test.sh
hack/e2e/down.sh
```

`up.sh` and `up-argocd.sh` cross-compile a static musl binary with cargo-zigbuild for the node architecture and build the scratch image from it, the same way the release workflow does. That needs `cargo install cargo-zigbuild` and the `aarch64-unknown-linux-musl` or `x86_64-unknown-linux-musl` target installed. The staged `argocd-promotion-gate-linux-*` binary lands in the project root and is gitignored.

kind runs on podman when no docker daemon answers, which `common.sh` decides once and `down.sh` reuses so a teardown looks for node containers with the same engine. `BUILDER` and `KIND_EXPERIMENTAL_PROVIDER` override it.

## Release

Bump `org.opencontainers.image.version` in the `Dockerfile` and merge to `main`. The Rust scratch container workflow runs fmt, clippy, tests, and the coverage floor, cross-compiles both architectures with cargo-zigbuild, and pushes a multi-arch image to GHCR, skipping if the version already exists. Bump `version` in `charts/argocd-promotion-gate/Chart.yaml` to publish the chart.

Chart README is generated by helm-docs from `values.yaml` comments. Regenerate with `make -C ../charts docs` and never edit it directly.
