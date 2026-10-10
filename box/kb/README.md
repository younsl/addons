# Knowledge Base

Personal SRE notes distilled from real work. Each note states one rule, why it holds, and where it stops applying, so a person or a coding agent can apply it without rediscovering the reasoning.

## Notes

### AI

| Note | Rule |
| --- | --- |
| [Keep agent context in versioned files](ai/agent-context-files.md) | Give coding agents conventions through versioned files: AGENTS.md for the repo, skills for procedures, DESIGN.md for UI. Review third-party skills like any dependency. |
| [Verify agent output by running it](ai/verify-agent-output.md) | Humans own requirements and the ship decision. Accept agent work only on evidence from a running system, not on reading the diff. |

### Kubernetes

| Note | Rule |
| --- | --- |
| [Run nodes on cgroup v2](kubernetes/cgroup-v2-nodes.md) | Run every node on cgroup v2. The kubelet refuses cgroup v1 by default and newer resource features require v2. |
| [Set ndots to 2](kubernetes/dns-ndots.md) | Set `dnsConfig` `ndots` to 2 on Pods so external lookups skip the cluster search list. |
| [Migrate workloads to Graviton](kubernetes/graviton-arm64-migration.md) | Publish multi-arch images first, pin arm64 scheduling per environment, and drop to single arm64 builds only after every environment runs on arm64. |
| [Balance gRPC per request with an L7 proxy](kubernetes/grpc-l7-load-balancing.md) | Balance gRPC per request with an Istio waypoint in ambient mode. Service load balancing pins every request on a connection to one Pod. |
| [Use Karpenter interruption handling instead of NTH](kubernetes/karpenter-interruption-handling.md) | Handle Spot and other involuntary EC2 interruptions with Karpenter's EventBridge to SQS interruption queue instead of AWS Node Termination Handler. |
| [Set the kube-proxy mode explicitly](kubernetes/kube-proxy-mode.md) | Set the mode to iptables or nftables and move off ipvs. ipvs is deprecated and the upstream default changes to nftables. |
| [Omit CPU limits](kubernetes/omit-cpu-limits.md) | Set CPU requests but omit CPU limits, so bursty workloads are not throttled by CFS quota into latency spikes and OOMKills. |
| [Prefer VAP and MAP over Kyverno](kubernetes/prefer-vap-map-over-kyverno.md) | Write admission policies as ValidatingAdmissionPolicy and MutatingAdmissionPolicy. Keep Kyverno for generate, cleanup, image verification, and background reports. |
| [Set trafficDistribution to PreferSameZone](kubernetes/service-traffic-distribution.md) | Set `trafficDistribution: PreferSameZone` on Services to keep in-cluster traffic in the client's zone and cut cross-zone cost. |

### Observability

| Note | Rule |
| --- | --- |
| [Alert on symptoms, diagnose with causes](observability/alert-on-symptoms.md) | Page on user-facing symptoms (p99 latency, error ratio) and keep cause metrics like CPU and memory for diagnosis. Causes fire without impact and miss impact without cause. |

### Security

| Note | Rule |
| --- | --- |
| [Detect runtime threats with Falco](security/falco-runtime-detection.md) | Run Falco on both syscall and Kubernetes audit sources, route alerts by priority, and automate response only for high-confidence rules. |

## Note format

Each domain is a directory and each note is one kebab-case file. A note starts with YAML frontmatter (`description`, `tags`, `resources`, `status`, `reviewed`), followed by one H1 and the sections `Rule`, `Why`, `Exceptions` (optional), `Example` (optional), and `References`, in that order.

A note marked `status: superseded` names its replacement in `superseded_by` and is no longer applied.

## Validation

[`scripts/kb-lint.sh`](../../scripts/kb-lint.sh) checks the frontmatter and section order of every note, and [lychee](https://github.com/lycheeverse/lychee) checks every link. Both run as pre-commit hooks on changes under this directory.

Add, rename, or supersede a note together with its row in this file.
