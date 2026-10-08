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
thorium-credentials secret, then a newly generated random value. With "placeholder" set (render
only, see renderOnly) a fixed placeholder is used instead of generating one, and with "external"
set a missing value fails since the external service already issued it. "name" is the
credential's value under secrets.credentials and "key" its data key in thorium-credentials.
Usage: include "secrets.resolve" (dict "explicit" .x "existing" $existing "name" "n" "key" "k"
  "length" 48 "placeholder" $renderOnly "external" "")
*/}}
{{- define "secrets.resolve" -}}
{{- if .explicit -}}
{{- .explicit -}}
{{- else if and .existing (hasKey .existing .key) -}}
{{- index .existing .key | b64dec -}}
{{- else if .placeholder -}}
{{- printf "render-only-%s" .key -}}
{{- else if .external -}}
{{- fail (printf "secrets.credentials.%s is required when global.managed.%s is false, since the external service issues it" (.name | default .key) .external) -}}
{{- else -}}
{{- randAlphaNum (int .length) -}}
{{- end -}}
{{- end -}}

{{/*
Fail when a value that isn't part of this chart is set, naming the values to use instead.
*/}}
{{- define "secrets.removedValues" -}}
{{- include "thorium.movedValues" (dict "values" .Values "prefix" "secrets." "moved" (list
  (list "quickwitS3Endpoint" "use global.quickwit.s3Endpoint")
  (list "quickwitMetastoreUri" "use global.quickwit.metastoreUri")
  (list "registryAuth" "use secrets.registryBasicAuth.username")
  (list "namespaces" "the namespace list is fixed (set secrets.createNamespaces to false to create them yourself)")
)) -}}
{{- if hasKey .Values "consumers" -}}
{{- fail "secrets.consumers is not a chart value: the Redis, SeaweedFS, and Postgres secrets follow global.managed.redis, global.managed.s3, and global.managed.postgres, and quickwit-credentials follows global.quickwit.enabled" -}}
{{- end -}}
{{- end -}}
