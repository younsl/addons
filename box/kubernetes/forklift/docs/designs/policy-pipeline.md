# Policy evaluation pipeline

## Status

**Implementation status: Implemented.** Verified on 2026-08-16 against `main`
commit `5db514b`. Pipeline configuration and order resolution live in
`src/repoconfig.rs` (`PolicyPipelineConfig`, `EffectiveOrder`),
and the gates in `src/repo/policypipeline.rs` (`policyGates` before the age
break, `finalPolicyGate` after it). Policy templates are exposed through the API
and UI only; they cannot be declared through
[Helm](https://github.com/helm/helm) values or GitOps.

Accepted for incremental implementation. This document describes the first
step toward reusable policy templates without changing the existing repository
configuration or policy semantics.

## Overview

This design defines the ordered pipeline that proxy request policies run in: the
fixed guards, the reorderable automated assessments, the break for age
evaluation, and the final human decision. It replaces policy calls duplicated
across the package-format handlers.

Read this before adding a policy, changing evaluation order, or touching a
format handler's request path.

## Context

Proxy request policies were invoked directly by each package-format handler.
Maven, npm, Cargo, Go, and PyPI repeated approval, vulnerability, and license
calls in a fixed order. Age evaluation happens later in the cache engine or in
format-specific metadata rewriting because it requires upstream publication
data.

This creates four problems:

- Changing policy order requires editing every format handler.
- The UI duplicates an order that is not represented in repository data.
- Policy evaluation is coupled to HTTP response writing and side effects.
- A future template cannot describe an executable pipeline consistently.

## Goals

- Let an administrator order automated supply-chain policies per proxy
  repository while keeping human approval as the final serving decision.
- Preserve existing behavior for repositories that do not contain pipeline
  configuration.
- Use one execution path for every package format.
- Validate the order before it is persisted.
- Leave a versioned configuration boundary that can later reference shared
  policy templates and support multiple policy instances.

## Non-goals

- Shared policy-template persistence and revision management.
- Reordering filters across incompatible execution phases.
- Reordering the human-approval boundary.
- Changing each policy's existing block, warn, audit, fail-open, or fail-closed
  behavior.

## Execution model

The serving path is a phase-constrained filter chain:

1. **Access**: repository state, RBAC, and source IP ACL evaluate request identity
   before package resolution.
2. **Artifact guard**: the explicit blocked-version lookup evaluates an exact
   repository/package/version coordinate before cache or upstream access. It is
   an always-enforced incident-response control, not an approval decision.
3. **Automated assessment**: vulnerability, license, and age filters run in
   repository-configured order. Age needs `published_at`, so reaching it
   transfers execution through the cache/upstream layer before the chain resumes.
4. **Human decision**: package approval is the final serving boundary.

Phase order is invariant. Administrators can reorder vulnerability, license, and
age only within Automated assessment. A filter can be placed only in a phase
that supplies its required context. A blocking result stops all later phases.
The blocked-version list overrides package approval, while rejecting a package
does not create or mutate blocked-version entries.

## Repository configuration

The first schema stores only orchestration data. Existing typed policy sections
remain the source of policy parameters.

```json
{
  "policy_pipeline": {
    "schema_version": 2,
    "order": ["vulnerability", "license", "age"]
  },
  "approval": {"enabled": true, "mode": "enforce"},
  "vuln": {"enabled": true, "threshold": "high", "action": "block"},
  "license": {"enabled": false, "action": "audit"},
  "age_policy": {"enabled": true, "min_age": "7d", "action": "block"}
}
```

Rules:

- Missing or empty `order` resolves to `vulnerability, license, age`.
- Every supported automated-assessment filter must occur exactly once.
- `version_deny` is invalid in v2 because Artifact guard always runs before the
  configurable assessment chain.
- `approval` is an invalid order entry because it is fixed last.
- Unknown and duplicate policy names are rejected.
- `schema_version` is `2`; zero is accepted as an omitted current value.
- Legacy v1 order is accepted on read and normalized by removing
  `version_deny`; the next save persists v2.
- Disabled policies stay in the order and are no-ops. This keeps layout stable
  when a policy is toggled.

## Backend design

`Manager.policyGates` always runs the blocked-version Artifact guard first, then
runs configured assessment filters before age. Artifact handlers pass a
request-bound `finalGate` to the cache engine; after age evaluation, it resumes
assessment filters that follow age and then runs approval. The same split is used
for cached, newly fetched, and pass-through content. npm packuments and PyPI
simple indexes resume the chain after their age rewrite.

The internal policy evaluator in `src/repo/policypipeline.rs` evaluates a
request against the configured stages and returns the response for a blocked
request. Allowed requests continue through the repository handler.

Blocked versions and approval are not members of the assessment registry.
Moving age earlier can increase upstream traffic because later local filters are
evaluated only after publication metadata is available.

## UI design

The Security view presents the policy workbench first and a read-only flow
preview below it. Configuration controls remain together in the first viewport,
while administrators can scroll down to verify the compiled execution order.

Policy settings use a master-detail layout. A compact vertical list groups every
filter under Access, Artifact guard, Automated assessment, or Human decision.
Selecting a row replaces the adjacent detail inspector without navigating or
scrolling to a second copy of the policy. Below the desktop breakpoint the list
and selected inspector stack in that order. ACL, blocked versions, and package
approval are selectable locked rows; package approval remains the final row.

Only vulnerability, license, and age are sortable. Each automated row separates
its interactions: the label selects the inspector, the switch changes enabled
state, and a dedicated handle starts sorting. This avoids nested draggable form
controls and keeps pointer, touch, and keyboard behavior unambiguous. Disabled
policies remain visible and reorderable, while their selected detail inputs are
disabled and visually recede. ACL settings and the blocked-version incident list
are edited in the same inspector used by the assessment filters.

The interaction uses `@dnd-kit/core` and `@dnd-kit/sortable`: a pointer sensor
starts after a small movement threshold, and a keyboard sensor uses sortable
coordinates from the focused handle. Only the handle sets `touch-action: none`,
so the surrounding list remains naturally scrollable on touch devices. Dragging
updates local edit state; Save changes persists `policy_pipeline.order` together
with policy settings.

The flow separates four semantic regions: the fixed access boundary, the fixed
artifact guard, the sortable automated-assessment sequence, and the fixed final
human-approval boundary.
Machine policies use their own 1-based sequence; fixed boundaries use lock/final
labels instead of sharing that sequence. The preview lays out the entire chain
horizontally when space permits and vertically on narrow containers, keeping all
nodes and connectors inside the visible graph area.

### Reference patterns

- Linear's 2026 interface refresh keeps task-critical content prominent while
  making supporting structure recede; it also softens borders and reduces
  unnecessary icon treatment: https://linear.app/now/behind-the-latest-design-refresh
- Linear's workflow configuration allows statuses to be reordered inside fixed
  status categories, matching Forklift's movable machine policies between fixed
  boundaries: https://linear.app/docs/configuring-workflows
- GitLab pipeline graphs group jobs by stage and advance to the next stage only
  after the prior stage succeeds: https://docs.gitlab.com/ci/pipelines/
- GitLab's pipeline editor keeps editing and visualization as distinct modes;
  Forklift adopts the same separation of responsibilities while placing its
  compact deterministic preview below the settings workbench:
  https://docs.gitlab.com/ci/pipeline_editor/
- GitHub workflow graphs visualize job dependencies, while protected
  environments can hold a job in a waiting state until required reviewers
  approve it: https://docs.github.com/en/actions/how-tos/monitor-workflows and
  https://docs.github.com/en/actions/reference/workflows-and-actions/deployments-and-environments

The Forklift preview adapts these conventions as four stage groups: Access,
Artifact guard, Automated assessment, and Human gate. It does not display runtime success
states because this screen edits configuration rather than showing one request
execution. Policy action colors describe configured behavior only.

The effective-flow nodes follow the Linear-inspired hierarchy: neutral surfaces
and soft borders define the structure, a narrow status rail and dot carry the
configured action color, and compact metadata recedes above the policy name.
Fixed boundaries use a lock marker while machine policies use a numbered marker.

Policy activation lives in the ordered list instead of being duplicated in
every detail form. Long operational notes use reduced text emphasis, while the
save action remains available in a restrained sticky footer.

The Security view separates editing from visualization:

1. **Policy settings** contains the sortable policy list and selected detail
   inspector at the top of the supply-chain section.
2. **Flow preview** is read-only and follows the settings workbench. It focuses
   on policy evaluation:
   fixed access, artifact-guard, and human-approval boundaries are shown with
   the effective automated-assessment order between them. Generic request/result terminals are
   omitted because they add no policy decision information.

Hosted and group repositories reuse the same preview surface without showing
inapplicable proxy filters. Hosted shows its access boundary. Group shows the
group access boundary, ordered member lookup, and the selected member's controls;
the member remains the owner of any proxy assessment chain it executes.

The order editor uses `dnd-kit`; the execution preview uses `@xyflow/react`
([React](https://github.com/facebook/react) Flow) with custom read-only nodes
and directed dashed edges. Graph editing, connection creation, selection,
panning, and zooming are disabled so the preview behaves as status visualization
rather than a second editor. The preview observes its own content width instead
of the browser viewport. It is laid out horizontally when at least 960px is
available and vertically below that threshold, and it refits whenever its
container is resized. The graph uses a stable 240px horizontal viewport or
1080px vertical viewport so nodes retain legible dimensions instead of being
compressed to fit a shallow preview. Dagre or ELK is intentionally omitted: the
effective graph is a single deterministic chain, so an automatic layout engine
would add bundle size and unpredictable spacing without improving the result.

The two surfaces intentionally use different backgrounds. Sortable editor rows
use the standard secondary panel surface because they are controls. In the
execution preview, fixed ACL and approval boundaries use that same secondary
surface, while machine-policy nodes use the more distinct tertiary panel
surface. A narrow action rail and status dot provide state without tinting the
entire node or its text.

Dashed connectors with arrowheads join every node in the read-only flow. They run
horizontally on wide containers and vertically after the responsive breakpoint.
Keeping controls out of this preview gives connectors and policy labels enough
room to remain legible.

The order is saved with the repository's existing full-config update. No new
API endpoint or database migration is required.

## Compatibility and rollout

- Existing JSON parses with the legacy default order.
- Existing create and update endpoints continue to use `repoconfig.Config` and
  therefore validate pipeline configuration before persistence.
- Existing policy parameter fields and API response shapes remain available.
- Existing HTTP status codes do not change. Enforced approval still returns 403.
- Upstream content can be cached before final approval; approval controls serving,
  not acquisition.

After this version is stable, reusable templates can be added as immutable
versioned pipeline documents with repository bindings. A compiler can resolve a
template plus repository overrides into the same effective order consumed by
the runner.

## Verification

Tests must cover:

- Legacy default and JSON round-trip behavior.
- Rejection of unknown, duplicate, missing, and unsupported-version orders.
- Machine policies executing in configured order and stopping at the first
  blocking result.
- Policies configured after age executing before final approval.
- Dragging each movable policy to a new position while fixed nodes remain fixed.
- Keyboard sorting from the focused policy card producing the same order as
  pointer or touch dragging.
- Age blocking before final approval without creating a pending approval row.
- All five package formats using the common runner.
- TypeScript compilation and production UI build.
