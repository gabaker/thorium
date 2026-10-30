#!/usr/bin/env bash
# Checks minithor's handling of a pre-upgrade CRD hook that can't start: --local-image is verified
# on every node after loading (explaining an architecture mismatch), a failed hook's pod events and
# logs are printed, and the chart's hook deadline leaves room to pull the new image.
# Runs offline with a stub minikube; needs helm for the chart check.
#   usage: minithor/tests/crd_hook_test.sh
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
# a stub minikube whose single node is arm64 and holds only the images in $STUB_IMAGES (name@arch)
mkdir -p "$work/bin"
cat > "$work/bin/minikube" <<'STUB'
#!/usr/bin/env bash
case "$*" in
    "node list") echo "node1 192.168.49.2" ;;
    *"crictl inspecti "*)
        for i in $STUB_IMAGES; do
            if [ "${i%@*}" = "${*: -1}" ]; then
                printf '{\n  "info": {\n    "imageSpec": {\n      "architecture": "%s",\n' "${i#*@}"
                exit 0
            fi
        done
        exit 1 ;;
    *"uname -m") echo aarch64 ;;
    *) exit 0 ;;
esac
STUB
chmod +x "$work/bin/minikube"
# load minithor's helpers without running its command dispatch
for fn in list_minikube_nodes verify_local_image; do
    sed -n "/^${fn}() {/,/^}/p" "$minithor" >> "$work/funcs.sh"
done
grep -q '^verify_local_image() {' "$work/funcs.sh"
check "minithor defines verify_local_image" $?
# an image every node holds passes
out="$(PATH="$work/bin:$PATH" STUB_IMAGES="thorium:dev@arm64" MINIKUBE_PROFILE=p bash -c ". $work/funcs.sh; verify_local_image thorium:dev" 2>&1)"
check "a loaded image passes" $?
# an image built for another architecture than the node fails and names both
out="$(PATH="$work/bin:$PATH" STUB_IMAGES="thorium:dev@amd64" MINIKUBE_PROFILE=p bash -c ". $work/funcs.sh; verify_local_image thorium:dev" 2>&1)"
rc=$?
[ "$rc" -ne 0 ] && grep -q "built for amd64 but node node1 is arm64" <<<"$out"
check "an image for another architecture fails" $?
# the mismatch explains host emulation and points at pulling a matching image with --image
grep -q "Rosetta" <<<"$out" && grep -q "Pass --image with a registry image that has a linux/arm64 variant" <<<"$out"
check "an architecture mismatch explains emulation and suggests --image" $?
# an image a node lacks fails
out="$(PATH="$work/bin:$PATH" STUB_IMAGES="" MINIKUBE_PROFILE=p bash -c ". $work/funcs.sh; verify_local_image thorium:dev" 2>&1)"
rc=$?
[ "$rc" -ne 0 ] && grep -q "not on node node1" <<<"$out"
check "a missing image fails" $?
# a failed hook Job's pods, events, and logs are printed (the stub kubectl records its calls)
cat > "$work/bin/kubectl-stub" <<'STUB'
#!/usr/bin/env bash
echo "$*" >> "$STUB_LOG"
case "$*" in
    *"describe pods"*) printf 'Name: thorium-apply-crd-x\nEvents:\n  Warning  Failed  pull access denied\n' ;;
    *"logs job/thorium-apply-crd"*) echo "hook log line" ;;
    *) exit 0 ;;
esac
STUB
chmod +x "$work/bin/kubectl-stub"
sed -n "/^report_crd_hook_failure() {/,/^}/p" "$minithor" > "$work/report.sh"
out="$(STUB_LOG="$work/kubectl.log" bash -c "kctl() { $work/bin/kubectl-stub \"\$@\"; }; . $work/report.sh; report_crd_hook_failure thorium" 2>&1)"
grep -q "pull access denied" <<<"$out" && grep -q "hook log line" <<<"$out" && ! grep -q "^Name:" <<<"$out"
check "a failed CRD hook prints its pod events and logs" $?
# deploy reports the hook only when the thorium chart's upgrade fails
# shellcheck disable=SC2016
grep -A6 'helm_cmd upgrade --install "$THORIUM_RELEASE"' "$minithor" | grep -q 'report_crd_hook_failure'
check "deploy reports the CRD hook when helm fails" $?
# deploy verifies the local image right after loading it (the patterns are literal minithor text)
# shellcheck disable=SC2016
grep -A1 'minikube image load "$local_image"' "$minithor" | grep -q 'verify_local_image "$local_image"'
check "deploy verifies --local-image after loading it" $?
# the hook Job's default deadline leaves time to pull the new image
deadline="$(helm template thorium "$repo/deploy/charts/thorium" -n thorium --set secrets.renderOnly=true --is-upgrade \
    | awk '/name: thorium-apply-crd/ {f = 1} f && /activeDeadlineSeconds:/ {print $2; exit}')"
[ "${deadline:-0}" -ge 300 ]
check "the CRD hook deadline is at least 300s (is ${deadline:-unset})" $?
exit "$fails"
