#!/usr/bin/env bash
# Shared helpers for the docs asciinema clips. Each clip-*.sh sources this file and is
# recorded with `asciinema rec -c "bash clip-<name>.sh"` by record-all.sh, so everything
# a helper prints lands in the recording — keep out-of-band work silent.

# clip handoff state: current clips hand entities to later clips by NAME, so nothing
# writes these today — the paths and save_var stay so a future id handoff just works
STATE_DIR="/tmp/thorium-docs-capture"
CLIPS_ENV="$STATE_DIR/clips.env"
SHOTS_STATE="$STATE_DIR/state.json"

# load any ids earlier clips saved
# shellcheck disable=SC1090
[ -f "$CLIPS_ENV" ] && source "$CLIPS_ENV"

# print a shell prompt, type the command character by character, then run it;
# commands may contain `\`+newline continuations, which type under a `>` prompt
type_cmd() {
  # the command to type and execute
  local cmd="$1"
  # a green prompt reads clearly on the player's dark theme
  printf '\033[1;32m$\033[0m '
  # type the command at a human-ish cadence
  local i ch
  for ((i = 0; i < ${#cmd}; i++)); do
    ch="${cmd:i:1}"
    if [ "$ch" = $'\n' ]; then
      # continue multi-line commands under a continuation prompt like a real shell
      printf '\n\033[1;32m>\033[0m '
    else
      printf '%s' "$ch"
    fi
    sleep 0.03
  done
  printf '\n'
  # a short beat between "typing" and output, and after the command finishes
  sleep 0.35
  eval "$cmd"
  sleep 0.9
}

# silently wait until a freshly created name resolves by name (the entities_by_name
# view updates asynchronously relative to the create); bounded so a real failure still
# surfaces on the next on-camera command instead of hanging the recording
await_name() {
  # poll the given probe command until it exits 0 or the budget runs out
  local tries=0
  until "$@" >/dev/null 2>&1; do
    tries=$((tries + 1))
    if [ "$tries" -ge 60 ]; then return 0; fi
    sleep 0.25
  done
}

# silently resolve the newest entity id for a name via the family's --json list output
entity_id() {
  # the thorctl family to list (rules | rulesets | hunts)
  local family="$1"
  # the entity name to resolve
  local name="$2"
  thorctl "$family" list -g demolab --json 2>/dev/null \
    | jq -r --arg n "$name" 'map(select(.name == $n)) | first | .id'
}

# persist a variable for later clips
save_var() {
  mkdir -p "$STATE_DIR"
  printf 'export %s=%q\n' "$1" "$2" >> "$CLIPS_ENV"
}
