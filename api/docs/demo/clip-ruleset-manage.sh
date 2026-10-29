#!/usr/bin/env bash
# Clip: create a rule set, add another member, and list the group's rule sets —
# the set and its member rules are all targeted by name.
# Embedded in users/rule_sets.md as ruleset-manage.cast.
cd "$(dirname "${BASH_SOURCE[0]}")"
source ./lib.sh

# wait off camera for the earlier clips' member rules to resolve by name
await_name thorctl rules get demo_keylogger_strings
await_name thorctl rules get demo_packer_check
# create the pack from two rules created in the earlier clips, picked by name;
# the create command types across two lines so it fits the recording width
type_cmd 'thorctl rulesets create --name demo-hunting-pack --groups demolab \
  --rules demo_keylogger_strings,demo_packer_check'
# wait off camera for the fresh set to resolve by name before updating it
await_name thorctl rulesets get demo-hunting-pack
# grow the membership, trim it back, and show the final count
type_cmd "thorctl rulesets update demo-hunting-pack --add-rules demo_persistence_check"
type_cmd "thorctl rulesets update demo-hunting-pack --remove-rules demo_packer_check"
type_cmd "thorctl rulesets list --groups demolab"

# later clips target the pack by name, so no id handoff is needed
