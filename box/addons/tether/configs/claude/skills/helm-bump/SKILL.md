---
name: helm-bump
description: Bump Helm chart versions (Chart.yaml appVersion/version, dependency versions, values.yaml image tags) after verifying the target version exists.
when_to_use: Updating, upgrading, or bumping chart or app versions, e.g. "차트 버전 업그레이드", "업스트림 최신 버전으로 범프", "이미지 태그 변경". Not for chart authoring or new values keys (use helm-chart), post-release mirror and consumer chart bumps of a first-party release (use release-promote), Rust toolchain re-releases (use rust-bump-rerelease), or commit, push, and MR (use git-ship).
argument-hint: "[chart] [version]"
license: Apache-2.0
compatibility: helm and crane, or aws CLI for ECR
metadata:
  version: "1.3.0"
  category: action
  related: helm-chart release-promote git-ship
allowed-tools: Bash(helm *) Bash(crane *) Bash(aws ecr *) Bash(git diff *) Bash(git log *) Bash(git status *) Read Edit Glob Grep
user-invocable: true
disable-model-invocation: false
---

# Helm Chart Version Bump

## Constraints

- **Wrapper chart pattern**: if the chart is a wrapper chart (minimal templates, upstream chart as dependency, values-only override), bump `dependencies[].version`, sync `appVersion` to the upstream chart's `appVersion`, and handle the wrapper `version` the way the repo's past bumps did: mirror the upstream chart version if it already tracks it, leave it as is if it stayed fixed across earlier bumps, else minor; never vendor or modify upstream templates
- **Verify version exists before editing**: query the registry first; if the version is not found, abort with no edits and show the latest available version
- **Edit in place only**: change only what the bump requires; no comment edits, reorder, reformat, or unrelated cleanup
- **Chart.lock**: if tracked, regenerate it with `helm dependency update` in the same change; leave `charts/*.tgz` out
- **Multi-environment overrides**: if environment-specific value files or directories exist, preserve their overrides; never collapse them to a single default; "전환경" means every environment directory, and each one with its own `Chart.yaml` gets the same bump
- **Upstream values sync**: wrapper values are the full upstream copy (helm-chart); diff upstream `helm show values` between the old and new chart versions and carry every hunk over, placing new keys at their upstream position with comments verbatim, then re-apply local overrides; never rewrite upstream helm-docs comments
- **Image repositories in new keys**: when the repo rewrites images to a registry proxy (repo AGENTS.md), new upstream image keys get the same rewrite even if the component is disabled; tags stay upstream
- **Pinned image tags follow the dependency**: an explicit image tag override equal to the old `appVersion` moves to the new `appVersion` in the same change, after verifying that tag exists in the registry the values point at; otherwise the dependency bump renders the old image
- **Image-only bump**: change `appVersion` and the explicit image tag override; leave chart `version` unchanged unless the chart itself changed
- **Unset with null**: removing an upstream default needs an explicit `null`; deleting the key does nothing under map merge

## Commit Message

Draft only: commit, push, and MR go through git-ship. The subject follows the repo's `git log` convention (younsl/addons: `[<component>] chore(deps): bump <chart> to <new version>`). Body:

```
* Bump <component> from <old version> to <new version>
* Source: <upstream release URL or registry>
```

## Version Verification Reference

| Registry Type | Verify Specific Version | List Available Versions |
|---------------|------------------------|------------------------|
| Helm repo | `helm show chart <chart> --repo <url> --version <ver>` | `helm show chart <chart> --repo <url>` (latest) |
| OCI | `helm show chart oci://<reg>/<chart> --version <ver>` | `crane ls <reg>/<chart>` |
| GHCR | `helm show chart oci://ghcr.io/<owner>/<chart> --version <ver>` | `crane ls ghcr.io/<owner>/<chart>` |
| ECR | `helm show chart oci://<acct>.dkr.ecr.<region>.amazonaws.com/<chart> --version <ver>` | `aws ecr describe-images --repository-name <chart>` |
| Container image | `crane digest <img>:<tag>` | `crane ls <img>` |

## Validation

- Wrapper chart: `helm show values <chart> --version <new>` diffed against the dependency block leaves only intended overrides
- Comment parity: every comment line in the new upstream values also appears in the local values
- `helm template` renders the new chart label and the new image tag, with no leftover old `appVersion` tag
- Bumped version matches a real, published version in the upstream registry
- `Chart.lock`, when tracked, lists the new dependency version
- `git diff` shows only version-related line changes: no unrelated modifications
