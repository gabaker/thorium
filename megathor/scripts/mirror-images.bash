#!/usr/bin/env bash
# Mirror the images an offline deployment needs into a private registry.
#
#   scripts/mirror-images.bash <registry> [image-list]
#
# <registry> is the offline_registry value (e.g. 10.0.0.1:5000). The image list
# defaults to files/offline-images.txt, written by offline-stage.yml, with one
# "<source image> <destination path>" pair per line. Uses docker (or podman when
# docker is missing); run it on a machine with internet access that can push to
# the registry.
set -euo pipefail

if [ $# -lt 1 ]; then
    echo "Usage: $0 <registry> [image-list]" >&2
    exit 1
fi
registry="$1"
list="${2:-$(dirname "$0")/../files/offline-images.txt}"
if [ ! -f "$list" ]; then
    echo "Image list ${list} not found; run offline-stage.yml first or pass a list" >&2
    exit 1
fi

# prefer docker but fall back to podman
cli=docker
if ! command -v docker >/dev/null 2>&1; then
    cli=podman
fi

failed=0
while read -r source destination _; do
    # skip comments and blank lines
    case "$source" in
        ''|'#'*) continue ;;
    esac
    target="${registry}/${destination}"
    echo "==> ${source} -> ${target}"
    # the CLI's stdin is closed so it can't consume the rest of the image list
    if "$cli" pull "$source" </dev/null && "$cli" tag "$source" "$target" </dev/null \
        && "$cli" push "$target" </dev/null; then
        continue
    fi
    echo "FAILED: ${source}" >&2
    failed=$((failed + 1))
done < "$list"

if [ "$failed" -gt 0 ]; then
    echo "${failed} image(s) failed to mirror" >&2
    exit 1
fi
echo "All images mirrored to ${registry}"
