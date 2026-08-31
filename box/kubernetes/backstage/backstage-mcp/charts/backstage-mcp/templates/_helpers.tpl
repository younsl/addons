{{- define "backstage-mcp.name" -}}
{{- default .Chart.Name .Values.nameOverride | trunc 63 | trimSuffix "-" -}}
{{- end -}}

{{- define "backstage-mcp.fullname" -}}
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

{{- define "backstage-mcp.labels" -}}
helm.sh/chart: {{ printf "%s-%s" .Chart.Name .Chart.Version | replace "+" "_" | trunc 63 | trimSuffix "-" }}
app.kubernetes.io/name: {{ include "backstage-mcp.name" . }}
app.kubernetes.io/instance: {{ .Release.Name }}
app.kubernetes.io/version: {{ .Chart.AppVersion | quote }}
app.kubernetes.io/managed-by: {{ .Release.Service }}
{{- end -}}

{{- define "backstage-mcp.selectorLabels" -}}
app.kubernetes.io/name: {{ include "backstage-mcp.name" . }}
app.kubernetes.io/instance: {{ .Release.Name }}
{{- end -}}

{{- define "backstage-mcp.serviceAccountName" -}}
{{- if .Values.serviceAccount.create -}}
{{- default (include "backstage-mcp.fullname" .) .Values.serviceAccount.name -}}
{{- else -}}
{{- default "default" .Values.serviceAccount.name -}}
{{- end -}}
{{- end -}}

{{/*
Name of the Secret holding the Backstage token: an externally managed one when
given, otherwise the Secret this chart renders.
*/}}
{{- define "backstage-mcp.backstageSecretName" -}}
{{- default (printf "%s-backstage" (include "backstage-mcp.fullname" .)) .Values.backstage.existingSecret -}}
{{- end -}}

{{- define "backstage-mcp.backstageSecretKey" -}}
{{- if .Values.backstage.existingSecret -}}
{{- .Values.backstage.existingSecretKey -}}
{{- else -}}
BACKSTAGE_TOKEN
{{- end -}}
{{- end -}}

{{- define "backstage-mcp.backstageAuthEnabled" -}}
{{- if or .Values.backstage.existingSecret .Values.backstage.token -}}
true
{{- end -}}
{{- end -}}

{{/*
Name of the Secret holding the inbound bearer token, on the same rules.
*/}}
{{- define "backstage-mcp.mcpSecretName" -}}
{{- default (printf "%s-mcp" (include "backstage-mcp.fullname" .)) .Values.mcp.existingSecret -}}
{{- end -}}

{{- define "backstage-mcp.mcpSecretKey" -}}
{{- if .Values.mcp.existingSecret -}}
{{- .Values.mcp.existingSecretKey -}}
{{- else -}}
MCP_BEARER_TOKEN
{{- end -}}
{{- end -}}

{{- define "backstage-mcp.mcpAuthorizationKey" -}}
{{- if .Values.mcp.existingSecret -}}
{{- .Values.mcp.existingSecretAuthorizationKey -}}
{{- else -}}
AUTHORIZATION
{{- end -}}
{{- end -}}

{{- define "backstage-mcp.mcpAuthEnabled" -}}
{{- if or .Values.mcp.existingSecret .Values.mcp.bearerToken -}}
true
{{- end -}}
{{- end -}}

{{/*
In-cluster URL of the MCP endpoint, as a RemoteMCPServer needs it.
*/}}
{{- define "backstage-mcp.url" -}}
{{- printf "http://%s.%s.svc:%d%s" (include "backstage-mcp.fullname" .) .Release.Namespace (int .Values.ports.http) .Values.mcp.path -}}
{{- end -}}
