#!/usr/bin/env bash
# Checks that a redeploy keeps a running deployment's infra sizes and settings (the Redis PVC, the
# Scylla rack storage, and the Elasticsearch node set) when this deploy's values render others,
# unless --reset-infra is given. The live objects are YAML files read by a stub minikube, and the
# chart is rendered by the real helm and read by the real kubectl (offline, with --local).
#   usage: minithor/tests/infra_live_test.sh   (KUBECTL=<path> picks the kubectl binary)
set -uo pipefail
repo="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
minithor="$repo/minithor/minithor"
chart="$repo/deploy/charts/thorium"
# shellcheck disable=SC2012
kubectl_bin="${KUBECTL:-$(command -v kubectl || ls "$HOME"/.minikube/cache/*/*/*/kubectl 2>/dev/null | tail -1)}"
if [ -z "$kubectl_bin" ] || ! command -v helm >/dev/null; then
    echo "SKIP infra_live_test.sh needs helm and kubectl (set KUBECTL to a kubectl binary)"
    exit 0
fi
work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT
fails=0
# report one check's result
#   $1 - the check's name, $2 - 0 when it passed
check() {
    if [ "$2" -eq 0 ]; then echo "ok   $1"; else echo "FAIL $1"; fails=$((fails + 1)); fi
}
# report whether a test expression holds
#   $1 - the check's name; the rest are the test's arguments
check_test() {
    local name=$1
    shift
    if [ "$@" ]; then check "$name" 0; else check "$name" 1; fi
}
# a stub minikube whose kubectl reads live objects from $LIVE/<namespace>/<resource>.yaml and runs
# every other kubectl command (annotate --local) with the real kubectl, offline
mkdir -p "$work/bin"
cat > "$work/bin/minikube" <<'STUB'
#!/usr/bin/env bash
[ "$1" = kubectl ] || exit 0
shift 4
ns=""
if [ "$1" = -n ]; then ns=$2; shift 2; fi
if [ "$1" = get ]; then
    file="$LIVE/$ns/${2%%.*}.yaml"
    [ -f "$file" ] || { echo "Error from server (NotFound)" >&2; exit 1; }
    [ "$5" = name ] && { echo "$2/$3"; exit 0; }
    exec "$STUB_KUBECTL" annotate --local -f "$file" stub=1 "${@:4}"
fi
exec "$STUB_KUBECTL" "$@"
STUB
chmod +x "$work/bin/minikube"
# load minithor's helpers without running its command dispatch
for fn in kctl helm_cmd yaml_str split_manifests infra_rendered_value infra_overlay_value \
    write_values_overlay infra_differences keep_live_infra; do
    sed -n "/^${fn}() /,/^}/p" "$minithor" >> "$work/funcs.sh"
    grep -q "^${fn}() " "$minithor"
    check "minithor defines $fn" $?
done
sed -n '/^INFRA_LIVE_FIELDS=/,/^'"'"'$/p' "$minithor" >> "$work/funcs.sh"
# write the live objects of a deployment an older chart installed with smaller settings
#   $1 - the Elasticsearch config the live node set has, as YAML flow mapping (empty for none)
write_live() {
    local config=""
    [ -z "$1" ] || config="config: $1"
    rm -rf "$work/live"
    mkdir -p "$work/live/redis" "$work/live/scylla" "$work/live/elastic"
    cat > "$work/live/redis/persistentvolumeclaim.yaml" <<EOF
apiVersion: v1
kind: PersistentVolumeClaim
metadata: { name: redis-persistent-storage-claim, namespace: redis }
spec: { resources: { requests: { storage: 4Gi } } }
EOF
    cat > "$work/live/scylla/scyllacluster.yaml" <<EOF
apiVersion: scylla.scylladb.com/v1
kind: ScyllaCluster
metadata: { name: scylla, namespace: scylla }
spec: { datacenter: { racks: [ { name: rack1, storage: { capacity: 8Gi } } ] } }
EOF
    cat > "$work/live/elastic/elasticsearch.yaml" <<EOF
apiVersion: elasticsearch.k8s.elastic.co/v1
kind: Elasticsearch
metadata: { name: elastic, namespace: elastic }
spec:
  nodeSets:
    - name: default
      count: 1
      $config
      podTemplate:
        spec:
          containers:
            - name: elasticsearch
              env: [ { name: ES_JAVA_OPTS, value: -Xms512m -Xmx512m } ]
              resources: { limits: { cpu: "1", memory: 1536Mi } }
      volumeClaimTemplates:
        - metadata: { name: elasticsearch-data }
          spec: { resources: { requests: { storage: 8Gi } } }
EOF
}
# run keep_live_infra as deploy does, with minithor's development defaults as the values
#   $1 - true for --reset-infra; the rest are the user's --values/--set arguments
run_keep() {
    local reset=$1
    shift
    sed -n '/^embedded_values() {/,/^}/p' "$minithor" > "$work/embedded.sh"
    bash -c ". $work/embedded.sh; embedded_values" > "$work/defaults.yaml"
    PATH="$work/bin:$PATH" LIVE="$work/live" STUB_KUBECTL="$kubectl_bin" KUBECONFIG=/dev/null bash -c '
        set -euo pipefail
        . "$0"
        HELM=helm MINIKUBE_PROFILE=p THORIUM_RELEASE=thorium THORIUM_CHART="$1" THORIUM_CHART_ARGS=()
        THORIUM_NAMESPACE=thorium REDIS_NAMESPACE=redis SCYLLA_NAMESPACE=scylla ELASTIC_NAMESPACE=elastic
        helm_cmd() { helm "$@"; }
        keep_live_infra "$2" "$3" 2 -f "$4" "${@:5}"' \
        "$work/funcs.sh" "$chart" "$reset" "$work/overlay.yaml" "$work/defaults.yaml" "$@"
}
# render one field of the chart with the given values arguments
#   $1 - the Kind, $2 - the JSONPath; the rest are helm values arguments
render_field() {
    local kind=$1 path=$2
    shift 2
    helm template thorium "$chart" -n thorium --set secrets.renderOnly=true "$@" > "$work/r.yaml"
    awk -v k="kind: $kind" 'BEGIN { RS = "\n---\n" } index($0, "\n" k "\n") { print; exit }' "$work/r.yaml" > "$work/r1.yaml"
    KUBECONFIG=/dev/null "$kubectl_bin" annotate --local -f "$work/r1.yaml" t=1 -o "jsonpath=$path" 2>/dev/null
}
base_config='{ node.store.allow_mmap: true, http.max_content_length: 1024mb, cluster.routing.allocation.disk.threshold_enabled: false }'
es_container='{.spec.nodeSets[0].podTemplate.spec.containers[?(@.name=="elasticsearch")]'

# keep-live: every differing setting is written to the overlay and printed
write_live "$base_config"
out="$(run_keep false 2>&1)"
check "keep-live succeeds" $?
grep -q "infra.redis.storage: running 4Gi, the values render 16Gi" <<<"$out"
check "keep-live names each kept setting with both values" $?
grep -q -- "--reset-infra" <<<"$out" && grep -q "storage: \"4Gi\"" <<<"$out"
check "keep-live prints the overlay and how to opt out" $?
check_test "keep-live writes the overlay" -s "$work/overlay.yaml"
args=(-f "$work/defaults.yaml" -f "$work/overlay.yaml")
check_test "the overlay keeps the Redis PVC size" "$(render_field PersistentVolumeClaim '{.spec.resources.requests.storage}' "${args[@]}")" = 4Gi
check_test "the overlay keeps the Scylla rack storage" "$(render_field ScyllaCluster '{.spec.datacenter.racks[0].storage.capacity}' "${args[@]}")" = 8Gi
check_test "the overlay keeps the Elasticsearch storage" "$(render_field Elasticsearch '{.spec.nodeSets[0].volumeClaimTemplates[0].spec.resources.requests.storage}' "${args[@]}")" = 8Gi
check_test "the overlay keeps the Elasticsearch heap" "$(render_field Elasticsearch "$es_container.env[?(@.name==\"ES_JAVA_OPTS\")].value}" "${args[@]}")" = "-Xms512m -Xmx512m"
check_test "the overlay keeps the Elasticsearch memory limit" "$(render_field Elasticsearch "$es_container.resources.limits.memory}" "${args[@]}")" = 1536Mi
# an unchanged setting (the node count, the config) is not written
! grep -q "count:" "$work/overlay.yaml" && ! grep -q "config:" "$work/overlay.yaml"
check "settings that already match are left out of the overlay" $?

# a config key the values add but the live node set lacks is dropped with a null
write_live '{ node.store.allow_mmap: true, http.max_content_length: 1024mb }'
run_keep false > /dev/null 2>&1
grep -q '"cluster.routing.allocation.disk.threshold_enabled":null' "$work/overlay.yaml"
check "the overlay nulls config keys the live node set lacks" $?
config="$(render_field Elasticsearch '{.spec.nodeSets[0].config}' "${args[@]}")"
check_test "the overlay keeps the live Elasticsearch config (renders $config)" "$config" = '{"http.max_content_length":"1024mb","node.store.allow_mmap":true}'

# a live node set with no config gets an overlay helm accepts, nulling every config key
write_live ""
run_keep false > /dev/null 2>&1
check "keep-live succeeds when the live node set has no config" $?
grep -q '^    config: {"cluster.routing.allocation.disk.threshold_enabled":null,"http.max_content_length":null,"node.store.allow_mmap":null}$' "$work/overlay.yaml"
check "the overlay nulls every config key in a mapping when the live node set has none" $?
helm template thorium "$chart" -n thorium --set secrets.renderOnly=true "${args[@]}" > /dev/null 2> "$work/helm.err" \
    && ! grep -q . "$work/helm.err"
rc=$?
check "helm takes the overlay's config as a mapping ($(head -c 200 "$work/helm.err"))" "$rc"

# the user's own --set still wins over the kept setting, and deploy says so
write_live "$base_config"
out="$(run_keep false --set infra.redis.storage=20Gi 2>&1)"
grep -q "infra.redis.storage: running 4Gi, your --values/--set render 20Gi" <<<"$out"
check "a --set that changes a kept setting is reported" $?

# --reset-infra writes no overlay but lists what changes
out="$(run_keep true 2>&1)"
check "--reset-infra succeeds" $?
[ ! -e "$work/overlay.yaml" ] && grep -q "infra.scylla.storage: running 8Gi, the values render 32Gi" <<<"$out"
check "--reset-infra writes no overlay and lists the changes" $?

# a first install (no live objects) writes no overlay and prints nothing
rm -rf "$work/live"
mkdir -p "$work/live"
out="$(run_keep false 2>&1)"
check_test "a first install writes no overlay" ! -e "$work/overlay.yaml" -a -z "$out"

# deploy keeps the live settings unless --reset-infra, with the overlay below the user's values
# (the patterns are literal minithor text)
# shellcheck disable=SC2016
grep -q -- '--reset-infra)' "$minithor" && grep -q 'keep_live_infra "$reset_infra"' "$minithor"
check "deploy parses --reset-infra and calls keep_live_infra" $?
bash "$minithor" deploy --help | grep -q -- "--reset-infra"
check "deploy --help documents --reset-infra" $?
exit "$fails"
