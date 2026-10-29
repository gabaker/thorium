#!/usr/bin/env bash
# Clip: create a Sigma rule from a YAML file and fetch it back by name (the score
# derives from the rule's `level`). Embedded in users/sigma_rules.md as sigma-create.cast.
cd "$(dirname "${BASH_SOURCE[0]}")"
source ./lib.sh

# show the YAML, create the rule, then fetch it by its name
type_cmd "cat rules/demo_lsass_dump.yml"
type_cmd "thorctl rules create --engine sigma --name demo_lsass_dump --groups demolab --rule-file rules/demo_lsass_dump.yml"
# wait off camera for the fresh rule to resolve by name before fetching it
await_name thorctl rules get demo_lsass_dump
type_cmd "thorctl rules get demo_lsass_dump"
