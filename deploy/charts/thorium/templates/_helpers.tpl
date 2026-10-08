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
{{- if not (or (eq .Release.Namespace "thorium") (hasSuffix "-thorium" .Release.Namespace)) -}}
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
Whether a changed credential is pushed into the service that checks it
(global.manageSecretUpdates.<service>): Redis and SeaweedFS restart with it, the chart's Postgres
gets an ALTER USER, and the operator sets Thorium's Scylla role through a superuser. A null value
turns it on wherever Thorium can change that password: the services this chart deploys, and an
external Scylla the operator bootstraps. An external Redis, S3 store, or Postgres has no API
Thorium can change a password through, so true fails there, as it does for an external Scylla
without its bootstrap. Elasticsearch has no setting, since ECK's file realm and the operator's
Elastic bootstrap always follow a changed credential. Renders "true" or nothing.
Usage: include "thorium.manageSecretUpdates" (dict "root" $ "service" "redis")
*/}}
{{- define "thorium.manageSecretUpdates" -}}
{{- $setting := dig .service nil (((.root.Values.global).manageSecretUpdates) | default dict) -}}
{{- $managed := include "thorium.managed" (dict "root" .root "service" .service) -}}
{{- $possible := $managed -}}
{{- $reason := printf "global.managed.%s is false" .service -}}
{{- if eq .service "postgres" -}}
{{- $possible = and $managed (include "thorium.quickwit" .root) -}}
{{- $reason = "the chart's Postgres needs both global.managed.postgres and global.quickwit.enabled" -}}
{{- else if eq .service "scylla" -}}
{{- /* the umbrella has the operator's values; the operator subchart has its own */ -}}
{{- $cluster := ((.root.Values.operator).cluster) | default .root.Values.cluster | default dict -}}
{{- $bootstrap := $cluster.bootstrap | default dict -}}
{{- $possible = or $managed (include "thorium.bootstrapScylla" (dict "root" .root "bootstrap" $bootstrap)) -}}
{{- $reason = "global.managed.scylla is false and operator.cluster.bootstrap.scylla is off, so the operator has no superuser to change it with" -}}
{{- end -}}
{{- if kindIs "invalid" $setting -}}
{{- $setting = $possible -}}
{{- else if and $setting (not $possible) -}}
{{- fail (printf "global.manageSecretUpdates.%s can't be true: %s. Change the credential in the service yourself, then set it under global.credentials" .service $reason) -}}
{{- end -}}
{{- if $setting -}}true{{- end -}}
{{- end -}}

{{/*
The Helm release revision at which each group of credentials last changed, as YAML, for the
thorium.sandia.gov/credentials-revision annotation that restarts what reads them: redis
(redisPassword), s3 (the S3 identity), postgres (postgresPassword), and quickwit (the metastore and
S3 credentials). A group changes when a value set under global.credentials differs from the one
stored in thorium-credentials, so no credential (or anything derived from it) is ever rendered
outside a Secret, and generated credentials, which never change, never restart anything. Each
subchart computes this itself, since Helm's lookup returns the stored values from before the
upgrade; the secrets subchart keeps the result in the thorium-credential-revisions ConfigMap, so an
unchanged group carries its revision forward. With global.manageSecretUpdates off for a service
its revision is kept and the change recorded as pending (<group>Pending), so turning it back on
still applies the change. Quickwit always follows. When lookup can't reach the cluster (GitOps
renderers) changes can't be detected, so a keyed hash of the explicit values is used instead,
keyed with global.credentials.thoriumSecretKey (which such renders must set) so it can't be
reversed without that key.
Usage: (include "thorium.credentialRevisions" $ | fromYaml).redis
*/}}
{{- define "thorium.credentialRevisions" -}}
{{- $explicit := ((.Values.global).credentials) | default dict -}}
{{- $groups := dict "redis" (list "redisPassword") "s3" (list "s3AccessKey" "s3SecretToken") "postgres" (list "postgresPassword") "quickwit" (list "postgresPassword" "s3AccessKey" "s3SecretToken") -}}
{{- /* thorium-credentials stores s3SecretToken under s3SecretKey */ -}}
{{- $storedKeys := dict "s3SecretToken" "s3SecretKey" -}}
{{- $ns := include "thorium.namespace" . -}}
{{- $revisions := dict -}}
{{- if lookup "v1" "Namespace" "" "kube-system" -}}
{{- $stored := (lookup "v1" "Secret" $ns "thorium-credentials").data | default dict -}}
{{- $previous := (lookup "v1" "ConfigMap" $ns "thorium-credential-revisions").data | default dict -}}
{{- range $group, $keys := $groups -}}
{{- /* a group changes when an explicit value differs from the stored one */ -}}
{{- $changed := false -}}
{{- range $keys -}}
{{- $value := toString (index $explicit . | default "") -}}
{{- $storedValue := index $stored (get $storedKeys . | default .) | default "" | b64dec -}}
{{- if and $value (ne $value $storedValue) -}}
{{- $changed = true -}}
{{- end -}}
{{- end -}}
{{- $pending := or $changed (eq (get $previous (printf "%sPending" $group)) "true") -}}
{{- $enabled := or (eq $group "quickwit") (include "thorium.manageSecretUpdates" (dict "root" $ "service" $group)) -}}
{{- if and $pending $enabled -}}
{{- $_ := set $revisions $group (toString $.Release.Revision) -}}
{{- else -}}
{{- $_ := set $revisions $group (get $previous $group | default "0") -}}
{{- if $pending -}}
{{- $_ := set $revisions (printf "%sPending" $group) "true" -}}
{{- end -}}
{{- end -}}
{{- end -}}
{{- else -}}
{{- $key := toString ($explicit.thoriumSecretKey | default "") -}}
{{- range $group, $keys := $groups -}}
{{- $parts := list -}}
{{- range $keys -}}
{{- $parts = append $parts (printf "%s=%s" . (toString (index $explicit . | default ""))) -}}
{{- end -}}
{{- /* without the key (render-only output) there is nothing to protect the values with */ -}}
{{- if $key -}}
{{- $inner := sha256sum (printf "%s%s" $key (toJson $parts)) -}}
{{- $_ := set $revisions $group (sha256sum (printf "%s%s" $key $inner) | trunc 16) -}}
{{- else -}}
{{- $_ := set $revisions $group "0" -}}
{{- end -}}
{{- end -}}
{{- end -}}
{{- toYaml $revisions -}}
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

{{/*
Whether Quickwit is enabled (global.quickwit.enabled, also the quickwit subchart's condition), so
the subcharts only deploy and point at what Quickwit needs while it is. Renders "true" or nothing.
Usage: include "thorium.quickwit" $
*/}}
{{- define "thorium.quickwit" -}}
{{- if dig "quickwit" "enabled" true (((.Values.global) | default dict)) -}}true{{- end -}}
{{- end -}}

{{/*
Fail when a value this chart doesn't read is set, naming the value to use instead. Helm ignores
unknown keys, so without this check a value under an unused name would silently have no effect (a
pull secret or basic auth that is never applied, a credential that is generated instead of the one
given). "moved" lists [path, advice] pairs, with paths relative to "values" and shown with
"prefix" in front.
Usage: include "thorium.movedValues" (dict "values" .Values "prefix" "secrets." "moved" (list
  (list "quickwitS3Endpoint" "use global.quickwit.s3Endpoint")))
*/}}
{{- define "thorium.movedValues" -}}
{{- $values := .values -}}
{{- $prefix := .prefix -}}
{{- range .moved -}}
{{- $node := $values -}}
{{- $found := true -}}
{{- range splitList "." (first .) -}}
{{- if and $found (kindIs "map" $node) (hasKey $node .) -}}
{{- $node = index $node . -}}
{{- else -}}
{{- $found = false -}}
{{- end -}}
{{- end -}}
{{- if $found -}}
{{- fail (printf "%s%s is not a chart value; %s" $prefix (first .) (last .)) -}}
{{- end -}}
{{- end -}}
{{- end -}}
