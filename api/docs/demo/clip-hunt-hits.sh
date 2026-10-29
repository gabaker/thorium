#!/usr/bin/env bash
# Clip: list a completed hunt's findings, then launch a rule-set hunt, cancel it, and
# delete the cancelled hunt — hunts and the rule set are all targeted by name.
# Embedded in users/hunts.md as hunt-hits.cast.
cd "$(dirname "${BASH_SOURCE[0]}")"
source ./lib.sh

# the exact findings record for the hunt recorded in hunt-run.cast
type_cmd "thorctl hunts hits demo-corpus-sweep"
# a second hunt selecting a whole rule set, cancelled before it finishes
type_cmd "thorctl hunts create --name demo-pack-sweep --groups demolab --ruleset demo-hunting-pack"
# wait off camera for the fresh hunt to resolve by name before cancelling it
await_name thorctl hunts status demo-pack-sweep
type_cmd "thorctl hunts cancel demo-pack-sweep --skip-confirm"
# retire the cancelled hunt; its hits and HuntId tags stay behind
type_cmd "thorctl hunts delete demo-pack-sweep --skip-confirm"
