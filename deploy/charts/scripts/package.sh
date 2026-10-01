#!/usr/bin/env bash
# Package the infra-operators and thorium charts as .tgz files, optionally pointing the thorium
# chart at a specific Thorium image. The Helm Charts workflow publishes what this script builds,
# and it can be run by hand to test charts without publishing them.
#
#   deploy/charts/scripts/package.sh [options]
#
#   -d, --destination <dir>      Where to write the packages (default: ./dist)
#   --version <version>          Chart version to package as (default: each Chart.yaml's version)
#   --image-repository <repo>    Thorium image repository the thorium chart deploys
#   --image-tag <tag>            Thorium image tag the thorium chart deploys (also its appVersion)
#   --pull-policy <policy>       imagePullPolicy for the operator and Thorium components
#                                (the chart defaults to Always)
#
# The image options are written into a temporary copy of the charts, so the checkout is never
# modified. The rendered thorium chart is checked afterwards, so a moved or renamed values key
# fails the script instead of silently packaging the default image. Packages are written as
# <destination>/infra-operators-<version>.tgz and <destination>/thorium-<version>.tgz, which
# `minithor deploy --chart <destination>/thorium-<version>.tgz` installs together.
set -euo pipefail

charts="$(cd "$(dirname "$0")/.." && pwd)"
destination="dist"
version=""
image_repository=""
image_tag=""
pull_policy=""

usage() {
    sed -n '2,19s/^# \{0,1\}//p' "$0"
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
        -d|--destination)   need_value "$1" $#; destination="$2"; shift ;;
        --version)          need_value "$1" $#; version="$2"; shift ;;
        --image-repository) need_value "$1" $#; image_repository="$2"; shift ;;
        --image-tag)        need_value "$1" $#; image_tag="$2"; shift ;;
        --pull-policy)      need_value "$1" $#; pull_policy="$2"; shift ;;
        -h|--help)          usage; exit 0 ;;
        *)                  echo "ERROR: unknown option $1" >&2; usage >&2; exit 1 ;;
    esac
    shift
done

# values are substituted into sed expressions, so only characters valid in image references and
# pull policies are accepted
for value in "$image_repository" "$image_tag" "$pull_policy"; do
    if [[ ! "$value" =~ ^[A-Za-z0-9._/:-]*$ ]]; then
        echo "ERROR: invalid image option value '$value'" >&2
        exit 1
    fi
done
if [ -n "$pull_policy" ] && [[ ! "$pull_policy" =~ ^(Always|IfNotPresent|Never)$ ]]; then
    echo "ERROR: --pull-policy must be Always, IfNotPresent, or Never" >&2
    exit 1
fi

# work on a copy so the checkout's charts are never changed
work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT
cp -R "$charts/infra-operators" "$charts/thorium" "$work/"
values="$work/thorium/charts/operator/values.yaml"

# set the image and pull policy in the operator subchart's own values: the image tag falls back to
# the subchart's appVersion, which `helm package --app-version` on the thorium chart cannot change
expected=()
if [ -n "$image_repository" ]; then
    sed -i.bak -E "/^image:/,/^[^ #]/ s|^(  repository:).*|\1 ${image_repository}|" "$values"
    expected+=("registry: \"${image_repository}\"")
fi
if [ -n "$image_tag" ]; then
    sed -i.bak -E "/^image:/,/^[^ #]/ s|^(  tag:).*|\1 \"${image_tag}\"|" "$values"
    expected+=("version: \"${image_tag}\"")
fi
if [ -n "$pull_policy" ]; then
    sed -i.bak -E "/^image:/,/^[^ #]/ s|^(  pullPolicy:).*|\1 ${pull_policy}|" "$values"
    expected+=("imagePullPolicy: ${pull_policy}" "image_pull_policy: ${pull_policy}")
fi
rm -f "$values.bak"

# check the rendered chart deploys what was asked for; whole lines are compared with indentation
# stripped, so labels such as app.kubernetes.io/version cannot satisfy the version check
if [ ${#expected[@]} -gt 0 ]; then
    rendered="$(helm template thorium "$work/thorium" -n thorium --set secrets.renderOnly=true \
        | sed -E 's/^[[:space:]]+//')"
    if [ -n "$image_repository" ] && [ -n "$image_tag" ]; then
        expected+=("image: ${image_repository}:${image_tag}")
    fi
    for line in "${expected[@]}"; do
        if ! grep -qxF -- "$line" <<< "$rendered"; then
            echo "ERROR: the rendered thorium chart is missing '${line}'" >&2
            exit 1
        fi
    done
    if [ -n "$image_repository" ]; then
        if stray="$(grep -E 'infrastructure/thorium' <<< "$rendered" | grep -vF -- "$image_repository")"; then
            echo "ERROR: the rendered thorium chart still references another Thorium image:" >&2
            echo "$stray" >&2
            exit 1
        fi
    fi
fi

# package both charts, giving the thorium chart the image tag as its appVersion
version_args=()
if [ -n "$version" ]; then
    version_args=(--version "$version")
fi
app_version_args=()
if [ -n "$image_tag" ]; then
    app_version_args=(--app-version "$image_tag")
fi
mkdir -p "$destination"
helm package "$work/infra-operators" ${version_args[@]+"${version_args[@]}"} -d "$destination"
helm package "$work/thorium" ${version_args[@]+"${version_args[@]}"} \
    ${app_version_args[@]+"${app_version_args[@]}"} -d "$destination"
