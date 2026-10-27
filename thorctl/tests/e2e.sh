#!/usr/bin/env bash
# End-to-end acceptance of the thorctl toolbox, tools, agents, workflows, and pipelines against a
# live Thorium.
#
# Walks the real toolbox through convert, validate, import, diff, export, a second import, and
# remove; then builds a tool, a toolset, an agent, a workflow, and a pipeline with the CLI, edits
# each of them in an editor script and from flags, and starts a pipeline run. Every step prints its
# command, its exit code, and its output, and the first failed expectation stops the script with
# that step's output. Everything it creates lives in two scratch groups named for this run, and the
# toolbox removes are the cleanup.
#
# Usage:
#   THORCTL_E2E_KEYS=/path/to/admin-keys.yml thorctl/tests/e2e.sh
#
# Environment:
#   THORCTL_E2E_KEYS   keys file of an admin (the real toolbox runs tools as root); required
#   THORCTL            the thorctl binary to test (default: thorctl on PATH)
#   THORCTL_E2E_WAIT   set to a duration (for example 5m) to also wait for the pipeline run to end
#                      and require that it completed; leave unset on clusters with no workers
#
# Needs bash, grep, sed, sha256sum, and cmp.
set -u -o pipefail

THORCTL=${THORCTL:-thorctl}
KEYS=${THORCTL_E2E_KEYS:?set THORCTL_E2E_KEYS to the keys file of an admin}
HERE=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
TOOLS=$(cd "$HERE/../../tools" && pwd)
WORK=$(mktemp -d)
SUFFIX=$(date +%s)-$$
G1=e2e-one-$SUFFIX
G2=e2e-two-$SUFFIX
G3=e2e-none-$SUFFIX
STEPS=0

# remove only the scratch directory this run made
[ -n "${E2E_KEEP:-}" ] || trap 'rm -rf "$WORK"' EXIT

# run thorctl as the admin without checking for updates
tc() { "$THORCTL" --keys "$KEYS" --skip-update "$@"; }

# stop the script, showing the output of the step that failed
fail() {
  echo "FAILED: $1" >&2
  [ -f "$WORK/last.out" ] && sed 's/^/    | /' "$WORK/last.out" >&2
  exit 1
}

# run thorctl, require an exit code, and keep the output for later checks
#   usage: run CODE NAME ARGS...
run() {
  local want=$1 name=$2 code
  shift 2
  STEPS=$((STEPS + 1))
  echo "[$STEPS] $name: thorctl $*"
  tc "$@" >"$WORK/last.out" 2>&1
  code=$?
  cp "$WORK/last.out" "$WORK/$STEPS-$name.out"
  echo "    exit $code (wanted $want)"
  [ "$code" -eq "$want" ] || fail "$name exited $code, wanted $want"
}

# require the last step's output to match a regular expression
#   usage: saw PATTERN
saw() { grep -Eq -- "$1" "$WORK/last.out" || fail "the output of the last step lacks /$1/"; }

# require the last step's output not to match a regular expression
#   usage: never PATTERN
never() { ! grep -Eq -- "$1" "$WORK/last.out" || fail "the output of the last step has /$1/"; }

# make an editor script that rewrites text in the buffer it is given
#   usage: editor FROM TO     prints the editor's path
editor() {
  local path
  path=$(mktemp "$WORK/editor.XXXXXX")
  cat >"$path" <<SCRIPT
#!/bin/sh
sed 's/$1/$2/' "\$1" > "\$1.new" && cat "\$1.new" > "\$1" && rm -f "\$1.new"
SCRIPT
  chmod +x "$path"
  echo "$path"
}

# let thorctl open the editor scripts below even when this script has no terminal
export THORCTL_EDIT_ASSUME_TTY=1

echo "== groups"
run 0 group-one groups create "$G1" -d "thorctl end to end"
# the second group has no description, so its export matches an export that renames the first
run 0 group-two groups create "$G2"
run 0 group-none groups create "$G3" -d "thorctl end to end"

echo "== convert and validate the real toolbox"
# toolbox.v1.json is the legacy toolbox kept from before the manifests became tools, and
# converting it gives the committed toolbox.json
run 0 convert-check toolbox convert "$TOOLS/toolbox.v1.json" --check -o "$TOOLS/toolbox.json"
run 0 convert-write toolbox convert "$TOOLS/toolbox.v1.json" -o "$WORK/converted.json"
cmp -s "$WORK/converted.json" "$TOOLS/toolbox.json" || fail "the converted toolbox differs from the committed one"
run 0 validate-legacy toolbox validate "$TOOLS/toolbox.v1.json"
run 0 validate-v2 toolbox validate "$TOOLS/toolbox.json"

echo "== import, diff, and the no-op re-import"
run 0 import-dry-run toolbox import "$TOOLS/toolbox.json" --group-override "$G1" --dry-run
run 0 import toolbox import "$TOOLS/toolbox.json" --group-override "$G1" -o json
saw '"outcome": *"applied"'
run 0 import-again toolbox import "$TOOLS/toolbox.json" --group-override "$G1" -o json
saw '"outcome": *"nothing_to_do"'
run 0 import-legacy-again toolbox import "$TOOLS/toolbox.v1.json" --group-override "$G1" -o json
saw '"outcome": *"nothing_to_do"'
run 0 diff-clean toolbox diff "$TOOLS/toolbox.json" --group-override "$G1" --exit-code
run 0 diff-clean-legacy toolbox diff "$TOOLS/toolbox.v1.json" --group-override "$G1" --exit-code
run 1 diff-drift toolbox diff "$TOOLS/toolbox.json" --group-override "$G3" --exit-code

echo "== export, import the export into a second group, and compare"
run 0 export toolbox export -g "$G1" --all-kinds --group-override "$G2" -o "$WORK/export-one"
run 0 export-validate toolbox validate "$WORK/export-one/toolbox.json"
run 0 export-import toolbox import "$WORK/export-one" -o json
saw '"outcome": *"applied"'
run 0 export-diff toolbox diff "$WORK/export-one" --exit-code
run 0 export-two toolbox export -g "$G2" --all-kinds -o "$WORK/export-two"
cmp -s "$WORK/export-one/toolbox.json" "$WORK/export-two/toolbox.json" \
  || fail "exporting the copy does not give the first export back"

echo "== tools: create, publish, edit in an editor and from flags, spec"
run 0 tools-get tools get -g "$G1" -l 1 -o json
BASE=$(grep -o '"name": *"[^"]*"' "$WORK/last.out" | head -1 | sed 's/.*: *"\(.*\)"/\1/')
[ -n "$BASE" ] || fail "the imported group lists no tools"
run 0 tools-spec tools spec "$G1/$BASE" -o "$WORK/base-spec.yaml"
run 0 tools-create tools create --spec "$WORK/base-spec.yaml" -n e2e-scratch -g "$G1" -v 1.0.0 -d "scratch tool"
run 0 tools-edit-set tools edit e2e-scratch --set Image.execution.timeout=99 --bump patch
run 0 tools-edit-patch tools edit e2e-scratch --patch '{"Image":{"execution":{"timeout":98}}}' --version 1.1.0
run 0 tools-versions tools versions e2e-scratch -o json
saw '1\.0\.1'
saw '1\.1\.0'
EDITOR_SCRIPT=$(editor 'timeout: 98' 'timeout: 97')
run 0 tools-edit-interactive tools edit e2e-scratch --editor "$EDITOR_SCRIPT" --bump patch
run 0 tools-spec-latest tools spec e2e-scratch
saw 'timeout: 97'
run 0 tools-update tools update e2e-scratch --set description=updated
run 0 tools-describe tools describe e2e-scratch --format json
saw '"description": *"updated"'

echo "== toolset, agent, workflow"
cat >"$WORK/native.yaml" <<'YAML'
Agentic:
  function_name: e2e_native
  model_description: Gets a sample from Thorium
  input_schema:
    type: object
    properties:
      sha256:
        type: string
  backing:
    ThoriumMcp:
      tool: get_sample
  timeout: 60
YAML
run 0 native-create tools create --spec "$WORK/native.yaml" -n e2e-native -g "$G1" -v 1.0.0
run 0 toolset-create toolsets create e2e-kit -g "$G1" --tool e2e-native -d kit
run 0 toolset-update toolsets update e2e-kit --set description=kit-two
run 0 toolset-edit toolsets edit e2e-kit --editor "$(editor 'description: kit-two' 'description: kit-three')"
cat >"$WORK/agent.yaml" <<'YAML'
system_prompt: You triage files
function_name: e2e_lead
model_description: Leads a triage
max_turns: 5
YAML
run 0 agent-create agents create -n e2e-lead -g "$G1" --config "$WORK/agent.yaml" --tool e2e-native --toolset e2e-kit --model-auth group --model-auth user
run 0 agent-update agents update e2e-lead --model-auth group
run 0 agent-edit agents edit e2e-lead --set config.max_turns=6 --yes
run 0 agent-spec agents spec e2e-lead
saw 'max_turns: 6'
printf 'name: e2e-flow\ngroups: [%s]\nagents: [e2e-lead]\nentry_agent: e2e-lead\n' "$G1" >"$WORK/workflow.yaml"
run 0 workflow-create workflows create --spec "$WORK/workflow.yaml"
run 0 workflow-update workflows update e2e-flow --max-alive-sub-agents 3
run 0 workflow-edit workflows edit e2e-flow --set auto_instance=false --yes
run 0 workflow-spec workflows spec e2e-flow
saw 'max_alive_sub_agents: 3'
run 0 instances-request instances request workflow e2e-flow
saw 'instance-revision=1'

echo "== pipeline: create, change stage tools, run"
run 0 pipeline-create pipelines create e2e-pipe -g "$G1" --stage fetch --stage scan --stage-tool "fetch=$BASE" --stage-tool "scan=e2e-scratch@1.0.0" --sla 100
run 0 pipeline-update pipelines update e2e-pipe -g "$G1" --sla 200 --stage-tool "scan=e2e-scratch"
run 0 pipeline-edit pipelines edit e2e-pipe -g "$G1" --set sla=300 --yes
run 0 pipeline-describe pipelines describe e2e-pipe -g "$G1"
saw 'SLA: 300s'
run 0 pipeline-stage-tool pipelines stage-tool e2e-pipe scan "$BASE" -g "$G1"
head -c 1024 /dev/urandom >"$WORK/sample.bin"
SHA=$(sha256sum "$WORK/sample.bin" | cut -d' ' -f1)
run 0 sample-upload files upload --groups "$G1" "$WORK/sample.bin"
run 0 pipeline-run pipelines run e2e-pipe -g "$G1" --sample "$SHA" --tag e2e --sla 120
RUN_ID=$(grep -Eo '[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}' "$WORK/last.out" | head -1)
[ -n "$RUN_ID" ] || fail "pipelines run printed no reaction id"
run 0 run-describe runs describe "$RUN_ID" -g "$G1"
saw 'e2e-pipe'
run 3 pipeline-run-wait-times-out pipelines run e2e-pipe -g "$G1" --sample "$SHA" --wait --timeout 1s --interval 200ms
saw 'Timed out'
if [ -n "${THORCTL_E2E_WAIT:-}" ]; then
  run 0 pipeline-run-completes pipelines run e2e-pipe -g "$G1" --sample "$SHA" --wait --logs --timeout "$THORCTL_E2E_WAIT"
  saw 'Completed'
fi
echo "== batch runs: one run per input, followed and cancelled by batch name"
head -c 1024 /dev/urandom >"$WORK/sample2.bin"
SHA2=$(sha256sum "$WORK/sample2.bin" | cut -d' ' -f1)
run 0 sample2-upload files upload --groups "$G1" "$WORK/sample2.bin"
run 0 run-dry-run run pipeline e2e-pipe -g "$G1" --sample "$SHA" --sample "$SHA2" --one-per-input --batch "e2e-dry-$SUFFIX" --dry-run
saw 'would start 2 runs of pipeline'
saw 'Nothing was started'
run 0 run-batch-start run pipeline e2e-pipe -g "$G1" --one-per-input --batch "e2e-batch-$SUFFIX" --sample-list - <<<"$SHA,$SHA2"
saw 'Batch: e2e-batch-'
saw 'Batch e2e-batch-.*: 2 inputs'
run 0 runs-get-batch runs get --batch "e2e-batch-$SUFFIX" -g "$G1" --kind pipeline -o json
saw "$SHA"
run 0 run-batch-cancel-dry runs cancel --batch "e2e-batch-$SUFFIX" -g "$G1" --dry-run
saw 'Would fail 2 runs'
run 0 run-batch-cancel runs cancel --batch "e2e-batch-$SUFFIX" -g "$G1" --skip-confirm
saw 'Failed 2 of 2 runs'
run 3 run-batch-wait-times-out run pipeline e2e-pipe -g "$G1" --sample "$SHA" --one-per-input --wait --timeout 1s --interval 200ms --batch "e2e-wait-$SUFFIX" -o json
saw 'Unfinished:'
run 0 run-batch-cancel-wait runs cancel --batch "e2e-wait-$SUFFIX" -g "$G1" --skip-confirm
run 0 pipeline-stage-tool-remove pipelines stage-tool e2e-pipe scan --remove -g "$G1"

echo "== clean up: delete what was built, then remove both toolboxes"
run 0 workflow-delete workflows delete e2e-flow --yes
run 0 agent-delete agents delete e2e-lead --yes
run 0 toolset-delete toolsets delete e2e-kit --yes
run 0 pipeline-delete pipelines delete e2e-pipe -g "$G1" --skip-confirm
run 0 tools-delete tools delete e2e-scratch --all-versions --yes
run 0 native-delete tools delete e2e-native --all-versions --yes
run 1 remove-needs-confirmation toolbox remove "$TOOLS/toolbox.json" --group-override "$G1"
run 0 remove-dry-run toolbox remove "$TOOLS/toolbox.json" --group-override "$G1" --dry-run
run 0 remove-one toolbox remove "$TOOLS/toolbox.json" --group-override "$G1" --skip-confirm
run 0 remove-two toolbox remove "$WORK/export-one" --yes
run 0 remove-again toolbox remove "$WORK/export-one" --yes

echo "PASSED: $STEPS steps (scratch groups $G1, $G2, $G3 remain; groups are never deleted)"
