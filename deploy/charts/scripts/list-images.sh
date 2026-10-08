#!/usr/bin/env bash
# Print every container image the infra-operators and thorium charts deploy, as
# "<source image> <path in a mirror registry>" lines (the format megathor's
# scripts/mirror-images.bash reads). Images the operators create from custom resources
# (Elasticsearch, Kibana, Scylla, the Thorium components) are included, as are the Scylla
# operator's auxiliary images from the infra-operators chart's scyllaOperatorConfig.
#
#   deploy/charts/scripts/list-images.sh [thorium chart values files...]
#
# Pass the same values files you deploy with so disabled components are left out and image
# overrides are honored, and list the infra-operators chart's values files (space separated) in
# INFRA_OPERATORS_VALUES the same way. Both charts are rendered with global.imageRegistry cleared,
# so values that point them at a mirror registry still list the upstream sources. Charts are read
# from this directory unless THORIUM_CHART and INFRA_OPERATORS_CHART point at other chart
# directories or packaged .tgz files.
set -euo pipefail

charts="$(cd "$(dirname "$0")/.." && pwd)"
thorium_chart="${THORIUM_CHART:-$charts/thorium}"
operators_chart="${INFRA_OPERATORS_CHART:-$charts/infra-operators}"
values_args=()
for file in "$@"; do
    values_args+=(-f "$file")
done
operators_values_args=()
# file names are split on whitespace, so they can't contain any
for file in ${INFRA_OPERATORS_VALUES:-}; do
    operators_values_args+=(-f "$file")
done

{
    # the ScyllaOperatorConfig isn't rendered without a mirror unless create is set, so it is
    # forced on to list its images
    helm template infra-operators "$operators_chart" -n infra-operators \
        ${operators_values_args[@]+"${operators_values_args[@]}"} --set global.imageRegistry= \
        --set scyllaOperatorConfig.create=true
    # the output is only read for images, so placeholder credentials stand in for the real ones
    helm template thorium "$thorium_chart" -n thorium ${values_args[@]+"${values_args[@]}"} \
        --set secrets.renderOnly=true --set global.imageRegistry=
} | python3 -c '
import sys
import yaml

images = set()


def walk(node):
    if isinstance(node, dict):
        for key, value in node.items():
            if key == "image" and isinstance(value, str) and value:
                images.add(value)
            else:
                walk(value)
    elif isinstance(node, list):
        for item in node:
            walk(item)


for doc in yaml.safe_load_all(sys.stdin):
    if not doc:
        continue
    kind = doc.get("kind")
    spec = doc.get("spec") or {}
    if kind == "ScyllaCluster":
        images.add("%s:%s" % (spec["repository"], spec["version"]))
        images.add("%s:%s" % (spec["agentRepository"], spec["agentVersion"]))
        continue
    if kind == "Elasticsearch":
        images.add(spec.get("image") or "docker.elastic.co/elasticsearch/elasticsearch:%s" % spec["version"])
        continue
    if kind == "Kibana":
        images.add(spec.get("image") or "docker.elastic.co/kibana/kibana:%s" % spec["version"])
        continue
    if kind == "ScyllaOperatorConfig":
        images.add(spec["scyllaUtilsImage"])
        images.add(spec["unsupportedBashToolsImageOverride"])
        continue
    if kind == "ThoriumCluster":
        images.add("%s:%s" % (spec["registry"], spec["version"]))
        continue
    walk(doc)

for image in sorted(images):
    parts = image.split("/")
    if len(parts) > 1 and ("." in parts[0] or ":" in parts[0] or parts[0] == "localhost"):
        parts = parts[1:]
    if len(parts) > 1 and parts[0] == "library":
        parts = parts[1:]
    print(image, "/".join(parts))
'
