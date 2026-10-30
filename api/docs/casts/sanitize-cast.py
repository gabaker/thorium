#!/usr/bin/env python3
"""Sanitize a freshly recorded asciicast and gate it on a leak denylist.

Two jobs, run after every take by the record-analyze-*.sh scripts:

1. Drop the v3 header's optional ``command`` field. asciinema records the
   exact ``--command`` line there, which for these demos names internal repo
   checkouts and demo-dir paths that have no business shipping in the docs.
2. Scan the whole cast for strings that must never appear in a published
   demo (the model id, provider/config paths, credential material). Any hit
   prints the offending needle with context and the script exits 1, so a
   leaky take fails loudly instead of landing in the book.

Usage: sanitize-cast.py <cast> [<cast> ...]
"""

import json
import re
import sys

# strings that must never ship in a demo cast (checked case-insensitively);
# a hit means the take is burned, not that this list should shrink
DENYLIST = [
    "mythos",            # the model id must stay hidden
    "anthropic.claude",  # any anthropic model id string
    "bedrock",           # provider name: config/check beats belong off camera
    "mantle",            # bedrock endpoint selector from the ai config
    "thorpal_evaluate",  # internal checkout names
    "thorpal_support",
    "aws_access_key",    # credential material from a sourced env file
    "aws_secret",
    "aws_session",
]

# credential-id prefixes are case-sensitive and shape-specific: AKIA/ASIA
# followed by 16 key chars (plain "Asia" in analysis prose must not trip this)
KEY_ID_RE = re.compile(r"(?:AKIA|ASIA)[0-9A-Z]{16}")


def sanitize(path):
    """Sanitize one cast in place and return its denylist hits.

    # Arguments

    * `path` - The asciicast v3 file to sanitize
    """
    with open(path, encoding="utf-8") as cast:
        lines = cast.read().splitlines(keepends=True)
    # drop the header's recorded command line, keeping every other field
    header = json.loads(lines[0])
    if "command" in header:
        del header["command"]
        lines[0] = json.dumps(header, separators=(",", ":")) + "\n"
        with open(path, "w", encoding="utf-8") as cast:
            cast.writelines(lines)
    # scan every event payload (and the header) for denylisted strings
    hits = []
    for number, line in enumerate(lines):
        lowered = line.lower()
        for needle in DENYLIST:
            column = lowered.find(needle)
            if column != -1:
                context = line[max(0, column - 60) : column + 80]
                hits.append((needle, number + 1, context.strip()))
        for match in KEY_ID_RE.finditer(line):
            hits.append((match.group(0), number + 1, "credential key id shape"))
    return hits


def main():
    """Sanitize every cast named on the command line."""
    if len(sys.argv) < 2:
        sys.exit("usage: sanitize-cast.py <cast> [<cast> ...]")
    failed = False
    for path in sys.argv[1:]:
        hits = sanitize(path)
        if hits:
            failed = True
            print(f"LEAK {path}:", file=sys.stderr)
            for needle, line, context in hits:
                print(f"  line {line}: {needle!r}: {context!r}", file=sys.stderr)
        else:
            print(f"clean {path}")
    sys.exit(1 if failed else 0)


if __name__ == "__main__":
    main()
