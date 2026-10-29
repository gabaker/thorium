#!/usr/bin/env bash
# Record every docs asciinema clip against a live Thorium instance.
#
# Requirements:
#   - a release thorctl at <repo>/target/release/thorctl (cargo build --release -p thorctl)
#   - THORCTL_KEYS pointing at a thorctl keys file for the instance (never committed)
#   - the asciinema recorder on PATH or named via ASCIINEMA
#   - jq, and the screenshot session's demolab seed data already on the instance (the
#     hunt clips freeze its demo_downloader_c2 marker rule by name; state.json itself
#     is no longer read since clips type entity names, not ids)
#
# Casts land in api/docs/src/static_resources/casts/ (override with CAST_DIR).
set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")"

# the keys file is mandatory — the shim injects it so recorded commands read cleanly
: "${THORCTL_KEYS:?set THORCTL_KEYS to a thorctl keys yaml for the target instance}"
export THORCTL_KEYS

# resolve the recorder and the repo's freshly built thorctl
ASCIINEMA="${ASCIINEMA:-asciinema}"
REPO_ROOT="$(cd ../../.. && pwd)"
THORCTL_BIN="$REPO_ROOT/target/release/thorctl"
[ -x "$THORCTL_BIN" ] || { echo "release thorctl missing at $THORCTL_BIN" >&2; exit 1; }

# a PATH shim so clips can type plain `thorctl ...` while auth stays out of frame;
# --skip-update keeps the version check from ever prompting mid-recording
SHIM_DIR="$(mktemp -d)"
trap 'rm -rf "$SHIM_DIR"' EXIT
printf '#!/usr/bin/env bash\nexec %q --keys "$THORCTL_KEYS" --skip-update "$@"\n' "$THORCTL_BIN" > "$SHIM_DIR/thorctl"
chmod +x "$SHIM_DIR/thorctl"
export PATH="$SHIM_DIR:$PATH"

# start the clip state fresh so reruns never source stale state (current clips hand
# entities off by name and write nothing here, but lib.sh still loads the file)
rm -f /tmp/thorium-docs-capture/clips.env

# record each clip at the geometry the player expects; idle gaps compress to 2s
OUT="${CAST_DIR:-$REPO_ROOT/api/docs/src/static_resources/casts}"
mkdir -p "$OUT"
for name in rules-create rules-import sigma-create ruleset-manage ruleset-inspect \
  hunt-run hunt-hits hunt-rules rules-update rules-notifications rules-admin rules-edit; do
  echo "recording $name.cast"
  "$ASCIINEMA" rec --headless --overwrite --window-size 120x24 --idle-time-limit 2 \
    --title "thorctl ${name//-/ }" --command "bash clip-$name.sh" "$OUT/$name.cast"
done
echo "casts written to $OUT"
