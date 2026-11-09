# Installation

Node and cluster requirements, the Helm install, and the first `Ward`. Read this before the first install.

## Prerequisites

- Kubernetes 1.30 or later for the Lease ValidatingAdmissionPolicy. Older clusters work with `leaseAdmissionPolicy.enabled=false`.
- Linux x86_64 or arm64 nodes with kernel 5.8 or later and BTF (`/sys/kernel/btf/vmlinux`) for the eBPF probes. Nodes without them fall back to `/proc` polling.
- The BPF LSM enabled on nodes that should block execution: `bpf` listed in `/sys/kernel/security/lsm`. See [eBPF probes](ebpf.md#node-requirements).
- A StorageClass for the manager's PersistentVolumeClaim, unless the manager is disabled.

## Discover versions

The chart is distributed through an OCI registry, so use [crane](https://github.com/google/go-containerregistry/blob/main/cmd/crane/README.md) instead of `helm search repo`:

```bash
crane ls ghcr.io/younsl/charts/ward
```

## Install the chart

```bash
helm upgrade --install ward oci://ghcr.io/younsl/charts/ward \
  --namespace ward-system --create-namespace
```

The chart installs the operator, both CRDs (`wards.security.ward.sh` and `wardsignatures.security.ward.sh`), the RBAC the operator binds for agents, and the Lease admission policy. Chart values are documented in the [chart README](../charts/ward/README.md).

To let the release render the `Ward` object too, set `ward.create=true` and put the spec under `ward.spec`. The CRDs are templates in the same release, so a first `helm install` with `ward.create=true` fails because the `Ward` kind does not exist yet when Helm maps the manifest. Argo CD handles this through the `SkipDryRunOnMissingResource` sync option the chart sets on the object. With plain Helm, install without `ward.create`, then upgrade with it or apply the `Ward` separately.

## Create a Ward

```bash
kubectl apply -f examples/ward.yaml
kubectl get wards -n ward-system
```

The `Ward` phase moves from `Progressing` to `Ready` once every agent and the manager are ready. `status.eventSources` shows how many agents run with `ebpf-lsm`, `ebpf`, or `procfs`.

More examples:

- [Custom signatures and YARA rules](../examples/signatures.yaml)
- [Signed feed with the manager relay](../examples/feed.yaml)
- [Agents only, no manager](../examples/ward-agent-only.yaml)
- [Detections in an existing PostgreSQL server](../examples/ward-external-postgresql.yaml)

## Enable the signature feed

The public feed is an OCI artifact signed keyless in CI, so there is no key to install. Set `spec.feed.image` and the workflow identity that must have signed it:

```yaml
spec:
  feed:
    image: ghcr.io/younsl/ward-db:latest
    keyless:
      issuer: https://token.actions.githubusercontent.com
      identity: https://github.com/younsl/addons/.github/workflows/ward-db.yml@refs/heads/main
```

See [Signature database](signature-database.md).

## Uninstall

```bash
kubectl delete wards --all -A
helm uninstall ward -n ward-system
```

The CRDs stay after uninstall (`crds.keep=true`). The manager's PersistentVolumeClaim stays too, delete it by hand when the detections are no longer needed.

## Conclusion

One Helm release installs the operator for the whole cluster, and each `Ward` deploys the agents. Continue with [Configuration](configuration.md).
