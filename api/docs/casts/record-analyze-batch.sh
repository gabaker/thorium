#!/usr/bin/env bash
# Regenerates the `thorctl analyze batch` demo recording embedded in
# api/docs/src/users/ai/ai.md (vendored at
# api/docs/src/static_resources/asciinema/analyze-batch.cast).
#
# This drives a LIVE two-unit evaluation (the corpus Stealc v2 and QUICKBIND
# samples) with the full RE prompt against the provider configured in
# ~/.thorium/thorpal-ai.yml. Expect ~20-40 minutes of wall time and roughly
# $130 of LLM spend at run-7 rates.
#
# Prerequisites:
#   * `cargo build --release -p thorctl` at the commit being documented
#     (the recording runs the repo build, displayed as plain `thorctl`)
#   * a demo dir (e.g. /home/ubuntu/thorctl-batch-demo) holding batch.yml,
#     prompt.md, and dataset/<the two password-protected zips>; see the
#     Batch Runs docs section for the manifest
#   * fresh AWS creds in ~/.thorium/bedrock.env (preflight:
#     `. ~/.thorium/bedrock.env; thorctl --config ~/.thorium/thorpal-ai.yml analyze check`)
#   * asciinema >= 3.x on PATH or at ~/.cargo/bin/asciinema
#
# Re-recording a clean take requires wiping the previous run's state:
#   rm -rf <demo-dir>/runs <demo-dir>/samples <demo-dir>/dataset-index.json \
#          <demo-dir>/GhidraProjects ~/.thorium/sessions/batch1-stealc_v2 \
#          ~/.thorium/sessions/batch1-quickbind
#
# Usage: record-analyze-batch.sh <demo-dir> <out.cast>
set -euo pipefail

if [[ "${BATCH_DEMO_INNER:-}" != "1" ]]; then
    demo_dir="${1:?usage: record-analyze-batch.sh <demo-dir> <out.cast>}"
    out_cast="${2:?usage: record-analyze-batch.sh <demo-dir> <out.cast>}"
    # the recorded commands use the default config, so its ai block (persisted
    # on camera by record-analyze-chat.sh main) must already exist
    if ! grep -q '^ai:' "${HOME}/.thorium/config.yml"; then
        echo "~/.thorium/config.yml has no ai block yet - run record-analyze-chat.sh main first" >&2
        exit 1
    fi
    # the cast must show a clean run: refuse to record if the pool's disk
    # guard would hold launches (and print its hold lines) mid-take
    free_gb=$(df -BG --output=avail / | tail -1 | tr -dc '0-9')
    if (( free_gb < 20 )); then
        echo "only ${free_gb}G free on / - free space above 20G so the pool never disk-holds on camera" >&2
        exit 1
    fi
    # the recording shows plain `thorctl`, resolved to the repo build
    repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../../.." && pwd)"
    export PATH="${repo_root}/target/release:${PATH}"
    asciinema_bin="$(command -v asciinema || echo "${HOME}/.cargo/bin/asciinema")"
    BATCH_DEMO_INNER=1 exec "${asciinema_bin}" rec \
        --headless --return --overwrite \
        --window-size 110x30 \
        --idle-time-limit 2 \
        --title "thorctl analyze batch: a two-unit live evaluation" \
        --command "bash '${BASH_SOURCE[0]}' '${demo_dir}'" \
        "${out_cast}"
fi

# ---- inner stage: the scripted session asciinema records ----
cd "${1:?inner stage needs the demo dir}"

# print a fake prompt, "type" the command, then run it for real
type_run() {
    printf '$ '
    printf '%s' "$1" | perl -e 'use Time::HiRes qw(usleep); $|=1; while (read(STDIN, $c, 1)) { print $c; usleep(35_000); }'
    printf '\n'
    eval "$1"
}

type_run 'cat batch.yml'
sleep 2
type_run 'thorctl analyze batch run --dir . --dry-run'
sleep 2
type_run 'thorctl analyze batch run --dir .'
sleep 1
type_run 'thorctl analyze batch status --dir .'
sleep 1
type_run 'ls runs/1/'
sleep 3
