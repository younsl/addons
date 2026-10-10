---
description: Detect attacks inside running workloads with Falco on both syscall and Kubernetes audit sources, routing alerts by priority and automating response only for high-confidence rules.
tags: [security, falco, runtime, ebpf, eks, audit]
resources: [DaemonSet, Deployment]
status: adopted
reviewed: 2026-10-10
---

# Detect runtime threats with Falco

## Rule

Clusters run [Falco](https://falco.org/) as two releases, one per event source, and treat a silent sensor as a detection gap.

- **Syscalls**: a DaemonSet on every node, tolerating every taint, with the [modern eBPF driver](https://falco.org/docs/concepts/event-sources/kernel/#modern-ebpf-probe). Managed nodes do not allow custom kernel modules
- **Kubernetes API**: a single-replica Deployment with the [k8saudit-eks plugin](https://github.com/falcosecurity/plugins/tree/main/plugins/k8saudit-eks) reading [EKS audit logs](https://docs.aws.amazon.com/eks/latest/userguide/control-plane-logs.html). Syscalls cannot see `pods/exec`, Secret deletion, or privileged pod creation
- Enable JSON output with container and Kubernetes metadata, so every alert names namespace, pod, image, and command line
- Alert on kernel event drops from [Falco metrics](https://falco.org/docs/concepts/metrics/). Drops during a burst blind the sensor exactly when an attack is noisy
- Scope rules to application namespaces and exclude platform namespaces with macros and [exceptions](https://falco.org/docs/concepts/rules/exceptions/)
- Allowlist benign activity observed in your own stack, such as agent wrappers and probes, before enabling any automated action
- Deliver rules through GitOps and keep a detection test matrix in Git

### Response tiers

Route by priority through [Falcosidekick](https://github.com/falcosecurity/falcosidekick) and automate response with [Falco Talon](https://github.com/falco-talon/falco-talon):

- **WARNING**, observe: interactive shell in an application container (`proc.tty != 0`). Label the pod, no notification
- **ERROR**, investigate: writes under `/etc`, a package manager running in a container. Notify the team channel
- **CRITICAL**, contain: `/etc/shadow` read, [instance metadata](https://docs.aws.amazon.com/AWSEC2/latest/UserGuide/instancedata-data-retrieval.html) access, outbound connection from a shell. Notify and terminate the pod after a short grace period

Talon actions match specific rule names, never every CRITICAL alert.

## Why

Image scanning, CIS benchmarks, and admission policies prevent known-bad workloads from starting. None of them report an attacker who is already executing inside a running container through an RCE or SSRF. Without runtime detection, a shell spawned in a container that never runs shells goes unnoticed.

- **Two sources**: syscall rules catch process, file, and network behavior on the node. Audit rules catch API abuse that never touches the node, such as `kubectl exec` from a stolen credential
- **Tiers**: unscoped rules produce alert fatigue, and teams mute the channel. Terminating on every shell kills the pod an engineer is debugging during an incident, which is why shells are labeled, not killed
- **Testing**: a deployed Helm release says nothing about whether rules fire or alerts arrive. Relay token and channel errors silence alerts without any Falco error

## Exceptions

- Debug namespaces and break-glass workflows where interactive shells are expected get their own exception, not a lower priority for everyone
- Nodes that cannot load eBPF fall back to the kernel module driver only where the node image allows it

## Example

Verify end to end from a throwaway pod after each rollout:

```bash
kubectl run falco-test --image=busybox --restart=Never -it --rm -- sh -c \
  'cat /etc/shadow; wget -qO- -T 2 http://169.254.169.254/latest/meta-data/'
```

Expect a CRITICAL alert naming the pod, a notification, and the Talon action.

## References

- [Falco rules](https://falco.org/docs/concepts/rules/)
- [Kubernetes Runtime Security: The Silence That Should Keep You Up at Night](https://blog.saintmalik.me/kubernetes-runtime-security/)
