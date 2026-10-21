#!/bin/bash
# Writes one committed golden backup into this directory: seeds the golden fixture into a fresh
# test namespace, backs it up with the given thoradm binary, and saves the namespace's snapshot as
# <name>/expected/snapshot.json and the golden's provenance as <name>/PROVENANCE.
#
# usage: generate.sh <name> <thoradm-binary> <commit>
#   name            legacy, format-1, or format-2
#   thoradm-binary  the thoradm that writes the backup
#   commit          the commit that binary was built from
#
# It runs thoradm's test harness, so the environment must point it at test backends the way the
# thoradm tests need (the THORIUM__* overrides, RUSTFLAGS, and THORADM_TEST_RUN).
set -euo pipefail
# Caution: each golden's backup/redis.json holds Admin users with live-format tokens and password
# hashes under the throwaway secret_key its PROVENANCE names, and generate_golden writes that
# caution into PROVENANCE; never restore a golden outside the thoradm test harness.
if [ $# -ne 3 ]; then
  sed -n 2,13p "$0" >&2
  exit 1
fi
case "$1" in
  legacy|format-1|format-2) ;;
  *) echo "generate.sh: unknown golden '$1'" >&2; exit 1 ;;
esac
here=$(cd "$(dirname "$0")" && pwd)
binary=$(cd "$(dirname "$2")" && pwd)/$(basename "$2")
cd "$here/../../.."
THORADM_GOLDEN_NAME=$1 \
THORADM_GOLDEN_BINARY=$binary \
THORADM_GOLDEN_COMMIT=$3 \
THORADM_GOLDEN_TOOLCHAIN=$(rustc --version) \
THORADM_GOLDEN_HARNESS=$(git describe --always --dirty --abbrev=7) \
  cargo test -p thoradm --test golden -- --ignored --exact generate_golden --nocapture
