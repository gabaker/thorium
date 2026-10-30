#!/usr/bin/env bash
# Checks that deploy explains a server-side apply conflict in a failed thorium chart upgrade with
# the exact command that fixes it, and that --force-conflicts is only offered when the helm in
# use has the flag. Runs offline with stub helm commands.
#   usage: minithor/tests/apply_conflict_test.sh
set -uo pipefail
repo="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
minithor="$repo/minithor/minithor"
work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT
fails=0
# report one check's result
#   $1 - the check's name, $2 - 0 when it passed
check() {
    if [ "$2" -eq 0 ]; then echo "ok   $1"; else echo "FAIL $1"; fails=$((fails + 1)); fi
}
# stub helms: Helm 4 lists --force-conflicts in its upgrade help, Helm 3 doesn't
mkdir -p "$work/bin"
printf '#!/usr/bin/env bash\necho "      --force-conflicts   if set server-side apply will force changes against conflicts"\n' \
    > "$work/bin/helm4"
printf '#!/usr/bin/env bash\necho "      --force   force resource updates through a replacement strategy"\n' > "$work/bin/helm3"
chmod +x "$work/bin/helm4" "$work/bin/helm3"
# load minithor's helpers without running its command dispatch
for fn in helm_supports_force_conflicts deploy_retry_command report_apply_conflicts; do
    sed -n "/^${fn}() /,/^}/p" "$minithor" >> "$work/funcs.sh"
    grep -q "^${fn}() " "$minithor"
    check "minithor defines $fn" $?
done
# the failure Helm 4 printed upgrading a release whose Elasticsearch node sets other managers own
cat > "$work/conflict.log" <<'EOF'
Error: UPGRADE FAILED: conflict occurred while applying object elastic/elastic elasticsearch.k8s.elastic.co/v1, Kind=Elasticsearch: Apply failed with 2 conflicts: conflicts with "elastic-operator" using elasticsearch.k8s.elastic.co/v1:
- .spec.nodeSets
conflicts with "before-first-apply" using elasticsearch.k8s.elastic.co/v1:
- .spec.nodeSets
EOF
echo "Error: UPGRADE FAILED: timed out waiting for the condition" > "$work/other.log"
# run a helper with the given helm
#   $1 - the stub helm's name; the rest are the shell code to run
with_helm() {
    local helm=$1
    shift
    HELM="$work/bin/$helm" MINIKUBE_PROFILE=minikube NAMESPACE_PREFIX="" bash -c ". $work/funcs.sh; $*" 2>&1
}

# a conflict with Helm 4 names the fields and their owners and prints the command to re-run
out="$(with_helm helm4 'report_apply_conflicts '"$work/conflict.log"' --chart deploy/charts/thorium --password s3cret')"
grep -q 'conflicts with "elastic-operator"' <<<"$out" && grep -q -- '- .spec.nodeSets' <<<"$out"
check "a conflict names the fields and their managers" $?
grep -qF "minithor deploy --chart deploy/charts/thorium --password '<password>' --force-conflicts" <<<"$out"
check "a conflict prints the deploy command with --force-conflicts, password hidden" $?
! grep -q s3cret <<<"$out"
check "the printed command never holds the password" $?
# a conflict with a helm that can't force conflicts says which helm can
out="$(with_helm helm3 'report_apply_conflicts '"$work/conflict.log"' --chart x')"
grep -q "Helm 4" <<<"$out" && ! grep -q -- "deploy --chart x --force-conflicts" <<<"$out"
check "without --force-conflicts support the fix names Helm 4" $?
# another failure prints nothing
out="$(with_helm helm4 'report_apply_conflicts '"$work/other.log"' --chart x')"
grep -q . <<<"$out"
check "other upgrade failures print no conflict advice" $((! $?))
# a profile and namespace prefix are repeated in the printed command
out="$(HELM="$work/bin/helm4" MINIKUBE_PROFILE=mac NAMESPACE_PREFIX=dev bash -c ". $work/funcs.sh; deploy_retry_command --no-kibana")"
rc=1
[ "$out" != "minithor --namespace-prefix dev --profile mac deploy --no-kibana --force-conflicts" ] || rc=0
check "the printed command keeps the global options (is: $out)" "$rc"
# credentials in proxy and other URLs and in credential --set values are hidden in every form
out="$(with_helm helm4 'deploy_retry_command --proxy http://bob:s3cret@p:3128 --set global.credentials.redisPassword=x \
    --set-string infra.elastic.apiToken=tok1 --set=auth.secretKey=sk1 --proxy=https://amy:pw2@q:8080 \
    --toolbox https://cat:pw3@h/t.json --password=pw4 --set infra.redis.storage=20Gi')"
! grep -qE 's3cret|redisPassword=x|tok1|sk1|pw2|pw3|pw4' <<<"$out"
check "the printed command hides credentials in URLs and credential --set values (is: $out)" $?
grep -qF "http://bob:***@p:3128" <<<"$out" && grep -qF "https://amy:***@q:8080" <<<"$out" \
    && grep -qF "https://cat:***@h/t.json" <<<"$out" && grep -qF "global.credentials.redisPassword=<hidden>" <<<"$out" \
    && grep -qF "infra.redis.storage=20Gi" <<<"$out"
check "the printed command keeps the URL users and the --set keys and other values" $?
# print the retry command for the given deploy options
retry() {
    HELM="$work/bin/helm4" MINIKUBE_PROFILE=minikube NAMESPACE_PREFIX="" \
        bash -c '. "$0"; deploy_retry_command "$@"' "$work/funcs.sh" "$@" 2>&1
}
# check that the retry command holds a text and no secret
#   $1 - the check's name, $2 - the text it must hold, $3 - a regex of the secrets it must not
#   hold; the rest are the deploy options
expect_retry() {
    local name=$1 want=$2 secret=$3 out
    shift 3
    out="$(retry "$@")"
    grep -qF -- "$want" <<<"$out" && ! grep -qE -- "$secret" <<<"$out"
    check "$name (is: $out)" $?
}
# every entry of a --set or --set-string list is checked, keeping helm's commas, escapes, and lists
expect_retry "a credential after another --set entry is hidden" \
    "deploy --set 'a=1,global.credentials.x=<hidden>' --force-conflicts" sec \
    --set 'a=1,global.credentials.x=sec'
expect_retry "entries after a credential --set entry are kept" \
    "deploy --set 'global.credentials.x=<hidden>,a=1' --force-conflicts" sec \
    --set 'global.credentials.x=sec,a=1'
expect_retry "escaped commas stay inside their --set value" \
    "deploy --set 'a=1\\,2,global.credentials.x=<hidden>,b=3' --force-conflicts" sec \
    --set 'a=1\,2,global.credentials.x=sec\,ret,b=3'
expect_retry "commas inside a --set list stay in their entry" \
    "deploy --set 'a={1,2},global.credentials.x=<hidden>,b=3' --force-conflicts" sec \
    --set 'a={1,2},global.credentials.x={sec,ret},b=3'
expect_retry "--set-string lists are checked entry by entry" \
    "deploy --set-string 'a=1,auth.token=<hidden>,b=2' --force-conflicts" sec \
    --set-string 'a=1,auth.token=sec,b=2'
expect_retry "--set= lists are checked entry by entry" \
    "deploy '--set=a=1,global.credentials.x=<hidden>' --force-conflicts" sec \
    --set=a=1,global.credentials.x=sec
expect_retry "--set-string= lists are checked entry by entry" \
    "deploy '--set-string=global.credentials.x=<hidden>,a=1' --force-conflicts" sec \
    --set-string=global.credentials.x=sec,a=1
# the argument after a flag that takes a value is that flag's value, even when it looks like a flag
expect_retry "a --password value that looks like a flag is hidden" \
    "deploy --password '<password>' --no-kibana --force-conflicts" sec \
    --password '--x=sec' --no-kibana
expect_retry "a --set value that looks like a flag is checked" \
    "deploy --set '--global.credentials.x=<hidden>' --force-conflicts" sec \
    --set '--global.credentials.x=sec'
# a URL password is hidden up to the last @ before the host
expect_retry "a --proxy password holding @ is hidden" \
    "deploy --proxy 'http://u:***@h:3128' --force-conflicts" 'p@|ss@' \
    --proxy 'http://u:p@ss@h:3128'
expect_retry "a --proxy= password holding @ is hidden" \
    "deploy '--proxy=http://u:***@h' --force-conflicts" 'p@|ss@' \
    --proxy=http://u:p@ss@h
expect_retry "a URL password before a port and path is hidden" \
    "deploy --toolbox 'https://u:***@h:443/path/t.json' --force-conflicts" 'p@|ss@' \
    --toolbox 'https://u:p@ss@h:443/path/t.json'
# paths, URLs without a password, and ports are shown as given
expect_retry "--values and --set-file paths stay visible" \
    "deploy --values /tmp/secret.yaml --set-file global.credentials.key=/tmp/k.pem --values=/tmp/v.yaml --force-conflicts" \
    '<hidden>' --values /tmp/secret.yaml --set-file global.credentials.key=/tmp/k.pem --values=/tmp/v.yaml
expect_retry "URLs without a password are unchanged" \
    "deploy --proxy http://u@p:3128 --toolbox https://h:443/path/t.json\\?a=b@c --force-conflicts" \
    '\*\*\*' --proxy http://u@p:3128 --toolbox 'https://h:443/path/t.json?a=b@c'

# --force-conflicts is only available with a helm that has the flag
with_helm helm4 helm_supports_force_conflicts > /dev/null
check "Helm 4 supports --force-conflicts" $?
! with_helm helm3 helm_supports_force_conflicts > /dev/null
check "Helm 3 doesn't support --force-conflicts" $?

# deploy wires it up (the patterns are literal minithor text)
# shellcheck disable=SC2016
grep -q -- '--force-conflicts) *force_conflicts=true' "$minithor" \
    && grep -q 'conflict_args=(--server-side=true --force-conflicts)' "$minithor" \
    && grep -A6 'helm_cmd upgrade --install "$THORIUM_RELEASE"' "$minithor" | grep -q 'report_apply_conflicts'
check "deploy parses --force-conflicts and reports conflicts when helm fails" $?
bash "$minithor" deploy --help | grep -q -- "--force-conflicts"
check "deploy --help documents --force-conflicts" $?
exit "$fails"
