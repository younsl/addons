---
description: Balance gRPC traffic per request with an L7 proxy, an Istio waypoint in ambient mode, because Service load balancing pins every request on a connection to one pod.
tags: [kubernetes, networking, grpc, istio, load-balancing]
resources: [Service, Gateway, DestinationRule]
status: adopted
reviewed: 2026-10-10
---

# Balance gRPC per request with an L7 proxy

## Rule

[gRPC](https://grpc.io/) Services that receive calls from long-lived clients are balanced per request by an L7 proxy, not per connection by the [Service](https://kubernetes.io/docs/concepts/services-networking/service/) data plane.

- In [Istio ambient mode](https://istio.io/latest/docs/ambient/overview/), attach a [waypoint](https://istio.io/latest/docs/ambient/usage/waypoint/) to the server's Service or namespace, for example with the [istio-waypoints](../../../box/kubernetes/charts/istio-waypoints) chart. [ztunnel](https://istio.io/latest/docs/ambient/architecture/data-plane/) alone is L4 and behaves like a plain Service for a single connection
- Set the waypoint's load balancing to `LEAST_REQUEST` in a [DestinationRule](https://istio.io/latest/docs/reference/config/networking/destination-rule/#LoadBalancerSettings-SimpleLB) so slow endpoints receive fewer requests
- Add [outlier detection](https://istio.io/latest/docs/reference/config/networking/destination-rule/#OutlierDetection) to eject endpoints that return errors
- Judge the result by server-side p99 per pod. An even request count across pods is not enough when one pod is slow

## Why

[kube-proxy](https://kubernetes.io/docs/reference/networking/virtual-ips/) and most CNIs balance at L4: a backend pod is chosen once, when the TCP connection opens. gRPC runs on [HTTP/2](https://www.rfc-editor.org/rfc/rfc9113#section-5), which multiplexes every call over one long-lived connection, so all requests from a client go to the same pod for the connection's lifetime.

- **Hot pods**: new replicas from a scale-out get no traffic from existing clients until they reconnect
- **Tail latency**: a client pinned to a degraded pod sends it every request. In one benchmark with one slow pod out of five, clients that landed on it saw every call slowed to its delay, with no errors to alert on
- **More connections do not fix it**: opening several connections per client lowers the median but still pins a share of traffic to the slow pod, and the tail gets worse

An L7 proxy terminates HTTP/2 and picks an endpoint for each call. Request-aware algorithms then react to slow endpoints. Envoy's [least request](https://www.envoyproxy.io/docs/envoy/latest/intro/arch_overview/upstream/load_balancing/load_balancers#weighted-least-request) policy, used by Istio waypoints, sidecars, and Cilium L7, reduces load on a slow pod but does not drain it completely under severe delay.

## Exceptions

- Clients that do their own balancing with the gRPC [round_robin policy](https://grpc.io/blog/grpc-load-balancing/) against a [headless Service](https://kubernetes.io/docs/concepts/services-networking/service/#headless-services) need no proxy, at the cost of per-language client configuration and DNS refresh tuning
- Short-lived clients that open one connection per call are already spread by L4 balancing
- Servers that set a low `MaxConnectionAge` force clients to reconnect and rebalance periodically, which mitigates scale-out imbalance but not a degraded pod

## Example

```yaml
apiVersion: networking.istio.io/v1
kind: DestinationRule
metadata:
  name: my-grpc-svc
spec:
  host: my-grpc-svc.my-ns.svc.cluster.local
  trafficPolicy:
    loadBalancer:
      simple: LEAST_REQUEST
    outlierDetection:
      consecutive5xxErrors: 5
      interval: 10s
      baseEjectionTime: 30s
```

Route the Service through its namespace waypoint:

```bash
kubectl label service my-grpc-svc -n my-ns istio.io/use-waypoint=waypoint
```

## References

- [Configure waypoint proxies](https://istio.io/latest/docs/ambient/usage/waypoint/)
- [gRPC Load Balancing](https://grpc.io/blog/grpc-load-balancing/)
- [Benchmarking gRPC Load Balancing on Kubernetes: Linkerd vs Istio vs Cilium](https://buoyant.io/blog/benchmarking-grpc-load-balancing-on-kubernetes-linkerd-vs-istio-vs-cilium), a vendor benchmark by the Linkerd maintainers
