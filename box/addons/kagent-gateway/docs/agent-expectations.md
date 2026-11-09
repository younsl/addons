# Agent expectations

This guide describes what the gateway sends to an agent and what it expects back, so an Agent CR can be written to fit. It is for whoever authors the agents behind the gateway.

The gateway is agent-agnostic: it sends the alert plus `ANALYSIS_INSTRUCTIONS` and posts whatever text comes back. The agent should be read-only. The gateway cannot enforce that, so the tools bound to the agent decide what an automatically triggered analysis is able to touch.

Reply text is posted as Slack mrkdwn, so the agent should use `*bold*` rather than markdown headings and stay inside `SLACK_MAX_TEXT`.

A mention gets `CHAT_INSTRUCTIONS` instead, which ask for a direct answer rather than the analysis sections. The same agent can serve both paths: an alert run and a mention turn never share a session, and `CHAT_AGENT` points them at different agents when they should not share a tool list either.

## Reading the rest of a thread

The gateway sends the alert and the thread's identifiers, and stops there. An agent that needs the discussion under the alert reads it itself through a Slack MCP server bound as a tool, which is what keeps the gateway out of guessing how much of a conversation a question needs, and what makes a longer thread, another channel, or a search reachable without a gateway release.

Bind read tools only. `slack_get_thread_replies` and `slack_get_channel_history` answer questions. `slack_post_message`, `slack_reply_to_thread`, and `slack_add_reaction` let an agent speak in the channel as the app, which is a capability an automatically triggered agent has no business holding. The gateway owns everything the bot says.

## Routing to several agents

`KAGENT_AGENT_ROUTING_MAP` splits alert categories across specialised agents, keyed by the same label that already routes the Slack channel:

```
SLACK_CHANNEL_LABEL=slack_channel
KAGENT_AGENT_ROUTING_MAP=infra-alerts=aws-alert-triage-agent,security-alerts=security-alert-triage-agent
```

Every agent still gets the same `ANALYSIS_INSTRUCTIONS`, because those describe the output contract the gateway posts to Slack: the sections, the length, and the mrkdwn. What each agent knows and which tools it may call belongs in its own system message and tool list, which the gateway never sees. Keeping the split there also keeps the instructions free of guidance that only one category needs.

## Conclusion

The gateway is agent-agnostic: it owns the output contract through `ANALYSIS_INSTRUCTIONS` and `CHAT_INSTRUCTIONS`, while each agent owns its knowledge and tools. Keep agents read-only, reply in Slack mrkdwn within `SLACK_MAX_TEXT`, and let an agent read more of a thread through read-only Slack MCP tools rather than through the gateway.
