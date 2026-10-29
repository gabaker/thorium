#!/usr/bin/env bash
# Clip: bulk-import a directory of YARA rules and show the compatibility report.
# Embedded in users/yara_rules.md as rules-import.cast.
cd "$(dirname "${BASH_SOURCE[0]}")"
source ./lib.sh

# show the corpus, then import it — every rule lands and reports its new id
type_cmd "ls rules/import/"
type_cmd "thorctl rules import rules/import/ --groups demolab"

# later clips target the imported rules by name, so no id handoffs are needed
