#/usr/bin/env bash
set -e

# determine our operating system type (x86/ARM)
OS=$(uname -s | awk '{print tolower($0)}')
HARDWARE=$(uname -m | sed -r 's/_/-/g')

echo
echo "Detected architecture: \"$OS/$HARDWARE\""

# ask where to store Thorctl at
echo
read -p "Enter Thorctl installation directory [~/.local/bin]: " TARGET </dev/tty
TARGET=${TARGET:-~/.local/bin}

# create our target dir if it doesn't already exist
mkdir -p $TARGET

# build the url to download Thorctl at
URL=$1/api/binaries/$OS/$HARDWARE/thorctl
# download Thorctl to our target folder
echo
# download insecurely if flag is set, avoiding certificate issues
if [ "$2" = "--insecure" ]; then
  echo "Downloading Thorctl insecurely from \"$URL\""
  curl -k $URL -o $TARGET/thorctl
else
  echo "Downloading Thorctl from \"$URL\""
  curl $URL -o $TARGET/thorctl
fi
# make sure we can execute Thorctl
chmod +x $TARGET/thorctl

# add Thorctl to our path
# test that the target is in the PATH
if ! [[ ":$PATH:" == *":$TARGET:"* ]]; then
  # add this to our path
  export PATH=$TARGET:$PATH
  # append to profile if it exists
  if [ -f "~/.profile" ]; then
    echo "export PATH=$TARGET:$PATH" >> "~/.profile"
  fi
  # let the user know they may need to add this to their path
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
echo "For help, run \"thorctl -h\" or see the docs at \"$1/api/docs/user/getting_started/thorctl.html\""
echo
