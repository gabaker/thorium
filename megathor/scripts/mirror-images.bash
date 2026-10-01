#!/usr/bin/env bash
# Mirror the images an offline deployment needs into a private registry.
#
#   scripts/mirror-images.bash <registry> [image-list]
#
# <registry> is the offline_registry value (e.g. 10.0.0.1:5000). The image list
# defaults to scripts/offline-images.txt, one "<source image> <destination path>"
# pair per line. Uses docker (or podman when docker is missing); run it on a
# machine with internet access that can push to the registry.
set -euo pipefail

if [ $# -lt 1 ]; then
    echo "Usage: $0 <registry> [image-list]" >&2
    exit 1
fi
registry="$1"
list="${2:-$(dirname "$0")/offline-images.txt}"

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
    if "$cli" pull "$source" && "$cli" tag "$source" "$target" && "$cli" push "$target"; then
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
