{{/*
Expand the name of the chart.
*/}}
{{- define "trivy-collector.name" -}}
{{- default .Chart.Name .Values.nameOverride | trunc 63 | trimSuffix "-" }}
{{- end }}

{{/*
Create a default fully qualified app name.
*/}}
{{- define "trivy-collector.fullname" -}}
{{- if .Values.fullnameOverride }}
{{- .Values.fullnameOverride | trunc 63 | trimSuffix "-" }}
{{- else }}
{{- $name := default .Chart.Name .Values.nameOverride }}
{{- if contains $name .Release.Name }}
{{- .Release.Name | trunc 63 | trimSuffix "-" }}
{{- else }}
{{- printf "%s-%s" .Release.Name $name | trunc 63 | trimSuffix "-" }}
{{- end }}
{{- end }}
{{- end }}

{{/*
Create chart name and version as used by the chart label.
*/}}
{{- define "trivy-collector.chart" -}}
{{- printf "%s-%s" .Chart.Name .Chart.Version | replace "+" "_" | trunc 63 | trimSuffix "-" }}
{{- end }}

{{/*
Common labels
*/}}
{{- define "trivy-collector.labels" -}}
helm.sh/chart: {{ include "trivy-collector.chart" . }}
{{ include "trivy-collector.selectorLabels" . }}
{{- if .Chart.AppVersion }}
app.kubernetes.io/version: {{ .Chart.AppVersion | quote }}
{{- end }}
app.kubernetes.io/managed-by: {{ .Release.Service }}
{{- end }}

{{/*
Selector labels (chart-wide — DO NOT use on per-role Deployments / Pods;
selectors must differ for server and scraper to avoid one replica set
managing the other's pods).
*/}}
{{- define "trivy-collector.selectorLabels" -}}
app.kubernetes.io/name: {{ include "trivy-collector.name" . }}
app.kubernetes.io/instance: {{ .Release.Name }}
{{- end }}

{{/*
Role-specific names: one pod per responsibility.
*/}}
{{- define "trivy-collector.serverName" -}}
{{- printf "%s-server" (include "trivy-collector.fullname" .) -}}
{{- end }}

{{- define "trivy-collector.scraperName" -}}
{{- printf "%s-scraper" (include "trivy-collector.fullname" .) -}}
{{- end }}

{{- define "trivy-collector.serverSelectorLabels" -}}
{{ include "trivy-collector.selectorLabels" . }}
app.kubernetes.io/component: server
{{- end }}

{{- define "trivy-collector.scraperSelectorLabels" -}}
{{ include "trivy-collector.selectorLabels" . }}
app.kubernetes.io/component: scraper
{{- end }}


{{/*
Name of the Secret holding the shared internal-API token.
*/}}
{{- define "trivy-collector.internalSecretName" -}}
{{- if .Values.internal.existingSecret -}}
{{- .Values.internal.existingSecret -}}
{{- else -}}
{{- printf "%s-internal" (include "trivy-collector.fullname" .) -}}
{{- end -}}
{{- end }}

{{/*
Names of the Kubernetes objects holding authored state. Reports live on the
scraper's emptyDir and are regenerated on every restart; these two hold what a
human typed and cannot be.
*/}}
{{- define "trivy-collector.notesConfigMapName" -}}
{{- printf "%s-notes" (include "trivy-collector.fullname" .) -}}
{{- end }}

{{- define "trivy-collector.apiTokensSecretName" -}}
{{- printf "%s-api-tokens" (include "trivy-collector.fullname" .) -}}
{{- end }}

{{/*
Base URL the server uses to reach the scraper's internal API.
*/}}
{{- define "trivy-collector.scraperUrl" -}}
{{- printf "http://%s.%s.svc:%v" (include "trivy-collector.scraperName" .) .Release.Namespace .Values.internal.port -}}
{{- end }}

{{/*
External base URL used to render "View report" deep links in outbound
notifications. Both pods need it: the server renders links, and the scraper
now owns alert dispatch. Resolution order:
  1. server.externalUrl (explicit override, full URL)
  2. gateway.hostnames[0] when gateway.enabled
Empty when neither yields a value.
*/}}
{{- define "trivy-collector.externalUrl" -}}
{{- if .Values.server.externalUrl -}}
{{- .Values.server.externalUrl -}}
{{- else if and .Values.server.gateway.enabled .Values.server.gateway.hostnames -}}
{{- printf "https://%s" (index .Values.server.gateway.hostnames 0) -}}
{{- end -}}
{{- end }}

{{/*
Environment shared by both pods: log settings and the internal-API token.
*/}}
{{- define "trivy-collector.commonEnv" -}}
- name: LOG_FORMAT
  value: {{ .Values.logging.format | quote }}
- name: LOG_LEVEL
  value: {{ .Values.logging.level | quote }}
- name: HEALTH_PORT
  value: {{ .Values.health.port | quote }}
- name: INTERNAL_TOKEN
  valueFrom:
    secretKeyRef:
      name: {{ include "trivy-collector.internalSecretName" . }}
      key: {{ .Values.internal.secretKey }}
{{- end }}

{{/*
Create the name of the service account to use
*/}}
{{- define "trivy-collector.serviceAccountName" -}}
{{- if .Values.serviceAccount.create }}
{{- default (include "trivy-collector.fullname" .) .Values.serviceAccount.name }}
{{- else }}
{{- default "default" .Values.serviceAccount.name }}
{{- end }}
{{- end }}
