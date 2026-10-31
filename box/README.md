# box

Everything this repository ships. Each component is one directory, grouped by what it ships as.

```
box/
├── addons/   container images, most with a Helm chart in charts/
├── charts/   standalone Helm charts with no image of their own
└── cli/      command-line tools for a workstation
```

| Directory | Ships as | Published to |
| --- | --- | --- |
| [addons](./addons/) | [scratch](https://hub.docker.com/_/scratch) image with a static Rust binary, plus a chart under `charts/` | `ghcr.io/younsl/{name}`, `ghcr.io/younsl/charts/{name}` |
| [charts](./charts/) | Helm chart as an [OCI artifact](https://helm.sh/docs/topics/registries/) | `ghcr.io/younsl/charts/{chart}` |
| [cli](./cli/) | Native binary | `cli/ij/dist/` for `ij`, `make install` for `vlt` |

## addons

| Name | Purpose |
| --- | --- |
| [argocd-canary-gate](./addons/argocd-canary-gate/) | Blocks an [Argo CD](https://argo-cd.readthedocs.io/en/stable/) sync while an [Argo Rollouts](https://argoproj.github.io/rollouts/) canary is in progress |
| [aurora-database-insights-exporter](./addons/aurora-database-insights-exporter/) | [Prometheus exporter](https://prometheus.io/docs/instrumenting/exporters/) for Aurora [Database Insights](https://docs.aws.amazon.com/AmazonRDS/latest/AuroraUserGuide/USER_DatabaseInsights.html) metrics |
| [aws-health-event-notifier](./addons/aws-health-event-notifier/) | Posts [AWS Health](https://docs.aws.amazon.com/health/latest/ug/health-api.html) events to Slack and records them as Kubernetes Events |
| [external-ebs-autoresizer](./addons/external-ebs-autoresizer/) | [Grows the root filesystem](https://docs.aws.amazon.com/ebs/latest/userguide/recognize-expanded-volume-linux.html) of standalone EC2 instances when disk usage crosses a threshold |
| [filesystem-cleaner](./addons/filesystem-cleaner/) | [Sidecar](https://kubernetes.io/docs/concepts/workloads/pods/sidecar-containers/) or [init container](https://kubernetes.io/docs/concepts/workloads/pods/init-containers/) image that removes files when disk usage crosses a threshold |
| [forklift](./addons/forklift/) | Artifact repository and proxy for Maven, npm, Cargo, Go, PyPI, and [OCI](https://opencontainers.org/) images and charts |
| [harbor](./addons/harbor/) | Rebuilds upstream [Harbor](https://goharbor.io/) images for [AWS Graviton](https://aws.amazon.com/ec2/graviton/) (arm64) |
| [kagent-gateway](./addons/kagent-gateway/) | Sends [Alertmanager](https://prometheus.io/docs/alerting/latest/alertmanager/) alerts to Slack and a [kagent](https://kagent.dev) agent that investigates them |
| [kubernetes-upgrade-operator](./addons/kubernetes-upgrade-operator/) | [Operator](https://kubernetes.io/docs/concepts/extend-kubernetes/operator/) that upgrades [EKS](https://docs.aws.amazon.com/eks/latest/userguide/what-is-eks.html) clusters from `EKSUpgrade` resources |
| [trivy-collector](./addons/trivy-collector/) | Collects [Trivy Operator](https://aquasecurity.github.io/trivy-operator/latest/) reports across clusters and serves a web viewer |

## charts

| Name | Purpose |
| --- | --- |
| [argo-workflows-templates](./charts/argo-workflows-templates/) | [Argo Workflows templates](https://argo-workflows.readthedocs.io/en/latest/workflow-templates/) |
| [deepsecurity](./charts/deepsecurity/) | [Trend Micro Deep Security](https://www.trendmicro.com/en_us/business/products/hybrid-cloud/deep-security.html) Agent on every Linux node |
| [istio-envoyfilters](./charts/istio-envoyfilters/) | Istio [EnvoyFilter](https://istio.io/latest/docs/reference/config/networking/envoy-filter/) resources |
| [istio-waypoints](./charts/istio-waypoints/) | Istio ambient [waypoint proxies](https://istio.io/latest/docs/ambient/usage/waypoint/) |
| [karpenter-nodepool](./charts/karpenter-nodepool/) | [Karpenter](https://karpenter.sh/docs/concepts/nodepools/) NodePool and EC2NodeClass |
| [kubernetes-admission-policies](./charts/kubernetes-admission-policies/) | [ValidatingAdmissionPolicy](https://kubernetes.io/docs/reference/access-authn-authz/validating-admission-policy/) and [MutatingAdmissionPolicy](https://kubernetes.io/docs/reference/access-authn-authz/mutating-admission-policy/) with bindings |

## cli

| Name | Purpose |
| --- | --- |
| [ij](./cli/ij/) | EC2 [Session Manager](https://docs.aws.amazon.com/systems-manager/latest/userguide/session-manager.html) connect and [AMI](https://docs.aws.amazon.com/AWSEC2/latest/UserGuide/AMIs.html) cleanup across regions |
| [vlt](./cli/vlt/) | Local password manager [TUI](https://ratatui.rs/) with an encrypted vault |

## Conventions

- Rust images are cross-compiled with [cargo-zigbuild](https://github.com/rust-cross/cargo-zigbuild) for `linux/amd64` and `linux/arm64`.
- A release runs when a version value changes on `main`: the Dockerfile `org.opencontainers.image.version` label, `version` in `Chart.yaml`, `version` in `Cargo.toml`, or `addons/harbor/VERSION`.
- Charts have no `helm repo` index. List versions with [crane](https://github.com/google/go-containerregistry/blob/main/cmd/crane/README.md): `crane ls ghcr.io/younsl/charts/{chart}`.
- A new component goes where its output belongs: an image in `addons/`, a chart only in `charts/`, a local tool in `cli/`.
