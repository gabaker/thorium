#!/usr/bin/env bash
set -e

OS=$(uname -s | awk '{print tolower($0)}')
HARDWARE=$(uname -m | sed -r 's/_/-/g')

echo
echo "Detected architecture: \"$OS/$HARDWARE\""

# parse flags
BASE_URL=""
INSECURE=false
TARGET=""
for arg in "$@"; do
  case "$arg" in
    --insecure) INSECURE=true ;;
    --target=*) TARGET="${arg#--target=}" ;;
    *)
      if [ -z "$BASE_URL" ]; then
        BASE_URL="$arg"
      fi
      ;;
  esac
done

if [ -z "$BASE_URL" ]; then
  echo "Usage: install-thorctl.sh <base-url> [--target=<dir>] [--insecure]" >&2
  exit 1
fi

# ask where to store Thorctl at (skip prompt if --target was provided)
if [ -z "$TARGET" ]; then
  echo
  read -p "Enter Thorctl installation directory [~/.local/bin]: " TARGET </dev/tty
  TARGET=${TARGET:-~/.local/bin}
fi

mkdir -p "$TARGET"

URL=$BASE_URL/api/binaries/$OS/$HARDWARE/thorctl

echo
# a platform this Thorium has no build for answers 404, which fails the download rather than
# installing the error page as thorctl
CURL_ARGS=(-f "$URL" -o "$TARGET/thorctl")
if [ "$INSECURE" = true ]; then
  echo "Downloading Thorctl insecurely from \"$URL\""
  CURL_ARGS=(-k "${CURL_ARGS[@]}")
else
  echo "Downloading Thorctl from \"$URL\""
fi
if ! curl "${CURL_ARGS[@]}"; then
  echo "Failed to download Thorctl for \"$OS/$HARDWARE\"; this Thorium may not serve a build for it" >&2
  exit 1
fi
chmod +x "$TARGET/thorctl"

if ! [[ ":$PATH:" == *":$TARGET:"* ]]; then
  export PATH=$TARGET:$PATH
  if [ -f "~/.profile" ]; then
    echo "export PATH=$TARGET:\$PATH" >> "~/.profile"
  fi
  echo
  echo "We have added Thorctl to your path but you may need to do it manually."
  echo "If thorctl fails to run add the following to your run commands file (\".bashrc\", \".zshrc\", etc.):"
  echo
  echo "  export PATH=\$PATH:$TARGET"
fi

echo
echo "---------------------------------"
echo
echo "Thorctl installed successfully!"
echo "Run \"thorctl login\" to get started."
echo
echo "For help, run \"thorctl -h\" or see the docs at \"$BASE_URL/api/docs/user/getting_started/thorctl.html\""
echo
