---
plugins:
  - slack-mr-bot-backend
---

# Slack MR Bot

Backend-only plugin that turns a Slack slash command into a review request message. A review request is posted when the author actually wants review, which is rarely the moment the merge request was opened, so this is driven by the person rather than by a repository webhook.

## Features

- `/mr` opens a modal with one URL field per merge request, up to five, then a second step with one editable field per merge request, pre-filled with its title
- `/mr <url> [<url> ...]` skips the URL step and goes straight to the titles
- Posts one line per merge request to the channel the command was run in, ending with a mention of the requester
- Follows each posted request and replies in its thread as reviewers approve and when someone merges, so the thread answers whether anyone has looked at it
- Appends the diff stats GitLab shows on the Changes tab (`+754 -10`) to each title line, regardless of what the author typed
- Provider-agnostic: URLs are routed to a provider adapter by host, so GitHub support is one more adapter rather than a second bot
- Runs in Socket Mode, so Slack never calls into this instance and no ingress or request URL is needed
- Reuses the `integrations.gitlab` hosts and tokens unless `slackMrBot.providers` overrides them

## Flow

1. `/mr` opens the URL modal: five optional single-line fields, so a URL is pasted into its own field rather than typed after a newline. Fields left empty are ignored and duplicates are dropped. A field holding several URLs at once is still split correctly.
2. Submitting replaces the modal with a loading state, looks every merge request up in parallel, and comes back with one multi-line field per merge request holding its title. The label carries the reference and diff stats (`!1706  +640 -4`).
3. Editing a field changes only that line of the message. Extra lines typed under a title are posted verbatim beneath it, which is how a merge request carries a note.
4. Submitting posts the message.

A lookup that fails leaves its field empty with the reason in the hint rather than failing the batch, so the author can type that title by hand. Fields left empty are skipped when posting.

## Review follow-up

Posting is not the end of it. The bot records where the message landed (channel and `ts`) and which merge requests it carried, then polls the provider on a schedule and replies in the thread as the state changes:

```
@jane.doe 님이 !1706 리뷰를 완료했습니다.
@john.doe 님이 !1706 을 머지했습니다.
```

One sentence per event, the reference linked to the merge request. Each approver is announced once per thread; an approval withdrawn and re-given is not repeated, and approvals arriving in the same tick share one reply. Closing without merging says nothing.

Polling rather than a GitLab webhook keeps the plugin's stance that nothing calls into this instance, and needs no per-project hook: the `read_api` token that looked the merge request up also reads `approvedBy` and `mergeUser` in one GraphQL query, with a two-call REST fallback (`/merge_requests/:iid` plus `/approvals`). Basic approvals are read, so every approver is announced on Free and Premium alike.

State lives in two tables on the backend database, `slack_mr_bot_requests` and `slack_mr_bot_approvals`, so a restart or an overdue scheduler run replays nothing, and the same merge request posted in two channels is answered in both threads. A request stops being followed when it is merged or closed, when it is `trackDays` old, after five consecutive failed lookups, or when the thread is gone (message deleted, channel archived).

### Who gets mentioned

An approver is mentioned when a Slack account can be matched, otherwise named with a link to their GitLab profile. GitLab exposes no email to a non-admin token, so the address is guessed as the GitLab username at the requester's own Slack email domain, read from their profile; a public email on the GitLab profile is tried first. Both lookups need the `users:read` and `users:read.email` bot scopes. Without them the bot notes the missing scope once in the log and every approver is a linked name, nothing else changes.

## Message format

```
MR 리뷰 부탁드립니다~
!1706: [vector-agent] prometheus_exporter suppress_timestamp 활성화 (+640 -4)
특정 vector-agent의 timestamp가 밀릴 경우 Prometheus 스크레이핑이 영향받는 업스트림 이슈 (#6725)
!1707: [alloy-operator] PostgreSQL 드라이버 CVE 취약점 패치 (+12 -12)
요청자: @jane.doe
```

The first line comes from `slackMrBot.headerText`, each reference links to its merge request, and the last line mentions the Slack user who ran the command.

## Configuration

```yaml
# app-config.yaml
slackMrBot:
  enabled: true
  # Bot token (xoxb-...). Scopes: commands, chat:write
  botToken: ${SLACK_MR_BOT_TOKEN}
  # App-level token (xapp-...) for Socket Mode. Scope: connections:write
  appToken: ${SLACK_MR_BOT_APP_TOKEN}
  command: /mr
  headerText: 'MR 리뷰 부탁드립니다~'
  # Thread replies on approval and merge. Every key has a default, so the
  # block can be omitted; an existing deployment needs no config change.
  # reviewNotify:
  #   enabled: true
  #   pollIntervalSeconds: 60
  #   trackDays: 14
  # Omit to reuse the `integrations.gitlab` hosts and tokens.
  # providers:
  #   gitlab:
  #     - host: gitlab.example.com
  #       apiBaseUrl: https://gitlab.example.com/api/v4
  #       token: ${GITLAB_TOKEN}
```

The plugin stays disabled when `slackMrBot.botToken` is unset, so an instance without the Slack app configured starts normally.

## Slack app setup

1. Create a Slack app, enable **Socket Mode**, and generate an app-level token with `connections:write`
2. Add bot token scopes `commands` and `chat:write`, then install the app to the workspace. Add `users:read` and `users:read.email` too if approvers should be mentioned rather than named
3. Register the `/mr` slash command. Socket Mode needs no request URL
4. Invite the bot to each channel it should post in (`/invite @<bot>`)

## GitLab permissions

Titles and diff stats come from one GraphQL query (`project.mergeRequest.diffStatsSummary`), since the REST merge request object carries no diff stats. When GraphQL is rejected — an older instance, or a token it does not accept — the plugin falls back to the REST endpoint for the title and posts that line without stats. Approval state is read the same way (`approvedBy`, `mergeUser` over GraphQL, `/approvals` over REST). Either path needs a token with `read_api` and access to the project; GitLab answers `404` rather than `403` for a project the token cannot see, so a missing membership looks like a wrong URL.

## Errors

| Situation | Behavior |
|-----------|----------|
| More than five URLs passed to the command | Ephemeral message naming the count, no modal |
| Every URL field left empty | Inline validation error on the first field |
| Unparseable or unsupported URL | That field's hint carries the reason; the rest still resolve |
| Merge request lookup fails | Same as above, field left empty for manual entry |
| Every field left empty | Inline validation error, nothing posted |
| Channel post fails (bot not invited) | The message is sent to the requester as a DM instead; the follow-up threads under that DM |
| Thread reply fails (rate limit, outage) | Retried next poll; the approver stays unannounced until a reply lands |
| Thread reply fails because the message or channel is gone | The request stops being followed |
| Merge request lookup fails five polls in a row | The request stops being followed, with the last error in the log |
| Slack app lacks `users:read.email` | Logged once; approvers are named with a GitLab profile link instead of mentioned |

Two URLs pasted with no separator are split at each `http(s)://`, and a URL with trailing junk is rejected rather than resolving to the first number found in it.
