#!/usr/bin/env bash
# Clip: launch a hunt over the group's historical files and watch its lanes run to
# completion. Embedded in users/hunts.md as hunt-run.cast.
#
# The hunt freezes the screenshot session's marker rule (demo_downloader_c2, created
# by the shots half of the capture session) so it provably finds hits; the rule is
# targeted by name, so nothing is read from the session's state.json anymore.
cd "$(dirname "${BASH_SOURCE[0]}")"
source ./lib.sh

# hunt with the marker rule by name so the hunt provably finds hits
type_cmd "thorctl hunts create --name demo-corpus-sweep --groups demolab --rules demo_downloader_c2"
# wait off camera for the fresh hunt to resolve by name before watching it
await_name thorctl hunts status demo-corpus-sweep
# watch lane progress tick until the hunt completes
type_cmd "thorctl hunts status demo-corpus-sweep --watch --refresh 5"

# later clips target the hunt by name, so no id handoff is needed
