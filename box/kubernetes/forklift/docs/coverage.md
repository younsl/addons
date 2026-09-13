# Coverage

## Overview

This document explains forklift coverage: what it measures, how the scan
decides whether a project is wired, and how to operate the scheduled report.

Read this if you are rolling forklift out across an organisation and need to
know how far that rollout has actually got, or if you own the report that names
the teams still to migrate.

## Background

Standing forklift up is the easy half of a migration. The hard half is getting
every project to build through it, and that is invisible from inside forklift:
the instance sees the projects that already use it, and nothing at all of the
ones that still resolve straight from the public registries. The number an
operator actually wants, "how many of our repositories go through forklift", can
only be answered from the source side.

Coverage answers it by walking a GitLab instance and reading each project's own
configuration. A project counts as wired when two things hold:

- its CI pipeline references forklift, either by host or through the forklift
  access token a job authenticates with, and
- a package-manager or image-build file pins the registry it resolves from.

Both, and the project is **applied**. One of the two, and it is **partial**,
which is the interesting state: those builds go through forklift on some paths
and around it on others. Neither, and it is **not applied**, which is the work
still to do.

A project with no GitLab CI file at all was never an integration candidate, so
it is counted separately as **no CI** and kept out of the denominator. Coverage
is applied divided by the projects that have CI and are in scope.

## What the scan reads

For each project the scanner lists branches, walks the tree of each one it
checks, and reads only files matching two patterns:

| Class | Files |
|-------|-------|
| CI | `.gitlab-ci.yml`, `.gitlab-ci.<suffix>.yml`, `ci/*.yml`, `.gitlab/ci/*.yml` |
| Registry | `.npmrc`, `.yarnrc`, `package.json`, `pnpm-workspace.yaml`, `settings.xml`, `pom.xml`, Gradle build/settings/properties files, `pip.conf`, `pip.ini`, `requirements*.txt`, `pyproject.toml`, `poetry.toml`, `Dockerfile*` |

A file counts as evidence when it contains the configured forklift host or
matches `FORKLIFT_[A-Z0-9_]*TOKEN`, which is how a job that authenticates
without naming the host inline is still recognised. `node_modules`, `vendor`,
`dist` and generated build output are skipped: a committed dependency tree
mentions a registry the project never chose. A README that merely mentions
forklift is not evidence either, for the same reason.

The default branch is always checked. Other branches are checked most recently
committed first, capped at `max_branches` and limited to those active within
`since_days`, so a project that wired forklift on a feature branch is reported
as wired with a "not default" flag rather than as missing. The first branch with
evidence settles the verdict.

The repository format shown against a project comes from the first path segment
after the host, so `https://forklift.example.com/npm/npmjs/` reads as `npm`.
Where nothing names a host, the format is guessed from the filenames that
matched.

## Configuration

Only the switch and the GitLab connection are deployment configuration:

| Variable | Chart value | Description |
|----------|-------------|-------------|
| `FORKLIFT_COVERAGE_ENABLED` | `coverageScanning.gitlab.enabled` | Turn coverage scanning on. Off by default. |
| `FORKLIFT_COVERAGE_GITLAB_URL` | `coverageScanning.gitlab.url` | GitLab base URL. Required when enabled. |
| `FORKLIFT_COVERAGE_GITLAB_TOKEN` | `coverageScanning.gitlab.token` / `coverageScanning.gitlab.existingSecret` | Access token with `read_api`. Required when enabled. |

The switch is separate from the credentials deliberately. A GitLab token is a
common thing to already have in a deployment's environment, and forklift gaining
the ability to crawl that instance is not a reason to start doing it. All three
are required before a single request goes out, and the chart refuses to render
with `enabled: true` and no URL or token rather than installing a feature that
would silently do nothing.

```yaml
coverageScanning:
  gitlab:
    enabled: true
    url: https://gitlab.example.com
    existingSecret: forklift-coverage
    existingSecretKey: coverage-gitlab-token
```

Turning the switch off leaves everything else intact: the stored settings, the
last result and the history all survive, and the console reports the feature as
turned off rather than as unconfigured.

The token is deliberately kept in the environment rather than the database: the
metadata store is snapshotted to object storage in s3 mode, and a credential
that never enters it never enters a backup either. It is also never returned by
the settings API.

Everything else is an operational decision and lives in the console on
**Workspace > Coverage > Settings**, stored in the database and editable at runtime:

| Setting | Default | Description |
|---------|---------|-------------|
| Forklift host | host of `FORKLIFT_EXTERNAL_URL` | The external domain a project must reference to count |
| Excluded topics | `forklift.excluded` | GitLab topics a repository can carry to opt itself out |
| Cron / timezone | `0 10 * * 1-5`, `UTC` | When the scheduled scan runs, in wall-clock time |
| Receiver | (none) | The notification receiver the report is sent to |
| Skip at full coverage | off | Drop the scheduled report when nothing needs action |
| Branches per project, branch age limit | 10, 180 days | How far past the default branch the scan looks |
| Use blob search | off | One search call per project instead of a tree walk |

**The forklift host** defaults to the host of `FORKLIFT_EXTERNAL_URL`, so a
normal deployment leaves it empty: the server already knows what it is called.
It stays editable because the name builds resolve through is not always the one
the console is served on. Clearing the field hands it back to the default.

A build reaches forklift at an external host domain, which is the only kind of
name that can appear in a repository's registry configuration, so that is what
the field accepts. A single label, an in-cluster name (`.svc`, `.internal`,
`.local`) or an IP literal is refused at save time rather than stored as a
pattern that silently matches nothing.

The console checks the value as it is typed, in two stages. The shape is decided
locally and blocks saving. DNS is resolved by the server and does not: forklift
looks names up from inside the cluster while the builds it measures look them up
from wherever they run, so a name that does not resolve here can be perfectly
correct for them. A failed lookup is shown as a warning, never as a refusal.

**Which projects are in scope** is decided by the access token, not by a group
name in a form. A group access token sees that group and its subgroups and
nothing else, which is the same narrowing, enforced by GitLab rather than by
forklift asking politely. Narrow the token to narrow the scan.

**The request rate** is discovered rather than configured; see below.

### Checking the connection

The GitLab connection is not on the settings page at all. The URL and the token
are one decision and both come from the environment, so there is nothing there to
edit: splitting them across the chart and the database would let a console edit
point the scan at an instance the deployment's token was never meant for.

It is checked automatically by calling `/api/v4/version` on the configured
instance. That endpoint requires authentication, so one answer separates the
three states that matter: the URL is unreachable, the URL answers but the token
was rejected, or both are good and the GitLab version comes back.

The result is surfaced on the dashboard, and only when it is failing. There is
nothing to edit on a form, and before the first scan a broken connection is the
only thing that can explain an empty page: the last-scan error needs a scan to
have failed first.

The token is never part of this. It stays in the environment, and the check
sends it in a `PRIVATE-TOKEN` header without following redirects, preventing the token from being forwarded
to a different host.

Saving is blocked until every required field is filled. A host left empty counts
as filled when the deployment supplies one to fall back on, which is why the
settings response reports that default separately from the effective value.

## Who can do what

The measurement is what the whole organisation is working towards, so every
signed-in user can read it: the dashboard, the per-group breakdown, the trend,
and any project's verdict and evidence.

Running a scan and changing what is measured are administrator-only: the manual
scan, the settings, muting a project, the report preview and send, and the
pipeline viewer.

## Taking a project out of scope

Two independent sources do this, so each owner can act without waiting on the
other:

- an **exclude topic** on the GitLab project, owned by the repository, and
- **muting**, toggled on the project page, owned by whoever runs the console.

They stay distinct because they answer different questions. A topic is a
repository saying "this is not a candidate", and it travels with the repository.
Muting is an operator saying "not this one, not now", and it is undone in the
same place it was done.

Muting keeps the project's verdict on the record rather than discarding it, so
the coverage number moves immediately and the toggle is reversible without a
rescan. The project page says plainly that a muted project is not being counted,
and why.

Muting is per check, not only per project. The two halves of the wiring, **CI**
and **registry**, are muted independently on the project page, and a muted check
is neither required for the verdict nor credited to it. That is the difference
between "this project is not our business", which is both checks muted and the
project out of the measurement entirely, and "this project has no packages to
pin", which is the registry check muted and the project still counted, applied
on its CI alone. A project with one check muted is never partial: with one
requirement left there is no half-way.

The GitLab topic stays all-or-nothing. A repository opting itself out is a
statement about the whole repository; splitting it per check would put the
console's judgement in the repository's hands.

## The report

After each scheduled scan, the projects still to migrate are posted to the
notification receiver named in the settings, the same receivers the approval
alarms use. One receiver, not a list: a coverage report is a single weekly
message to whoever owns the migration, and fanning it out is what receivers are
for on the receiver side. The message leads with the coverage percentage, then the breakdown,
then the list of projects, worst first: nothing wired before half wired. The
list is capped at 50 entries with a link to the console for the rest.

The report has its own switch, separate from the schedule: turning it off stops
the post and leaves the scan running, because the number is read on the console
whether or not anybody is told about it. The title links back to the coverage
page, which is where every question the message raises is answered.

Turning on **skip at full coverage** drops the automatic post when every target
project is applied, since a report nobody has to act on is noise. A manual send
from the settings page ignores it.

Only the automatic scans report: the scheduled one, and the first scan after a
cold start. **Scan now** on the coverage page does not, because asking for the
current number is not asking to tell the receiver about it. Sending is its own
action: **Send now** on the settings page, which posts what the last completed
scan measured, to the receiver named there.

## Operations

The scan is leader-gated: it writes the result, the history row and the
exclusion state, and SQLite has one writer. In an HA deployment only the elected
leader scans.

A completed scan is persisted, so a restart shows the previous picture rather
than an empty page, and a rolling deploy does not re-crawl GitLab once per pod.
Only a forklift with no stored result at all scans on startup, 30 seconds after
it takes leadership.

### Request rate

The crawl has no rate setting, because nobody knows the right number. How much
a GitLab instance can absorb depends on its size, on what else is hitting it at
that moment, and on which endpoint is being called. A figure entered once in a
form is wrong in both directions over a day, and guessing high degrades somebody
else's GitLab.

So the limit is discovered instead, using the AIMD loop
[Vector](https://vector.dev/docs/reference/configuration/sinks/http/#adaptive_concurrency)
uses for adaptive request concurrency:

- start at two in-flight requests and add one for every request that succeeds
  while the limit is what is holding the scan back,
- cut the limit multiplicatively the moment the instance pushes back.

Push-back is read from two signals. The obvious one is an explicit 429 or a 5xx,
and a `Retry-After` is honoured as given. The other is round-trip time rising
clear of its own recent average, which is what a loaded service does before it
starts refusing outright; reacting to that keeps the scan from being the request
that tips it over.

A latency level that persists is folded into the average and becomes the new
normal, so the scan recovers its throughput against an instance that is simply
slower rather than throttling itself to one request forever. An instance that is
genuinely overloaded goes on to return 429 or 5xx, and that path cuts the limit
regardless of timing.

Back-pressure applies to every worker, not only the one that hit it: per-request
backoff alone leaves the others firing at the same rate and the instance never
gets back under its limit. Nothing here is tunable; the concurrency the scan
settled on is logged when it completes:

```
coverage: scan complete target=56 applied=36 percent=64 concurrency=14 peak_concurrency=22 duration=3m12s
```

A project the scan could not read becomes an `error` verdict with the failing
stage in its note, rather than failing the whole run. Those projects are counted
in the target but are not applied, so a persistent error shows up as coverage
that will not close.

### Trend retention

A coverage reading is kept for 14 days, and the chart shows exactly that window.
The trend is read for two things, whether the rollout moved this sprint and
whether it moved backwards, and neither is asked of a year-old reading; keeping
one only crowds the axis. The store prunes to the window on every scan, the API
refuses a wider one rather than returning less than asked, and the chart states
the window under itself so a short line is not mistaken for a scan that stopped
running.

## Metrics

The scan exports its own state so the thing that actually goes wrong is
alertable. A scan that stops running looks healthy from outside: the console
keeps showing the last result. Alert on the age of
`forklift_coverage_last_scan_timestamp_seconds`, not on the coverage number.

The verdict counts (`forklift_coverage_projects` by `state`), the percentage,
the scan outcome counter and the concurrency the crawl settled on are all
exported; see [Metrics](metrics.md).

## Conclusion

Coverage measures the half of a forklift rollout that forklift cannot see by
itself: which projects still build around it. Point it at GitLab, set the host
and the group, and let the scheduled report name the work that is left. Start by
reading the partial column, which is where builds resolve through forklift and
around it at the same time.
