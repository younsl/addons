---
description: Set dnsConfig ndots to 2 on Pods so external lookups skip the cluster search list.
tags: [kubernetes, dns, coredns, eks]
resources: [Pod]
status: adopted
reviewed: 2026-10-10
---

# Set ndots to 2

## Rule

Pods default to [`ndots:5`](https://man7.org/linux/man-pages/man5/resolv.conf.5.html), which turns most external lookups into a burst of failed queries. Set `ndots:2` on workloads so that external names resolve on the first try while short in-cluster names keep working.

- In-cluster calls use `my-svc`, `my-svc.my-ns`, or the full `my-svc.my-ns.svc.cluster.local`
- Avoid the `my-svc.my-ns.svc` form: it has 2 dots, so it is tried absolute first and fails once before the search list catches it
- External names with a single dot (`example.com`) still walk the search list, so add a trailing dot (`example.com.`) when that path is hot
- Apply cluster-wide with a [MutatingAdmissionPolicy](https://kubernetes.io/docs/reference/access-authn-authz/mutating-admission-policy/) rather than editing every chart, see [prefer-vap-map-over-kyverno.md](prefer-vap-map-over-kyverno.md)

## Why

The default pod `/etc/resolv.conf` looks like this:

```
search <namespace>.svc.cluster.local svc.cluster.local cluster.local <node search domains>
options ndots:5
```

A name with fewer dots than `ndots` is tried against every search domain before it is tried as is. Resolving `api.example.com` (2 dots) under `ndots:5` produces:

- `api.example.com.<namespace>.svc.cluster.local` NXDOMAIN
- `api.example.com.svc.cluster.local` NXDOMAIN
- `api.example.com.cluster.local` NXDOMAIN
- one NXDOMAIN per node search domain (for example the cloud provider's internal zone)
- `api.example.com` finally answers

Each step runs for both A and AAAA records, so one lookup becomes 8 to 10 queries. The cost shows up as:

- higher latency on every external call that is not cached
- [CoreDNS](https://coredns.io/) CPU and cache churn
- upstream resolver rate limits (on AWS, the [VPC resolver](https://docs.aws.amazon.com/vpc/latest/userguide/AmazonDNS-concepts.html) drops packets past a per-ENI packet rate, seen as [`linklocal_allowance_exceeded`](https://docs.aws.amazon.com/AWSEC2/latest/UserGuide/monitoring-network-performance-ena.html))

### Why 2 and not 1

`ndots:2` keeps the in-cluster short forms resolving on the first query:

| Name | Dots | Resolution under `ndots:2` |
|------|------|----------------------------|
| `my-svc` | 0 | search list, first try hits |
| `my-svc.my-ns` | 1 | search list, hits via `svc.cluster.local` |
| `api.example.com` | 2 | absolute first, hits |
| `my-svc.my-ns.svc.cluster.local` | 4 | absolute first, hits |

With `ndots:1`, `my-svc.my-ns` would go to the upstream resolver as an absolute name first, fail, and leak cluster service names outside the cluster before falling back to the search list.

## Exceptions

- Workloads that address in-cluster names with 2 or more dots but no full domain, such as [StatefulSet pods](https://kubernetes.io/docs/concepts/workloads/controllers/statefulset/#stable-network-id) via `web-0.nginx.my-ns`, pay one failed upstream query per lookup. Switch them to the full `svc.cluster.local` name, or leave them on the default
- Workloads that never resolve in-cluster names can skip cluster DNS entirely with [`dnsPolicy: None`](https://kubernetes.io/docs/concepts/services-networking/dns-pod-service/#pod-s-dns-policy) and their own resolver and search list

## Example

[`dnsConfig`](https://kubernetes.io/docs/concepts/services-networking/dns-pod-service/#pod-dns-config) merges with the default `dnsPolicy: ClusterFirst`, so only the option needs to be set.

```yaml
spec:
  dnsConfig:
    options:
      - name: ndots
        value: "2"
```

Verify inside the pod:

```bash
kubectl exec <pod> -- cat /etc/resolv.conf
```

## References

- [DNS for Services and Pods](https://kubernetes.io/docs/concepts/services-networking/dns-pod-service/)
- [EKS Best Practices: Reduce external queries by lowering ndots](https://docs.aws.amazon.com/eks/latest/best-practices/scale-cluster-services.html#_reduce_external_queries_by_lowering_ndots)
