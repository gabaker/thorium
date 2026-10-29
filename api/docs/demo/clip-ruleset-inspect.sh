#!/usr/bin/env bash
# Clip: create a soon-to-be-retired rule set, inspect an active one, and delete the
# retiree — all by name. Embedded in users/rule_sets.md as ruleset-inspect.cast.
cd "$(dirname "${BASH_SOURCE[0]}")"
source ./lib.sh

# create a scratch set on camera so the later delete has an obvious target
type_cmd "thorctl rulesets create --name demo-retired-pack --groups demolab --rules demo_persistence_check"
# wait off camera for the scratch set to resolve by name before the later delete
await_name thorctl rulesets get demo-retired-pack
# inspect the hunting pack built in the earlier clip, members and all
type_cmd "thorctl rulesets get demo-hunting-pack"
# retire the scratch set; member rules are never deleted with a set
type_cmd "thorctl rulesets delete demo-retired-pack --skip-confirm"
