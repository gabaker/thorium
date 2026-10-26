#!/usr/bin/env bash
# Regenerates the `thorctl analyze chat` demo recordings embedded in
# api/docs/src/users/ai/ai.md (vendored under
# api/docs/src/static_resources/asciinema/):
#
#   main       analyze-chat.cast     fresh full-prompt session + /report (costs ~$60-100)
#   forum      chat-forum.cast       the forum sidebar          (free: resume replay)
#   search     chat-search.cast      the forum search picker    (free: resume replay)
#   usage      chat-usage.cast       the usage overlay          (free: resume replay)
#   timeline   chat-timeline.cast    the gantt timeline         (free: resume replay)
#   traversal  chat-traversal.cast   the traversal graph        (free: resume replay)
#   report     resume-report.cast    resume the main session + /report (one round trip)
#   boot       (no cast) start the resumed TUI the view clips attach to
#   stop       (no cast) tear the tmux session down
#   all        main, then report, then boot + every view clip + stop
#
# The view clips resume an already-finished session (default: the batch demo's
# batch1-stealc_v2), so retakes are free: re-run `boot`, then the one clip.
# The TUI is driven through tmux send-keys and recorded by attaching asciinema
# to the tmux session; a recording ends when the attached client detaches.
#
# Prerequisites:
#   * `cargo build --release -p thorctl` at the commit being documented
#   * the batch demo dir with its finished run (see record-analyze-batch.sh);
#     the resumed session's Ghidra project lives under <demo-dir>/GhidraProjects,
#     so always run this script with the demo dir intact
#   * fresh AWS creds in ~/.thorium/bedrock.env (only `main` spends tokens)
#   * asciinema >= 3.x and tmux
#
# Usage: record-analyze-chat.sh <main|boot|forum|search|usage|timeline|traversal|stop|all>
# Env overrides: THORDEMO_DIR, THORDEMO_OUT, THORDEMO_SESSION, THORDEMO_RESUME
set -euo pipefail

# --- configuration -----------------------------------------------------------
SESSION="${THORDEMO_SESSION:-thordemo}"
DEMO="${THORDEMO_DIR:-/home/ubuntu/thorctl-batch-demo}"
REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../../.." && pwd)"
THORCTL="${REPO_ROOT}/target/release/thorctl"
ASCIINEMA="$(command -v asciinema || echo "${HOME}/.cargo/bin/asciinema")"
OUT_DIR="${THORDEMO_OUT:-${DEMO}}"
# the TUI needs more than 80 columns to be readable (the forum board alone
# takes two fifths of the width), so record a 120x34 terminal
COLS=120 ROWS=34
SAMPLE="samples/3caaaa1451f4ef6a6af13c99a0e4f1704e5337e8a7557b6d45ab82ff691583c4"
CHAT_SESSION="docs-demo-chat"
RESUME_SESSION="${THORDEMO_RESUME:-batch1-stealc_v2}"
REC_PID=""

# --- helpers -----------------------------------------------------------------

# md5 of the visible pane, used to detect an idle (pixel-static) TUI
pane_hash() { tmux capture-pane -pt "$SESSION" | md5sum | cut -d' ' -f1; }

# the resume/clip takes rely on the ai block `main` persists into the default
# thorctl config on camera; refuse to run before that setup exists
require_ai_config() {
    if ! grep -q '^ai:' "${HOME}/.thorium/config.yml"; then
        echo "~/.thorium/config.yml has no ai block yet - run '$0 main' first" >&2
        exit 1
    fi
}

# strip any ai block out of the default thorctl config so a retake of `main`
# can append it on camera again without creating duplicate keys
reset_demo_config() {
    local cfg="${HOME}/.thorium/config.yml"
    # keep a one-time backup of the pre-demo config
    [[ -f "${cfg}.bak-pre-ai-demo" ]] || cp "$cfg" "${cfg}.bak-pre-ai-demo"
    # drop the top-level ai mapping (bare or populated) but nothing else
    awk 'BEGIN{skip=0} /^ai:/{skip=1; next} skip && /^[^ \t]/{skip=0} !skip{print}' \
        "$cfg" > "${cfg}.tmp" && mv "${cfg}.tmp" "$cfg"
}

# wait until the screen stops changing: N identical captures <interval>s apart
# (the TUI's redraw ticker only runs while agents work or a replay animates,
# so a stable screen means the turn is over); gives up after <cap> polls
wait_idle() {
    local interval=$1 cap=$2
    local last="" same=0 tries=0
    while [[ $same -lt 3 && $tries -lt $cap ]]; do
        sleep "$interval"
        local h
        h=$(pane_hash)
        if [[ "$h" == "$last" ]]; then same=$((same + 1)); else same=0; fi
        last="$h"
        tries=$((tries + 1))
    done
    if [[ $same -lt 3 ]]; then
        echo "wait_idle: screen never stabilized (${tries} polls)" >&2
        return 1
    fi
}

# send one named key (Enter, Escape, Space, Tab, Down, C-c, ...)
key() { tmux send-keys -t "$SESSION" -- "$1"; }

# wait until a literal string shows up in the pane (for shell beats whose
# quiet network wait would fool the stable-screen check)
wait_for_text() {
    local pat=$1 cap=$2 waited=0
    while ! tmux capture-pane -pt "$SESSION" | grep -qaF "$pat"; do
        sleep 2
        waited=$((waited + 2))
        if [[ $waited -ge $cap ]]; then
            echo "wait_for_text: '$pat' never appeared (${cap}s)" >&2
            return 1
        fi
    done
}

# send literal characters one at a time for a natural typing cadence
type_text() {
    local s=$1 i
    for ((i = 0; i < ${#s}; i++)); do
        tmux send-keys -t "$SESSION" -l -- "${s:$i:1}"
        sleep 0.05
    done
}

# start recording the tmux session into a cast file
rec_start() {
    REC_CAST=$1
    "$ASCIINEMA" rec --headless --overwrite \
        --window-size "${COLS}x${ROWS}" --idle-time-limit 2 \
        --command "tmux attach -t ${SESSION}" "$REC_CAST" &
    REC_PID=$!
    sleep 2
}

# stop recording by detaching the attached client, then drop the tmux
# detach banner the client prints on its way out
rec_stop() {
    sleep 2
    tmux detach-client -s "$SESSION"
    wait "$REC_PID"
    python3 -I "$(dirname "${BASH_SOURCE[0]}")/strip-detach.py" "$REC_CAST"
}

# --- the main chat cast ------------------------------------------------------

# record a fresh real-world session over the staged Stealc v2 sample: persist
# the ai settings into the default thorctl config on camera, paste the full
# evaluation prompt straight into the home screen, let the agents run the
# whole analysis, then write a report with /report. Run from the demo dir so
# the batch run's Ghidra project is reused (fast boot). The cast is kept at
# full length — no trimming or speed-up.
cmd_main() {
    # retakes need a clean config append, session name, and report path
    reset_demo_config
    rm -rf "${HOME}/.thorium/sessions/${CHAT_SESSION}"
    rm -f "${DEMO}/report.md"
    tmux kill-session -t "$SESSION" 2>/dev/null || true
    tmux new-session -d -s "$SESSION" -c "$DEMO" -x "$COLS" -y "$ROWS" \
        "env PS1='\$ ' bash --noprofile --norc"
    tmux set -t "$SESSION" status off
    # off camera: resolve the repo thorctl as plain `thorctl` and load the AWS
    # credential chain (sourcing prints nothing), then clear
    type_text "export PATH=${REPO_ROOT}/target/release:\$PATH; set -a; . ~/.thorium/bedrock.env; set +a; clear"
    key Enter
    sleep 1
    rec_start "${OUT_DIR}/analyze-chat.cast"
    # one-time setup, on camera: persist the ai settings into the default
    # thorctl config so every later command is plain `thorctl analyze ...`
    type_text 'cat >> ~/.thorium/config.yml <<EOF'
    key Enter
    local line
    for line in \
        'ai:' \
        '  provider: bedrock' \
        '  bedrock:' \
        '    mantle: true' \
        '    model: anthropic.claude-mythos-5' \
        '  ghidra_install: /home/ubuntu/.local/share/ghidra-cli/ghidra/ghidra_12.1.4_PUBLIC' \
        '  yara_rules: /home/ubuntu/thorpal-eval/toolbox/yara-rules/rules' \
        'EOF'; do
        type_text "$line"
        key Enter
    done
    sleep 1
    # prove the provider is reachable before starting the session
    type_text 'thorctl analyze check'
    key Enter
    wait_for_text 'AI provider check passed' 120
    sleep 2
    # start the session; the home screen takes the prompt straight away
    type_text "thorctl analyze chat ${SAMPLE} --name ${CHAT_SESSION}"
    sleep 1
    key Enter
    # give the TUI its first frames, then paste the full evaluation prompt
    # into the home prompt box, which is already focused at startup; Enter
    # queues the question while Ghidra/detector seeding still runs
    sleep 5
    tmux load-buffer -b thordemo-prompt "${DEMO}/prompt.md"
    tmux paste-buffer -p -t "$SESSION" -b thordemo-prompt -d
    sleep 3
    key Enter
    # the full analysis: agents work until the screen settles (up to 90 min)
    wait_idle 20 270
    # a short usage-overlay beat
    key Escape
    sleep 1
    key Space
    sleep 1
    key u
    sleep 5
    key q
    sleep 2
    # write a report from the findings, the way a real run ends: type the
    # command, then Esc (drops focus and any slash completion popup), i
    # (refocus), Enter (submits) — Enter straight after typing can hit the
    # popup, which completes instead of submitting
    key i
    sleep 1
    type_text '/report report.md'
    sleep 1
    key Escape
    sleep 1
    key i
    sleep 1
    key Enter
    # report generation does not animate the screen, so wait for the file
    local main_waited=0
    while [[ ! -s "${DEMO}/report.md" && $main_waited -lt 900 ]]; do
        sleep 5
        main_waited=$((main_waited + 5))
    done
    sleep 5
    # quit gracefully (the session persists and stays resumable)
    key C-c
    sleep 3
    rec_stop
    tmux kill-session -t "$SESSION" 2>/dev/null || true
}

# record resuming the finished main session and writing its report: boots the
# saved session (free), then one /report round trip writes report.md
cmd_report() {
    require_ai_config
    tmux kill-session -t "$SESSION" 2>/dev/null || true
    tmux new-session -d -s "$SESSION" -c "$DEMO" -x "$COLS" -y "$ROWS" \
        "env PS1='\$ ' bash --noprofile --norc"
    tmux set -t "$SESSION" status off
    # off camera: resolve the repo thorctl as plain `thorctl` and load the AWS
    # credential chain (sourcing prints nothing), then clear
    type_text "export PATH=${REPO_ROOT}/target/release:\$PATH; set -a; . ~/.thorium/bedrock.env; set +a; clear"
    key Enter
    sleep 1
    # a stale report would raise the overwrite-confirmation modal mid-take
    rm -f "${DEMO}/report.md"
    rec_start "${OUT_DIR}/resume-report.cast"
    type_text "thorctl analyze chat --resume ${CHAT_SESSION}"
    sleep 1
    key Enter
    # the saved history replays instantly; the Ghidra project reopen is quick
    wait_idle 5 120
    # write the report: type the command, then Esc (drops focus and any slash
    # completion popup), i (refocus), Enter (submits) — Enter straight after
    # typing can hit the popup, which completes instead of submitting
    key Escape
    sleep 1
    key i
    sleep 1
    type_text '/report report.md'
    sleep 1
    key Escape
    sleep 1
    key i
    sleep 1
    key Enter
    # report generation does not animate the screen, so wait for the file
    local waited=0
    while [[ ! -s "${DEMO}/report.md" && $waited -lt 900 ]]; do
        sleep 5
        waited=$((waited + 5))
    done
    # let the TUI render its written-report note before quitting
    sleep 5
    key C-c
    sleep 3
    rec_stop
    tmux kill-session -t "$SESSION" 2>/dev/null || true
}

# --- the resumed TUI the view clips attach to --------------------------------

cmd_boot() {
    require_ai_config
    tmux kill-session -t "$SESSION" 2>/dev/null || true
    tmux new-session -d -s "$SESSION" -c "$DEMO" -x "$COLS" -y "$ROWS" \
        "$THORCTL analyze chat --resume ${RESUME_SESSION}"
    tmux set -t "$SESSION" status off
    # replay is instant; the Ghidra project reopen takes a little while
    wait_idle 5 120
    # a resumed session opens with the prompt focused (INSERT); drop to NORMAL
    # so the clips' Space shortcuts open the menu instead of typing a space
    key Escape
    sleep 1
}

cmd_stop() {
    tmux send-keys -t "$SESSION" C-c 2>/dev/null || true
    sleep 2
    tmux kill-session -t "$SESSION" 2>/dev/null || true
}

# --- the view clips (all against the booted resume session) ------------------

cmd_forum() {
    # off camera: make sure the page is in NORMAL mode before recording
    key Escape
    sleep 1
    rec_start "${OUT_DIR}/chat-forum.cast"
    key Space; sleep 1; key m; sleep 3      # slide the board in
    key j; sleep 1; key j; sleep 1; key k; sleep 1
    key Enter; sleep 4                      # unfold the selected post
    key Escape; sleep 1                     # hand the keyboard back
    key Space; sleep 1; key m; sleep 2      # slide the board away
    rec_stop
}

cmd_search() {
    # off camera: make sure the page is in NORMAL mode before recording
    key Escape
    sleep 1
    rec_start "${OUT_DIR}/chat-search.cast"
    key Space; sleep 1; key s; sleep 2      # open the search picker
    type_text 'c2'
    sleep 3                                 # live-ranked hits
    key Down; sleep 1; key Up; sleep 1
    key Enter; sleep 3                      # jump to the hit in the board
    key Escape; sleep 1
    key Space; sleep 1; key m; sleep 2      # close the board
    rec_stop
}

cmd_usage() {
    # off camera: make sure the page is in NORMAL mode before recording
    key Escape
    sleep 1
    rec_start "${OUT_DIR}/chat-usage.cast"
    key Space; sleep 1; key u; sleep 4      # the per-agent cost bars
    key Tab; sleep 1; key Tab; sleep 1
    key Right; sleep 2                      # next metric
    key Enter; sleep 3                      # drill into an agent's chat
    key j; sleep 1; key j; sleep 1
    key Escape; sleep 1                     # close the drill-down
    key q; sleep 1                          # close the overlay
    rec_stop
}

cmd_timeline() {
    # off camera: make sure the page is in NORMAL mode before recording
    key Escape
    sleep 1
    rec_start "${OUT_DIR}/chat-timeline.cast"
    key Space; sleep 1; key t; sleep 3      # the gantt of the whole run
    key j; sleep 1                          # select the first root row (main)
    key j; sleep 1                          # move onto the swarm row
    key Enter; sleep 2                      # expand it: sub-agent rows appear
    key j; sleep 1                          # select the first sub-agent
    key Enter; sleep 2                      # expand it: its tool calls appear
    key j; sleep 1                          # select the first tool call
    key Enter; sleep 3                      # open the call's args/result detail
    key j; sleep 1; key j; sleep 1; key j; sleep 1
    key Escape; sleep 2                     # back out, selection kept
    key +; sleep 2                          # zoom the time window in
    key 0; sleep 2                          # re-fit the whole run
    key q; sleep 1
    rec_stop
}

cmd_traversal() {
    # off camera: make sure the page is in NORMAL mode before recording
    key Escape
    sleep 1
    rec_start "${OUT_DIR}/chat-traversal.cast"
    key Space; sleep 1; key g; sleep 3      # the walked-function graph
    key r; sleep 12                         # replay the walk from the start
    key '>'; sleep 1; key '>'; sleep 4      # double the replay speed twice
    key G; sleep 2                          # jump back to the live state
    key H; sleep 3                          # heat view (visit frequency)
    key q; sleep 1
    rec_stop
}

# --- dispatch ----------------------------------------------------------------

case "${1:?usage: record-analyze-chat.sh <main|boot|forum|search|usage|timeline|traversal|stop|all>}" in
    main) cmd_main ;;
    report) cmd_report ;;
    boot) cmd_boot ;;
    forum) cmd_forum ;;
    search) cmd_search ;;
    usage) cmd_usage ;;
    timeline) cmd_timeline ;;
    traversal) cmd_traversal ;;
    stop) cmd_stop ;;
    all)
        cmd_main
        cmd_report
        cmd_boot
        cmd_forum
        cmd_search
        cmd_usage
        cmd_timeline
        cmd_traversal
        cmd_stop
        ;;
    *) echo "unknown subcommand: $1" >&2; exit 2 ;;
esac
