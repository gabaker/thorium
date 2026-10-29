#!/usr/bin/env bash
# Clip: edit a rule's source in place with `thorctl rules edit`, targeted by name and
# opened in helix. Embedded in users/yara_rules.md as rules-edit.cast.
#
# Requirements beyond the usual clip set: tmux, and helix (`hx`) on PATH with the
# onedark theme configured (~/.config/helix/config.toml: theme = "onedark"). The
# editor runs inside a dedicated tmux server (so the PATH shim is inherited) and is
# driven by a background send-keys script while the recording stays attached. The
# clip passes --editor explicitly because the recording shim authenticates with
# --keys, which builds a default config whose editor is vi; real users set theirs
# once with `thorctl config --default-editor hx`.
cd "$(dirname "${BASH_SOURCE[0]}")"
source ./lib.sh

# helix wants a capable TERM inside the headless recording pty
export TERM=xterm-256color
# a dedicated tmux socket keeps us off any existing server and inherits our PATH shim
TMUX_ARGS=(-L thorium-clip)

# build the driven session: a clean minimal-prompt shell with no status bar
tmux "${TMUX_ARGS[@]}" kill-server 2>/dev/null || true
tmux "${TMUX_ARGS[@]}" new-session -d -s rules-edit -x 120 -y 24 \
  "env PS1='\\[\\e[1;32m\\]\$\\[\\e[0m\\] ' bash --norc"
tmux "${TMUX_ARGS[@]}" set -g status off

# feed the session keystrokes on a timer while the recording is attached to it
(
  # give the attach a beat to land before typing
  sleep 1.2
  # type the edit command like a human and run it
  tmux "${TMUX_ARGS[@]}" send-keys -t rules-edit -l "thorctl rules edit demo_keylogger_strings --editor hx"
  sleep 0.8
  tmux "${TMUX_ARGS[@]}" send-keys -t rules-edit Enter
  # wait for helix to open the rule source
  sleep 3
  # jump to the end of the rule and note the review inline
  tmux "${TMUX_ARGS[@]}" send-keys -t rules-edit -l "ge"
  sleep 0.8
  tmux "${TMUX_ARGS[@]}" send-keys -t rules-edit -l "o"
  sleep 0.8
  tmux "${TMUX_ARGS[@]}" send-keys -t rules-edit -l "// reviewed by demolab triage"
  sleep 1
  tmux "${TMUX_ARGS[@]}" send-keys -t rules-edit Escape
  sleep 0.8
  # save and quit helix, handing the edit back to thorctl
  tmux "${TMUX_ARGS[@]}" send-keys -t rules-edit -l ":wq"
  sleep 0.6
  tmux "${TMUX_ARGS[@]}" send-keys -t rules-edit Enter
  # let thorctl print its update confirmation
  sleep 2.5
  # end the inner shell, which detaches the recording cleanly
  tmux "${TMUX_ARGS[@]}" send-keys -t rules-edit -l "exit"
  sleep 0.4
  tmux "${TMUX_ARGS[@]}" send-keys -t rules-edit Enter
) &

# attach the recording to the driven session until its shell exits
tmux "${TMUX_ARGS[@]}" attach -t rules-edit
# make sure the driver finished and the dedicated server is gone
wait
tmux "${TMUX_ARGS[@]}" kill-server 2>/dev/null || true
