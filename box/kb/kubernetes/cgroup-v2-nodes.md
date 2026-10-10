---
description: Run every node on cgroup v2, because the kubelet refuses cgroup v1 by default and newer resource features require v2.
tags: [kubernetes, nodes, cgroup, kubelet, eks, upgrade]
resources: [Node, NodePool, EC2NodeClass]
status: adopted
reviewed: 2026-10-10
---

# Run nodes on cgroup v2

## Rule

Nodes run an OS that uses [cgroup v2](https://kubernetes.io/docs/concepts/architecture/cgroups/) by default, such as [AL2023](https://docs.aws.amazon.com/linux/al2023/ug/cgroupv2.html) or Bottlerocket. Never keep nodes on cgroup v1 with the kubelet `failCgroupV1: false` override except as a short bridge during migration.

- Replace Amazon Linux 2 node groups and Karpenter EC2NodeClasses with AL2023 or Bottlerocket images
- Expect different OOM behavior after the move: cgroup v2 kills the whole container cgroup when it runs out of memory, where v1 killed a single process. Multi-process containers such as web servers with workers notice this first
- Set the kubelet `singleProcessOOMKill: true` only for workloads that depend on the old per-process behavior

## Why

- **The kubelet fails on v1**: `failCgroupV1` has defaulted to `true` since 1.35, so a kubelet on a cgroup v1 node does not start. The override still exists in 1.37 but is a temporary bridge, and cgroup v1 support is scheduled for removal
- **Features need v2**: [in-place Pod resize](https://kubernetes.io/docs/tasks/configure-pod-container/resize-container-resources/), memory QoS, and tiered memory protection work only on cgroup v2
- **Better accounting**: cgroup v2 has a single unified hierarchy with consistent memory and I/O accounting, and pressure stall information for each container

## Exceptions

- EKS Fargate still runs cgroup v1 and is managed by AWS

## Example

Check the cgroup version on a node:

```bash
kubectl debug node/<node> -it --image=busybox -- stat -fc %T /host/sys/fs/cgroup
```

`cgroup2fs` means v2, `tmpfs` means v1.

## References

- [About cgroup v2](https://kubernetes.io/docs/concepts/architecture/cgroups/)
- [Kubernetes v1.37 Sneak Peek](https://kubernetes.io/blog/2026/07/31/kubernetes-v1-37-sneak-peek/)
- [Amazon EKS Kubernetes versions](https://docs.aws.amazon.com/eks/latest/userguide/kubernetes-versions-standard.html)
