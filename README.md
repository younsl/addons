# addons

![Abstract blue seascape painting](https://images.unsplash.com/photo-1528804431125-842f17de657b?q=80&w=1480&auto=format&fit=crop)

Cover image from [Unsplash](https://unsplash.com/), free to use under the [Unsplash License](https://unsplash.com/license).

A monorepo of [Observability](https://opentelemetry.io/docs/concepts/observability-primer/) and [Kubernetes](https://github.com/kubernetes/kubernetes) operation addons. [Rust](https://github.com/rust-lang/rust) [1.98+](https://github.com/rust-lang/rust/releases/tag/1.98.0) is the primary runtime. Includes CLI [tools](./box/tools/), [kubernetes](./box/kubernetes/) addons, [kubernetes operators](https://kubernetes.io/docs/concepts/extend-kubernetes/operator/), runtime images, and [docs](./docs/).

## Background

### Monorepo

This repository follows the single-repository model Google describes in [Why Google Stores Billions of Lines of Code in a Single Repository](https://cacm.acm.org/research/why-google-stores-billions-of-lines-of-code-in-a-single-repository/) (CACM, 2016). The scale here is far smaller, but the trade-offs it identifies apply the same way. One source of truth for every tool, addon, and chart removes cross-repo version skew, because a change and the code depending on it land in the same commit. Shared conventions live in one place as well, so updating [Makefiles](https://www.gnu.org/software/make/), reusable [CI workflows](https://github.com/features/actions), release pipelines, or the [Helm](https://github.com/helm/helm) chart README template across every component is one commit instead of a coordinated sweep across many repositories.

A single checkout also keeps the cost of starting something small close to zero. A new tool is a directory with no repository to create, no CI to bootstrap, and no release pipeline to wire up, which is what keeps the [Unix-philosophy](https://en.wikipedia.org/wiki/Unix_philosophy) split of one component per job practical rather than aspirational, and an existing pattern nearby is easier to find than to rewrite. The same property serves coding agents, which get full cross-project context at once and can trace conventions, reuse patterns, and make atomic changes across several tools without losing the connections between them.

## License

This repository is licensed under the Apache License 2.0. See the [LICENSE](./LICENSE) file for details.
