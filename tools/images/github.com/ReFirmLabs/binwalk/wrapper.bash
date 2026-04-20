#!/bin/bash

mkdir -p /tmp/thorium/binwalk
binwalk -e $1 -C /tmp/thorium/binwalk > /tmp/thorium/results

if find "/tmp/thorium/binwalk" -maxdepth 1 -type d -name "*.extracted" -print -quit | grep -q .; then
    mv /tmp/thorium/binwalk/*.extracted/* /tmp/thorium/children/carved/unknown
else
    echo "INFO: No files extracted"
    exit 0
fi
