# Changelog

Notable changes to the volume-autoscaler app (the container image), newest first. Entries are headed by the image version and its release date (KST).

## 0.1.0 (2026-11-04)

First release under the volume-autoscaler name, the successor of external-ebs-autoresizer (last released as 0.8.0), which is no longer released. It adds a second autoscaler for PersistentVolumeClaims. Both run from one image.

### Added

- `PersistentVolumeClaimAutoscaler` custom resource (`autoscaling.volume-autoscaler.sh/v1alpha1`, namespaced, short name `pvca`). It grows every claim of a StatefulSet, or one PersistentVolumeClaim, when space or inode utilization from `kubelet_volume_stats_*` crosses the target. See [docs/persistent-volume-claim-autoscaler.md](docs/persistent-volume-claim-autoscaler.md).
- The spec follows the HorizontalPodAutoscaler: `scaleTargetRef`, `maxCapacity`, `metrics` (`storage` and `inodes`), and `behavior.scaleUp` with `Percent` and `GiB` policies, `selectPolicy`, and `cooldownDuration` (default `30m`, at least `1m`). The schema and CEL rules reject an invalid spec on admission.
- A claim is left alone when it is not bound, its StorageClass does not allow expansion, its previous resize has not finished, or it has no metrics. After a resize it also waits, as `AwaitingFilesystemResize`, until kubelet reports a larger filesystem than the one recorded in `status.claims[].capacityAtLastScale`, so a stale measurement never grows it twice. Each step is clamped to `maxCapacity`. Two autoscalers targeting the same claim both stop with `AmbiguousSelector`.
- StorageClass pre-check. A missing or non-expandable class on a target claim or on a StatefulSet `volumeClaimTemplate` (the default class when the template names none) turns `AbleToScale` false with reason `VolumeExpansionNotAllowed` and records one Warning Event, before the first claim exists.
- A claim with the Argo CD `argocd.argoproj.io/tracking-id` annotation shows its Application in `status.claims[].trackedBy`, and each resize records a `GitOpsDriftExpected` Warning Event naming the `ignoreDifferences` entry the Application needs to accept the larger size.
- `kubectl get pvca` prints `TARGET`, `UTILIZATION`, `VOLUMES`, `STATUS`, and `LASTSCALE`. `status.claims` lists each claim's capacity, desired capacity, utilization, cooldown, and reason.
- `--controllers` flag (env `CONTROLLERS`) choosing which of `instance-volume-autoscaler`, `persistent-volume-claim-autoscaler`, and `unused-volume-scan` run. All three by default.
- `metrics` config block (`url`, `tenantId`, `labelSelector`) shared by the claim autoscaler and the throughput recommender. It serves a cluster-local Prometheus and a central Mimir, where `labelSelector` (for example `cluster: prd`) scopes every query to this cluster.
- `claimReconcileInterval` config key (default `1m`, at least `10s`).
- `crd --kind` subcommand flag selecting `instance-volume-autoscaler` or `persistent-volume-claim-autoscaler`. `make crd` renders both into the chart.
- Metrics `volume_autoscaler_claim_utilization_percent`, `volume_autoscaler_claim_capacity_bytes`, `volume_autoscaler_claim_resize_total`, `volume_autoscaler_claim_skip_total`, and `volume_autoscaler_claim_reconcile_total`.
- Examples [examples/statefulset.yaml](examples/statefulset.yaml) and [examples/persistent-volume-claim.yaml](examples/persistent-volume-claim.yaml).

### Changed

- Renamed from external-ebs-autoresizer. Image `ghcr.io/younsl/volume-autoscaler`, API group `autoscaling.volume-autoscaler.sh` (was `autoscaling.external-ebs-autoresizer.sh`), metric prefix `volume_autoscaler_` (was `external_ebs_autoresizer_`), annotation prefix `volume-autoscaler/` (was `external-ebs-autoresizer/`), and config path `/etc/volume-autoscaler/config.yaml`. InstanceVolumeAutoscaler resources must be recreated in the new group.
- `throughputRecommendation.prometheusUrl` and `prometheusTenantId` moved to `metrics.url` and `metrics.tenantId`. The old keys fail startup with a pointer to the new ones.
- Root volume sizes are read with one `DescribeVolumes` call per 100 volumes instead of one per running instance.
- The claim autoscaler reads claims, StatefulSets, StorageClasses, and autoscalers from watch caches holding only the fields it uses, and queries Prometheus for the target claims only. It needs `watch` on PersistentVolumeClaims, StatefulSets, and StorageClasses.
- The async runtime runs two worker threads instead of one per node core.
- In a benchmark with 300 instances (30 targeted) and 200 claims under 10 autoscalers, CPU time fell 62 percent and peak memory 45 percent against the same workload on the previous code.

### Removed

- The AWS SDK's legacy HTTP client (hyper 0.14, rustls 0.21, ring). Every client now shares one rustls stack on aws-lc-rs, and the binary is 1.4 MB smaller.
