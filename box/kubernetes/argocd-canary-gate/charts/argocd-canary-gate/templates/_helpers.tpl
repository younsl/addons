{{- define "argocd-canary-gate.name" -}}
{{- default .Chart.Name .Values.nameOverride | trunc 63 | trimSuffix "-" -}}
{{- end -}}

{{- define "argocd-canary-gate.fullname" -}}
{{- if .Values.fullnameOverride -}}
{{- .Values.fullnameOverride | trunc 63 | trimSuffix "-" -}}
{{- else -}}
{{- $name := default .Chart.Name .Values.nameOverride -}}
{{- if contains $name .Release.Name -}}
{{- .Release.Name | trunc 63 | trimSuffix "-" -}}
{{- else -}}
{{- printf "%s-%s" .Release.Name $name | trunc 63 | trimSuffix "-" -}}
{{- end -}}
{{- end -}}
{{- end -}}

{{- define "argocd-canary-gate.chart" -}}
{{- printf "%s-%s" .Chart.Name .Chart.Version | replace "+" "_" | trunc 63 | trimSuffix "-" -}}
{{- end -}}

{{- define "argocd-canary-gate.labels" -}}
helm.sh/chart: {{ include "argocd-canary-gate.chart" . }}
{{ include "argocd-canary-gate.selectorLabels" . }}
{{- if .Chart.AppVersion }}
app.kubernetes.io/version: {{ .Chart.AppVersion | quote }}
{{- end }}
app.kubernetes.io/managed-by: {{ .Release.Service }}
{{- end -}}

{{- define "argocd-canary-gate.selectorLabels" -}}
app.kubernetes.io/name: {{ include "argocd-canary-gate.name" . }}
app.kubernetes.io/instance: {{ .Release.Name }}
{{- end -}}

{{- define "argocd-canary-gate.serviceAccountName" -}}
{{- if .Values.serviceAccount.create -}}
{{- default (include "argocd-canary-gate.fullname" .) .Values.serviceAccount.name -}}
{{- else -}}
{{- default "default" .Values.serviceAccount.name -}}
{{- end -}}
{{- end -}}

{{- define "argocd-canary-gate.image" -}}
{{- $registry := .Values.image.registry | trimSuffix "/" -}}
{{- $tag := default .Chart.AppVersion .Values.image.tag -}}
{{- if $registry -}}
{{- printf "%s/%s:%s" $registry .Values.image.repository $tag -}}
{{- else -}}
{{- printf "%s:%s" .Values.image.repository $tag -}}
{{- end -}}
{{- end -}}

{{/*
The gate config file, rendered straight from .Values.canaryGate. Its shape is
the binary's own configuration schema.
*/}}
{{- define "argocd-canary-gate.config" -}}
{{- toYaml .Values.canaryGate -}}
{{- end -}}

{{/*
CEL match conditions. Narrowing here rather than in the handler keeps the API
server from calling this webhook on the constant stream of status writes that
Argo CD makes for every Application.
*/}}
{{- define "argocd-canary-gate.matchConditions" -}}
- name: only-new-sync-operations
  expression: "has(object.operation) && (oldObject == null || !has(oldObject.operation))"
{{- range .Values.canaryGate.exempt.usernames }}
- name: {{ printf "not-%s" (. | replace ":" "-" | replace "." "-" | lower | trunc 55 | trimSuffix "-") | quote }}
  expression: "request.userInfo.username != '{{ . }}'"
{{- end }}
{{- with .Values.webhook.extraMatchConditions }}
{{- toYaml . | nindent 0 }}
{{- end }}
{{- end -}}

{{/*
Serving certificate for the webhook listener.

Memoized on .Values for the duration of one render, because the Secret and the
ValidatingWebhookConfiguration live in separate files and must agree: genSignedCert
is not deterministic, so calling it twice would hand the API server a CA that does
not match the certificate the pod serves.

An existing Secret is reused so `helm upgrade` does not rotate the certificate out
from under a running API server. That reuse depends on `lookup`, which a renderer
without cluster access answers with nothing, so `webhook.certManager.enabled` skips
this path entirely and leaves the pair to cert-manager.
*/}}
{{- define "argocd-canary-gate.certs" -}}
{{- if and (not .Values.webhook.certManager.enabled) (not (hasKey .Values "generatedCerts")) -}}
{{- $fullName := include "argocd-canary-gate.fullname" . -}}
{{- $secretName := printf "%s-tls" $fullName -}}
{{- $existing := lookup "v1" "Secret" .Release.Namespace $secretName -}}
{{- if and $existing $existing.data (index $existing.data "tls.crt") (index $existing.data "tls.key") (index $existing.data "ca.crt") -}}
{{- $_ := set .Values "generatedCerts" (dict
      "cert" (index $existing.data "tls.crt")
      "key" (index $existing.data "tls.key")
      "ca" (index $existing.data "ca.crt")) -}}
{{- else -}}
{{- $days := int .Values.webhook.certValidityDays -}}
{{- $altNames := list
      $fullName
      (printf "%s.%s" $fullName .Release.Namespace)
      (printf "%s.%s.svc" $fullName .Release.Namespace)
      (printf "%s.%s.svc.cluster.local" $fullName .Release.Namespace) -}}
{{- $ca := genCA (printf "%s-ca" $fullName) $days -}}
{{- $signed := genSignedCert $fullName nil $altNames $days $ca -}}
{{- $_ := set .Values "generatedCerts" (dict
      "cert" ($signed.Cert | b64enc)
      "key" ($signed.Key | b64enc)
      "ca" ($ca.Cert | b64enc)) -}}
{{- end -}}
{{- end -}}
{{- end -}}

{{/*
The Prometheus Operator apiVersion, so the ServiceMonitor can be skipped on a
cluster that has no operator installed rather than failing the apply.
*/}}
{{- define "argocd-canary-gate.apiVersions.monitoring" -}}
monitoring.coreos.com/v1
{{- end -}}
