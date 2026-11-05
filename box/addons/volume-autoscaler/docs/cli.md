# Built-in CLI

## Overview

The `volume-autoscaler` binary ships operational subcommands alongside the controller, in the style of Grafana Alloy. They validate the config file and list unused volumes, from a laptop before deploying or inside the [Pod][k8s-pod] with `kubectl exec` after. What each autoscaler targets and does is in its status: `kubectl get iva` and `kubectl get pvca`, with per-target detail in `kubectl describe`. See [InstanceVolumeAutoscaler](instance-volume-autoscaler.md#status) and [PersistentVolumeClaimAutoscaler](persistent-volume-claim-autoscaler.md#status).

Read this if you are:

- A platform or DevOps engineer changing the controller config who wants to check it before deploying.
- An on-call engineer looking for PersistentVolumeClaims and PersistentVolumes no workload uses.

```
Usage: volume-autoscaler [OPTIONS] [COMMAND]

Commands:
  run       Run the controller (the default when no subcommand is given)
  validate  Load and validate the config file, then exit
  unused    List unused PersistentVolumeClaims and PersistentVolumes (reads the Kubernetes API, writes nothing)
  help      Print this message or the help of the given subcommand(s)

Options:
      --config <CONFIG>  Path to the config file (else the mounted default) [env: CONFIG_FILE=]
  -v, --verbose          Verbose output (debug logging) [env: VERBOSE=]
      --controllers <CONTROLLERS>
                         Controllers to run, comma separated [env: CONTROLLERS=]
                         [default: instance-volume-autoscaler,persistent-volume-claim-autoscaler,unused-volume-scan]
  -h, --help             Print help
  -V, --version          Print version
```

With no subcommand (or `run`) the binary starts the controller. All commands
exit `0` on success and non-zero on any error, so they compose with CI checks
and shell scripts.

## The --config flag

Every command reads the same config file, resolved in this order:

1. `--config <path>` flag
2. `CONFIG_FILE` environment variable
3. `/etc/volume-autoscaler/config.yaml` (the chart's [ConfigMap][k8s-configmap] mount)

## The --controllers flag

One image runs every controller. `--controllers` (env `CONTROLLERS`, chart value `controllers`) picks which ones this process runs, comma separated:

- `instance-volume-autoscaler`: grows EC2 root volumes. The throughput recommender runs with it when `throughputRecommendation.enabled` is set.
- `persistent-volume-claim-autoscaler`: grows PersistentVolumeClaims. Requires `metrics.url`.
- `unused-volume-scan`: reports unused claims and volumes.

All three run by default. An unknown name fails at startup. The chart grants RBAC only for the enabled controllers.

```bash
volume-autoscaler --controllers persistent-volume-claim-autoscaler
```

## Commands

### validate

Loads the config file, applies defaults, strict-decodes it (unknown keys are errors), and validates every field. Exits non-zero with the first error. Never contacts AWS or the Kubernetes API.

```console
$ volume-autoscaler validate --config config.example.yaml
config config.example.yaml is valid: region=ap-northeast-2
```

Typical use: a CI check on config changes. A file that still carries the retired `defaultPolicy` or `policies` key fails here with a pointer to InstanceVolumeAutoscaler.

### unused

Lists the [PersistentVolumeClaims][k8s-pvc] and [PersistentVolumes][k8s-pv] no workload is using,
sorted longest-unused first. It reads the Kubernetes API and writes nothing, so
it is safe to run at any time. Requires in-cluster access, so it runs inside the
Pod rather than from a laptop.

```console
$ kubectl exec deploy/volume-autoscaler -- volume-autoscaler unused
KIND                   NAMESPACE  NAME               REASON                   UNUSED_FOR  CAPACITY  STORAGE_CLASS  EBS_VOLUME             BOUND_TO
persistentvolume       -          pvc-9f2c1a4b       released                 512h0m0s    100.0Gi   gp3            vol-0a1b2c3d4e5f67890  legacy/reports
persistentvolumeclaim  analytics  data-clickhouse-3  statefulset_scaled_down  336h0m0s    500.0Gi   gp3            vol-0123456789abcdef0  pvc-3d7e2b91
persistentvolumeclaim  legacy     uploads            no_consumer_pod          72h0m0s     20.0Gi    gp3            vol-0fedcba9876543210  pvc-77c1e004

3 unused objects holding 620.0Gi, out of 214 objects scanned (minUnusedAge 24h0m0s)
```

| Column | Meaning |
|--------|---------|
| `KIND` | `persistentvolumeclaim` or `persistentvolume` |
| `NAMESPACE` | The claim's namespace, `-` for a cluster-scoped volume |
| `REASON` | Why it is unused. See [Unused volume identification](unused-volumes.md#what-counts-as-unused) for each value |
| `UNUSED_FOR` | How long it has been continuously unused |
| `CAPACITY` | Provisioned size, the number that turns the report into a cost |
| `EBS_VOLUME` | The EBS volume ID to price or delete in EC2, `-` when not EBS-backed |
| `BOUND_TO` | The bound volume for a claim, the bound claim for a volume |

The `UNUSED_FOR` clock lives in the [annotation][k8s-annotations] the scan loop writes, so a freshly
started controller reports every object as unused since now until its first pass
lands. Which objects are listed is unaffected. Pass `--all` to include objects
that have not yet been unused for 24 hours.

Nothing in this command deletes anything, and neither does the loop behind it.
"Unused" is an observation about the cluster's current state, not a statement
that the data is disposable: a claim held for a quarterly job and a claim nobody
will ever read again look identical from here.

### run

Starts the controllers `--controllers` enables (identical to running with no subcommand): loads the config, serves health and metrics, elects a leader when enabled, and reconciles on the configured intervals.

## Running inside the Pod

The container image ships the same binary, so every command works via
`kubectl exec` against the mounted ConfigMap:

```bash
kubectl exec deploy/volume-autoscaler -- volume-autoscaler unused
```

`unused` needs no AWS credentials, only the [ClusterRole][k8s-rbac] the chart creates whenever `rbac.create` is true.

## Local verification

`config.example.yaml` at the project root mirrors the chart-rendered config.
The Makefile wraps each command against it; override the file with
`CONFIG=path`:

```bash
make validate     # validate     --config config.example.yaml
make crd          # crdgen, rendered into the chart
```

`make crd` runs `crdgen`, a separate binary the image does not ship, and renders its output into the chart. `cargo test` fails when a chart CRD no longer matches the Rust types.

`unused` has no local equivalent: it reads the Kubernetes API through the
in-cluster config, which does not exist outside a Pod.

## Conclusion

- Is the config valid? `validate`
- Which volumes does no workload use? `unused`
- What does each autoscaler target, and why did it (not) resize? `kubectl get iva`, `kubectl get pvca`, and `kubectl describe`

Wire `validate` into CI for config changes.

[k8s-pod]: https://kubernetes.io/docs/concepts/workloads/pods/
[k8s-configmap]: https://kubernetes.io/docs/concepts/configuration/configmap/
[k8s-pvc]: https://kubernetes.io/docs/concepts/storage/persistent-volumes/#persistentvolumeclaims
[k8s-pv]: https://kubernetes.io/docs/concepts/storage/persistent-volumes/
[k8s-annotations]: https://kubernetes.io/docs/concepts/overview/working-with-objects/annotations/
[k8s-rbac]: https://kubernetes.io/docs/reference/access-authn-authz/rbac/#role-and-clusterrole
