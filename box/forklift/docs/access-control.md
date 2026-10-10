# Access control

## Overview

This document explains how forklift decides what a caller may do: the action
model, how roles reach users, how to declare roles and grants in the chart, and
what a personal access token can and cannot carry.

Read this when granting a team access, when separating security duties from
repository administration, or when issuing tokens for CI and agents.

## Background

Every request arrives as one of three identities: a session from OIDC login, a
session from a local account, or a personal access token. Authorization is the
same for all three and is role-based. A role carries permissions, each pairing a
set of actions with a repository glob pattern, so access is always scoped to
repositories rather than granted globally.

Two ideas matter before reading further. First, the management-plane actions
(`approve`, `audit`, `security`, `admin`) are deliberately separate from the
data-plane ones (`read`, `write`, `delete`), which is what lets a security team
work without repository administration rights. Second, roles and grants can be
declared in the chart and reconciled on startup, the way
[Argo CD](https://github.com/argoproj/argo-cd) handles its RBAC policy, so
access can live in Git alongside the rest of the deployment.

Authorization is role-based. A role bundles permissions, each granting a set of actions (`read`, `write`, `delete`, `approve`, `audit`, `security`, `admin`) on repositories matching a glob pattern. Roles reach principals two ways: assigned directly to a user, or mapped from a [Keycloak](https://github.com/keycloak/keycloak) group claim. A user with no matching grant has no access unless a default role is configured.

The three security-team actions are deliberately separate. `approve` decides individual packages, `audit` reads the administrative surfaces without changing anything, and `security` edits the policy those decisions are measured against: the Security tab's age, approval, vulnerability, license, IP ACL, notification-receiver and public-visibility settings, through `PUT /api/v1/repositories/{id}/security`. That route accepts only those sections; the upstream URL and its credentials live on the admin-only `PUT /api/v1/repositories/{id}`, so a security engineer can tighten policy without gaining control over where packages come from. Like `approve` and `audit`, the `security` action cannot be carried by a personal access token.

Artifact labels are the one write that no action grants on its own: they may be changed by an administrator on the repository, or by the principal recorded as having uploaded that artifact. `write` deliberately does not carry it, because a label is an assertion about an artifact someone else may have published. Every attempt, refused ones included, is recorded in the repository's audit log.

Roles, grants and group mappings can be managed interactively in the UI/API, or declared once in the [Helm](https://github.com/helm/helm) chart and reconciled on startup (ArgoCD-style). The two coexist: declarative entries are authoritative and read-only in the UI; interactively-created entries are left untouched.

Two switches open reads to callers with no matching grant. `FORKLIFT_ANONYMOUS_READ` allows unauthenticated downloads instance-wide. The per-repository Visibility toggle (Settings tab, `config.public`, Harbor-style) does the same for one repository: anyone — anonymous or authenticated without a grant — may download from a public repository, while writes and deletes always require an authenticated principal with the `write`/`delete` action. New repositories are private by default.

## Declarative RBAC (chart)

`auth.rbac.policy` is an ArgoCD-style policy reconciled on every startup. It is authoritative for the roles, grants and group mappings it defines (managed rows): removing an entry removes it from the database on the next restart, while UI-created rows survive.

```csv
# p, <role>, repo, <action>, <repo-glob>, allow   (action: read|write|delete|approve|audit|security|admin, or '*' = admin)
# g, <subject>, <role>                              (subject: group:<keycloak-group> | user:<name> | bare = user)
p, readonly, repo, read, *, allow
p, developer, repo, read, team-a-*, allow
p, developer, repo, write, team-a-*, allow
g, group:/platform-admins, admins
g, user:alice, developer
```

- `auth.rbac.policyDefault` (default `readonly`) is the role granted to every authenticated user, even with no explicit grant. Set it empty for deny-all.
- `auth.rbac.accounts` provisions local (password) accounts; each password is generated into the chart Secret (key `local-user-<name>-password`) and preserved across upgrades. Grant them roles with `g, user:<name>, <role>` lines. Existing accounts (including the bootstrap admin) are never overwritten.
- Out of the box the chart ships a `readonly` role (read on all repositories), an `admins` role (full access), and `policyDefault: readonly`, so every signed-in user can pull and access is granted from there.

## Personal access tokens

Tokens are scoped per repository pattern and action, and their scopes accept only `read`, `write`, `delete` and `audit`. A token can never approve a package or edit security policy, so a leaked CI token cannot taint those decisions.

## Related documents

- [Configuration](configuration.md) for the `FORKLIFT_RBAC_*` and OIDC environment variables.
- [Security policies](security-policies.md) for what the `approve` and `security` actions govern.
