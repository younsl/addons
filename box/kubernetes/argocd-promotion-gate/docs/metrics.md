# Metrics

## Overview

Every metric the gate exposes, every value their labels can take, and the queries worth keeping. Also which metric answers which question, because the obvious one does not count what its name suggests.

For whoever builds the dashboard or the alerts. [docs/configuration.md](configuration.md) covers the settings these metrics report on.

The admin listener serves `/metrics` on port 8080, alongside `/healthz`, `/readyz`, and the UI extension API. The webhook port serves admission and nothing else, so a scrape never touches it.

The registry is the gate's own rather than the client library default, so the exposed series are limited to what this binary owns plus the standard Go runtime and process collectors.

```yaml
serviceMonitor:
  enabled: true
  interval: 30s
```

## The metric set

| Metric | Type | Labels | Incremented |
| --- | --- | --- | --- |
| `argocd_promotion_gate_decisions_total` | counter | `env`, `code`, `allowed` | once per verdict, from either the webhook or the panel API |
| `argocd_promotion_gate_admission_requests_total` | counter | `outcome` | once per AdmissionReview the API server sends |
| `argocd_promotion_gate_lookup_failures_total` | counter | `kind` | once per fact the gate could not read |
| `argocd_promotion_gate_events_total` | counter | `reason`, `type` | once per Kubernetes Event handed to the broadcaster |
| `argocd_promotion_gate_admission_duration_seconds` | histogram | `outcome` | once per AdmissionReview, measured before the response is written |
| `argocd_promotion_gate_upstream_lookup_duration_seconds` | histogram | `result` | once per Kubernetes read of the upstream Application |
| `argocd_promotion_gate_desired_images_duration_seconds` | histogram | `result` | once per desired image lookup, cache hits included |
| `argocd_promotion_gate_webhook_certificate_expiry_seconds` | gauge | none | read at scrape time from the loaded certificate |

## What each label carries

`code` is the machine-readable reason, the same value the panel and the JSON API return.

| `code` | Meaning | Usual `allowed` |
| --- | --- | --- |
| `NotGated` | the environment is outside the chain or is its head | `true` |
| `Exempt` | the principal or the Application opted out | `true` |
| `Rollback` | the target revision was already deployed here | `true` |
| `UpstreamMissing` | no upstream Application exists for this identity | always `true`, not configurable |
| `UpstreamOutOfSync` | the upstream exists but is not `Synced` | `false` |
| `UpstreamUnhealthy` | the upstream is `Synced` but not `Healthy` | `false` |
| `ImageTagMismatch` | this sync would deploy a tag the upstream is not running | `false` in `enforce`, `true` in `warn` |
| `LookupFailed` | a fact the verdict needs could not be read | set by `promotionGate.imageTag.onError` |
| `Passed` | every configured check passed | `true` |

`outcome` describes what the webhook did with an admission request.

| `outcome` | Meaning |
| --- | --- |
| `allowed` | the gate evaluated and let the sync through |
| `denied` | the gate evaluated and refused |
| `skipped` | not a sync, an exempt principal, or an automated operation, so no evaluation happened |
| `malformed` | the review could not be decoded. Allowed with a warning, because refusing what cannot be read would take all syncs down |

`kind` names the lookup that failed.

| `kind` | Meaning |
| --- | --- |
| `upstream` | reading the upstream Application from the Kubernetes API |
| `desired_images` | reading `managed-resources` from the Argo CD API |

`result` labels the histograms by how the lookup ended, because the three outcomes have latencies that have nothing to do with each other.

| `result` | Meaning |
| --- | --- |
| `ok` | the fact was read |
| `missing` | the upstream Application does not exist, which the API server answers immediately |
| `error` | the read failed, possibly after burning the whole timeout |

`reason` on `events_total` is the Event reason, which is one of two values rather than a verdict code.

| `reason` | `type` | Meaning |
| --- | --- | --- |
| `PromotionBlocked` | `Warning` | the gate refused the sync |
| `PromotionWarning` | `Normal` | the gate allowed the sync and recorded something about it |

## Latency

The histograms exist to size two settings that are otherwise guesses: `webhook.timeoutSeconds` and `promotionGate.argocd.timeoutSeconds`. A verdict that arrives after the API server gave up is indistinguishable from an outage, and with `failurePolicy: Fail` both block the sync while explaining nothing.

Buckets run from 1ms to 5s. A request slower than the top bucket has already been abandoned, so where it landed after that is not worth a series.

```promql
histogram_quantile(0.99, sum by (le) (
  rate(argocd_promotion_gate_admission_duration_seconds_bucket[30m])
))
```

`desired_images_duration_seconds` is bimodal on purpose. A cache hit inside `argocd.cacheTtlSeconds` never calls argocd-server, so the fast mode is cache hits and the slow mode is real lookups. Read the two modes rather than the average.

```promql
sum(rate(argocd_promotion_gate_admission_duration_seconds_bucket{le="1"}[30m]))
  / sum(rate(argocd_promotion_gate_admission_duration_seconds_count[30m]))
```

## Events

Every blocked or warned verdict is also written onto the Application as a Kubernetes Event. This is the only thing the gate writes to the cluster, and it is not optional. [docs/configuration.md](configuration.md) covers the RBAC it needs.

```bash
kubectl -n argocd describe application prd-payment-api | tail -20
kubectl -n argocd get events --field-selector reason=PromotionBlocked
```

Every refusal carries the reason `PromotionBlocked`, whatever the underlying code was, so one field selector finds all of them. A mismatch that `imageTag.mode: warn` let through is `PromotionWarning`, because it is worth recording but refused nobody. The verdict code stays out of the Event and lives on `decisions_total`, which is the surface built to be aggregated over. An Event is deleted an hour after it is written, so it is context on one Application rather than a history to query.

`events_total` counts submissions rather than writes. Delivery is asynchronous through a queue drained by a background task, and a write the API server refuses is logged and dropped, so the counter can run ahead of the objects that exist.

## The certificate gauge cannot see the failure that matters

`webhook_certificate_expiry_seconds` is the `notAfter` of the pair the process currently has loaded, as a unix timestamp, and zero when none is loaded. The gate polls the pair on disk every 10 seconds and reloads it when either file changes, so it tracks what is actually being served rather than what was on disk at startup.

It only catches a certificate running out. The failure that takes the webhook down in practice is a re-issued CA: cert-manager mints a new one, cainjector publishes it into `caBundle`, and the leaf still being served is valid but no longer chains to it. The gauge reads healthy throughout, because the handshake fails before any request reaches this process. Nothing on this side can see it, so alert on the API server instead.

```promql
sum(rate(apiserver_admission_webhook_rejection_count{
  name="argocd-promotion-gate.younsl.github.io",
  error_type="calling_webhook_error"
}[10m])) > 0
```

## Decisions are not sync attempts

The panel calls the same engine the webhook does, which is what keeps the two from disagreeing, and every one of those calls counts in `decisions_total`. Opening an Application page therefore moves the counter without anybody pressing Sync.

So `decisions_total` measures verdicts, not deploys. For traffic that actually passed through admission, use `admission_requests_total`. Both are useful, as long as which one answers which question stays clear.

## No application label

Application identity is deliberately absent from every label. A denial costs one log line, but one time series per Application would outlive the Application itself.

The logs carry what the labels do not. Every verdict, allowed or denied, logs `app`, `namespace`, `principal`, `outcome`, `reason`, and `duration_ms`, plus `initiated_by` and `revision` when the operation names them. Denials go out at warn level.

```bash
kubectl -n argocd logs deploy/argocd-promotion-gate | jq 'select(.outcome == "denied")'
```

## Queries

What `enforce` would block, which is the number to watch during rollout:

```promql
sum by (env) (increase(argocd_promotion_gate_decisions_total{code="ImageTagMismatch", allowed="true"}[24h]))
```

Denials by environment and reason:

```promql
sum by (env, code) (rate(argocd_promotion_gate_decisions_total{allowed="false"}[1h]))
```

Share of admission requests that were refused:

```promql
sum(rate(argocd_promotion_gate_admission_requests_total{outcome="denied"}[1h]))
  / sum(rate(argocd_promotion_gate_admission_requests_total[1h]))
```

Lookups failing, which is usually a missing or expired Argo CD token rather than a promotion problem:

```promql
sum by (kind) (rate(argocd_promotion_gate_lookup_failures_total[15m])) > 0
```

## Worth alerting on

| Signal | Why it matters |
| --- | --- |
| `lookup_failures_total{kind="desired_images"}` rising | the Argo CD API or its token is broken, and `onError` is deciding every tag check |
| `lookup_failures_total{kind="upstream"}` rising | the gate cannot read Applications, so RBAC or the API server is the problem |
| `admission_requests_total{outcome="malformed"}` above zero | the gate is allowing syncs it could not parse |
| `decisions_total{code="LookupFailed"}` rising with `allowed="false"` | gated syncs are being refused for a reason that is not a promotion failure |
| `admission_duration_seconds` p99 approaching `webhook.timeoutSeconds` | verdicts are about to start timing out, which `failurePolicy: Fail` turns into blocked syncs with no message |
| `webhook_certificate_expiry_seconds` within a month of now | the serving certificate is running out, and only a handful of paths renew it |
| `apiserver_admission_webhook_rejection_count{error_type="calling_webhook_error"}` above zero | the API server cannot reach or verify the webhook, which no gate metric can show |

A quiet `admission_requests_total` is not an alert on its own. An estate that deploys a few times a day looks identical to a broken registration, so pair it with a synthetic sync or watch the webhook's own error rate instead.
