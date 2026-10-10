---
description: Set trafficDistribution PreferSameZone on Services to keep in-cluster traffic in the client's zone and cut cross-zone cost.
tags: [kubernetes, networking, service, topology, cost]
resources: [Service]
status: adopted
reviewed: 2026-10-10
---

# Set trafficDistribution to PreferSameZone

## Rule

Services set [`spec.trafficDistribution: PreferSameZone`](https://kubernetes.io/docs/concepts/services-networking/service/#traffic-distribution) so that in-cluster traffic stays in the client's zone whenever a ready endpoint exists there.

- Spread backends across every zone with [`topologySpreadConstraints`](https://kubernetes.io/docs/concepts/scheduling-eviction/topology-spread-constraints/), otherwise one zone falls back to cross-zone and the benefit is lost
- Keep replicas at or above the zone count, including the [HPA](https://kubernetes.io/docs/concepts/workloads/autoscaling/horizontal-pod-autoscale/) `minReplicas`
- Watch per-zone load: there is no capacity balancing, so a zone with many clients and few endpoints gets overloaded instead of spilling over
- Use `PreferSameZone`, not `PreferClose`, which is its deprecated older alias. Check what the cluster accepts with `kubectl explain service.spec.trafficDistribution`

## Why

- **Cost**: cross-zone traffic is billed in both directions on most clouds (on AWS, [inter-AZ data transfer](https://aws.amazon.com/ec2/pricing/on-demand/#Data_Transfer_within_the_same_AWS_Region) is charged per GB on each side). Chatty east-west traffic between services is often the largest share of that bill.
- **Latency**: same-zone hops are faster and more consistent than cross-zone ones.
- **Blast radius**: a degraded zone affects mostly its own clients instead of every caller in the cluster.
- **Simple**: one Service field, no annotation, and none of the proportional allocation rules of [Topology Aware Routing](https://kubernetes.io/docs/concepts/services-networking/topology-aware-routing/) (`service.kubernetes.io/topology-mode: Auto`) that silently disable hints when a zone has too few endpoints.

### How it behaves

- The [EndpointSlice](https://kubernetes.io/docs/concepts/services-networking/endpoint-slices/) controller adds zone hints to each endpoint
- [kube-proxy](https://kubernetes.io/docs/reference/command-line-tools-reference/kube-proxy/) on a node sends traffic only to endpoints hinted for its own zone
- If the client's zone has no ready endpoint, traffic falls back to all endpoints in the cluster

## Exceptions

- Services whose clients are heavily concentrated in one zone gain little and risk overloading that zone
- [`internalTrafficPolicy: Local`](https://kubernetes.io/docs/reference/networking/virtual-ips/#internal-traffic-policy) and [`externalTrafficPolicy: Local`](https://kubernetes.io/docs/reference/networking/virtual-ips/#external-traffic-policy) take precedence over it
- The data plane must implement it. kube-proxy does, and a CNI that replaces kube-proxy needs its own support checked
- It shapes only Service routing inside the cluster, so load balancers that target pod IPs directly are unaffected

## Example

```yaml
apiVersion: v1
kind: Service
metadata:
  name: my-svc
spec:
  selector:
    app: my-app
  ports:
    - port: 80
      targetPort: 8080
  trafficDistribution: PreferSameZone
```

Spread the backing pods evenly across zones:

```yaml
spec:
  topologySpreadConstraints:
    - maxSkew: 1
      topologyKey: topology.kubernetes.io/zone
      whenUnsatisfiable: ScheduleAnyway
      labelSelector:
        matchLabels:
          app: my-app
```

Confirm hints are set:

```bash
kubectl get endpointslices -l kubernetes.io/service-name=my-svc -o yaml | grep -A2 forZones
```

## References

- [Traffic distribution](https://kubernetes.io/docs/concepts/services-networking/service/#traffic-distribution)
- [Topology Aware Routing](https://kubernetes.io/docs/concepts/services-networking/topology-aware-routing/)
