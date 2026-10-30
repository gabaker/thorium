#!/usr/bin/env bash
# Regenerates the `thorctl analyze batch` demo recording embedded in
# api/docs/src/users/ai/ai.md (vendored at
# api/docs/src/static_resources/asciinema/analyze-batch.cast).
#
# This drives a LIVE BLIND two-unit evaluation (the corpus Stealc v2 and
# QUICKBIND samples) with the full RE prompt. Expect ~50-70 minutes of wall
# time and roughly $220 of LLM spend at run-10 rates.
#
# Blindness: the pool and its child sessions run with every cross-run store
# pointed at fresh demo-local dirs via off-camera THORCTL__AI__* env overlays
# (Ghidra projects root, memory library, script library), so the children
# import their Ghidra projects cold and recall no prior-run intel - exactly
# what a new user's first campaign looks like. The provider config (and the
# model id) never appears on camera: the ai block lives in the default
# thorctl config, the manifest's env_file is the neutral demo-local
# `creds.env` (a symlink to the operator's real credential file), and the
# finished cast is gated by sanitize-cast.py, which fails the take on any
# model/provider/credential string.
#
# Prerequisites:
#   * a thorctl build at the commit being documented (target/release, or the
#     musl target as fallback; the recording shows plain `thorctl`)
#   * a FRESH batch demo dir (default /home/ubuntu/thorctl-demo/batch)
#     holding batch.yml, prompt.md, creds.env, and dataset/<the two
#     password-protected zips>, with EMPTY GhidraProjects/memories/scripts
#     and no runs/ or dataset-index.json - the blind guard refuses a dirty
#     dir and never deletes anything to fix it
#   * an `ai:` block in ~/.thorium/config.yml naming the provider
#   * fresh AWS creds behind creds.env
#   * asciinema >= 3.x on PATH or at ~/.cargo/bin/asciinema
#
# Usage: record-analyze-batch.sh [<demo-dir>] [<out.cast>]
set -euo pipefail

if [[ "${BATCH_DEMO_INNER:-}" != "1" ]]; then
    demo_dir="${1:-/home/ubuntu/thorctl-demo/batch}"
    out_cast="${2:-${demo_dir}/analyze-batch.cast}"
    script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
    # the recorded commands use the default config's ai block for the provider
    if ! grep -q '^ai:' "${HOME}/.thorium/config.yml"; then
        echo "~/.thorium/config.yml has no ai block - configure a provider first" >&2
        exit 1
    fi
    # the cast must show a clean run: refuse to record if the pool's disk
    # guard would hold launches (and print its hold lines) mid-take
    free_gb=$(df -BG --output=avail / | tail -1 | tr -dc '0-9')
    if (( free_gb < 20 )); then
        echo "only ${free_gb}G free on / - free space above 20G so the pool never disk-holds on camera" >&2
        exit 1
    fi
    # a blind take needs a fresh campaign dir and fresh demo-local stores;
    # never delete anything to get them - retakes use a fresh demo dir
    for stale in runs dataset-index.json; do
        if [[ -e "${demo_dir}/${stale}" ]]; then
            echo "${demo_dir}/${stale} exists - a blind take needs a fresh demo dir" >&2
            exit 1
        fi
    done
    for sub in GhidraProjects memories scripts; do
        mkdir -p "${demo_dir}/${sub}"
        if [[ -n "$(ls -A "${demo_dir}/${sub}")" ]]; then
            echo "${demo_dir}/${sub} is not empty - a blind take needs fresh stores" >&2
            exit 1
        fi
    done
    # the manifest's name_template must not collide with saved sessions (the
    # pool would error on camera); this script never deletes them. Render the
    # run-1 names the pool will claim: shuffled sNN aliases by default, the
    # unit names verbatim when the manifest pins `blind: false`.
    name_tpl=$(sed -n 's/^name_template: *"\([^"]*\)".*/\1/p' "${demo_dir}/batch.yml")
    if [[ -z "$name_tpl" ]]; then name_tpl='batch{run}-{alias}'; fi
    if grep -q '^blind: *false' "${demo_dir}/batch.yml"; then
        aliases=$(sed -n 's/^ *- name: *\([^ #]*\).*/\1/p' "${demo_dir}/batch.yml")
    else
        aliases='s[0-9][0-9]'
    fi
    for alias in $aliases; do
        name_glob="${name_tpl/\{run\}/1}"
        name_glob="${name_glob/\{alias\}/${alias}}"
        if compgen -G "${HOME}/.thorium/sessions/${name_glob}" > /dev/null; then
            echo "sessions matching '${name_glob}' already exist - remove them yourself or change name_template" >&2
            exit 1
        fi
    done
    if [[ ! -r "${demo_dir}/creds.env" ]]; then
        echo "${demo_dir}/creds.env is missing or unreadable" >&2
        exit 1
    fi
    # the recording shows plain `thorctl`, resolved to the repo build (native
    # release first, the static musl build as fallback)
    repo_root="$(cd "${script_dir}/../../.." && pwd)"
    if [[ -x "${repo_root}/target/release/thorctl" ]]; then
        export PATH="${repo_root}/target/release:${PATH}"
    else
        export PATH="${repo_root}/target/x86_64-unknown-linux-musl/release:${PATH}"
    fi
    # the blind overlays ride plain env: the pool and every child it spawns
    # inherit them, so all Ghidra projects, memories, and scripts stay local
    # to the demo dir and the operator's real stores are never touched
    export THORCTL__AI__GHIDRA_PROJECTS="${demo_dir}/GhidraProjects"
    export THORCTL__AI__MEMORIES_DIR="${demo_dir}/memories"
    export THORCTL__AI__SCRIPTS_DIR="${demo_dir}/scripts"
    # prove the provider is reachable before spending a take on it, off
    # camera (the check output names the model id, which must never ship)
    if ! check_out=$(bash -c "set -a; . '${demo_dir}/creds.env'; set +a; exec thorctl analyze check" 2>&1) \
        || ! grep -q 'AI provider check passed' <<<"$check_out"; then
        echo "preflight 'thorctl analyze check' failed:" >&2
        echo "$check_out" >&2
        exit 1
    fi
    asciinema_bin="$(command -v asciinema || echo "${HOME}/.cargo/bin/asciinema")"
    BATCH_DEMO_INNER=1 "${asciinema_bin}" rec \
        --headless --return --overwrite \
        --window-size 150x42 \
        --idle-time-limit 2 \
        --title "thorctl analyze batch: a two-unit live evaluation" \
        --command "bash '${BASH_SOURCE[0]}' '${demo_dir}'" \
        "${out_cast}"
    # drop the header's recorded command line and fail the take on any leak
    exec python3 -I "${script_dir}/sanitize-cast.py" "${out_cast}"
fi

# ---- inner stage: the scripted session asciinema records ----
cd "${1:?inner stage needs the demo dir}"

# the pool itself never calls the provider, but source the credential chain
# anyway so a `--dry-run`'s config resolution matches a real user's shell
set -a
. ./creds.env
set +a

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
