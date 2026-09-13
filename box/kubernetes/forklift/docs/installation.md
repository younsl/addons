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
S3 bucket holding blobs directly with periodic metadata snapshots. Pick by what
your cluster can provide: RWX storage, block storage only, or object storage.
[Architecture](architecture.md) explains how each behaves during failover.

## Shared RWX volume (default)

```bash
helm install forklift oci://ghcr.io/younsl/charts/forklift \
  --namespace forklift --create-namespace \
  --set persistence.storageClass=efs-sc \
  --set auth.bootstrap.adminPassword=change-me
```

For HA (default `replicaCount: 2`), the volume must be ReadWriteMany (EFS, NFS, CephFS). Set `replicaCount: 1` for a single instance with ReadWriteOnce storage.

## PV-based replication (no RWX storage)

On EBS-only clusters, enable PV-based replication; the chart then renders a StatefulSet with one ReadWriteOnce PVC per pod:

```bash
helm install forklift oci://ghcr.io/younsl/charts/forklift \
  --namespace forklift --create-namespace \
  --set replication.enabled=true \
  --set persistence.storageClass=gp3 \
  --set auth.bootstrap.adminPassword=change-me
```

## S3 backend (no volume at all)

Set `storage.backend=s3` and a bucket; the chart renders a Deployment with an `emptyDir`. Credentials come from the AWS default chain, so annotate the ServiceAccount for EKS IRSA (or associate an EKS Pod Identity) and keep keys out of the chart:

```bash
helm install forklift oci://ghcr.io/younsl/charts/forklift \
  --namespace forklift --create-namespace \
  --set storage.backend=s3 \
  --set storage.s3.bucket=my-forklift-bucket \
  --set storage.s3.region=ap-northeast-2 \
  --set serviceAccount.annotations."eks\.amazonaws\.com/role-arn"=arn:aws:iam::123456789012:role/forklift \
  --set auth.bootstrap.adminPassword=change-me
```

The IAM role needs `s3:GetObject`, `s3:PutObject`, `s3:DeleteObject` and `s3:ListBucket` on the bucket and its prefix. Enabling bucket versioning is recommended for metadata-snapshot point-in-time recovery. For an S3-compatible store (e.g. [MinIO](https://github.com/minio/minio)) set `storage.s3.endpoint` and `storage.s3.forcePathStyle=true`, and supply static keys via `storage.s3.existingSecret` (keys `access-key-id`, `secret-access-key`).

## Chart versions

List chart versions with [crane](https://github.com/google/go-containerregistry/blob/main/cmd/crane/README.md):

```bash
crane ls ghcr.io/younsl/charts/forklift
```

## Related documents

- [Architecture](architecture.md) for how each storage layout behaves on failover.
- [Configuration](configuration.md) for the environment variables the chart maps to.
- [Access control](access-control.md) for declaring roles and grants in the chart.
