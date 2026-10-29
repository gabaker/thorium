#!/usr/bin/env bash
# Clip: create a YARA rule from a file and list the group's rules.
# Embedded in users/yara_rules.md as rules-create.cast.
cd "$(dirname "${BASH_SOURCE[0]}")"
source ./lib.sh

# show the rule source, create it, then list the demo group's rules; the create
# command types across two lines so it fits the recording width
type_cmd "cat rules/demo_keylogger_strings.yar"
type_cmd 'thorctl rules create --engine yara --name demo_keylogger_strings --groups demolab \
  --rule-file rules/demo_keylogger_strings.yar --score 40'
type_cmd "thorctl rules list --groups demolab"

# later clips target this rule by name, so no id handoff is needed
