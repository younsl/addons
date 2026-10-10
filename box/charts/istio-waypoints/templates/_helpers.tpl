{{/*
Common labels
*/}}
{{- define "istio-waypoints.labels" -}}
helm.sh/chart: {{ include "istio-waypoints.chart" . }}
{{ include "istio-waypoints.selectorLabels" . }}
{{- if .Chart.AppVersion }}
app.kubernetes.io/version: {{ .Chart.AppVersion | quote }}
{{- end }}
app.kubernetes.io/managed-by: {{ .Release.Service }}
{{- if .Values.commonLabels }}
{{- range $key, $value := .Values.commonLabels }}
{{ $key }}: {{ $value | quote }}
{{- end }}
{{- end }}
{{- end -}}

{{/*
Selector labels
*/}}
{{- define "istio-waypoints.selectorLabels" -}}
app.kubernetes.io/name: {{ include "istio-waypoints.name" . }}
app.kubernetes.io/instance: {{ .Release.Name }}
{{- end -}}

{{/*
Create chart name and version as used by the chart label.
*/}}
{{- define "istio-waypoints.chart" -}}
{{- printf "%s-%s" .Chart.Name .Chart.Version | replace "+" "_" | trunc 63 | trimSuffix "-" -}}
{{- end -}}

{{/*
Return the application name.
*/}}
{{- define "istio-waypoints.name" -}}
{{- default .Chart.Name .Values.nameOverride | trunc 63 | trimSuffix "-" -}}
{{- end -}}

{{/*
Create a default fully qualified app name.
*/}}
{{- define "istio-waypoints.fullname" -}}
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

{{/*
Resolve the effective configuration of a single waypoint.
Deep-merges the per-waypoint entry over .Values.defaults (maps merge, lists are replaced)
and fills in derived fields: key, name, namespace.

Usage: {{ include "istio-waypoints.resolve" (dict "key" $key "waypoint" $waypoint "root" $) | fromYaml }}
*/}}
{{- define "istio-waypoints.resolve" -}}
{{- $cfg := mergeOverwrite (deepCopy .root.Values.defaults) (deepCopy (.waypoint | default dict)) -}}
{{- if and $cfg.enabled (not $cfg.namespace) -}}
{{- fail (printf "waypoints.%s: namespace is required" .key) -}}
{{- end -}}
{{- $_ := set $cfg "key" .key -}}
{{- $_ := set $cfg "name" ($cfg.name | default .key) -}}
{{- $cfg | toYaml -}}
{{- end -}}

{{/*
Name of the parametersRef ConfigMap of a waypoint.
*/}}
{{- define "istio-waypoints.parametersName" -}}
{{- printf "%s-options" .name -}}
{{- end -}}

{{/*
Whether a waypoint carries any parametersRef content.
*/}}
{{- define "istio-waypoints.hasParameters" -}}
{{- $p := .parameters | default dict -}}
{{- range $kind, $patch := $p -}}
{{- if $patch -}}true{{- end -}}
{{- end -}}
{{- end -}}

{{/*
istio-proxy container patch composed from .container. Empty output when it has no content
besides name.
*/}}
{{- define "istio-waypoints.proxyContainer" -}}
{{- $container := .container | default dict -}}
{{- $c := dict -}}
{{- range $field, $value := $container -}}
{{- if and $value (ne $field "name") -}}
{{- $_ := set $c $field $value -}}
{{- end -}}
{{- end -}}
{{- if $c -}}
{{- $_ := set $c "name" ($container.name | default "istio-proxy") -}}
{{- $c | toYaml -}}
{{- end -}}
{{- end -}}

{{/*
Labels shared by every resource rendered for one waypoint.
*/}}
{{- define "istio-waypoints.waypointLabels" -}}
{{ include "istio-waypoints.labels" .root }}
istio.io/waypoint-for: {{ .cfg.waypointFor | quote }}
{{- range $key, $value := .cfg.labels }}
{{ $key }}: {{ $value | quote }}
{{- end }}
{{- end -}}

{{/*
Annotations shared by every resource rendered for one waypoint.
*/}}
{{- define "istio-waypoints.waypointAnnotations" -}}
{{- range $key, $value := .root.Values.commonAnnotations }}
{{ $key }}: {{ $value | quote }}
{{- end }}
{{- range $key, $value := .cfg.annotations }}
{{ $key }}: {{ $value | quote }}
{{- end }}
{{- end -}}
