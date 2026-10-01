{{/*
Helpers shared by the thorium chart and its subcharts. Every subchart is rendered with the
umbrella's release, so these see the same release namespace in all of them.
*/}}

{{/*
The namespace prefix, derived from the release namespace: installing into "thorium" gives no
prefix, installing into "b-thorium" gives "b-".
*/}}
{{- define "thorium.prefix" -}}
{{- if not (hasSuffix "thorium" .Release.Namespace) -}}
{{- fail (printf "install the thorium chart into the thorium namespace or a prefixed one such as b-thorium (got %q)" .Release.Namespace) -}}
{{- end -}}
{{- $prefix := trimSuffix "thorium" .Release.Namespace -}}
{{- if and $prefix (not (regexMatch "^[a-z0-9]([-a-z0-9]*[a-z0-9])?-$" $prefix)) -}}
{{- fail (printf "the namespace prefix %q must be a DNS label followed by '-' (e.g. b-thorium)" $prefix) -}}
{{- end -}}
{{- $prefix -}}
{{- end -}}

{{/*
A prefixed namespace name.
Usage: include "thorium.ns" (dict "root" $ "name" "redis")
*/}}
{{- define "thorium.ns" -}}
{{- printf "%s%s" (include "thorium.prefix" .root) .name -}}
{{- end -}}

{{/*
The namespace the Thorium operator and components run in (the release namespace).
*/}}
{{- define "thorium.namespace" -}}
{{- include "thorium.ns" (dict "root" . "name" "thorium") -}}
{{- end -}}

{{/*
An in-cluster service hostname in a prefixed namespace.
Usage: include "thorium.host" (dict "root" $ "service" "redis" "ns" "redis")
*/}}
{{- define "thorium.host" -}}
{{- $domain := (.root.Values.global).clusterDomain | default "cluster.local" -}}
{{- printf "%s.%s.svc.%s" .service (include "thorium.ns" (dict "root" .root "name" .ns)) $domain -}}
{{- end -}}

{{/*
An image reference, moved to global.imageRegistry when one is set. The upstream registry host
is dropped and the rest of the path kept (docker.io/library/redis:7 -> <mirror>/redis:7,
docker.io/scylladb/scylla -> <mirror>/scylladb/scylla), matching how mirror registries are
usually populated.
Usage: include "thorium.image" (dict "root" $ "image" "docker.io/redis:7.4.11")
*/}}
{{- define "thorium.image" -}}
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
