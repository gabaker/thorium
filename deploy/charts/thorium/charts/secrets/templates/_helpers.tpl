{{/*
Common labels applied to every resource.
*/}}
{{- define "secrets.labels" -}}
app.kubernetes.io/managed-by: {{ .Release.Service }}
app.kubernetes.io/part-of: thorium
helm.sh/chart: {{ printf "%s-%s" .Chart.Name .Chart.Version }}
{{- end -}}

{{/*
Resolve one credential: an explicit value wins, then the value stored in the existing
thorium-credentials secret, then a newly generated random value.
Usage: include "secrets.resolve" (dict "explicit" .x "existing" $existing "key" "k" "length" 48)
*/}}
{{- define "secrets.resolve" -}}
{{- if .explicit -}}
{{- .explicit -}}
{{- else if and .existing (hasKey .existing .key) -}}
{{- index .existing .key | b64dec -}}
{{- else -}}
{{- randAlphaNum (int .length) -}}
{{- end -}}
{{- end -}}
