"""Truncate a tmux-recorded asciicast at the client's detach message.

Recordings made by attaching asciinema to a tmux session end with the tmux
client printing ``[detached (from session <name>)]`` after the driver detaches
it; that line is terminal noise, not demo content. This drops the first event
whose data contains the detach banner and every event after it.

Usage: strip-detach.py <cast> [<cast> ...]   (edits each file in place)
"""
import json
import sys

for path in sys.argv[1:]:
    # read the whole cast: one header line plus one JSON event per line
    with open(path) as f:
        lines = f.read().splitlines()
    header, events = lines[0], lines[1:]
    kept = []
    dropped = 0
    for line in events:
        if not line.strip():
            continue
        ev = json.loads(line)
        # the detach banner marks the end of the demo content
        if len(ev) > 2 and "detached (from session" in str(ev[2]):
            dropped = len(events) - len(kept)
            break
        kept.append(line)
    # rewrite the cast without the banner and anything after it
    with open(path, "w") as f:
        f.write(header + "\n")
        for line in kept:
            f.write(line + "\n")
    print(f"{path}: kept={len(kept)} dropped={dropped}")
