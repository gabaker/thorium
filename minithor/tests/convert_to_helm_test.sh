#!/usr/bin/env bash
# Checks that deploy/charts/scripts/convert-to-helm.sh, which converts the pre-Helm deployments
# minithor deploys over, hands what it adopts to Helm in a form Helm 4's server-side apply accepts:
# the banner round-trips byte for byte (trailing newlines included), each adopted object's field
# ownership goes to the conversion alone, and the printed install skips the chart's CRD. Runs
# offline with a stub kubectl.
#   usage: minithor/tests/convert_to_helm_test.sh
set -uo pipefail
repo="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
script="$repo/deploy/charts/scripts/convert-to-helm.sh"
work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT
fails=0
# report one check's result
#   $1 - the check's name, $2 - 0 when it passed
check() {
    if [ "$2" -eq 0 ]; then echo "ok   $1"; else echo "FAIL $1"; fails=$((fails + 1)); fi
}
# report whether a value is the one expected
#   $1 - the check's name, $2 - the expected value, $3 - the value
same() {
    if [ "$2" = "$3" ]; then check "$1" 0; else check "$1 (is: $3)" 1; fi
}
# load the script's helpers without running the conversion
for fn in get_json read_banner ownership_patch; do
    sed -n "/^${fn}() {\$/,/^}\$/p" "$script" >> "$work/funcs.sh"
    grep -q "^${fn}() {\$" "$script"
    check "convert-to-helm.sh defines $fn" $?
done
# a stub kc that prints the ConfigMap in $work/configmap.json, or fails when there is none
cat >> "$work/funcs.sh" <<EOF
kc() { [ -f "$work/configmap.json" ] && cat "$work/configmap.json"; }
EOF
# run a helper with the stub kc
#   the arguments are the shell code to run
with_stub() {
    bash -c ". $work/funcs.sh; $*" 2>&1
}

# a banner made from a file keeps its trailing newline all the way into the values
jq -n '{data: {"banner.txt": "Welcome to the lab\n\n"}}' > "$work/configmap.json"
out="$(with_stub 'read_banner thorium')"
same "the banner keeps its trailing newlines" "$(printf 'Welcome to the lab\n\n' | od -c)" \
    "$(jq -j --argjson banner "$out" -n '$banner' | od -c)"
# the converted values render the banner the chart puts in ConfigMap banner unchanged
jq -n --argjson banner "$out" '{operator: ({} + (if $banner != null then {banner: $banner} else {} end))}' \
    > "$work/values.json"
rendered="$(helm template thorium "$repo/deploy/charts/thorium" -n thorium --set secrets.renderOnly=true \
    -f "$work/values.json" --show-only charts/operator/templates/operator.yaml 2>&1 \
    | awk '/^kind: ConfigMap/ { cm = 1 } cm && /^  banner.txt:/ { print; exit }')"
same "the chart renders the converted banner byte for byte" '  banner.txt: "Welcome to the lab\n\n"' "$rendered"
# an empty banner is carried over rather than replaced by the chart's default
jq -n '{data: {"banner.txt": ""}}' > "$work/configmap.json"
out="$(with_stub 'read_banner thorium')"
same "an empty banner stays empty" '""' "$out"
# without a banner the chart's default is used
jq -n '{data: {}}' > "$work/configmap.json"
out="$(with_stub 'read_banner thorium')"
same "a ConfigMap without banner.txt gives no banner" null "$out"
rm -f "$work/configmap.json"
out="$(with_stub 'read_banner thorium')"
same "no ConfigMap banner gives no banner" null "$out"
# the script passes the banner to jq as JSON, so its newlines survive
# shellcheck disable=SC2016
grep -q -- '--argjson banner "$banner"' "$script"
check "the values are built from the banner as JSON" $?

# the ownership patch leaves one Update entry for the conversion, owning only the fields it set
patch="$(with_stub 'ownership_patch v1')"
jq -e '.metadata.managedFields | length == 1 and .[0].manager == "convert-to-helm"
    and .[0].operation == "Update" and .[0].apiVersion == "v1" and .[0].fieldsType == "FieldsV1"' \
    <<<"$patch" > /dev/null
check "the ownership patch keeps one entry for the conversion (is: $patch)" $?
jq -e '[.metadata.managedFields[0].fieldsV1 | paths(type == "object" and length == 0)
    | map(ltrimstr("f:")) | join(".")] | sort == ["metadata.annotations.meta.helm.sh/release-name",
    "metadata.annotations.meta.helm.sh/release-namespace", "metadata.labels.app.kubernetes.io/managed-by"]' \
    <<<"$patch" > /dev/null
check "the conversion owns only the release annotations and managed-by label" $?
# every adopted object gets it, through change so a dry run shows it (literal script text)
# shellcheck disable=SC2016
grep -A12 '^for entry in ${adopt\[@\]+"${adopt\[@\]}"}; do$' "$script" | grep -q 'change "hand the fields of' \
    && grep -A12 '^for entry in ${adopt\[@\]+"${adopt\[@\]}"}; do$' "$script" | grep -q 'ownership_patch "$api"'
check "each adopted object's fields are handed to Helm through change" $?
# objects an earlier run adopted are handed over again, so a rerun finishes the job
# shellcheck disable=SC2016
grep -A6 'if helm_owned "$live"; then' "$script" | grep -q 'adopt+='
check "a rerun hands over the fields of objects an earlier run adopted" $?

# the printed install leaves the CRD the conversion applied to it
# shellcheck disable=SC2016
grep -q 'helm upgrade --install $release <thorium chart> -n $namespace -f $values_out --skip-crds' "$script"
check "the printed helm command skips the chart's CRD" $?
exit "$fails"
