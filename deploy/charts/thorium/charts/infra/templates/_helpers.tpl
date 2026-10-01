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

{{/*
Whether this chart deploys a managed backing service (global.managed.<service>). Fails if an
infra-level `enabled` toggle is set for the service, since the secrets and operator subcharts
can only follow the global one. "key" names the service's section in these values
when it differs from the service (seaweedfs for s3, postgres for postgres).
Usage: include "infra.managed" (dict "root" $ "service" "s3" "key" "seaweedfs")
*/}}
{{- define "infra.managed" -}}
{{- $key := .key | default .service -}}
{{- if hasKey ((index .root.Values $key) | default dict) "enabled" -}}
{{- fail (printf "infra.%s.enabled is not a chart value; set global.managed.%s instead" $key .service) -}}
{{- end -}}
{{- include "thorium.managed" (dict "root" .root "service" .service) -}}
{{- end -}}
