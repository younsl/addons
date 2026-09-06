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
2. Add bot token scopes `commands` and `chat:write`, then install the app to the workspace
3. Register the `/mr` slash command. Socket Mode needs no request URL
4. Invite the bot to each channel it should post in (`/invite @<bot>`)

## GitLab permissions

Titles and diff stats come from one GraphQL query (`project.mergeRequest.diffStatsSummary`), since the REST merge request object carries no diff stats. When GraphQL is rejected — an older instance, or a token it does not accept — the plugin falls back to the REST endpoint for the title and posts that line without stats. Either path needs a token with `read_api` and access to the project; GitLab answers `404` rather than `403` for a project the token cannot see, so a missing membership looks like a wrong URL.

## Errors

| Situation | Behavior |
|-----------|----------|
| More than five URLs passed to the command | Ephemeral message naming the count, no modal |
| Every URL field left empty | Inline validation error on the first field |
| Unparseable or unsupported URL | That field's hint carries the reason; the rest still resolve |
| Merge request lookup fails | Same as above, field left empty for manual entry |
| Every field left empty | Inline validation error, nothing posted |
| Channel post fails (bot not invited) | The message is sent to the requester as a DM instead |

Two URLs pasted with no separator are split at each `http(s)://`, and a URL with trailing junk is rejected rather than resolving to the first number found in it.
