{{/*
Helpers shared by the thorium chart and its subcharts. Every subchart is rendered with the
umbrella's release, so these see the same release namespace in all of them.
*/}}

{{/*
The namespace prefix with its "-" separator, derived from the release namespace: installing
into "thorium" gives no prefix, installing into "dev-thorium" gives "dev-". The prefix itself
must be a lowercase DNS label of at most 32 characters, matching megathor's namespace_prefix.
*/}}
{{- define "thorium.prefix" -}}
{{- if not (hasSuffix "thorium" .Release.Namespace) -}}
{{- fail (printf "install the thorium chart into the thorium namespace or <prefix>-thorium, such as dev-thorium (got %q)" .Release.Namespace) -}}
{{- end -}}
{{- $prefix := trimSuffix "thorium" .Release.Namespace -}}
{{- if and $prefix (not (regexMatch "^[a-z0-9]([-a-z0-9]{0,30}[a-z0-9])?-$" $prefix)) -}}
{{- fail (printf "the namespace prefix %q must be a lowercase DNS label of at most 32 characters (install into thorium or <prefix>-thorium, such as dev-thorium)" (trimSuffix "-" $prefix)) -}}
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

{{/*
Whether a backing service is deployed and managed by this chart (global.managed.<service>).
Renders "true" or nothing so it can be used directly in an if.
Usage: include "thorium.managed" (dict "root" $ "service" "elastic")
*/}}
{{- define "thorium.managed" -}}
{{- if dig .service true (((.root.Values.global).managed) | default dict) -}}true{{- end -}}
{{- end -}}

{{/*
Whether the operator bootstraps Thorium's Scylla role (cluster.bootstrap.scylla). A null
enabled follows global.managed.scylla. Renders "true" or nothing. The operator subchart and the
umbrella's credential checks both use this so they always agree.
Usage: include "thorium.bootstrapScylla" (dict "root" $ "bootstrap" .Values.cluster.bootstrap)
*/}}
{{- define "thorium.bootstrapScylla" -}}
{{- $enabled := dig "scylla" "enabled" nil (.bootstrap | default dict) -}}
{{- if kindIs "invalid" $enabled -}}
{{- $enabled = include "thorium.managed" (dict "root" .root "service" "scylla") -}}
{{- end -}}
{{- if $enabled -}}true{{- end -}}
{{- end -}}

{{/*
Whether the operator bootstraps Thorium's role and user in an external Elasticsearch
(cluster.bootstrap.elastic). A null enabled turns it on only when global.managed.elastic is
false and adminSecret.name is set. Renders "true" or nothing. The operator subchart and the
umbrella's credential checks both use this so they always agree.
Usage: include "thorium.bootstrapElastic" (dict "root" $ "bootstrap" .Values.cluster.bootstrap)
*/}}
{{- define "thorium.bootstrapElastic" -}}
{{- $bootstrap := .bootstrap | default dict -}}
{{- $enabled := dig "elastic" "enabled" nil $bootstrap -}}
{{- if kindIs "invalid" $enabled -}}
{{- $external := not (include "thorium.managed" (dict "root" .root "service" "elastic")) -}}
{{- $enabled = and $external (ne (toString (dig "elastic" "adminSecret" "name" "" $bootstrap)) "") -}}
{{- end -}}
{{- if $enabled -}}true{{- end -}}
{{- end -}}

{{/*
The Secret in <prefix>-elastic holding Thorium's user for ECK's file realm.
*/}}
{{- define "thorium.elasticUserSecret" -}}
thorium-elastic-user
{{- end -}}

{{/*
The Secret in <prefix>-elastic holding Thorium's Elastic role for ECK.
*/}}
{{- define "thorium.elasticRolesSecret" -}}
thorium-elastic-roles
{{- end -}}

{{/*
The Elastic role granting Thorium access to its indexes.
*/}}
{{- define "thorium.elasticRole" -}}
thorium
{{- end -}}

{{/*
Thorium's Elastic index names as a dict (global.elasticIndices with the defaults filled in).
Usage: include "thorium.elasticIndices" $ | fromYaml
*/}}
{{- define "thorium.elasticIndices" -}}
{{- $defaults := dict "sampleResults" "thorium_sample_results" "repoResults" "thorium_repo_results" "sampleTags" "thorium_sample_tags" "repoTags" "thorium_repo_tags" -}}
{{- toYaml (mustMergeOverwrite $defaults (deepCopy (((.Values.global).elasticIndices) | default dict))) -}}
{{- end -}}
