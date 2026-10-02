# Installation

## Overview

This document covers installing forklift on
[Kubernetes](https://github.com/kubernetes/kubernetes) with the
[Helm](https://github.com/helm/helm) chart. It walks through the three storage
layouts the chart supports, the AWS permissions the S3 layout needs, and how to
list the published chart versions.

Read this before the first install, or when moving an existing install onto
different storage. Basic Helm and Kubernetes storage knowledge is enough.

## Background

forklift keeps two kinds of state: artifact bytes in a content-addressed blob
store, and metadata (repositories, users, roles, tokens, the artifact index) in
an embedded [SQLite](https://github.com/sqlite/sqlite) database. SQLite
tolerates exactly one writer, so a high-availability install elects a single
leader through a Kubernetes Lease and only that pod writes.

That constraint is what makes storage a deployment-time decision rather than a
detail. The chart offers one ReadWriteMany volume shared by both pods, one
ReadWriteOnce volume per pod with the standby replicating from the leader, or an
S3 bucket holding blobs directly with periodic metadata snapshots, which by
default is the bundled SeaweedFS. Pick by what
your cluster can provide: RWX storage, block storage only, or object storage.
[Architecture](architecture.md) explains how each behaves during failover.

## Shared RWX volume

```bash
helm install forklift oci://ghcr.io/younsl/charts/forklift \
  --namespace forklift --create-namespace \
  --set seaweedfs.enabled=false \
  --set persistence.storageClass=efs-sc \
  --set auth.bootstrap.adminPassword=change-me
```

For HA (default `replicaCount: 2`), the volume must be ReadWriteMany (EFS, NFS, CephFS). Set `replicaCount: 1` for a single instance with ReadWriteOnce storage.

## PV-based replication (no RWX storage)

On EBS-only clusters, enable PV-based replication; the chart then renders a StatefulSet with one ReadWriteOnce PVC per pod:

```bash
helm install forklift oci://ghcr.io/younsl/charts/forklift \
  --namespace forklift --create-namespace \
  --set seaweedfs.enabled=false \
  --set replication.enabled=true \
  --set persistence.storageClass=gp3 \
  --set auth.bootstrap.adminPassword=change-me
```

## S3 backend (no volume at all)

Set `storage.backend=s3` and a bucket; the chart renders a Deployment with an `emptyDir`. Credentials come from the AWS default chain, so annotate the ServiceAccount for EKS IRSA (or associate an EKS Pod Identity) and keep keys out of the chart:

```bash
helm install forklift oci://ghcr.io/younsl/charts/forklift \
  --namespace forklift --create-namespace \
  --set seaweedfs.enabled=false \
  --set storage.backend=s3 \
  --set storage.s3.bucket=my-forklift-bucket \
  --set storage.s3.region=ap-northeast-2 \
  --set serviceAccount.annotations."eks\.amazonaws\.com/role-arn"=arn:aws:iam::123456789012:role/forklift \
  --set auth.bootstrap.adminPassword=change-me
```

The IAM role needs `s3:GetObject`, `s3:PutObject`, `s3:DeleteObject` and `s3:ListBucket` on the bucket and its prefix. Enabling bucket versioning is recommended for metadata-snapshot point-in-time recovery.

## Supported storage

Verified end to end on kind for chart 0.14.0: boot, the conditional-write probe, artifact upload and download by digest, the Storage page and admin API, failover where HA applies, and `migrate-storage` in both directions.

| Storage | Backend | Verified version | `storage.s3.provider` | HA (`replicaCount > 1`) | Storage page metrics |
|---------|---------|------------------|-----------------------|-------------------------|----------------------|
| Filesystem ([PersistentVolume](https://kubernetes.io/docs/concepts/storage/persistent-volumes/)) | `fs` | Any POSIX volume | n/a | RWX volume or PV replication | Volume capacity |
| [SeaweedFS](https://github.com/seaweedfs/seaweedfs) (bundled default) | `s3` | 4.48 | `seaweedfs` | Yes | Disk capacity, volume servers, logical bytes |
| [MinIO](https://github.com/minio/minio) | `s3` | RELEASE.2025-09-07T16-13-09Z | `minio` | Yes | Drives, servers, objects, buckets |
| [RustFS](https://github.com/rustfs/rustfs) | `s3` | 1.0.0 | `rustfs` | Yes | Drives, servers, objects, buckets |
| [Garage](https://git.deuxfleurs.fr/Deuxfleurs/garage) | `s3` | v2.4.1 | `garage` | No, conditional writes are ignored | Nodes, objects, buckets |

[Amazon S3](https://aws.amazon.com/s3/) (`provider: aws`) remains supported through IRSA or EKS Pod Identity but was not part of this verification round. Any other S3-compatible store runs as `generic`: the startup probe decides whether HA is allowed, and the Storage page shows no cluster metrics.

## Bundled SeaweedFS (default)

The chart bundles [SeaweedFS](https://github.com/seaweedfs/seaweedfs) in all-in-one mode (`seaweedfs.enabled=true`) and wires the s3 backend to it, so a plain `helm install` runs on object storage with no external bucket. Change `seaweedfs.s3.credentials.admin.*` for anything beyond a demo. It replaces the MinIO subchart that earlier charts bundled: MinIO pulled its images from the public quay.io registry (`quay.io/minio/minio`), which broke every install that pulled them. See [IBM's advisory](https://www.ibm.com/support/pages/node/7289585) for the image removal. Disable it with `seaweedfs.enabled=false` for the fs backend, PV replication, or an external store. To move existing data off MinIO, follow [Storage migration](storage-migration.md).

## Self-hosted S3-compatible stores

Set `storage.s3.endpoint`, `storage.s3.forcePathStyle=true`, static keys via `storage.s3.existingSecret` (keys `access-key-id`, `secret-access-key`, optional `admin-token`) and `storage.s3.provider` so the Storage page reads the store's admin API:

| Provider | `provider` | Admin API | Extra settings | HA |
|----------|------------|-----------|----------------|----|
| [MinIO](https://github.com/minio/minio) | `minio` | `/minio/admin/v3/info` on the S3 endpoint, SigV4 | none | yes |
| [RustFS](https://github.com/rustfs/rustfs) | `rustfs` | `/rustfs/admin/v3/info` on the S3 endpoint, SigV4 | none | yes |
| [SeaweedFS](https://github.com/seaweedfs/seaweedfs) | `seaweedfs` | Master `/dir/status`, `/vol/status`, volume server `/status` | `adminEndpoint: http://master:9333` | yes |
| [Garage](https://git.deuxfleurs.fr/Deuxfleurs/garage) | `garage` | `/v2/GetClusterStatus`, `/v2/GetClusterStatistics`, bearer token | `adminEndpoint: http://garage:3903`, `adminToken` (no `.`) | no |
| Any other | `generic` | none | none | if probed |

HA metadata fencing needs S3 conditional writes (`If-Match`, `If-None-Match`). Forklift probes the store at startup: when the store accepts a write whose precondition failed, a single instance logs a warning and HA (`replicaCount > 1`) refuses to start. Garage ignores the headers by design, so run it with `replicaCount: 1`.

## Chart versions

List chart versions with [crane](https://github.com/google/go-containerregistry/blob/main/cmd/crane/README.md):

```bash
crane ls ghcr.io/younsl/charts/forklift
```

## Related documents

- [Architecture](architecture.md) for how each storage layout behaves on failover.
- [Configuration](configuration.md) for the environment variables the chart maps to.
- [Storage migration](storage-migration.md) for moving data between object stores.
- [Access control](access-control.md) for declaring roles and grants in the chart.
