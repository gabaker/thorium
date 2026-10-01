{{/*
Common labels applied to every resource.
*/}}
{{- define "infra.labels" -}}
app.kubernetes.io/managed-by: {{ .Release.Service }}
app.kubernetes.io/part-of: thorium
helm.sh/chart: {{ printf "%s-%s" .Chart.Name .Chart.Version }}
{{- end -}}

{{/*
Render a storageClassName field when a storage class is configured.
*/}}
{{- define "infra.storageClass" -}}
{{- if .Values.storageClass }}
storageClassName: {{ .Values.storageClass | quote }}
{{- end }}
{{- end -}}
