{{- define "ward.name" -}}
{{- default .Chart.Name .Values.nameOverride | trunc 63 | trimSuffix "-" -}}
{{- end -}}

{{- define "ward.fullname" -}}
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

{{- define "ward.labels" -}}
helm.sh/chart: {{ printf "%s-%s" .Chart.Name .Chart.Version | replace "+" "_" | trunc 63 | trimSuffix "-" }}
app.kubernetes.io/name: {{ include "ward.name" . }}
app.kubernetes.io/instance: {{ .Release.Name }}
app.kubernetes.io/version: {{ .Chart.AppVersion | quote }}
app.kubernetes.io/managed-by: {{ .Release.Service }}
{{- end -}}

{{- define "ward.selectorLabels" -}}
app.kubernetes.io/name: {{ include "ward.name" . }}
app.kubernetes.io/instance: {{ .Release.Name }}
app.kubernetes.io/component: operator
{{- end -}}

{{- define "ward.serviceAccountName" -}}
{{- if .Values.serviceAccount.create -}}
{{- default (include "ward.fullname" .) .Values.serviceAccount.name -}}
{{- else -}}
{{- default "default" .Values.serviceAccount.name -}}
{{- end -}}
{{- end -}}

{{- define "ward.workloadClusterRoleName" -}}
{{- printf "%s-workload" (include "ward.fullname" .) | trunc 63 | trimSuffix "-" -}}
{{- end -}}

{{- define "ward.leaseClusterRoleName" -}}
{{- printf "%s-agent-lease" (include "ward.fullname" .) | trunc 63 | trimSuffix "-" -}}
{{- end -}}

{{- define "ward.leaderElectEnabled" -}}
{{- if gt (int .Values.replicaCount) 1 -}}
true
{{- end -}}
{{- end -}}

{{- define "ward.image" -}}
{{- $registry := .Values.image.registry | trimSuffix "/" -}}
{{- $tag := .Values.image.tag | default .Chart.AppVersion -}}
{{- if $registry -}}
{{- printf "%s/%s:%s" $registry .Values.image.repository $tag -}}
{{- else -}}
{{- printf "%s:%s" .Values.image.repository $tag -}}
{{- end -}}
{{- with .Values.image.digest -}}
@{{ . }}
{{- end -}}
{{- end -}}
