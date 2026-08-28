{{- define "trivy-collector.name" -}}
{{- default .Chart.Name .Values.nameOverride | trunc 63 | trimSuffix "-" }}
{{- end }}

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

{{- define "trivy-collector.chart" -}}
{{- printf "%s-%s" .Chart.Name .Chart.Version | replace "+" "_" | trunc 63 | trimSuffix "-" }}
{{- end }}

{{- define "trivy-collector.labels" -}}
helm.sh/chart: {{ include "trivy-collector.chart" . }}
{{ include "trivy-collector.selectorLabels" . }}
{{- if .Chart.AppVersion }}
app.kubernetes.io/version: {{ .Chart.AppVersion | quote }}
{{- end }}
app.kubernetes.io/managed-by: {{ .Release.Service }}
{{- end }}

{{- define "trivy-collector.selectorLabels" -}}
app.kubernetes.io/name: {{ include "trivy-collector.name" . }}
app.kubernetes.io/instance: {{ .Release.Name }}
{{- end }}

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

{{- define "trivy-collector.internalSecretName" -}}
{{- if .Values.internal.existingSecret -}}
{{- .Values.internal.existingSecret -}}
{{- else -}}
{{- printf "%s-internal" (include "trivy-collector.fullname" .) -}}
{{- end -}}
{{- end }}

{{- define "trivy-collector.notesConfigMapName" -}}
{{- printf "%s-notes" (include "trivy-collector.fullname" .) -}}
{{- end }}

{{- define "trivy-collector.apiTokensSecretName" -}}
{{- printf "%s-api-tokens" (include "trivy-collector.fullname" .) -}}
{{- end }}

{{- define "trivy-collector.scraperUrl" -}}
{{- printf "http://%s.%s.svc:%v" (include "trivy-collector.scraperName" .) .Release.Namespace .Values.internal.port -}}
{{- end }}

{{- define "trivy-collector.externalUrl" -}}
{{- if .Values.server.externalUrl -}}
{{- .Values.server.externalUrl -}}
{{- else if and .Values.server.gateway.enabled .Values.server.gateway.hostnames -}}
{{- printf "https://%s" (index .Values.server.gateway.hostnames 0) -}}
{{- end -}}
{{- end }}

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

{{- define "trivy-collector.serviceAccountName" -}}
{{- if .Values.serviceAccount.create }}
{{- default (include "trivy-collector.fullname" .) .Values.serviceAccount.name }}
{{- else }}
{{- default "default" .Values.serviceAccount.name }}
{{- end }}
{{- end }}
