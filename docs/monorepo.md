# Monorepo

This repository follows the single-repository model Google describes in [Why Google Stores Billions of Lines of Code in a Single Repository](https://cacm.acm.org/research/why-google-stores-billions-of-lines-of-code-in-a-single-repository/) (CACM, 2016). The scale here is far smaller, but the trade-offs the paper identifies apply the same way, and this document records which ones the repository actually leans on.

## Why

**Unified versioning.** One source of truth for every tool, addon, and chart means there is no cross-repo version skew to reconcile. A change and the code that depends on it land in the same commit, so there is never a window where two repositories disagree about what the current state is.

**Atomic convention changes.** Shared conventions live in one place: Makefiles, reusable CI workflows, release pipelines, the Helm chart README template. Updating a convention across every component is one commit rather than a coordinated sweep across many repositories, each with its own review and merge latency.

**Discoverability over duplication.** All tools and addons sit under one checkout, so an existing pattern is easier to find than to rewrite. A new addon starts by reading its neighbors instead of copying boilerplate from whichever repository was most recently touched.

**Cheap component creation.** A new tool starts as a directory. There is no new repository to create, no CI to bootstrap, no permissions to grant, no release pipeline to wire up. The cost of starting something small stays close to zero, which is what keeps the Unix-philosophy split of one component per job practical rather than aspirational.

**Fits AI-assisted development.** A single checkout gives coding agents full cross-project context at once. They can trace shared conventions, reuse existing patterns, and make atomic changes across several tools without juggling multiple repositories or losing the connections between them.

## Layout

| Path | Contents |
| --- | --- |
| `box/kubernetes/` | Kubernetes addons, operators, and runtime container images |
| `box/kubernetes/charts/` | Helm charts, distributed as OCI artifacts |
| `box/tools/` | CLI tools |
| `docs/` | Repository documentation, articles, and the resume |
| `.github/workflows/` | Shared and per-artifact release pipelines |

Components are mixed-language by design. Go 1.27.0 is the primary runtime, Rust 1.98+ covers the CLI tools and several addons, and one component is neither: Backstage is Node.js and React. The repository holds the conventions, not a single toolchain.

## Release isolation

The main risk of a monorepo is that every push rebuilds everything. Two mechanisms keep that from happening.

Each release workflow declares an explicit `paths:` filter listing the exact files it owns, so a workflow only wakes for changes inside its own components. The `paths:` list at the top of a workflow is the authoritative record of which artifacts that workflow releases.

Releases then trigger on a version value changing inside a file rather than on any change at all. A container releases when the `org.opencontainers.image.version` label in its Dockerfile changes, a chart when `version` in its `Chart.yaml` changes, and the Rust CLI when `version` in its `Cargo.toml` changes. Editing a Dockerfile without touching its version label still starts a workflow run, which then finds the version already published and no-ops. That is the intended way to make a non-releasing change.

The `ij` release workflow creates the GitHub release itself, and the tag it cuts is namespaced per component, as in `ij/x.y.z`, because a flat `vx.y.z` tag space cannot express independent versions for independent components sharing one history.

## Costs

This model is not free, and the costs are worth naming.

Access control is repository-wide. There is no way to grant someone write access to one component without granting it everywhere, so the model suits a repository with a single maintainer or a small trusted set far better than one with many external contributors.

History is shared. Every component's commits interleave in one log, which makes per-component history harder to read and makes commit message discipline load-bearing rather than cosmetic. That is why commits carry a `[<TOOLNAME>]` prefix.

CI correctness depends on the `paths:` filters staying accurate. Adding a component without adding it to the right filter produces a component that silently never releases, and the failure mode is silence rather than an error.
