#!/usr/bin/env bash
# Checks that a redeploy keeps a running deployment's infra sizes and settings (the Redis PVC, the
# Scylla rack storage, resources, and developer mode, and the Elasticsearch node set) when this
# deploy's values render others, unless --reset-infra is given, and that it refuses to lower the
# Scylla CPU limit (its shard count) either way, or to deploy unchecked while Scylla runs. The live
# objects are YAML files read by a stub minikube, and the chart is rendered by the real helm and
# read by the real kubectl (offline, with --local).
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
    write_values_overlay infra_differences cpu_millicores refuse_scylla_cpu_decrease infra_compare_failed \
    keep_live_infra; do
    sed -n "/^${fn}() /,/^}/p" "$minithor" >> "$work/funcs.sh"
    grep -q "^${fn}() " "$minithor"
    check "minithor defines $fn" $?
done
sed -n '/^INFRA_LIVE_FIELDS=/,/^'"'"'$/p' "$minithor" >> "$work/funcs.sh"
# write the live objects of a deployment an older chart installed with smaller settings
#   $1 - the Elasticsearch config the live node set has, as YAML flow mapping (empty for none)
#   $2 - the Scylla rack's CPU limit (default 1, none for a rack without one)
write_live() {
    local config="" limits="cpu: \"${2:-1}\", memory: 3Gi"
    [ -z "$1" ] || config="config: $1"
    [ "${2:-}" != none ] || limits="memory: 3Gi"
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
spec:
  developerMode: false
  datacenter:
    racks:
      - name: rack1
        storage: { capacity: 8Gi }
        resources: { requests: { cpu: 500m, memory: "0" }, limits: { $limits } }
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
# a stub helm whose template fails when HELM_FAIL is all (every render) or second (the render with
# the kept settings' overlay), and that runs the real helm otherwise
mkdir -p "$work/helmbin"
cat > "$work/helmbin/helm" <<'STUB'
#!/usr/bin/env bash
if [ "$1" = template ] && { [ "${HELM_FAIL:-}" = all ] || { [ "${HELM_FAIL:-}" = second ] && [[ "$*" == *overlay.yaml* ]]; }; }; then
    echo "Error: stub render failure" >&2
    exit 1
fi
exec "$REAL_HELM" "$@"
STUB
chmod +x "$work/helmbin/helm"
real_helm="$(command -v helm)"
# run keep_live_infra as deploy does (in a || list, so set -e doesn't apply inside it), with
# minithor's development defaults as the values
#   $1 - true for --reset-infra; the rest are the user's --values/--set arguments
run_keep() {
    local reset=$1
    shift
    sed -n '/^embedded_values() {/,/^}/p' "$minithor" > "$work/embedded.sh"
    bash -c ". $work/embedded.sh; embedded_values" > "$work/defaults.yaml"
    PATH="$work/helmbin:$work/bin:$PATH" REAL_HELM="$real_helm" LIVE="$work/live" STUB_KUBECTL="$kubectl_bin" KUBECONFIG=/dev/null bash -c '
        set -euo pipefail
        . "$0"
        HELM=helm MINIKUBE_PROFILE=p THORIUM_RELEASE=thorium THORIUM_CHART="$1" THORIUM_CHART_ARGS=()
        THORIUM_NAMESPACE=thorium REDIS_NAMESPACE=redis SCYLLA_NAMESPACE=scylla ELASTIC_NAMESPACE=elastic
        helm_cmd() { helm "$@"; }
        keep_live_infra "$2" "$3" 2 -f "$4" "${@:5}" || exit 1' \
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
scylla_rack='{.spec.datacenter.racks[0].resources'
grep -q "infra.scylla.resources.limits.cpu: running 1, the values render 2" <<<"$out"
check "keep-live names the kept Scylla CPU limit" $?
check_test "the overlay keeps the Scylla CPU limit" "$(render_field ScyllaCluster "$scylla_rack.limits.cpu}" "${args[@]}")" = 1
check_test "the overlay keeps the Scylla memory limit" "$(render_field ScyllaCluster "$scylla_rack.limits.memory}" "${args[@]}")" = 3Gi
check_test "the overlay keeps the Scylla CPU request" "$(render_field ScyllaCluster "$scylla_rack.requests.cpu}" "${args[@]}")" = 500m
check_test "the overlay keeps the Scylla developer mode" "$(render_field ScyllaCluster '{.spec.developerMode}' "${args[@]}")" = false
grep -q '^    developerMode: false$' "$work/overlay.yaml"
check "the overlay writes developer mode as a boolean" $?
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

# raising the Scylla CPU limit is allowed, with --reset-infra or the user's --set
grep -q "infra.scylla.resources.limits.cpu: running 1, the values render 2" <<<"$out"
check "--reset-infra raises the Scylla CPU limit" $?
out="$(run_keep false --set infra.scylla.resources.limits.cpu=3 2>&1)"
check "a --set that raises the Scylla CPU limit succeeds" $?
grep -q "infra.scylla.resources.limits.cpu: running 1, your --values/--set render 3" <<<"$out"
check "a --set that raises the Scylla CPU limit is reported" $?

# lowering the Scylla CPU limit (and so its shard count) is refused, with --reset-infra or a --set
write_live "$base_config" 4
out="$(run_keep true 2>&1)"
rc=$?
[ "$rc" -ne 0 ] && grep -q "lower the running Scylla CPU limit (infra.scylla.resources.limits.cpu) from 4 to 2" <<<"$out" \
    && grep -q "shard" <<<"$out" && grep -q -- "--set infra.scylla.resources.limits.cpu=4" <<<"$out"
check "--reset-infra refuses to lower the Scylla CPU limit and says why" $?
write_live "$base_config"
out="$(run_keep false --set infra.scylla.resources.limits.cpu=500m 2>&1)"
rc=$?
[ "$rc" -ne 0 ] && grep -q "from 1 to 500m" <<<"$out"
check "a --set that lowers the Scylla CPU limit is refused" $?
out="$(run_keep false --set infra.scylla.resources.limits.cpu=1000m 2>&1)"
check "a --set that keeps the Scylla CPU limit in other units succeeds" $?
out="$(run_keep false --set infra.scylla.resources.limits.cpu=null 2>&1)"
rc=$?
[ "$rc" -ne 0 ] && grep -q "from 1 to no limit" <<<"$out"
check "a --set that removes the Scylla CPU limit is refused" $?

# a running Scylla without a CPU limit runs a shard count the limit doesn't set, so a deploy that
# adds one is refused; a plain redeploy keeps it without one
write_live "$base_config" none
out="$(run_keep false 2>&1)"
check "keep-live succeeds for a Scylla without a CPU limit" $?
check_test "the overlay keeps the Scylla rack without a CPU limit" -z "$(render_field ScyllaCluster "$scylla_rack.limits.cpu}" "${args[@]}")"
out="$(run_keep true 2>&1)"
rc=$?
[ "$rc" -ne 0 ] && grep -q "has no CPU limit" <<<"$out" && grep -q "shard" <<<"$out"
check "--reset-infra refuses to add a CPU limit to a Scylla without one and says why" $?
out="$(run_keep false --set infra.scylla.resources.limits.cpu=8 2>&1)"
rc=$?
[ "$rc" -ne 0 ] && grep -q "has no CPU limit" <<<"$out" && [ ! -e "$work/overlay.yaml" ]
check "a --set that adds a CPU limit to a Scylla without one is refused" $?

# with a ScyllaCluster running, a deploy that can't compare the settings refuses rather than
# rendering the chart's defaults over them
write_live "$base_config" 4
out="$(TMPDIR=/nonexistent run_keep false 2>&1)"
rc=$?
[ "$rc" -ne 0 ] && grep -q "ERROR: couldn't compare" <<<"$out" && [ ! -e "$work/overlay.yaml" ]
ok=$?
check "a deploy that can't make a temporary directory refuses ($rc: $(tr "\n" " " <<<"$out" | head -c 300))" "$ok"
out="$(HELM_FAIL=all run_keep false 2>&1)"
rc=$?
[ "$rc" -ne 0 ] && grep -q "ERROR: couldn't compare" <<<"$out" && grep -q "stub render failure" <<<"$out" \
    && [ ! -e "$work/overlay.yaml" ]
ok=$?
check "a deploy whose chart render fails refuses ($rc: $(tr "\n" " " <<<"$out" | head -c 300))" "$ok"
out="$(HELM_FAIL=second run_keep false 2>&1)"
rc=$?
[ "$rc" -ne 0 ] && grep -q "ERROR: couldn't compare" <<<"$out" && [ ! -e "$work/overlay.yaml" ]
ok=$?
check "a deploy whose render with the kept settings fails refuses ($rc: $(tr "\n" " " <<<"$out" | head -c 300))" "$ok"
# without a ScyllaCluster, a failed render only warns and keeps nothing
rm -f "$work/live/scylla/scyllacluster.yaml"
out="$(HELM_FAIL=all run_keep false 2>&1)"
rc=$?
[ "$rc" -eq 0 ] && grep -q "WARNING: couldn't compare" <<<"$out" && [ ! -e "$work/overlay.yaml" ]
ok=$?
check "a failed render without a ScyllaCluster warns and keeps nothing ($rc: $(tr "\n" " " <<<"$out" | head -c 300))" "$ok"

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
bash "$minithor" deploy --help | grep -q "lower the Scylla CPU limit"
check "deploy --help documents the Scylla CPU limit refusal" $?
# shellcheck disable=SC2016
grep -q 'keep_live_infra "$reset_infra" .* || exit 1$' "$minithor"
check "deploy stops when keep_live_infra refuses, as run_keep calls it" $?
exit "$fails"
