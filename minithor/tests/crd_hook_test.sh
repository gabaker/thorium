#!/usr/bin/env bash
# Checks minithor's handling of a pre-upgrade CRD hook that can't start: --local-image is verified
# on every node after loading (explaining an architecture mismatch, whether or not minikube loaded
# the image), a failed hook's pod events and
# logs are printed (or, once the Job controller deleted its pods, the Job's status and the last
# events of the Job and its pods), and the chart's hook deadline leaves room to pull the new image.
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
# a stub docker whose local image is built for $STUB_LOCAL_ARCH (no image when unset), and a stub
# podman without images, so the host's own engines are never asked
cat > "$work/bin/docker" <<'STUB'
#!/usr/bin/env bash
[ "$*" = "image inspect --format {{.Architecture}} thorium:dev" ] && [ -n "${STUB_LOCAL_ARCH:-}" ] || exit 1
echo "$STUB_LOCAL_ARCH"
STUB
printf '#!/usr/bin/env bash\nexit 1\n' > "$work/bin/podman"
chmod +x "$work/bin/docker" "$work/bin/podman"
# load minithor's helpers without running its command dispatch
for fn in list_minikube_nodes local_image_arch verify_local_image; do
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
# an image for another architecture that minikube refused to load is missing from the node, and the
# local image's architecture still explains why
out="$(PATH="$work/bin:$PATH" STUB_IMAGES="" STUB_LOCAL_ARCH=amd64 MINIKUBE_PROFILE=p bash -c ". $work/funcs.sh; verify_local_image thorium:dev" 2>&1)"
rc=$?
[ "$rc" -ne 0 ] && grep -q "built for amd64 but node node1 is arm64" <<<"$out" && grep -q "Rosetta" <<<"$out" \
    && grep -q "linux/arm64" <<<"$out"
check "a refused load of an image for another architecture prints the architecture hint" $?
# a local image for the node's architecture that every node holds passes
out="$(PATH="$work/bin:$PATH" STUB_IMAGES="thorium:dev@arm64" STUB_LOCAL_ARCH=arm64 MINIKUBE_PROFILE=p bash -c ". $work/funcs.sh; verify_local_image thorium:dev" 2>&1)"
check "a local image for the node's architecture passes" $?
# a failed hook Job's pods, events, and logs are printed (the stub kubectl records its calls)
cat > "$work/bin/kubectl-stub" <<'STUB'
#!/usr/bin/env bash
echo "$*" >> "$STUB_LOG"
case "$*" in
    *"get pods"*"-o name"*) echo pod/thorium-apply-crd-x ;;
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
# a hook Job whose pods the Job controller deleted at its deadline prints the Job's status and the
# last events of this run's Job and pods, leaving out earlier runs and other objects
cat > "$work/bin/kubectl-gone" <<'STUB'
#!/usr/bin/env bash
case "$*" in
    *"get job thorium-apply-crd -o jsonpath={.metadata.creationTimestamp}"*) echo 2026-10-30T03:33:00Z ;;
    *"get job thorium-apply-crd"*) echo "job.batch/thorium-apply-crd" ;;
    *"get pods"*"-o name"*) ;;
    *"get pods"*) echo "No resources found in thorium namespace." >&2 ;;
    *"describe pods"*) echo "No resources found in thorium namespace." >&2 ;;
    *"logs job/thorium-apply-crd"*) echo "error: no pods found" >&2; exit 1 ;;
    *"describe job thorium-apply-crd"*)
        printf 'Name:             thorium-apply-crd
Start Time:       Fri, 30 Oct 2026 03:33:00 +0000
'
        printf 'Active Deadline Seconds:  60s
Pods Statuses:    0 Active / 0 Succeeded / 1 Failed
'
        printf 'Pod Template:
  Containers:
   apply-crd:
    Image:      ghcr.io/x/thorium:nonexistent
'
        printf 'Events:
  Type  Reason  Age  From  Message
' ;;
    *"get events"*)
        t='\t'
        printf "2026-10-30T03:10:00Z${t}Pod${t}thorium-apply-crd-old1${t}Normal${t}Completed${t}an earlier run\n"
        printf "2026-10-30T03:33:10Z${t}Pod${t}thorium-api-abc${t}Warning${t}BackOff${t}another pod\n"
        printf "2026-10-30T03:33:01Z${t}Job${t}thorium-apply-crd${t}Normal${t}SuccessfulCreate${t}Created pod: thorium-apply-crd-nfrrt\n"
        for i in 10 11 12 13 14 15 16 17 18 19 20 21 22 23 24 25; do
            printf "2026-10-30T03:33:%s${t}Pod${t}thorium-apply-crd-nfrrt${t}Normal${t}BackOff${t}Back-off pulling image %s\n" "${i}Z" "$i"
        done
        printf "2026-10-30T03:33:40Z${t}Pod${t}thorium-apply-crd-nfrrt${t}Warning${t}Failed${t}Failed to pull image \"ghcr.io/x/thorium:nonexistent\": not found\n"
        printf "2026-10-30T03:34:01Z${t}Job${t}thorium-apply-crd${t}Normal${t}SuccessfulDelete${t}Deleted pod: thorium-apply-crd-nfrrt\n"
        printf "2026-10-30T03:34:02Z${t}Job${t}thorium-apply-crd${t}Warning${t}DeadlineExceeded${t}Job was active longer than specified deadline\n" ;;
    *) exit 0 ;;
esac
STUB
chmod +x "$work/bin/kubectl-gone"
out="$(bash -c "kctl() { $work/bin/kubectl-gone \"\$@\"; }; . $work/report.sh; report_crd_hook_failure thorium" 2>&1)"
grep -q "Failed to pull image" <<<"$out" && grep -q "DeadlineExceeded" <<<"$out" && grep -q "Deleted pod" <<<"$out"
check "a hook Job without pods prints its last events" $?
grep -q "Image: *ghcr.io/x/thorium:nonexistent" <<<"$out" && grep -q "Pods Statuses: .*1 Failed" <<<"$out" \
    && ! grep -q "^Pod Template:" <<<"$out"
check "a hook Job without pods prints its image and pod statuses from describe job" $?
! grep -q "an earlier run" <<<"$out" && ! grep -q "another pod" <<<"$out" && ! grep -q "No resources found" <<<"$out"
check "a hook Job without pods leaves out earlier runs, other objects, and empty pod listings" $?
events="$(grep -c "thorium-apply-crd" <<<"$out")"
[ "$events" -le 17 ] && ! grep -q "Back-off pulling image 10$" <<<"$out"
check "a hook Job without pods prints only the last events (printed $events lines naming the Job)" $?
# deploy reports the hook only when the thorium chart's upgrade fails
# shellcheck disable=SC2016
grep -A6 'helm_cmd upgrade --install "$THORIUM_RELEASE"' "$minithor" | grep -q 'report_crd_hook_failure'
check "deploy reports the CRD hook when helm fails" $?
# deploy verifies the local image right after loading it (the patterns are literal minithor text)
# shellcheck disable=SC2016
grep -A1 'minikube image load "$local_image"' "$minithor" | grep -q 'verify_local_image "$local_image"'
check "deploy verifies --local-image after loading it" $?
# a failed load still runs the verification (which explains an architecture mismatch) and stops deploy
# shellcheck disable=SC2016
grep -q 'minikube image load "$local_image" || load_rc=$?$' "$minithor" \
    && grep -A6 'minikube image load "$local_image"' "$minithor" | grep -q 'load_rc" -ne 0'
check "deploy verifies --local-image even when minikube refuses to load it" $?
# the hook Job's default deadline leaves time to pull the new image
deadline="$(helm template thorium "$repo/deploy/charts/thorium" -n thorium --set secrets.renderOnly=true --is-upgrade \
    | awk '/name: thorium-apply-crd/ {f = 1} f && /activeDeadlineSeconds:/ {print $2; exit}')"
[ "${deadline:-0}" -ge 300 ]
check "the CRD hook deadline is at least 300s (is ${deadline:-unset})" $?
exit "$fails"
