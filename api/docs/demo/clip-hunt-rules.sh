#!/usr/bin/env bash
# Clip: list the group's hunts and the frozen rules one of them scans with.
# Embedded in users/hunts.md as hunt-rules.cast.
cd "$(dirname "${BASH_SOURCE[0]}")"
source ./lib.sh

# list the group's hunts, then the frozen snapshot of the completed one by name
type_cmd "thorctl hunts list --groups demolab"
type_cmd "thorctl hunts rules demo-corpus-sweep"
