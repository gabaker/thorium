#!/bin/sh
# Checks a built core Thorium image is for the architecture its tag names: the image's platform,
# and the ELF machine of crane and of every server binary the image runs.
#
# Usage: check_image_arch.sh <image> <amd64|arm64>
#
# The ELF machine field (2 bytes at offset 18, little-endian) is read with od, so the check runs in
# any POSIX shell with docker, including the busybox GitLab job image, which has no `file`.
set -eu

if [ "$#" -ne 2 ]; then
  echo "usage: $0 <image> <amd64|arm64>" >&2
  exit 2
fi
image=$1
arch=$2
# the ELF machine bytes each architecture's binaries carry
case "$arch" in
  amd64) machine="3e 00" ;;
  arm64) machine="b7 00" ;;
  *)
    echo "error: no ELF machine is known for architecture ${arch}" >&2
    exit 2
    ;;
esac

# the image's own platform
platform=$(docker image inspect --format '{{.Os}}/{{.Architecture}}' "$image")
if [ "$platform" != "linux/${arch}" ]; then
  echo "error: ${image} is for ${platform}, not linux/${arch}" >&2
  exit 1
fi

# copy the binaries out of a created (never started) container
scratch=$(mktemp -d)
container=""
cleanup() {
  if [ -n "$container" ]; then
    docker rm "$container" > /dev/null 2>&1 || true
  fi
  rm -rf "$scratch"
}
trap cleanup EXIT
container=$(docker create "$image")
failed=0
for binary in crane thorium-api thorium-operator thoradm thorium-scaler thorium-search-streamer thorium-event-handler; do
  if ! docker cp "${container}:/app/${binary}" "${scratch}/${binary}" > /dev/null 2>&1; then
    echo "error: ${image} has no /app/${binary}" >&2
    failed=1
    continue
  fi
  # the ELF machine field, as two space-separated hex bytes
  found=$(od -An -tx1 -j18 -N2 "${scratch}/${binary}" | tr -s ' \n' ' ' | sed 's/^ //; s/ $//')
  if [ "$found" != "$machine" ]; then
    echo "error: ${image}'s /app/${binary} has ELF machine '${found}', not '${machine}' (${arch})" >&2
    failed=1
  fi
done
if [ "$failed" -ne 0 ]; then
  exit 1
fi
echo "${image} is linux/${arch}, and its binaries are all ${arch}"
