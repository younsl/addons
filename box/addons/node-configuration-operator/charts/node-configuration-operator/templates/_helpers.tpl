{{- define "node-configuration-operator.name" -}}
{{- default .Chart.Name .Values.nameOverride | trunc 63 | trimSuffix "-" }}
{{- end }}

{{- define "node-configuration-operator.fullname" -}}
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

{{- define "node-configuration-operator.labels" -}}
helm.sh/chart: {{ printf "%s-%s" .Chart.Name .Chart.Version | replace "+" "_" | trunc 63 | trimSuffix "-" }}
app.kubernetes.io/name: {{ include "node-configuration-operator.name" . }}
app.kubernetes.io/instance: {{ .Release.Name }}
app.kubernetes.io/version: {{ .Chart.AppVersion | quote }}
app.kubernetes.io/managed-by: {{ .Release.Service }}
{{- end }}

{{- define "node-configuration-operator.selectorLabels" -}}
app.kubernetes.io/name: {{ include "node-configuration-operator.name" . }}
app.kubernetes.io/instance: {{ .Release.Name }}
{{- end }}

{{- define "node-configuration-operator.serviceAccountName" -}}
{{- if .Values.serviceAccount.create }}
{{- default (include "node-configuration-operator.fullname" .) .Values.serviceAccount.name }}
{{- else }}
{{- default "default" .Values.serviceAccount.name }}
{{- end }}
{{- end }}

{{- define "node-configuration-operator.image" -}}
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

{{- define "node-configuration-operator.agentName" -}}
{{- printf "%s-agent" (include "node-configuration-operator.fullname" .) | trunc 63 | trimSuffix "-" }}
{{- end }}

{{- define "node-configuration-operator.agentSelectorLabels" -}}
app.kubernetes.io/name: {{ include "node-configuration-operator.name" . }}-agent
app.kubernetes.io/instance: {{ .Release.Name }}
{{- end }}

{{- define "node-configuration-operator.agentServiceAccountName" -}}
{{- if .Values.agent.serviceAccount.create }}
{{- default (include "node-configuration-operator.agentName" .) .Values.agent.serviceAccount.name }}
{{- else }}
{{- default "default" .Values.agent.serviceAccount.name }}
{{- end }}
{{- end }}

{{- define "node-configuration-operator.agentTemplateName" -}}
{{- printf "%s-agent-template" (include "node-configuration-operator.fullname" .) | trunc 63 | trimSuffix "-" }}
{{- end }}

{{- define "node-configuration-operator.agentDaemonSet" -}}
apiVersion: apps/v1
kind: DaemonSet
metadata:
  name: {{ include "node-configuration-operator.agentName" . }}
  namespace: {{ .Release.Namespace }}
  labels:
    {{- include "node-configuration-operator.labels" . | nindent 4 }}
spec:
  revisionHistoryLimit: {{ .Values.revisionHistoryLimit }}
  selector:
    matchLabels:
      {{- include "node-configuration-operator.agentSelectorLabels" . | nindent 6 }}
  {{- with .Values.agent.updateStrategy }}
  updateStrategy:
    {{- toYaml . | nindent 4 }}
  {{- end }}
  template:
    metadata:
      labels:
        {{- include "node-configuration-operator.agentSelectorLabels" . | nindent 8 }}
      {{- with .Values.agent.podAnnotations }}
      annotations:
        {{- toYaml . | nindent 8 }}
      {{- end }}
    spec:
      serviceAccountName: {{ include "node-configuration-operator.agentServiceAccountName" . }}
      {{- with .Values.agent.priorityClassName }}
      priorityClassName: {{ . }}
      {{- end }}
      {{- with .Values.imagePullSecrets }}
      imagePullSecrets:
        {{- toYaml . | nindent 8 }}
      {{- end }}
      containers:
        - name: agent
          image: {{ include "node-configuration-operator.image" . | quote }}
          imagePullPolicy: {{ .Values.image.pullPolicy }}
          args:
            - agent
          env:
            - name: LOG_LEVEL
              value: {{ .Values.logging.level | quote }}
            - name: LOG_FORMAT
              value: {{ .Values.logging.format | quote }}
            - name: HOST_ROOT
              value: /host
            - name: ALLOW_SCRIPTS
              value: {{ .Values.agent.allowScripts | quote }}
            - name: NODE_NAME
              valueFrom:
                fieldRef:
                  apiVersion: v1
                  fieldPath: spec.nodeName
          ports:
            - name: http
              containerPort: 8080
              protocol: TCP
          livenessProbe:
            httpGet:
              path: /healthz
              port: http
            periodSeconds: 10
          readinessProbe:
            httpGet:
              path: /readyz
              port: http
            periodSeconds: 10
          resources:
            {{- toYaml .Values.agent.resources | nindent 12 }}
          {{- with .Values.agent.resizePolicy }}
          resizePolicy:
            {{- toYaml . | nindent 12 }}
          {{- end }}
          securityContext:
            {{- if .Values.agent.allowScripts }}
            runAsUser: 0
            runAsGroup: 0
            privileged: true
            readOnlyRootFilesystem: true
            {{- else }}
            {{- toYaml .Values.agent.securityContext | nindent 12 }}
            {{- end }}
          volumeMounts:
            - name: host-root
              mountPath: /host
              mountPropagation: HostToContainer
      {{- if .Values.agent.allowScripts }}
      hostPID: true
      {{- end }}
      volumes:
        - name: host-root
          hostPath:
            path: /
            type: Directory
      {{- with .Values.agent.nodeSelector }}
      nodeSelector:
        {{- toYaml . | nindent 8 }}
      {{- end }}
      {{- with .Values.agent.affinity }}
      affinity:
        {{- toYaml . | nindent 8 }}
      {{- end }}
      {{- with .Values.agent.tolerations }}
      tolerations:
        {{- toYaml . | nindent 8 }}
      {{- end }}
      {{- with .Values.agent.extraPodSpec }}
      {{- toYaml . | nindent 6 }}
      {{- end }}
{{- end }}
