#!/usr/bin/env bash
# Render the thorium chart's toolbox import Job for each combination of the operator.toolbox
# mode, createGroups, strict, and groupOverride values, and check the thorctl command line and
# retry limit the Job gets. Needs helm and python3 with PyYAML.
#
#   deploy/charts/scripts/check-toolbox-job.sh
set -euo pipefail

charts="$(cd "$(dirname "$0")/.." && pwd)"

# Render the Job with the given extra helm arguments and print its backoff limit on the first
# line followed by the import command line.
#
#   render_job [helm args...]
render_job() {
    helm template thorium "$charts/thorium" -n thorium --set secrets.renderOnly=true \
        --set operator.toolbox.url=https://example.invalid/toolbox.json \
        --show-only charts/operator/templates/toolbox.yaml "$@" \
        | python3 -c '
import sys, yaml
job = next(d for d in yaml.safe_load_all(sys.stdin) if d and d.get("kind") == "Job")
print(job["spec"]["backoffLimit"])
script = job["spec"]["template"]["spec"]["containers"][0]["command"][2]
print(next(l.strip() for l in script.splitlines() if l.strip().startswith("\"$thorctl\" toolbox import")))
'
}

failed=0
# Check one combination: the expected backoff limit, the expected import command line, then the
# helm arguments that select the combination.
#
#   expect <backoff> <command line> [helm args...]
expect() {
    local backoff="$1" command="$2 || rc=\$?" got
    shift 2
    got="$(render_job "$@")"
    if [ "$got" != "$backoff"$'\n'"$command" ]; then
        echo "FAIL: helm $*"
        echo "  expected: $backoff / $command"
        echo "  got:      ${got//$'\n'/ / }"
        failed=1
    else
        echo "ok: ${*:-defaults}"
    fi
}

base='"$thorctl" toolbox import "$TOOLBOX"'
expect 6 "$base --skip-conflicts"
expect 6 "$base --overwrite" --set operator.toolbox.mode=overwrite
expect 6 "$base --skip-conflicts --create-groups" --set operator.toolbox.createGroups=true
expect 6 "$base --skip-conflicts --strict" --set operator.toolbox.strict=true
expect 6 "$base --overwrite --create-groups --strict" --set operator.toolbox.mode=overwrite \
    --set operator.toolbox.createGroups=true --set operator.toolbox.strict=true
expect 6 "$base --skip-conflicts --group-override 'it'\\''s'" --set "operator.toolbox.groupOverride=it's"
expect 6 "$base --overwrite --create-groups --strict --group-override 'g1'" \
    --set operator.toolbox.mode=overwrite --set operator.toolbox.createGroups=true \
    --set operator.toolbox.strict=true --set operator.toolbox.groupOverride=g1
expect 3 "$base --skip-conflicts" --set operator.toolbox.backoffLimit=3
exit "$failed"
