---
description: Set CPU requests but omit CPU limits so bursty workloads are not throttled by CFS quota into latency spikes and OOMKills.
tags: [kubernetes, resources, cpu, cfs, throttling]
resources: [Pod, Deployment, StatefulSet, LimitRange, ResourceQuota]
status: adopted
reviewed: 2026-10-10
---

# Omit CPU limits

## Rule

Containers set [`requests.cpu`](https://kubernetes.io/docs/concepts/configuration/manage-resources-containers/#requests-and-limits) and `limits.memory`, and leave `limits.cpu` unset.

- Size `requests.cpu` from baseline usage under normal load, not the minimum that boots the app. History recorded under a CPU limit is distorted by throttling, so do not size from it
- Keep `limits.memory` equal to `requests.memory`. Memory is not compressible, and running out kills the container
- Pin the runtime CPU count before removing a limit: `GOMAXPROCS` for [Go](https://pkg.go.dev/runtime#hdr-Environment_Variables), `ActiveProcessorCount` for the JVM, `DOTNET_PROCESSOR_COUNT` for .NET. Without a limit, runtimes size thread pools to every core on the node
- Reserve node CPU for system daemons with kubelet [`system-reserved` and `kube-reserved`](https://kubernetes.io/docs/tasks/administer-cluster/reserve-compute-resources/), so unbounded pods cannot starve them
- Check that no [LimitRange](https://kubernetes.io/docs/concepts/policy/limit-range/) default or [ResourceQuota](https://kubernetes.io/docs/concepts/policy/resource-quotas/) on `limits.cpu` injects or requires a CPU limit in the namespace
- Watch throttling, not average CPU: `container_cpu_cfs_throttled_periods_total` over `container_cpu_cfs_periods_total` from [cAdvisor](https://github.com/google/cadvisor/blob/master/docs/storage/prometheus.md)

## Why

A CPU limit is enforced by the Linux [CFS bandwidth controller](https://docs.kernel.org/scheduler/sched-bwc.html) as a quota per 100 ms period. A 500m limit grants 50 ms of CPU time per period across all threads. Four busy threads spend it in about 12 ms, and the container then stalls for the rest of the period even when the node is idle.

- **Latency**: the stall lands on whatever request is in flight. Average CPU stays under the limit, so dashboards look healthy while p50 and p99 rise several times over
- **OOMKills**: a throttled consumer drains its backlog slower than work arrives, a throttled garbage collector falls behind allocation, and a throttled API holds more requests in flight. Memory climbs until the memory limit kills the pod, and the root cause looks like a memory problem
- **Cost**: limits push teams to oversize requests to stay clear of throttling. Without limits, requests can be sized to real baseline usage

Requests alone still protect neighbors. The request sets the container's CFS weight, so under contention each container gets CPU in proportion to its request, and idle CPU is shared instead of wasted.

### QoS impact

Without a CPU limit, a pod moves from [Guaranteed to Burstable](https://kubernetes.io/docs/concepts/workloads/pods/pod-qos/#burstable). With memory request equal to memory limit, it is not evicted for memory before its request is exceeded, so eviction exposure barely changes.

## Exceptions

- Pods that need exclusive cores: the kubelet [static CPU manager policy](https://kubernetes.io/docs/tasks/administer-cluster/cpu-management-policies/#static-policy-configuration) pins cores only for Guaranteed pods with integer CPU requests equal to limits
- Multi-tenant platforms that bill per usage or must cap one tenant's share
- Benchmarks that need a reproducible hard ceiling
- Platforms that enforce requests equal to limits, such as GKE Autopilot
- JVM style startup bursts are better handled by staggered rollouts or a higher request than by a limit

## Example

```yaml
resources:
  requests:
    cpu: 250m
    memory: 512Mi
  limits:
    memory: 512Mi
env:
  - name: GOMAXPROCS
    value: "2"
```

Throttled ratio per container:

```promql
sum by (namespace, pod, container) (rate(container_cpu_cfs_throttled_periods_total[5m]))
/
sum by (namespace, pod, container) (rate(container_cpu_cfs_periods_total[5m]))
```

## References

- [Resource Management for Pods and Containers](https://kubernetes.io/docs/concepts/configuration/manage-resources-containers/)
- [Kubernetes CPU limits make your apps (very) slow and costly](https://github.com/inevolin/k8s-cpu-limits-analyzed)
