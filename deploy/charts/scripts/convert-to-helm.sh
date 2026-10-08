#!/usr/bin/env bash
# Convert a Thorium deployment made by the pre-Helm minithor or megathor scripts so the Helm charts
# can take it over. Run it once per deployment, before deploying the thorium chart (directly or
# with the new minithor or megathor) over it; see "Converting a Pre-Helm Deployment" in the
# Thorium deployment docs.
#
#   deploy/charts/scripts/convert-to-helm.sh [options]
#
#   --namespace-prefix <prefix>  The deployment's namespace prefix: minithor's --instance or
#                                megathor's namespace_prefix (default: none, namespace thorium)
#   --context <name>             The kubectl context to use (default: the current context)
#   --release <name>             The Helm release that will own Thorium (default: thorium)
#   --chart <path|oci-ref>       The thorium chart that will be deployed, rendered to find the
#                                objects it would collide with (default: ../thorium next to this
#                                script)
#   --chart-version <version>    The chart version for an oci:// chart
#   --values <file>              Extra values the deployment will use (such as minithor's or
#                                megathor's), rendered before the converted values (which win,
#                                as when deploying) when looking for collisions (repeatable)
#   --values-out <file>          Where to write the values for the thorium chart
#                                (default: ./thorium-converted-values.yaml)
#   --admin-user <name>          The existing Thorium admin to record in thorium-admin
#   --admin-password <password>  Its password. A command-line argument is visible to every local
#                                user in the process list, so prefer the THORIUM_ADMIN_PASSWORD
#                                environment variable or --admin-password-stdin (default: read
#                                from minithor-credentials when minithor saved one; without any
#                                of these the admin bootstrap is off)
#   --admin-password-stdin       Read the admin password from the first line of stdin (stdin is
#                                then no terminal to confirm on, so also pass --yes or --dry-run)
#   --dry-run                    Show what would change and write the values file, changing
#                                nothing in the cluster
#   --yes                        Don't ask for confirmation
#   --cleanup                    After the operator finished upgrading the converted deployment,
#                                remove the legacy RBAC kept for its old scaler
#   -h, --help                   Show this help
#
# The conversion keeps every database, credential, and Thorium component running. It first
# writes chart values that treat the existing databases as external services, carry the
# credentials and settings over (including the image registry and its --docker-config pull
# login, and an nginx Ingress or Traefik IngressRoute with its TLS secret), and turn off
# Quickwit, the registry, and the toolbox import. Then it:
#   1. stops every pre-Helm Thorium operator (they would fight the new one over the cluster)
#   2. saves the ThoriumCluster in the thorium-legacy-cluster Secret, applies the chart's
#      ThoriumCluster CRD (Helm never updates an existing one), and records upgrade revision
#      2026-10-v01 in the thorium-upgrade-state ConfigMap
#   3. moves the old scaler's ClusterRole aside when the chart renders one with the same name
#   4. hands objects the chart renders identically to the Helm release and deletes the ones the
#      chart replaces: the old operator, the legacy ingress, and the thorium account's
#      non-expiring token Secret, which nothing uses (Jaeger keeps running, since the converted
#      values turn the chart's own tracing off)
#   5. deletes the ThoriumCluster without letting anything clean up after it, so the chart
#      creates it again from the values (its components keep running and are adopted)
# Every step checks whether it already ran, so a failed run can simply be run again; once the
# chart has taken the ThoriumCluster over, a run only writes the values again.
set -euo pipefail

script_dir="$(cd "$(dirname "$0")" && pwd)"
prefix=""
context=""
release="thorium"
chart="$script_dir/../thorium"
chart_version=""
values_out="./thorium-converted-values.yaml"
admin_user=""
admin_password="${THORIUM_ADMIN_PASSWORD:-}"
admin_password_stdin=false
dry_run=false
assume_yes=false
cleanup=false
extra_values=()

# The revision every converted deployment starts from
BASELINE_REVISION="2026-10-v01"
# Where the pre-Helm ThoriumCluster is saved
SNAPSHOT_SECRET="thorium-legacy-cluster"
# Where the upgrade revision is recorded
STATE_CONFIG_MAP="thorium-upgrade-state"
# The non-expiring service account token Secret the pre-Helm scripts created for the thorium
# account; nothing reads it, so it is deleted rather than handed to Helm
LEGACY_TOKEN_SECRET="thorium-account-token"
# The ClusterRole the pre-Helm scripts bound the old operator and scaler to
LEGACY_ROLE="thorium-operator"
# Where the old scaler's rules are moved when the chart renders a ClusterRole named LEGACY_ROLE
LEGACY_COPY_ROLE="thorium-legacy-components"
# Marks the objects this script created so --cleanup can find them
LEGACY_LABEL="thorium.sandia.gov/legacy-components"

usage() {
    awk 'NR > 1 && /^#/ { sub(/^# ?/, ""); print; next } NR > 1 { exit }' "$0"
}

# read a flag's value, failing when it is missing
need_value() {
    if [ "$2" -lt 2 ]; then
        echo "ERROR: $1 needs a value" >&2
        exit 1
    fi
}

while [ $# -gt 0 ]; do
    case "$1" in
        --namespace-prefix) need_value "$1" $#; prefix="$2"; shift ;;
        --context)          need_value "$1" $#; context="$2"; shift ;;
        --release)          need_value "$1" $#; release="$2"; shift ;;
        --chart)            need_value "$1" $#; chart="$2"; shift ;;
        --chart-version)    need_value "$1" $#; chart_version="$2"; shift ;;
        --values-out)       need_value "$1" $#; values_out="$2"; shift ;;
        --admin-user)       need_value "$1" $#; admin_user="$2"; shift ;;
        --admin-password)   need_value "$1" $#; admin_password="$2"; shift ;;
        --admin-password-stdin) admin_password_stdin=true ;;
        --dry-run)          dry_run=true ;;
        --yes)              assume_yes=true ;;
        --cleanup)          cleanup=true ;;
        --values)           need_value "$1" $#; extra_values+=(-f "$2"); shift ;;
        -h|--help)          usage; exit 0 ;;
        *) echo "ERROR: unknown option $1" >&2; usage >&2; exit 1 ;;
    esac
    shift
done

if [ -n "$prefix" ] && ! [[ "$prefix" =~ ^[a-z0-9]([-a-z0-9]{0,30}[a-z0-9])?$ ]]; then
    echo "ERROR: --namespace-prefix must be a lowercase DNS label of at most 32 characters" >&2
    exit 1
fi
if [ "$admin_password_stdin" = true ]; then
    IFS= read -r admin_password || true
fi
if { [ -n "$admin_user" ] && [ -z "$admin_password" ]; } || { [ -z "$admin_user" ] && [ -n "$admin_password" ]; }; then
    echo "ERROR: --admin-user and its password (--admin-password, --admin-password-stdin, or" \
        "THORIUM_ADMIN_PASSWORD) must be given together" >&2
    exit 1
fi

namespace="${prefix:+$prefix-}thorium"
credentials_namespace="${prefix:+$prefix-}minithor"

# run kubectl against the chosen context
kc() {
    kubectl ${context:+--context "$context"} "$@"
}

# print a step heading
step() {
    echo
    echo "==> $1"
}

# run a command that changes the cluster, or only describe it during a dry run
change() {
    local description="$1"
    shift
    if [ "$dry_run" = true ]; then
        echo "    would $description"
    else
        echo "    $description"
        "$@"
    fi
}

# whether an object is owned by our Helm release
#
#   helm_owned <object json>
helm_owned() {
    [ "$(jq -r '.metadata.annotations["meta.helm.sh/release-name"] // ""' <<< "$1")" = "$release" ]
}

# whether an object is owned by any Helm release
#
#   helm_managed <object json>
helm_managed() {
    [ -n "$(jq -r '.metadata.annotations["meta.helm.sh/release-name"] // ""' <<< "$1")" ]
}

# get an object as JSON, or nothing when it doesn't exist
#
#   get_json <kind> <name> [namespace]
get_json() {
    kc get "$1" "$2" ${3:+-n "$3"} -o json 2>/dev/null || true
}

for tool in kubectl helm jq; do
    if ! command -v "$tool" >/dev/null 2>&1; then
        echo "ERROR: $tool is required" >&2
        exit 1
    fi
done
if ! kc get namespace "$namespace" >/dev/null 2>&1; then
    echo "ERROR: namespace $namespace doesn't exist (check --namespace-prefix and --context)" >&2
    exit 1
fi

########################################
# Cleanup after a finished conversion
########################################
if [ "$cleanup" = true ]; then
    step "Checking that the converted deployment finished upgrading"
    revision=$(kc get configmap "$STATE_CONFIG_MAP" -n "$namespace" \
        -o jsonpath='{.metadata.labels.thorium\.sandia\.gov/state-revision}' 2>/dev/null || true)
    phase=$(kc get thoriumclusters -n "$namespace" -o jsonpath='{.items[0].status.phase}' 2>/dev/null || true)
    if [ -z "$revision" ] || [ "$revision" = "$BASELINE_REVISION" ] || [ "$phase" != Ready ]; then
        echo "ERROR: the deployment is at revision ${revision:-unknown} in phase ${phase:-unknown};" \
            "run --cleanup once the operator upgraded it and reports Ready" >&2
        exit 1
    fi
    echo "    revision $revision, phase $phase"
    step "Removing the legacy RBAC of the old scaler"
    # bindings of this deployment's thorium service account to a legacy role
    bindings=$(kc get clusterrolebindings -o json | jq -r --arg ns "$namespace" \
        --arg legacy "$LEGACY_ROLE" --arg copy "$LEGACY_COPY_ROLE" '
        .items[]
        | select((.metadata.annotations["meta.helm.sh/release-name"] // "") == "")
        | select(.roleRef.name == $legacy or .roleRef.name == $copy)
        | select(any(.subjects[]?; .kind == "ServiceAccount" and .name == "thorium" and .namespace == $ns))
        | .metadata.name')
    for binding in $bindings; do
        change "delete ClusterRoleBinding $binding" kc delete clusterrolebinding "$binding"
    done
    # drop the legacy roles once nothing outside Helm binds to them
    for role in "$LEGACY_COPY_ROLE" "$LEGACY_ROLE"; do
        role_json=$(get_json clusterrole "$role")
        if [ -z "$role_json" ] || helm_managed "$role_json"; then
            continue
        fi
        users=$(kc get clusterrolebindings -o json | jq -r --arg role "$role" \
            '[.items[] | select(.roleRef.name == $role)] | length')
        if [ "$users" = 0 ]; then
            change "delete ClusterRole $role" kc delete clusterrole "$role"
        else
            echo "    keeping ClusterRole $role, which $users other binding(s) still use"
        fi
    done
    echo
    echo "Cleanup complete. The saved ThoriumCluster is kept in Secret $namespace/$SNAPSHOT_SECRET;"
    echo "delete it once you no longer need to roll back."
    exit 0
fi

########################################
# Read the pre-Helm deployment
########################################
step "Reading the pre-Helm ThoriumCluster in $namespace"
snapshot_json=$(get_json secret "$SNAPSHOT_SECRET" "$namespace")
converted=false
if [ -n "$snapshot_json" ]; then
    # a previous run already saved (and may have deleted) the ThoriumCluster
    cluster_json=$(jq -r '.data["cluster.json"]' <<< "$snapshot_json" | base64 -d)
    echo "    using the copy a previous run saved in Secret $SNAPSHOT_SECRET"
    # the chart may already have created the ThoriumCluster again, which must be left alone
    live_cluster=$(get_json thoriumcluster "$(jq -r .metadata.name <<< "$cluster_json")" "$namespace")
    if [ -n "$live_cluster" ] && { helm_owned "$live_cluster" \
        || [ "$(jq '.spec.config_secrets // [] | length' <<< "$live_cluster")" != 0 ]; }; then
        converted=true
    fi
else
    clusters=$(kc get thoriumclusters -n "$namespace" -o json)
    count=$(jq '.items | length' <<< "$clusters")
    if [ "$count" != 1 ]; then
        echo "ERROR: expected one ThoriumCluster in $namespace but found $count" >&2
        exit 1
    fi
    cluster_json=$(jq '.items[0]' <<< "$clusters")
    if [ "$(jq '.spec.config_secrets // [] | length' <<< "$cluster_json")" != 0 ] \
        || helm_owned "$cluster_json"; then
        echo "ERROR: ThoriumCluster $(jq -r .metadata.name <<< "$cluster_json") is already deployed by" \
            "the Helm charts, so there is nothing to convert" >&2
        exit 1
    fi
fi
cluster_name=$(jq -r .metadata.name <<< "$cluster_json")
echo "    ThoriumCluster $cluster_name"
# every credential the chart must be given so it never generates a new one
missing=$(jq -r '.spec.config as $c | [
    ["thorium.secret_key", $c.thorium.secret_key], ["thorium.s3.access_key", $c.thorium.s3.access_key],
    ["thorium.s3.secret_token", $c.thorium.s3.secret_token], ["redis.password", $c.redis.password],
    ["redis.host", $c.redis.host], ["scylla.auth.password", $c.scylla.auth.password],
    ["scylla.nodes", ($c.scylla.nodes // [] | first)], ["elastic.password", $c.elastic.password],
    ["elastic.node", $c.elastic.node], ["thorium.s3.endpoint", $c.thorium.s3.endpoint]
    ] | map(select((.[1] // "") == "") | .[0]) | join(", ")' <<< "$cluster_json")
if [ -n "$missing" ]; then
    echo "ERROR: the ThoriumCluster's spec.config is missing $missing, which the conversion" \
        "carries over from it" >&2
    exit 1
fi

# the existing admin login, from the flags or what minithor saved
if [ -z "$admin_user" ]; then
    admin_user=$(kc get secret minithor-credentials -n "$credentials_namespace" \
        -o go-template='{{with index .data "admin-user"}}{{. | base64decode}}{{end}}' 2>/dev/null || true)
    admin_password=$(kc get secret minithor-credentials -n "$credentials_namespace" \
        -o go-template='{{with index .data "admin-password"}}{{. | base64decode}}{{end}}' 2>/dev/null || true)
    if [ -n "$admin_user" ]; then
        echo "    using the admin login minithor saved in $credentials_namespace/minithor-credentials"
    fi
fi
if [ -z "$admin_user" ] || [ -z "$admin_password" ]; then
    admin_user=""
    admin_password=""
    echo "    no admin login given or saved, so the chart's admin bootstrap is turned off"
fi
# the login banner the UI shows
banner=$(kc get configmap banner -n "$namespace" \
    -o go-template='{{with .data}}{{with index . "banner.txt"}}{{.}}{{end}}{{end}}' 2>/dev/null || true)

########################################
# Build the chart values
########################################
step "Writing the thorium chart values to $values_out"
# the pre-Helm minithor's nginx Ingress, carried over so a plain helm deploy stays reachable
# (read from the copy a previous run saved once the Ingress itself was deleted)
legacy_ingress=$(get_json ingress thorium-default-ingress "$namespace")
if [ -z "$legacy_ingress" ] && [ -n "$snapshot_json" ]; then
    legacy_ingress=$(jq -r '.data["ingress.json"] // empty' <<< "$snapshot_json" | base64 -d)
fi
# the pre-Helm megathor's Traefik IngressRoute and default TLSStore, carried over the same way
# when there is no nginx Ingress; a TLSStore another Helm release owns stays with it, so the
# chart then doesn't render its own
legacy_route=$(get_json ingressroute thorium-ingress "$namespace")
if [ -z "$legacy_route" ] && [ -n "$snapshot_json" ]; then
    legacy_route=$(jq -r '.data["ingressroute.json"] // empty' <<< "$snapshot_json" | base64 -d)
fi
legacy_store=$(get_json tlsstore default "$namespace")
if [ -n "$legacy_store" ] && helm_managed "$legacy_store"; then
    legacy_store=""
elif [ -z "$legacy_store" ] && [ -n "$snapshot_json" ]; then
    legacy_store=$(jq -r '.data["tlsstore.json"] // empty' <<< "$snapshot_json" | base64 -d)
fi
ingress=$(jq -nc --argjson nginx "${legacy_ingress:-null}" --argjson route "${legacy_route:-null}" \
    --argjson store "${legacy_store:-null}" '
    if $nginx != null then {type: "nginx",
        className: ($nginx.spec.ingressClassName // "nginx"), host: ($nginx.spec.rules[0].host // ""),
        tls: {secretName: ($nginx.spec.tls[0].secretName // "")}}
    elif $route != null then
        # megathor matched Host(`<name>`) (in any case) or every path when it had no hostname
        ([$route.spec.routes[]?.match // "" | capture("host\\(`(?<host>[^`]+)`\\)"; "i").host]
          | first // "") as $host
        | ($store.spec.defaultCertificate.secretName // "") as $store_secret
        | ($route.spec.tls.secretName // $store_secret) as $secret
        | {type: "traefik", host: $host,
           tls: {secretName: $secret, traefikDefaultStore: ($store_secret != "" and $store_secret == $secret)}}
    else {} end')
# the pre-Helm scripts' --docker-config pull Secret, which the components pulled the Thorium image
# with through the thorium service account; without registry_auth the operator doesn't render
# registry-token, yet it deletes a Secret by that name on uninstall, so its contents are carried
# into the chart's own thorium-image-pull Secret instead of being referenced by name
legacy_pull=""
if [ "$(jq '.spec.registry_auth // {} | length' <<< "$cluster_json")" = 0 ]; then
    pull_json=$(get_json secret registry-token "$namespace")
    if [ -n "$pull_json" ] && ! helm_managed "$pull_json" \
        && [ "$(jq -r '.type' <<< "$pull_json")" = kubernetes.io/dockerconfigjson ]; then
        legacy_pull=$(jq -r '.data[".dockerconfigjson"] // empty' <<< "$pull_json" | base64 -d)
    elif [ -n "$snapshot_json" ]; then
        legacy_pull=$(jq -r '.data["registry-token.json"] // empty' <<< "$snapshot_json" | base64 -d)
    fi
    # an empty config (megathor writes one without thorium_image_pull_secret) holds no login
    if [ "$(jq '.auths // {} | length' <<< "${legacy_pull:-null}" 2>/dev/null || echo 0)" = 0 ]; then
        legacy_pull=""
    fi
fi
# the password reaches jq through its environment, which other users can't read, rather than as
# an argument visible in the process list
values=$(ADMIN_PASSWORD="$admin_password" PULL_CONFIG="$legacy_pull" jq --arg admin_user "$admin_user" \
    --arg banner "$banner" --argjson ingress "$ingress" '
    .spec as $s | $s.config as $c
    # the scaler cluster the chart renders: the primary cluster when it is listed, else the first
    | ($c.thorium.scaler.k8s.clusters // {} | to_entries) as $all_clusters
    | ($all_clusters | map(select(.key == $c.thorium.scaler.k8s.primary_cluster))
       + $all_clusters) as $clusters
    # the chart renders an external tracing endpoint only when one is given (Quickwit is off)
    | ($c.thorium.tracing.external.Grpc.endpoint // "") as $tracing
    # the pre-Helm operator stored every config default, so insecure_certificates is always set;
    # the pre-Helm search streamer never verified Elastic either way, so only a configured CA
    # turns verification on
    | (if $c.elastic.cert_validation then false else true end) as $insecure
    # registry-token is the name of the pull Secret the operator renders, so it is never listed
    | (($s.image_pull_secrets // []) - ["registry-token"]) as $pull_secrets
    | {
        global: {
          managed: {scylla: false, elastic: false, redis: false, s3: false, postgres: false},
          quickwit: {enabled: false},
          elasticIndices: {
            sampleResults: ($c.elastic.results.samples // "thorium_sample_results"),
            repoResults: ($c.elastic.results.repos // "thorium_repo_results"),
            sampleTags: ($c.elastic.tags.samples // "thorium_sample_tags"),
            repoTags: ($c.elastic.tags.repos // "thorium_repo_tags")
          }
        },
        infra: {registry: {enabled: false}},
        secrets: {
          credentials: ({
            thoriumSecretKey: $c.thorium.secret_key,
            redisPassword: $c.redis.password,
            scyllaUsername: ($c.scylla.auth.username // "thorium"),
            scyllaPassword: $c.scylla.auth.password,
            elasticUsername: ($c.elastic.username // "thorium"),
            elasticPassword: $c.elastic.password,
            s3AccessKey: $c.thorium.s3.access_key,
            s3SecretToken: $c.thorium.s3.secret_token
          } + (if $admin_user != "" then {adminUsername: $admin_user, adminPassword: $ENV.ADMIN_PASSWORD}
               else {} end))
        },
        operator: ({
          cluster: ({
            name: .metadata.name,
            imagePullPolicy: ($s.image_pull_policy // ""),
            components: {
              api: ($s.components.api // {}),
              scaler: ($s.components.scaler // null),
              baremetal_scaler: ($s.components.baremetal_scaler // null),
              search_streamer: ($s.components.search_streamer // null),
              event_handler: ($s.components.event_handler // null)
            },
            registryAuth: ($s.registry_auth // {}),
            namespaceBlacklist: ($c.thorium.namespace_blacklist // []),
            bootstrap: {
              admin: {enabled: ($admin_user != "")},
              scylla: {enabled: false},
              elastic: {enabled: false}
            },
            config: ($c
              | del(.thorium.secret_key, .thorium.s3.access_key, .thorium.s3.secret_token,
                    .thorium.s3.endpoint, .thorium.s3.region, .thorium.s3.use_path_style,
                    .redis.host, .redis.port, .redis.password,
                    .scylla.nodes, .scylla.replication, .scylla.auth,
                    .elastic.node, .elastic.username, .elastic.password,
                    .elastic.insecure_certificates, .elastic.results, .elastic.tags,
                    .thorium.namespace_blacklist)
              # the chart renders the Grpc endpoint, so other tracing settings without one are
              # dropped along with it
              | if $tracing != "" then del(.thorium.tracing.external.Grpc.endpoint)
                elif $c.thorium.tracing.external.Grpc then del(.thorium.tracing.external)
                else . end
              | if $clusters | length > 0 then del(.thorium.scaler.k8s.clusters[$clusters[0].key])
                  | del(.thorium.scaler.k8s.primary_cluster)
                else . end
              | del(.. | nulls))
          } + (if $clusters | length > 0 then {scaler: {k8s: {
                 context: $clusters[0].key,
                 alias: ($clusters[0].value.alias // "thorium"),
                 nodes: ($clusters[0].value.nodes // [])}}} else {} end)),
          backends: {
            redis: {host: $c.redis.host, port: ($c.redis.port // 6379)},
            scylla: {nodes: $c.scylla.nodes, replication: ($c.scylla.replication // 1)},
            elastic: {node: $c.elastic.node, insecureCertificates: $insecure},
            s3: {
              endpoint: $c.thorium.s3.endpoint,
              region: ($c.thorium.s3.region // "us-east-1"),
              usePathStyle: ($c.thorium.s3.use_path_style // true)
            },
            tracing: {grpcEndpoint: $tracing}
          },
          toolbox: {enabled: false, url: "", json: ""}
        } + (if $banner != "" then {banner: $banner} else {} end)
          + (if $s.registry then {image: {repository: $s.registry}} else {} end)
          + (if $pull_secrets != [] then {imagePullSecrets: $pull_secrets} else {} end)
          + (if $ENV.PULL_CONFIG != "" then {createImagePullSecret: true,
               dockerConfigJson: $ENV.PULL_CONFIG} else {} end)
          + (if $ingress != {} then {ingress: $ingress} else {} end))
      }' <<< "$cluster_json")
# the values hold credentials, so only the current user may read them
rm -f "$values_out"
(umask 077 && jq . <<< "$values" > "$values_out")
echo "    written (JSON is valid YAML); it holds credentials, so keep it private"

if [ "$converted" = true ]; then
    echo
    echo "ThoriumCluster $cluster_name is already deployed by the Helm charts, so nothing else was"
    echo "done. Run with --cleanup once it reports Ready to remove the old scaler's RBAC."
    exit 0
fi

########################################
# Find what the chart would collide with
########################################
step "Rendering $chart to find the objects it would collide with"
# the converted values go last so they win, as when the chart is deployed with them
rendered=$(helm template "$release" "$chart" -n "$namespace" ${chart_version:+--version "$chart_version"} \
    ${extra_values[@]+"${extra_values[@]}"} -f "$values_out" --set secrets.renderOnly=true)
# list each rendered object as "<kind> <namespace> <name>"
objects=$(awk '
    function emit() { if (kind != "") print kind, (ns == "" ? "-" : ns), name }
    /^---/ { emit(); kind = ""; name = ""; ns = ""; inmeta = 0; next }
    /^kind:/ { kind = $2; next }
    /^metadata:/ { inmeta = 1; next }
    /^[^ #]/ { inmeta = 0 }
    inmeta && /^  name:/ { name = $2 }
    inmeta && /^  namespace:/ { ns = $2 }
    END { emit() }' <<< "$rendered" | tr -d '"' | sort -u)
adopt=()
replace=()
conflicts=()
while read -r kind ns name; do
    [ -n "$kind" ] || continue
    # namespaced objects rendered without a namespace go to the release namespace
    if [ "$ns" = "-" ]; then
        case "$kind" in
            Namespace|ClusterRole|ClusterRoleBinding|CustomResourceDefinition|PriorityClass|StorageClass) ns="" ;;
            *) ns="$namespace" ;;
        esac
    fi
    live=$(get_json "$kind" "$name" "$ns")
    if [ -z "$live" ] || helm_owned "$live"; then
        continue
    fi
    case "$kind/$name" in
        # rendered the same as the pre-Helm scripts made them
        Namespace/*|ServiceAccount/thorium|ConfigMap/banner)
            adopt+=("$kind ${ns:--} $name") ;;
        # replaced by the chart's own
        Deployment/operator|ClusterRole/"$LEGACY_ROLE"|ThoriumCluster/"$cluster_name")
            replace+=("$kind ${ns:--} $name") ;;
        # the legacy Traefik objects the chart's own ingress replaces; another Helm release's
        # default TLSStore is left to it
        IngressRoute/thorium-ingress|TLSStore/default)
            if helm_managed "$live"; then
                conflicts+=("$kind ${ns:-(cluster)} $name (owned by Helm release $(jq -r \
                    '.metadata.annotations["meta.helm.sh/release-name"]' <<< "$live"))")
            else
                replace+=("$kind ${ns:--} $name")
            fi ;;
        *) conflicts+=("$kind ${ns:-(cluster)} $name") ;;
    esac
done <<< "$objects"
if [ "${#conflicts[@]}" -gt 0 ]; then
    echo "ERROR: the chart renders these objects, which already exist outside Helm and that the" \
        "conversion doesn't know how to hand over:" >&2
    printf '    %s\n' "${conflicts[@]}" >&2
    echo "Delete or rename them (after checking what they hold) and run the conversion again." >&2
    exit 1
fi
for entry in ${adopt[@]+"${adopt[@]}"}; do echo "    adopt: $entry"; done
for entry in ${replace[@]+"${replace[@]}"}; do echo "    replace: $entry"; done

# the pre-Helm operators: the Deployment named operator, running as the thorium service account
operators=$(kc get deployments -A -l app=operator -o json | jq -r --arg release "$release" '
    .items[]
    | select(.spec.template.spec.serviceAccountName == "thorium")
    | select((.metadata.annotations["meta.helm.sh/release-name"] // "") != $release)
    | "\(.metadata.namespace) \(.metadata.name) \(.spec.replicas)"')

# the legacy ingress objects route the same hosts the chart's ingress will, and the thorium
# account's non-expiring token is unused; objects any Helm release owns are left alone, and ones
# the chart replaces are already deleted with the rest of what it replaces
legacy_objects=()
for legacy in "Ingress thorium-default-ingress" "IngressRoute thorium-ingress" \
    "Middleware ui-prefix-prepend" "TLSStore default" "Secret $LEGACY_TOKEN_SECRET"; do
    read -r kind name <<< "$legacy"
    if printf '%s\n' ${replace[@]+"${replace[@]}"} | grep -qx "$kind $namespace $name"; then
        continue
    fi
    live=$(get_json "$kind" "$name" "$namespace")
    if [ -n "$live" ] && ! helm_managed "$live"; then
        legacy_objects+=("$kind $name")
    fi
done
# what is left to do, so a rerun only lists the steps that didn't finish
state_recorded=false
if [ -n "$(get_json configmap "$STATE_CONFIG_MAP" "$namespace")" ]; then
    state_recorded=true
fi
live_cluster=$(get_json thoriumcluster "$cluster_name" "$namespace")

########################################
# Confirm
########################################
if [ "$dry_run" = true ]; then
    step "Dry run: these changes would be made"
else
    step "These changes will be made"
fi
while read -r op_ns op_name op_replicas; do
    [ -n "$op_ns" ] || continue
    [ "$op_replicas" != 0 ] || continue
    echo "    stop the pre-Helm operator $op_ns/$op_name (replicas $op_replicas)"
    if [ "$op_ns" != "$namespace" ]; then
        echo "      it is shared with other pre-Helm deployments, which keep running unmanaged" \
            "until they are retired"
    fi
done <<< "$operators"
# the chart refuses to install while a ThoriumCluster exists in any other namespace
others=$(kc get thoriumclusters -A -o json | jq -r --arg ns "$namespace" \
    '.items[] | select(.metadata.namespace != $ns) | "\(.metadata.namespace)/\(.metadata.name)"')
if [ -n "$others" ]; then
    echo "    WARNING: the chart won't install while these other ThoriumClusters exist; retire them" \
        "first (see \"Retiring other pre-Helm instances\" in the conversion docs):"
    printf '      %s\n' $others
fi
if [ -z "$snapshot_json" ]; then
    echo "    save ThoriumCluster $cluster_name in Secret $SNAPSHOT_SECRET"
fi
echo "    apply the chart's ThoriumCluster CRD"
if [ "$state_recorded" != true ]; then
    echo "    record revision $BASELINE_REVISION in ConfigMap $STATE_CONFIG_MAP"
fi
if [ "${#adopt[@]}" -gt 0 ] || [ "${#replace[@]}" -gt 0 ]; then
    echo "    hand ${#adopt[@]} object(s) to Helm release $release and delete ${#replace[@]} the chart replaces"
fi
for entry in ${legacy_objects[@]+"${legacy_objects[@]}"}; do
    echo "    delete the legacy $entry"
done
if [ -n "$live_cluster" ]; then
    echo "    delete ThoriumCluster $cluster_name (its components keep running)"
fi
if [ "$dry_run" != true ] && [ "$assume_yes" != true ]; then
    if [ ! -t 0 ]; then
        echo "ERROR: confirm the changes interactively or pass --yes" >&2
        exit 1
    fi
    read -r -p "Continue? [y/N] " answer
    case "$answer" in
        y|Y|yes|YES) ;;
        *) echo "Nothing was changed."; exit 1 ;;
    esac
fi

########################################
# Stop the pre-Helm operators
########################################
step "Stopping the pre-Helm operators"
while read -r op_ns op_name op_replicas; do
    [ -n "$op_ns" ] || continue
    if [ "$op_replicas" != 0 ]; then
        change "scale $op_ns/$op_name to 0" kc scale deployment "$op_name" -n "$op_ns" --replicas=0
    fi
done <<< "$operators"
if [ "$dry_run" != true ] && [ -n "$operators" ]; then
    # wait for the operator pods to exit so nothing reconciles the ThoriumCluster mid-conversion
    while read -r op_ns op_name _; do
        [ -n "$op_ns" ] || continue
        kc wait --for=delete pod -n "$op_ns" -l app=operator --timeout=120s >/dev/null 2>&1 || true
    done <<< "$operators"
fi

########################################
# Save the ThoriumCluster
########################################
# the copy is taken before the CRD changes, since the new schema prunes fields the old one kept
step "Saving the ThoriumCluster"
if [ -z "$snapshot_json" ]; then
    # keep only what is needed to create the ThoriumCluster again
    restorable=$(jq 'del(.status, .metadata.managedFields, .metadata.resourceVersion, .metadata.uid,
        .metadata.creationTimestamp, .metadata.generation, .metadata.deletionTimestamp,
        .metadata.deletionGracePeriodSeconds)' <<< "$cluster_json")
    # the ThoriumCluster holds credentials, so it reaches jq through its environment rather than
    # as an argument visible in the process list
    # the legacy ingress objects and pull Secret are deleted below or later, so they are kept too
    # for a rerun to carry them over again
    snapshot=$(DATA="$restorable" INGRESS="$legacy_ingress" ROUTE="$legacy_route" \
        STORE="$legacy_store" PULL="$legacy_pull" jq -n --arg ns "$namespace" \
        --arg name "$SNAPSHOT_SECRET" '{
        apiVersion: "v1", kind: "Secret", type: "Opaque",
        metadata: {name: $name, namespace: $ns,
          labels: {"thorium.sandia.gov/legacy-components": "true"},
          annotations: {"helm.sh/resource-policy": "keep"}},
        stringData: ({"cluster.json": $ENV.DATA}
          + ([["ingress.json", $ENV.INGRESS], ["ingressroute.json", $ENV.ROUTE],
              ["tlsstore.json", $ENV.STORE], ["registry-token.json", $ENV.PULL]]
             | map(select(.[1] != "") | {key: .[0], value: .[1]}) | from_entries))}')
    change "create Secret $SNAPSHOT_SECRET" kc create -f - <<< "$snapshot"
else
    echo "    Secret $SNAPSHOT_SECRET already exists"
fi
########################################
# Update the ThoriumCluster CRD
########################################
# Helm never updates a CRD that already exists, and the new operator only applies its own once
# the chart installed it, so the pre-Helm schema (which rejects the chart's ThoriumCluster) is
# replaced here, with the field manager the operator applies it with
step "Updating the ThoriumCluster CRD to the chart's"
crd=$(helm show crds "$chart" ${chart_version:+--version "$chart_version"} | awk '
    /^---/ { if (doc ~ /name: thoriumclusters\.sandia\.gov/) print doc; doc = ""; next }
    { doc = doc $0 "\n" }
    END { if (doc ~ /name: thoriumclusters\.sandia\.gov/) print doc }')
if [ -z "$crd" ]; then
    echo "ERROR: $chart has no ThoriumCluster CRD" >&2
    exit 1
fi
change "apply the ThoriumCluster CRD" kc apply --server-side --force-conflicts \
    --field-manager thorium_cluster_apply -f - <<< "$crd"

########################################
# Record the starting revision
########################################
step "Recording revision $BASELINE_REVISION"
if [ "$state_recorded" != true ]; then
    now=$(date -u +%Y-%m-%dT%H:%M:%SZ)
    state=$(jq -nc --arg rev "$BASELINE_REVISION" --arg at "$now" '{schema_version: 1,
        revision: $rev, applied: [{revision: $rev, step: "convert-to-helm", outcome: "Done",
        at: $at, by: "convert-to-helm.sh"}]}')
    state_cm=$(jq -n --arg ns "$namespace" --arg name "$STATE_CONFIG_MAP" --arg rev "$BASELINE_REVISION" \
        --arg state "$state" '{apiVersion: "v1", kind: "ConfigMap",
        metadata: {name: $name, namespace: $ns,
          labels: {"thorium.sandia.gov/state-revision": $rev}},
        data: {"state.json": $state}}')
    change "create ConfigMap $STATE_CONFIG_MAP at revision $BASELINE_REVISION" kc create -f - <<< "$state_cm"
else
    echo "    ConfigMap $STATE_CONFIG_MAP already exists"
fi

########################################
# Move the old scaler's ClusterRole aside
########################################
step "Keeping the old scaler's permissions"
if printf '%s\n' ${replace[@]+"${replace[@]}"} | grep -qx "ClusterRole - $LEGACY_ROLE"; then
    # copy the legacy rules so the old scaler keeps them until its Deployment is upgraded
    if [ -z "$(get_json clusterrole "$LEGACY_COPY_ROLE")" ]; then
        copy=$(get_json clusterrole "$LEGACY_ROLE" | jq --arg name "$LEGACY_COPY_ROLE" --arg label "$LEGACY_LABEL" '
            {apiVersion, kind, rules, metadata: {name: $name, labels: {($label): "true"}}}')
        change "create ClusterRole $LEGACY_COPY_ROLE with the rules of $LEGACY_ROLE" kc create -f - <<< "$copy"
    fi
    # bind each subject of the legacy role to the copy under a new binding before removing the
    # old one, so a failure at any point leaves the old scaler with its permissions
    bindings=$(kc get clusterrolebindings -o json | jq -c --arg role "$LEGACY_ROLE" '
        .items[] | select(.roleRef.name == $role and .roleRef.kind == "ClusterRole")
        | select((.metadata.annotations["meta.helm.sh/release-name"] // "") == "")')
    while read -r binding; do
        [ -n "$binding" ] || continue
        name=$(jq -r .metadata.name <<< "$binding")
        moved=$(jq --arg copy "$LEGACY_COPY_ROLE" --arg label "$LEGACY_LABEL" '
            {apiVersion, kind, subjects, roleRef: (.roleRef | .name = $copy),
             metadata: {name: "\(.metadata.name)-legacy", labels: {($label): "true"}}}' <<< "$binding")
        if [ -z "$(get_json clusterrolebinding "$name-legacy")" ]; then
            change "bind the subjects of ClusterRoleBinding $name to $LEGACY_COPY_ROLE as $name-legacy" \
                kc create -f - <<< "$moved"
        fi
        change "delete ClusterRoleBinding $name" kc delete clusterrolebinding "$name"
    done <<< "$bindings"
    change "delete ClusterRole $LEGACY_ROLE so the chart can create its own" kc delete clusterrole "$LEGACY_ROLE"
elif [ -n "$(get_json clusterrole "$LEGACY_COPY_ROLE")" ]; then
    echo "    already moved to ClusterRole $LEGACY_COPY_ROLE"
else
    echo "    the chart doesn't render a ClusterRole named $LEGACY_ROLE, so the legacy RBAC stays as is"
fi

########################################
# Hand objects to Helm and delete what the chart replaces
########################################
step "Handing objects to Helm release $release"
for entry in ${adopt[@]+"${adopt[@]}"}; do
    read -r kind ns name <<< "$entry"
    [ "$ns" = "-" ] && ns=""
    change "adopt $kind ${ns:+$ns/}$name" kc annotate --overwrite "$kind" "$name" ${ns:+-n "$ns"} \
        "meta.helm.sh/release-name=$release" "meta.helm.sh/release-namespace=$namespace"
    change "label $kind ${ns:+$ns/}$name as managed by Helm" kc label --overwrite "$kind" "$name" \
        ${ns:+-n "$ns"} app.kubernetes.io/managed-by=Helm
done
step "Deleting what the chart replaces"
for entry in ${replace[@]+"${replace[@]}"}; do
    read -r kind ns name <<< "$entry"
    [ "$ns" = "-" ] && ns=""
    case "$kind" in
        # handled above and below
        ClusterRole|ThoriumCluster) continue ;;
    esac
    change "delete $kind ${ns:+$ns/}$name" kc delete "$kind" "$name" ${ns:+-n "$ns"} --wait=true
done
for entry in ${legacy_objects[@]+"${legacy_objects[@]}"}; do
    read -r kind name <<< "$entry"
    change "delete legacy $kind $namespace/$name" kc delete "$kind" "$name" -n "$namespace"
done

########################################
# Delete the ThoriumCluster without cleaning up after it
########################################
step "Deleting ThoriumCluster $cluster_name for the chart to create again"
if [ -n "$live_cluster" ]; then
    # with every operator stopped nothing would remove the finalizer, and nothing should clean up
    change "remove the finalizers of ThoriumCluster $cluster_name" kc patch thoriumcluster \
        "$cluster_name" -n "$namespace" --type merge -p '{"metadata":{"finalizers":null}}'
    change "delete ThoriumCluster $cluster_name" kc delete thoriumcluster "$cluster_name" \
        -n "$namespace" --wait=true
else
    echo "    already deleted"
fi

########################################
# Next steps
########################################
echo
if [ "$dry_run" = true ]; then
    echo "Dry run complete; nothing in the cluster was changed. Review $values_out and run again"
    echo "without --dry-run to convert the deployment."
    exit 0
fi
cat <<EOF
Conversion complete. Thorium's components keep running unmanaged until the thorium chart is
deployed with the values in $values_out (pass it after any other values so it wins):

  helm upgrade --install $release <thorium chart> -n $namespace -f $values_out
  minithor ${prefix:+--namespace-prefix $prefix }deploy --skip-operators --no-toolbox --values $values_out
  megathor: thorium_deployment_name=$cluster_name, infra_operators_enabled=false, and
            thorium_values_files=[$values_out]

The operator then waits in the UpgradeRequired phase until a target revision is set (minithor
sets autoTargetDev), runs the upgrade steps, and deploys the new components. Once the
ThoriumCluster reports Ready, remove the legacy RBAC kept for the old scaler:

  $0 ${prefix:+--namespace-prefix $prefix }${context:+--context $context }--cleanup

To roll back before deploying the chart, run the old minithor or megathor again (it recreates
the operator, its RBAC, and the ingress), restore the ThoriumCluster from Secret
$namespace/$SNAPSHOT_SECRET if needed, delete that Secret and ConfigMap
$namespace/$STATE_CONFIG_MAP, and remove the copied ClusterRole $LEGACY_COPY_ROLE with its
bindings and the Helm ownership of the objects handed over (see "Rolling back" in "Converting a
Pre-Helm Deployment" in the Thorium deployment docs).
EOF
