{{/*
The Thorium image tag.
*/}}
{{- define "operator.tag" -}}
{{- .Values.image.tag | default .Chart.AppVersion -}}
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
backend host is overridden. Secrets are merged in by the operator from configSecrets.
*/}}
{{- define "operator.defaultConfig" -}}
{{- $b := .Values.backends -}}
{{- $scyllaNodes := $b.scylla.nodes | default (list (include "thorium.host" (dict "root" . "service" "scylla-client" "ns" "scylla"))) -}}
thorium:
  tracing:
    external:
      Grpc:
        endpoint: {{ $b.tracing.endpoint | default (printf "http://%s:7281" (include "thorium.host" (dict "root" . "service" "quickwit-indexer" "ns" "quickwit"))) | quote }}
        level: Info
    local:
      level: Info
  cors:
    insecure: true
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
    endpoint: {{ $b.s3.endpoint | default (printf "http://%s:8333" (include "thorium.host" (dict "root" . "service" "seaweedfs" "ns" "seaweedfs"))) | quote }}
    region: {{ $b.s3.region | quote }}
    use_path_style: {{ $b.s3.usePathStyle }}
  scaler:
    crane:
      insecure: true
    k8s:
      clusters:
        {{ .Values.cluster.scaler.context | quote }}:
          alias: {{ .Values.cluster.scaler.alias | quote }}
          nodes: {{ .Values.cluster.scaler.nodes | toJson }}
redis:
  host: {{ $b.redis.host | default (include "thorium.host" (dict "root" . "service" "redis" "ns" "redis")) | quote }}
  port: {{ $b.redis.port }}
scylla:
  nodes: {{ $scyllaNodes | toJson }}
  replication: {{ $b.scylla.replication }}
elastic:
  node: {{ $b.elastic.node | default (printf "https://%s:9200" (include "thorium.host" (dict "root" . "service" "elastic-es-http" "ns" "elastic"))) | quote }}
  insecure_certificates: {{ $b.elastic.insecureCertificates }}
  results:
    repos: thorium_repo_results
    samples: thorium_sample_results
  tags:
    repos: thorium_repo_tags
    samples: thorium_sample_tags
{{- end -}}

{{/*
The final ThoriumCluster config: user config deep-merged over the generated defaults.
*/}}
{{- define "operator.config" -}}
{{- $defaults := include "operator.defaultConfig" . | fromYaml -}}
{{- $merged := mustMergeOverwrite $defaults (deepCopy .Values.cluster.config) -}}
{{- toYaml $merged -}}
{{- end -}}
