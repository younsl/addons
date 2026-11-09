# Alertmanager

This guide shows the Alertmanager receiver configuration the gateway expects in each parent mode. It is for whoever owns the Alertmanager config.

In `lookup` mode the existing `slack_configs` stays, a `webhook_configs` entry is added next to it, and the template gains a `footer` carrying the alert fingerprint so the gateway can recognise the message:

```yaml
receivers:
- name: 'infra-alerts'
  slack_configs:
  - api_url: '<existing incoming webhook>'
    channel: '#infra-alerts'
    send_resolved: true
    title: '{{ if eq .Status "firing" }}🚨{{ else }}✅{{ end }} [{{ .Status | toUpper }}] {{ .CommonLabels.alertname }}'
    text: |-
      {{ range .Alerts }}
      *Severity:* {{ .Labels.severity }}
      *Summary:* {{ .Annotations.summary }}
      {{ end }}
    footer: 'alert-id {{ (index .Alerts 0).Fingerprint }}'
  webhook_configs:
  - url: 'http://kagent-gateway.kagent:8080/alert'
    send_resolved: false
    http_config:
      authorization:
        type: Bearer
        credentials_file: /etc/alertmanager/secrets/kagent-gateway/WEBHOOK_BEARER_TOKEN
```

Without the `footer` the gateway has nothing to match on and every analysis is posted at channel level instead of in a thread. The marker may live in the title, text, or fallback instead. The footer is simply the least intrusive place. Both deliveries must resolve to the same channel, so `SLACK_CHANNEL_MAP` has to agree with what `slack_configs` uses.

In `post` mode the receiver carries `webhook_configs` alone. Leaving `slack_configs` in place there posts every alert to Slack twice.

Set `send_resolved: true` on the webhook only together with `ANALYZE_RESOLVED`, otherwise the gateway is woken for notifications it will never analyse.

## Conclusion

In `lookup` mode keep `slack_configs`, add a `webhook_configs` entry, and render the first alert's fingerprint into the template footer, with both deliveries resolving to the same channel. In `post` mode send the webhook alone. Enable `send_resolved` on the webhook only together with `ANALYZE_RESOLVED`.
