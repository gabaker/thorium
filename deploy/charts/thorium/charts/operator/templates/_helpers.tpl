{{/*
The Thorium image tag.
*/}}
{{- define "operator.tag" -}}
{{- .Values.image.tag | default .Chart.AppVersion -}}
{{- end -}}

{{/*
The cluster-scoped RBAC rules the operator always needs.
*/}}
{{- define "operator.clusterRules" -}}
# the operator applies the ThoriumCluster CRD on startup (server-side apply) and waits for it to
# be established. Server-side apply is a PATCH, and a PATCH that creates a missing CRD is also
# authorized as create, which RBAC can't limit by name
- apiGroups: ["apiextensions.k8s.io"]
  resources: ["customresourcedefinitions"]
  verbs: ["get", "list", "watch", "create"]
# the operator only ever changes its own CRD
- apiGroups: ["apiextensions.k8s.io"]
  resources: ["customresourcedefinitions"]
  resourceNames: ["thoriumclusters.sandia.gov"]
  verbs: ["update", "patch"]
# watching, labelling, and provisioning the nodes the scaler schedules on
- apiGroups: [""]
  resources: ["nodes"]
  verbs: ["get", "list", "watch", "patch"]
{{- end -}}

{{/*
The RBAC rules for the resources the operator manages in a ThoriumCluster's namespace. These are
bound in the thorium namespace with watchOwnNamespaceOnly and cluster-wide otherwise.
*/}}
{{- define "operator.namespacedRules" -}}
# watching ThoriumClusters, managing their finalizer, and reporting status
- apiGroups: ["sandia.gov"]
  resources: ["thoriumclusters"]
  verbs: ["get", "list", "watch", "update", "patch"]
- apiGroups: ["sandia.gov"]
  resources: ["thoriumclusters/status"]
  verbs: ["get", "update", "patch"]
# the config, bootstrap, and rendered Secrets (watched by metadata for changes)
- apiGroups: [""]
  resources: ["secrets"]
  verbs: ["get", "list", "watch", "create", "update", "patch", "delete"]
# node provision pods, rollout diagnostics, and the MCP label on API pods
- apiGroups: [""]
  resources: ["pods"]
  verbs: ["get", "list", "watch", "create", "patch", "delete", "deletecollection"]
# the thorium-api and thorium-mcp services
- apiGroups: [""]
  resources: ["services"]
  verbs: ["get", "create", "update", "patch", "delete"]
# the tracing-conf ConfigMap, and the banner ConfigMap (watched by metadata for changes)
- apiGroups: [""]
  resources: ["configmaps"]
  verbs: ["get", "list", "watch", "create", "update", "patch", "delete"]
# the Thorium component deployments
- apiGroups: ["apps"]
  resources: ["deployments"]
  verbs: ["get", "list", "watch", "create", "update", "patch", "delete"]
{{- end -}}

{{/*
The cluster-wide RBAC rules for the Thorium components, which only the k8s scaler uses (the API,
event handler, search streamer, and agents never call the k8s API). The scaler lists pods, Secrets,
and ConfigMaps across namespaces with field selectors, so these can't be namespaced.
*/}}
{{- define "operator.componentRules" -}}
# a namespace per Thorium group
- apiGroups: [""]
  resources: ["namespaces"]
  verbs: ["list", "create"]
# the schedulable nodes labelled thorium=enabled
- apiGroups: [""]
  resources: ["nodes"]
  verbs: ["list"]
# job pods in the group namespaces
- apiGroups: [""]
  resources: ["pods"]
  verbs: ["list", "create", "delete"]
# each user's keys and the group namespaces' Secrets
- apiGroups: [""]
  resources: ["secrets"]
  verbs: ["get", "list", "create", "update"]
# the passwd/group ConfigMaps in the group namespaces
- apiGroups: [""]
  resources: ["configmaps"]
  verbs: ["list", "create"]
# Thorium network policies in the group namespaces
- apiGroups: ["networking.k8s.io"]
  resources: ["networkpolicies"]
  verbs: ["list", "create", "delete"]
{{- end -}}

{{/*
The name of the image pull secret the chart renders from dockerConfigJson.
*/}}
{{- define "operator.createdPullSecret" -}}
thorium-image-pull
{{- end -}}

{{/*
The names of the Thorium image pull secrets as a YAML list (the ThoriumCluster's
image_pull_secrets): the one the chart renders, then imagePullSecrets, without duplicates.
Usage: include "operator.pullSecretNames" . | fromYamlArray
*/}}
{{- define "operator.pullSecretNames" -}}
{{- $names := list -}}
{{- if .Values.createImagePullSecret -}}
{{- $names = append $names (include "operator.createdPullSecret" .) -}}
{{- end -}}
{{- range .Values.imagePullSecrets | default list -}}
{{- if eq (toString .) "registry-token" -}}
{{- fail "operator.imagePullSecrets must not list registry-token: the operator renders it from operator.cluster.registryAuth and references it whenever registryAuth is set" -}}
{{- end -}}
{{- $names = append $names (toString .) -}}
{{- end -}}
{{- toYaml ($names | uniq) -}}
{{- end -}}

{{/*
The imagePullSecrets of the pods the chart runs: the Thorium image pull secrets, plus
registry-token, which the operator renders from cluster.registryAuth and adds to every Thorium
pod only while registryAuth is set. Renders nothing when there are none.
*/}}
{{- define "operator.imagePullSecrets" -}}
{{- range include "operator.pullSecretNames" . | fromYamlArray }}
- name: {{ . | quote }}
{{- end }}
{{- if .Values.cluster.registryAuth }}
- name: registry-token
{{- end }}
{{- end -}}

{{/*
Common labels applied to every resource.
*/}}
{{- define "operator.labels" -}}
app.kubernetes.io/managed-by: {{ .Release.Service }}
app.kubernetes.io/part-of: thorium
helm.sh/chart: {{ printf "%s-%s" .Chart.Name .Chart.Version }}
{{- end -}}

{{/*
The non-secret thorium.yml defaults, pointing at the infra subchart services unless a
backend host is overridden. A backend that global.managed turns off has no default and must be
set. Secrets are merged in by the operator from configSecrets.
*/}}
{{- define "operator.defaultConfig" -}}
{{- $b := .Values.backends -}}
{{- $k8s := .Values.cluster.scaler.k8s -}}
{{- $indices := include "thorium.elasticIndices" . | fromYaml -}}
{{- $scyllaNodes := $b.scylla.nodes -}}
{{- if not $scyllaNodes -}}
{{- if not (include "thorium.managed" (dict "root" . "service" "scylla")) -}}
{{- fail "operator.backends.scylla.nodes is required when global.managed.scylla is false" -}}
{{- end -}}
{{- $scyllaNodes = list (include "thorium.host" (dict "root" . "service" "scylla-client" "ns" "scylla")) -}}
{{- end -}}
{{- $elasticNode := $b.elastic.node -}}
{{- if not $elasticNode -}}
{{- if not (include "thorium.managed" (dict "root" . "service" "elastic")) -}}
{{- fail "operator.backends.elastic.node is required when global.managed.elastic is false" -}}
{{- end -}}
{{- $elasticNode = printf "https://%s:9200" (include "thorium.host" (dict "root" . "service" "elastic-es-http" "ns" "elastic")) -}}
{{- end -}}
{{- $redisHost := $b.redis.host -}}
{{- if not $redisHost -}}
{{- if not (include "thorium.managed" (dict "root" . "service" "redis")) -}}
{{- fail "operator.backends.redis.host is required when global.managed.redis is false" -}}
{{- end -}}
{{- $redisHost = include "thorium.host" (dict "root" . "service" "redis" "ns" "redis") -}}
{{- end -}}
{{- $s3Endpoint := $b.s3.endpoint -}}
{{- if not $s3Endpoint -}}
{{- if not (include "thorium.managed" (dict "root" . "service" "s3")) -}}
{{- fail "operator.backends.s3.endpoint is required when global.managed.s3 is false" -}}
{{- end -}}
{{- $s3Endpoint = printf "http://%s:8333" (include "thorium.host" (dict "root" . "service" "seaweedfs" "ns" "seaweedfs")) -}}
{{- end -}}
thorium:
  namespace_blacklist:
    {{- include "operator.namespaceBlacklist" . | nindent 4 }}
  tracing:
    {{- /* without an endpoint or Quickwit there is no collector, so no external traces are sent */}}
    {{- $tracing := $b.tracing.grpcEndpoint }}
    {{- if and (not $tracing) (include "thorium.quickwit" .) }}
    {{- $tracing = printf "http://%s:7281" (include "thorium.host" (dict "root" . "service" "quickwit-indexer" "ns" "quickwit")) }}
    {{- end }}
    {{- with $tracing }}
    external:
      Grpc:
        endpoint: {{ . | quote }}
        level: Info
    {{- end }}
    local:
      level: Info
  cors:
    insecure: {{ .Values.cluster.cors.insecure }}
  files:
    bucket: thorium-files
    earliest: 1610596807
    partition_size: 3600
  repos:
    bucket: thorium-repos
    partition_size: 3600
  attachments:
    bucket: thorium-attachments
  results:
    bucket: thorium-results
    earliest: 1610596807
    partition_size: 3600
  ephemeral:
    bucket: thorium-ephemeral
  s3:
    endpoint: {{ $s3Endpoint | quote }}
    region: {{ $b.s3.region | quote }}
    use_path_style: {{ $b.s3.usePathStyle }}
  scaler:
    crane:
      insecure: {{ .Values.cluster.crane.insecure }}
    k8s:
      primary_cluster: {{ $k8s.context | quote }}
      clusters:
        {{ $k8s.context | quote }}:
          alias: {{ $k8s.alias | quote }}
          nodes: {{ $k8s.nodes | default list | toJson }}
redis:
  host: {{ $redisHost | quote }}
  port: {{ $b.redis.port }}
scylla:
  nodes: {{ $scyllaNodes | toJson }}
  replication: {{ $b.scylla.replication }}
elastic:
  node: {{ $elasticNode | quote }}
  {{- if include "operator.elasticCaSecret" . }}
  {{- /* the operator enforces the same validation whenever elastic_ca_secret is set */}}
  insecure_certificates: false
  cert_validation:
    Full: /etc/thorium/elastic-ca/ca.crt
  {{- else }}
  insecure_certificates: {{ $b.elastic.insecureCertificates }}
  {{- end }}
  results:
    repos: {{ $indices.repoResults | quote }}
    samples: {{ $indices.sampleResults | quote }}
  tags:
    repos: {{ $indices.repoTags | quote }}
    samples: {{ $indices.sampleTags | quote }}
{{- end -}}

{{/*
The namespaces Thorium must never create or use for a group, as a YAML list: every namespace this
chart's deployment uses (with the namespace prefix), the infra-operators and Kubernetes system
namespaces, Thorium's built-in defaults, and any extra names from cluster.namespaceBlacklist.
Usage: include "operator.namespaceBlacklist" .
*/}}
{{- define "operator.namespaceBlacklist" -}}
{{- $names := list -}}
{{- range list "thorium" "redis" "scylla" "elastic" "seaweedfs" "quickwit" "jaeger" -}}
{{- $names = append $names (include "thorium.ns" (dict "root" $ "name" .)) -}}
{{- end -}}
{{- $names = concat $names (list "infra-operators" "kube-system" "kube-public" "kube-node-lease" "default") -}}
{{- /* the defaults built into Thorium's config (default_namespace_blacklist in api/src/conf.rs) */ -}}
{{- $names = concat $names (list "thorium" "scylla" "scylla-operator" "cert-manager" "redis" "elastic-system" "jaeger" "quickwit") -}}
{{- $names = concat $names (.Values.cluster.namespaceBlacklist | default list) -}}
{{- toYaml ($names | uniq | sortAlpha) -}}
{{- end -}}

{{/*
A SecretCredentials reference for the ThoriumCluster bootstrap. The Secret is always read from
the thorium namespace.
Usage: include "operator.secretCredentials" .Values.cluster.bootstrap.scylla.adminSecret
*/}}
{{- define "operator.secretCredentials" -}}
name: {{ required "a bootstrap adminSecret needs a name" .name | quote }}
{{- with .username }}
username: {{ . | quote }}
{{- end }}
{{- with .usernameKey }}
username_key: {{ . | quote }}
{{- end }}
password_key: {{ .passwordKey | default "password" | quote }}
{{- end -}}

{{/*
The final ThoriumCluster config: user config deep-merged over the generated defaults.
*/}}
{{- define "operator.config" -}}
{{- $defaults := include "operator.defaultConfig" . | fromYaml -}}
{{- $merged := mustMergeOverwrite $defaults (deepCopy .Values.cluster.config) -}}
{{- toYaml $merged -}}
{{- end -}}

{{/*
The name of the Secret holding an external Elasticsearch's CA (backends.elastic.caSecret.name),
or nothing when it isn't set. The Secret is always read from the thorium namespace, and a CA is
only accepted for an external Elasticsearch since the chart's ECK certificates stay unverified.
Usage: include "operator.elasticCaSecret" .
*/}}
{{- define "operator.elasticCaSecret" -}}
{{- $ca := ((.Values.backends).elastic).caSecret | default dict -}}
{{- if $ca.name -}}
{{- if include "thorium.managed" (dict "root" . "service" "elastic") -}}
{{- fail "operator.backends.elastic.caSecret is only for an external Elasticsearch; set global.managed.elastic to false or unset caSecret.name" -}}
{{- end -}}
{{- $ca.name -}}
{{- end -}}
{{- end -}}

{{/*
A string quoted for a POSIX shell: wrapped in single quotes, with each single quote inside it
closed, escaped, and reopened, so the shell passes it on unchanged.
Usage: include "operator.shellQuote" "it's"
*/}}
{{- define "operator.shellQuote" -}}
{{- printf "'%s'" (replace "'" "'\\''" (toString .)) -}}
{{- end -}}
