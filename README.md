# addons

A monorepo of [Observability](https://opentelemetry.io/docs/concepts/observability-primer/) and [Kubernetes](https://github.com/kubernetes/kubernetes) addons where every addon and container image is built in [Rust](https://github.com/rust-lang/rust) [1.99+](https://github.com/rust-lang/rust/releases/tag/1.99.1) except [Backstage](./box/addons/backstage/). It holds [CLI tools](./box/cli/), [Kubernetes addons](./box/addons/), [Helm charts](./box/charts/), runtime images and [docs](./docs/). [Kubernetes operators](https://kubernetes.io/docs/concepts/extend-kubernetes/operator/) are built with [kube.rs](https://github.com/kube-rs/kube).

Everything lives in one repository, like [Google does](https://cacm.acm.org/research/why-google-stores-billions-of-lines-of-code-in-a-single-repository/), so one commit can change code, [Makefiles](https://www.gnu.org/software/make/), [CI workflows](https://github.com/features/actions), and [Helm](https://github.com/helm/helm) charts together. A new tool is just a new folder, which keeps each tool small and focused on one job ([Unix philosophy](https://en.wikipedia.org/wiki/Unix_philosophy)) and lets coding agents see all projects at once.

## Consulting

Every tool here is free and stays free. If your team wants the same results without spending months to get there, I offer paid consulting and contract work, built on the same production experience that shaped this repository.

- **Adoption and support**: deploy, tune, and operate any addon, operator, or chart from this repository in your clusters, with priority fixes and features for your use case.
- **Custom Kubernetes operators and addons**: purpose-built controllers, admission webhooks, and exporters in Rust with kube.rs, shipped as small scratch images with Helm charts.
- **Kubernetes platform engineering**: EKS cluster upgrades, Karpenter, Gateway API and Istio, External Secrets, Argo CD GitOps, and Helm chart design.
- **Observability**: Prometheus and Grafana dashboards, alert rules that page only on real problems, OpenTelemetry pipelines, and incident analysis.
- **CI/CD and supply chain**: multi-arch release pipelines, image vulnerability and SBOM collection, and faster, smaller container builds.

Engagements range from a one-off review to ongoing support, delivered in Korean or English. Email [cysl@kakao.com](mailto:cysl@kakao.com) with a short description of your environment and goal, and I will reply with scope and pricing.

## License

This repository is licensed under the Apache License 2.0. See the [LICENSE](./LICENSE) file for details.

## AI Disclosure

The entire codebase of this project was developed with the assistance of [Claude Opus 5.5](https://www.anthropic.com/claude/opus) (High effort).
