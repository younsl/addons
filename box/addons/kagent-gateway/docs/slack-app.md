# Slack app

This guide lists what the Slack app needs for the gateway: tokens, bot scopes, Socket Mode, and channel membership. It is for whoever owns the Slack app configuration.

The gateway needs a bot token (`xoxb-...`). Mention invocation adds an app-level token (`xapp-...`), which is created under Basic Information with the `connections:write` scope and is not an OAuth scope. A signing secret and a request URL are never used: Slack never calls the gateway.

Bot token scopes:

| Scope | Needed when |
|-------|-------------|
| `chat:write` | Always. Also covers `chat.update` for the live status message and `chat.postEphemeral` for the mention hints. |
| `channels:history` | `lookup` mode, public channels, and reading the alert a mention was asked under. |
| `channels:read` | `lookup` mode, when channels are configured by name. Configuring conversation IDs instead skips the name lookup and this scope. Public channels are resolved with this scope alone. Private channels are only listed when the name is not public. Mention invocation needs it too when `CHAT_CHANNELS` or `CHAT_AGENT_MAP` name channels rather than IDs. |
| `groups:read` | Any private channel configured by name: `lookup` mode, the alert reactions, and `CHAT_CHANNELS` or `CHAT_AGENT_MAP` entries. Names are tried against public channels first, so without this scope a private name fails with `missing_scope` and never matches. Configuring the conversation ID (`G...` or `C...`) instead skips the name lookup and this scope. |
| `groups:history` | `lookup` mode in private channels, and reading the alert a mention was asked under in a private channel. |
| `reactions:write` | The investigating/completed reactions on the alert notification, and the `:eyes:` a mention carries while it is being answered. The alert reactions can be turned off by setting `SLACK_INVESTIGATING_REACTION` and `SLACK_COMPLETED_REACTION` to empty. The mention one is fixed, so enabling mentions requires this scope. |
| `app_mentions:read` | Mention invocation. The only scope the feature adds. |

Working in a private channel requires `groups:read`. The `channels:*` scopes cover public channels only, so a private channel named in the config fails to resolve without it.

Mention invocation also needs Socket Mode switched on and the `app_mention` event subscribed, neither of which is a permission. Adding the scope means reinstalling the app. The existing bot token survives a reinstall, so the alert path keeps working through it.

The handle people type is the bot user's display name, which the binary does not control. Reaching a mention that reads `@kagent` means renaming the app under Basic Information and the bot display name under App Home. The rename is retroactive: Slack renders every message the app already posted under the current name.

The bot must be a member of every channel it reads or posts in.

## Conclusion

The alert path needs a bot token with `chat:write` plus the history and read scopes `lookup` mode uses, and mentions add an app-level token, `app_mentions:read`, Socket Mode, and the `app_mention` event. Configuring conversation IDs instead of names removes the name lookup scopes, and the bot must be a member of every channel it touches.
