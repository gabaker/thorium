{{/*
An image reference, moved to global.imageRegistry when one is set (the upstream registry host
is dropped and the rest of the path kept, as in the thorium chart).
Usage: include "infra-operators.image" (dict "root" $ "image" "docker.io/reactivetechio/kubegres:1.19")
*/}}
{{- define "infra-operators.image" -}}
{{- $mirror := (.root.Values.global).imageRegistry -}}
{{- if not $mirror -}}
{{- .image -}}
{{- else -}}
{{- $parts := splitList "/" .image -}}
{{- $first := first $parts -}}
{{- if and (gt (len $parts) 1) (or (contains "." $first) (contains ":" $first) (eq $first "localhost")) -}}
{{- $parts = rest $parts -}}
{{- end -}}
{{- if and (gt (len $parts) 1) (eq (first $parts) "library") -}}
{{- $parts = rest $parts -}}
{{- end -}}
{{- printf "%s/%s" (trimSuffix "/" $mirror) (join "/" $parts) -}}
{{- end -}}
{{- end -}}
