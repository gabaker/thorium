#!/usr/bin/env bash
# Clip: bump a rule's score with a flag-driven update, confirm it, and delete a retired
# rule — every rule is targeted by name.
# Embedded in users/yara_rules.md as rules-update.cast.
cd "$(dirname "${BASH_SOURCE[0]}")"
source ./lib.sh

# raise the persistence rule's score and show the updated summary
type_cmd "thorctl rules update demo_persistence_check --score 65"
type_cmd "thorctl rules get demo_persistence_check"
# the packer check was dropped from the pack earlier, so retire it entirely
type_cmd "thorctl rules delete demo_packer_check --skip-confirm"
