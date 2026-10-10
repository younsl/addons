# addons

A monorepo of [Observability](https://opentelemetry.io/docs/concepts/observability-primer/) and [Kubernetes](https://github.com/kubernetes/kubernetes) operation addons. [Rust](https://github.com/rust-lang/rust) [1.99+](https://github.com/rust-lang/rust/releases/tag/1.99.0) is the primary runtime. Includes CLI [tools](./box/tools/), [kubernetes](./box/kubernetes/) addons, [kubernetes operators](https://kubernetes.io/docs/concepts/extend-kubernetes/operator/), runtime images, and [docs](./docs/).

Everything lives in one repository, like [Google does](https://cacm.acm.org/research/why-google-stores-billions-of-lines-of-code-in-a-single-repository/), so one commit can change code, [Makefiles](https://www.gnu.org/software/make/), [CI workflows](https://github.com/features/actions), and [Helm](https://github.com/helm/helm) charts together. A new tool is just a new folder, which keeps each tool small and focused on one job ([Unix philosophy](https://en.wikipedia.org/wiki/Unix_philosophy)) and lets coding agents see all projects at once.

## License

This repository is licensed under the Apache License 2.0. See the [LICENSE](./LICENSE) file for details.
