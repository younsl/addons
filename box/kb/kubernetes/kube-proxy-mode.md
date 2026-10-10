---
description: Set the kube-proxy mode explicitly to iptables or nftables and move off ipvs, because ipvs is deprecated and the upstream default changes to nftables.
tags: [kubernetes, networking, kube-proxy, nftables, eks, upgrade]
resources: [DaemonSet, ConfigMap]
status: adopted
reviewed: 2026-10-10
---

# Set the kube-proxy mode explicitly

## Rule

Every cluster sets [kube-proxy](https://kubernetes.io/docs/reference/networking/virtual-ips/) `mode` explicitly in its configuration, to `iptables` or `nftables`, never `ipvs` and never left to the default.

- On EKS, set it through the kube-proxy [add-on configuration values](https://docs.aws.amazon.com/eks/latest/userguide/managing-kube-proxy.html) so an add-on update does not reset it
- Move clusters running `ipvs` to `iptables` or `nftables` before the removal release, during a maintenance window
- Switch to [nftables](https://kubernetes.io/docs/reference/networking/virtual-ips/#proxy-mode-nftables) only when every node runs Linux 5.13 or later and the CNI supports it. On EKS, the [Amazon VPC CNI](https://docs.aws.amazon.com/eks/latest/best-practices/nftables.html) supports it from v1.23.0
- Test anything that reaches a NodePort through `localhost` before switching, since nftables mode does not serve it by default

## Why

- **Default change**: [KEP-5343](https://www.kubernetes.dev/resources/keps/5343/) makes nftables the default backend. From 1.37, kube-proxy warns through logs and events when the mode defaults to iptables, and the default flips to nftables in 1.40. An explicit mode makes that upgrade a no-op
- **ipvs deprecation**: [KEP-5495](https://www.kubernetes.dev/resources/keps/5495/) deprecates ipvs mode. It logs a warning from 1.37, is disabled by default in 1.40, and is removed in 1.43. ipvs cannot implement Services fully on its own and still relies on iptables underneath
- **Disruptive switch**: changing the mode rewrites every Service rule on the node. Doing it deliberately in a window beats having an upgrade do it

## Exceptions

- Clusters where the CNI replaces kube-proxy, such as [Cilium kube-proxy replacement](https://docs.cilium.io/en/stable/network/kubernetes/kubeproxy-free/), run no kube-proxy and need no mode

## Example

Check the current mode (EKS stores it in the kube-proxy-config ConfigMap):

```bash
kubectl -n kube-system get configmap kube-proxy-config -o jsonpath='{.data.config}' | grep 'mode:'
```

Inspect what the EKS add-on accepts before setting it:

```bash
aws eks describe-addon-configuration --addon-name kube-proxy --addon-version <version> \
  --query configurationSchema --output text | jq '.properties.mode'
```

## References

- [Kubernetes v1.37 Sneak Peek](https://kubernetes.io/blog/2026/07/31/kubernetes-v1-37-sneak-peek/)
- [Kubernetes 1.37: Deep dive into new alpha features](https://palark.com/blog/kubernetes-1-37-release-features/)
- [EKS Best Practices: Running kube-proxy in nftables mode](https://docs.aws.amazon.com/eks/latest/best-practices/nftables.html)
