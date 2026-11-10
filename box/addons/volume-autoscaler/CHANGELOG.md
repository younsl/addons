# Changelog

Notable changes to the volume-autoscaler app (the container image), newest first. Entries are headed by the image version and its release date (KST).

## 0.2.0 (2026-11-10)

Adds opt-in orphan PVC cleanup and moves every annotation to the `volume-autoscaler.sh/` prefix.

### Added

- Orphan PVC cleanup, configured by the new `orphanPvcCleanup` config block and disabled by default. It deletes PersistentVolumeClaims the unused volume scan reports, never a PersistentVolume or an EBS volume, which follow their own reclaim policy. See [docs/orphan-pvc-cleanup.md](docs/orphan-pvc-cleanup.md).
- A claim unused for `minUnusedAge` (default `7d`, at least `24h`) is scheduled with a `volume-autoscaler.sh/orphan-pvc-delete-after` annotation and a Warning Event `OrphanPvcDeletionScheduled`, and deleted after `noticePeriod` (default `1d`, at least `1h`) with a Normal Event `OrphanPvcDeleted`. The deadline lives on the claim, so a restart neither resets nor skips the notice.
- A scheduled claim that stops being eligible is canceled with a Normal Event `OrphanPvcDeletionCanceled`, and a later schedule starts a new, full notice.
- `reasons` (default all of `no_consumer_pod`, `statefulset_scaled_down`, and `unbound`), `includeNamespaces`, `excludeNamespaces` (default `[kube-system]`), and `maxDeletionsPerPass` (default `1`, 1 to 100) scope what is deleted.
- Alertmanager alerts `OrphanPvcDeletionUpcoming` (warning), `OrphanPvcDeleted` (info), and `OrphanPvcDeletionFailed` (warning), switched by `orphanPvcCleanup.alertEnabled` (default `true`) behind `alertmanager.enabled`. The upcoming alert is sent once per claim, `orphanPvcCleanup.alertBefore` (default `6h`, at least `1h`) before its deadline, and recorded in `volume-autoscaler.sh/orphan-pvc-alerted-at` so a restart never repeats it.
- Guards that keep a claim: the `volume-autoscaler.sh/orphan-pvc-cleanup` annotation with any value but `enabled`, an owner reference, and a Deployment, ReplicaSet, DaemonSet, StatefulSet, Job, or CronJob Pod template naming the claim. Each delete rechecks for a mounting Pod and carries a UID precondition. A failed list pauses cleanup for the pass.
- Metrics `volume_autoscaler_orphan_pvc_cleanup_total{action}` and `volume_autoscaler_orphan_pvc_cleanup_scheduled`, and `volume_autoscaler_error_total` stages `orphan_pvc_cleanup_references`, `orphan_pvc_cleanup_recheck`, and `orphan_pvc_delete`.

### Changed

- Duration keys accept `d` for days (`7d`, `1d12h`) and reject fractions. A value such as `1.5h`, valid in 0.1.0, is now invalid.
- Invalid config values fail open. An unparseable or out-of-range value takes its default or is clamped, a feature missing what it needs (such as `alertmanager.enabled` without `url`) is turned off, and unknown or retired keys are ignored. Each correction is logged at WARN on startup and printed by `volume-autoscaler validate`. Only a missing `region`, malformed YAML, or a value of the wrong type still fail startup.
- Annotation prefix `volume-autoscaler.sh/` (was `volume-autoscaler/`), the same domain as the API group. Update kubectl commands, dashboards, and alerts that read the old keys, and escape the dot in custom-columns and JSONPath as `volume-autoscaler\.sh/`.
- 0.1.0 keys are still read where losing them would hurt: the `unused-since` clock, so the 24-hour grace period does not restart, and the `protective-cordon` mark, so a Node cordoned by 0.1.0 is still uncordoned. Every 0.1.0 key is removed on the next write to the object.

## 0.1.0 (2026-11-04)

First release under the volume-autoscaler name, the successor of external-ebs-autoresizer (last released as 0.8.0), which is no longer released. It adds a second autoscaler for PersistentVolumeClaims. Both run from one image.

### Added

- `PersistentVolumeClaimAutoscaler` custom resource (`autoscaling.volume-autoscaler.sh/v1alpha1`, namespaced, short name `pvca`). It grows every claim of a StatefulSet, or one PersistentVolumeClaim, when space or inode utilization from `kubelet_volume_stats_*` crosses the target. See [docs/persistent-volume-claim-autoscaler.md](docs/persistent-volume-claim-autoscaler.md).
- The spec follows the HorizontalPodAutoscaler: `scaleTargetRef`, `maxCapacity`, `metrics` (`storage` and `inodes`), and `behavior.scaleUp` with `Percent` and `GiB` policies, `selectPolicy`, and `cooldownDuration` (default `30m`, at least `1m`). The schema and CEL rules reject an invalid spec on admission.
- A claim is left alone when it is not bound, its StorageClass does not allow expansion, its previous resize has not finished, or it has no metrics. After a resize it also waits, as `AwaitingFilesystemResize`, until kubelet reports a larger filesystem than the one recorded in `status.claims[].capacityAtLastScale`, so a stale measurement never grows it twice. Each step is clamped to `maxCapacity`. Two autoscalers targeting the same claim both stop with `AmbiguousSelector`.
- StorageClass pre-check. A missing or non-expandable class on a target claim or on a StatefulSet `volumeClaimTemplate` (the default class when the template names none) turns `AbleToScale` false with reason `VolumeExpansionNotAllowed` and records one Warning Event, before the first claim exists.
- A claim with the Argo CD `argocd.argoproj.io/tracking-id` annotation shows its Application in `status.claims[].trackedBy`, and each resize records a `GitOpsDriftExpected` Warning Event naming the `ignoreDifferences` entry the Application needs to accept the larger size.
- `kubectl get pvca` and `kubectl get iva` print `TARGET`, `STATUS`, `VOLUMES`, `UTILIZATION`, `MAXSIZE`, `LASTSCALE`, and `AGE`. `status.claims` lists each claim's capacity, desired capacity, utilization, cooldown, and reason.
- `--controllers` flag (env `CONTROLLERS`) choosing which of `instance-volume-autoscaler`, `persistent-volume-claim-autoscaler`, and `unused-volume-scan` run. All three by default.
- `metrics` config block (`url`, `tenantId`, `labelSelector`) shared by the claim autoscaler and the throughput recommender. It serves a cluster-local Prometheus and a central Mimir, where `labelSelector` (for example `cluster: prd`) scopes every query to this cluster.
- `claimReconcileInterval` config key (default `1m`, at least `10s`).
- `crdgen` build binary (`make crd`) that renders both CRDs into the chart from the Rust types. A drift test fails `cargo test` when the chart is stale, and the image ships only the controller binary.
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

- The `crd`, `autoscalers`, and `instances` subcommands. CRDs come from the `crdgen` build binary, and `kubectl get iva` with `status.instances` shows what the other two printed.
- The AWS SDK's legacy HTTP client (hyper 0.14, rustls 0.21, ring). Every client now shares one rustls stack on aws-lc-rs, and the binary is 1.4 MB smaller.
