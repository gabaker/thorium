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
when it differs from the service (seaweedfs for s3).
Usage: include "infra.managed" (dict "root" $ "service" "s3" "key" "seaweedfs")
*/}}
{{- define "infra.managed" -}}
{{- $key := .key | default .service -}}
{{- if hasKey ((index .root.Values $key) | default dict) "enabled" -}}
{{- fail (printf "infra.%s.enabled is not a chart value; set global.managed.%s instead" $key .service) -}}
{{- end -}}
{{- include "thorium.managed" (dict "root" .root "service" .service) -}}
{{- end -}}

{{/*
Redis's redis.conf. The password isn't part of it: it includes auth.conf from the redis-auth
secret (rendered by the secrets subchart), which sets requirepass.
*/}}
{{- define "infra.redisConf" -}}
bind 0.0.0.0
protected-mode yes
port 6379
tcp-backlog 511
timeout 0
tcp-keepalive 300
daemonize no
supervised no
loglevel notice
logfile ""
databases 16
always-show-logo yes
save 900 1
save 300 10
save 60 10000
stop-writes-on-bgsave-error yes
rdbcompression yes
rdbchecksum yes
dbfilename dump2.rdb
dir /data
appendonly no
appendfilename "appendonly.aof"
appendfsync everysec
no-appendfsync-on-rewrite no
auto-aof-rewrite-percentage 100
auto-aof-rewrite-min-size 64mb
aof-load-truncated yes
aof-use-rdb-preamble yes
lua-time-limit 5000
slowlog-log-slower-than 10000
slowlog-max-len 128
latency-monitor-threshold 0
notify-keyspace-events ""
hash-max-ziplist-entries 512
hash-max-ziplist-value 64
list-max-ziplist-size -2
list-compress-depth 0
set-max-intset-entries 512
zset-max-ziplist-entries 128
zset-max-ziplist-value 64
hll-sparse-max-bytes 3000
stream-node-max-bytes 4096
stream-node-max-entries 100
activerehashing yes
hz 10
dynamic-hz yes
aof-rewrite-incremental-fsync yes
rdb-save-incremental-fsync yes
jemalloc-bg-thread yes
# the password (requirepass), from the redis-auth secret
include /etc/redis/auth/auth.conf
{{- end -}}
