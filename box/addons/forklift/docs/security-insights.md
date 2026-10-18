# Security Insights

## Overview

This document explains Security Insights: what forklift analyzes, the rules that turn the data into findings, how the score is computed, who sees what, and how to link to a view.

Read this if you operate forklift (DevOps engineer or SRE), review its security posture (security engineer), or want to know what to fix in the packages and images your team pulls (developer).

## Background

forklift already holds the data a security review needs: every package version and image the organisation pulls or publishes, their vulnerability and license scans, the policies on each repository, the accounts and tokens that reach them, and an audit log of who asked for what. Each surface shows its own slice. Nobody sees the whole picture, and nothing says which improvement matters most.

Security Insights reads all of it on a schedule and returns one ranked list of improvements, each with the affected items and what to do about them. Forklift coverage, which measures which GitLab projects build through forklift at all, sits next to it under the same Insights menu, because a project that bypasses forklift skips every control described here.

## How it runs

The leader analyzes when it takes leadership if the interval has passed since the last report, and then every interval. The interval is set on the Settings page (see [Settings](#settings)) and defaults to `FORKLIFT_INSIGHTS_INTERVAL` (`1h`) until somebody saves it. A run reads every versioned artifact row, the scan results, the OCI tags, the accounts, roles, tokens, pending approvals, the last 24 hours of refused requests and the last 7 days of policy blocks, then evaluates the rules in memory. The report is stored in the database, so every replica serves the same one.

Administrators and security reviewers can start a run from the page (Analyze now) or with `POST /api/v1/insights/refresh`. The run is detached from the request, and the page polls until it finishes. A run already in progress, or a request that reaches a non-leader replica, answers 409.

Each run keeps a trend row (score and severity counts) for 90 days. Only the newest run keeps its full report.

Set `FORKLIFT_INSIGHTS_ENABLED=false` to turn the analysis off. The page then reports that it is disabled.

## Checks

The view follows AWS Trusted Advisor. Every rule is a check that is listed whether or not it fired, grouped by category, with the category holding the most urgent check first. Each check has a status:

| Status | Trusted Advisor | When |
|--------|-----------------|------|
| Action recommended | Red | The check found a critical or high problem |
| Investigation recommended | Yellow | The check found a medium, low or info problem |
| Acknowledged | Blue | The check failed and an acknowledgement is in force |
| No problems detected | Green | The check ran and found nothing |
| Not available | Gray | The check could not run, for example coverage before its first scan |

Each check has its own page at `/workspace/insights/<check id>` with what Trusted Advisor shows for a check: the status, severity, category and audience, the result of the latest run, why it matters, the alert criteria, the recommended action, and the flagged items with links to where each is fixed. A passing check shows the same reason, criteria and action, so the page also documents what forklift verified.

The categories are:

| Category | Covers |
|----------|--------|
| Supply chain | What enters builds: packages, images and where they come from |
| Policy | The controls configured on each repository |
| Exposure | Who and what can reach forklift's content |
| Access | Accounts, tokens and authentication |
| Operations | Day-to-day hygiene that keeps the controls effective |

## Rules

| Code | Rule | Severity | Category | For | Fires when |
|------|------|----------|----------|-----|------------|
| SC-01 | `vuln-in-use` | Critical (any critical advisory) or High | Supply chain | Developer, Security | A version with a critical or high advisory was served in the last 30 days |
| SC-02 | `vuln-dormant` | Medium | Supply chain | DevOps, Security | A version with a critical or high advisory has not been served for 30 days |
| SC-03 | `vuln-moderate` | Low | Supply chain | Developer | A version whose worst advisory is medium or low was served in the last 30 days |
| SC-04 | `vuln-widespread` | High (critical advisory) or Medium | Supply chain | Security, Developer | One critical or high advisory affects 3 or more stored versions |
| SC-05 | `scan-gap` | Medium | Supply chain | DevOps, Security | More than 20% of a repository's package versions (at least 10) have no vulnerability scan |
| SC-06 | `copyleft-licenses` | Medium (AGPL, SSPL) or Low | Supply chain | Security, Developer | A stored version is under GPL, AGPL, SSPL, EUPL or OSL |
| SC-07 | `oci-unsigned` | Low | Supply chain | DevOps, Developer | A hosted image tag has no cosign `sha256-<digest>.sig` tag beside it |
| SC-08 | `oci-latest-tag` | Low | Supply chain | Developer | A hosted image is pushed as `latest` |
| SC-09 | `coverage-bypass` | Medium (coverage under 50%) or Low | Supply chain | DevOps, Developer | A GitLab project in scope does not fully build through forklift (last coverage scan) |
| PO-01 | `vuln-policy-off` | Medium | Policy | Security, DevOps | The vulnerability policy is off or audit-only on a scannable repository |
| PO-02 | `age-policy-off` | Medium | Policy | Security, DevOps | A proxy has no release-age cooldown |
| PO-03 | `license-policy-off` | Low | Policy | Security | A repository with license resolution has no license policy |
| PO-04 | `vuln-accepted` | Info | Policy | Security | A repository's vulnerability policy ignores advisory ids |
| EX-01 | `public-repositories` | High (any hosted) or Medium | Exposure | Security, DevOps | A repository allows anonymous downloads |
| EX-02 | `plaintext-upstream` | High | Exposure | DevOps | A proxy fetches from an `http://` upstream |
| AC-01 | `admin-sprawl` | Medium | Access | Security | More than 3 enabled accounts hold admin on `*` |
| AC-02 | `stale-accounts` | Medium | Access | Security, DevOps | An enabled human account with a role has not signed in for 90 days |
| AC-03 | `locked-accounts` | Info | Access | Security | An account is locked after failed sign-ins |
| AC-04 | `tokens-no-expiry` | High (any admin-owned) or Medium | Access | Security | An access token has no expiry |
| AC-05 | `tokens-unscoped-admin` | High | Access | Security, DevOps | An administrator's token carries no scopes |
| AC-06 | `tokens-unused` | Medium | Access | Security, DevOps | An unexpired token has not been used for 90 days |
| AC-07 | `auth-failures` | High (200+ in a day) or Medium | Access | Security, DevOps | A client was refused (401, 403) at least 20 times in 24 hours |
| OP-01 | `retention-off` | Low | Operations | DevOps | A repository without idle retention holds at least 10 artifacts not downloaded for 180 days |
| OP-02 | `policy-blocks` | Low | Operations | Developer | A vulnerability or license policy blocked a path in the last 7 days |
| OP-03 | `approvals-aging` | Low | Operations | Security, Developer | A package approval has been pending for more than 7 days |

The alert criteria column above is condensed. The check page states each rule's full criteria, and the API returns them as `criteria` along with `rationale` (why it matters) and `recommendation`. Each finding lists up to 100 affected items, worst first, and links each one to where it is fixed: the artifact (the Artifacts tab searched for its path), the image, the repository's Settings, Security or Statistics tab, the user, the approval queue, or the project's Coverage page.

## Codes

Every check has a stable code, after Security Hub controls: a category prefix and a number (SC supply chain, PO policy, EX exposure, AC access, OP operations), for example AC-05. A code is never renumbered or reused, so it can be quoted in a ticket, a chat or an alarm. Check pages answer to the code as well as the id: `/workspace/insights/AC-05` and `/workspace/insights/tokens-unscoped-admin` are the same page.

## History

Every run also records each check's status (with the acknowledgements then in force), severity and flagged count, kept for 90 days. A check page shows the last 30 days under Metrics, side by side: the status over time on the left, one band per stretch of runs with the same status, named with the time it began, and the flagged items on the right, with a count axis. Each run is marked on both, and pointing at one shows its time (with the time zone), status and flagged count in a tooltip, with a crosshair shared by the two panels. Dragging across either chart zooms both into the runs it spans, as in Grafana. The range is kept in the URL (`?from=...&to=...`) so a zoomed view can be shared, and Reset zoom returns to the whole window. The charts stay readable at thousands of runs: the time axis names days for a window of two days or more and never overlaps its labels, and past one run per two pixels the status bar groups runs into buckets that take the most urgent status among them. Only administrators, auditors and security reviewers see it, because the stored counts span every repository. The API is `GET /api/v1/insights/checks/{id}/history?days=N` (1 to 90).

## Acknowledging a check

As in Trusted Advisor, a failing check can be acknowledged when its risk is accepted or being handled. Administrators and security reviewers acknowledge it with the Acknowledge button on the check page, which asks for a required reason and a period (7, 30, 90, 180 or 365 days), or with `PUT /api/v1/insights/checks/{id}/acknowledgement` and `{"note": "...", "days": 30}`. An acknowledged check shows a blue Acknowledged status with who, when, until when and why, and is left out of the score, the severity counts, the sidebar badge and the alarms.

An acknowledgement covers the state it was given for. It lapses, and the check counts again, when the period ends or when the check flags more items than it did when acknowledged. The page then says which of the two happened. `DELETE` on the same path removes it. Acknowledging and removing are audited as `insights.acknowledge` and `insights.unacknowledge`. Everyone who can see a check sees its acknowledgement.

## Settings

Administrators and security reviewers edit the settings from Settings on the Security tab (`/workspace/insights/settings`), next to the Coverage tab's own Settings. Each tab keeps its own schedule and receiver, and the receivers themselves are managed in Notifications. The leader reads the settings every minute, so a save takes effect within a minute.

| Setting | Default | Effect |
| --- | --- | --- |
| Analyze on a schedule | on (off when `FORKLIFT_INSIGHTS_INTERVAL=0`) | Runs the analysis whenever the interval has passed since the last report |
| Interval | `FORKLIFT_INSIGHTS_INTERVAL` | Minutes between analyses, from 5 to 1440 |
| Send the alarm on a schedule | off | Sends the latest analysis to the receiver on the cron schedule |
| Receiver | none | The notification receiver the scheduled alarm goes to |
| Cron and timezone | `0 10 * * 1-5`, `UTC` | When the scheduled alarm goes out, read in the timezone |
| Minimum severity | high | The alarm goes out only while an open (not acknowledged) finding is at least this severe |
| Skip when nothing is new | off | Holds the alarm back unless a check is newly flagged or flags more items than at the last scheduled alarm |

The API is `GET` and `PUT /api/v1/insights/settings`. Every save is audited as `insights.settings.update`.

## Alarms

The alarm goes out on the schedule set in [Settings](#settings), or at once with Send now under Scheduled alarm on the same page, which sends to the receiver selected there and reports whether it was delivered. Its Preview shows the exact message, rendered as Slack or Mattermost shows it by default and as the unrendered mrkdwn source on the Raw tab, which has a copy icon. The message opens with a short description in words, without figures, written for a reader who has never used Forklift: what Forklift is, what the alarm is and the state it reports (critical risks to fix first, high risks worth fixing soon, only lower findings for regular planning, or every check passed), then carries the score, the findings by severity and up to 20 findings, each linked to its check page (links need `FORKLIFT_EXTERNAL_URL`). Slack receives the message as rich text too, a `blocks` field whose findings are a native numbered list, while `text` keeps the mrkdwn for Mattermost. The webhook body adds the figures as fields (`event: insights.report`, `score`, `grade`, the severity counts and the finding ids) for consumers other than Slack or Mattermost. The API is `GET /api/v1/insights/notification/preview` and `POST /api/v1/insights/notification/send` with `{"receiver_ids": [..]}`. Every send is audited as `insights.notify`.

## Score

The score is shown out of 100 (for example 45 / 100). Acknowledged checks do not count. It starts at 100 and loses 25 per critical finding, 12 per high, 5 per medium and 2 per low, floored at 0. Info findings cost nothing. Grades are A (90 and above), B (75), C (60), D (40) and F. The score counts findings, not affected items: fixing one of ten public repositories leaves the finding in place, fixing all ten removes it.

## Who sees what

Every signed-in user can open Insights. Administrators, auditors and security reviewers see the whole report with its 30-day trend. Everyone else sees the findings about repositories they can read, cut down to those repositories and rescored, plus the coverage finding (coverage is readable by everyone). Their page says so: the score is labelled as the score for their repositories, and the figures under it count only their repositories, artifacts, images and the scan coverage of those, and their own access tokens. Account, token and client findings are never shown to them. Starting a run requires admin or the security action.

In the console, a viewer limited to some repositories sees the check list and each check's status, dimmed and not clickable: the check pages (`/workspace/insights/<check>`) are for administrators, auditors and security reviewers, and a direct link shows that instead of the details. The Settings button and page are for administrators and security reviewers.

## Links

Every view on the page is a URL, so the address bar is always the link to share. The list takes `category`, `severity` and `status` (`action`, `investigate`, `ok`, `unavailable`) as query parameters, for example `/workspace/insights?category=access&status=action`. A check's page is `/workspace/insights/<check id>`, for example `/workspace/insights/vuln-in-use`. The link icon on a check's page copies its address.

The same holds elsewhere in the console. The Artifacts, Approvals and Coverage tables keep their search and page in `q` and `page`, the Approvals queue its `status` and `repo`, and the Audit tab its `event` filter and `page`.

## Conclusion

Security Insights turns what forklift already knows into a short, ranked list of things to fix, each pointing at where it is fixed. Start with the critical and high findings, filter by your role, and share the finding link with the team that owns the fix.
