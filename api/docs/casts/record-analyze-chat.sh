#!/usr/bin/env bash
# Regenerates the `thorctl analyze chat` demo recordings embedded in
# api/docs/src/users/ai/ai.md (vendored under
# api/docs/src/static_resources/asciinema/):
#
#   main       analyze-chat.cast     fresh BLIND full-prompt session + /report
#   report     resume-report.cast    resume the main session + /report (one round trip)
#   boot       (no cast) start the resumed TUI the view clips attach to
#   forum      chat-forum.cast       the forum sidebar          (free: resume replay)
#   search     chat-search.cast      the forum search picker    (free: resume replay)
#   usage      chat-usage.cast       the usage overlay          (free: resume replay)
#   timeline   chat-timeline.cast    the gantt timeline         (free: resume replay)
#   traversal  chat-traversal.cast   the traversal graph        (free: resume replay)
#   stop       (no cast) tear the tmux session down
#   all        main, report, then boot + every view clip + stop
#
# Blindness: a published take must show what a NEW user sees, so `main` runs
# with every cross-run store pointed at fresh demo-local dirs via off-camera
# THORCTL__AI__* env overlays (Ghidra projects root, memory library, script
# library). No prior project is reused, no family intel is recalled, and the
# operator's real ~/.thorium stores are never read, written, or deleted. The
# provider config (the model id) stays off camera too: the ai block must
# already exist in ~/.thorium/config.yml, `thorctl analyze check` runs as a
# script-side preflight instead of on camera, and the recorded shell is
# cleared (screen AND scrollback) after the setup lines so no redraw can
# repaint them mid-take. Every finished cast is gated by sanitize-cast.py,
# which fails the take on any model/provider/credential string.
#
# Record order across both scripts: `main` first (fresh chat), then
# record-analyze-batch.sh (fresh batch), then `report`, then `boot` + the
# view clips (they resume the batch run's blinded stealc session, resolved
# from its runs/1/aliases.tsv).
#
# Prerequisites:
#   * a thorctl build at the commit being documented (target/release, or the
#     musl target as fallback; override with THORDEMO_THORCTL)
#   * a chat demo dir (default /home/ubuntu/thorctl-demo/chat) holding
#     prompt.md and samples/<sha256> with EMPTY GhidraProjects/memories/
#     scripts dirs — the blind guard refuses a dirty one; retakes need a
#     fresh demo dir (this script never deletes outside the demo dir)
#   * an `ai:` block in ~/.thorium/config.yml naming the provider
#   * fresh AWS creds in ~/.thorium/bedrock.env (only `main` spends tokens)
#   * asciinema >= 3.x and tmux
#
# Usage: record-analyze-chat.sh <main|report|boot|forum|search|usage|timeline|traversal|stop|all>
# Env overrides: THORDEMO_DIR, THORDEMO_BATCH, THORDEMO_OUT, THORDEMO_SESSION,
#                THORDEMO_RESUME, THORDEMO_CREDS, THORDEMO_THORCTL
set -euo pipefail

# --- configuration -----------------------------------------------------------
SESSION="${THORDEMO_SESSION:-thordemo}"
DEMO="${THORDEMO_DIR:-/home/ubuntu/thorctl-demo/chat}"
BATCH_DEMO="${THORDEMO_BATCH:-/home/ubuntu/thorctl-demo/batch}"
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/../../.." && pwd)"
# prefer the native release build, fall back to the static musl one
if [[ -x "${REPO_ROOT}/target/release/thorctl" ]]; then
    THORCTL_DIR="${REPO_ROOT}/target/release"
else
    THORCTL_DIR="${REPO_ROOT}/target/x86_64-unknown-linux-musl/release"
fi
THORCTL="${THORDEMO_THORCTL:-${THORCTL_DIR}/thorctl}"
THORCTL_DIR="$(dirname "$THORCTL")"
CREDS_ENV="${THORDEMO_CREDS:-${HOME}/.thorium/bedrock.env}"
ASCIINEMA="$(command -v asciinema || echo "${HOME}/.cargo/bin/asciinema")"
OUT_DIR="${THORDEMO_OUT:-${DEMO}}"
# a denser grid than the docs page needs at full width: the player's
# fit:'width' scales the font down, so more cols/rows = smaller type and
# more content on screen (see the --cast-max-width knob in the book theme)
COLS=150 ROWS=42
SAMPLE="samples/3caaaa1451f4ef6a6af13c99a0e4f1704e5337e8a7557b6d45ab82ff691583c4"
CHAT_SESSION="docs-demo"
RESUME_SESSION="${THORDEMO_RESUME:-}"
REC_PID=""

# --- helpers -----------------------------------------------------------------

# md5 of the visible pane, used to detect an idle (pixel-static) TUI
pane_hash() { tmux capture-pane -pt "$SESSION" | md5sum | cut -d' ' -f1; }

# the recorded commands rely on the default thorctl config's ai block for the
# provider; refuse to run without one (it is never shown on camera)
require_ai_config() {
    if ! grep -q '^ai:' "${HOME}/.thorium/config.yml"; then
        echo "~/.thorium/config.yml has no ai block - configure a provider first" >&2
        exit 1
    fi
}

# a blind take must start from empty demo-local stores and an unclaimed
# session name; refuse to record otherwise and NEVER delete anything to fix
# it - the operator points THORDEMO_DIR at a fresh dir instead
require_blind_demo_dir() {
    local sub
    for sub in GhidraProjects memories scripts; do
        mkdir -p "${DEMO}/${sub}"
        if [[ -n "$(ls -A "${DEMO}/${sub}")" ]]; then
            echo "${DEMO}/${sub} is not empty - a blind take needs fresh stores;" \
                 "use a fresh THORDEMO_DIR (never wipe live ~/.thorium state)" >&2
            exit 1
        fi
    done
    if [[ -e "${HOME}/.thorium/sessions/${CHAT_SESSION}" ]]; then
        echo "session '${CHAT_SESSION}' already exists - this script never deletes" \
             "sessions; remove it yourself or record under a different name" >&2
        exit 1
    fi
}

# prove the provider is reachable before spending a take on it, off camera
# (the check output names the model id, which must never appear in a cast)
preflight_check() {
    local out
    if ! out=$(env \
        THORCTL__AI__GHIDRA_PROJECTS="${DEMO}/GhidraProjects" \
        THORCTL__AI__MEMORIES_DIR="${DEMO}/memories" \
        THORCTL__AI__SCRIPTS_DIR="${DEMO}/scripts" \
        bash -c "set -a; . '${CREDS_ENV}'; set +a; exec '${THORCTL}' analyze check" 2>&1); then
        echo "preflight 'thorctl analyze check' failed:" >&2
        echo "$out" >&2
        exit 1
    fi
    if ! grep -q 'AI provider check passed' <<<"$out"; then
        echo "preflight check printed no pass line:" >&2
        echo "$out" >&2
        exit 1
    fi
}

# wait until the screen stops changing: <need> identical captures <interval>s
# apart (the TUI's redraw ticker runs while agents work or a replay animates,
# so a stable screen means the turn is over); gives up after <cap> polls.
# The main take uses a long window so a provider backoff lull cannot end it.
wait_idle() {
    local interval=$1 cap=$2 need=${3:-3}
    local last="" same=0 tries=0
    while [[ $same -lt $need && $tries -lt $cap ]]; do
        sleep "$interval"
        local h
        h=$(pane_hash)
        if [[ "$h" == "$last" ]]; then same=$((same + 1)); else same=0; fi
        last="$h"
        tries=$((tries + 1))
    done
    if [[ $same -lt $need ]]; then
        echo "wait_idle: screen never stabilized (${tries} polls)" >&2
        return 1
    fi
}

# send one named key (Enter, Escape, Space, Tab, Down, C-c, ...)
key() { tmux send-keys -t "$SESSION" -- "$1"; }

# send a whole line at once and run it (off camera only - no typing cadence)
line() {
    tmux send-keys -t "$SESSION" -l -- "$1"
    tmux send-keys -t "$SESSION" Enter
    sleep 0.5
}

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

# wait until the TUI has exited and the shell prompt is the last pane line
# (the post-quit Ghidra flush takes a while on a fresh project)
wait_for_prompt() {
    local cap=$1 waited=0 last
    while (( waited < cap )); do
        last=$(tmux capture-pane -pt "$SESSION" | sed 's/[[:space:]]*$//' | awk 'NF{l=$0} END{print l}')
        [[ "$last" == '$' ]] && return 0
        sleep 3
        waited=$((waited + 3))
    done
    echo "wait_for_prompt: shell prompt never returned (${cap}s)" >&2
    return 1
}

# send literal characters one at a time for a natural typing cadence
type_text() {
    local s=$1 i
    for ((i = 0; i < ${#s}; i++)); do
        tmux send-keys -t "$SESSION" -l -- "${s:$i:1}"
        sleep 0.05
    done
}

# the whole off-camera setup for a recorded shell: resolve the repo thorctl
# as plain `thorctl`, point every cross-run store at the demo dir (the blind
# overlays), load the AWS credential chain, advertise the creds file so a
# mid-take rotation self-heals through the pause gate, then scrub the screen
# AND scrollback so no redraw can ever repaint the setup on camera
offcam_prologue() {
    line "export PATH=${THORCTL_DIR}:\$PATH"
    line "export THORCTL__AI__GHIDRA_PROJECTS=${DEMO}/GhidraProjects"
    line "export THORCTL__AI__MEMORIES_DIR=${DEMO}/memories"
    line "export THORCTL__AI__SCRIPTS_DIR=${DEMO}/scripts"
    line "set -a; . ${CREDS_ENV}; set +a"
    line "export THORPAL_CREDS_ENV_FILE=${CREDS_ENV}"
    line "clear"
    sleep 1
    tmux clear-history -t "$SESSION"
    sleep 1
    assert_clean_pane
}

# refuse to start recording unless the pane holds nothing but the prompt
# (the previous take leaked its setup lines through exactly this hole)
assert_clean_pane() {
    local lines
    lines=$(tmux capture-pane -pt "$SESSION" | sed 's/[[:space:]]*$//' | grep -c . || true)
    if [[ "$lines" -gt 1 ]]; then
        echo "pane not clean before recording (${lines} non-empty lines) - aborting" >&2
        tmux capture-pane -pt "$SESSION" >&2
        exit 1
    fi
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

# stop recording by detaching the attached client, then drop the tmux detach
# banner, scrub the header's command line, and fail the take on any leak
rec_stop() {
    sleep 2
    tmux detach-client -s "$SESSION"
    wait "$REC_PID"
    python3 -I "${SCRIPT_DIR}/strip-detach.py" "$REC_CAST"
    python3 -I "${SCRIPT_DIR}/sanitize-cast.py" "$REC_CAST"
}

# start the demo shell every take records into
start_shell() {
    local cwd=$1
    tmux kill-session -t "$SESSION" 2>/dev/null || true
    tmux new-session -d -s "$SESSION" -c "$cwd" -x "$COLS" -y "$ROWS" \
        "env PS1='\$ ' bash --noprofile --norc"
    tmux set -t "$SESSION" status off
    sleep 1
}

# --- the main chat cast ------------------------------------------------------

# record a fresh BLIND real-world session over the staged Stealc v2 sample:
# the provider/stores are set up off camera, the recording opens directly on
# the chat command, the full evaluation prompt is pasted into the home
# screen, the agents run the whole analysis cold (the Ghidra import and
# auto-analysis stream on camera), and after a usage-overlay beat the session
# ends by writing a report with /report. The cast is kept at full length - no
# trimming or speed-up.
cmd_main() {
    require_ai_config
    require_blind_demo_dir
    preflight_check
    rm -f "${DEMO}/report.md"
    start_shell "$DEMO"
    offcam_prologue
    rec_start "${OUT_DIR}/analyze-chat.cast"
    # start the session; the home screen takes the prompt straight away
    type_text "thorctl analyze chat ${SAMPLE} --name ${CHAT_SESSION}"
    sleep 1
    key Enter
    # give the TUI its first frames, then paste the full evaluation prompt
    # into the home prompt box, which is already focused at startup; Enter
    # queues the question while the Ghidra import and detector seeding run
    sleep 5
    tmux load-buffer -b thordemo-prompt "${DEMO}/prompt.md"
    tmux paste-buffer -p -t "$SESSION" -b thordemo-prompt -d
    sleep 3
    key Enter
    # the full cold-start analysis: agents work until the screen settles.
    # 6 x 30s of identical frames are required so a provider-backoff lull
    # can never end the take early (cap: ~2h15m of polling)
    wait_idle 30 270 6
    # a paused session must not be mistaken for a finished one
    if tmux capture-pane -pt "$SESSION" | grep -qa '⏸'; then
        echo "session shows a pause banner - stopping the take for review" >&2
        rec_stop
        exit 1
    fi
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
    # (refocus), Enter (submits) - Enter straight after typing can hit the
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
    # quit gracefully (the session persists and stays resumable) and let the
    # fresh project's Ghidra flush finish on camera
    key C-c
    wait_for_prompt 300 || true
    sleep 2
    rec_stop
    tmux kill-session -t "$SESSION" 2>/dev/null || true
}

# record resuming the finished main session and writing its report: boots the
# saved session (free), then one /report round trip writes report.md
cmd_report() {
    require_ai_config
    start_shell "$DEMO"
    offcam_prologue
    # a stale report would raise the overwrite-confirmation modal mid-take
    rm -f "${DEMO}/report.md"
    rec_start "${OUT_DIR}/resume-report.cast"
    type_text "thorctl analyze chat --resume ${CHAT_SESSION}"
    sleep 1
    key Enter
    # the saved history replays instantly; the Ghidra project reopen is quick
    wait_idle 5 120
    # write the report: type the command, then Esc (drops focus and any slash
    # completion popup), i (refocus), Enter (submits) - Enter straight after
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
    wait_for_prompt 300 || true
    sleep 2
    rec_stop
    tmux kill-session -t "$SESSION" 2>/dev/null || true
}

# --- the resumed TUI the view clips attach to --------------------------------

# the clips resume the batch run's blinded stealc session; its alias is
# shuffled per run, so resolve it from the run's evaluator-side aliases.tsv
resolve_resume_session() {
    if [[ -n "$RESUME_SESSION" ]]; then
        return
    fi
    local tsv="${BATCH_DEMO}/runs/1/aliases.tsv"
    if [[ ! -f "$tsv" ]]; then
        echo "no ${tsv} - run record-analyze-batch.sh first or set THORDEMO_RESUME" >&2
        exit 1
    fi
    # rows are `alias<TAB>unit<TAB>session`; the session column is the name
    RESUME_SESSION=$(awk -F'\t' '$2 == "stealc_v2" {print $3; exit}' "$tsv")
    if [[ -z "$RESUME_SESSION" ]]; then
        echo "no stealc_v2 row in ${tsv} - set THORDEMO_RESUME explicitly" >&2
        exit 1
    fi
}

cmd_boot() {
    require_ai_config
    resolve_resume_session
    tmux kill-session -t "$SESSION" 2>/dev/null || true
    # boot straight into the resumed TUI with the same blind overlays the
    # batch children ran under (nothing here is recorded)
    local boot_cmd
    boot_cmd="set -a; . '${CREDS_ENV}'; set +a; "
    boot_cmd+="export THORCTL__AI__GHIDRA_PROJECTS='${BATCH_DEMO}/GhidraProjects'"
    boot_cmd+=" THORCTL__AI__MEMORIES_DIR='${BATCH_DEMO}/memories'"
    boot_cmd+=" THORCTL__AI__SCRIPTS_DIR='${BATCH_DEMO}/scripts'"
    boot_cmd+=" THORPAL_CREDS_ENV_FILE='${CREDS_ENV}'; "
    boot_cmd+="exec '${THORCTL}' analyze chat --resume '${RESUME_SESSION}'"
    tmux new-session -d -s "$SESSION" -c "$BATCH_DEMO" -x "$COLS" -y "$ROWS" "$boot_cmd"
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
    key j; sleep 1                          # select the first root row (ghidra)
    key j; sleep 1                          # move onto the main agent row
    key j; sleep 1                          # move onto the first swarm row
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

case "${1:?usage: record-analyze-chat.sh <main|report|boot|forum|search|usage|timeline|traversal|stop|all>}" in
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
