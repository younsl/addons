# Mention invocation

This guide covers answering `@bot` mentions inside a Slack thread by running a kagent agent and replying in place. It is for operators turning the feature on and for anyone explaining to users how a turn behaves.

Setting `SLACK_APP_TOKEN` opens a Socket Mode connection and lets an engineer talk to the agent by mentioning the bot inside a thread. Everything else about the feature is optional, and leaving the token unset keeps the binary behaving exactly as the alert-only build does. The design is written up in [docs/designs/slack-mention-invocation.md](designs/slack-mention-invocation.md).

One turn works like this:

1. The mention arrives as a Socket Mode envelope, which is acknowledged immediately: Slack redelivers anything not acknowledged within 3 seconds, so the acknowledgement never waits on the agent.
2. A status message goes up in the thread before a concurrency slot is even held, so the asker sees the question was taken.
3. That one message is then rewritten in place every `CHAT_STATUS_INTERVAL`, carrying the elapsed time and the task state the controller reports, and it ends as the answer itself. The thread gets one message per turn rather than one per state change.
4. The alert the thread hangs from is quoted in front of the question, together with the `channel_id` and `thread_ts` the mention arrived on. A question asked under an alert is unanswerable without the alert, so the gateway always sends it rather than leaving the agent to discover it.
5. The thread keeps the A2A `contextId` for `CHAT_SESSION_TTL`, so a follow-up mention continues the same agent session instead of starting cold.

Only thread replies invoke the agent. A mention at channel level is answered with an ephemeral hint rather than a run, which keeps the bot attached to a thread that already has context. Mention traffic has its own concurrency limit and deadline, so a burst of questions cannot starve alert analysis of model concurrency.

Socket Mode is an outbound WebSocket, so no inbound network path, request URL, or signing secret is involved. Sessions live in memory at one replica: a restart drops them and the next mention starts a fresh session.

The kagent controller caps a turn at 3 minutes in the v0.9.x line ([issue](https://github.com/kagent-dev/kagent/issues/2112)), so `CHAT_TIMEOUT` above `180s` has no effect until the deployment moves to a release carrying `KAGENT_A2A_CLIENT_TIMEOUT`.

## Conclusion

Mentions ride an outbound Socket Mode connection, run only inside threads, keep their A2A session per thread, and have their own concurrency and deadline, so questions never compete with alert analysis. Leaving `SLACK_APP_TOKEN` unset keeps the gateway alert-only. Who may ask which agent is covered in [Agent access policy](access-policy.md).
